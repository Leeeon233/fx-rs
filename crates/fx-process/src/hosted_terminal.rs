#![cfg(unix)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use fx_core::{CancellationSignal, ToolError, ToolOutput};

use crate::monitor::MonitorOperation;
use crate::native_terminal::{
    ClosePolicy, NamedKey, SessionBackend, StartSpec, TerminalSessionHost, TerminalSignal,
    WaitCondition, WritePayload, failure_output,
};
use crate::terminal_host_protocol::{
    ReturnCondition, TerminalBackend, TerminalClosePolicy, TerminalKey, TerminalOperation,
    TerminalSignal as WireSignal, TerminalWrite,
};
use crate::terminal_host_server::{HostClient, HostServerConfig, HostServerError};
use crate::tmux_terminal::TmuxTerminalHost;

const NATIVE_BACKENDS: &[&str] = &["native"];
const NATIVE_AND_TMUX_BACKENDS: &[&str] = &["native", "tmux"];

#[derive(Clone, Debug)]
pub(crate) struct HostedTerminalHost {
    config: HostServerConfig,
    executable: PathBuf,
    tmux_available: bool,
}

impl HostedTerminalHost {
    pub(crate) fn discover() -> Option<Self> {
        if std::env::var_os("FX_TERMINAL_HOST_DISABLE").is_some() {
            return None;
        }
        let executable = std::env::var_os("FX_TERMINAL_HOST_EXE")
            .map(PathBuf::from)
            .or_else(discover_sibling)?
            .canonicalize()
            .ok()?;
        if !executable.is_file() {
            return None;
        }
        Some(Self {
            config: HostServerConfig::from_environment().ok()?,
            executable,
            tmux_available: TmuxTerminalHost::discover().is_some(),
        })
    }

    fn client(&self) -> Result<HostClient, ToolError> {
        HostClient::connect_or_spawn(&self.config, &self.executable).map_err(map_host_error)
    }

    fn call(&self, operation: TerminalOperation) -> Result<ToolOutput, ToolError> {
        self.client()?.terminal(operation).map_err(map_host_error)
    }

    fn call_cancellable(
        &self,
        operation: TerminalOperation,
        cancellation: Arc<dyn CancellationSignal>,
    ) -> Result<ToolOutput, ToolError> {
        self.client()?
            .terminal_cancellable(operation, cancellation)
            .map_err(map_host_error)
    }
}

impl TerminalSessionHost for HostedTerminalHost {
    fn backends(&self) -> &'static [&'static str] {
        if self.tmux_available {
            NATIVE_AND_TMUX_BACKENDS
        } else {
            NATIVE_BACKENDS
        }
    }

    fn start(
        &self,
        spec: StartSpec,
        cancellation: Arc<dyn CancellationSignal>,
    ) -> Result<ToolOutput, ToolError> {
        if cancellation.is_cancelled() {
            return Ok(failure_output("start", None, "cancelled", false));
        }
        self.call_cancellable(
            TerminalOperation::Start {
                backend: map_backend(spec.backend),
                workspace_root: spec.workspace_root,
                cwd: spec.cwd,
                initial_monitors: spec.initial_monitors,
                command: spec.command,
                shell: spec.shell,
                arguments: spec.arguments,
                sandbox_profile: spec.sandbox_profile,
                rows: spec.rows,
                columns: spec.columns,
                return_when: spec.return_when.map(map_return),
                wait_ceiling_ms: spec.wait_ceiling.map(duration_ms),
            },
            cancellation,
        )
    }

    fn read(&self, session_id: &str, segment: u64, offset: u64) -> Result<ToolOutput, ToolError> {
        self.call(TerminalOperation::Read {
            session_id: session_id.into(),
            cursor_segment: segment,
            cursor_offset: offset,
        })
    }

    fn screen(&self, session_id: &str) -> Result<ToolOutput, ToolError> {
        self.call(TerminalOperation::Screen {
            session_id: session_id.into(),
        })
    }

    fn write(&self, session_id: &str, payload: WritePayload) -> Result<ToolOutput, ToolError> {
        self.call(TerminalOperation::Write {
            session_id: session_id.into(),
            write: map_write(payload),
        })
    }

    fn wait(
        &self,
        session_id: &str,
        condition: WaitCondition,
        ceiling: Duration,
        cancellation: Arc<dyn CancellationSignal>,
    ) -> Result<ToolOutput, ToolError> {
        self.call_cancellable(
            TerminalOperation::Wait {
                session_id: session_id.into(),
                return_when: map_return(condition),
                wait_ceiling_ms: duration_ms(ceiling),
            },
            cancellation,
        )
    }

    fn inspect(
        &self,
        session_id: &str,
        after_event_id: Option<u64>,
        acknowledge_event_id: Option<u64>,
        max_events: usize,
    ) -> Result<ToolOutput, ToolError> {
        self.call(TerminalOperation::Inspect {
            session_id: session_id.into(),
            after_event_id,
            acknowledge_event_id,
            max_events: u16::try_from(max_events).unwrap_or(u16::MAX),
        })
    }

    fn monitor(
        &self,
        session_id: &str,
        operation: MonitorOperation,
    ) -> Result<ToolOutput, ToolError> {
        self.call(TerminalOperation::Monitor {
            session_id: session_id.into(),
            operation,
        })
    }

    fn list(&self, backend: Option<SessionBackend>) -> Result<ToolOutput, ToolError> {
        self.call(TerminalOperation::List {
            backend: backend.map(map_backend),
        })
    }

    fn resize(&self, session_id: &str, rows: u16, columns: u16) -> Result<ToolOutput, ToolError> {
        self.call(TerminalOperation::Resize {
            session_id: session_id.into(),
            rows,
            columns,
        })
    }

    fn signal(&self, session_id: &str, signal: TerminalSignal) -> Result<ToolOutput, ToolError> {
        self.call(TerminalOperation::Signal {
            session_id: session_id.into(),
            signal: map_signal(signal),
        })
    }

    fn close(&self, session_id: &str, policy: ClosePolicy) -> Result<ToolOutput, ToolError> {
        self.call(TerminalOperation::Close {
            session_id: session_id.into(),
            close_policy: match policy {
                ClosePolicy::Graceful => TerminalClosePolicy::Graceful,
                ClosePolicy::Force => TerminalClosePolicy::Force,
            },
        })
    }
}

