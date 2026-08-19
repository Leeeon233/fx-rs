use std::sync::Arc;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{BoxFuture, ChatMessage, ToolCall, Usage};

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoice {
    #[default]
    Auto,
    None,
    Required,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GatewayRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<ToolAdvertisement>,
    pub tool_choice: ToolChoice,
    pub max_output_tokens: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ToolAdvertisement {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    pub kind: ToolAdvertisementKind,
}

impl ToolAdvertisement {
    pub fn function(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: serde_json::Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            input_schema,
            kind: ToolAdvertisementKind::Function,
        }
    }

    pub fn provider(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: serde_json::Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: String::new(),
            input_schema: serde_json::Value::Null,
            kind: ToolAdvertisementKind::Provider {
                id: id.into(),
                arguments,
            },
        }
    }
}

/// How a provider should project a tool advertisement on its wire protocol.
///
/// Local fx tools are ordinary functions. Provider tools are executed by the
/// model provider and carry provider-owned configuration instead of a JSON
/// input schema.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub enum ToolAdvertisementKind {
    Function,
    Provider {
        id: String,
        arguments: serde_json::Value,
    },
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct GatewayResponse {
    pub content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub generation_id: Option<String>,
    pub finish_reason: Option<crate::FinishReason>,
    pub usage: Usage,
    /// True when request delivery may have incurred cost but no reliable
    /// generation identity was recovered.
    pub delivery_ambiguous: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum GatewayEvent {
    ContentDelta(String),
    ReasoningDelta(String),
    ToolStarted { id: String, name: String },
}

pub trait GatewayEventSink: Send {
    fn emit(&mut self, event: GatewayEvent);
}

#[derive(Debug, Error)]
pub enum GatewayError {
    #[error("request was cancelled")]
    Cancelled,
    #[error("authentication failed")]
    Authentication,
    #[error("provider rejected the request: {0}")]
    Rejected(String),
    #[error("gateway transport failed before delivery")]
    DefinitelyUnsent,
    #[error("gateway transport failed after possible delivery")]
    PossiblySent,
    #[error("gateway response was invalid: {0}")]
    InvalidResponse(String),
    #[error("gateway is unavailable: {0}")]
    Unavailable(String),
}

/// Provider boundary for model streaming.
///
/// The trait owns semantic retries; HTTP adapters may only retry failures that
/// are known to be unsent. This avoids duplicate billed generations.
pub trait Gateway: Send + Sync {
    fn complete<'a>(
        &'a self,
        request: GatewayRequest,
        events: &'a mut dyn GatewayEventSink,
    ) -> BoxFuture<'a, Result<GatewayResponse, GatewayError>>;
}

/// Provider-neutral retry boundary for requests proven not to have been sent.
///
/// `PossiblySent`, protocol errors, rejections, and invalid streams are never
/// replayed because they may represent a billed generation or partial output.
#[derive(Clone)]
pub struct SafeRetryGateway {
    inner: Arc<dyn Gateway>,
    max_unsent_retries: usize,
}

impl SafeRetryGateway {
    pub fn new(inner: Arc<dyn Gateway>) -> Self {
        Self {
            inner,
            max_unsent_retries: 1,
        }
    }

    pub fn with_max_unsent_retries(mut self, max_unsent_retries: usize) -> Self {
        self.max_unsent_retries = max_unsent_retries;
        self
    }
}

impl Gateway for SafeRetryGateway {
    fn complete<'a>(
        &'a self,
        request: GatewayRequest,
        events: &'a mut dyn GatewayEventSink,
    ) -> BoxFuture<'a, Result<GatewayResponse, GatewayError>> {
        Box::pin(async move {
            let mut retries = 0usize;
            loop {
                match self.inner.complete(request.clone(), events).await {
                    Err(GatewayError::DefinitelyUnsent) if retries < self.max_unsent_retries => {
                        retries += 1;
                    }
                    result => return result,
                }
            }
        })
    }
}

impl std::fmt::Debug for SafeRetryGateway {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SafeRetryGateway")
            .field("max_unsent_retries", &self.max_unsent_retries)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use super::*;

    struct ScriptedGateway(Mutex<VecDeque<Result<GatewayResponse, GatewayError>>>);

    impl Gateway for ScriptedGateway {
        fn complete<'a>(
            &'a self,
            _request: GatewayRequest,
            _events: &'a mut dyn GatewayEventSink,
        ) -> BoxFuture<'a, Result<GatewayResponse, GatewayError>> {
            Box::pin(async move { self.0.lock().unwrap().pop_front().unwrap() })
        }
    }

    struct Events;

    impl GatewayEventSink for Events {
        fn emit(&mut self, _event: GatewayEvent) {}
    }

    fn request() -> GatewayRequest {
        GatewayRequest {
            model: "model".into(),
            messages: Vec::new(),
            tools: Vec::new(),
            tool_choice: ToolChoice::Auto,
            max_output_tokens: None,
        }
    }

    #[test]
    fn retries_only_definitely_unsent_requests() {
        let successful = Arc::new(ScriptedGateway(Mutex::new(VecDeque::from([
            Err(GatewayError::DefinitelyUnsent),
            Ok(GatewayResponse::default()),
        ]))));
        let retrying = SafeRetryGateway::new(successful.clone());
        assert!(pollster::block_on(retrying.complete(request(), &mut Events)).is_ok());
        assert!(successful.0.lock().unwrap().is_empty());

        let ambiguous = Arc::new(ScriptedGateway(Mutex::new(VecDeque::from([
            Err(GatewayError::PossiblySent),
            Ok(GatewayResponse::default()),
        ]))));
        let retrying = SafeRetryGateway::new(ambiguous.clone());
        assert!(matches!(
            pollster::block_on(retrying.complete(request(), &mut Events)),
            Err(GatewayError::PossiblySent)
        ));
        assert_eq!(ambiguous.0.lock().unwrap().len(), 1);
    }
}
