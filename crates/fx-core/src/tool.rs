use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::{
    BoxFuture, PermissionRequest, ReadEvidenceStore, ScopedProjectContextProvider,
    ToolAdvertisement, ToolCall, ToolResultStore,
};

/// Executor-neutral cooperative cancellation shared by agent and tool hosts.
pub trait CancellationSignal: Send + Sync {
    fn is_cancelled(&self) -> bool;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct NeverCancelled;

impl CancellationSignal for NeverCancelled {
    fn is_cancelled(&self) -> bool {
        false
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolEffect {
    Read,
    Write,
    Process,
    Network,
    UserInteraction,
    Delegation,
}

/// Host execution isolation selected before a tool is prepared.
///
/// Core carries only the semantic choice. Native process adapters decide how
/// to project `Os` (currently the macOS seatbelt profile) without pulling host
/// details into policy or agent orchestration.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SandboxMode {
    #[default]
    None,
    Os,
}

#[derive(Clone)]
pub struct ToolContext {
    pub workspace_root: PathBuf,
    pub additional_roots: Vec<PathBuf>,
    pub limits: ToolLimits,
    pub read_evidence: Option<Arc<dyn ReadEvidenceStore>>,
    pub tool_results: Option<Arc<dyn ToolResultStore>>,
    pub project_context: Option<Arc<dyn ScopedProjectContextProvider>>,
    pub cancellation: Arc<dyn CancellationSignal>,
    pub sandbox: SandboxMode,
}

impl ToolContext {
    pub fn new(workspace_root: PathBuf) -> Self {
        Self {
            workspace_root,
            additional_roots: Vec::new(),
            limits: ToolLimits::default(),
            read_evidence: None,
            tool_results: None,
            project_context: None,
            cancellation: Arc::new(NeverCancelled),
            sandbox: SandboxMode::None,
        }
    }
}

impl fmt::Debug for ToolContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolContext")
            .field("workspace_root", &self.workspace_root)
            .field("additional_roots", &self.additional_roots)
            .field("limits", &self.limits)
            .field("has_read_evidence", &self.read_evidence.is_some())
            .field("has_tool_result_store", &self.tool_results.is_some())
            .field("has_project_context", &self.project_context.is_some())
            .field("cancelled", &self.cancellation.is_cancelled())
            .field("sandbox", &self.sandbox)
            .finish()
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ToolLimits {
    pub max_result_bytes: usize,
    pub max_read_file_lines: usize,
    pub max_read_file_line_bytes: usize,
    pub max_list_entries: usize,
    pub command_timeout: Option<Duration>,
}

impl Default for ToolLimits {
    fn default() -> Self {
        Self {
            max_result_bytes: 64 * 1024,
            max_read_file_lines: 400,
            max_read_file_line_bytes: 2_000,
            max_list_entries: 100,
            command_timeout: None,
        }
    }
}

#[derive(Clone, Deserialize, PartialEq, Serialize)]
pub struct ToolOutput {
    pub content: String,
    #[serde(default)]
    pub is_error: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured: Option<Value>,
    #[serde(default)]
    pub original_bytes: usize,
    #[serde(default)]
    pub truncated: bool,
    /// Complete transient content before projection into the model context.
    ///
    /// Tools that already hold a complete textual result may set this field so
    /// the Agent can persist large output in a session-scoped sidecar. It is
    /// deliberately excluded from serialization and must never be treated as
    /// durable history by protocol or session adapters.
    #[serde(skip)]
    pub durable_content: Option<String>,
}

impl fmt::Debug for ToolOutput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolOutput")
            .field("content", &self.content)
            .field("is_error", &self.is_error)
            .field("structured", &self.structured)
            .field("original_bytes", &self.original_bytes)
            .field("truncated", &self.truncated)
            .field("has_durable_content", &self.durable_content.is_some())
            .finish()
    }
}

#[derive(Debug, Error)]
pub enum ToolError {
    #[error("invalid arguments: {0}")]
    InvalidArguments(String),
    #[error("tool call is outside the active workspace: {0}")]
    OutsideWorkspace(String),
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    #[error("tool execution failed: {0}")]
    Execution(String),
    #[error("tool was cancelled")]
    Cancelled,
}

