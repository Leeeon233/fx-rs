#![cfg(unix)]

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use atomic_write_file::AtomicWriteFile;
use fx_core::{CancellationSignal, ToolError, ToolOutput};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::monitor::{MonitorOperation, MonitorSignal};
use crate::native_terminal::{
    ClosePolicy, NamedKey, SessionBackend, StartSpec, TerminalSessionHost, TerminalSignal,
    WaitCondition, WritePayload, cursor, failure_output, raw_range, render_snapshot,
    success_output, valid_dimensions,
};
use crate::terminal_observation::{ObservedLifecycle, TerminalMonitorSource, TerminalObservation};

const METADATA_SCHEMA_VERSION: u32 = 2;
const METADATA_FILE: &str = "session.json";
const OUTPUT_FILE: &str = "output.log";
const COMMAND_FILE: &str = "command.sh";
const MAX_METADATA_BYTES: usize = 64 * 1024;
const MAX_READ_BYTES: u64 = 1024 * 1024;
const MAX_MATCH_SOURCE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_LIST_RESULTS: usize = 256;
const WAIT_POLL: Duration = Duration::from_millis(20);

#[derive(Clone, Debug)]
pub(crate) struct TmuxTerminalHost {
    executable: PathBuf,
    root: PathBuf,
    socket_name: String,
}

