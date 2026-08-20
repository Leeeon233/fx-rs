//! Vercel AI Gateway provider: a cold-start-safe model catalog, Vercel device
//! OAuth, ambient bearer credentials, refresh, team routing, and transport
//! construction.

use std::collections::{BTreeMap, BTreeSet};
use std::process::{Command, Stdio};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::transport::{VercelGateway, VercelGatewayConfig, vercel_endpoint_from_base};
use crate::{
    AuthMethod, Credential, CredentialStore, Model, ModelCapabilities, Provider, ProviderError,
    VercelRoutingPolicy,
};
use fx_core::{Gateway, SafeRetryGateway};
use serde::Deserialize;
use url::Url;
use zeroize::{Zeroize, Zeroizing};

pub const PROVIDER_ID: &str = "vercel";
pub const DEFAULT_MODEL: &str = "zai/glm-5.2";
pub const DEVICE_AUTH_METHOD: &str = "oauth";

const PROVIDER_NAME: &str = "Vercel AI Gateway";
const ISSUER: &str = "https://vercel.com";
const DEFAULT_CLIENT_ID: &str = "cl_zzh5hiOZbwJ9bfqEcYqPIJv3TaPaEYL0";
const TEAMS_ENDPOINT: &str = "https://api.vercel.com/v2/teams";
const OAUTH_SCOPE: &str = "openid offline_access";
const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
const REFRESH_SKEW_MS: i64 = 60 * 1000;
const MAX_OAUTH_BODY_BYTES: u64 = 64 * 1024;
const MAX_CATALOG_BODY_BYTES: u64 = 2 * 1024 * 1024;
const DEFAULT_AUTH_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const DEFAULT_CATALOG_ENDPOINT: &str = "https://ai-gateway.vercel.sh/coding-agent/v1/models";

const ISSUER_ATTRIBUTE: &str = "issuer";
const CLIENT_ID_ATTRIBUTE: &str = "client_id";
const SCOPE_ATTRIBUTE: &str = "scope";
const TOKEN_TYPE_ATTRIBUTE: &str = "token_type";
const TEAM_ID_ATTRIBUTE: &str = "team_id";
const TEAM_SLUG_ATTRIBUTE: &str = "team_slug";

#[derive(Clone, Debug)]
pub struct VercelProviderConfig {
    pub issuer: String,
    pub client_id: String,
    pub gateway_endpoint: String,
    pub catalog_endpoint: String,
    pub teams_endpoint: String,
    pub team: Option<String>,
    pub open_browser: bool,
    pub auth_timeout: Duration,
    /// Additional provider-local model IDs advertised without a startup
    /// catalog request. This preserves cold start while allowing newly added
    /// Gateway models to be selected immediately.
    pub additional_models: Vec<String>,
    /// Optional provider routing independent of model identity. `only`
    /// disables fallback outside the allow-list; `order` only changes
    /// preference and keeps Gateway fallback behavior.
    pub routing: VercelRoutingPolicy,
}

impl VercelProviderConfig {
    pub fn from_process() -> Self {
        let issuer = std::env::var("FX_E2E_OAUTH_ISSUER_URL")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| ISSUER.into());
        let teams_endpoint = if is_loopback_origin(&issuer) {
            format!("{}/v2/teams", issuer.trim_end_matches('/'))
        } else {
            TEAMS_ENDPOINT.into()
        };
        Self {
            issuer,
            client_id: std::env::var("FX_OAUTH_CLIENT_ID")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_CLIENT_ID.into()),
            gateway_endpoint: vercel_endpoint_from_base(
                std::env::var("FX_GATEWAY_BASE_URL").ok().as_deref(),
            ),
            catalog_endpoint: vercel_catalog_endpoint_from_base(
                std::env::var("FX_GATEWAY_BASE_URL").ok().as_deref(),
            ),
            teams_endpoint,
            team: std::env::var("FX_VERCEL_TEAM")
                .ok()
                .filter(|value| !value.trim().is_empty()),
            open_browser: true,
            auth_timeout: DEFAULT_AUTH_TIMEOUT,
            additional_models: parse_additional_models(
                std::env::var("FX_VERCEL_MODELS").ok().as_deref(),
            ),
            routing: VercelRoutingPolicy {
                only: parse_provider_routes(
                    std::env::var("FX_VERCEL_PROVIDER_ONLY").ok().as_deref(),
                ),
                order: parse_provider_routes(
                    std::env::var("FX_VERCEL_PROVIDER_ORDER").ok().as_deref(),
                ),
            },
        }
    }
}

#[derive(Clone, Debug)]
pub struct VercelProvider {
    config: VercelProviderConfig,
    models: Vec<Model>,
    accepted_model_ids: Arc<RwLock<BTreeSet<String>>>,
    http: ureq::Agent,
}

impl VercelProvider {
    pub fn new(config: VercelProviderConfig) -> Result<Self, ProviderError> {
        validate_issuer(&config.issuer)?;
        validate_client_id(&config.client_id)?;
        validate_trusted_endpoint(&config.gateway_endpoint, EndpointKind::Gateway)?;
        validate_trusted_endpoint(&config.catalog_endpoint, EndpointKind::Gateway)?;
        validate_trusted_endpoint(&config.teams_endpoint, EndpointKind::VercelApi)?;
        validate_routing_policy(&config.routing)?;
        let models = model_catalog(&config.additional_models)?;
        let accepted_model_ids = Arc::new(RwLock::new(
            models.iter().map(|model| model.id.clone()).collect(),
        ));
        let http = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .new_agent();
        Ok(Self {
            config,
            models,
            accepted_model_ids,
            http,
        })
    }

    pub fn from_process() -> Self {
        Self::new(VercelProviderConfig::from_process())
            .expect("built-in Vercel provider configuration is valid")
    }

    fn accepts_model(&self, id: &str) -> bool {
        self.accepted_model_ids
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(id)
    }

