use std::collections::{BTreeMap, VecDeque};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant};

use fx_core::{CancellationSignal, ToolError, ToolOutput};
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::monitor::{MonitorDefinition, MonitorOperation, MonitorSignal};
use crate::terminal_observation::{ObservedLifecycle, TerminalMonitorSource, TerminalObservation};

const DEFAULT_ROWS: u16 = 24;
const DEFAULT_COLUMNS: u16 = 80;
const MAX_DIMENSION: u16 = 4096;
const MAX_RENDER_CELLS: usize = 262_144;
const MAX_RAW_BYTES: usize = 8 * 1024 * 1024;
const MAX_SESSIONS: usize = 32;
const WAIT_POLL: Duration = Duration::from_millis(10);

#[derive(Clone, Debug)]
pub(crate) struct StartSpec {
    pub backend: SessionBackend,
    pub workspace_root: PathBuf,
    pub cwd: PathBuf,
    pub initial_monitors: Vec<MonitorDefinition>,
    pub command: Option<String>,
    pub shell: PathBuf,
    pub arguments: Vec<String>,
    pub sandbox_profile: Option<String>,
    pub rows: u16,
    pub columns: u16,
    pub return_when: Option<WaitCondition>,
    pub wait_ceiling: Option<Duration>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SessionBackend {
    Native,
    Tmux,
}

#[derive(Clone, Debug)]
pub(crate) enum WaitCondition {
    Started,
    Exit,
    Quiet(Duration),
    Match(String),
}

#[derive(Clone, Debug)]
pub(crate) enum WritePayload {
    Text(String),
    Keys(Vec<NamedKey>),
    Controls(Vec<u8>),
    Paste(String),
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum NamedKey {
    Enter,
    Tab,
    Escape,
    Backspace,
    Delete,
    Insert,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    Home,
    End,
    PageUp,
    PageDown,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum TerminalSignal {
    Hangup,
    Interrupt,
    Quit,
    Terminate,
    Kill,
}

impl TerminalSignal {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Hangup => "hangup",
            Self::Interrupt => "interrupt",
            Self::Quit => "quit",
            Self::Terminate => "terminate",
            Self::Kill => "kill",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum ClosePolicy {
    Graceful,
    Force,
}

/// Blocking terminal-session port. The tool layer runs calls on Tokio's
/// blocking pool, while concrete backends retain their own synchronization
/// and lifecycle model. A tmux/recovery backend can implement this contract
/// without changing the public `terminal` tool or core agent traits.
pub(crate) trait TerminalSessionHost: Send + Sync {
    fn backends(&self) -> &'static [&'static str];

    fn start(
        &self,
        spec: StartSpec,
        cancellation: Arc<dyn CancellationSignal>,
    ) -> Result<ToolOutput, ToolError>;

    fn read(&self, session_id: &str, segment: u64, offset: u64) -> Result<ToolOutput, ToolError>;

    fn screen(&self, session_id: &str) -> Result<ToolOutput, ToolError>;

    fn write(&self, session_id: &str, payload: WritePayload) -> Result<ToolOutput, ToolError>;

    fn wait(
        &self,
        session_id: &str,
        condition: WaitCondition,
        ceiling: Duration,
        cancellation: Arc<dyn CancellationSignal>,
    ) -> Result<ToolOutput, ToolError>;

    fn monitor(
        &self,
        session_id: &str,
        operation: MonitorOperation,
    ) -> Result<ToolOutput, ToolError>;

    fn inspect(
        &self,
        session_id: &str,
        after_event_id: Option<u64>,
        acknowledge_event_id: Option<u64>,
        max_events: usize,
    ) -> Result<ToolOutput, ToolError>;

    fn list(&self, backend: Option<SessionBackend>) -> Result<ToolOutput, ToolError>;

    fn resize(&self, session_id: &str, rows: u16, columns: u16) -> Result<ToolOutput, ToolError>;

    fn signal(&self, session_id: &str, signal: TerminalSignal) -> Result<ToolOutput, ToolError>;

    fn close(&self, session_id: &str, policy: ClosePolicy) -> Result<ToolOutput, ToolError>;
}

impl ClosePolicy {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Graceful => "graceful",
            Self::Force => "force",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Lifecycle {
    Running,
    Exited,
    Lost,
    Closed,
}

impl Lifecycle {
    fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Exited => "exited",
            Self::Lost => "lost",
            Self::Closed => "closed",
        }
    }
}

#[derive(Clone, Debug)]
enum ReturnOutcome {
    Started,
    ConditionMet,
    SafetyCeiling,
    Cancelled,
    Exited(u32),
    Signal(String),
}

impl ReturnOutcome {
    fn to_json(&self) -> Value {
        match self {
            Self::Started => json!({"started": {}}),
            Self::ConditionMet => json!({"condition_met": {}}),
            Self::SafetyCeiling => json!({"safety_ceiling": {}}),
            Self::Cancelled => json!({"cancelled": {}}),
            Self::Exited(code) => json!({"exited": code}),
            // portable-pty exposes a platform-readable signal name. Preserve
            // that identity rather than fabricating a cross-platform number.
            Self::Signal(signal) => json!({"signal": signal}),
        }
    }
}

#[derive(Default)]
pub(crate) struct NativeTerminalHost {
    sessions: Mutex<BTreeMap<String, Arc<Session>>>,
}

impl NativeTerminalHost {
    pub(crate) fn has_sessions(&self) -> Result<bool, ToolError> {
        Ok(!self.lock_sessions()?.is_empty())
    }

