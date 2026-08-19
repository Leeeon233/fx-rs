//! Native process tools.
//!
//! Foreground execution is kept in a separate crate so process-group support,
//! Tokio process I/O, and shell startup policy are linked only by hosts that
//! advertise execution.

#[cfg(unix)]
mod hosted_terminal;
pub mod monitor;
mod native_terminal;
mod sandbox;
mod terminal;
mod terminal_host;
pub mod terminal_host_protocol;
#[cfg(unix)]
pub mod terminal_host_server;
#[cfg(unix)]
mod terminal_monitor_store;
#[cfg(unix)]
mod terminal_observation;
#[cfg(unix)]
mod tmux_terminal;

pub use terminal::Terminal;

use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::{Duration, Instant};

use fx_core::{
    BoxFuture, CommandReview, PermissionRequest, PreparedToolAction, PreparedToolCall,
    RegistryError, SandboxMode, Tool, ToolContext, ToolEffect, ToolError, ToolOutput,
    ToolPreparation, ToolRegistry, ToolReview,
};
use fx_workspace::resolve_existing;
#[cfg(windows)]
use process_wrap::tokio::JobObject;
#[cfg(unix)]
use process_wrap::tokio::ProcessGroup;
use process_wrap::tokio::{CommandWrap, KillOnDrop};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt};

const MAX_COMMAND_BYTES: usize = 64 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(10);

const DESCRIPTION: &str = "Run one foreground command and return a captured result. The omitted profile defaults to user; profile=clean skips Bash/zsh startup files. This initial Rust slice supports action=exec only; durable terminal sessions will add the remaining actions without changing the tool identity.";

#[derive(Clone, Debug, Default)]
pub struct TerminalExec {
    login_shell: Option<PathBuf>,
}

impl TerminalExec {
    pub fn with_login_shell(path: impl Into<PathBuf>) -> Self {
        Self {
            login_shell: Some(path.into()),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Action {
    Exec,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Profile {
    Clean,
    #[default]
    User,
}

impl Profile {
    fn as_str(self) -> &'static str {
        match self {
            Self::Clean => "clean",
            Self::User => "user",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    action: Action,
    command: String,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    profile: Profile,
}

impl Tool for TerminalExec {
    fn name(&self) -> &str {
        "terminal"
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["exec"] },
                "command": { "type": "string", "maxLength": MAX_COMMAND_BYTES },
                "cwd": { "type": "string" },
                "profile": { "type": "string", "enum": ["clean", "user"] }
            },
            "required": ["action", "command"],
            "additionalProperties": false
        })
    }

    fn effect(&self, arguments: &Value) -> Result<ToolEffect, ToolError> {
        let input = decode(arguments)?;
        match input.action {
            Action::Exec => Ok(ToolEffect::Process),
        }
    }

    fn project_context_targets(
        &self,
        context: &ToolContext,
        arguments: &Value,
    ) -> Result<Vec<PathBuf>, ToolError> {
        let input = decode(arguments)?;
        let cwd = resolve_existing(&context.workspace_root, input.cwd.as_deref().unwrap_or("."))?;
        Ok(vec![cwd.absolute])
    }

    fn prepare(
        &self,
        context: &ToolContext,
        arguments: &Value,
    ) -> Result<ToolPreparation, ToolError> {
        let input = decode(arguments)?;
        let prepared = prepare_command(context, input, self.login_shell.as_deref())?;
        let permission =
            PermissionRequest::new(self.name(), prepared.command.clone(), ToolEffect::Process)
                .with_grant_target(grant_target(&prepared));
        let review = CommandReview {
            command: prepared.command.clone(),
            cwd: prepared.cwd.clone(),
            shell: prepared.shell.clone(),
            profile: prepared.profile.as_str().into(),
        };
        Ok(ToolPreparation::Prepared(PreparedToolCall::new(
            self.name(),
            vec![permission],
            false,
            Some(ToolReview::Command(review)),
            prepared,
        )))
    }

    fn execute<'a>(
        &'a self,
        _context: &'a ToolContext,
        _arguments: Value,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async {
            Err(ToolError::PermissionDenied(
                "terminal execution requires canonical tool runtime authorization".into(),
            ))
        })
    }
}

pub fn register_process_tools(registry: &mut ToolRegistry) -> Result<(), RegistryError> {
    registry.register(Terminal::default())
}