    fn resolve_auth(
        &self,
        credentials: &dyn CredentialStore,
    ) -> Result<ResolvedVercelAuth, ProviderError> {
        if let Some(token) = nonempty_env("VERCEL_OIDC_TOKEN") {
            return Ok(ResolvedVercelAuth {
                access_token: Zeroizing::new(token),
                team_id: self.config.team.clone(),
                source: VercelCredentialSource::ApiKey,
            });
        }
        if let Some(token) = nonempty_env("AI_GATEWAY_API_KEY") {
            return Ok(ResolvedVercelAuth {
                access_token: Zeroizing::new(token),
                team_id: self.config.team.clone(),
                source: VercelCredentialSource::ApiKey,
            });
        }

        let mut lease = credentials.lock(PROVIDER_ID)?;
        let Some(credential) = lease.credential() else {
            return Err(ProviderError::AuthenticationRequired {
                provider: PROVIDER_NAME.into(),
                message: "call ACP authenticate with method `vercel:oauth`, or set `VERCEL_OIDC_TOKEN` or `AI_GATEWAY_API_KEY`".into(),
            });
        };
        match credential {
            Credential::ApiKey { secret, attributes } => Ok(ResolvedVercelAuth {
                access_token: Zeroizing::new(secret.clone()),
                team_id: configured_or_stored_team(&self.config, attributes),
                source: VercelCredentialSource::ApiKey,
            }),
            Credential::OAuth {
                access_token,
                refresh_token,
                expires_at_ms,
                attributes,
            } => {
                let team_id = configured_or_stored_team(&self.config, attributes);
                if expires_at_ms.saturating_sub(REFRESH_SKEW_MS) > now_ms() {
                    return Ok(ResolvedVercelAuth {
                        access_token: Zeroizing::new(access_token.clone()),
                        team_id,
                        source: VercelCredentialSource::FxLogin,
                    });
                }
                let refresh_token = refresh_token.as_deref().ok_or_else(|| {
                    ProviderError::Authentication(
                        "the saved Vercel session expired and cannot be refreshed; authenticate again"
                            .into(),
                    )
                })?;
                let issuer = attributes
                    .get(ISSUER_ATTRIBUTE)
                    .map(String::as_str)
                    .unwrap_or(&self.config.issuer);
                let client_id = attributes
                    .get(CLIENT_ID_ATTRIBUTE)
                    .map(String::as_str)
                    .unwrap_or(&self.config.client_id);
                let previous_scope = attributes
                    .get(SCOPE_ATTRIBUTE)
                    .map(String::as_str)
                    .unwrap_or(OAUTH_SCOPE);
                validate_issuer(issuer)?;
                validate_client_id(client_id)?;
                let metadata = self.discover(issuer)?;
                let token =
                    self.refresh_token(&metadata, client_id, refresh_token, previous_scope)?;
                let resolved = ResolvedVercelAuth {
                    access_token: Zeroizing::new(token.access_token.clone()),
                    team_id: team_id.clone(),
                    source: VercelCredentialSource::FxLogin,
                };
                let team = TeamSelection {
                    id: team_id,
                    slug: attributes.get(TEAM_SLUG_ATTRIBUTE).cloned(),
                };
                lease.replace(token.into_credential(issuer, client_id, &team))?;
                Ok(resolved)
            }
        }
    }

    fn device_login(&self) -> Result<(OAuthToken, TeamSelection), ProviderError> {
        let metadata = self.discover(&self.config.issuer)?;
        let device = self.request_device_authorization(&metadata)?;
        let display_url = device
            .verification_uri_complete
            .as_deref()
            .unwrap_or(&device.verification_uri);
        eprintln!("fxrs Vercel login: open {display_url}");
        eprintln!("fxrs Vercel login code: {}", device.user_code);
        if self.config.open_browser
            && let Err(error) = open_browser(display_url)
        {
            eprintln!(
                "fxrs Vercel login: browser could not be opened ({error}); open the URL manually"
            );
        }

        let token = self.poll_device_token(&metadata, &device)?;
        let teams = self.fetch_teams(&token.access_token).unwrap_or_default();
        let team = select_team(&teams, self.config.team.as_deref())?;
        Ok((token, team))
    }

    fn discover(&self, issuer: &str) -> Result<OAuthMetadata, ProviderError> {
        let endpoint = format!(
            "{}/.well-known/openid-configuration",
            issuer.trim_end_matches('/')
        );
        let (status, bytes) = self.get_bounded(&endpoint, "Vercel OAuth discovery")?;
        if status != 200 {
            return Err(ProviderError::Authentication(format!(
                "Vercel OAuth discovery returned HTTP {status}"
            )));
        }
        let metadata: OAuthMetadata = serde_json::from_slice(&bytes).map_err(|_| {
            ProviderError::Authentication("Vercel OAuth discovery response was invalid".into())
        })?;
        if metadata.issuer.trim_end_matches('/') != issuer.trim_end_matches('/') {
            return Err(ProviderError::Authentication(
                "Vercel OAuth discovery returned a different issuer".into(),
            ));
        }
        validate_oauth_endpoint(issuer, &metadata.device_authorization_endpoint)?;
        validate_oauth_endpoint(issuer, &metadata.token_endpoint)?;
        Ok(metadata)
    }

    fn request_device_authorization(
        &self,
        metadata: &OAuthMetadata,
    ) -> Result<DeviceAuthorization, ProviderError> {
        let (status, bytes) = self.post_form_bounded(
            &metadata.device_authorization_endpoint,
            &[
                ("client_id", self.config.client_id.as_str()),
                ("scope", OAUTH_SCOPE),
            ],
            "Vercel device authorization",
        )?;
        if status != 200 {
            return Err(oauth_status_error(status, &bytes, "device authorization"));
        }
        let device: DeviceAuthorization = serde_json::from_slice(&bytes).map_err(|_| {
            ProviderError::Authentication("Vercel device authorization response was invalid".into())
        })?;
        if device.device_code.is_empty()
            || device.user_code.is_empty()
            || device.verification_uri.is_empty()
            || device.expires_in <= 0
        {
            return Err(ProviderError::Authentication(
                "Vercel device authorization response was incomplete".into(),
            ));
        }
        validate_oauth_endpoint(&self.config.issuer, &device.verification_uri)?;
        if let Some(complete) = device.verification_uri_complete.as_deref() {
            validate_oauth_endpoint(&self.config.issuer, complete)?;
        }
        Ok(device)
    }