    pub(crate) fn start(
        &self,
        spec: StartSpec,
        cancellation: Arc<dyn CancellationSignal>,
    ) -> Result<ToolOutput, ToolError> {
        if spec.backend != SessionBackend::Native {
            return Ok(failure_output("start", None, "unsupported_host", false));
        }
        if cancellation.is_cancelled() {
            return Ok(failure_output("start", None, "cancelled", false));
        }
        if !valid_dimensions(spec.rows, spec.columns) {
            return Ok(failure_output("start", None, "invalid_request", false));
        }
        {
            let sessions = self.lock_sessions()?;
            if sessions.len() >= MAX_SESSIONS {
                return Ok(failure_output("start", None, "capacity_exceeded", true));
            }
        }

        let pair = match native_pty_system().openpty(PtySize {
            rows: spec.rows,
            cols: spec.columns,
            pixel_width: 0,
            pixel_height: 0,
        }) {
            Ok(pair) => pair,
            Err(_) => return Ok(failure_output("start", None, "pty_unavailable", false)),
        };
        let reader = match pair.master.try_clone_reader() {
            Ok(reader) => reader,
            Err(_) => return Ok(failure_output("start", None, "pty_unavailable", false)),
        };
        let writer = match pair.master.take_writer() {
            Ok(writer) => writer,
            Err(_) => return Ok(failure_output("start", None, "pty_unavailable", false)),
        };
        let mut command = if let Some(profile) = spec.sandbox_profile.as_deref() {
            let mut command = CommandBuilder::new(crate::sandbox::MACOS_SANDBOX_EXEC);
            command.args(["-p", profile]);
            command.arg(&spec.shell);
            command.args(&spec.arguments);
            command
        } else {
            let mut command = CommandBuilder::new(&spec.shell);
            command.args(&spec.arguments);
            command
        };
        command.cwd(&spec.cwd);
        command.env("TERM", "xterm-256color");
        let child = match pair.slave.spawn_command(command) {
            Ok(child) => child,
            Err(_) => return Ok(failure_output("start", None, "startup_failed", false)),
        };
        drop(pair.slave);
        #[cfg(unix)]
        let process_group = pair.master.process_group_leader();
        let killer = child.clone_killer();

        let session_id = format!("terminal-n-{}", Uuid::new_v4().simple());
        let session = Arc::new(Session {
            session_id: session_id.clone(),
            workspace_root: spec.workspace_root,
            cwd: spec.cwd,
            command: spec.command,
            shell: spec.shell,
            master: Mutex::new(pair.master),
            writer: Mutex::new(writer),
            killer: Mutex::new(killer),
            #[cfg(unix)]
            process_group,
            state: Mutex::new(SessionState::new(spec.rows, spec.columns)),
            changed: Condvar::new(),
        });
        self.lock_sessions()?
            .insert(session_id.clone(), session.clone());

        if spawn_reader(Arc::downgrade(&session), reader).is_err() {
            self.lock_sessions()?.remove(&session_id);
            return Ok(failure_output(
                "start",
                Some(&session_id),
                "startup_failed",
                false,
            ));
        }
        if spawn_waiter(Arc::downgrade(&session), child).is_err() {
            self.lock_sessions()?.remove(&session_id);
            return Ok(failure_output(
                "start",
                Some(&session_id),
                "startup_failed",
                false,
            ));
        }

        let outcome = match spec.return_when {
            None | Some(WaitCondition::Started) => ReturnOutcome::Started,
            Some(condition) => {
                let ceiling = spec.wait_ceiling.unwrap_or(Duration::ZERO);
                wait_for(&session, &condition, ceiling, cancellation.as_ref())?
            }
        };
        let state = session.lock_state()?;
        let payload = json!({
            "session": session.facts(&state),
            "outcome": outcome.to_json()
        });
        Ok(success_output("start", payload))
    }

