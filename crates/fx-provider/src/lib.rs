//! Provider registry and provider-neutral authentication contracts.
//!
//! A provider owns its model catalog, authentication lifecycle, and gateway
//! construction. The registry intentionally supports several providers in one
//! process; selecting a model is therefore also selecting its provider.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use fx_core::Gateway;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use zeroize::Zeroize;

pub const MODEL_ROUTE_SEPARATOR: char = '/';

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Model {
    pub provider_id: String,
    pub id: String,
    pub name: String,
    pub context_window: u32,
    pub max_output_tokens: u32,
    pub reasoning: bool,
    pub capabilities: ModelCapabilities,
}

impl Model {
    pub fn route(&self) -> String {
        format!("{}{MODEL_ROUTE_SEPARATOR}{}", self.provider_id, self.id)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ModelCapabilities {
    pub native_web_search: Option<NativeWebSearch>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeWebSearch {
    pub provider_tool_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthMethod {
    pub id: String,
    pub name: String,
    pub description: String,
}

impl AuthMethod {
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            description: description.into(),
        }
    }
}

/// Durable provider credential. Providers may add non-secret routing fields
/// (for example an account id) without changing the store schema.
#[derive(Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Credential {
    ApiKey {
        secret: String,
        #[serde(default)]
        attributes: BTreeMap<String, String>,
    },
    OAuth {
        access_token: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        refresh_token: Option<String>,
        expires_at_ms: i64,
        #[serde(default)]
        attributes: BTreeMap<String, String>,
    },
}

impl Drop for Credential {
    fn drop(&mut self) {
        match self {
            Self::ApiKey { secret, attributes } => {
                secret.zeroize();
                for value in attributes.values_mut() {
                    value.zeroize();
                }
            }
            Self::OAuth {
                access_token,
                refresh_token,
                attributes,
                ..
            } => {
                access_token.zeroize();
                refresh_token.zeroize();
                for value in attributes.values_mut() {
                    value.zeroize();
                }
            }
        }
    }
}

impl fmt::Debug for Credential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ApiKey { attributes, .. } => formatter
                .debug_struct("ApiKeyCredential")
                .field("secret", &"[redacted]")
                .field("attributes", attributes)
                .finish(),
            Self::OAuth {
                refresh_token,
                expires_at_ms,
                attributes,
                ..
            } => formatter
                .debug_struct("OAuthCredential")
                .field("access_token", &"[redacted]")
                .field(
                    "refresh_token",
                    &refresh_token.as_ref().map(|_| "[redacted]"),
                )
                .field("expires_at_ms", expires_at_ms)
                .field("attributes", attributes)
                .finish(),
        }
    }
}

/// An exclusive provider credential lease. Implementations hold their lock
/// until this value is dropped, including while a provider refreshes OAuth.
pub trait CredentialLease {
    fn credential(&self) -> Option<&Credential>;
    fn replace(&mut self, credential: Credential) -> Result<(), ProviderError>;
    fn delete(&mut self) -> Result<(), ProviderError>;
}

pub trait CredentialStore: Send + Sync {
    fn lock<'a>(
        &'a self,
        provider_id: &str,
    ) -> Result<Box<dyn CredentialLease + 'a>, ProviderError>;
}

pub trait Provider: Send + Sync {
    fn id(&self) -> &str;
    fn name(&self) -> &str;
    fn models(&self) -> Vec<Model>;
    fn default_model(&self) -> &str;
    fn auth_methods(&self) -> Vec<AuthMethod>;

    /// Performs an interactive authentication method and persists only Fx's
    /// owned credential. This is called on a blocking worker by the ACP host.
    fn authenticate(
        &self,
        method_id: &str,
        credentials: &dyn CredentialStore,
    ) -> Result<(), ProviderError>;

    /// Removes Fx-owned authentication state. Ambient state belonging to
    /// another application must never be modified.
    fn logout(&self, credentials: &dyn CredentialStore) -> Result<(), ProviderError> {
        let mut lease = credentials.lock(self.id())?;
        lease.delete()
    }