    fn poll_device_token(
        &self,
        metadata: &OAuthMetadata,
        device: &DeviceAuthorization,
    ) -> Result<OAuthToken, ProviderError> {
        let device_lifetime = Duration::from_secs(u64::try_from(device.expires_in).unwrap_or(0));
        let deadline = Instant::now() + self.config.auth_timeout.min(device_lifetime);
        let mut interval = Duration::from_secs(u64::try_from(device.interval.max(1)).unwrap_or(5));
        while Instant::now() < deadline {
            thread::sleep(interval.min(deadline.saturating_duration_since(Instant::now())));
            let (status, bytes) = self.post_form_bounded(
                &metadata.token_endpoint,
                &[
                    ("client_id", self.config.client_id.as_str()),
                    ("grant_type", DEVICE_GRANT),
                    ("device_code", device.device_code.as_str()),
                ],
                "Vercel device token",
            )?;
            if status == 200 {
                return parse_token(&bytes, None, None);
            }
            match oauth_error_code(&bytes) {
                Some("authorization_pending") => {}
                Some("slow_down") => interval += Duration::from_secs(5),
                Some("access_denied") => {
                    return Err(ProviderError::Authentication(
                        "Vercel device authorization was denied".into(),
                    ));
                }
                Some("expired_token") => {
                    return Err(ProviderError::Authentication(
                        "Vercel device authorization expired".into(),
                    ));
                }
                _ => return Err(oauth_status_error(status, &bytes, "device token")),
            }
        }
        Err(ProviderError::Authentication(
            "timed out waiting for Vercel device authorization".into(),
        ))
    }

    fn refresh_token(
        &self,
        metadata: &OAuthMetadata,
        client_id: &str,
        refresh_token: &str,
        previous_scope: &str,
    ) -> Result<OAuthToken, ProviderError> {
        let (status, bytes) = self.post_form_bounded(
            &metadata.token_endpoint,
            &[
                ("client_id", client_id),
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token),
            ],
            "Vercel token refresh",
        )?;
        if status != 200 {
            return Err(oauth_status_error(status, &bytes, "token refresh"));
        }
        parse_token(&bytes, Some(refresh_token), Some(previous_scope))
    }

    fn fetch_teams(&self, access_token: &str) -> Result<Vec<Team>, ProviderError> {
        let authorization = Zeroizing::new(format!("Bearer {access_token}"));
        let mut response = self
            .http
            .get(&self.config.teams_endpoint)
            .header("authorization", authorization.as_str())
            .header("accept", "application/json")
            .header("user-agent", concat!("fxrs/", env!("CARGO_PKG_VERSION")))
            .call()
            .map_err(|error| {
                ProviderError::Authentication(format!("Vercel team request failed: {error}"))
            })?;
        let status = response.status().as_u16();
        let bytes = read_bounded_response(&mut response, "Vercel team response")?;
        if status != 200 {
            return Err(ProviderError::Authentication(format!(
                "Vercel team request returned HTTP {status}"
            )));
        }
        let response: TeamsResponse = serde_json::from_slice(&bytes).map_err(|_| {
            ProviderError::Authentication("Vercel team response was invalid".into())
        })?;
        Ok(response
            .teams
            .into_iter()
            .filter(|team| !team.id.is_empty() && !team.slug.is_empty())
            .collect())
    }

    fn fetch_model_catalog(
        &self,
        auth: Option<&ResolvedVercelAuth>,
    ) -> Result<(u16, Zeroizing<Vec<u8>>), ProviderError> {
        let mut url = Url::parse(&self.config.catalog_endpoint).map_err(|error| {
            ProviderError::Configuration(format!("invalid Vercel catalog endpoint: {error}"))
        })?;
        if let Some(ResolvedVercelAuth {
            source: VercelCredentialSource::FxLogin,
            team_id: Some(team_id),
            ..
        }) = auth
        {
            url.query_pairs_mut().append_pair("teamId", team_id);
        }

        let mut request = self
            .http
            .get(url.as_str())
            .header("accept", "application/json")
            .header("user-agent", concat!("fxrs/", env!("CARGO_PKG_VERSION")));
        if let Some(auth) = auth {
            let authorization = Zeroizing::new(format!("Bearer {}", auth.access_token.as_str()));
            request = request.header("authorization", authorization.as_str());
            if auth.source == VercelCredentialSource::ApiKey
                && let Some(team_id) = auth.team_id.as_deref()
            {
                request = request.header("x-vercel-ai-gateway-team", team_id);
            }
            let mut response = request.call().map_err(|error| {
                ProviderError::Transport(format!("Vercel model catalog request failed: {error}"))
            })?;
            let status = response.status().as_u16();
            let bytes = read_bounded_response_with_limit(
                &mut response,
                "Vercel model catalog response",
                MAX_CATALOG_BODY_BYTES,
            )?;
            return Ok((status, bytes));
        }

        let mut response = request.call().map_err(|error| {
            ProviderError::Transport(format!(
                "Vercel public model catalog request failed: {error}"
            ))
        })?;
        let status = response.status().as_u16();
        let bytes = read_bounded_response_with_limit(
            &mut response,
            "Vercel public model catalog response",
            MAX_CATALOG_BODY_BYTES,
        )?;
        Ok((status, bytes))
    }

    fn load_model_catalog(
        &self,
        credentials: &dyn CredentialStore,
    ) -> Result<Vec<Model>, ProviderError> {
        let auth = self.resolve_auth(credentials)?;
        // An fx login without a selected team cannot address its private
        // catalog. This mirrors the Zig implementation's public-only access.
        let authenticated =
            !(auth.source == VercelCredentialSource::FxLogin && auth.team_id.is_none());
        let (status, bytes) = self.fetch_model_catalog(authenticated.then_some(&auth))?;
        let bytes = if authenticated && matches!(status, 401 | 403) {
            let (fallback_status, fallback) = self.fetch_model_catalog(None)?;
            if fallback_status != 200 {
                return Err(catalog_status_error(fallback_status));
            }
            fallback
        } else {
            if status != 200 {
                return Err(catalog_status_error(status));
            }
            bytes
        };
        let models = parse_model_catalog(&bytes, &self.config.additional_models)?;
        if !models.iter().any(|model| model.id == DEFAULT_MODEL) {
            return Err(ProviderError::Transport(format!(
                "Vercel model catalog omitted the default model `{DEFAULT_MODEL}`"
            )));
        }
        Ok(models)
    }

    fn get_bounded(
        &self,
        endpoint: &str,
        label: &str,
    ) -> Result<(u16, Zeroizing<Vec<u8>>), ProviderError> {
        let mut response = self.http.get(endpoint).call().map_err(|error| {
            ProviderError::Authentication(format!("{label} request failed: {error}"))
        })?;
        let status = response.status().as_u16();
        let bytes = read_bounded_response(&mut response, label)?;
        Ok((status, bytes))
    }

    fn post_form_bounded(
        &self,
        endpoint: &str,
        form: &[(&str, &str)],
        label: &str,
    ) -> Result<(u16, Zeroizing<Vec<u8>>), ProviderError> {
        let mut response = self
            .http
            .post(endpoint)
            .send_form(form.iter().copied())
            .map_err(|error| {
                ProviderError::Authentication(format!("{label} request failed: {error}"))
            })?;
        let status = response.status().as_u16();
        let bytes = read_bounded_response(&mut response, label)?;
        Ok((status, bytes))
    }
}

