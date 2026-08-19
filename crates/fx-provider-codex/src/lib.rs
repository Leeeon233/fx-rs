//! OpenAI Codex provider: model catalog, ChatGPT OAuth, ambient Codex
//! compatibility, and Responses transport construction.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::prelude::{BASE64_URL_SAFE_NO_PAD, Engine as _};
use fx_core::{Gateway, SafeRetryGateway};
use fx_gateway::{CodexGateway, CodexGatewayConfig, codex_endpoint_from_base};
use fx_provider::{
    AuthMethod, Credential, CredentialStore, Model, ModelCapabilities, NativeWebSearch, Provider,
    ProviderError,
};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use url::Url;
use uuid::Uuid;
use zeroize::Zeroizing;

pub const PROVIDER_ID: &str = "codex";
pub const DEFAULT_MODEL: &str = "gpt-5.6-sol";
pub const BROWSER_AUTH_METHOD: &str = "chatgpt";

const PROVIDER_NAME: &str = "OpenAI Codex";
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const AUTH_BASE_URL: &str = "https://auth.openai.com";
const SCOPE: &str = "openid profile email offline_access";
const ACCOUNT_ATTRIBUTE: &str = "account_id";
const JWT_AUTH_CLAIM: &str = "https://api.openai.com/auth";
const REFRESH_SKEW_MS: i64 = 5 * 60 * 1000;
const AUTH_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const MAX_AUTH_FILE_BYTES: u64 = 64 * 1024;
const MAX_TOKEN_BODY_BYTES: u64 = 64 * 1024;
const MAX_HTTP_HEADER_BYTES: usize = 32 * 1024;

#[derive(Clone, Debug)]
pub struct CodexProviderConfig {
    pub home: Option<PathBuf>,
    pub responses_endpoint: String,
    pub auth_base_url: String,
    pub callback_addr: SocketAddr,
    pub open_browser: bool,
}

impl CodexProviderConfig {
    pub fn from_process(home: Option<PathBuf>) -> Self {
        Self {
            home,
            responses_endpoint: codex_endpoint_from_base(
                std::env::var("FX_CODEX_BASE_URL").ok().as_deref(),
            ),
            auth_base_url: AUTH_BASE_URL.into(),
            callback_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 1455),
            open_browser: true,
        }
    }
}

#[derive(Clone, Debug)]
pub struct CodexProvider {
    config: CodexProviderConfig,
    http: ureq::Agent,
}

impl CodexProvider {
    pub fn new(config: CodexProviderConfig) -> Result<Self, ProviderError> {
        validate_auth_base(&config.auth_base_url)?;
        if !config.callback_addr.ip().is_loopback() {
            return Err(ProviderError::Configuration(
                "Codex OAuth callback must bind to loopback".into(),
            ));
        }
        Ok(Self {
            config,
            http: ureq::Agent::new_with_defaults(),
        })
    }

    pub fn from_process(home: Option<PathBuf>) -> Self {
        Self::new(CodexProviderConfig::from_process(home))
            .expect("built-in Codex provider configuration is valid")
    }

    fn model(&self, id: &str) -> Result<Model, ProviderError> {
        model_catalog()
            .into_iter()
            .find(|model| model.id == id)
            .ok_or_else(|| ProviderError::UnknownModel(format!("{PROVIDER_ID}/{id}")))
    }

