use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use fx_core::{
    BoxFuture, CommandReview, PermissionRequest, PreparedToolAction, PreparedToolCall, Tool,
    ToolContext, ToolEffect, ToolError, ToolOutput, ToolPreparation, ToolReview,
};
use fx_workspace::resolve_existing;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::monitor::{MonitorDefinition, MonitorOperation};
use crate::native_terminal::{
    ClosePolicy, NamedKey, SessionBackend, StartSpec, TerminalSessionHost, TerminalSignal,
    WaitCondition, WritePayload, default_dimensions,
};
use crate::terminal_host::RoutingTerminalHost;
use crate::{MAX_COMMAND_BYTES, Profile, TerminalExec, resolve_shell};

const MAX_WRITE_BYTES: usize = 64 * 1024;
const MAX_WRITE_ITEMS: usize = 4096;
const MAX_MATCH_BYTES: usize = 4096;

const DESCRIPTION: &str = "Select one action and set fields unused by that action to null or omit them. exec runs one captured foreground command. start/read/screen/write/wait/monitor/inspect/list/resize/signal/close control PTY sessions. On Unix, native sessions are owned by a detached private companion and survive agent-client restarts while that host lives; backend=tmux is advertised when installed and is restart-durable. Monitors are durable, non-consuming observations; retrieve and acknowledge their events with inspect. Omitted profile means user; profile=clean skips Bash/zsh startup files.";

#[derive(Clone)]
pub struct Terminal {
    foreground: TerminalExec,
    sessions: Arc<dyn TerminalSessionHost>,
}

impl Default for Terminal {
    fn default() -> Self {
        Self {
            foreground: TerminalExec::default(),
            sessions: Arc::new(RoutingTerminalHost::discover()),
        }
    }
}

impl std::fmt::Debug for Terminal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Terminal")
            .field("foreground", &self.foreground)
            .finish_non_exhaustive()
    }
}

impl Terminal {
    pub fn with_login_shell(path: impl Into<PathBuf>) -> Self {
        Self {
            foreground: TerminalExec::with_login_shell(path),
            sessions: Arc::new(RoutingTerminalHost::discover()),
        }
    }