impl Provider for VercelProvider {
    fn id(&self) -> &str {
        PROVIDER_ID
    }

    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    fn models(&self) -> Vec<Model> {
        self.models.clone()
    }

    fn default_model(&self) -> &str {
        DEFAULT_MODEL
    }

    fn auth_methods(&self) -> Vec<AuthMethod> {
        vec![AuthMethod::new(
            DEVICE_AUTH_METHOD,
            "Sign in with Vercel",
            "Authorize fxrs for Vercel AI Gateway using the browser device flow.",
        )]
    }

    fn authenticate(
        &self,
        method_id: &str,
        credentials: &dyn CredentialStore,
    ) -> Result<(), ProviderError> {
        if method_id != DEVICE_AUTH_METHOD {
            return Err(ProviderError::UnknownAuthMethod(format!(
                "{PROVIDER_ID}:{method_id}"
            )));
        }
        let (token, team) = self.device_login()?;
        let credential = token.into_credential(&self.config.issuer, &self.config.client_id, &team);
        credentials.lock(PROVIDER_ID)?.replace(credential)
    }

    fn refresh_models(
        &self,
        credentials: &dyn CredentialStore,
    ) -> Result<Option<Vec<Model>>, ProviderError> {
        let models = self.load_model_catalog(credentials)?;
        self.accepted_model_ids
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend(models.iter().map(|model| model.id.clone()));
        Ok(Some(models))
    }

    fn gateway(
        &self,
        model_id: &str,
        session_id: Option<&str>,
        credentials: &dyn CredentialStore,
    ) -> Result<Arc<dyn Gateway>, ProviderError> {
        if !self.accepts_model(model_id) {
            return Err(ProviderError::UnknownModel(format!(
                "{PROVIDER_ID}/{model_id}"
            )));
        }
        let auth = self.resolve_auth(credentials)?;
        let mut config = VercelGatewayConfig::new(model_id, auth.access_token.as_str());
        config.endpoint = self.config.gateway_endpoint.clone();
        config.team_id = auth.team_id;
        config.session_id = session_id.map(str::to_owned);
        config.routing = self.config.routing.clone();
        Ok(Arc::new(SafeRetryGateway::new(Arc::new(
            VercelGateway::new(config),
        ))))
    }
}

fn model_catalog(additional: &[String]) -> Result<Vec<Model>, ProviderError> {
    const BUILT_INS: &[&str] = &[
        "anthropic/claude-opus-4.8",
        "anthropic/claude-opus-4.7",
        "anthropic/claude-opus-4.6",
        "anthropic/claude-opus-4.5",
        "anthropic/claude-sonnet-4.6",
        "alibaba/qwen-3.7-max",
        "alibaba/qwen-3.7-plus",
        "openai/gpt-5.5",
        "openai/gpt-5.5-pro",
        "openai/gpt-5.4",
        "openai/gpt-5.4-mini",
        "openai/gpt-5.3-codex",
        "openai/gpt-5.2-codex",
        "openai/gpt-5.2",
        "openai/o3",
        "openai/gpt-oss-120b",
        "xai/grok-build-0.1",
        "google/gemini-3.1-pro",
        DEFAULT_MODEL,
        "zai/glm-5.2-fast",
        "zai/glm-5.1",
        "zai/glm-5",
        "deepseek/deepseek-v4",
        "deepseek/deepseek-v4-flash",
        "deepseek/deepseek-v4-pro",
        "minimax/minimax-m3",
        "minimax/minimax-m2.7",
    ];
    let mut seen = BTreeSet::new();
    BUILT_INS
        .iter()
        .copied()
        .map(str::to_owned)
        .chain(additional.iter().cloned())
        .filter(|id| seen.insert(id.clone()))
        .map(|id| {
            validate_model_id(&id)?;
            Ok(Model {
                provider_id: PROVIDER_ID.into(),
                name: id.clone(),
                id,
                // Static values are deliberately conservative. The live
                // credential-scoped catalog replaces them after a session is
                // active without adding network I/O to registry construction.
                context_window: 128_000,
                max_output_tokens: 32_000,
                reasoning: true,
                capabilities: ModelCapabilities::default(),
            })
        })
        .collect()
}

fn parse_model_catalog(bytes: &[u8], additional: &[String]) -> Result<Vec<Model>, ProviderError> {
    let document: serde_json::Value = serde_json::from_slice(bytes).map_err(|_| {
        ProviderError::Transport("Vercel model catalog response was invalid JSON".into())
    })?;
    let entries = document
        .get("data")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            ProviderError::Transport("Vercel model catalog response omitted `data`".into())
        })?;
    let mut seen = BTreeSet::new();
    let mut models = entries
        .iter()
        .filter_map(parse_model_catalog_entry)
        .filter(|model| seen.insert(model.id.clone()))
        .collect::<Vec<_>>();
    for id in additional {
        if seen.contains(id) {
            continue;
        }
        validate_model_id(id)?;
        seen.insert(id.clone());
        models.push(Model {
            provider_id: PROVIDER_ID.into(),
            id: id.clone(),
            name: id.clone(),
            context_window: 128_000,
            max_output_tokens: 32_000,
            reasoning: true,
            capabilities: ModelCapabilities::default(),
        });
    }
    if models.is_empty() {
        return Err(ProviderError::Transport(
            "Vercel model catalog contained no language models".into(),
        ));
    }
    Ok(models)
}