    fn resolve_auth(
        &self,
        credentials: &dyn CredentialStore,
    ) -> Result<ResolvedCodexAuth, ProviderError> {
        let mut lease = credentials.lock(PROVIDER_ID)?;
        if let Some(credential) = lease.credential() {
            let Credential::OAuth {
                access_token,
                refresh_token,
                expires_at_ms,
                attributes,
            } = credential
            else {
                return Err(ProviderError::Authentication(
                    "the saved Codex credential has the wrong kind".into(),
                ));
            };
            let account_id = attributes
                .get(ACCOUNT_ATTRIBUTE)
                .filter(|value| !value.is_empty())
                .cloned()
                .or_else(|| account_id_from_jwt(access_token))
                .ok_or_else(|| {
                    ProviderError::Authentication(
                        "the saved Codex credential has no ChatGPT account id".into(),
                    )
                })?;
            if expires_at_ms.saturating_sub(REFRESH_SKEW_MS) > now_ms() {
                return Ok(ResolvedCodexAuth {
                    access_token: Zeroizing::new(access_token.clone()),
                    account_id,
                });
            }
            let refresh_token = refresh_token.as_deref().ok_or_else(|| {
                ProviderError::Authentication(
                    "the saved Codex session expired and cannot be refreshed; authenticate again"
                        .into(),
                )
            })?;
            // The provider lock stays held across refresh. A second process
            // will observe only the new complete credential, never a partial
            // or concurrently rotated refresh token.
            let refreshed = self.refresh_token(refresh_token)?;
            let resolved = resolved_from_token(&refreshed)?;
            lease.replace(token_credential(refreshed)?)?;
            return Ok(resolved);
        }
        drop(lease);

        if let Some(auth) = self.load_ambient_codex_auth()? {
            return Ok(auth);
        }
        Err(ProviderError::AuthenticationRequired {
            provider: PROVIDER_NAME.into(),
            message: "call ACP authenticate with method `codex:chatgpt`, or run `codex login` to reuse its local ChatGPT session".into(),
        })
    }

    fn load_ambient_codex_auth(&self) -> Result<Option<ResolvedCodexAuth>, ProviderError> {
        let Some(home) = self.config.home.as_deref() else {
            return Ok(None);
        };
        let path = home.join(".codex").join("auth.json");
        let Some(bytes) = read_private_file(&path)? else {
            return Ok(None);
        };
        let file: AmbientAuthFile = serde_json::from_slice(&bytes).map_err(|_| {
            ProviderError::Authentication(format!(
                "ambient Codex credential `{}` is invalid",
                path.display()
            ))
        })?;
        let Some(tokens) = file.tokens else {
            return Ok(None);
        };
        if tokens.access_token.trim().is_empty() {
            return Ok(None);
        }
        let expires_at_ms = jwt_expiry_ms(&tokens.access_token).ok_or_else(|| {
            ProviderError::Authentication(
                "ambient Codex access token has no valid expiry; run `codex login`".into(),
            )
        })?;
        if expires_at_ms.saturating_sub(REFRESH_SKEW_MS) <= now_ms() {
            return Err(ProviderError::Authentication(
                "ambient Codex session is expired; run `codex login` or use ACP authenticate"
                    .into(),
            ));
        }
        let account_id = tokens
            .account_id
            .filter(|value| !value.is_empty())
            .or_else(|| account_id_from_jwt(&tokens.access_token))
            .ok_or_else(|| {
                ProviderError::Authentication(
                    "ambient Codex session has no ChatGPT account id".into(),
                )
            })?;
        Ok(Some(ResolvedCodexAuth {
            access_token: Zeroizing::new(tokens.access_token),
            account_id,
        }))
    }

    fn browser_login(&self) -> Result<OAuthToken, ProviderError> {
        let listener = TcpListener::bind(self.config.callback_addr).map_err(|error| {
            ProviderError::Authentication(format!(
                "could not bind Codex OAuth callback on {}: {error}",
                self.config.callback_addr
            ))
        })?;
        listener.set_nonblocking(true).map_err(|error| {
            ProviderError::Authentication(format!("could not configure OAuth callback: {error}"))
        })?;

        let verifier = Zeroizing::new(format!(
            "{}{}",
            Uuid::new_v4().simple(),
            Uuid::new_v4().simple()
        ));
        let challenge = BASE64_URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let state = Uuid::new_v4().simple().to_string();
        let redirect_uri = format!(
            "http://localhost:{}/auth/callback",
            self.config.callback_addr.port()
        );
        let mut auth_url = Url::parse(&format!(
            "{}/oauth/authorize",
            self.config.auth_base_url.trim_end_matches('/')
        ))
        .map_err(|error| ProviderError::Configuration(error.to_string()))?;
        auth_url
            .query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", CLIENT_ID)
            .append_pair("redirect_uri", &redirect_uri)
            .append_pair("scope", SCOPE)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", &state)
            .append_pair("id_token_add_organizations", "true")
            .append_pair("codex_cli_simplified_flow", "true")
            .append_pair("originator", "fxrs");
        if self.config.open_browser {
            open_browser(auth_url.as_str())?;
        }

        let code = wait_for_callback(&listener, &state, AUTH_TIMEOUT)?;
        self.exchange_code(&code, &verifier, &redirect_uri)
    }