impl TmuxTerminalHost {
    pub(crate) fn discover() -> Option<Self> {
        let executable = std::env::var_os("FX_TMUX_EXE")
            .map(PathBuf::from)
            .or_else(|| find_executable("tmux"))?;
        let executable = executable.canonicalize().ok()?;
        let root = std::env::var_os("FX_TERMINAL_STATE_DIR")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".fx/terminal-rs"))
            })?;
        if !root.is_absolute() {
            return None;
        }
        let socket_name = std::env::var("FX_TMUX_SOCKET").unwrap_or_else(|_| "fx-rs".into());
        if !valid_socket_name(&socket_name) {
            return None;
        }
        Some(Self {
            executable,
            root,
            socket_name,
        })
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(executable: PathBuf, root: PathBuf, socket_name: String) -> Self {
        Self {
            executable,
            root,
            socket_name,
        }
    }

    fn start_tmux(
        &self,
        spec: StartSpec,
        cancellation: Arc<dyn CancellationSignal>,
    ) -> Result<ToolOutput, ToolError> {
        if spec.backend != SessionBackend::Tmux {
            return Ok(failure_output("start", None, "unsupported_host", false));
        }
        if cancellation.is_cancelled() {
            return Ok(failure_output("start", None, "cancelled", false));
        }
        if !valid_dimensions(spec.rows, spec.columns) {
            return Ok(failure_output("start", None, "invalid_request", false));
        }
        self.ensure_root()?;
        let suffix = Uuid::new_v4().simple().to_string();
        let session_id = format!("terminal-t-{suffix}");
        let tmux_name = format!("fxrs_{suffix}");
        let directory = self.session_directory(&session_id)?;
        fs::create_dir(&directory)
            .map_err(|error| ToolError::Execution(format!("create terminal state: {error}")))?;
        set_private_directory(&directory)?;
        let log_path = directory.join(OUTPUT_FILE);
        create_private_file(&log_path)?;

        let command_path = directory.join(COMMAND_FILE);
        if let Some(command) = spec.command.as_deref() {
            write_command_file(&command_path, command)?;
        }

        let shell_arguments = interactive_shell_arguments(&spec)?;
        let mut invocation = if let Some(profile) = spec.sandbox_profile.as_deref() {
            vec![
                crate::sandbox::MACOS_SANDBOX_EXEC.to_owned(),
                "-p".to_owned(),
                profile.to_owned(),
                spec.shell.display().to_string(),
            ]
        } else {
            vec![spec.shell.display().to_string()]
        };
        invocation.extend(shell_arguments);
        let shell_command = shell_words(&invocation);
        let output = self.run(
            [
                "new-session".into(),
                "-d".into(),
                "-s".into(),
                tmux_name.clone(),
                "-x".into(),
                spec.columns.to_string(),
                "-y".into(),
                spec.rows.to_string(),
                "-c".into(),
                spec.cwd.display().to_string(),
                shell_command,
            ],
            None,
        )?;
        if !output.status.success() {
            let _ = fs::remove_dir_all(&directory);
            return Ok(failure_output("start", None, "startup_failed", false));
        }

        let metadata = TmuxMetadata {
            schema_version: METADATA_SCHEMA_VERSION,
            session_id: session_id.clone(),
            tmux_name,
            workspace_root: spec.workspace_root,
            cwd: spec.cwd,
            command: spec.command,
            shell: spec.shell,
            rows: spec.rows,
            columns: spec.columns,
            reader_offset: 0,
        };
        let configured = self.configure_session(&metadata, &log_path, &command_path);
        if let Err(error) = configured {
            let _ = self.kill_tmux_session(&metadata);
            let _ = fs::remove_dir_all(&directory);
            return Err(error);
        }
        self.write_metadata(&metadata)?;

        let outcome = match spec.return_when {
            None | Some(WaitCondition::Started) => ReturnOutcome::Started,
            Some(condition) => self.wait_outcome(
                &metadata,
                &condition,
                spec.wait_ceiling.unwrap_or(Duration::ZERO),
                cancellation.as_ref(),
            )?,
        };
        let facts = self.facts(&metadata)?;
        Ok(success_output(
            "start",
            json!({"session": facts, "outcome": outcome.to_json()}),
        ))
    }

    fn configure_session(
        &self,
        metadata: &TmuxMetadata,
        log_path: &Path,
        command_path: &Path,
    ) -> Result<(), ToolError> {
        self.require_success(self.run(
            [
                "set-option".into(),
                "-t".into(),
                metadata.tmux_name.clone(),
                "remain-on-exit".into(),
                "on".into(),
            ],
            None,
        )?)?;
        let pipe = format!("cat >> {}", shell_word(&log_path.display().to_string()));
        self.require_success(self.run(
            [
                "pipe-pane".into(),
                "-t".into(),
                metadata.target(),
                "-o".into(),
                pipe,
            ],
            None,
        )?)?;
        if metadata.command.is_some() {
            let source = format!(". {}", shell_word(&command_path.display().to_string()));
            self.send_literal(metadata, &source)?;
            self.send_key(metadata, "Enter")?;
        }
        Ok(())
    }

    fn read_tmux(
        &self,
        session_id: &str,
        segment: u64,
        offset: u64,
    ) -> Result<ToolOutput, ToolError> {
        let Some(mut metadata) = self.load_metadata_optional(session_id)? else {
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
        let log_path = self.session_directory(session_id)?.join(OUTPUT_FILE);
        let mut file = File::open(&log_path)
            .map_err(|error| ToolError::Execution(format!("open terminal log: {error}")))?;
        let total = file
            .metadata()
            .map_err(|error| ToolError::Execution(format!("stat terminal log: {error}")))?
            .len();
        if offset > total {
            return Ok(failure_output(
                "read",
                Some(session_id),
                "cursor_gap",
                false,
            ));
        }
        file.seek(SeekFrom::Start(offset))
            .map_err(|error| ToolError::Execution(format!("seek terminal log: {error}")))?;
        let mut bytes = Vec::new();
        file.take(MAX_READ_BYTES)
            .read_to_end(&mut bytes)
            .map_err(|error| ToolError::Execution(format!("read terminal log: {error}")))?;
        let end = offset.saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        metadata.reader_offset = end;
        self.write_metadata(&metadata)?;
        let facts = self.facts(&metadata)?;
        Ok(success_output(
            "read",
            json!({
                "session": facts,
                "output": String::from_utf8_lossy(&bytes),
                "raw_range": if offset == end { Value::Null } else { raw_range(offset, end) }
            }),
        ))
    }

    fn screen_tmux(&self, session_id: &str) -> Result<ToolOutput, ToolError> {
        let Some(metadata) = self.load_metadata_optional(session_id)? else {
            return Ok(failure_output(
                "screen",
                Some(session_id),
                "session_not_found",
                false,
            ));
        };
        let pane = self.pane_facts(&metadata)?;
        if pane.lifecycle == Lifecycle::Lost {
            return Ok(failure_output(
                "screen",
                Some(session_id),
                "screen_unavailable",
                false,
            ));
        }
        let capture = self.run(
            [
                "capture-pane".into(),
                "-p".into(),
                "-e".into(),
                "-J".into(),
                "-t".into(),
                metadata.target(),
            ],
            None,
        )?;
        if !capture.status.success() {
            return Ok(failure_output(
                "screen",
                Some(session_id),
                "screen_unavailable",
                false,
            ));
        }
        let mut parser = vt100::Parser::new(pane.rows, pane.columns, 0);
        parser.process(&capture.stdout);
        let facts = self.facts_with_pane(&metadata, &pane)?;
        Ok(success_output(
            "screen",
            json!({
                "session": facts,
                "snapshot": render_snapshot(
                    &parser,
                    pane.rows,
                    pane.columns,
                    Some((pane.cursor_row, pane.cursor_column)),
                )
            }),
        ))
    }

    fn write_tmux(&self, session_id: &str, payload: WritePayload) -> Result<ToolOutput, ToolError> {
        let Some(metadata) = self.load_metadata_optional(session_id)? else {
            return Ok(failure_output(
                "write",
                Some(session_id),
                "session_not_found",
                false,
            ));
        };
        if self.pane_facts(&metadata)?.lifecycle != Lifecycle::Running {
            return Ok(failure_output(
                "write",
                Some(session_id),
                "invalid_lifecycle",
                false,
            ));
        }
        let accepted = match payload {
            WritePayload::Text(text) => {
                let count = text.len();
                self.send_literal(&metadata, &text)?;
                count
            }
            WritePayload::Paste(text) => {
                let count = text.len();
                self.require_success(
                    self.run(["load-buffer".into(), "-".into()], Some(text.as_bytes()))?,
                )?;
                self.require_success(self.run(
                    [
                        "paste-buffer".into(),
                        "-d".into(),
                        "-p".into(),
                        "-t".into(),
                        metadata.target(),
                    ],
                    None,
                )?)?;
                count
            }
            WritePayload::Keys(keys) => {
                for key in &keys {
                    self.send_key(&metadata, tmux_key(*key))?;
                }
                keys.len()
            }
            WritePayload::Controls(controls) => {
                for control in &controls {
                    let key = format!("C-{}", char::from(control.to_ascii_lowercase()));
                    self.send_key(&metadata, &key)?;
                }
                controls.len()
            }
        };
        Ok(success_output(
            "write",
            json!({"session": self.facts(&metadata)?, "accepted_bytes": accepted}),
        ))
    }

    fn wait_tmux(
        &self,
        session_id: &str,
        condition: WaitCondition,
        ceiling: Duration,
        cancellation: Arc<dyn CancellationSignal>,
    ) -> Result<ToolOutput, ToolError> {
        let Some(metadata) = self.load_metadata_optional(session_id)? else {
            return Ok(failure_output(
                "wait",
                Some(session_id),
                "session_not_found",
                false,
            ));
        };
        let outcome = self.wait_outcome(&metadata, &condition, ceiling, cancellation.as_ref())?;
        Ok(success_output(
            "wait",
            json!({"session": self.facts(&metadata)?, "outcome": outcome.to_json()}),
        ))
    }

    fn inspect_tmux(&self, session_id: &str) -> Result<ToolOutput, ToolError> {
        let Some(metadata) = self.load_metadata_optional(session_id)? else {
            return Ok(failure_output(
                "inspect",
                Some(session_id),
                "session_not_found",
                false,
            ));
        };
        Ok(success_output(
            "inspect",
            json!({
                "session": self.facts(&metadata)?,
                "shell": metadata.shell,
                "cwd": metadata.cwd,
                "command": metadata.command,
                "monitors": [],
                "events": [],
                "event_gap_through": 0,
                "next_event_id": 1
            }),
        ))
    }

    fn list_tmux(&self) -> Result<ToolOutput, ToolError> {
        let mut sessions = Vec::new();
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(success_output("list", json!({"sessions": []})));
            }
            Err(error) => {
                return Err(ToolError::Execution(format!(
                    "list terminal state: {error}"
                )));
            }
        };
        for entry in entries.take(MAX_LIST_RESULTS) {
            let entry = entry.map_err(|error| {
                ToolError::Execution(format!("read terminal state entry: {error}"))
            })?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if !name.starts_with("terminal-t-") {
                continue;
            }
            if let Some(metadata) = self.load_metadata_optional(&name)? {
                sessions.push(self.facts(&metadata)?);
            }
        }
        Ok(success_output("list", json!({"sessions": sessions})))
    }

    fn resize_tmux(
        &self,
        session_id: &str,
        rows: u16,
        columns: u16,
    ) -> Result<ToolOutput, ToolError> {
        let Some(mut metadata) = self.load_metadata_optional(session_id)? else {
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
        self.require_success(self.run(
            [
                "resize-window".into(),
                "-t".into(),
                metadata.tmux_name.clone(),
                "-x".into(),
                columns.to_string(),
                "-y".into(),
                rows.to_string(),
            ],
            None,
        )?)?;
        metadata.rows = rows;
        metadata.columns = columns;
        self.write_metadata(&metadata)?;
        Ok(success_output(
            "resize",
            json!({
                "session": self.facts(&metadata)?,
                "dimensions": {"rows": rows, "columns": columns}
            }),
        ))
    }

    fn signal_tmux(
        &self,
        session_id: &str,
        signal: TerminalSignal,
    ) -> Result<ToolOutput, ToolError> {
        let Some(metadata) = self.load_metadata_optional(session_id)? else {
            return Ok(failure_output(
                "signal",
                Some(session_id),
                "session_not_found",
                false,
            ));
        };
        if self.pane_facts(&metadata)?.lifecycle != Lifecycle::Running {
            return Ok(failure_output(
                "signal",
                Some(session_id),
                "invalid_lifecycle",
                false,
            ));
        }
        match signal {
            TerminalSignal::Interrupt => self.send_key(&metadata, "C-c")?,
            TerminalSignal::Quit => self.send_key(&metadata, "C-\\")?,
            TerminalSignal::Hangup => self.send_key(&metadata, "C-d")?,
            TerminalSignal::Terminate | TerminalSignal::Kill => {
                self.require_success(
                    self.run(["kill-pane".into(), "-t".into(), metadata.target()], None)?,
                )?;
            }
        }
        Ok(success_output(
            "signal",
            json!({"session": self.facts(&metadata)?, "signal": signal.as_str()}),
        ))
    }

    fn close_tmux(&self, session_id: &str, policy: ClosePolicy) -> Result<ToolOutput, ToolError> {
        let Some(metadata) = self.load_metadata_optional(session_id)? else {
            return Ok(failure_output(
                "close",
                Some(session_id),
                "session_not_found",
                false,
            ));
        };
        let _ = self.kill_tmux_session(&metadata);
        let facts = self.facts_for(&metadata, Lifecycle::Closed, 0)?;
        let directory = self.session_directory(session_id)?;
        fs::remove_dir_all(&directory)
            .map_err(|error| ToolError::Execution(format!("remove terminal state: {error}")))?;
        Ok(success_output(
            "close",
            json!({"session": facts, "policy": policy.as_str()}),
        ))
    }

    fn wait_outcome(
        &self,
        metadata: &TmuxMetadata,
        condition: &WaitCondition,
        ceiling: Duration,
        cancellation: &dyn CancellationSignal,
    ) -> Result<ReturnOutcome, ToolError> {
        let started = Instant::now();
        let mut last_size = self.log_len(metadata)?;
        let mut last_change = Instant::now();
        loop {
            if cancellation.is_cancelled() {
                return Ok(ReturnOutcome::Cancelled);
            }
            let pane = self.pane_facts(metadata)?;
            if pane.lifecycle != Lifecycle::Running {
                return Ok(match pane.lifecycle {
                    Lifecycle::Exited => ReturnOutcome::Exited(pane.exit_code.unwrap_or(1)),
                    Lifecycle::Lost => ReturnOutcome::Signal("session_lost".into()),
                    Lifecycle::Running | Lifecycle::Closed => unreachable!(),
                });
            }
            let size = self.log_len(metadata)?;
            if size != last_size {
                last_size = size;
                last_change = Instant::now();
            }
            let matched = match condition {
                WaitCondition::Started => true,
                WaitCondition::Exit => false,
                WaitCondition::Quiet(duration) => last_change.elapsed() >= *duration,
                WaitCondition::Match(pattern) => self.log_contains(metadata, pattern.as_bytes())?,
            };
            if matched {
                return Ok(match condition {
                    WaitCondition::Started => ReturnOutcome::Started,
                    _ => ReturnOutcome::ConditionMet,
                });
            }
            if started.elapsed() >= ceiling {
                return Ok(ReturnOutcome::SafetyCeiling);
            }
            std::thread::sleep(WAIT_POLL.min(ceiling.saturating_sub(started.elapsed())));
        }
    }

    fn facts(&self, metadata: &TmuxMetadata) -> Result<Value, ToolError> {
        let pane = self.pane_facts(metadata)?;
        self.facts_with_pane(metadata, &pane)
    }

    fn facts_with_pane(
        &self,
        metadata: &TmuxMetadata,
        pane: &PaneFacts,
    ) -> Result<Value, ToolError> {
        self.facts_for(metadata, pane.lifecycle, self.log_len(metadata)?)
    }

    fn facts_for(
        &self,
        metadata: &TmuxMetadata,
        lifecycle: Lifecycle,
        total: u64,
    ) -> Result<Value, ToolError> {
        let running = lifecycle == Lifecycle::Running;
        let observable = lifecycle != Lifecycle::Closed;
        let unread_range = if metadata.reader_offset < total {
            raw_range(metadata.reader_offset, total)
        } else {
            Value::Null
        };
        Ok(json!({
            "session_id": metadata.session_id,
            "lifecycle": lifecycle.as_str(),
            "attention": {"attention": "background", "write_lease": "none"},
            "backend": "tmux",
            "persistence": "durable",
            "output_cursor": cursor(total),
            "unread_range": unread_range,
            "raw_gap": null,
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
        }))
    }

    fn pane_facts(&self, metadata: &TmuxMetadata) -> Result<PaneFacts, ToolError> {
        let output = self.run(
            [
                "display-message".into(),
                "-p".into(),
                "-t".into(),
                metadata.target(),
                "#{pane_dead}\t#{pane_dead_status}\t#{cursor_y}\t#{cursor_x}\t#{pane_height}\t#{pane_width}".into(),
            ],
            None,
        )?;
        if !output.status.success() {
            return Ok(PaneFacts::lost(metadata));
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let fields: Vec<_> = text.trim().split('\t').collect();
        if fields.len() != 6 {
            return Ok(PaneFacts::lost(metadata));
        }
        let dead = fields[0] == "1";
        Ok(PaneFacts {
            lifecycle: if dead {
                Lifecycle::Exited
            } else {
                Lifecycle::Running
            },
            exit_code: dead.then(|| fields[1].parse().unwrap_or(1)),
            cursor_row: fields[2].parse().unwrap_or(0),
            cursor_column: fields[3].parse().unwrap_or(0),
            rows: fields[4].parse().unwrap_or(metadata.rows),
            columns: fields[5].parse().unwrap_or(metadata.columns),
        })
    }

    fn send_literal(&self, metadata: &TmuxMetadata, text: &str) -> Result<(), ToolError> {
        self.require_success(self.run(
            [
                "send-keys".into(),
                "-t".into(),
                metadata.target(),
                "-l".into(),
                "--".into(),
                text.into(),
            ],
            None,
        )?)
    }

    fn send_key(&self, metadata: &TmuxMetadata, key: &str) -> Result<(), ToolError> {
        self.require_success(self.run(
            [
                "send-keys".into(),
                "-t".into(),
                metadata.target(),
                key.into(),
            ],
            None,
        )?)
    }

    fn log_len(&self, metadata: &TmuxMetadata) -> Result<u64, ToolError> {
        let path = self
            .session_directory(&metadata.session_id)?
            .join(OUTPUT_FILE);
        fs::metadata(path)
            .map(|metadata| metadata.len())
            .map_err(|error| ToolError::Execution(format!("stat terminal log: {error}")))
    }

    fn log_contains(&self, metadata: &TmuxMetadata, pattern: &[u8]) -> Result<bool, ToolError> {
        if pattern.is_empty() {
            return Ok(false);
        }
        let path = self
            .session_directory(&metadata.session_id)?
            .join(OUTPUT_FILE);
        let mut file = File::open(path)
            .map_err(|error| ToolError::Execution(format!("open terminal log: {error}")))?;
        let length = file
            .metadata()
            .map_err(|error| ToolError::Execution(format!("stat terminal log: {error}")))?
            .len();
        let start = length.saturating_sub(MAX_MATCH_SOURCE_BYTES);
        file.seek(SeekFrom::Start(start))
            .map_err(|error| ToolError::Execution(format!("seek terminal log: {error}")))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|error| ToolError::Execution(format!("read terminal log: {error}")))?;
        Ok(bytes.windows(pattern.len()).any(|window| window == pattern))
    }

    fn kill_tmux_session(&self, metadata: &TmuxMetadata) -> Result<(), ToolError> {
        let output = self.run(
            [
                "kill-session".into(),
                "-t".into(),
                metadata.tmux_name.clone(),
            ],
            None,
        )?;
        if output.status.success() || self.pane_facts(metadata)?.lifecycle == Lifecycle::Lost {
            Ok(())
        } else {
            Err(ToolError::Execution("could not close tmux session".into()))
        }
    }

    fn load_metadata_optional(&self, session_id: &str) -> Result<Option<TmuxMetadata>, ToolError> {
        let path = self.session_directory(session_id)?.join(METADATA_FILE);
        let mut file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(ToolError::Execution(format!(
                    "open terminal metadata: {error}"
                )));
            }
        };
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take((MAX_METADATA_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|error| ToolError::Execution(format!("read terminal metadata: {error}")))?;
        if bytes.is_empty() || bytes.len() > MAX_METADATA_BYTES {
            return Err(ToolError::Execution(
                "terminal metadata has an invalid size".into(),
            ));
        }
        let metadata: TmuxMetadata = serde_json::from_slice(&bytes)
            .map_err(|error| ToolError::Execution(format!("decode terminal metadata: {error}")))?;
        metadata.validate(session_id)?;
        Ok(Some(metadata))
    }

    fn write_metadata(&self, metadata: &TmuxMetadata) -> Result<(), ToolError> {
        metadata.validate(&metadata.session_id)?;
        let path = self
            .session_directory(&metadata.session_id)?
            .join(METADATA_FILE);
        let bytes = serde_json::to_vec(metadata)
            .map_err(|error| ToolError::Execution(format!("encode terminal metadata: {error}")))?;
        if bytes.is_empty() || bytes.len() > MAX_METADATA_BYTES {
            return Err(ToolError::Execution(
                "terminal metadata exceeds its size limit".into(),
            ));
        }
        let mut stage = AtomicWriteFile::open(&path)
            .map_err(|error| ToolError::Execution(format!("stage terminal metadata: {error}")))?;
        set_private_file(stage.as_file())?;
        stage
            .write_all(&bytes)
            .and_then(|()| stage.sync_all())
            .map_err(|error| ToolError::Execution(format!("write terminal metadata: {error}")))?;
        stage
            .commit()
            .map_err(|error| ToolError::Execution(format!("commit terminal metadata: {error}")))
    }

    fn session_directory(&self, session_id: &str) -> Result<PathBuf, ToolError> {
        if !valid_tmux_session_id(session_id) {
            return Err(ToolError::InvalidArguments(
                "terminal session_id is invalid".into(),
            ));
        }
        Ok(self.root.join(session_id))
    }

    fn ensure_root(&self) -> Result<(), ToolError> {
        fs::create_dir_all(&self.root)
            .map_err(|error| ToolError::Execution(format!("create terminal root: {error}")))?;
        set_private_directory(&self.root)
    }

    fn run<I>(&self, arguments: I, stdin: Option<&[u8]>) -> Result<Output, ToolError>
    where
        I: IntoIterator<Item = String>,
    {
        let mut command = Command::new(&self.executable);
        command.arg("-L").arg(&self.socket_name).args(arguments);
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        if stdin.is_some() {
            command.stdin(Stdio::piped());
        } else {
            command.stdin(Stdio::null());
        }
        let mut child = command
            .spawn()
            .map_err(|error| ToolError::Execution(format!("start tmux: {error}")))?;
        if let Some(bytes) = stdin {
            child
                .stdin
                .take()
                .ok_or_else(|| ToolError::Execution("tmux stdin unavailable".into()))?
                .write_all(bytes)
                .map_err(|error| ToolError::Execution(format!("write tmux stdin: {error}")))?;
        }
        child
            .wait_with_output()
            .map_err(|error| ToolError::Execution(format!("wait for tmux: {error}")))
    }

    fn require_success(&self, output: Output) -> Result<(), ToolError> {
        if output.status.success() {
            Ok(())
        } else {
            let message = String::from_utf8_lossy(&output.stderr);
            Err(ToolError::Execution(format!(
                "tmux command failed: {}",
                message.trim()
            )))
        }
    }
}