fn parse_model_catalog_entry(value: &serde_json::Value) -> Option<Model> {
    let object = value.as_object()?;
    if object
        .get("type")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|kind| !kind.eq_ignore_ascii_case("language"))
    {
        return None;
    }
    let id = object.get("id")?.as_str()?;
    if validate_model_id(id).is_err() {
        return None;
    }
    let name = object
        .get("name")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(id);
    let tags = object
        .get("tags")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .collect::<Vec<_>>();
    let reasoning = tags.iter().any(|tag| tag.eq_ignore_ascii_case("reasoning"))
        || object
            .get("reasoning_options")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|options| {
                options.iter().any(|option| {
                    option.get("type").and_then(serde_json::Value::as_str) == Some("effort")
                })
            });
    Some(Model {
        provider_id: PROVIDER_ID.into(),
        id: id.into(),
        name: name.into(),
        context_window: catalog_u32(object.get("context_window")).unwrap_or(128_000),
        max_output_tokens: catalog_u32(object.get("max_tokens")).unwrap_or(32_000),
        reasoning,
        capabilities: ModelCapabilities::default(),
    })
}

fn catalog_u32(value: Option<&serde_json::Value>) -> Option<u32> {
    value?.as_u64().and_then(|value| u32::try_from(value).ok())
}

fn catalog_status_error(status: u16) -> ProviderError {
    ProviderError::Transport(format!(
        "Vercel model catalog request returned HTTP {status}"
    ))
}

fn vercel_catalog_endpoint_from_base(base: Option<&str>) -> String {
    let Some(base) = base else {
        return DEFAULT_CATALOG_ENDPOINT.into();
    };
    let normalized = base.trim_end_matches('/');
    if is_loopback_origin(normalized) {
        format!("{normalized}/coding-agent/v1/models")
    } else {
        DEFAULT_CATALOG_ENDPOINT.into()
    }
}

fn validate_model_id(value: &str) -> Result<(), ProviderError> {
    let valid = !value.is_empty()
        && value.len() <= 256
        && value.split('/').all(|component| {
            !component.is_empty()
                && component.len() <= 128
                && component
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        });
    if valid {
        Ok(())
    } else {
        Err(ProviderError::Configuration(format!(
            "Vercel model id `{value}` is invalid"
        )))
    }
}

fn parse_additional_models(value: Option<&str>) -> Vec<String> {
    value
        .into_iter()
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect()
}

fn parse_provider_routes(value: Option<&str>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    value
        .into_iter()
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .filter(|value| seen.insert((*value).to_owned()))
        .map(str::to_owned)
        .collect()
}

fn validate_routing_policy(policy: &VercelRoutingPolicy) -> Result<(), ProviderError> {
    for provider in policy.only.iter().chain(&policy.order) {
        let valid = !provider.is_empty()
            && provider.len() <= 128
            && provider
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
        if !valid {
            return Err(ProviderError::Configuration(format!(
                "Vercel Gateway provider route `{provider}` is invalid"
            )));
        }
    }
    Ok(())
}

struct ResolvedVercelAuth {
    access_token: Zeroizing<String>,
    team_id: Option<String>,
    source: VercelCredentialSource,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum VercelCredentialSource {
    ApiKey,
    FxLogin,
}

#[derive(Deserialize)]
struct OAuthMetadata {
    issuer: String,
    device_authorization_endpoint: String,
    token_endpoint: String,
}

#[derive(Deserialize)]
struct DeviceAuthorization {
    device_code: String,
    user_code: String,
    verification_uri: String,
    #[serde(default)]
    verification_uri_complete: Option<String>,
    expires_in: i64,
    #[serde(default = "default_poll_interval")]
    interval: i64,
}

fn default_poll_interval() -> i64 {
    5
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    expires_in: i64,
    #[serde(default)]
    scope: String,
    token_type: String,
}

struct OAuthToken {
    access_token: String,
    refresh_token: String,
    expires_at_ms: i64,
    scope: String,
    token_type: String,
}

impl OAuthToken {
    fn into_credential(
        mut self,
        issuer: &str,
        client_id: &str,
        team: &TeamSelection,
    ) -> Credential {
        let mut attributes = BTreeMap::new();
        attributes.insert(ISSUER_ATTRIBUTE.into(), issuer.into());
        attributes.insert(CLIENT_ID_ATTRIBUTE.into(), client_id.into());
        attributes.insert(SCOPE_ATTRIBUTE.into(), std::mem::take(&mut self.scope));
        attributes.insert(
            TOKEN_TYPE_ATTRIBUTE.into(),
            std::mem::take(&mut self.token_type),
        );
        if let Some(team_id) = &team.id {
            attributes.insert(TEAM_ID_ATTRIBUTE.into(), team_id.clone());
        }
        if let Some(team_slug) = &team.slug {
            attributes.insert(TEAM_SLUG_ATTRIBUTE.into(), team_slug.clone());
        }
        Credential::OAuth {
            access_token: std::mem::take(&mut self.access_token),
            refresh_token: Some(std::mem::take(&mut self.refresh_token)),
            expires_at_ms: self.expires_at_ms,
            attributes,
        }
    }
}

impl Drop for OAuthToken {
    fn drop(&mut self) {
        self.access_token.zeroize();
        self.refresh_token.zeroize();
    }
}

fn parse_token(
    bytes: &[u8],
    previous_refresh: Option<&str>,
    previous_scope: Option<&str>,
) -> Result<OAuthToken, ProviderError> {
    let mut response: TokenResponse = serde_json::from_slice(bytes).map_err(|_| {
        ProviderError::Authentication("Vercel OAuth token response was invalid".into())
    })?;
    if response.access_token.is_empty()
        || response.expires_in <= 0
        || !response.token_type.eq_ignore_ascii_case("bearer")
    {
        response.access_token.zeroize();
        if let Some(refresh_token) = response.refresh_token.as_mut() {
            refresh_token.zeroize();
        }
        return Err(ProviderError::Authentication(
            "Vercel OAuth token response was incomplete".into(),
        ));
    }
    let refresh_token = response
        .refresh_token
        .take()
        .filter(|value| !value.is_empty())
        .or_else(|| previous_refresh.map(str::to_owned))
        .ok_or_else(|| {
            ProviderError::Authentication(
                "Vercel OAuth token response omitted its refresh token".into(),
            )
        })?;
    let expires_at_ms = response
        .expires_in
        .checked_mul(1000)
        .and_then(|duration| now_ms().checked_add(duration))
        .ok_or_else(|| ProviderError::Authentication("Vercel OAuth expiry was invalid".into()))?;
    let scope = if response.scope.trim().is_empty() {
        previous_scope.unwrap_or("").into()
    } else {
        response.scope
    };
    Ok(OAuthToken {
        access_token: response.access_token,
        refresh_token,
        expires_at_ms,
        scope,
        token_type: response.token_type,
    })
}

#[derive(Clone, Debug, Default, Deserialize)]
struct Team {
    id: String,
    slug: String,
    #[serde(default, rename = "name")]
    _name: Option<String>,
}

#[derive(Deserialize)]
struct TeamsResponse {
    teams: Vec<Team>,
}

#[derive(Clone, Debug, Default)]
struct TeamSelection {
    id: Option<String>,
    slug: Option<String>,
}

fn select_team(teams: &[Team], requested: Option<&str>) -> Result<TeamSelection, ProviderError> {
    if let Some(requested) = requested {
        let team = teams
            .iter()
            .find(|team| team.id == requested || team.slug == requested)
            .ok_or_else(|| {
                ProviderError::Authentication(format!(
                    "configured Vercel team `{requested}` was not returned by the account"
                ))
            })?;
        return Ok(TeamSelection {
            id: Some(team.id.clone()),
            slug: Some(team.slug.clone()),
        });
    }
    Ok(teams
        .first()
        .map_or_else(TeamSelection::default, |team| TeamSelection {
            id: Some(team.id.clone()),
            slug: Some(team.slug.clone()),
        }))
}

fn configured_or_stored_team(
    config: &VercelProviderConfig,
    attributes: &BTreeMap<String, String>,
) -> Option<String> {
    let stored_id = attributes.get(TEAM_ID_ATTRIBUTE);
    let stored_slug = attributes.get(TEAM_SLUG_ATTRIBUTE);
    match config.team.as_ref() {
        Some(configured) if stored_id == Some(configured) || stored_slug == Some(configured) => {
            stored_id.cloned().or_else(|| Some(configured.clone()))
        }
        Some(configured) => Some(configured.clone()),
        None => stored_id.cloned(),
    }
}

fn nonempty_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

fn read_bounded_response(
    response: &mut ureq::http::Response<ureq::Body>,
    label: &str,
) -> Result<Zeroizing<Vec<u8>>, ProviderError> {
    read_bounded_response_with_limit(response, label, MAX_OAUTH_BODY_BYTES)
}

fn read_bounded_response_with_limit(
    response: &mut ureq::http::Response<ureq::Body>,
    label: &str,
    limit: u64,
) -> Result<Zeroizing<Vec<u8>>, ProviderError> {
    response
        .body_mut()
        .with_config()
        .limit(limit)
        .read_to_vec()
        .map(Zeroizing::new)
        .map_err(|error| ProviderError::Authentication(format!("could not read {label}: {error}")))
}

#[derive(Deserialize)]
struct OAuthErrorBody<'a> {
    #[serde(borrow)]
    error: &'a str,
    #[serde(default, borrow)]
    error_description: Option<&'a str>,
}