    fn exchange_code(
        &self,
        code: &str,
        verifier: &str,
        redirect_uri: &str,
    ) -> Result<OAuthToken, ProviderError> {
        self.token_request(
            &[
                ("grant_type", "authorization_code"),
                ("client_id", CLIENT_ID),
                ("code", code),
                ("code_verifier", verifier),
                ("redirect_uri", redirect_uri),
            ],
            None,
        )
    }

    fn refresh_token(&self, refresh_token: &str) -> Result<OAuthToken, ProviderError> {
        self.token_request(
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token),
                ("client_id", CLIENT_ID),
            ],
            Some(refresh_token),
        )
    }

    fn token_request(
        &self,
        form: &[(&str, &str)],
        previous_refresh: Option<&str>,
    ) -> Result<OAuthToken, ProviderError> {
        let endpoint = format!(
            "{}/oauth/token",
            self.config.auth_base_url.trim_end_matches('/')
        );
        let mut response = self
            .http
            .post(&endpoint)
            .send_form(form.iter().copied())
            .map_err(|error| {
                ProviderError::Authentication(format!("Codex OAuth token request failed: {error}"))
            })?;
        let bytes = response
            .body_mut()
            .with_config()
            .limit(MAX_TOKEN_BODY_BYTES)
            .read_to_vec()
            .map_err(|error| {
                ProviderError::Authentication(format!("could not read Codex OAuth token: {error}"))
            })?;
        let response: TokenResponse = serde_json::from_slice(&bytes).map_err(|_| {
            ProviderError::Authentication("Codex OAuth token response was invalid".into())
        })?;
        if response.access_token.is_empty() || response.expires_in == 0 {
            return Err(ProviderError::Authentication(
                "Codex OAuth token response was incomplete".into(),
            ));
        }
        let refresh_token = response
            .refresh_token
            .filter(|value| !value.is_empty())
            .or_else(|| previous_refresh.map(str::to_owned))
            .ok_or_else(|| {
                ProviderError::Authentication(
                    "Codex OAuth token response omitted its refresh token".into(),
                )
            })?;
        let expires_ms = i64::try_from(response.expires_in)
            .ok()
            .and_then(|seconds| seconds.checked_mul(1000))
            .and_then(|duration| now_ms().checked_add(duration))
            .ok_or_else(|| {
                ProviderError::Authentication("Codex OAuth expiry was invalid".into())
            })?;
        Ok(OAuthToken {
            access_token: response.access_token,
            refresh_token,
            expires_at_ms: expires_ms,
        })
    }
}

impl Provider for CodexProvider {
    fn id(&self) -> &str {
        PROVIDER_ID
    }

    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    fn models(&self) -> Vec<Model> {
        model_catalog()
    }

    fn default_model(&self) -> &str {
        DEFAULT_MODEL
    }

    fn auth_methods(&self) -> Vec<AuthMethod> {
        vec![AuthMethod::new(
            BROWSER_AUTH_METHOD,
            "Sign in with ChatGPT",
            "Open a browser and authorize Fx for an OpenAI Codex subscription.",
        )]
    }

    fn authenticate(
        &self,
        method_id: &str,
        credentials: &dyn CredentialStore,
    ) -> Result<(), ProviderError> {
        if method_id != BROWSER_AUTH_METHOD {
            return Err(ProviderError::UnknownAuthMethod(format!(
                "{PROVIDER_ID}:{method_id}"
            )));
        }
        let token = self.browser_login()?;
        let credential = token_credential(token)?;
        credentials.lock(PROVIDER_ID)?.replace(credential)
    }

    fn gateway(
        &self,
        model_id: &str,
        session_id: Option<&str>,
        credentials: &dyn CredentialStore,
    ) -> Result<Arc<dyn Gateway>, ProviderError> {
        self.model(model_id)?;
        let auth = self.resolve_auth(credentials)?;
        let mut config =
            CodexGatewayConfig::new(model_id, auth.access_token.as_str(), auth.account_id);
        config.endpoint = self.config.responses_endpoint.clone();
        config.session_id = session_id.map(str::to_owned);
        config.originator = "fxrs".into();
        Ok(Arc::new(SafeRetryGateway::new(Arc::new(
            CodexGateway::new(config),
        ))))
    }
}