fn decode(arguments: &Value) -> Result<Input, ToolError> {
    let input: Input = serde_json::from_value(arguments.clone())
        .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;
    if input.command.trim().is_empty()
        || input.command.len() > MAX_COMMAND_BYTES
        || input.command.contains('\0')
    {
        return Err(ToolError::InvalidArguments(
            "terminal exec command must be nonempty, NUL-free, and at most 65536 bytes".into(),
        ));
    }
    Ok(input)
}

fn prepare_command(
    context: &ToolContext,
    input: Input,
    shell_override: Option<&Path>,
) -> Result<PreparedCommand, ToolError> {
    let requested_cwd = input.cwd.as_deref().unwrap_or(".");
    let cwd = resolve_existing(&context.workspace_root, requested_cwd)?.absolute;
    if !cwd.is_dir() {
        return Err(ToolError::InvalidArguments(format!(
            "terminal cwd is not a directory: {}",
            cwd.display()
        )));
    }
    let shell = resolve_shell(shell_override)?;
    Ok(PreparedCommand {
        command: input.command,
        cwd,
        shell,
        profile: input.profile,
        max_output_bytes: context.limits.max_result_bytes,
        timeout: context.limits.command_timeout,
        sandbox: context.sandbox,
        sandbox_profile: sandbox::profile(context)?,
    })
}

fn resolve_shell(shell_override: Option<&Path>) -> Result<PathBuf, ToolError> {
    let candidate = shell_override
        .map(Path::to_owned)
        .or_else(|| {
            std::env::var_os("SHELL")
                .filter(|value| !value.is_empty())
                .map(Into::into)
        })
        .or_else(default_shell)
        .ok_or_else(|| ToolError::Execution("no Bash or zsh login shell is available".into()))?;
    if !candidate.is_absolute() {
        return Err(ToolError::InvalidArguments(
            "the configured login shell must be absolute".into(),
        ));
    }
    let shell = candidate
        .canonicalize()
        .map_err(|error| ToolError::Execution(format!("{}: {error}", candidate.display())))?;
    match shell.file_name().and_then(|name| name.to_str()) {
        Some("bash" | "zsh") => Ok(shell),
        _ => Err(ToolError::InvalidArguments(format!(
            "unsupported login shell {}; only Bash and zsh are supported",
            shell.display()
        ))),
    }
}

fn default_shell() -> Option<PathBuf> {
    ["/bin/zsh", "/bin/bash"]
        .into_iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
}

fn grant_target(command: &PreparedCommand) -> String {
    let shell = command.shell.to_string_lossy();
    let cwd = command.cwd.to_string_lossy();
    format!(
        "@fx-terminal-env:{}:{}:{}:{}:{}:{}:{}",
        command.profile.as_str(),
        match command.sandbox {
            SandboxMode::None => "none",
            SandboxMode::Os => "os",
        },
        shell.len(),
        shell,
        cwd.len(),
        cwd,
        command.command
    )
}

struct PreparedCommand {
    command: String,
    cwd: PathBuf,
    shell: PathBuf,
    profile: Profile,
    max_output_bytes: usize,
    timeout: Option<Duration>,
    sandbox: SandboxMode,
    sandbox_profile: Option<String>,
}

impl PreparedToolAction for PreparedCommand {
    fn commit<'a>(
        self: Box<Self>,
        context: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async move { run_command(*self, context).await })
    }
}

enum Termination {
    Natural(ExitStatus),
    Cancelled,
    TimedOut,
}