fn oauth_error_code(bytes: &[u8]) -> Option<&str> {
    serde_json::from_slice::<OAuthErrorBody<'_>>(bytes)
        .ok()
        .map(|body| body.error)
}

fn oauth_status_error(status: u16, bytes: &[u8], operation: &str) -> ProviderError {
    let detail = serde_json::from_slice::<OAuthErrorBody<'_>>(bytes)
        .ok()
        .map(|body| {
            body.error_description.map_or_else(
                || body.error.into(),
                |description| format!("{}: {description}", body.error),
            )
        })
        .unwrap_or_else(|| format!("HTTP {status}"));
    ProviderError::Authentication(format!("Vercel OAuth {operation} failed: {detail}"))
}

#[derive(Clone, Copy)]
enum EndpointKind {
    Gateway,
    VercelApi,
}

fn validate_issuer(value: &str) -> Result<(), ProviderError> {
    let normalized = value.trim_end_matches('/');
    if normalized == ISSUER || is_loopback_origin(normalized) {
        Ok(())
    } else {
        Err(ProviderError::Configuration(
            "Vercel OAuth issuer must be vercel.com or an explicit loopback test origin".into(),
        ))
    }
}

fn validate_client_id(value: &str) -> Result<(), ProviderError> {
    if !value.trim().is_empty() && value.len() <= 256 {
        Ok(())
    } else {
        Err(ProviderError::Configuration(
            "Vercel OAuth client id is empty or too long".into(),
        ))
    }
}

fn validate_trusted_endpoint(value: &str, kind: EndpointKind) -> Result<(), ProviderError> {
    let url = Url::parse(value)
        .map_err(|error| ProviderError::Configuration(format!("invalid endpoint: {error}")))?;
    let production = url.scheme() == "https"
        && match kind {
            EndpointKind::Gateway => url.host_str() == Some("ai-gateway.vercel.sh"),
            EndpointKind::VercelApi => url.host_str() == Some("api.vercel.com"),
        };
    if production || is_loopback_url(&url) {
        Ok(())
    } else {
        Err(ProviderError::Configuration(
            "Vercel endpoint must use its production origin or loopback".into(),
        ))
    }
}

fn validate_oauth_endpoint(issuer: &str, value: &str) -> Result<(), ProviderError> {
    let url = Url::parse(value).map_err(|error| {
        ProviderError::Authentication(format!("Vercel OAuth endpoint was invalid: {error}"))
    })?;
    let valid = if issuer.trim_end_matches('/') == ISSUER {
        url.scheme() == "https" && matches!(url.host_str(), Some("vercel.com" | "api.vercel.com"))
    } else {
        let issuer = Url::parse(issuer)
            .map_err(|_| ProviderError::Authentication("Vercel OAuth issuer was invalid".into()))?;
        is_loopback_url(&url)
            && url.host_str() == issuer.host_str()
            && url.port_or_known_default() == issuer.port_or_known_default()
    };
    if valid {
        Ok(())
    } else {
        Err(ProviderError::Authentication(
            "Vercel OAuth discovery returned an untrusted endpoint".into(),
        ))
    }
}

fn is_loopback_origin(value: &str) -> bool {
    Url::parse(value).is_ok_and(|url| {
        is_loopback_url(&url)
            && url.path() == "/"
            && url.query().is_none()
            && url.fragment().is_none()
            && url.port().is_some()
    })
}

fn is_loopback_url(url: &Url) -> bool {
    url.scheme() == "http"
        && matches!(
            url.host_str(),
            Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
        )
        && url.username().is_empty()
        && url.password().is_none()
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or(i64::MAX)
}