impl TerminalSessionHost for TmuxTerminalHost {
    fn backends(&self) -> &'static [&'static str] {
        &["tmux"]
    }

    fn start(
        &self,
        spec: StartSpec,
        cancellation: Arc<dyn CancellationSignal>,
    ) -> Result<ToolOutput, ToolError> {
        self.start_tmux(spec, cancellation)
    }

    fn read(&self, session_id: &str, segment: u64, offset: u64) -> Result<ToolOutput, ToolError> {
        self.read_tmux(session_id, segment, offset)
    }

    fn screen(&self, session_id: &str) -> Result<ToolOutput, ToolError> {
        self.screen_tmux(session_id)
    }

    fn write(&self, session_id: &str, payload: WritePayload) -> Result<ToolOutput, ToolError> {
        self.write_tmux(session_id, payload)
    }

    fn wait(
        &self,
        session_id: &str,
        condition: WaitCondition,
        ceiling: Duration,
        cancellation: Arc<dyn CancellationSignal>,
    ) -> Result<ToolOutput, ToolError> {
        self.wait_tmux(session_id, condition, ceiling, cancellation)
    }

    fn inspect(
        &self,
        session_id: &str,
        _after_event_id: Option<u64>,
        _acknowledge_event_id: Option<u64>,
        _max_events: usize,
    ) -> Result<ToolOutput, ToolError> {
        self.inspect_tmux(session_id)
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
        if backend == Some(SessionBackend::Native) {
            return Ok(success_output("list", json!({"sessions": []})));
        }
        self.list_tmux()
    }

    fn resize(&self, session_id: &str, rows: u16, columns: u16) -> Result<ToolOutput, ToolError> {
        self.resize_tmux(session_id, rows, columns)
    }

    fn signal(&self, session_id: &str, signal: TerminalSignal) -> Result<ToolOutput, ToolError> {
        self.signal_tmux(session_id, signal)
    }

    fn close(&self, session_id: &str, policy: ClosePolicy) -> Result<ToolOutput, ToolError> {
        self.close_tmux(session_id, policy)
    }
}

