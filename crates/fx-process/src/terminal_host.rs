use std::sync::Arc;
use std::time::Duration;

use fx_core::{CancellationSignal, ToolError, ToolOutput};
use serde_json::json;

#[cfg(unix)]
use crate::hosted_terminal::HostedTerminalHost;
use crate::monitor::MonitorOperation;
use crate::native_terminal::{
    ClosePolicy, NativeTerminalHost, SessionBackend, StartSpec, TerminalSessionHost,
    TerminalSignal, WaitCondition, WritePayload, failure_output, success_output,
};
#[cfg(unix)]
use crate::terminal_observation::{TerminalMonitorSource, TerminalObservation};
#[cfg(unix)]
use crate::tmux_terminal::TmuxTerminalHost;

const NATIVE_BACKENDS: &[&str] = &["native"];
#[cfg(unix)]
const NATIVE_AND_TMUX_BACKENDS: &[&str] = &["native", "tmux"];

/// Routes the public terminal capability to a lightweight in-process PTY or,
/// when installed, a restart-durable tmux host. Backend identity is encoded in
/// the opaque session id so control actions need no additional public field.
pub(crate) struct RoutingTerminalHost {
    #[cfg(unix)]
    hosted: Option<HostedTerminalHost>,
    native: NativeTerminalHost,
    #[cfg(unix)]
    tmux: Option<TmuxTerminalHost>,
}

impl RoutingTerminalHost {
    pub(crate) fn discover() -> Self {
        Self {
            #[cfg(unix)]
            hosted: HostedTerminalHost::discover(),
            native: NativeTerminalHost::default(),
            #[cfg(unix)]
            tmux: TmuxTerminalHost::discover(),
        }
    }

    #[cfg(unix)]
    pub(crate) fn local() -> Self {
        Self {
            hosted: None,
            native: NativeTerminalHost::default(),
            tmux: TmuxTerminalHost::discover(),
        }
    }

    pub(crate) fn has_process_local_sessions(&self) -> bool {
        // Treat lock poisoning conservatively: keeping the host alive preserves
        // more evidence than retiring and dropping an unreachable PTY.
        self.native.has_sessions().unwrap_or(true)
    }

    fn host_for(&self, session_id: &str) -> Option<&dyn TerminalSessionHost> {
        #[cfg(unix)]
        if let Some(hosted) = &self.hosted {
            return Some(hosted);
        }
        #[cfg(unix)]
        if session_id.starts_with("terminal-t-") {
            return self
                .tmux
                .as_ref()
                .map(|host| host as &dyn TerminalSessionHost);
        }
        Some(&self.native)
    }

    fn unavailable(action: &str, session_id: &str) -> ToolOutput {
        failure_output(action, Some(session_id), "unsupported_host", false)
    }
}

impl Default for RoutingTerminalHost {
    fn default() -> Self {
        Self::discover()
    }
}