    #[cfg(all(test, unix))]
    fn with_local_login_shell(path: impl Into<PathBuf>) -> Self {
        Self {
            foreground: TerminalExec::with_login_shell(path),
            sessions: Arc::new(RoutingTerminalHost::local()),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Action {
    Exec,
    Start,
    Read,
    Screen,
    Write,
    Wait,
    Monitor,
    Inspect,
    List,
    Resize,
    Signal,
    Close,
}

impl Action {
    fn as_str(self) -> &'static str {
        match self {
            Self::Exec => "exec",
            Self::Start => "start",
            Self::Read => "read",
            Self::Screen => "screen",
            Self::Write => "write",
            Self::Wait => "wait",
            Self::Monitor => "monitor",
            Self::Inspect => "inspect",
            Self::List => "list",
            Self::Resize => "resize",
            Self::Signal => "signal",
            Self::Close => "close",
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    action: Action,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    profile: Option<Profile>,
    #[serde(default)]
    shell: Option<ShellInput>,
    #[serde(default)]
    backend: Option<Backend>,
    #[serde(default)]
    return_when: Option<ReturnInput>,
    #[serde(default)]
    wait_ceiling_ms: Option<u64>,
    #[serde(default)]
    dimensions: Option<DimensionsInput>,
    #[serde(default)]
    initial_monitors: Vec<MonitorDefinition>,
    #[serde(default)]
    cursor_segment: Option<u64>,
    #[serde(default)]
    cursor_offset: Option<u64>,
    #[serde(default)]
    after_event_id: Option<u64>,
    #[serde(default)]
    acknowledge_event_id: Option<u64>,
    #[serde(default)]
    max_events: Option<u16>,
    #[serde(default)]
    write: Option<WriteInput>,
    #[serde(default)]
    monitor: Option<MonitorOperation>,
    #[serde(default)]
    rows: Option<u16>,
    #[serde(default)]
    columns: Option<u16>,
    #[serde(default)]
    signal: Option<SignalInput>,
    #[serde(default)]
    close_policy: Option<ClosePolicyInput>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ShellInput {
    #[serde(default)]
    kind: ShellKind,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    clean_start: bool,
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ShellKind {
    #[default]
    UserLogin,
    Executable,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Backend {
    Native,
    Tmux,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReturnInput {
    kind: ReturnKind,
    #[serde(default)]
    duration_ms: Option<u64>,
    #[serde(default)]
    pattern: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ReturnKind {
    Started,
    Exit,
    Quiet,
    Match,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DimensionsInput {
    rows: u16,
    columns: u16,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteInput {
    kind: PayloadKind,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    keys: Vec<NamedKeyInput>,
    #[serde(default)]
    controls: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PayloadKind {
    Text,
    Keys,
    Controls,
    Paste,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum NamedKeyInput {
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

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SignalInput {
    Hangup,
    Interrupt,
    Quit,
    Terminate,
    Kill,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ClosePolicyInput {
    Graceful,
    Force,
}

impl Tool for Terminal {
    fn name(&self) -> &str {
        "terminal"
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn input_schema(&self) -> Value {
        terminal_schema(self.sessions.backends())
    }

    fn effect(&self, arguments: &Value) -> Result<ToolEffect, ToolError> {
        let input = decode(arguments)?;
        Ok(match input.action {
            Action::Read | Action::Screen | Action::Inspect | Action::List => ToolEffect::Read,
            Action::Exec
            | Action::Start
            | Action::Write
            | Action::Wait
            | Action::Monitor
            | Action::Resize
            | Action::Signal
            | Action::Close => ToolEffect::Process,
        })
    }

    fn project_context_targets(
        &self,
        context: &ToolContext,
        arguments: &Value,
    ) -> Result<Vec<PathBuf>, ToolError> {
        let input = decode(arguments)?;
        if !matches!(input.action, Action::Exec | Action::Start) {
            return Ok(Vec::new());
        }
        let cwd = resolve_existing(&context.workspace_root, input.cwd.as_deref().unwrap_or("."))?;
        Ok(vec![cwd.absolute])
    }

    fn prepare(
        &self,
        context: &ToolContext,
        arguments: &Value,
    ) -> Result<ToolPreparation, ToolError> {
        let input = decode(arguments)?;
        match input.action {
            Action::Exec => self.foreground.prepare(context, arguments),
            Action::Start => self.prepare_start(context, input),
            Action::Monitor if input.monitor.as_ref().and_then(custom_probe).is_some() => {
                self.prepare_monitor(input)
            }
            _ => Ok(ToolPreparation::Direct {
                // Session ownership is capability authority. Once a reviewed
                // start created the session, control actions do not repeatedly
                // ask the user for the same process authority.
                permission_requests: Vec::new(),
                irreversible: false,
            }),
        }
    }

    fn execute<'a>(
        &'a self,
        context: &'a ToolContext,
        arguments: Value,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            let input = decode(&arguments)?;
            let sessions = self.sessions.clone();
            let cancellation = context.cancellation.clone();
            tokio::task::spawn_blocking(move || execute_session(sessions, input, cancellation))
                .await
                .map_err(|error| ToolError::Execution(format!("terminal task failed: {error}")))?
        })
    }
}

impl Terminal {
    fn prepare_start(
        &self,
        context: &ToolContext,
        input: Input,
    ) -> Result<ToolPreparation, ToolError> {
        let requested_cwd = input.cwd.as_deref().unwrap_or(".");
        let cwd = resolve_existing(&context.workspace_root, requested_cwd)?.absolute;
        if !cwd.is_dir() {
            return Err(ToolError::InvalidArguments(format!(
                "terminal cwd is not a directory: {}",
                cwd.display()
            )));
        }
        let (shell, profile_label, clean_start) = match input.shell.as_ref() {
            None => {
                let profile = input.profile.unwrap_or_default();
                (
                    resolve_shell(self.foreground.login_shell.as_deref())?,
                    profile.as_str().to_owned(),
                    profile == Profile::Clean,
                )
            }
            Some(shell) if matches!(shell.kind, ShellKind::UserLogin) => (
                resolve_shell(self.foreground.login_shell.as_deref())?,
                "user".into(),
                false,
            ),
            Some(shell) => {
                let path = shell.path.as_deref().ok_or_else(|| {
                    ToolError::InvalidArguments(
                        "terminal start executable shell requires path".into(),
                    )
                })?;
                let path = PathBuf::from(path);
                if !path.is_absolute() {
                    return Err(ToolError::InvalidArguments(
                        "terminal start executable shell path must be absolute".into(),
                    ));
                }
                let path = path.canonicalize().map_err(|error| {
                    ToolError::Execution(format!("{}: {error}", path.display()))
                })?;
                (path, "custom".into(), shell.clean_start)
            }
        };
        let profile = if clean_start {
            Profile::Clean
        } else {
            Profile::User
        };
        let command = input.command.clone().filter(|command| !command.is_empty());
        let arguments = start_arguments(&shell, profile, command.as_deref())?;
        let (rows, columns) = input
            .dimensions
            .map_or_else(default_dimensions, |value| (value.rows, value.columns));
        let return_when = input.return_when.map(build_wait_condition).transpose()?;
        let non_immediate = return_when
            .as_ref()
            .is_some_and(|condition| !matches!(condition, WaitCondition::Started));
        let wait_ceiling = input.wait_ceiling_ms.map(Duration::from_millis);
        if non_immediate && wait_ceiling.is_none() {
            return Err(ToolError::InvalidArguments(
                "terminal start requires wait_ceiling_ms for a non-immediate return condition"
                    .into(),
            ));
        }
        let command_target = command
            .clone()
            .unwrap_or_else(|| shell.display().to_string());
        let spec = StartSpec {
            backend: match input.backend.unwrap_or(Backend::Native) {
                Backend::Native => SessionBackend::Native,
                Backend::Tmux => SessionBackend::Tmux,
            },
            workspace_root: context.workspace_root.canonicalize().map_err(|error| {
                ToolError::Execution(format!("workspace is unavailable: {error}"))
            })?,
            cwd: cwd.clone(),
            initial_monitors: input.initial_monitors,
            command,
            shell: shell.clone(),
            arguments,
            sandbox_profile: crate::sandbox::profile(context)?,
            rows,
            columns,
            return_when,
            wait_ceiling,
        };
        let permission = PermissionRequest::new(self.name(), &command_target, ToolEffect::Process)
            .with_grant_target(start_grant_target(
                &command_target,
                &cwd,
                &shell,
                &profile_label,
            ));
        let mut permissions = vec![permission];
        for definition in &spec.initial_monitors {
            if let Some((command, probe_cwd)) = definition_custom_probe(definition) {
                permissions.push(
                    PermissionRequest::new(self.name(), command, ToolEffect::Process)
                        .with_grant_target(format!(
                            "@fx-terminal-monitor:{}:{probe_cwd}:{command}",
                            probe_cwd.len()
                        )),
                );
            }
        }
        let review = CommandReview {
            command: command_target,
            cwd,
            shell,
            profile: profile_label,
        };
        Ok(ToolPreparation::Prepared(PreparedToolCall::new(
            self.name(),
            permissions,
            false,
            Some(ToolReview::Command(review)),
            PreparedStart {
                sessions: self.sessions.clone(),
                spec,
            },
        )))
    }

    fn prepare_monitor(&self, input: Input) -> Result<ToolPreparation, ToolError> {
        let session_id = input.session_id.ok_or_else(|| {
            ToolError::InvalidArguments("terminal monitor requires session_id".into())
        })?;
        let operation = input.monitor.ok_or_else(|| {
            ToolError::InvalidArguments("terminal monitor requires monitor operation".into())
        })?;
        let Some((command, cwd)) = custom_probe(&operation) else {
            return Err(ToolError::InvalidArguments(
                "terminal monitor has no custom probe".into(),
            ));
        };
        let target = format!("@fx-terminal-monitor:{}:{cwd}:{command}", cwd.len());
        let permission = PermissionRequest::new(self.name(), command, ToolEffect::Process)
            .with_grant_target(target);
        Ok(ToolPreparation::Prepared(PreparedToolCall::new(
            self.name(),
            vec![permission],
            false,
            None,
            PreparedMonitor {
                sessions: self.sessions.clone(),
                session_id,
                operation,
            },
        )))
    }
}

struct PreparedStart {
    sessions: Arc<dyn TerminalSessionHost>,
    spec: StartSpec,
}

impl PreparedToolAction for PreparedStart {
    fn commit<'a>(
        self: Box<Self>,
        context: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        let cancellation = context.cancellation.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || self.sessions.start(self.spec, cancellation))
                .await
                .map_err(|error| ToolError::Execution(format!("terminal start failed: {error}")))?
        })
    }
}

struct PreparedMonitor {
    sessions: Arc<dyn TerminalSessionHost>,
    session_id: String,
    operation: MonitorOperation,
}

impl PreparedToolAction for PreparedMonitor {
    fn commit<'a>(
        self: Box<Self>,
        _context: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                self.sessions.monitor(&self.session_id, self.operation)
            })
            .await
            .map_err(|error| ToolError::Execution(format!("terminal monitor failed: {error}")))?
        })
    }
}

fn custom_probe(operation: &MonitorOperation) -> Option<(&str, &str)> {
    let definition = match operation {
        MonitorOperation::Add { definition } | MonitorOperation::Update { definition, .. } => {
            definition
        }
        MonitorOperation::Pause { .. }
        | MonitorOperation::Resume { .. }
        | MonitorOperation::Remove { .. } => return None,
    };
    definition_custom_probe(definition)
}

fn definition_custom_probe(definition: &MonitorDefinition) -> Option<(&str, &str)> {
    match &definition.condition {
        crate::monitor::MonitorCondition::CustomProbe { command, cwd } => Some((command, cwd)),
        _ => None,
    }
}

fn execute_session(
    sessions: Arc<dyn TerminalSessionHost>,
    input: Input,
    cancellation: Arc<dyn fx_core::CancellationSignal>,
) -> Result<ToolOutput, ToolError> {
    let session_id = || {
        input.session_id.as_deref().ok_or_else(|| {
            ToolError::InvalidArguments("terminal action requires session_id".into())
        })
    };
    match input.action {
        Action::Exec => Err(ToolError::PermissionDenied(
            "terminal exec requires canonical tool runtime authorization".into(),
        )),
        Action::Start => Err(ToolError::PermissionDenied(
            "terminal start requires canonical tool runtime authorization".into(),
        )),
        Action::Read => sessions.read(
            session_id()?,
            input.cursor_segment.ok_or_else(|| {
                ToolError::InvalidArguments("terminal read requires cursor_segment".into())
            })?,
            input.cursor_offset.unwrap_or(0),
        ),
        Action::Screen => sessions.screen(session_id()?),
        Action::Write => sessions.write(
            session_id()?,
            build_write_payload(input.write.ok_or_else(|| {
                ToolError::InvalidArguments("terminal write requires write payload".into())
            })?)?,
        ),
        Action::Wait => sessions.wait(
            session_id()?,
            build_wait_condition(input.return_when.ok_or_else(|| {
                ToolError::InvalidArguments("terminal wait requires return_when".into())
            })?)?,
            Duration::from_millis(input.wait_ceiling_ms.ok_or_else(|| {
                ToolError::InvalidArguments("terminal wait requires wait_ceiling_ms".into())
            })?),
            cancellation,
        ),
        Action::Monitor => sessions.monitor(
            session_id()?,
            input.monitor.ok_or_else(|| {
                ToolError::InvalidArguments("terminal monitor requires monitor operation".into())
            })?,
        ),
        Action::Inspect => sessions.inspect(
            session_id()?,
            input.after_event_id,
            input.acknowledge_event_id,
            usize::from(input.max_events.unwrap_or(256)),
        ),
        Action::List => sessions.list(input.backend.map(map_backend)),
        Action::Resize => sessions.resize(
            session_id()?,
            input.rows.ok_or_else(|| {
                ToolError::InvalidArguments("terminal resize requires rows".into())
            })?,
            input.columns.ok_or_else(|| {
                ToolError::InvalidArguments("terminal resize requires columns".into())
            })?,
        ),
        Action::Signal => sessions.signal(
            session_id()?,
            map_signal(input.signal.ok_or_else(|| {
                ToolError::InvalidArguments("terminal signal requires signal".into())
            })?),
        ),
        Action::Close => sessions.close(
            session_id()?,
            match input.close_policy.ok_or_else(|| {
                ToolError::InvalidArguments("terminal close requires close_policy".into())
            })? {
                ClosePolicyInput::Graceful => ClosePolicy::Graceful,
                ClosePolicyInput::Force => ClosePolicy::Force,
            },
        ),
    }
}

fn decode(arguments: &Value) -> Result<Input, ToolError> {
    let object = arguments.as_object().ok_or_else(|| {
        ToolError::InvalidArguments("terminal arguments must be an object".into())
    })?;
    let action: Action = serde_json::from_value(
        object
            .get("action")
            .cloned()
            .ok_or_else(|| ToolError::InvalidArguments("terminal action is required".into()))?,
    )
    .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;
    let allowed = allowed_fields(action);
    let invalid: Vec<_> = object
        .iter()
        .filter(|(_, value)| !value.is_null())
        .map(|(name, _)| name.as_str())
        .filter(|name| !allowed.contains(name))
        .collect();
    if !invalid.is_empty() {
        return Err(ToolError::InvalidArguments(format!(
            "terminal {} does not accept fields: {}",
            action.as_str(),
            invalid.join(", ")
        )));
    }
    let input: Input = serde_json::from_value(arguments.clone())
        .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;
    validate_input(&input)?;
    Ok(input)
}

fn validate_input(input: &Input) -> Result<(), ToolError> {
    if input.profile.is_some() && input.shell.is_some() {
        return Err(ToolError::InvalidArguments(
            "terminal start fields profile and shell are mutually exclusive".into(),
        ));
    }
    if let Some(command) = input.command.as_deref()
        && (command.len() > MAX_COMMAND_BYTES || command.contains('\0'))
    {
        return Err(ToolError::InvalidArguments(
            "terminal command must be NUL-free and at most 65536 bytes".into(),
        ));
    }
    if input.action == Action::Exec && input.command.as_deref().is_none_or(str::is_empty) {
        return Err(ToolError::InvalidArguments(
            "terminal exec requires a nonempty command".into(),
        ));
    }
    if let Some(session_id) = input.session_id.as_deref()
        && (session_id.is_empty() || session_id.len() > 128 || session_id.contains('\0'))
    {
        return Err(ToolError::InvalidArguments(
            "terminal session_id is invalid".into(),
        ));
    }
    if input.wait_ceiling_ms == Some(0) {
        return Err(ToolError::InvalidArguments(
            "terminal wait_ceiling_ms must be positive".into(),
        ));
    }
    if input.max_events == Some(0) || input.max_events.is_some_and(|value| value > 256) {
        return Err(ToolError::InvalidArguments(
            "terminal max_events must be between 1 and 256".into(),
        ));
    }
    if let Some(operation) = &input.monitor {
        operation
            .validate()
            .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;
    }
    if input.initial_monitors.len() > 32 {
        return Err(ToolError::InvalidArguments(
            "terminal start accepts at most 32 initial monitors".into(),
        ));
    }
    for definition in &input.initial_monitors {
        definition
            .validate()
            .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;
    }
    let _ = input.backend;
    let _ = input.after_event_id;
    let _ = input.acknowledge_event_id;
    Ok(())
}

fn allowed_fields(action: Action) -> &'static [&'static str] {
    match action {
        Action::Exec => &["action", "command", "cwd", "profile"],
        Action::Start => &[
            "action",
            "cwd",
            "command",
            "profile",
            "shell",
            "backend",
            "return_when",
            "wait_ceiling_ms",
            "dimensions",
            "initial_monitors",
        ],
        Action::Read => &["action", "session_id", "cursor_segment", "cursor_offset"],
        Action::Screen => &["action", "session_id"],
        Action::Write => &["action", "session_id", "write"],
        Action::Wait => &["action", "session_id", "return_when", "wait_ceiling_ms"],
        Action::Monitor => &["action", "session_id", "monitor"],
        Action::Inspect => &[
            "action",
            "session_id",
            "after_event_id",
            "acknowledge_event_id",
            "max_events",
        ],
        Action::List => &["action", "backend"],
        Action::Resize => &["action", "session_id", "rows", "columns"],
        Action::Signal => &["action", "session_id", "signal"],
        Action::Close => &["action", "session_id", "close_policy"],
    }
}

fn build_wait_condition(input: ReturnInput) -> Result<WaitCondition, ToolError> {
    match input.kind {
        ReturnKind::Started => Ok(WaitCondition::Started),
        ReturnKind::Exit => Ok(WaitCondition::Exit),
        ReturnKind::Quiet => input
            .duration_ms
            .filter(|duration| *duration > 0)
            .map(Duration::from_millis)
            .map(WaitCondition::Quiet)
            .ok_or_else(|| {
                ToolError::InvalidArguments("terminal quiet return requires duration_ms".into())
            }),
        ReturnKind::Match => input
            .pattern
            .filter(|pattern| !pattern.is_empty() && pattern.len() <= MAX_MATCH_BYTES)
            .map(WaitCondition::Match)
            .ok_or_else(|| {
                ToolError::InvalidArguments("terminal match return requires pattern".into())
            }),
    }
}

fn build_write_payload(input: WriteInput) -> Result<WritePayload, ToolError> {
    match input.kind {
        PayloadKind::Text | PayloadKind::Paste => {
            let text = input.text.filter(|text| !text.is_empty()).ok_or_else(|| {
                ToolError::InvalidArguments("terminal text/paste write requires text".into())
            })?;
            if text.len() > MAX_WRITE_BYTES {
                return Err(ToolError::InvalidArguments(
                    "terminal write is larger than 65536 bytes".into(),
                ));
            }
            Ok(if matches!(input.kind, PayloadKind::Text) {
                WritePayload::Text(text)
            } else {
                WritePayload::Paste(text)
            })
        }
        PayloadKind::Keys => {
            if input.keys.is_empty() || input.keys.len() > MAX_WRITE_ITEMS {
                return Err(ToolError::InvalidArguments(
                    "terminal keys write requires 1 to 4096 keys".into(),
                ));
            }
            Ok(WritePayload::Keys(
                input.keys.into_iter().map(map_key).collect(),
            ))
        }
        PayloadKind::Controls => {
            if input.controls.is_empty()
                || input.controls.len() > MAX_WRITE_ITEMS
                || input
                    .controls
                    .iter()
                    .any(|value| !matches!(value, b'?' | b'@'..=b'_' | b'a'..=b'z'))
            {
                return Err(ToolError::InvalidArguments(
                    "terminal controls require 1 to 4096 printable control designators".into(),
                ));
            }
            Ok(WritePayload::Controls(input.controls))
        }
    }
}

fn map_key(key: NamedKeyInput) -> NamedKey {
    match key {
        NamedKeyInput::Enter => NamedKey::Enter,
        NamedKeyInput::Tab => NamedKey::Tab,
        NamedKeyInput::Escape => NamedKey::Escape,
        NamedKeyInput::Backspace => NamedKey::Backspace,
        NamedKeyInput::Delete => NamedKey::Delete,
        NamedKeyInput::Insert => NamedKey::Insert,
        NamedKeyInput::ArrowUp => NamedKey::ArrowUp,
        NamedKeyInput::ArrowDown => NamedKey::ArrowDown,
        NamedKeyInput::ArrowLeft => NamedKey::ArrowLeft,
        NamedKeyInput::ArrowRight => NamedKey::ArrowRight,
        NamedKeyInput::Home => NamedKey::Home,
        NamedKeyInput::End => NamedKey::End,
        NamedKeyInput::PageUp => NamedKey::PageUp,
        NamedKeyInput::PageDown => NamedKey::PageDown,
    }
}

fn map_signal(signal: SignalInput) -> TerminalSignal {
    match signal {
        SignalInput::Hangup => TerminalSignal::Hangup,
        SignalInput::Interrupt => TerminalSignal::Interrupt,
        SignalInput::Quit => TerminalSignal::Quit,
        SignalInput::Terminate => TerminalSignal::Terminate,
        SignalInput::Kill => TerminalSignal::Kill,
    }
}

fn map_backend(backend: Backend) -> SessionBackend {
    match backend {
        Backend::Native => SessionBackend::Native,
        Backend::Tmux => SessionBackend::Tmux,
    }
}

fn start_arguments(
    shell: &Path,
    profile: Profile,
    command: Option<&str>,
) -> Result<Vec<String>, ToolError> {
    let mut arguments = match (shell.file_name().and_then(|name| name.to_str()), profile) {
        (Some("bash"), Profile::Clean) => {
            vec!["--noprofile".into(), "--norc".into(), "-i".into()]
        }
        (Some("bash"), Profile::User) => vec!["--login".into(), "-i".into()],
        (Some("zsh"), Profile::Clean) => vec!["-f".into(), "-i".into()],
        (Some("zsh"), Profile::User) => vec!["-l".into(), "-i".into()],
        _ => {
            return Err(ToolError::InvalidArguments(
                "terminal start supports only Bash and zsh".into(),
            ));
        }
    };
    if let Some(command) = command {
        arguments.push("-c".into());
        arguments.push(command.into());
    }
    Ok(arguments)
}

fn start_grant_target(command: &str, cwd: &Path, shell: &Path, profile: &str) -> String {
    let shell = shell.to_string_lossy();
    let cwd = cwd.to_string_lossy();
    format!(
        "@fx-terminal-env:{profile}:{}:{shell}:{}:{cwd}:{command}",
        shell.len(),
        cwd.len()
    )
}

fn monitor_operation_schema() -> Value {
    json!({
        "type": ["object", "null"],
        "properties": {
            "kind": {"type": "string", "enum": ["add", "update", "pause", "resume", "remove"]},
            "monitor_id": {"type": ["string", "null"], "maxLength": crate::monitor::MAXIMUM_MONITOR_ID_BYTES},
            "definition": monitor_definition_schema()
        },
        "required": ["kind"],
        "additionalProperties": false
    })
}

fn monitor_definition_schema() -> Value {
    json!({
        "type": ["object", "null"],
        "properties": {
            "condition": {
                "type": "object",
                "properties": {
                    "kind": {"type": "string", "enum": ["process_exit", "exit_code", "signal", "output_contains", "output_matches", "output_quiet", "screen_matches", "tcp_ready", "http_ready", "path_exists", "path_changed", "path_size", "custom_probe"]},
                    "pattern": {"type": ["string", "null"], "maxLength": crate::monitor::MAXIMUM_AUTHORITY_TEXT_BYTES},
                    "duration_ms": {"type": ["integer", "null"], "minimum": crate::monitor::MINIMUM_SCHEDULE_MS, "maximum": crate::monitor::MAXIMUM_SCHEDULE_MS},
                    "exit_code": {"type": ["integer", "null"]},
                    "signal": {"type": ["string", "null"], "enum": ["hangup", "interrupt", "quit", "terminate", "kill", null]},
                    "host": {"type": ["string", "null"], "maxLength": crate::monitor::MAXIMUM_AUTHORITY_TEXT_BYTES},
                    "port": {"type": ["integer", "null"], "minimum": 1, "maximum": 65535},
                    "path": {"type": ["string", "null"], "maxLength": crate::monitor::MAXIMUM_AUTHORITY_TEXT_BYTES},
                    "minimum_bytes": {"type": ["integer", "null"], "minimum": 0},
                    "command": {"type": ["string", "null"], "maxLength": crate::monitor::MAXIMUM_COMMAND_BYTES},
                    "cwd": {"type": ["string", "null"], "maxLength": crate::monitor::MAXIMUM_AUTHORITY_TEXT_BYTES}
                },
                "required": ["kind"],
                "additionalProperties": false
            },
            "check_interval_ms": {"type": ["integer", "null"], "minimum": crate::monitor::MINIMUM_SCHEDULE_MS, "maximum": crate::monitor::MAXIMUM_SCHEDULE_MS},
            "notify": {
                "type": "object",
                "properties": {
                    "kind": {"type": "string", "enum": ["on_match", "on_state_change", "on_exit", "every_check", "every_n_checks", "interval"]},
                    "count": {"type": ["integer", "null"], "minimum": 1, "maximum": 1000000},
                    "interval_ms": {"type": ["integer", "null"], "minimum": crate::monitor::MINIMUM_SCHEDULE_MS, "maximum": crate::monitor::MAXIMUM_SCHEDULE_MS}
                },
                "required": ["kind"],
                "additionalProperties": false
            },
            "lifetime": {
                "type": "object",
                "properties": {
                    "kind": {"type": "string", "enum": ["until_match", "until_session_end", "duration"]},
                    "duration_ms": {"type": ["integer", "null"], "minimum": 1, "maximum": crate::monitor::MAXIMUM_LIFETIME_MS}
                },
                "required": ["kind"],
                "additionalProperties": false
            }
        },
        "required": ["condition", "notify", "lifetime"],
        "additionalProperties": false
    })
}

fn terminal_schema(backends: &[&str]) -> Value {
    let mut backend_values: Vec<Value> = backends.iter().map(|backend| json!(backend)).collect();
    backend_values.push(Value::Null);
    json!({
        "type": "object",
        "properties": {
            "action": {"type": "string", "enum": ["exec", "start", "read", "screen", "write", "wait", "monitor", "inspect", "list", "resize", "signal", "close"]},
            "session_id": {"type": ["string", "null"]},
            "cwd": {"type": ["string", "null"]},
            "command": {"type": ["string", "null"], "maxLength": MAX_COMMAND_BYTES},
            "profile": {"type": ["string", "null"], "enum": ["clean", "user", null]},
            "shell": {
                "type": ["object", "null"],
                "properties": {
                    "kind": {"type": "string", "enum": ["user_login", "executable"]},
                    "path": {"type": ["string", "null"]},
                    "clean_start": {"type": "boolean"}
                },
                "additionalProperties": false
            },
            "backend": {"type": ["string", "null"], "enum": backend_values},
            "return_when": {
                "type": ["object", "null"],
                "properties": {
                    "kind": {"type": "string", "enum": ["started", "exit", "quiet", "match"]},
                    "duration_ms": {"type": ["integer", "null"], "minimum": 1},
                    "pattern": {"type": ["string", "null"], "maxLength": MAX_MATCH_BYTES}
                },
                "required": ["kind"],
                "additionalProperties": false
            },
            "wait_ceiling_ms": {"type": ["integer", "null"], "minimum": 1},
            "dimensions": {
                "type": ["object", "null"],
                "properties": {
                    "rows": {"type": "integer", "minimum": 1, "maximum": 4096},
                    "columns": {"type": "integer", "minimum": 1, "maximum": 4096}
                },
                "required": ["rows", "columns"],
                "additionalProperties": false
            },
            "initial_monitors": {"type": "array", "maxItems": 32, "items": monitor_definition_schema()},
            "cursor_segment": {"type": ["integer", "null"], "minimum": 1},
            "cursor_offset": {"type": ["integer", "null"], "minimum": 0},
            "after_event_id": {"type": ["integer", "null"], "minimum": 0},
            "acknowledge_event_id": {"type": ["integer", "null"], "minimum": 1},
            "max_events": {"type": ["integer", "null"], "minimum": 1, "maximum": 256},
            "write": {
                "type": ["object", "null"],
                "properties": {
                    "kind": {"type": "string", "enum": ["text", "keys", "controls", "paste"]},
                    "text": {"type": ["string", "null"], "maxLength": MAX_WRITE_BYTES},
                    "keys": {"type": "array", "maxItems": MAX_WRITE_ITEMS, "items": {"type": "string", "enum": ["enter", "tab", "escape", "backspace", "delete", "insert", "arrow_up", "arrow_down", "arrow_left", "arrow_right", "home", "end", "page_up", "page_down"]}},
                    "controls": {"type": "array", "maxItems": MAX_WRITE_ITEMS, "items": {"type": "integer", "minimum": 0, "maximum": 127}}
                },
                "required": ["kind"],
                "additionalProperties": false
            },
            "monitor": monitor_operation_schema(),
            "rows": {"type": ["integer", "null"], "minimum": 1, "maximum": 4096},
            "columns": {"type": ["integer", "null"], "minimum": 1, "maximum": 4096},
            "signal": {"type": ["string", "null"], "enum": ["hangup", "interrupt", "quit", "terminate", "kill", null]},
            "close_policy": {"type": ["string", "null"], "enum": ["graceful", "force", null]}
        },
        "required": ["action"],
        "additionalProperties": false
    })
}

#[cfg(all(test, unix))]
mod tests {
    use std::time::Instant;

    use fx_core::ToolPreparation;

    use super::*;

    fn shell() -> PathBuf {
        ["/bin/zsh", "/bin/bash"]
            .into_iter()
            .map(PathBuf::from)
            .find(|path| path.is_file())
            .expect("test host has Bash or zsh")
    }

    fn context() -> ToolContext {
        ToolContext::new(std::env::current_dir().unwrap())
    }

    async fn start(tool: &Terminal, arguments: Value) -> ToolOutput {
        let context = context();
        let ToolPreparation::Prepared(prepared) = tool.prepare(&context, &arguments).unwrap()
        else {
            panic!("terminal start must prepare owned execution")
        };
        prepared.commit(&context).await.unwrap()
    }

    async fn call(tool: &Terminal, arguments: Value) -> ToolOutput {
        tool.execute(&context(), arguments).await.unwrap()
    }

    fn session_id(output: &ToolOutput) -> String {
        output.structured.as_ref().unwrap()["success"]["start"]["session"]["session_id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    #[test]
    fn schema_advertises_only_the_implemented_terminal_actions() {
        let schema = Terminal::default().input_schema();
        let actions = schema["properties"]["action"]["enum"].as_array().unwrap();
        assert!(actions.contains(&json!("exec")));
        assert!(actions.contains(&json!("start")));
        assert!(actions.contains(&json!("screen")));
        assert!(actions.contains(&json!("close")));
        assert!(actions.contains(&json!("monitor")));
        assert_eq!(
            schema["properties"]["monitor"]["properties"]["definition"]["properties"]["condition"]
                ["properties"]["kind"]["enum"][7],
            "tcp_ready"
        );
        let backends = schema["properties"]["backend"]["enum"].as_array().unwrap();
        assert!(backends.contains(&json!("native")));
        assert!(backends.contains(&Value::Null));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn native_pty_session_supports_start_read_screen_write_wait_resize_and_close() {
        let tool = Terminal::with_local_login_shell(shell());
        let started = start(
            &tool,
            json!({
                "action": "start",
                "profile": "clean",
                "command": "printf ready; IFS= read -r line; printf '\\ngot:%s\\n' \"$line\"",
                "return_when": {"kind": "match", "pattern": "ready"},
                "wait_ceiling_ms": 2000,
                "dimensions": {"rows": 8, "columns": 40}
            }),
        )
        .await;
        assert!(!started.is_error, "{}", started.content);
        assert_eq!(
            started.structured.as_ref().unwrap()["success"]["start"]["outcome"],
            json!({"condition_met": {}})
        );
        let id = session_id(&started);

        let first = call(
            &tool,
            json!({
                "action": "read",
                "session_id": id,
                "cursor_segment": 1,
                "cursor_offset": 0
            }),
        )
        .await;
        assert!(first.content.contains("ready"), "{}", first.content);
        let cursor =
            first.structured.as_ref().unwrap()["success"]["read"]["raw_range"]["end"]["offset"]
                .as_u64()
                .unwrap();

        let screen = call(&tool, json!({"action": "screen", "session_id": id})).await;
        let snapshot = &screen.structured.as_ref().unwrap()["success"]["screen"]["snapshot"];
        assert_eq!(snapshot["dimensions"], json!({"rows": 8, "columns": 40}));
        assert!(
            snapshot["cells"]
                .as_array()
                .unwrap()
                .iter()
                .any(|cell| cell["text"] == "r")
        );

        let resized = call(
            &tool,
            json!({"action": "resize", "session_id": id, "rows": 10, "columns": 50}),
        )
        .await;
        assert_eq!(
            resized.structured.as_ref().unwrap()["success"]["resize"]["dimensions"],
            json!({"rows": 10, "columns": 50})
        );

        let written = call(
            &tool,
            json!({
                "action": "write",
                "session_id": id,
                "write": {"kind": "text", "text": "hello\n"}
            }),
        )
        .await;
        assert_eq!(
            written.structured.as_ref().unwrap()["success"]["write"]["accepted_bytes"],
            6
        );

        let before_wait = Instant::now();
        let waited = call(
            &tool,
            json!({
                "action": "wait",
                "session_id": id,
                "return_when": {"kind": "exit"},
                "wait_ceiling_ms": 2000
            }),
        )
        .await;
        assert!(before_wait.elapsed() < Duration::from_secs(3));
        assert!(
            waited.structured.as_ref().unwrap()["success"]["wait"]["outcome"]
                .get("exited")
                .is_some(),
            "{}",
            waited.content
        );

        let second = call(
            &tool,
            json!({
                "action": "read",
                "session_id": id,
                "cursor_segment": 1,
                "cursor_offset": cursor
            }),
        )
        .await;
        assert!(second.content.contains("got:hello"), "{}", second.content);

        let listed = call(&tool, json!({"action": "list"})).await;
        assert_eq!(
            listed.structured.as_ref().unwrap()["success"]["list"]["sessions"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let inspected = call(&tool, json!({"action": "inspect", "session_id": id})).await;
        assert_eq!(
            inspected.structured.as_ref().unwrap()["success"]["inspect"]["cwd"],
            context().workspace_root.display().to_string()
        );

        let closed = call(
            &tool,
            json!({"action": "close", "session_id": id, "close_policy": "force"}),
        )
        .await;
        assert_eq!(
            closed.structured.as_ref().unwrap()["success"]["close"]["session"]["lifecycle"],
            "closed"
        );
        let listed = call(&tool, json!({"action": "list"})).await;
        assert!(
            listed.structured.as_ref().unwrap()["success"]["list"]["sessions"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn action_field_ownership_rejects_cross_action_arguments() {
        let error = decode(&json!({
            "action": "read",
            "session_id": "terminal-a",
            "cursor_segment": 1,
            "command": "pwd"
        }))
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("does not accept fields: command")
        );
    }
}