    /// Resolves/refreshes authentication and constructs a transport for one
    /// provider-local model id.
    fn gateway(
        &self,
        model_id: &str,
        session_id: Option<&str>,
        credentials: &dyn CredentialStore,
    ) -> Result<Arc<dyn Gateway>, ProviderError>;
}

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("provider `{0}` is not registered")]
    UnknownProvider(String),
    #[error("model `{0}` is not registered")]
    UnknownModel(String),
    #[error("authentication method `{0}` is not registered")]
    UnknownAuthMethod(String),
    #[error("provider `{0}` is already registered")]
    DuplicateProvider(String),
    #[error("model route `{0}` is already registered")]
    DuplicateModel(String),
    #[error("authentication method `{0}` is already registered")]
    DuplicateAuthMethod(String),
    #[error("authentication is required for {provider}: {message}")]
    AuthenticationRequired { provider: String, message: String },
    #[error("provider authentication failed: {0}")]
    Authentication(String),
    #[error("provider credential store failed: {0}")]
    CredentialStore(String),
    #[error("provider configuration is invalid: {0}")]
    Configuration(String),
    #[error("provider transport could not be created: {0}")]
    Transport(String),
}

#[derive(Clone, Default)]
pub struct ProviderRegistry {
    providers: BTreeMap<String, Arc<dyn Provider>>,
    models: BTreeMap<String, Model>,
    auth_methods: BTreeMap<String, RegisteredAuthMethod>,
    default_model_route: Option<String>,
}

#[derive(Clone)]
struct RegisteredAuthMethod {
    provider_id: String,
    local_id: String,
    descriptor: AuthMethod,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, provider: Arc<dyn Provider>) -> Result<(), ProviderError> {
        validate_component("provider id", provider.id())?;
        if self.providers.contains_key(provider.id()) {
            return Err(ProviderError::DuplicateProvider(provider.id().into()));
        }

        let mut models = Vec::new();
        let mut seen_models = BTreeSet::new();
        for model in provider.models() {
            validate_component("model id", &model.id)?;
            if model.provider_id != provider.id() {
                return Err(ProviderError::Configuration(format!(
                    "model `{}` declares provider `{}` instead of `{}`",
                    model.id,
                    model.provider_id,
                    provider.id()
                )));
            }
            let route = model.route();
            if self.models.contains_key(&route) || !seen_models.insert(route.clone()) {
                return Err(ProviderError::DuplicateModel(route));
            }
            models.push((route, model));
        }
        if models.is_empty() {
            return Err(ProviderError::Configuration(format!(
                "provider `{}` has no models",
                provider.id()
            )));
        }
        let default_route = format!("{}/{}", provider.id(), provider.default_model());
        if !models.iter().any(|(route, _)| route == &default_route) {
            return Err(ProviderError::Configuration(format!(
                "provider `{}` default model `{}` is not in its catalog",
                provider.id(),
                provider.default_model()
            )));
        }

        let mut methods = Vec::new();
        let mut seen_methods = BTreeSet::new();
        for method in provider.auth_methods() {
            validate_component("authentication method id", &method.id)?;
            let global_id = auth_route(provider.id(), &method.id);
            if self.auth_methods.contains_key(&global_id) || !seen_methods.insert(global_id.clone())
            {
                return Err(ProviderError::DuplicateAuthMethod(global_id));
            }
            let mut descriptor = method.clone();
            descriptor.id = global_id.clone();
            methods.push((
                global_id,
                RegisteredAuthMethod {
                    provider_id: provider.id().into(),
                    local_id: method.id,
                    descriptor,
                },
            ));
        }

        self.models.extend(models);
        self.auth_methods.extend(methods);
        if self.default_model_route.is_none() {
            self.default_model_route = Some(default_route);
        }
        self.providers.insert(provider.id().into(), provider);
        Ok(())
    }

    pub fn models(&self) -> Vec<Model> {
        self.models.values().cloned().collect()
    }

    pub fn model(&self, route: &str) -> Result<&Model, ProviderError> {
        self.models
            .get(route)
            .ok_or_else(|| ProviderError::UnknownModel(route.into()))
    }

    pub fn default_model(&self) -> Result<&Model, ProviderError> {
        let route = self
            .default_model_route
            .as_deref()
            .ok_or_else(|| ProviderError::Configuration("provider registry is empty".into()))?;
        self.model(route)
    }

    pub fn auth_methods(&self) -> Vec<AuthMethod> {
        self.auth_methods
            .values()
            .map(|method| method.descriptor.clone())
            .collect()
    }

    pub fn authenticate(
        &self,
        method_id: &str,
        credentials: &dyn CredentialStore,
    ) -> Result<(), ProviderError> {
        let method = self
            .auth_methods
            .get(method_id)
            .ok_or_else(|| ProviderError::UnknownAuthMethod(method_id.into()))?;
        self.providers[&method.provider_id].authenticate(&method.local_id, credentials)
    }

    pub fn logout_all(&self, credentials: &dyn CredentialStore) -> Result<(), ProviderError> {
        let mut failures = Vec::new();
        for provider in self.providers.values() {
            if let Err(error) = provider.logout(credentials) {
                failures.push(format!("{}: {error}", provider.id()));
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(ProviderError::CredentialStore(failures.join("; ")))
        }
    }

    pub fn gateway(
        &self,
        route: &str,
        session_id: Option<&str>,
        credentials: &dyn CredentialStore,
    ) -> Result<Arc<dyn Gateway>, ProviderError> {
        let model = self.model(route)?;
        self.providers[&model.provider_id].gateway(&model.id, session_id, credentials)
    }
}