fn open_browser(url: &str) -> Result<(), ProviderError> {
    #[cfg(target_os = "macos")]
    let mut command = Command::new("/usr/bin/open");
    #[cfg(target_os = "linux")]
    let mut command = Command::new("xdg-open");
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = Command::new("cmd");
        command.args(["/C", "start", ""]);
        command
    };
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    return Err(ProviderError::Authentication(format!(
        "open this URL in a browser to authenticate: {url}"
    )));
    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
    {
        command
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| {
                ProviderError::Authentication(format!("could not open login browser: {error}"))
            })?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::Mutex;

    use crate::{CredentialLease, CredentialStore};

    use super::*;

    #[derive(Default)]
    struct MemoryStore(Mutex<Option<Credential>>);

    struct MemoryLease<'a>(std::sync::MutexGuard<'a, Option<Credential>>);

    impl CredentialLease for MemoryLease<'_> {
        fn credential(&self) -> Option<&Credential> {
            self.0.as_ref()
        }

        fn replace(&mut self, credential: Credential) -> Result<(), ProviderError> {
            *self.0 = Some(credential);
            Ok(())
        }

        fn delete(&mut self) -> Result<(), ProviderError> {
            *self.0 = None;
            Ok(())
        }
    }

    impl CredentialStore for MemoryStore {
        fn lock<'a>(
            &'a self,
            _provider_id: &str,
        ) -> Result<Box<dyn CredentialLease + 'a>, ProviderError> {
            Ok(Box::new(MemoryLease(self.0.lock().unwrap())))
        }
    }

    fn test_config() -> VercelProviderConfig {
        VercelProviderConfig {
            issuer: "http://127.0.0.1:1234".into(),
            client_id: "client".into(),
            gateway_endpoint: "http://127.0.0.1:1234/v3/ai/language-model".into(),
            catalog_endpoint: "http://127.0.0.1:1234/coding-agent/v1/models".into(),
            teams_endpoint: "http://127.0.0.1:1234/v2/teams".into(),
            team: None,
            open_browser: false,
            auth_timeout: Duration::from_secs(1),
            additional_models: Vec::new(),
            routing: VercelRoutingPolicy::default(),
        }
    }

    fn read_http_request(stream: &mut TcpStream) -> String {
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut bytes = Vec::new();
        let header_end = loop {
            let mut chunk = [0_u8; 4096];
            let count = stream.read(&mut chunk).unwrap();
            assert!(count > 0);
            bytes.extend_from_slice(&chunk[..count]);
            if let Some(position) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                break position + 4;
            }
        };
        let headers = String::from_utf8_lossy(&bytes[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0);
        while bytes.len() < header_end + content_length {
            let mut chunk = [0_u8; 4096];
            let count = stream.read(&mut chunk).unwrap();
            assert!(count > 0);
            bytes.extend_from_slice(&chunk[..count]);
        }
        String::from_utf8(bytes).unwrap()
    }

    fn respond_json(stream: &mut TcpStream, body: &str) {
        respond(stream, 200, "OK", body);
    }

    fn respond(stream: &mut TcpStream, status: u16, reason: &str, body: &str) {
        write!(
            stream,
            "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .unwrap();
    }

    #[test]
    fn advertises_vercel_models_and_device_auth() {
        let provider = VercelProvider::new(test_config()).unwrap();
        assert_eq!(provider.default_model(), "zai/glm-5.2");
        assert!(
            provider
                .models()
                .iter()
                .any(|model| model.id == "openai/gpt-5.5")
        );
        assert!(
            !provider
                .models()
                .iter()
                .any(|model| model.id == "blackbox/zai/glm-5.2")
        );
        assert_eq!(provider.auth_methods()[0].id, "oauth");
    }

    #[test]
    fn additional_models_extend_the_static_catalog_without_network_io() {
        let mut config = test_config();
        config.additional_models = vec!["future/model-1".into(), DEFAULT_MODEL.into()];
        let provider = VercelProvider::new(config).unwrap();
        assert_eq!(
            provider
                .models()
                .iter()
                .filter(|model| model.id == DEFAULT_MODEL)
                .count(),
            1
        );
        assert!(
            provider
                .models()
                .iter()
                .any(|model| model.id == "future/model-1")
        );
    }

    #[test]
    fn authenticated_catalog_uses_fx_login_team_query_and_metadata() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!(
            "http://{}/coding-agent/v1/models",
            listener.local_addr().unwrap()
        );
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_http_request(&mut stream);
            respond_json(
                &mut stream,
                r#"{"data":[{"id":"zai/glm-5.2","name":"GLM 5.2","type":"language","context_window":1000000,"max_tokens":128000,"tags":["reasoning","tool-use"]},{"id":"image/model","type":"image"},{"id":42,"type":"language"}]}"#,
            );
            request
        });

        let mut config = test_config();
        config.catalog_endpoint = endpoint;
        let provider = VercelProvider::new(config).unwrap();
        let mut attributes = BTreeMap::new();
        attributes.insert(TEAM_ID_ATTRIBUTE.into(), "team one".into());
        let store = MemoryStore(Mutex::new(Some(Credential::OAuth {
            access_token: "catalog-secret".into(),
            refresh_token: Some("refresh-secret".into()),
            expires_at_ms: now_ms() + 3_600_000,
            attributes,
        })));

        let models = provider.refresh_models(&store).unwrap().unwrap();
        let model = models
            .iter()
            .find(|model| model.id == DEFAULT_MODEL)
            .unwrap();
        assert_eq!(model.name, "GLM 5.2");
        assert_eq!(model.context_window, 1_000_000);
        assert_eq!(model.max_output_tokens, 128_000);
        assert!(model.reasoning);
        assert_eq!(models.len(), 1);

        let request = server.join().unwrap();
        assert!(request.starts_with("GET /coding-agent/v1/models?teamId=team+one "));
        assert!(request.contains("authorization: Bearer catalog-secret"));
    }

    #[test]
    fn rejected_authenticated_catalog_falls_back_to_public_catalog() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!(
            "http://{}/coding-agent/v1/models",
            listener.local_addr().unwrap()
        );
        let server = thread::spawn(move || {
            let mut requests = Vec::new();
            let (mut authenticated, _) = listener.accept().unwrap();
            requests.push(read_http_request(&mut authenticated));
            respond(&mut authenticated, 401, "Unauthorized", "{}");
            let (mut public, _) = listener.accept().unwrap();
            requests.push(read_http_request(&mut public));
            respond_json(
                &mut public,
                r#"{"data":[{"id":"zai/glm-5.2","type":"language"}]}"#,
            );
            requests
        });

        let mut config = test_config();
        config.catalog_endpoint = endpoint;
        let provider = VercelProvider::new(config).unwrap();
        let store = MemoryStore(Mutex::new(Some(Credential::ApiKey {
            secret: "gateway-key".into(),
            attributes: BTreeMap::new(),
        })));
        let models = provider.refresh_models(&store).unwrap().unwrap();
        assert_eq!(models.len(), 1);

        let requests = server.join().unwrap();
        assert!(requests[0].contains("authorization: Bearer gateway-key"));
        assert!(!requests[1].contains("authorization:"));
    }

    #[test]
    fn api_key_credential_constructs_a_nested_model_gateway() {
        let provider = VercelProvider::new(test_config()).unwrap();
        let store = MemoryStore(Mutex::new(Some(Credential::ApiKey {
            secret: "gateway-key".into(),
            attributes: BTreeMap::new(),
        })));
        assert!(
            provider
                .gateway(DEFAULT_MODEL, Some("session"), &store)
                .is_ok()
        );
        assert!(
            provider
                .gateway("blackbox/zai/glm-5.2", Some("session"), &store)
                .is_err()
        );
        assert!(provider.gateway("missing/model", None, &store).is_err());
    }

    #[test]
    fn validates_generic_gateway_provider_routing() {
        let mut config = test_config();
        config.routing = VercelRoutingPolicy {
            only: vec!["blackbox".into()],
            order: vec!["blackbox".into(), "zai".into()],
        };
        assert!(VercelProvider::new(config).is_ok());

        let mut invalid = test_config();
        invalid.routing.only = vec!["blackbox/zai".into()];
        assert!(matches!(
            VercelProvider::new(invalid),
            Err(ProviderError::Configuration(message))
                if message.contains("provider route")
        ));
    }

    #[test]
    fn token_parser_preserves_rotating_refresh_and_scope() {
        let token = parse_token(
            br#"{"access_token":"access","refresh_token":"next","expires_in":3600,"scope":"openid offline_access","token_type":"Bearer"}"#,
            Some("old"),
            None,
        )
        .unwrap();
        assert_eq!(token.refresh_token, "next");
        assert_eq!(token.scope, OAUTH_SCOPE);

        let token = parse_token(
            br#"{"access_token":"access","expires_in":3600,"token_type":"bearer"}"#,
            Some("old"),
            Some(OAUTH_SCOPE),
        )
        .unwrap();
        assert_eq!(token.refresh_token, "old");
        assert_eq!(token.scope, OAUTH_SCOPE);
    }

    #[test]
    fn team_selection_is_deterministic_and_configurable() {
        let teams = vec![
            Team {
                id: "team_a".into(),
                slug: "alpha".into(),
                _name: None,
            },
            Team {
                id: "team_b".into(),
                slug: "beta".into(),
                _name: None,
            },
        ];
        assert_eq!(
            select_team(&teams, None).unwrap().id.as_deref(),
            Some("team_a")
        );
        assert_eq!(
            select_team(&teams, Some("beta")).unwrap().id.as_deref(),
            Some("team_b")
        );
        assert!(select_team(&teams, Some("missing")).is_err());
    }

    #[test]
    fn rejects_non_vercel_and_non_loopback_endpoints() {
        let mut config = test_config();
        config.gateway_endpoint = "https://example.com/v3/ai/language-model".into();
        assert!(VercelProvider::new(config).is_err());
        assert!(
            validate_oauth_endpoint(ISSUER, "https://api.vercel.com/login/oauth/token").is_ok()
        );
        assert!(validate_oauth_endpoint(ISSUER, "https://example.com/token").is_err());
    }

    #[test]
    fn device_oauth_discovers_polls_selects_team_and_persists() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let issuer = format!("http://{}", listener.local_addr().unwrap());
        let server_issuer = issuer.clone();
        let server = thread::spawn(move || {
            let mut requests = Vec::new();
            for response_body in [
                format!(
                    "{{\"issuer\":\"{server_issuer}\",\"device_authorization_endpoint\":\"{server_issuer}/device\",\"token_endpoint\":\"{server_issuer}/token\"}}"
                ),
                format!(
                    "{{\"device_code\":\"device-secret\",\"user_code\":\"ABCD-EFGH\",\"verification_uri\":\"{server_issuer}/verify\",\"expires_in\":30,\"interval\":0}}"
                ),
                "{\"access_token\":\"access-secret\",\"refresh_token\":\"refresh-secret\",\"expires_in\":3600,\"scope\":\"openid offline_access\",\"token_type\":\"Bearer\"}".into(),
                "{\"teams\":[{\"id\":\"team_1\",\"slug\":\"alpha\",\"name\":\"Alpha\"}]}".into(),
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                requests.push(read_http_request(&mut stream));
                respond_json(&mut stream, &response_body);
            }
            requests
        });

        let mut config = test_config();
        config.issuer = issuer.clone();
        config.gateway_endpoint = format!("{issuer}/v3/ai/language-model");
        config.teams_endpoint = format!("{issuer}/v2/teams");
        config.auth_timeout = Duration::from_secs(3);
        let provider = VercelProvider::new(config).unwrap();
        let store = MemoryStore::default();
        provider.authenticate(DEVICE_AUTH_METHOD, &store).unwrap();

        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /.well-known/openid-configuration "));
        assert!(requests[1].contains("client_id=client"));
        assert!(
            requests[1].contains("scope=openid%20offline_access")
                || requests[1].contains("scope=openid+offline_access")
        );
        assert!(requests[2].contains("device_code=device-secret"));
        assert!(requests[3].contains("authorization: Bearer access-secret"));

        let lease = store.lock(PROVIDER_ID).unwrap();
        let Credential::OAuth {
            refresh_token,
            attributes,
            ..
        } = lease.credential().unwrap()
        else {
            panic!("device OAuth did not persist an OAuth credential")
        };
        assert_eq!(refresh_token.as_deref(), Some("refresh-secret"));
        assert_eq!(
            attributes.get(TEAM_ID_ATTRIBUTE).map(String::as_str),
            Some("team_1")
        );
        assert_eq!(
            attributes.get(TEAM_SLUG_ATTRIBUTE).map(String::as_str),
            Some("alpha")
        );
    }
}