    pub(crate) fn read(
        &self,
        session_id: &str,
        segment: u64,
        offset: u64,
    ) -> Result<ToolOutput, ToolError> {
        let Some(session) = self.find(session_id)? else {
            return Ok(failure_output(
                "read",
                Some(session_id),
                "session_not_found",
                false,
            ));
        };
        if segment != 1 {
            return Ok(failure_output(
                "read",
                Some(session_id),
                "cursor_gap",
                false,
            ));
        }
        let mut state = session.lock_state()?;
        if offset < state.base_offset || offset > state.total_offset {
            return Ok(failure_output(
                "read",
                Some(session_id),
                "cursor_gap",
                false,
            ));
        }
        let start = usize::try_from(offset - state.base_offset)
            .map_err(|_| ToolError::Execution("terminal cursor overflow".into()))?;
        let bytes: Vec<u8> = state.raw.iter().skip(start).copied().collect();
        let end = state.total_offset;
        state.reader_offset = end;
        let raw_range = if offset == end {
            Value::Null
        } else {
            raw_range(offset, end)
        };
        let payload = json!({
            "session": session.facts(&state),
            "output": String::from_utf8_lossy(&bytes),
            "raw_range": raw_range
        });
        Ok(success_output("read", payload))
    }

    pub(crate) fn screen(&self, session_id: &str) -> Result<ToolOutput, ToolError> {
        let Some(session) = self.find(session_id)? else {
            return Ok(failure_output(
                "screen",
                Some(session_id),
                "session_not_found",
                false,
            ));
        };
        let state = session.lock_state()?;
        let payload = json!({
            "session": session.facts(&state),
            "snapshot": render_snapshot(&state.parser, state.rows, state.columns, None)
        });
        Ok(success_output("screen", payload))
    }

    pub(crate) fn write(
        &self,
        session_id: &str,
        payload: WritePayload,
    ) -> Result<ToolOutput, ToolError> {
        let Some(session) = self.find(session_id)? else {
            return Ok(failure_output(
                "write",
                Some(session_id),
                "session_not_found",
                false,
            ));
        };
        {
            let state = session.lock_state()?;
            if state.lifecycle != Lifecycle::Running {
                return Ok(failure_output(
                    "write",
                    Some(session_id),
                    "invalid_lifecycle",
                    false,
                ));
            }
        }
        let bytes = encode_write(payload);
        let accepted = bytes.len();
        let mut writer = session
            .writer
            .lock()
            .map_err(|_| ToolError::Execution("terminal writer lock is poisoned".into()))?;
        writer
            .write_all(&bytes)
            .and_then(|()| writer.flush())
            .map_err(|error| ToolError::Execution(format!("terminal write failed: {error}")))?;
        drop(writer);
        let state = session.lock_state()?;
        let payload = json!({
            "session": session.facts(&state),
            "accepted_bytes": accepted
        });
        Ok(success_output("write", payload))
    }

    pub(crate) fn wait(
        &self,
        session_id: &str,
        condition: WaitCondition,
        ceiling: Duration,
        cancellation: Arc<dyn CancellationSignal>,
    ) -> Result<ToolOutput, ToolError> {
        let Some(session) = self.find(session_id)? else {
            return Ok(failure_output(
                "wait",
                Some(session_id),
                "session_not_found",
                false,
            ));
        };
        let outcome = wait_for(&session, &condition, ceiling, cancellation.as_ref())?;
        let state = session.lock_state()?;
        let payload = json!({
            "session": session.facts(&state),
            "outcome": outcome.to_json()
        });
        Ok(success_output("wait", payload))
    }

    pub(crate) fn inspect(&self, session_id: &str) -> Result<ToolOutput, ToolError> {
        let Some(session) = self.find(session_id)? else {
            return Ok(failure_output(
                "inspect",
                Some(session_id),
                "session_not_found",
                false,
            ));
        };
        let state = session.lock_state()?;
        let payload = json!({
            "session": session.facts(&state),
            "shell": session.shell,
            "cwd": session.cwd,
            "command": session.command,
            "monitors": [],
            "events": [],
            "event_gap_through": 0,
            "next_event_id": 1
        });
        Ok(success_output("inspect", payload))
    }