fn model_catalog() -> Vec<Model> {
    [
        ("gpt-5.3-codex-spark", 128_000),
        ("gpt-5.4", 272_000),
        ("gpt-5.4-mini", 272_000),
        ("gpt-5.5", 272_000),
        ("gpt-5.6-luna", 272_000),
        ("gpt-5.6-sol", 272_000),
        ("gpt-5.6-terra", 272_000),
    ]
    .into_iter()
    .map(|(id, context_window)| Model {
        provider_id: PROVIDER_ID.into(),
        id: id.into(),
        name: id.into(),
        context_window,
        max_output_tokens: 128_000,
        reasoning: true,
        capabilities: ModelCapabilities {
            native_web_search: Some(NativeWebSearch {
                provider_tool_id: fx_gateway::CODEX_WEB_SEARCH_TOOL_ID.into(),
            }),
        },
    })
    .collect()
}

struct ResolvedCodexAuth {
    access_token: Zeroizing<String>,
    account_id: String,
}

struct OAuthToken {
    access_token: String,
    refresh_token: String,
    expires_at_ms: i64,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    expires_in: u64,
}

fn token_credential(token: OAuthToken) -> Result<Credential, ProviderError> {
    let account_id = account_id_from_jwt(&token.access_token).ok_or_else(|| {
        ProviderError::Authentication(
            "Codex access token did not contain a ChatGPT account id".into(),
        )
    })?;
    let mut attributes = BTreeMap::new();
    attributes.insert(ACCOUNT_ATTRIBUTE.into(), account_id);
    Ok(Credential::OAuth {
        access_token: token.access_token,
        refresh_token: Some(token.refresh_token),
        expires_at_ms: token.expires_at_ms,
        attributes,
    })
}

fn resolved_from_token(token: &OAuthToken) -> Result<ResolvedCodexAuth, ProviderError> {
    let account_id = account_id_from_jwt(&token.access_token).ok_or_else(|| {
        ProviderError::Authentication(
            "Codex access token did not contain a ChatGPT account id".into(),
        )
    })?;
    Ok(ResolvedCodexAuth {
        access_token: Zeroizing::new(token.access_token.clone()),
        account_id,
    })
}

fn account_id_from_jwt(token: &str) -> Option<String> {
    let payload = decode_jwt(token)?;
    payload
        .get(JWT_AUTH_CLAIM)?
        .get("chatgpt_account_id")?
        .as_str()
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn jwt_expiry_ms(token: &str) -> Option<i64> {
    let seconds = decode_jwt(token)?.get("exp")?.as_i64()?;
    seconds.checked_mul(1000)
}

fn decode_jwt(token: &str) -> Option<Value> {
    let mut parts = token.split('.');
    let _header = parts.next()?;
    let payload = parts.next()?;
    let _signature = parts.next()?;
    if parts.next().is_some() || payload.len() > MAX_AUTH_FILE_BYTES as usize {
        return None;
    }
    let bytes = BASE64_URL_SAFE_NO_PAD.decode(payload).ok()?;
    serde_json::from_slice(&bytes).ok()
}

#[derive(Deserialize)]
struct AmbientAuthFile {
    #[serde(default)]
    tokens: Option<AmbientTokens>,
}

#[derive(Deserialize)]
struct AmbientTokens {
    access_token: String,
    #[serde(default)]
    account_id: Option<String>,
}

fn read_private_file(path: &Path) -> Result<Option<Zeroizing<Vec<u8>>>, ProviderError> {
    let file = match open_read_no_follow(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(ProviderError::Authentication(format!(
                "could not open ambient Codex credential: {error}"
            )));
        }
    };
    let metadata = file.metadata().map_err(|error| {
        ProviderError::Authentication(format!(
            "could not inspect ambient Codex credential: {error}"
        ))
    })?;
    if !metadata.is_file() || !private_file_permissions(&metadata) {
        return Err(ProviderError::Authentication(format!(
            "ambient Codex credential `{}` is not a private regular file",
            path.display()
        )));
    }
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(MAX_AUTH_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            ProviderError::Authentication(format!(
                "could not read ambient Codex credential: {error}"
            ))
        })?;
    if bytes.len() as u64 > MAX_AUTH_FILE_BYTES {
        return Err(ProviderError::Authentication(
            "ambient Codex credential exceeds 64 KiB".into(),
        ));
    }
    Ok(Some(bytes))
}