impl TerminalMonitorSource for TmuxTerminalHost {
    fn observe_terminal(
        &self,
        session_id: &str,
        after_offset: u64,
        include_screen: bool,
    ) -> Result<Option<TerminalObservation>, ToolError> {
        let Some(metadata) = self.load_metadata_optional(session_id)? else {
            return Ok(None);
        };
        let pane = self.pane_facts(&metadata)?;
        let log_path = self.session_directory(session_id)?.join(OUTPUT_FILE);
        let mut file = File::open(&log_path)
            .map_err(|error| ToolError::Execution(format!("open terminal log: {error}")))?;
        let total = file
            .metadata()
            .map_err(|error| ToolError::Execution(format!("stat terminal log: {error}")))?
            .len();
        let raw_gap = after_offset > total;
        let cursor_start = if raw_gap { 0 } else { after_offset };
        file.seek(SeekFrom::Start(cursor_start))
            .map_err(|error| ToolError::Execution(format!("seek terminal log: {error}")))?;
        let mut output = Vec::new();
        file.take(MAX_READ_BYTES)
            .read_to_end(&mut output)
            .map_err(|error| ToolError::Execution(format!("read terminal log: {error}")))?;
        let cursor_end =
            cursor_start.saturating_add(u64::try_from(output.len()).unwrap_or(u64::MAX));
        let screen_text = if include_screen && pane.lifecycle != Lifecycle::Lost {
            let capture = self.run(
                [
                    "capture-pane".into(),
                    "-p".into(),
                    "-e".into(),
                    "-J".into(),
                    "-t".into(),
                    metadata.target(),
                ],
                None,
            )?;
            if capture.status.success() {
                let mut parser = vt100::Parser::new(pane.rows, pane.columns, 0);
                parser.process(&capture.stdout);
                Some(parser.screen().contents())
            } else {
                None
            }
        } else {
            None
        };
        let lifecycle = match pane.lifecycle {
            Lifecycle::Running => ObservedLifecycle::Running,
            Lifecycle::Exited => ObservedLifecycle::Exited,
            Lifecycle::Lost => ObservedLifecycle::Lost,
            Lifecycle::Closed => ObservedLifecycle::Closed,
        };
        let exit_code = pane.exit_code.and_then(|code| i32::try_from(code).ok());
        Ok(Some(TerminalObservation {
            lifecycle,
            exit_code,
            signal: pane.exit_code.and_then(tmux_monitor_signal),
            cursor_start,
            cursor_end,
            raw_gap,
            output,
            screen_text,
            workspace_root: metadata.workspace_root.clone(),
            cwd: metadata.cwd.clone(),
        }))
    }
}