impl TerminalSessionHost for RoutingTerminalHost {
    fn backends(&self) -> &'static [&'static str] {
        #[cfg(unix)]
        if let Some(hosted) = &self.hosted {
            return hosted.backends();
        }
        #[cfg(unix)]
        if self.tmux.is_some() {
            return NATIVE_AND_TMUX_BACKENDS;
        }
        NATIVE_BACKENDS
    }

    fn start(
        &self,
        spec: StartSpec,
        cancellation: Arc<dyn CancellationSignal>,
    ) -> Result<ToolOutput, ToolError> {
        #[cfg(unix)]
        if let Some(hosted) = &self.hosted {
            return hosted.start(spec, cancellation);
        }
        if !spec.initial_monitors.is_empty() {
            return Ok(failure_output("start", None, "monitor_unavailable", false));
        }
        match spec.backend {
            SessionBackend::Native => self.native.start(spec, cancellation),
            SessionBackend::Tmux => {
                #[cfg(unix)]
                if let Some(tmux) = &self.tmux {
                    return tmux.start(spec, cancellation);
                }
                Ok(failure_output("start", None, "unsupported_host", false))
            }
        }
    }

    fn read(&self, session_id: &str, segment: u64, offset: u64) -> Result<ToolOutput, ToolError> {
        let Some(host) = self.host_for(session_id) else {
            return Ok(Self::unavailable("read", session_id));
        };
        host.read(session_id, segment, offset)
    }

    fn screen(&self, session_id: &str) -> Result<ToolOutput, ToolError> {
        let Some(host) = self.host_for(session_id) else {
            return Ok(Self::unavailable("screen", session_id));
        };
        host.screen(session_id)
    }

    fn write(&self, session_id: &str, payload: WritePayload) -> Result<ToolOutput, ToolError> {
        let Some(host) = self.host_for(session_id) else {
            return Ok(Self::unavailable("write", session_id));
        };
        host.write(session_id, payload)
    }

    fn wait(
        &self,
        session_id: &str,
        condition: WaitCondition,
        ceiling: Duration,
        cancellation: Arc<dyn CancellationSignal>,
    ) -> Result<ToolOutput, ToolError> {
        let Some(host) = self.host_for(session_id) else {
            return Ok(Self::unavailable("wait", session_id));
        };
        host.wait(session_id, condition, ceiling, cancellation)
    }

    fn inspect(
        &self,
        session_id: &str,
        after_event_id: Option<u64>,
        acknowledge_event_id: Option<u64>,
        max_events: usize,
    ) -> Result<ToolOutput, ToolError> {
        let Some(host) = self.host_for(session_id) else {
            return Ok(Self::unavailable("inspect", session_id));
        };
        host.inspect(session_id, after_event_id, acknowledge_event_id, max_events)
    }

    fn monitor(
        &self,
        session_id: &str,
        operation: MonitorOperation,
    ) -> Result<ToolOutput, ToolError> {
        let Some(host) = self.host_for(session_id) else {
            return Ok(Self::unavailable("monitor", session_id));
        };
        host.monitor(session_id, operation)
    }

    fn list(&self, backend: Option<SessionBackend>) -> Result<ToolOutput, ToolError> {
        #[cfg(unix)]
        if let Some(hosted) = &self.hosted {
            return hosted.list(backend);
        }
        match backend {
            Some(SessionBackend::Native) => {
                return TerminalSessionHost::list(&self.native, backend);
            }
            Some(SessionBackend::Tmux) => {
                #[cfg(unix)]
                if let Some(tmux) = &self.tmux {
                    return tmux.list(backend);
                }
                return Ok(failure_output("list", None, "unsupported_host", false));
            }
            None => {}
        }
        let native = TerminalSessionHost::list(&self.native, None)?;
        let mut sessions = list_sessions(&native)?;
        #[cfg(unix)]
        if let Some(tmux) = &self.tmux {
            sessions.extend(list_sessions(&tmux.list(None)?)?);
        }
        Ok(success_output("list", json!({"sessions": sessions})))
    }

    fn resize(&self, session_id: &str, rows: u16, columns: u16) -> Result<ToolOutput, ToolError> {
        let Some(host) = self.host_for(session_id) else {
            return Ok(Self::unavailable("resize", session_id));
        };
        host.resize(session_id, rows, columns)
    }

    fn signal(&self, session_id: &str, signal: TerminalSignal) -> Result<ToolOutput, ToolError> {
        let Some(host) = self.host_for(session_id) else {
            return Ok(Self::unavailable("signal", session_id));
        };
        host.signal(session_id, signal)
    }

    fn close(&self, session_id: &str, policy: ClosePolicy) -> Result<ToolOutput, ToolError> {
        let Some(host) = self.host_for(session_id) else {
            return Ok(Self::unavailable("close", session_id));
        };
        host.close(session_id, policy)
    }
}

#[cfg(unix)]
impl TerminalMonitorSource for RoutingTerminalHost {
    fn observe_terminal(
        &self,
        session_id: &str,
        after_offset: u64,
        include_screen: bool,
    ) -> Result<Option<TerminalObservation>, ToolError> {
        if session_id.starts_with("terminal-t-") {
            return self.tmux.as_ref().map_or(Ok(None), |host| {
                host.observe_terminal(session_id, after_offset, include_screen)
            });
        }
        self.native
            .observe_terminal(session_id, after_offset, include_screen)
    }
}

fn list_sessions(output: &ToolOutput) -> Result<Vec<serde_json::Value>, ToolError> {
    output
        .structured
        .as_ref()
        .and_then(|value| value.pointer("/success/list/sessions"))
        .and_then(serde_json::Value::as_array)
        .cloned()
        .ok_or_else(|| ToolError::Execution("terminal host returned an invalid list result".into()))
}
