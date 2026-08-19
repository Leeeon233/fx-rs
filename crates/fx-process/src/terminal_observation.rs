use std::path::PathBuf;

use fx_core::ToolError;
use serde::{Deserialize, Serialize};

use crate::monitor::MonitorSignal;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ObservedLifecycle {
    Running,
    Exited,
    Lost,
    Closed,
}

#[derive(Clone, Debug)]
pub(crate) struct TerminalObservation {
    pub lifecycle: ObservedLifecycle,
    pub exit_code: Option<i32>,
    pub signal: Option<MonitorSignal>,
    pub cursor_start: u64,
    pub cursor_end: u64,
    pub raw_gap: bool,
    pub output: Vec<u8>,
    pub screen_text: Option<String>,
    pub workspace_root: PathBuf,
    pub cwd: PathBuf,
}

/// Non-consuming observation port used by the detached monitor supervisor.
/// Implementations must not advance the user-visible terminal read cursor.
pub(crate) trait TerminalMonitorSource: Send + Sync {
    fn observe_terminal(
        &self,
        session_id: &str,
        after_offset: u64,
        include_screen: bool,
    ) -> Result<Option<TerminalObservation>, ToolError>;
}