fn tmux_monitor_signal(status: u32) -> Option<MonitorSignal> {
    match status.checked_sub(128)? {
        1 => Some(MonitorSignal::Hangup),
        2 => Some(MonitorSignal::Interrupt),
        3 => Some(MonitorSignal::Quit),
        9 => Some(MonitorSignal::Kill),
        15 => Some(MonitorSignal::Terminate),
        _ => None,
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct TmuxMetadata {
    schema_version: u32,
    session_id: String,
    tmux_name: String,
    workspace_root: PathBuf,
    cwd: PathBuf,
    command: Option<String>,
    shell: PathBuf,
    rows: u16,
    columns: u16,
    reader_offset: u64,
}

impl TmuxMetadata {
    fn target(&self) -> String {
        format!("{}:0.0", self.tmux_name)
    }

    fn validate(&self, expected_id: &str) -> Result<(), ToolError> {
        if self.schema_version != METADATA_SCHEMA_VERSION
            || self.session_id != expected_id
            || !valid_tmux_session_id(&self.session_id)
            || !self.tmux_name.starts_with("fxrs_")
            || !self.workspace_root.is_absolute()
            || !self.workspace_root.is_dir()
            || !self.cwd.is_absolute()
            || !self.cwd.starts_with(&self.workspace_root)
            || !self.shell.is_absolute()
            || !valid_dimensions(self.rows, self.columns)
        {
            return Err(ToolError::Execution(
                "terminal metadata failed validation".into(),
            ));
        }
        Ok(())
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

struct PaneFacts {
    lifecycle: Lifecycle,
    exit_code: Option<u32>,
    cursor_row: u16,
    cursor_column: u16,
    rows: u16,
    columns: u16,
}

impl PaneFacts {
    fn lost(metadata: &TmuxMetadata) -> Self {
        Self {
            lifecycle: Lifecycle::Lost,
            exit_code: None,
            cursor_row: 0,
            cursor_column: 0,
            rows: metadata.rows,
            columns: metadata.columns,
        }
    }
}

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
            Self::Signal(signal) => json!({"signal": signal}),
        }
    }
}

fn interactive_shell_arguments(spec: &StartSpec) -> Result<Vec<String>, ToolError> {
    let mut arguments = spec.arguments.clone();
    if spec.command.is_some() {
        if arguments.len() < 2 || arguments[arguments.len() - 2] != "-c" {
            return Err(ToolError::Execution(
                "terminal start invocation is inconsistent".into(),
            ));
        }
        arguments.truncate(arguments.len() - 2);
    }
    Ok(arguments)
}

fn write_command_file(path: &Path, command: &str) -> Result<(), ToolError> {
    let mut stage = AtomicWriteFile::open(path)
        .map_err(|error| ToolError::Execution(format!("stage terminal command: {error}")))?;
    set_private_file(stage.as_file())?;
    stage
        .write_all(command.as_bytes())
        .and_then(|()| stage.write_all(b"\nfx_terminal_status=$?\nexit \"$fx_terminal_status\"\n"))
        .and_then(|()| stage.sync_all())
        .map_err(|error| ToolError::Execution(format!("write terminal command: {error}")))?;
    stage
        .commit()
        .map_err(|error| ToolError::Execution(format!("commit terminal command: {error}")))
}

fn create_private_file(path: &Path) -> Result<(), ToolError> {
    let file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .map_err(|error| ToolError::Execution(format!("create terminal file: {error}")))?;
    set_private_file(&file)
}

fn set_private_file(file: &File) -> Result<(), ToolError> {
    use std::os::unix::fs::PermissionsExt as _;
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|error| ToolError::Execution(format!("secure terminal file: {error}")))
}