fn open_read_no_follow(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    options.open(path)
}

fn private_file_permissions(metadata: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o077 == 0
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        true
    }
}

fn validate_auth_base(value: &str) -> Result<(), ProviderError> {
    let url = Url::parse(value)
        .map_err(|error| ProviderError::Configuration(format!("invalid auth base: {error}")))?;
    let production = url.scheme() == "https" && url.host_str() == Some("auth.openai.com");
    let loopback = url.scheme() == "http"
        && url
            .host_str()
            .is_some_and(|host| host == "localhost" || host == "127.0.0.1");
    if production || loopback {
        Ok(())
    } else {
        Err(ProviderError::Configuration(
            "Codex auth base must be auth.openai.com or loopback".into(),
        ))
    }
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

fn wait_for_callback(
    listener: &TcpListener,
    expected_state: &str,
    timeout: Duration,
) -> Result<Zeroizing<String>, ProviderError> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match listener.accept() {
            Ok((mut stream, _)) => match parse_callback(&stream, expected_state) {
                Ok(Callback::Code(code)) => {
                    respond_html(
                        &mut stream,
                        200,
                        "Authentication complete. You can close this window.",
                    );
                    return Ok(Zeroizing::new(code));
                }
                Ok(Callback::Ignore) => {
                    respond_html(&mut stream, 404, "OAuth callback route not found.");
                }
                Err(error) => {
                    respond_html(
                        &mut stream,
                        400,
                        "Authentication failed. Return to your ACP client.",
                    );
                    return Err(error);
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(25));
            }
            Err(error) => {
                return Err(ProviderError::Authentication(format!(
                    "OAuth callback failed: {error}"
                )));
            }
        }
    }
    Err(ProviderError::Authentication(
        "timed out waiting for Codex browser login".into(),
    ))
}

enum Callback {
    Code(String),
    Ignore,
}

fn parse_callback(stream: &TcpStream, expected_state: &str) -> Result<Callback, ProviderError> {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|error| ProviderError::Authentication(error.to_string()))?;
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    reader.read_line(&mut request_line).map_err(|error| {
        ProviderError::Authentication(format!("could not read OAuth callback: {error}"))
    })?;
    if request_line.len() > MAX_HTTP_HEADER_BYTES {
        return Err(ProviderError::Authentication(
            "OAuth callback request was too large".into(),
        ));
    }
    let target = request_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| ProviderError::Authentication("invalid OAuth callback request".into()))?;
    let url = Url::parse(&format!("http://localhost{target}"))
        .map_err(|_| ProviderError::Authentication("invalid OAuth callback URL".into()))?;
    if url.path() != "/auth/callback" {
        return Ok(Callback::Ignore);
    }
    let parameters: BTreeMap<_, _> = url.query_pairs().into_owned().collect();
    if parameters.get("state").map(String::as_str) != Some(expected_state) {
        return Err(ProviderError::Authentication(
            "OAuth callback state did not match".into(),
        ));
    }
    if let Some(error) = parameters.get("error") {
        return Err(ProviderError::Authentication(format!(
            "Codex authorization was denied: {error}"
        )));
    }
    let code = parameters
        .get("code")
        .filter(|value| !value.is_empty() && value.len() <= 16 * 1024)
        .cloned()
        .ok_or_else(|| ProviderError::Authentication("OAuth callback omitted its code".into()))?;
    Ok(Callback::Code(code))
}