    pub(crate) fn list(&self) -> Result<ToolOutput, ToolError> {
        let sessions: Vec<_> = self.lock_sessions()?.values().cloned().collect();
        let mut facts = Vec::with_capacity(sessions.len());
        for session in sessions {
            let state = session.lock_state()?;
            facts.push(session.facts(&state));
        }
        Ok(success_output("list", json!({"sessions": facts})))
    }

    pub(crate) fn resize(
        &self,
        session_id: &str,
        rows: u16,
        columns: u16,
    ) -> Result<ToolOutput, ToolError> {
        let Some(session) = self.find(session_id)? else {
            return Ok(failure_output(
                "resize",
                Some(session_id),
                "session_not_found",
                false,
            ));
        };
        if !valid_dimensions(rows, columns) {
            return Ok(failure_output(
                "resize",
                Some(session_id),
                "invalid_request",
                false,
            ));
        }
        session
            .master
            .lock()
            .map_err(|_| ToolError::Execution("terminal master lock is poisoned".into()))?
            .resize(PtySize {
                rows,
                cols: columns,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|error| ToolError::Execution(format!("terminal resize failed: {error}")))?;
        let mut state = session.lock_state()?;
        state.rows = rows;
        state.columns = columns;
        state.parser.screen_mut().set_size(rows, columns);
        let payload = json!({
            "session": session.facts(&state),
            "dimensions": {"rows": rows, "columns": columns}
        });
        Ok(success_output("resize", payload))
    }

    pub(crate) fn signal(
        &self,
        session_id: &str,
        signal: TerminalSignal,
    ) -> Result<ToolOutput, ToolError> {
        let Some(session) = self.find(session_id)? else {
            return Ok(failure_output(
                "signal",
                Some(session_id),
                "session_not_found",
                false,
            ));
        };
        if session.lock_state()?.lifecycle != Lifecycle::Running {
            return Ok(failure_output(
                "signal",
                Some(session_id),
                "invalid_lifecycle",
                false,
            ));
        }
        session.send_signal(signal)?;
        let state = session.lock_state()?;
        let payload = json!({
            "session": session.facts(&state),
            "signal": signal.as_str()
        });
        Ok(success_output("signal", payload))
    }

    pub(crate) fn close(
        &self,
        session_id: &str,
        policy: ClosePolicy,
    ) -> Result<ToolOutput, ToolError> {
        let Some(session) = self.find(session_id)? else {
            return Ok(failure_output(
                "close",
                Some(session_id),
                "session_not_found",
                false,
            ));
        };
        let running = session.lock_state()?.lifecycle == Lifecycle::Running;
        if running {
            match policy {
                ClosePolicy::Graceful => session.send_signal(TerminalSignal::Hangup)?,
                ClosePolicy::Force => session.send_signal(TerminalSignal::Kill)?,
            }
        }
        let mut state = session.lock_state()?;
        state.lifecycle = Lifecycle::Closed;
        let facts = session.facts(&state);
        drop(state);
        self.lock_sessions()?.remove(session_id);
        Ok(success_output(
            "close",
            json!({"session": facts, "policy": policy.as_str()}),
        ))
    }

    fn find(&self, session_id: &str) -> Result<Option<Arc<Session>>, ToolError> {
        Ok(self.lock_sessions()?.get(session_id).cloned())
    }

    fn lock_sessions(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, BTreeMap<String, Arc<Session>>>, ToolError> {
        self.sessions
            .lock()
            .map_err(|_| ToolError::Execution("terminal session map is poisoned".into()))
    }
}

impl TerminalSessionHost for NativeTerminalHost {
    fn backends(&self) -> &'static [&'static str] {
        &["native"]
    }

    fn start(
        &self,
        spec: StartSpec,
        cancellation: Arc<dyn CancellationSignal>,
    ) -> Result<ToolOutput, ToolError> {
        Self::start(self, spec, cancellation)
    }

    fn read(&self, session_id: &str, segment: u64, offset: u64) -> Result<ToolOutput, ToolError> {
        Self::read(self, session_id, segment, offset)
    }

    fn screen(&self, session_id: &str) -> Result<ToolOutput, ToolError> {
        Self::screen(self, session_id)
    }

    fn write(&self, session_id: &str, payload: WritePayload) -> Result<ToolOutput, ToolError> {
        Self::write(self, session_id, payload)
    }

    fn wait(
        &self,
        session_id: &str,
        condition: WaitCondition,
        ceiling: Duration,
        cancellation: Arc<dyn CancellationSignal>,
    ) -> Result<ToolOutput, ToolError> {
        Self::wait(self, session_id, condition, ceiling, cancellation)
    }

    fn inspect(
        &self,
        session_id: &str,
        _after_event_id: Option<u64>,
        _acknowledge_event_id: Option<u64>,
        _max_events: usize,
    ) -> Result<ToolOutput, ToolError> {
        Self::inspect(self, session_id)
    }

    fn monitor(
        &self,
        session_id: &str,
        _operation: MonitorOperation,
    ) -> Result<ToolOutput, ToolError> {
        Ok(failure_output(
            "monitor",
            Some(session_id),
            "monitor_unavailable",
            false,
        ))
    }

    fn list(&self, backend: Option<SessionBackend>) -> Result<ToolOutput, ToolError> {
        if backend == Some(SessionBackend::Tmux) {
            return Ok(success_output("list", json!({"sessions": []})));
        }
        Self::list(self)
    }

    fn resize(&self, session_id: &str, rows: u16, columns: u16) -> Result<ToolOutput, ToolError> {
        Self::resize(self, session_id, rows, columns)
    }

    fn signal(&self, session_id: &str, signal: TerminalSignal) -> Result<ToolOutput, ToolError> {
        Self::signal(self, session_id, signal)
    }

    fn close(&self, session_id: &str, policy: ClosePolicy) -> Result<ToolOutput, ToolError> {
        Self::close(self, session_id, policy)
    }
}

impl TerminalMonitorSource for NativeTerminalHost {
    fn observe_terminal(
        &self,
        session_id: &str,
        after_offset: u64,
        include_screen: bool,
    ) -> Result<Option<TerminalObservation>, ToolError> {
        let Some(session) = self.find(session_id)? else {
            return Ok(None);
        };
        let state = session.lock_state()?;
        if after_offset > state.total_offset {
            return Err(ToolError::Execution(
                "terminal monitor cursor is ahead of output".into(),
            ));
        }
        let raw_gap = after_offset < state.base_offset;
        let cursor_start = after_offset.max(state.base_offset);
        let start = usize::try_from(cursor_start - state.base_offset)
            .map_err(|_| ToolError::Execution("terminal monitor cursor overflow".into()))?;
        let output = state.raw.iter().skip(start).copied().collect();
        let lifecycle = match state.lifecycle {
            Lifecycle::Running => ObservedLifecycle::Running,
            Lifecycle::Exited => ObservedLifecycle::Exited,
            Lifecycle::Lost => ObservedLifecycle::Lost,
            Lifecycle::Closed => ObservedLifecycle::Closed,
        };
        Ok(Some(TerminalObservation {
            lifecycle,
            exit_code: state.exit_code.and_then(|code| i32::try_from(code).ok()),
            signal: state.exit_signal.as_deref().and_then(monitor_signal),
            cursor_start,
            cursor_end: state.total_offset,
            raw_gap,
            output,
            screen_text: include_screen.then(|| state.parser.screen().contents()),
            workspace_root: session.workspace_root.clone(),
            cwd: session.cwd.clone(),
        }))
    }
}

fn monitor_signal(signal: &str) -> Option<MonitorSignal> {
    let normalized = signal.to_ascii_lowercase();
    if normalized.contains("hangup") || normalized == "sighup" {
        Some(MonitorSignal::Hangup)
    } else if normalized.contains("interrupt") || normalized == "sigint" {
        Some(MonitorSignal::Interrupt)
    } else if normalized.contains("quit") || normalized == "sigquit" {
        Some(MonitorSignal::Quit)
    } else if normalized.contains("kill") || normalized == "sigkill" {
        Some(MonitorSignal::Kill)
    } else if normalized.contains("terminat") || normalized == "sigterm" {
        Some(MonitorSignal::Terminate)
    } else {
        None
    }
}

struct Session {
    session_id: String,
    workspace_root: PathBuf,
    cwd: PathBuf,
    command: Option<String>,
    shell: PathBuf,
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    #[cfg(unix)]
    process_group: Option<i32>,
    state: Mutex<SessionState>,
    changed: Condvar,
}

impl Session {
    fn lock_state(&self) -> Result<std::sync::MutexGuard<'_, SessionState>, ToolError> {
        self.state
            .lock()
            .map_err(|_| ToolError::Execution("terminal state lock is poisoned".into()))
    }

