use fx_core::{ChatMessage, PermissionMode};
use serde::{Deserialize, Serialize};

pub(crate) const MAX_NAME_BYTES: usize = 128;
pub(crate) const MAX_MODEL_BYTES: usize = 256;
pub(crate) const MAX_PROMPT_BYTES: usize = 64 * 1024;
pub(crate) const MAX_MESSAGE_BYTES: usize = 64 * 1024;
pub(crate) const MAX_PAGE_LIMIT: usize = 100;
pub(crate) const MAX_WAIT_MS: u64 = 60_000;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChildMode {
    OneOff,
    Persistent,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChildState {
    Idle,
    Queued,
    Running,
    Interrupted,
    Completed,
    Failed,
    Cancelled,
    Archived,
}

impl ChildState {
    pub(crate) fn is_settled(self) -> bool {
        !matches!(self, Self::Queued | Self::Running)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RootInput {
    pub command: CommandInput,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CommandInput {
    #[serde(default)]
    pub create: Option<CreateInput>,
    #[serde(default)]
    pub inspect: Option<InspectInput>,
    #[serde(default)]
    pub message: Option<MessageInput>,
    #[serde(default)]
    pub relationship: Option<RelationshipInput>,
    #[serde(default)]
    pub configure: Option<ConfigureInput>,
    #[serde(default)]
    pub lifecycle: Option<LifecycleInput>,
}

impl CommandInput {
    pub(crate) fn branch_count(&self) -> usize {
        [
            self.create.is_some(),
            self.inspect.is_some(),
            self.message.is_some(),
            self.relationship.is_some(),
            self.configure.is_some(),
            self.lifecycle.is_some(),
        ]
        .into_iter()
        .filter(|selected| *selected)
        .count()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreateInput {
    pub name: String,
    pub mode: ChildMode,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub permission_mode: Option<PermissionMode>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InspectInput {
    pub id: String,
    pub sections: Vec<InspectSection>,
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub wait: Option<InspectWait>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum InspectSection {
    Status,
    Messages,
    ToolActivity,
    Events,
    Configuration,
    Relationship,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InspectWait {
    pub until: WaitUntil,
    #[serde(default)]
    pub after_generation: Option<u64>,
    pub timeout_ms: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WaitUntil {
    Settled,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MessageInput {
    #[serde(default)]
    pub send: Option<SendInput>,
    #[serde(default)]
    pub milestone: Option<MilestoneInput>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SendInput {
    pub id: String,
    pub content: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MilestoneInput {
    pub name: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RelationshipInput {
    pub action: RelationshipAction,
    pub id: String,
    #[serde(default)]
    pub parent_id: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RelationshipAction {
    Attach,
    Detach,
    Reparent,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConfigureInput {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub permission_mode: Option<PermissionMode>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LifecycleInput {
    pub id: String,
    pub action: LifecycleAction,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LifecycleAction {
    Cancel,
    Resume,
    Close,
    Reopen,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ToolActivity {
    pub generation: u64,
    pub phase: String,
    pub tool: String,
    pub call_id: String,
    pub is_error: Option<bool>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ChildEvent {
    pub generation: u64,
    pub kind: String,
    pub detail: String,
    pub at_ms: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ChildSnapshot {
    pub id: String,
    pub parent_id: Option<String>,
    pub name: String,
    pub mode: ChildMode,
    pub model: String,
    pub permission_mode: PermissionMode,
    pub state: ChildState,
    pub generation: u64,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub history: Vec<ChatMessage>,
    pub queued_messages: Vec<String>,
    pub tool_activity: Vec<ToolActivity>,
    pub events: Vec<ChildEvent>,
    pub last_output: Option<String>,
    pub failure: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ChildRunRequest {
    pub id: String,
    pub name: String,
    pub model: String,
    pub permission_mode: PermissionMode,
    pub history: Vec<ChatMessage>,
    pub prompt: String,
}

#[derive(Clone, Debug)]
pub struct ChildRunResult {
    pub history: Vec<ChatMessage>,
    pub output: String,
}