fn respond_html(stream: &mut TcpStream, status: u16, message: &str) {
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>Fx authentication</title><p>{message}</p>"
    );
    let reason = if status == 200 { "OK" } else { "Error" };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fx_auth::FileCredentialStore;
    use fx_provider::{CredentialStore, ProviderRegistry};

    fn temporary(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "fx-provider-codex-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn test_jwt(account_id: &str, expiry_ms: i64) -> String {
        let payload = json_object(account_id, expiry_ms / 1000);
        format!(
            "e30.{}.signature",
            BASE64_URL_SAFE_NO_PAD.encode(payload.as_bytes())
        )
    }

    fn json_object(account_id: &str, expiry: i64) -> String {
        serde_json::json!({
            "exp": expiry,
            JWT_AUTH_CLAIM: {"chatgpt_account_id": account_id}
        })
        .to_string()
    }

    fn read_http_request(stream: &mut TcpStream) -> String {
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut bytes = Vec::new();
        let header_end = loop {
            let mut chunk = [0_u8; 4096];
            let count = stream.read(&mut chunk).unwrap();
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
            bytes.extend_from_slice(&chunk[..count]);
        }
        String::from_utf8(bytes[header_end..header_end + content_length].to_vec()).unwrap()
    }

    #[test]
    fn catalog_registers_one_provider_with_explicit_default() {
        let mut registry = ProviderRegistry::new();
        registry
            .register(Arc::new(CodexProvider::from_process(None)))
            .unwrap();
        assert_eq!(
            registry.default_model().unwrap().route(),
            "codex/gpt-5.6-sol"
        );
        assert_eq!(registry.models().len(), 7);
        assert_eq!(registry.auth_methods()[0].id, "codex:chatgpt");
    }

    #[test]
    fn owned_valid_oauth_builds_gateway_without_ambient_state() {
        let root = temporary("owned");
        let store = FileCredentialStore::new(root.join("credentials"));
        let token = test_jwt("acct_1", now_ms() + 60 * 60 * 1000);
        store
            .lock(PROVIDER_ID)
            .unwrap()
            .replace(Credential::OAuth {
                access_token: token,
                refresh_token: Some("refresh".into()),
                expires_at_ms: now_ms() + 60 * 60 * 1000,
                attributes: BTreeMap::from([(ACCOUNT_ATTRIBUTE.into(), "acct_1".into())]),
            })
            .unwrap();
        let provider = CodexProvider::from_process(None);
        assert!(
            provider
                .gateway(DEFAULT_MODEL, Some("session"), &store)
                .is_ok()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ambient_codex_auth_is_read_only_compatible() {
        let home = temporary("ambient");
        let directory = home.join(".codex");
        fs::create_dir_all(&directory).unwrap();
        let auth = directory.join("auth.json");
        let token = test_jwt("acct_ambient", now_ms() + 60 * 60 * 1000);
        fs::write(
            &auth,
            serde_json::json!({
                "auth_mode": "chatgpt",
                "tokens": {"access_token": token, "account_id": "acct_ambient"}
            })
            .to_string(),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&auth, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let store = FileCredentialStore::new(home.join("fx-credentials"));
        let provider = CodexProvider::from_process(Some(home.clone()));
        assert!(provider.gateway(DEFAULT_MODEL, None, &store).is_ok());
        assert!(auth.exists());
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn unsafe_auth_endpoint_is_rejected() {
        let mut config = CodexProviderConfig::from_process(None);
        config.auth_base_url = "http://example.com".into();
        assert!(CodexProvider::new(config).is_err());
    }

    #[test]
    fn refresh_uses_provider_lock_compatible_token_contract() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let access = test_jwt("acct_refreshed", now_ms() + 60 * 60 * 1000);
        let server_access = access.clone();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let body = read_http_request(&mut stream);
            assert!(body.contains("grant_type=refresh_token"));
            assert!(body.contains("refresh_token=old-refresh"));
            assert!(body.contains(CLIENT_ID));
            let payload = serde_json::json!({
                "access_token": server_access,
                "refresh_token": "new-refresh",
                "expires_in": 3600
            })
            .to_string();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                payload.len(),
                payload
            )
            .unwrap();
        });
        let mut config = CodexProviderConfig::from_process(None);
        config.auth_base_url = format!("http://{address}");
        let provider = CodexProvider::new(config).unwrap();
        let token = provider.refresh_token("old-refresh").unwrap();
        assert_eq!(token.refresh_token, "new-refresh");
        assert_eq!(
            account_id_from_jwt(&token.access_token).as_deref(),
            Some("acct_refreshed")
        );
        server.join().unwrap();
    }

    #[test]
    fn callback_requires_exact_state_and_extracts_code() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let client = thread::spawn(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            stream
                .write_all(
                    b"GET /auth/callback?code=authorization-code&state=expected HTTP/1.1\r\nHost: localhost\r\n\r\n",
                )
                .unwrap();
        });
        let (stream, _) = listener.accept().unwrap();
        match parse_callback(&stream, "expected").unwrap() {
            Callback::Code(code) => assert_eq!(code, "authorization-code"),
            Callback::Ignore => panic!("callback was ignored"),
        }
        client.join().unwrap();
    }
}