    fn facts(&self, state: &SessionState) -> Value {
        let unread_range = if state.reader_offset < state.total_offset {
            raw_range(
                state.reader_offset.max(state.base_offset),
                state.total_offset,
            )
        } else {
            Value::Null
        };
        let raw_gap = if state.reader_offset < state.base_offset {
            json!({
                "missing_from": cursor(state.reader_offset),
                "available_from": cursor(state.base_offset)
            })
        } else {
            Value::Null
        };
        let running = state.lifecycle == Lifecycle::Running;
        let observable = state.lifecycle != Lifecycle::Closed;
        json!({
            "session_id": self.session_id,
            "lifecycle": state.lifecycle.as_str(),
            "attention": {"attention": "background", "write_lease": "none"},
            "backend": "native",
            "persistence": "process",
            "output_cursor": cursor(state.total_offset),
            "unread_range": unread_range,
            "raw_gap": raw_gap,
            "screen_recovery": {"unavailable": "missing"},
            "active_monitor_count": 0,
            "next_actions": {
                "read": observable,
                "screen": observable,
                "write": running,
                "wait": observable,
                "monitor": false,
                "inspect": observable,
                "list": true,
                "resize": running,
                "signal": running,
                "close": observable
            }
        })
    }

    fn send_signal(&self, signal: TerminalSignal) -> Result<(), ToolError> {
        #[cfg(unix)]
        if let Some(group) = self.process_group {
            use nix::sys::signal::{Signal, killpg};
            use nix::unistd::Pid;

            let signal = match signal {
                TerminalSignal::Hangup => Signal::SIGHUP,
                TerminalSignal::Interrupt => Signal::SIGINT,
                TerminalSignal::Quit => Signal::SIGQUIT,
                TerminalSignal::Terminate => Signal::SIGTERM,
                TerminalSignal::Kill => Signal::SIGKILL,
            };
            return killpg(Pid::from_raw(group), signal).map_err(|error| {
                ToolError::Execution(format!("could not signal terminal process group: {error}"))
            });
        }

        match signal {
            TerminalSignal::Interrupt => self.write_control(3),
            TerminalSignal::Quit => self.write_control(28),
            TerminalSignal::Hangup => self.write_control(4),
            TerminalSignal::Terminate | TerminalSignal::Kill => self
                .killer
                .lock()
                .map_err(|_| ToolError::Execution("terminal killer lock is poisoned".into()))?
                .kill()
                .map_err(|error| ToolError::Execution(format!("could not kill terminal: {error}"))),
        }
    }