impl fmt::Debug for ProviderRegistry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderRegistry")
            .field("providers", &self.providers.keys().collect::<Vec<_>>())
            .field("models", &self.models.keys().collect::<Vec<_>>())
            .field(
                "auth_methods",
                &self.auth_methods.keys().collect::<Vec<_>>(),
            )
            .finish()
    }
}

fn auth_route(provider_id: &str, method_id: &str) -> String {
    format!("{provider_id}:{method_id}")
}

fn validate_component(label: &str, value: &str) -> Result<(), ProviderError> {
    let valid = !value.is_empty()
        && value.len() <= 128
        && !value.contains(MODEL_ROUTE_SEPARATOR)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
    if valid {
        Ok(())
    } else {
        Err(ProviderError::Configuration(format!(
            "{label} `{value}` is not a safe identifier"
        )))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use fx_core::{BoxFuture, GatewayError, GatewayEventSink, GatewayRequest, GatewayResponse};

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

    struct EmptyGateway;

    impl Gateway for EmptyGateway {
        fn complete<'a>(
            &'a self,
            _request: GatewayRequest,
            _events: &'a mut dyn GatewayEventSink,
        ) -> BoxFuture<'a, Result<GatewayResponse, GatewayError>> {
            Box::pin(async { Ok(GatewayResponse::default()) })
        }
    }

    struct TestProvider(&'static str);

    impl Provider for TestProvider {
        fn id(&self) -> &str {
            self.0
        }

        fn name(&self) -> &str {
            self.0
        }

        fn models(&self) -> Vec<Model> {
            vec![Model {
                provider_id: self.0.into(),
                id: "model".into(),
                name: "Model".into(),
                context_window: 1,
                max_output_tokens: 1,
                reasoning: false,
                capabilities: ModelCapabilities::default(),
            }]
        }

        fn default_model(&self) -> &str {
            "model"
        }

        fn auth_methods(&self) -> Vec<AuthMethod> {
            vec![AuthMethod::new("login", "Login", "Login")]
        }

        fn authenticate(
            &self,
            _method_id: &str,
            _credentials: &dyn CredentialStore,
        ) -> Result<(), ProviderError> {
            Ok(())
        }

        fn gateway(
            &self,
            _model_id: &str,
            _session_id: Option<&str>,
            _credentials: &dyn CredentialStore,
        ) -> Result<Arc<dyn Gateway>, ProviderError> {
            Ok(Arc::new(EmptyGateway))
        }
    }

    #[test]
    fn registry_routes_multiple_providers_without_global_state() {
        let mut registry = ProviderRegistry::new();
        registry.register(Arc::new(TestProvider("alpha"))).unwrap();
        registry.register(Arc::new(TestProvider("beta"))).unwrap();
        assert_eq!(
            registry
                .models()
                .iter()
                .map(Model::route)
                .collect::<Vec<_>>(),
            ["alpha/model", "beta/model"]
        );
        assert_eq!(
            registry
                .auth_methods()
                .iter()
                .map(|method| method.id.as_str())
                .collect::<Vec<_>>(),
            ["alpha:login", "beta:login"]
        );
        assert!(
            registry
                .gateway("beta/model", None, &MemoryStore::default())
                .is_ok()
        );
    }

    #[test]
    fn registration_is_transactional() {
        let mut registry = ProviderRegistry::new();
        registry.register(Arc::new(TestProvider("alpha"))).unwrap();
        assert!(registry.register(Arc::new(TestProvider("alpha"))).is_err());
        assert_eq!(registry.models().len(), 1);
    }
}