fn discover_sibling() -> Option<PathBuf> {
    let executable = std::env::current_exe().ok()?;
    let parent = executable.parent()?;
    [
        parent.join("fx-terminal-host"),
        parent.parent()?.join("fx-terminal-host"),
    ]
    .into_iter()
    .find(|candidate| candidate.is_file())
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn map_backend(backend: SessionBackend) -> TerminalBackend {
    match backend {
        SessionBackend::Native => TerminalBackend::Native,
        SessionBackend::Tmux => TerminalBackend::Tmux,
    }
}

fn map_return(condition: WaitCondition) -> ReturnCondition {
    match condition {
        WaitCondition::Started => ReturnCondition::Started,
        WaitCondition::Exit => ReturnCondition::Exit,
        WaitCondition::Quiet(duration) => ReturnCondition::Quiet {
            duration_ms: duration_ms(duration),
        },
        WaitCondition::Match(pattern) => ReturnCondition::Match { pattern },
    }
}

fn map_write(payload: WritePayload) -> TerminalWrite {
    match payload {
        WritePayload::Text(text) => TerminalWrite::Text { text },
        WritePayload::Paste(text) => TerminalWrite::Paste { text },
        WritePayload::Keys(keys) => TerminalWrite::Keys {
            keys: keys.into_iter().map(map_key).collect(),
        },
        WritePayload::Controls(controls) => TerminalWrite::Controls { controls },
    }
}

fn map_key(key: NamedKey) -> TerminalKey {
    match key {
        NamedKey::Enter => TerminalKey::Enter,
        NamedKey::Tab => TerminalKey::Tab,
        NamedKey::Escape => TerminalKey::Escape,
        NamedKey::Backspace => TerminalKey::Backspace,
        NamedKey::Delete => TerminalKey::Delete,
        NamedKey::Insert => TerminalKey::Insert,
        NamedKey::ArrowUp => TerminalKey::ArrowUp,
        NamedKey::ArrowDown => TerminalKey::ArrowDown,
        NamedKey::ArrowLeft => TerminalKey::ArrowLeft,
        NamedKey::ArrowRight => TerminalKey::ArrowRight,
        NamedKey::Home => TerminalKey::Home,
        NamedKey::End => TerminalKey::End,
        NamedKey::PageUp => TerminalKey::PageUp,
        NamedKey::PageDown => TerminalKey::PageDown,
    }
}

fn map_signal(signal: TerminalSignal) -> WireSignal {
    match signal {
        TerminalSignal::Hangup => WireSignal::Hangup,
        TerminalSignal::Interrupt => WireSignal::Interrupt,
        TerminalSignal::Quit => WireSignal::Quit,
        TerminalSignal::Terminate => WireSignal::Terminate,
        TerminalSignal::Kill => WireSignal::Kill,
    }
}

fn map_host_error(error: HostServerError) -> ToolError {
    ToolError::Execution(error.to_string())
}