fn set_private_directory(path: &Path) -> Result<(), ToolError> {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|error| ToolError::Execution(format!("secure terminal directory: {error}")))
}

fn valid_tmux_session_id(value: &str) -> bool {
    value.strip_prefix("terminal-t-").is_some_and(|suffix| {
        suffix.len() == 32 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
    })
}

fn valid_socket_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn find_executable(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .map(|directory| directory.join(name))
        .find(|path| path.is_file())
}

fn shell_words(words: &[String]) -> String {
    words
        .iter()
        .map(|word| shell_word(word))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_word(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

fn tmux_key(key: NamedKey) -> &'static str {
    match key {
        NamedKey::Enter => "Enter",
        NamedKey::Tab => "Tab",
        NamedKey::Escape => "Escape",
        NamedKey::Backspace => "BSpace",
        NamedKey::Delete => "DC",
        NamedKey::Insert => "IC",
        NamedKey::ArrowUp => "Up",
        NamedKey::ArrowDown => "Down",
        NamedKey::ArrowLeft => "Left",
        NamedKey::ArrowRight => "Right",
        NamedKey::Home => "Home",
        NamedKey::End => "End",
        NamedKey::PageUp => "PPage",
        NamedKey::PageDown => "NPage",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fx_core::NeverCancelled;

    fn shell() -> Option<(PathBuf, Vec<String>)> {
        if Path::new("/bin/bash").is_file() {
            Some((
                PathBuf::from("/bin/bash"),
                vec!["--noprofile".into(), "--norc".into(), "-i".into()],
            ))
        } else if Path::new("/bin/zsh").is_file() {
            Some((PathBuf::from("/bin/zsh"), vec!["-f".into(), "-i".into()]))
        } else {
            None
        }
    }

    #[test]
    fn tmux_session_recovers_after_host_reconstruction() {
        let Some(executable) = find_executable("tmux") else {
            return;
        };
        let Some((shell, mut arguments)) = shell() else {
            return;
        };
        let suffix = Uuid::new_v4().simple().to_string();
        let root = std::env::temp_dir().join(format!("fx-rs-tmux-test-{suffix}"));
        let socket = format!("fx-rs-test-{suffix}");
        let command = "printf tmux_ready; sleep 0.2; printf tmux_done".to_owned();
        arguments.extend(["-c".into(), command.clone()]);
        let spec = StartSpec {
            backend: SessionBackend::Tmux,
            workspace_root: std::env::current_dir().unwrap(),
            cwd: std::env::current_dir().unwrap(),
            initial_monitors: Vec::new(),
            command: Some(command),
            shell,
            arguments,
            sandbox_profile: None,
            rows: 8,
            columns: 40,
            return_when: Some(WaitCondition::Match("tmux_ready".into())),
            wait_ceiling: Some(Duration::from_secs(3)),
        };

        let first =
            TmuxTerminalHost::new_for_test(executable.clone(), root.clone(), socket.clone());
        let started = first
            .start(spec, Arc::new(NeverCancelled))
            .expect("tmux start succeeds");
        assert!(!started.is_error, "{}", started.content);
        let session_id =
            started.structured.as_ref().unwrap()["success"]["start"]["session"]["session_id"]
                .as_str()
                .unwrap()
                .to_owned();
        assert_eq!(
            started.structured.as_ref().unwrap()["success"]["start"]["session"]["persistence"],
            "durable"
        );
        drop(first);

        let recovered = TmuxTerminalHost::new_for_test(executable, root.clone(), socket);
        let listed = recovered.list(None).expect("persistent metadata lists");
        assert_eq!(
            listed.structured.as_ref().unwrap()["success"]["list"]["sessions"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let read = recovered
            .read(&session_id, 1, 0)
            .expect("persistent output reads");
        assert!(read.content.contains("tmux_ready"), "{}", read.content);
        let waited = recovered
            .wait(
                &session_id,
                WaitCondition::Exit,
                Duration::from_secs(3),
                Arc::new(NeverCancelled),
            )
            .expect("recovered session waits");
        assert!(
            waited.structured.as_ref().unwrap()["success"]["wait"]["outcome"]
                .get("exited")
                .is_some(),
            "{}",
            waited.content
        );
        let closed = recovered
            .close(&session_id, ClosePolicy::Force)
            .expect("recovered session closes");
        assert!(!closed.is_error, "{}", closed.content);
        fs::remove_dir(&root).expect("test state root is empty after close");
    }
}