    fn write_control(&self, byte: u8) -> Result<(), ToolError> {
        let mut writer = self
            .writer
            .lock()
            .map_err(|_| ToolError::Execution("terminal writer lock is poisoned".into()))?;
        writer
            .write_all(&[byte])
            .and_then(|()| writer.flush())
            .map_err(|error| {
                ToolError::Execution(format!("terminal control write failed: {error}"))
            })
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let running = self
            .state
            .get_mut()
            .is_ok_and(|state| state.lifecycle == Lifecycle::Running);
        if running {
            #[cfg(unix)]
            if let Some(group) = self.process_group {
                let _ = nix::sys::signal::killpg(
                    nix::unistd::Pid::from_raw(group),
                    nix::sys::signal::Signal::SIGHUP,
                );
                return;
            }
            if let Ok(killer) = self.killer.get_mut() {
                let _ = killer.kill();
            }
        }
    }
}

struct SessionState {
    lifecycle: Lifecycle,
    exit_code: Option<u32>,
    exit_signal: Option<String>,
    raw: VecDeque<u8>,
    base_offset: u64,
    total_offset: u64,
    reader_offset: u64,
    parser: vt100::Parser,
    rows: u16,
    columns: u16,
    last_output: Instant,
}

impl SessionState {
    fn new(rows: u16, columns: u16) -> Self {
        Self {
            lifecycle: Lifecycle::Running,
            exit_code: None,
            exit_signal: None,
            raw: VecDeque::new(),
            base_offset: 0,
            total_offset: 0,
            reader_offset: 0,
            parser: vt100::Parser::new(rows, columns, 10_000),
            rows,
            columns,
            last_output: Instant::now(),
        }
    }

    fn append(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
        self.raw.extend(bytes.iter().copied());
        self.total_offset = self
            .total_offset
            .saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        if self.raw.len() > MAX_RAW_BYTES {
            let remove = self.raw.len() - MAX_RAW_BYTES;
            self.raw.drain(..remove);
            self.base_offset = self
                .base_offset
                .saturating_add(u64::try_from(remove).unwrap_or(u64::MAX));
        }
        self.last_output = Instant::now();
    }