async fn run_command(
    prepared: PreparedCommand,
    context: &ToolContext,
) -> Result<ToolOutput, ToolError> {
    if context.cancellation.is_cancelled() {
        return Err(ToolError::Cancelled);
    }
    let args = shell_arguments(prepared.profile, &prepared.shell, &prepared.command)?;
    let (program, arguments) = match prepared.sandbox_profile.as_deref() {
        Some(profile) => {
            let mut wrapped = vec!["-p".to_owned(), profile.to_owned()];
            wrapped.push(prepared.shell.display().to_string());
            wrapped.extend(args);
            (Path::new(sandbox::MACOS_SANDBOX_EXEC), wrapped)
        }
        None => (prepared.shell.as_path(), args),
    };
    let mut command = tokio::process::Command::new(program);
    command
        .args(arguments)
        .current_dir(&prepared.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut wrapped = CommandWrap::from(command);
    wrapped.wrap(KillOnDrop);
    #[cfg(unix)]
    wrapped.wrap(ProcessGroup::leader());
    #[cfg(windows)]
    wrapped.wrap(JobObject);
    let mut child = wrapped
        .spawn()
        .map_err(|error| ToolError::Execution(format!("could not start command: {error}")))?;
    let stdout = child
        .stdout()
        .take()
        .ok_or_else(|| ToolError::Execution("command stdout pipe is unavailable".into()))?;
    let stderr = child
        .stderr()
        .take()
        .ok_or_else(|| ToolError::Execution("command stderr pipe is unavailable".into()))?;
    let capture_limit = prepared.max_output_bytes;
    let stdout_task = tokio::spawn(capture_stream(stdout, capture_limit));
    let stderr_task = tokio::spawn(capture_stream(stderr, capture_limit));
    let started = Instant::now();

    let termination = loop {
        tokio::select! {
            status = child.wait() => {
                break Termination::Natural(status.map_err(|error| {
                    ToolError::Execution(format!("could not wait for command: {error}"))
                })?);
            }
            () = tokio::time::sleep(POLL_INTERVAL) => {
                let cause = if context.cancellation.is_cancelled() {
                    Some(Termination::Cancelled)
                } else if prepared.timeout.is_some_and(|timeout| started.elapsed() >= timeout) {
                    Some(Termination::TimedOut)
                } else {
                    None
                };
                if let Some(cause) = cause {
                    Box::into_pin(child.kill()).await.map_err(|error| {
                        ToolError::Execution(format!("could not terminate command: {error}"))
                    })?;
                    break cause;
                }
            }
        }
    };

    let stdout = stdout_task
        .await
        .map_err(|error| ToolError::Execution(format!("stdout reader failed: {error}")))??;
    let stderr = stderr_task
        .await
        .map_err(|error| ToolError::Execution(format!("stderr reader failed: {error}")))??;
    Ok(format_result(
        &prepared,
        termination,
        stdout,
        stderr,
        started.elapsed(),
    ))
}

fn shell_arguments(
    profile: Profile,
    shell: &Path,
    command: &str,
) -> Result<Vec<String>, ToolError> {
    let name = shell.file_name().and_then(|name| name.to_str());
    match (name, profile) {
        (Some("bash"), Profile::Clean) => Ok(vec![
            "--noprofile".into(),
            "--norc".into(),
            "-c".into(),
            command.into(),
        ]),
        (Some("bash"), Profile::User) => Ok(vec![
            "--login".into(),
            "-O".into(),
            "expand_aliases".into(),
            "-c".into(),
            command.into(),
        ]),
        (Some("zsh"), Profile::Clean) => Ok(vec!["-f".into(), "-c".into(), command.into()]),
        (Some("zsh"), Profile::User) => {
            Ok(vec!["-l".into(), "-i".into(), "-c".into(), command.into()])
        }
        _ => Err(ToolError::InvalidArguments(
            "only Bash and zsh are supported".into(),
        )),
    }
}

#[derive(Debug)]
struct CapturedStream {
    head: Vec<u8>,
    tail: Vec<u8>,
    total_bytes: usize,
    max_bytes: usize,
}

impl CapturedStream {
    fn new(max_bytes: usize) -> Self {
        Self {
            head: Vec::new(),
            tail: Vec::new(),
            total_bytes: 0,
            max_bytes,
        }
    }

    fn append(&mut self, bytes: &[u8]) {
        self.total_bytes = self.total_bytes.saturating_add(bytes.len());
        let head_limit = self.max_bytes.div_ceil(2);
        let tail_limit = self.max_bytes.saturating_sub(head_limit);
        if self.head.len() < head_limit {
            let take = (head_limit - self.head.len()).min(bytes.len());
            self.head.extend_from_slice(&bytes[..take]);
        }
        if tail_limit == 0 {
            return;
        }
        if bytes.len() >= tail_limit {
            self.tail.clear();
            self.tail
                .extend_from_slice(&bytes[bytes.len() - tail_limit..]);
        } else {
            let overflow = self
                .tail
                .len()
                .saturating_add(bytes.len())
                .saturating_sub(tail_limit);
            if overflow > 0 {
                self.tail.drain(..overflow);
            }
            self.tail.extend_from_slice(bytes);
        }
    }

    fn render(&self, budget: usize) -> String {
        if self.total_bytes == 0 {
            return String::new();
        }
        if self.total_bytes <= budget {
            let overlap = self
                .head
                .len()
                .saturating_add(self.tail.len())
                .saturating_sub(self.total_bytes);
            let mut bytes = self.head.clone();
            bytes.extend_from_slice(&self.tail[overlap..]);
            return String::from_utf8_lossy(&bytes).into_owned();
        }
        let head_budget = budget.div_ceil(2);
        let tail_budget = budget.saturating_sub(head_budget);
        let head = &self.head[..self.head.len().min(head_budget)];
        let tail = &self.tail[self.tail.len().saturating_sub(tail_budget)..];
        format!(
            "{}\n[... {} bytes truncated ...]\n{}",
            String::from_utf8_lossy(head),
            self.total_bytes.saturating_sub(budget),
            String::from_utf8_lossy(tail)
        )
    }
}

async fn capture_stream(
    mut stream: impl AsyncRead + Unpin,
    max_bytes: usize,
) -> Result<CapturedStream, ToolError> {
    let mut captured = CapturedStream::new(max_bytes);
    let mut chunk = [0u8; 8192];
    loop {
        let count = stream.read(&mut chunk).await.map_err(|error| {
            ToolError::Execution(format!("could not read command output: {error}"))
        })?;
        if count == 0 {
            return Ok(captured);
        }
        captured.append(&chunk[..count]);
    }
}

fn format_result(
    prepared: &PreparedCommand,
    termination: Termination,
    stdout: CapturedStream,
    stderr: CapturedStream,
    duration: Duration,
) -> ToolOutput {
    #[cfg(unix)]
    use std::os::unix::process::ExitStatusExt as _;

    let (exit_code, signal, timed_out, cancelled) = match termination {
        Termination::Natural(status) => (
            status.code().map(i64::from),
            {
                #[cfg(unix)]
                {
                    status.signal().and_then(|value| u32::try_from(value).ok())
                }
                #[cfg(not(unix))]
                {
                    None
                }
            },
            false,
            false,
        ),
        Termination::Cancelled => (None, None, false, true),
        Termination::TimedOut => (None, None, true, false),
    };
    let stdout_bytes = stdout.total_bytes;
    let stderr_bytes = stderr.total_bytes;
    let total_bytes = stdout_bytes.saturating_add(stderr_bytes);
    let truncated = total_bytes > prepared.max_output_bytes;
    let (stdout_budget, stderr_budget) = if !truncated {
        (prepared.max_output_bytes, prepared.max_output_bytes)
    } else if stderr_bytes == 0 {
        (prepared.max_output_bytes, 0)
    } else if stdout_bytes == 0 {
        (0, prepared.max_output_bytes)
    } else {
        let stdout_budget = prepared.max_output_bytes.div_ceil(2);
        (
            stdout_budget,
            prepared.max_output_bytes.saturating_sub(stdout_budget),
        )
    };
    let stdout_text = stdout.render(stdout_budget);
    let stderr_text = stderr.render(stderr_budget);
    let sandbox_denied = prepared.sandbox_profile.is_some() && sandbox::denied(&stderr_text);
    let duration_ms = u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
    let structured = json!({
        "kind": "foreground",
        "command": prepared.command,
        "cwd": prepared.cwd,
        "exit_code": exit_code,
        "signal": signal,
        "timed_out": timed_out,
        "duration_ms": duration_ms,
        "stdout_bytes": stdout_bytes,
        "stderr_bytes": stderr_bytes,
        "truncated": truncated,
        "output_file": null,
        "stdout_file": null,
        "stderr_file": null,
        "sandbox": match prepared.sandbox { SandboxMode::None => "none", SandboxMode::Os => "os" },
        "sandbox_denied": sandbox_denied
    });
    let failed =
        cancelled || timed_out || signal.is_some() || exit_code.is_some_and(|code| code != 0);
    let content = if cancelled {
        format!(
            "command cancelled\n{}",
            output_envelopes(&stdout_text, &stderr_text)
        )
    } else if timed_out {
        let timeout = prepared.timeout.map_or_else(
            || "timeout=true\n".to_owned(),
            |value| format!("timeout=true\ntimeout_ms={}\n", value.as_millis()),
        );
        format!(
            "{timeout}command timed out and was terminated\n{}",
            output_envelopes(&stdout_text, &stderr_text)
        )
    } else if failed {
        serde_json::to_string(&json!({
            "error": {
                "tool_name": "terminal",
                "message": if exit_code.is_some() {
                    "Command exited with non-zero status"
                } else {
                    "Command terminated before completing successfully"
                },
                "details": {
                    "command": prepared.command,
                    "cwd": prepared.cwd,
                    "exit_code": exit_code,
                    "signal": signal,
                    "stderr": stderr_text.trim()
                },
                "suggestion": "Inspect stderr and the command context, then fix the command or explain the blocker rather than retrying unchanged."
            }
        }))
        .expect("static command error JSON is serializable")
    } else {
        let status = exit_code.map_or_else(
            || "process finished\n".to_owned(),
            |code| format!("exit_code={code}\n"),
        );
        format!("{status}{}", output_envelopes(&stdout_text, &stderr_text))
    };
    let original_bytes = if truncated {
        content.len().max(total_bytes)
    } else {
        content.len()
    };
    ToolOutput {
        original_bytes,
        content,
        is_error: failed,
        structured: Some(structured),
        truncated,
        durable_content: None,
    }
}

fn output_envelopes(stdout: &str, stderr: &str) -> String {
    let stdout = stdout.trim_matches([' ', '\r', '\n', '\t']);
    let stderr = stderr.trim_matches([' ', '\r', '\n', '\t']);
    let mut output = String::new();
    if !stdout.is_empty() {
        output.push_str("<stdout>\n");
        output.push_str(stdout);
        output.push_str("\n</stdout>\n");
    }
    if !stderr.is_empty() {
        output.push_str("<stderr>\n");
        output.push_str(stderr);
        output.push_str("\n</stderr>\n");
    }
    if output.is_empty() {
        output.push_str("(no output)\n");
    }
    output
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use fx_core::{CancellationSignal, ToolPreparation};

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

    async fn approved_call(
        tool: &TerminalExec,
        context: &ToolContext,
        arguments: Value,
    ) -> ToolOutput {
        let ToolPreparation::Prepared(prepared) = tool.prepare(context, &arguments).unwrap() else {
            panic!("terminal must prepare an owned command")
        };
        prepared.commit(context).await.unwrap()
    }

    #[tokio::test(flavor = "current_thread")]
    async fn executes_with_zig_compatible_envelopes_and_metadata() {
        let tool = TerminalExec::with_login_shell(shell());
        let output = approved_call(
            &tool,
            &context(),
            json!({
                "action": "exec",
                "profile": "clean",
                "command": "printf 'hello'; printf 'warn' >&2"
            }),
        )
        .await;
        assert!(!output.is_error, "{}", output.content);
        assert!(output.content.contains("exit_code=0"));
        assert!(output.content.contains("<stdout>\nhello\n</stdout>"));
        assert!(output.content.contains("<stderr>\nwarn\n</stderr>"));
        let metadata = output.structured.unwrap();
        assert_eq!(metadata["exit_code"], 0);
        assert_eq!(metadata["stdout_bytes"], 5);
        assert_eq!(metadata["stderr_bytes"], 4);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn nonzero_exit_uses_the_zig_failure_contract_and_retains_metadata() {
        let tool = TerminalExec::with_login_shell(shell());
        let output = approved_call(
            &tool,
            &context(),
            json!({
                "action": "exec",
                "profile": "clean",
                "command": "printf out; printf warn >&2; exit 7"
            }),
        )
        .await;
        assert!(output.is_error);
        let failure: Value = serde_json::from_str(&output.content).unwrap();
        assert_eq!(
            failure["error"]["message"],
            "Command exited with non-zero status"
        );
        assert_eq!(failure["error"]["details"]["exit_code"], 7);
        assert_eq!(failure["error"]["details"]["stderr"], "warn");
        let metadata = output.structured.unwrap();
        assert_eq!(metadata["exit_code"], 7);
        assert_eq!(metadata["stdout_bytes"], 3);
        assert_eq!(metadata["stderr_bytes"], 4);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn large_output_is_bounded_with_head_tail_metadata() {
        let tool = TerminalExec::with_login_shell(shell());
        let mut context = context();
        context.limits.max_result_bytes = 8;
        let output = approved_call(
            &tool,
            &context,
            json!({
                "action": "exec",
                "profile": "clean",
                "command": "printf 0123456789abcdef"
            }),
        )
        .await;
        assert!(!output.is_error);
        assert!(output.truncated);
        assert!(output.content.contains("bytes truncated"));
        assert!(output.content.contains("0123"));
        assert!(output.content.contains("cdef"));
        assert!(output.original_bytes >= 16);
        let metadata = output.structured.unwrap();
        assert_eq!(metadata["stdout_bytes"], 16);
        assert_eq!(metadata["truncated"], true);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn timeout_terminates_the_process_group_and_preserves_output() {
        let tool = TerminalExec::with_login_shell(shell());
        let mut context = context();
        context.limits.command_timeout = Some(Duration::from_millis(80));
        let started = Instant::now();
        let output = approved_call(
            &tool,
            &context,
            json!({
                "action": "exec",
                "profile": "clean",
                "command": "printf before; sleep 5 & wait"
            }),
        )
        .await;
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(output.is_error);
        assert!(output.content.contains("timeout=true"));
        assert!(output.content.contains("before"));
        assert_eq!(output.structured.unwrap()["timed_out"], true);
    }

    struct AtomicCancellation(Arc<AtomicBool>);

    impl CancellationSignal for AtomicCancellation {
        fn is_cancelled(&self) -> bool {
            self.0.load(Ordering::Acquire)
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancellation_terminates_an_in_flight_command() {
        let tool = TerminalExec::with_login_shell(shell());
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut context = context();
        context.cancellation = Arc::new(AtomicCancellation(cancelled.clone()));
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            cancelled.store(true, Ordering::Release);
        });
        let output = approved_call(
            &tool,
            &context,
            json!({
                "action": "exec",
                "profile": "clean",
                "command": "printf started; sleep 5"
            }),
        )
        .await;
        assert!(output.is_error);
        assert!(output.content.contains("command cancelled"));
        assert!(output.content.contains("started"));
    }

    #[cfg(target_os = "macos")]
    #[tokio::test(flavor = "current_thread")]
    async fn os_sandbox_allows_workspace_writes_and_denies_outside_writes() {
        let root = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!("fx-sandbox-test-{}", std::process::id()));
        let workspace = root.join("workspace");
        let outside = root.join("outside.txt");
        fs::create_dir_all(&workspace).unwrap();
        let mut context = ToolContext::new(workspace.clone());
        context.sandbox = SandboxMode::Os;
        let tool = TerminalExec::with_login_shell(shell());

        let allowed = approved_call(
            &tool,
            &context,
            json!({
                "action": "exec",
                "profile": "clean",
                "command": "printf allowed > inside.txt"
            }),
        )
        .await;
        assert!(!allowed.is_error, "{}", allowed.content);
        assert_eq!(
            fs::read_to_string(workspace.join("inside.txt")).unwrap(),
            "allowed"
        );
        assert_eq!(allowed.structured.as_ref().unwrap()["sandbox"], "os");

        let denied = approved_call(
            &tool,
            &context,
            json!({
                "action": "exec",
                "profile": "clean",
                "command": format!("printf blocked > '{}'", outside.display())
            }),
        )
        .await;
        assert!(denied.is_error, "outside write unexpectedly succeeded");
        assert_eq!(denied.structured.as_ref().unwrap()["sandbox_denied"], true);
        assert!(!outside.exists());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn session_grant_binds_shell_cwd_and_profile_but_rules_match_command() {
        let tool = TerminalExec::with_login_shell(shell());
        let context = context();
        let ToolPreparation::Prepared(prepared) = tool
            .prepare(
                &context,
                &json!({"action": "exec", "command": "pwd", "profile": "clean"}),
            )
            .unwrap()
        else {
            panic!("terminal must prepare")
        };
        let permission = &prepared.permission_requests[0];
        assert_eq!(permission.target, "pwd");
        assert!(
            permission
                .grant_target
                .starts_with("@fx-terminal-env:clean:")
        );
        assert_ne!(permission.target, permission.grant_target);
        assert!(matches!(prepared.review, Some(ToolReview::Command(_))));
    }
}