/// Host-neutral material needed to review a file mutation.
///
/// Keeping bytes instead of UTF-8 strings preserves the source behavior for
/// an existing file whose contents are not valid Unicode. Presentation layers
/// are responsible for producing a bounded textual or binary-aware diff.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileChangeReview {
    pub path: PathBuf,
    pub before: Option<Vec<u8>>,
    pub after: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandReview {
    pub command: String,
    pub cwd: PathBuf,
    pub shell: PathBuf,
    pub profile: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolReview {
    FileChange(FileChangeReview),
    Command(CommandReview),
}

/// An approved action that can be consumed exactly once.
///
/// The consuming receiver makes replay impossible without cloning the
/// concrete implementation's private state. That replaces the mutable
/// commit-token bookkeeping required by the original Zig runtime.
pub trait PreparedToolAction: Send {
    fn commit<'a>(
        self: Box<Self>,
        context: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>>;
}

pub struct PreparedToolCall {
    pub tool_name: String,
    pub permission_requests: Vec<PermissionRequest>,
    pub irreversible: bool,
    pub review: Option<ToolReview>,
    action: Box<dyn PreparedToolAction>,
}

impl PreparedToolCall {
    pub fn new(
        tool_name: impl Into<String>,
        permission_requests: Vec<PermissionRequest>,
        irreversible: bool,
        review: Option<ToolReview>,
        action: impl PreparedToolAction + 'static,
    ) -> Self {
        Self {
            tool_name: tool_name.into(),
            permission_requests,
            irreversible,
            review,
            action: Box::new(action),
        }
    }

    pub fn commit<'a>(
        self,
        context: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        self.action.commit(context)
    }
}

impl fmt::Debug for PreparedToolCall {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedToolCall")
            .field("tool_name", &self.tool_name)
            .field("permission_requests", &self.permission_requests)
            .field("irreversible", &self.irreversible)
            .field("review", &self.review)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub enum ToolPreparation {
    Direct {
        permission_requests: Vec<PermissionRequest>,
        irreversible: bool,
    },
    Prepared(PreparedToolCall),
}

/// Runtime-extensible tool contract. Validation, effect classification, and
/// execution stay together so registry metadata cannot drift from behavior.
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn input_schema(&self) -> Value;
    fn effect(&self, arguments: &Value) -> Result<ToolEffect, ToolError>;
    fn irreversible(&self, _arguments: &Value) -> Result<bool, ToolError> {
        Ok(false)
    }

    /// Filesystem targets used to select nested project instructions.
    ///
    /// This is separate from permission targets: command permission matches
    /// human command text while its project-context target is the canonical
    /// cwd, and network tools intentionally expose no filesystem target.
    fn project_context_targets(
        &self,
        _context: &ToolContext,
        _arguments: &Value,
    ) -> Result<Vec<PathBuf>, ToolError> {
        Ok(Vec::new())
    }

    fn permission_requests(
        &self,
        context: &ToolContext,
        arguments: &Value,
    ) -> Result<Vec<PermissionRequest>, ToolError> {
        Ok(vec![PermissionRequest::new(
            self.name(),
            context.workspace_root.display().to_string(),
            self.effect(arguments)?,
        )])
    }

    /// Produces the authorization contract for a call. Most tools execute
    /// directly after permission evaluation; TOCTOU-sensitive mutations
    /// override this to return an owned, one-shot prepared action.
    fn prepare(
        &self,
        context: &ToolContext,
        arguments: &Value,
    ) -> Result<ToolPreparation, ToolError> {
        Ok(ToolPreparation::Direct {
            permission_requests: self.permission_requests(context, arguments)?,
            irreversible: self.irreversible(arguments)?,
        })
    }

    fn execute<'a>(
        &'a self,
        context: &'a ToolContext,
        arguments: Value,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>>;

    fn advertisement(&self) -> ToolAdvertisement {
        ToolAdvertisement::function(self.name(), self.description(), self.input_schema())
    }
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum RegistryError {
    #[error("tool `{0}` is already registered")]
    Duplicate(String),
    #[error("tool `{0}` is not registered")]
    Unknown(String),
}

#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
    order: Vec<String>,
}

impl ToolRegistry {
    pub fn register<T: Tool + 'static>(&mut self, tool: T) -> Result<(), RegistryError> {
        let name = tool.name().to_owned();
        if self.tools.contains_key(&name) {
            return Err(RegistryError::Duplicate(name));
        }
        self.tools.insert(name.clone(), Arc::new(tool));
        self.order.push(name);
        Ok(())
    }

    pub fn get(&self, name: &str) -> Result<Arc<dyn Tool>, RegistryError> {
        self.tools
            .get(name)
            .cloned()
            .ok_or_else(|| RegistryError::Unknown(name.to_owned()))
    }

    pub fn advertisements(&self) -> Vec<ToolAdvertisement> {
        self.order
            .iter()
            .filter_map(|name| self.tools.get(name))
            .map(|tool| tool.advertisement())
            .collect()
    }

    pub fn validate_call(&self, call: &ToolCall) -> Result<(Arc<dyn Tool>, Value), ToolError> {
        let tool = self
            .get(&call.name)
            .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;
        let arguments = call
            .arguments()
            .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;
        if !arguments.is_object() {
            return Err(ToolError::InvalidArguments(
                "tool arguments must be a JSON object".into(),
            ));
        }
        Ok((tool, arguments))
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}