    fn terminal_outcome(&self) -> Option<ReturnOutcome> {
        match self.lifecycle {
            Lifecycle::Running => None,
            Lifecycle::Exited => Some(match &self.exit_signal {
                Some(signal) => ReturnOutcome::Signal(signal.clone()),
                None => ReturnOutcome::Exited(self.exit_code.unwrap_or(1)),
            }),
            Lifecycle::Lost => Some(ReturnOutcome::Signal("session_lost".into())),
            Lifecycle::Closed => Some(ReturnOutcome::Signal("closed".into())),
        }
    }
}

fn spawn_reader(session: Weak<Session>, mut reader: Box<dyn Read + Send>) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("fx-terminal-reader".into())
        .spawn(move || {
            let mut buffer = [0_u8; 8192];
            loop {
                let count = match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => count,
                    Err(_) => break,
                };
                let Some(session) = session.upgrade() else {
                    break;
                };
                if let Ok(mut state) = session.state.lock() {
                    state.append(&buffer[..count]);
                    session.changed.notify_all();
                } else {
                    break;
                }
            }
        })?;
    Ok(())
}

fn spawn_waiter(
    session: Weak<Session>,
    mut child: Box<dyn portable_pty::Child + Send + Sync>,
) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("fx-terminal-waiter".into())
        .spawn(move || {
            let status = child.wait();
            let Some(session) = session.upgrade() else {
                return;
            };
            if let Ok(mut state) = session.state.lock() {
                if state.lifecycle == Lifecycle::Running {
                    match status {
                        Ok(status) => {
                            state.lifecycle = Lifecycle::Exited;
                            state.exit_code = Some(status.exit_code());
                            state.exit_signal = status.signal().map(str::to_owned);
                        }
                        Err(_) => state.lifecycle = Lifecycle::Lost,
                    }
                }
                session.changed.notify_all();
            }
        })?;
    Ok(())
}

fn wait_for(
    session: &Session,
    condition: &WaitCondition,
    ceiling: Duration,
    cancellation: &dyn CancellationSignal,
) -> Result<ReturnOutcome, ToolError> {
    let started = Instant::now();
    let mut state = session.lock_state()?;
    loop {
        if cancellation.is_cancelled() {
            return Ok(ReturnOutcome::Cancelled);
        }
        if let Some(outcome) = state.terminal_outcome() {
            return Ok(outcome);
        }
        let condition_met = match condition {
            WaitCondition::Started => true,
            WaitCondition::Exit => false,
            WaitCondition::Quiet(duration) => state.last_output.elapsed() >= *duration,
            WaitCondition::Match(pattern) => retained_output_contains(&state, pattern.as_bytes()),
        };
        if condition_met {
            return Ok(match condition {
                WaitCondition::Started => ReturnOutcome::Started,
                _ => ReturnOutcome::ConditionMet,
            });
        }
        if started.elapsed() >= ceiling {
            return Ok(ReturnOutcome::SafetyCeiling);
        }
        let remaining = ceiling.saturating_sub(started.elapsed());
        let wait = remaining.min(WAIT_POLL);
        let (next, _) = session
            .changed
            .wait_timeout(state, wait)
            .map_err(|_| ToolError::Execution("terminal wait lock is poisoned".into()))?;
        state = next;
    }
}

fn retained_output_contains(state: &SessionState, pattern: &[u8]) -> bool {
    if pattern.is_empty() || pattern.len() > state.raw.len() {
        return false;
    }
    let contiguous: Vec<_> = state.raw.iter().copied().collect();
    contiguous
        .windows(pattern.len())
        .any(|window| window == pattern)
}

fn encode_write(payload: WritePayload) -> Vec<u8> {
    match payload {
        WritePayload::Text(text) => text.into_bytes(),
        WritePayload::Paste(text) => {
            let mut bytes = Vec::with_capacity(text.len() + 12);
            bytes.extend_from_slice(b"\x1b[200~");
            bytes.extend_from_slice(text.as_bytes());
            bytes.extend_from_slice(b"\x1b[201~");
            bytes
        }
        WritePayload::Controls(controls) => controls
            .into_iter()
            .map(|character| match character {
                b'?' => 0x7f,
                b'a'..=b'z' => character - b'a' + 1,
                b'@'..=b'_' => character - b'@',
                _ => character,
            })
            .collect(),
        WritePayload::Keys(keys) => {
            let mut bytes = Vec::new();
            for key in keys {
                bytes.extend_from_slice(match key {
                    NamedKey::Enter => b"\r",
                    NamedKey::Tab => b"\t",
                    NamedKey::Escape => b"\x1b",
                    NamedKey::Backspace => b"\x7f",
                    NamedKey::Delete => b"\x1b[3~",
                    NamedKey::Insert => b"\x1b[2~",
                    NamedKey::ArrowUp => b"\x1b[A",
                    NamedKey::ArrowDown => b"\x1b[B",
                    NamedKey::ArrowLeft => b"\x1b[D",
                    NamedKey::ArrowRight => b"\x1b[C",
                    NamedKey::Home => b"\x1b[H",
                    NamedKey::End => b"\x1b[F",
                    NamedKey::PageUp => b"\x1b[5~",
                    NamedKey::PageDown => b"\x1b[6~",
                });
            }
            bytes
        }
    }
}

