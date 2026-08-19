use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A provider-neutral conversation role.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// Integrity of the exact tool arguments received from a provider.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolArgumentIntegrity {
    #[default]
    Valid,
    MalformedJson,
}

/// Origin of a tool result. Provider-executed calls are never dispatched again
/// by the local runtime.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolExecutionProvenance {
    #[default]
    FxLocal,
    Provider,
}

/// Stable tool-call representation shared by gateways, sessions, and tools.
///
/// `arguments_json` retains the provider bytes for diagnostics and durable
/// replay. Callers should use [`ToolCall::arguments`] when they need a parsed
/// value.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments_json: String,
    #[serde(default)]
    pub argument_integrity: ToolArgumentIntegrity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provisional_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_result: Option<String>,
    #[serde(default)]
    pub provenance: ToolExecutionProvenance,
}

impl ToolCall {
    pub fn arguments(&self) -> Result<Value, serde_json::Error> {
        serde_json::from_str(&self.arguments_json)
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CachePolicy {
    #[default]
    Default,
    NoCache,
}

/// Provider-neutral message. Optional fields are role-dependent but remain in
/// one type so gateways can project them without lossy intermediate formats.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ChatMessage {
    pub role: Role,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default)]
    pub permission_feedback: bool,
    #[serde(default)]
    pub cache_policy: CachePolicy,
}

impl ChatMessage {
    pub fn text(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: Some(content.into()),
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
            permission_feedback: false,
            cache_policy: CachePolicy::Default,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum FinishReason {
    Stop,
    Length,
    ContentFilter,
    ToolCalls,
    Error,
    Other,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct Usage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_call_preserves_raw_json_and_parses_on_demand() {
        let call = ToolCall {
            id: "call_1".into(),
            name: "read_file".into(),
            arguments_json: r#"{"path":"README.md"}"#.into(),
            argument_integrity: ToolArgumentIntegrity::Valid,
            provisional_id: None,
            provider_result: None,
            provenance: ToolExecutionProvenance::FxLocal,
        };

        assert_eq!(call.arguments().unwrap()["path"], "README.md");
        assert_eq!(call.arguments_json, r#"{"path":"README.md"}"#);
    }
}