pub(crate) fn render_snapshot(
    parser: &vt100::Parser,
    rows: u16,
    columns: u16,
    cursor_override: Option<(u16, u16)>,
) -> Value {
    let screen = parser.screen();
    let (cursor_row, cursor_column) = cursor_override.unwrap_or_else(|| screen.cursor_position());
    let mut cells = Vec::with_capacity(usize::from(rows) * usize::from(columns));
    for row in 0..rows {
        for column in 0..columns {
            let Some(cell) = screen.cell(row, column) else {
                cells.push(blank_cell());
                continue;
            };
            let kind = if cell.is_wide_continuation() {
                "continuation"
            } else if cell.is_wide() {
                "wide"
            } else if cell.has_contents() {
                "single"
            } else {
                "blank"
            };
            cells.push(json!({
                "kind": kind,
                "text": cell.contents(),
                "style": {
                    "foreground": color(cell.fgcolor()),
                    "background": color(cell.bgcolor()),
                    "bold": cell.bold(),
                    "faint": cell.dim(),
                    "italic": cell.italic(),
                    "underline": cell.underline(),
                    "inverse": cell.inverse(),
                    "strikethrough": false
                }
            }));
        }
    }
    json!({
        "dimensions": {"rows": rows, "columns": columns},
        "cursor": {
            "row": cursor_row,
            "column": cursor_column,
            "visible": !screen.hide_cursor(),
            "shape": "block",
            "blinking": true
        },
        "modes": {
            "insert": false,
            "bracketed_paste": screen.bracketed_paste(),
            "mouse_tracking": !matches!(screen.mouse_protocol_mode(), vt100::MouseProtocolMode::None),
            "focus_tracking": false,
            "application_cursor_keys": screen.application_cursor(),
            "application_keypad": screen.application_keypad(),
            "keyboard_protocol": false,
            "synchronized_updates": false
        },
        "cells": cells
    })
}

fn blank_cell() -> Value {
    json!({
        "kind": "blank",
        "text": "",
        "style": {
            "foreground": {"default": {}},
            "background": {"default": {}},
            "bold": false,
            "faint": false,
            "italic": false,
            "underline": false,
            "inverse": false,
            "strikethrough": false
        }
    })
}

fn color(color: vt100::Color) -> Value {
    match color {
        vt100::Color::Default => json!({"default": {}}),
        vt100::Color::Idx(index) => json!({"indexed": index}),
        vt100::Color::Rgb(red, green, blue) => {
            json!({"rgb": {"red": red, "green": green, "blue": blue}})
        }
    }
}

pub(crate) fn valid_dimensions(rows: u16, columns: u16) -> bool {
    rows > 0
        && columns > 0
        && rows <= MAX_DIMENSION
        && columns <= MAX_DIMENSION
        && usize::from(rows).saturating_mul(usize::from(columns)) <= MAX_RENDER_CELLS
}

pub(crate) fn default_dimensions() -> (u16, u16) {
    (DEFAULT_ROWS, DEFAULT_COLUMNS)
}

pub(crate) fn cursor(offset: u64) -> Value {
    json!({"segment": 1, "offset": offset})
}

pub(crate) fn raw_range(start: u64, end: u64) -> Value {
    json!({"start": cursor(start), "end": cursor(end)})
}

pub(crate) fn success_output(action: &str, payload: Value) -> ToolOutput {
    let mut action_result = Map::new();
    action_result.insert(action.to_owned(), payload);
    let root = json!({"success": Value::Object(action_result)});
    output(root, false)
}

pub(crate) fn failure_output(
    action: &str,
    session_id: Option<&str>,
    code: &str,
    retryable: bool,
) -> ToolOutput {
    output(
        json!({
            "failure": {
                "action": action,
                "code": code,
                "session_id": session_id,
                "retryable": retryable
            }
        }),
        true,
    )
}

fn output(root: Value, is_error: bool) -> ToolOutput {
    let content = serde_json::to_string(&root).expect("terminal result JSON is serializable");
    ToolOutput {
        original_bytes: content.len(),
        content,
        is_error,
        structured: Some(root),
        truncated: false,
        durable_content: None,
    }
}
