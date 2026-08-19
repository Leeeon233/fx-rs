#![cfg(unix)]

use std::fs::{self, File};
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs as _};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, UNIX_EPOCH};

use atomic_write_file::AtomicWriteFile;
use fx_core::ToolError;
use fx_workspace::{resolve_existing, resolve_target};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tokio::io::AsyncReadExt as _;

use crate::monitor::{
    Decision, EventReason, Monitor, MonitorCondition, MonitorOperation, MonitorState,
    NotifySchedule, Observation, PathBaseline, pattern_matches, stable_id,
};
use crate::terminal_observation::{ObservedLifecycle, TerminalObservation};

const SCHEMA_VERSION: u16 = 1;
const MAXIMUM_MONITORS: usize = 32;
const MAXIMUM_EVENTS: usize = 1_024;
const MAXIMUM_FILE_BYTES: usize = 2 * 1_024 * 1_024;
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const PROBE_OUTPUT_BYTES: u64 = 16 * 1_024;
const NETWORK_CONNECT_TIMEOUT: Duration = Duration::from_millis(250);
const HTTP_IO_TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct MonitorEvent {
    pub event_id: u64,
    pub monitor_id: String,
    pub reason: EventReason,
    pub lifecycle: ObservedLifecycle,
    pub cursor_segment: u64,
    pub cursor_offset: u64,
    pub created_at_ms: i64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct MonitorSummary {
    pub monitor_id: String,
    pub state: MonitorState,
}

#[derive(Clone, Debug)]
pub(crate) struct InspectMonitors {
    pub monitors: Vec<MonitorSummary>,
    pub events: Vec<MonitorEvent>,
    pub event_gap_through: u64,
    pub next_event_id: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct PersistedMonitorSet {
    schema_version: u16,
    pub session_id: String,
    next_monitor_id: u64,
    pub next_event_id: u64,
    pub event_gap_through: u64,
    acknowledged_through: u64,
    pub cursor_offset: u64,
    pub last_output_at_ms: i64,
    pub last_lifecycle: ObservedLifecycle,
    pub monitors: Vec<Monitor>,
    pub events: Vec<MonitorEvent>,
}

impl PersistedMonitorSet {
    fn empty(
        session_id: &str,
        cursor_offset: u64,
        lifecycle: ObservedLifecycle,
        now_ms: i64,
    ) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            session_id: session_id.into(),
            next_monitor_id: 1,
            next_event_id: 1,
            event_gap_through: 0,
            acknowledged_through: 0,
            cursor_offset,
            last_output_at_ms: now_ms,
            last_lifecycle: lifecycle,
            monitors: Vec::new(),
            events: Vec::new(),
        }
    }

    fn validate(&self, expected_session_id: &str) -> Result<(), ToolError> {
        if self.schema_version != SCHEMA_VERSION
            || self.session_id != expected_session_id
            || !valid_session_id(&self.session_id)
            || self.next_monitor_id == 0
            || self.next_event_id == 0
            || self.last_output_at_ms < 0
            || self.acknowledged_through >= self.next_event_id
            || self.event_gap_through >= self.next_event_id
            || self.monitors.len() > MAXIMUM_MONITORS
            || self.events.len() > MAXIMUM_EVENTS
        {
            return Err(corrupt("monitor set metadata is invalid"));
        }
        for monitor in &self.monitors {
            monitor
                .validate()
                .map_err(|error| corrupt(&error.to_string()))?;
        }
        let mut previous = self.event_gap_through.max(self.acknowledged_through);
        for event in &self.events {
            if event.event_id <= previous
                || event.event_id >= self.next_event_id
                || event.monitor_id.is_empty()
                || event.cursor_segment == 0
                || event.created_at_ms < 0
            {
                return Err(corrupt("monitor event sequence is invalid"));
            }
            previous = event.event_id;
        }
        Ok(())
    }

    pub(crate) fn active_count(&self) -> usize {
        self.monitors
            .iter()
            .filter(|monitor| {
                !matches!(
                    monitor.runtime.state,
                    MonitorState::Paused | MonitorState::Degraded
                )
            })
            .count()
    }

    pub(crate) fn push_event(
        &mut self,
        monitor_index: usize,
        reason: EventReason,
        lifecycle: ObservedLifecycle,
        cursor_offset: u64,
        now_ms: i64,
    ) -> Result<u64, ToolError> {
        let event_id = self.next_event_id;
        self.next_event_id = self
            .next_event_id
            .checked_add(1)
            .ok_or_else(|| corrupt("monitor event id exhausted"))?;
        let monitor = self
            .monitors
            .get_mut(monitor_index)
            .ok_or_else(|| corrupt("monitor index is invalid"))?;
        monitor
            .note_notification(event_id, reason)
            .map_err(|error| corrupt(&error.to_string()))?;
        self.events.push(MonitorEvent {
            event_id,
            monitor_id: monitor.monitor_id.clone(),
            reason,
            lifecycle,
            cursor_segment: 1,
            cursor_offset,
            created_at_ms: now_ms,
        });
        while self.events.len() > MAXIMUM_EVENTS {
            let removed = self.events.remove(0);
            self.event_gap_through = self.event_gap_through.max(removed.event_id);
        }
        Ok(event_id)
    }

    fn summaries(&self) -> Vec<MonitorSummary> {
        self.monitors
            .iter()
            .map(|monitor| MonitorSummary {
                monitor_id: monitor.monitor_id.clone(),
                state: monitor.runtime.state,
            })
            .collect()
    }
}

#[derive(Clone, Debug)]
pub(crate) struct MonitorStore {
    root: PathBuf,
}

#[derive(Clone, Copy)]
pub(crate) struct MonitorContext<'a> {
    pub current_cursor: u64,
    pub lifecycle: ObservedLifecycle,
    pub workspace_root: &'a Path,
    pub cwd: &'a Path,
    pub now_ms: i64,
}

impl MonitorStore {
    pub(crate) fn new(state_directory: &Path) -> Result<Self, ToolError> {
        let root = state_directory.join("monitors");
        prepare_private_directory(&root)?;
        Ok(Self { root })
    }

    pub(crate) fn validate_initial(
        &self,
        definitions: &[crate::monitor::MonitorDefinition],
        workspace_root: &Path,
        cwd: &Path,
        now_ms: i64,
    ) -> Result<(), ToolError> {
        build_initial_monitors(definitions, workspace_root, cwd, now_ms).map(|_| ())
    }

    pub(crate) fn install_initial(
        &self,
        session_id: &str,
        definitions: &[crate::monitor::MonitorDefinition],
        context: MonitorContext<'_>,
    ) -> Result<PersistedMonitorSet, ToolError> {
        let MonitorContext {
            lifecycle,
            workspace_root,
            cwd,
            now_ms,
            ..
        } = context;
        if definitions.is_empty() {
            return Ok(PersistedMonitorSet::empty(session_id, 0, lifecycle, now_ms));
        }
        if self.load_optional(session_id)?.is_some() {
            return Err(corrupt("initial monitor set already exists"));
        }
        let monitors = build_initial_monitors(definitions, workspace_root, cwd, now_ms)?;
        let mut set = PersistedMonitorSet::empty(session_id, 0, lifecycle, now_ms);
        set.next_monitor_id = u64::try_from(monitors.len())
            .ok()
            .and_then(|count| count.checked_add(1))
            .ok_or_else(|| corrupt("initial monitor id exhausted"))?;
        set.monitors = monitors;
        set.validate(session_id)?;
        self.save(&set)?;
        Ok(set)
    }

    pub(crate) fn operate(
        &self,
        session_id: &str,
        operation: MonitorOperation,
        context: MonitorContext<'_>,
    ) -> Result<(PersistedMonitorSet, Option<String>), ToolError> {
        let MonitorContext {
            current_cursor,
            lifecycle,
            workspace_root,
            cwd,
            now_ms,
        } = context;
        operation
            .validate()
            .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;
        let mut candidate = self.load_optional(session_id)?.unwrap_or_else(|| {
            PersistedMonitorSet::empty(session_id, current_cursor, lifecycle, now_ms)
        });
        candidate.last_lifecycle = lifecycle;
        let monitor_id = match operation {
            MonitorOperation::Add { definition } => {
                if candidate.monitors.len() >= MAXIMUM_MONITORS {
                    return Err(ToolError::InvalidArguments(
                        "terminal session already has 32 monitors".into(),
                    ));
                }
                let id = stable_id(candidate.next_monitor_id)
                    .map_err(|error| corrupt(&error.to_string()))?;
                candidate.next_monitor_id = candidate
                    .next_monitor_id
                    .checked_add(1)
                    .ok_or_else(|| corrupt("monitor id exhausted"))?;
                let mut monitor = Monitor::new(id.clone(), definition, now_ms)
                    .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;
                prepare_monitor_effects(&mut monitor, workspace_root, cwd)?;
                candidate.monitors.push(monitor);
                Some(id)
            }
            MonitorOperation::Update {
                monitor_id,
                definition,
            } => {
                let index = find_monitor(&candidate, &monitor_id)?;
                let generation = candidate.monitors[index]
                    .runtime
                    .generation
                    .checked_add(1)
                    .ok_or_else(|| corrupt("monitor generation exhausted"))?;
                let mut replacement = Monitor::new(monitor_id.clone(), definition, now_ms)
                    .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;
                replacement.runtime.generation = generation;
                prepare_monitor_effects(&mut replacement, workspace_root, cwd)?;
                candidate.monitors[index] = replacement;
                candidate.push_event(
                    index,
                    EventReason::Updated,
                    lifecycle,
                    current_cursor,
                    now_ms,
                )?;
                Some(monitor_id)
            }
            MonitorOperation::Pause { monitor_id } => {
                let index = find_monitor(&candidate, &monitor_id)?;
                if candidate.monitors[index].pause() {
                    candidate.monitors[index]
                        .bump_generation()
                        .map_err(|error| corrupt(&error.to_string()))?;
                    candidate.push_event(
                        index,
                        EventReason::Paused,
                        lifecycle,
                        current_cursor,
                        now_ms,
                    )?;
                }
                Some(monitor_id)
            }
            MonitorOperation::Resume { monitor_id } => {
                let index = find_monitor(&candidate, &monitor_id)?;
                if candidate.monitors[index]
                    .resume(now_ms)
                    .map_err(|error| corrupt(&error.to_string()))?
                {
                    candidate.monitors[index]
                        .bump_generation()
                        .map_err(|error| corrupt(&error.to_string()))?;
                    candidate.push_event(
                        index,
                        EventReason::Resumed,
                        lifecycle,
                        current_cursor,
                        now_ms,
                    )?;
                }
                Some(monitor_id)
            }
            MonitorOperation::Remove { monitor_id } => {
                let index = find_monitor(&candidate, &monitor_id)?;
                candidate.push_event(
                    index,
                    EventReason::Removed,
                    lifecycle,
                    current_cursor,
                    now_ms,
                )?;
                candidate.monitors.remove(index);
                Some(monitor_id)
            }
        };
        candidate.validate(session_id)?;
        self.save(&candidate)?;
        Ok((candidate, monitor_id))
    }

    pub(crate) fn inspect(
        &self,
        session_id: &str,
        after_event_id: Option<u64>,
        acknowledge_event_id: Option<u64>,
        max_events: usize,
    ) -> Result<InspectMonitors, ToolError> {
        if max_events == 0 || max_events > 256 {
            return Err(ToolError::InvalidArguments(
                "terminal max_events must be between 1 and 256".into(),
            ));
        }
        let Some(mut candidate) = self.load_optional(session_id)? else {
            return Ok(InspectMonitors {
                monitors: Vec::new(),
                events: Vec::new(),
                event_gap_through: 0,
                next_event_id: 1,
            });
        };
        if let Some(acknowledge) = acknowledge_event_id {
            if acknowledge == 0 || acknowledge >= candidate.next_event_id {
                return Err(ToolError::InvalidArguments(
                    "terminal acknowledge_event_id is outside the event log".into(),
                ));
            }
            candidate.acknowledged_through = candidate.acknowledged_through.max(acknowledge);
            candidate
                .events
                .retain(|event| event.event_id > acknowledge);
            self.save(&candidate)?;
        }
        let after = after_event_id.unwrap_or(candidate.acknowledged_through);
        let events = candidate
            .events
            .iter()
            .filter(|event| event.event_id > after)
            .take(max_events)
            .cloned()
            .collect();
        Ok(InspectMonitors {
            monitors: candidate.summaries(),
            events,
            event_gap_through: candidate.event_gap_through,
            next_event_id: candidate.next_event_id,
        })
    }

    pub(crate) fn monitored_sessions(&self) -> Result<Vec<String>, ToolError> {
        let mut session_ids = Vec::new();
        let entries = fs::read_dir(&self.root)
            .map_err(|error| io_error("list terminal monitor sets", error))?;
        for entry in entries.take(256) {
            let entry = entry.map_err(|error| io_error("read terminal monitor entry", error))?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Some(session_id) = name
                .strip_prefix("monitors-")
                .and_then(|value| value.strip_suffix(".json"))
            else {
                continue;
            };
            if !valid_session_id(session_id) {
                continue;
            }
            if self
                .load_optional(session_id)?
                .is_some_and(|set| !set.monitors.is_empty())
            {
                session_ids.push(session_id.to_owned());
            }
        }
        session_ids.sort();
        Ok(session_ids)
    }

    pub(crate) fn observation_request(
        &self,
        session_id: &str,
    ) -> Result<Option<(u64, bool)>, ToolError> {
        let Some(set) = self.load_optional(session_id)? else {
            return Ok(None);
        };
        let include_screen = set.monitors.iter().any(|monitor| {
            !matches!(
                monitor.runtime.state,
                MonitorState::Paused | MonitorState::Degraded
            ) && matches!(
                monitor.definition.condition,
                MonitorCondition::ScreenMatches { .. }
            )
        });
        Ok((!set.monitors.is_empty()).then_some((set.cursor_offset, include_screen)))
    }

    pub(crate) fn active_count_for(&self, session_id: &str) -> Result<usize, ToolError> {
        Ok(self
            .load_optional(session_id)?
            .map_or(0, |set| set.active_count()))
    }

    pub(crate) fn evaluate_terminal(
        &self,
        session_id: &str,
        observation: &TerminalObservation,
        now_ms: i64,
    ) -> Result<bool, ToolError> {
        let Some(mut candidate) = self.load_optional(session_id)? else {
            return Ok(false);
        };
        if candidate.monitors.is_empty() {
            return Ok(false);
        }
        if !observation.raw_gap && observation.cursor_start != candidate.cursor_offset {
            // A concurrent monitor operation may have sampled a newer cursor.
            // Discard this observation and let the supervisor retry from the
            // committed cursor instead of treating a benign race as damage.
            return Ok(false);
        }
        if observation.cursor_end < observation.cursor_start {
            return Err(corrupt("terminal observation cursor is inconsistent"));
        }
        let mut changed = false;
        if observation.raw_gap {
            for monitor in &mut candidate.monitors {
                changed = monitor
                    .degrade_for_raw_gap()
                    .map_err(|error| corrupt(&error.to_string()))?
                    || changed;
            }
        }
        if !observation.output.is_empty() {
            candidate.last_output_at_ms = now_ms;
            changed = true;
            let mut index = 0;
            while index < candidate.monitors.len() {
                if matches!(
                    candidate.monitors[index].runtime.state,
                    MonitorState::Paused | MonitorState::Degraded
                ) {
                    index += 1;
                    continue;
                }
                let decision = match &candidate.monitors[index].definition.condition {
                    MonitorCondition::OutputQuiet { .. } => {
                        candidate.monitors[index]
                            .note_output(now_ms)
                            .map_err(|error| corrupt(&error.to_string()))?;
                        changed = true;
                        None
                    }
                    MonitorCondition::OutputContains { .. }
                    | MonitorCondition::OutputMatches { .. } => {
                        let matched = candidate.monitors[index]
                            .feed_output(&observation.output)
                            .map_err(|error| corrupt(&error.to_string()))?;
                        Some(
                            candidate.monitors[index]
                                .observe(Observation::Output, matched, now_ms)
                                .map_err(|error| corrupt(&error.to_string()))?,
                        )
                    }
                    MonitorCondition::ScreenMatches { pattern } => {
                        let matched = observation.screen_text.as_deref().is_some_and(|screen| {
                            pattern_matches(pattern.as_bytes(), true, screen.as_bytes())
                                .unwrap_or(false)
                        });
                        Some(
                            candidate.monitors[index]
                                .observe(Observation::Screen, matched, now_ms)
                                .map_err(|error| corrupt(&error.to_string()))?,
                        )
                    }
                    _ => None,
                };
                if let Some(decision) = decision {
                    changed = true;
                    if apply_decision(
                        &mut candidate,
                        index,
                        decision,
                        observation.lifecycle,
                        observation.cursor_end,
                        now_ms,
                    )? {
                        continue;
                    }
                }
                index += 1;
            }
        }
        candidate.cursor_offset = observation.cursor_end;

        let mut index = 0;
        while index < candidate.monitors.len() {
            let quiet = candidate.monitors[index].quiet_due(now_ms);
            let decision = if quiet {
                candidate.monitors[index]
                    .observe(Observation::Quiet, true, now_ms)
                    .map_err(|error| corrupt(&error.to_string()))?
            } else {
                candidate.monitors[index]
                    .timer_decision(now_ms)
                    .map_err(|error| corrupt(&error.to_string()))?
            };
            if decision != Decision::default() {
                changed = true;
                if apply_decision(
                    &mut candidate,
                    index,
                    decision,
                    observation.lifecycle,
                    observation.cursor_end,
                    now_ms,
                )? {
                    continue;
                }
            }
            index += 1;
        }

        if observation.lifecycle == ObservedLifecycle::Running
            && let Some(index) = candidate
                .monitors
                .iter()
                .position(|monitor| monitor.polling_due(now_ms))
        {
            let matched = poll_condition(
                &mut candidate.monitors[index],
                &observation.workspace_root,
                &observation.cwd,
            );
            let decision = candidate.monitors[index]
                .observe(Observation::Check, matched, now_ms)
                .map_err(|error| corrupt(&error.to_string()))?;
            changed = true;
            apply_decision(
                &mut candidate,
                index,
                decision,
                observation.lifecycle,
                observation.cursor_end,
                now_ms,
            )?;
        }

        if candidate.last_lifecycle == ObservedLifecycle::Running
            && observation.lifecycle != ObservedLifecycle::Running
        {
            changed = true;
            end_monitors(
                &mut candidate,
                observation.lifecycle,
                observation.exit_code,
                observation.signal,
                observation.cursor_end,
                now_ms,
            )?;
        }
        if candidate.last_lifecycle != observation.lifecycle {
            candidate.last_lifecycle = observation.lifecycle;
            changed = true;
        }
        if changed {
            self.save(&candidate)?;
        }
        Ok(changed)
    }

    pub(crate) fn end_missing_session(
        &self,
        session_id: &str,
        now_ms: i64,
    ) -> Result<bool, ToolError> {
        self.end_session(session_id, ObservedLifecycle::Lost, now_ms)
    }

    pub(crate) fn end_session(
        &self,
        session_id: &str,
        lifecycle: ObservedLifecycle,
        now_ms: i64,
    ) -> Result<bool, ToolError> {
        let Some(mut candidate) = self.load_optional(session_id)? else {
            return Ok(false);
        };
        if candidate.monitors.is_empty() {
            return Ok(false);
        }
        let cursor_offset = candidate.cursor_offset;
        end_monitors(&mut candidate, lifecycle, None, None, cursor_offset, now_ms)?;
        candidate.last_lifecycle = lifecycle;
        self.save(&candidate)?;
        Ok(true)
    }

    pub(crate) fn load_optional(
        &self,
        session_id: &str,
    ) -> Result<Option<PersistedMonitorSet>, ToolError> {
        let path = self.path(session_id)?;
        let mut file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(io_error("open terminal monitor set", error)),
        };
        let metadata = file
            .metadata()
            .map_err(|error| io_error("stat terminal monitor set", error))?;
        if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAXIMUM_FILE_BYTES as u64
        {
            return Err(corrupt("terminal monitor set has an invalid size"));
        }
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|error| io_error("read terminal monitor set", error))?;
        let set: PersistedMonitorSet = serde_json::from_slice(&bytes)
            .map_err(|error| corrupt(&format!("decode terminal monitor set: {error}")))?;
        set.validate(session_id)?;
        Ok(Some(set))
    }

    pub(crate) fn save(&self, set: &PersistedMonitorSet) -> Result<(), ToolError> {
        set.validate(&set.session_id)?;
        let bytes = serde_json::to_vec(set)
            .map_err(|error| corrupt(&format!("encode terminal monitor set: {error}")))?;
        if bytes.is_empty() || bytes.len() > MAXIMUM_FILE_BYTES {
            return Err(corrupt("terminal monitor set exceeds its size limit"));
        }
        let path = self.path(&set.session_id)?;
        let mut stage = AtomicWriteFile::open(&path)
            .map_err(|error| io_error("stage terminal monitor set", error))?;
        stage
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|error| io_error("secure terminal monitor set", error))?;
        stage
            .write_all(&bytes)
            .and_then(|()| stage.sync_all())
            .map_err(|error| io_error("write terminal monitor set", error))?;
        stage
            .commit()
            .map_err(|error| io_error("commit terminal monitor set", error))
    }

    fn path(&self, session_id: &str) -> Result<PathBuf, ToolError> {
        if !valid_session_id(session_id) {
            return Err(ToolError::InvalidArguments(
                "terminal session_id is invalid".into(),
            ));
        }
        Ok(self.root.join(format!("monitors-{session_id}.json")))
    }
}

fn build_initial_monitors(
    definitions: &[crate::monitor::MonitorDefinition],
    workspace_root: &Path,
    cwd: &Path,
    now_ms: i64,
) -> Result<Vec<Monitor>, ToolError> {
    if definitions.len() > MAXIMUM_MONITORS {
        return Err(ToolError::InvalidArguments(
            "terminal start accepts at most 32 initial monitors".into(),
        ));
    }
    let mut monitors = Vec::with_capacity(definitions.len());
    for (index, definition) in definitions.iter().cloned().enumerate() {
        let sequence = u64::try_from(index)
            .ok()
            .and_then(|value| value.checked_add(1))
            .ok_or_else(|| corrupt("initial monitor id exhausted"))?;
        let monitor_id = stable_id(sequence).map_err(|error| corrupt(&error.to_string()))?;
        let mut monitor = Monitor::new(monitor_id, definition, now_ms)
            .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;
        prepare_monitor_effects(&mut monitor, workspace_root, cwd)?;
        monitors.push(monitor);
    }
    Ok(monitors)
}

fn prepare_monitor_effects(
    monitor: &mut Monitor,
    workspace_root: &Path,
    cwd: &Path,
) -> Result<(), ToolError> {
    match &monitor.definition.condition {
        MonitorCondition::PathExists { path } | MonitorCondition::PathSize { path, .. } => {
            resolve_monitor_path(workspace_root, cwd, path, false)?;
        }
        MonitorCondition::PathChanged { path } => {
            let resolved = resolve_monitor_path(workspace_root, cwd, path, false)?;
            monitor.runtime.path_baseline = Some(path_baseline(&resolved)?);
        }
        MonitorCondition::CustomProbe { cwd: probe_cwd, .. } => {
            let resolved = resolve_monitor_path(workspace_root, cwd, probe_cwd, true)?;
            if !resolved.is_dir() {
                return Err(ToolError::InvalidArguments(
                    "terminal monitor probe cwd is not a directory".into(),
                ));
            }
            monitor.runtime.probe_cwd_fingerprint = Some(path_fingerprint(&resolved));
        }
        MonitorCondition::HttpReady { pattern } => {
            parse_http_target(pattern)?;
        }
        _ => {}
    }
    Ok(())
}

fn poll_condition(monitor: &mut Monitor, workspace_root: &Path, cwd: &Path) -> bool {
    match &monitor.definition.condition {
        MonitorCondition::TcpReady { host, port } => tcp_ready(host, *port),
        MonitorCondition::HttpReady { pattern } => http_ready(pattern),
        MonitorCondition::PathExists { path } => {
            resolve_monitor_path(workspace_root, cwd, path, false)
                .and_then(|path| path_baseline(&path))
                .is_ok_and(|baseline| baseline.exists)
        }
        MonitorCondition::PathChanged { path } => {
            let Ok(current) = resolve_monitor_path(workspace_root, cwd, path, false)
                .and_then(|path| path_baseline(&path))
            else {
                return false;
            };
            let changed = monitor
                .runtime
                .path_baseline
                .is_some_and(|previous| previous != current);
            monitor.runtime.path_baseline = Some(current);
            changed
        }
        MonitorCondition::PathSize {
            path,
            minimum_bytes,
        } => resolve_monitor_path(workspace_root, cwd, path, false)
            .and_then(|path| path_baseline(&path))
            .is_ok_and(|baseline| baseline.exists && baseline.size >= *minimum_bytes),
        MonitorCondition::CustomProbe {
            command,
            cwd: probe_cwd,
        } => resolve_monitor_path(workspace_root, cwd, probe_cwd, true).is_ok_and(|resolved| {
            monitor.runtime.probe_cwd_fingerprint == Some(path_fingerprint(&resolved))
                && run_custom_probe(command, &resolved)
        }),
        _ => false,
    }
}

fn resolve_monitor_path(
    workspace_root: &Path,
    cwd: &Path,
    input: &str,
    existing: bool,
) -> Result<PathBuf, ToolError> {
    let canonical_workspace = workspace_root
        .canonicalize()
        .map_err(|error| ToolError::Execution(format!("workspace is unavailable: {error}")))?;
    let canonical_cwd = cwd
        .canonicalize()
        .map_err(|error| ToolError::Execution(format!("terminal cwd is unavailable: {error}")))?;
    let relative_cwd = canonical_cwd
        .strip_prefix(&canonical_workspace)
        .map_err(|_| ToolError::OutsideWorkspace(canonical_cwd.display().to_string()))?;
    let requested = if Path::new(input).is_absolute() {
        PathBuf::from(input)
    } else {
        relative_cwd.join(input)
    };
    let requested = requested.to_str().ok_or_else(|| {
        ToolError::InvalidArguments("terminal monitor path is not valid UTF-8".into())
    })?;
    let resolved = if existing {
        resolve_existing(&canonical_workspace, requested)?
    } else {
        resolve_target(&canonical_workspace, requested)?
    };
    if !resolved.absolute.starts_with(&canonical_workspace) {
        return Err(ToolError::OutsideWorkspace(
            resolved.absolute.display().to_string(),
        ));
    }
    Ok(resolved.absolute)
}

fn path_baseline(path: &Path) -> Result<PathBaseline, ToolError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(PathBaseline {
                exists: false,
                size: 0,
                modified_ns: 0,
            });
        }
        Err(error) => return Err(io_error("stat terminal monitor path", error)),
    };
    let modified_ns = metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .and_then(|duration| i128::try_from(duration.as_nanos()).ok())
        .unwrap_or(0);
    Ok(PathBaseline {
        exists: true,
        size: metadata.len(),
        modified_ns,
    })
}

fn path_fingerprint(path: &Path) -> [u8; 32] {
    Sha256::digest(path.as_os_str().as_bytes()).into()
}

fn tcp_ready(host: &str, port: u16) -> bool {
    (host, port)
        .to_socket_addrs()
        .ok()
        .into_iter()
        .flatten()
        .take(4)
        .any(|address| TcpStream::connect_timeout(&address, NETWORK_CONNECT_TIMEOUT).is_ok())
}

struct HttpTarget {
    host: String,
    port: u16,
    authority: String,
    path_and_query: String,
}

fn parse_http_target(url: &str) -> Result<HttpTarget, ToolError> {
    let uri: http::Uri = url
        .parse()
        .map_err(|_| ToolError::InvalidArguments("terminal HTTP monitor URL is invalid".into()))?;
    if uri.scheme_str() != Some("http") {
        return Err(ToolError::InvalidArguments(
            "terminal HTTP monitor supports only http:// URLs".into(),
        ));
    }
    let authority = uri.authority().ok_or_else(|| {
        ToolError::InvalidArguments("terminal HTTP monitor URL has no authority".into())
    })?;
    if authority.as_str().contains('@') {
        return Err(ToolError::InvalidArguments(
            "terminal HTTP monitor URL must not contain user information".into(),
        ));
    }
    let host = uri.host().ok_or_else(|| {
        ToolError::InvalidArguments("terminal HTTP monitor URL has no host".into())
    })?;
    let path_and_query = uri
        .path_and_query()
        .map_or("/", http::uri::PathAndQuery::as_str);
    Ok(HttpTarget {
        host: host.to_owned(),
        port: uri.port_u16().unwrap_or(80),
        authority: authority.as_str().to_owned(),
        path_and_query: path_and_query.to_owned(),
    })
}

fn http_ready(url: &str) -> bool {
    let Ok(target) = parse_http_target(url) else {
        return false;
    };
    let Some(mut stream) = (target.host.as_str(), target.port)
        .to_socket_addrs()
        .ok()
        .into_iter()
        .flatten()
        .take(4)
        .find_map(|address| TcpStream::connect_timeout(&address, NETWORK_CONNECT_TIMEOUT).ok())
    else {
        return false;
    };
    if stream.set_read_timeout(Some(HTTP_IO_TIMEOUT)).is_err()
        || stream.set_write_timeout(Some(HTTP_IO_TIMEOUT)).is_err()
    {
        return false;
    }
    let request = format!(
        "GET {} HTTP/1.0\r\nHost: {}\r\nConnection: close\r\n\r\n",
        target.path_and_query, target.authority
    );
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }
    let mut response = [0_u8; 12];
    let mut received = 0;
    while received < response.len() {
        match stream.read(&mut response[received..]) {
            Ok(0) => break,
            Ok(count) => received += count,
            Err(_) => return false,
        }
    }
    received >= response.len() && response.starts_with(b"HTTP/")
}

fn run_custom_probe(command: &str, cwd: &Path) -> bool {
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return false;
    };
    runtime.block_on(run_custom_probe_async(command, cwd))
}

async fn run_custom_probe_async(command: &str, cwd: &Path) -> bool {
    let mut process = tokio::process::Command::new("/bin/sh");
    process
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    process.as_std_mut().process_group(0);
    let Ok(mut child) = process.spawn() else {
        return false;
    };
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill().await;
        return false;
    };
    let Some(stderr) = child.stderr.take() else {
        let _ = child.kill().await;
        return false;
    };
    let process_id = child.id();
    let completed = tokio::time::timeout(PROBE_TIMEOUT, async {
        tokio::join!(
            child.wait(),
            drain_probe_output(stdout),
            drain_probe_output(stderr)
        )
    })
    .await;
    terminate_probe_group(process_id);
    match completed {
        Ok((Ok(status), stdout_bytes, stderr_bytes)) => {
            status.success() && stdout_bytes.saturating_add(stderr_bytes) <= PROBE_OUTPUT_BYTES
        }
        Ok((Err(_), _, _)) | Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            false
        }
    }
}

async fn drain_probe_output(mut reader: impl tokio::io::AsyncRead + Unpin) -> u64 {
    let mut total = 0_u64;
    let mut buffer = [0_u8; 8 * 1_024];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) => return total,
            Ok(count) => {
                total = total.saturating_add(u64::try_from(count).unwrap_or(u64::MAX));
            }
            Err(_) => return u64::MAX,
        }
    }
}

fn terminate_probe_group(process_id: Option<u32>) {
    if let Some(process_id) = process_id.and_then(|value| i32::try_from(value).ok()) {
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(process_id),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
}

fn apply_decision(
    set: &mut PersistedMonitorSet,
    monitor_index: usize,
    decision: Decision,
    lifecycle: ObservedLifecycle,
    cursor_offset: u64,
    now_ms: i64,
) -> Result<bool, ToolError> {
    if let Some(reason) = decision.notify {
        set.push_event(monitor_index, reason, lifecycle, cursor_offset, now_ms)?;
    }
    if decision.remove {
        set.monitors.remove(monitor_index);
        return Ok(true);
    }
    Ok(false)
}

fn end_monitors(
    set: &mut PersistedMonitorSet,
    lifecycle: ObservedLifecycle,
    exit_code: Option<i32>,
    signal: Option<crate::monitor::MonitorSignal>,
    cursor_offset: u64,
    now_ms: i64,
) -> Result<(), ToolError> {
    while !set.monitors.is_empty() {
        let condition_matches = match set.monitors[0].definition.condition {
            MonitorCondition::ProcessExit => lifecycle == ObservedLifecycle::Exited,
            MonitorCondition::ExitCode {
                exit_code: expected,
            } => lifecycle == ObservedLifecycle::Exited && exit_code == Some(expected),
            MonitorCondition::Signal { signal: expected } => {
                lifecycle == ObservedLifecycle::Exited && signal == Some(expected)
            }
            _ => false,
        };
        let exit_relevant = matches!(
            set.monitors[0].definition.condition,
            MonitorCondition::ProcessExit
                | MonitorCondition::ExitCode { .. }
                | MonitorCondition::Signal { .. }
        );
        let mut decision = if exit_relevant && lifecycle == ObservedLifecycle::Exited {
            set.monitors[0]
                .observe(Observation::Exit, condition_matches, now_ms)
                .map_err(|error| corrupt(&error.to_string()))?
        } else {
            Decision::default()
        };
        if decision.notify.is_none()
            && set.monitors[0].definition.notify == NotifySchedule::OnExit
            && set.monitors[0].runtime.last_event_reason != Some(EventReason::SessionExit)
        {
            decision = set.monitors[0]
                .observe(Observation::SessionExit, false, now_ms)
                .map_err(|error| corrupt(&error.to_string()))?;
        }
        decision.remove = true;
        apply_decision(set, 0, decision, lifecycle, cursor_offset, now_ms)?;
    }
    Ok(())
}

fn find_monitor(set: &PersistedMonitorSet, monitor_id: &str) -> Result<usize, ToolError> {
    set.monitors
        .iter()
        .position(|monitor| monitor.monitor_id == monitor_id)
        .ok_or_else(|| {
            ToolError::InvalidArguments(format!("terminal monitor not found: {monitor_id}"))
        })
}

fn valid_session_id(value: &str) -> bool {
    value
        .strip_prefix("terminal-n-")
        .or_else(|| value.strip_prefix("terminal-t-"))
        .is_some_and(|suffix| {
            suffix.len() == 32 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
}

fn prepare_private_directory(path: &Path) -> Result<(), ToolError> {
    fs::create_dir_all(path).map_err(|error| io_error("create terminal monitor root", error))?;
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| io_error("stat terminal monitor root", error))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(corrupt("terminal monitor root is unsafe"));
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|error| io_error("secure terminal monitor root", error))
}

fn corrupt(message: &str) -> ToolError {
    ToolError::Execution(format!("terminal monitor state is corrupt: {message}"))
}

fn io_error(context: &str, error: std::io::Error) -> ToolError {
    ToolError::Execution(format!("{context}: {error}"))
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;
    use std::thread;
    use std::time::Instant;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::monitor::{MonitorCondition, MonitorDefinition, MonitorLifetime, NotifySchedule};

    fn definition() -> MonitorDefinition {
        MonitorDefinition {
            condition: MonitorCondition::OutputContains {
                pattern: "ready".into(),
            },
            check_interval_ms: None,
            notify: NotifySchedule::OnMatch,
            lifetime: MonitorLifetime::UntilSessionEnd,
        }
    }

    #[test]
    fn operations_are_atomic_ids_are_stable_and_events_acknowledge_once() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = PathBuf::from("/tmp").join(format!(
            "fx-monitor-store-test-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&root).unwrap();
        let store = MonitorStore::new(&root).unwrap();
        let session_id = format!("terminal-t-{}", uuid::Uuid::new_v4().simple());
        let (_, first) = store
            .operate(
                &session_id,
                MonitorOperation::Add {
                    definition: definition(),
                },
                MonitorContext {
                    current_cursor: 7,
                    lifecycle: ObservedLifecycle::Running,
                    workspace_root: &root,
                    cwd: &root,
                    now_ms: 100,
                },
            )
            .unwrap();
        assert_eq!(first.as_deref(), Some("monitor-1"));
        store
            .operate(
                &session_id,
                MonitorOperation::Pause {
                    monitor_id: "monitor-1".into(),
                },
                MonitorContext {
                    current_cursor: 9,
                    lifecycle: ObservedLifecycle::Running,
                    workspace_root: &root,
                    cwd: &root,
                    now_ms: 110,
                },
            )
            .unwrap();
        let before = fs::read(store.path(&session_id).unwrap()).unwrap();
        assert!(
            store
                .operate(
                    &session_id,
                    MonitorOperation::Resume {
                        monitor_id: "monitor-missing".into(),
                    },
                    MonitorContext {
                        current_cursor: 9,
                        lifecycle: ObservedLifecycle::Running,
                        workspace_root: &root,
                        cwd: &root,
                        now_ms: 120,
                    },
                )
                .is_err()
        );
        assert_eq!(fs::read(store.path(&session_id).unwrap()).unwrap(), before);

        let recovered = MonitorStore::new(&root).unwrap();
        let inspect = recovered.inspect(&session_id, None, None, 256).unwrap();
        assert_eq!(inspect.monitors[0].state, MonitorState::Paused);
        assert_eq!(inspect.events.len(), 1);
        assert_eq!(inspect.events[0].reason, EventReason::Paused);
        let event_id = inspect.events[0].event_id;
        let acknowledged = recovered
            .inspect(&session_id, Some(event_id), Some(event_id), 256)
            .unwrap();
        assert!(acknowledged.events.is_empty());
        let (_, second) = recovered
            .operate(
                &session_id,
                MonitorOperation::Add {
                    definition: definition(),
                },
                MonitorContext {
                    current_cursor: 9,
                    lifecycle: ObservedLifecycle::Running,
                    workspace_root: &root,
                    cwd: &root,
                    now_ms: 130,
                },
            )
            .unwrap();
        assert_eq!(second.as_deref(), Some("monitor-2"));

        fs::remove_file(recovered.path(&session_id).unwrap()).unwrap();
        fs::remove_dir(root.join("monitors")).unwrap();
        fs::remove_dir(root).unwrap();
    }

    #[test]
    fn polling_paths_are_workspace_scoped_and_match_without_consuming_output() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = PathBuf::from("/tmp").join(format!(
            "fx-monitor-poll-test-{}-{nonce}",
            std::process::id()
        ));
        let workspace = root.join("workspace");
        let cwd = workspace.join("nested");
        fs::create_dir_all(&cwd).unwrap();
        let store = MonitorStore::new(&root).unwrap();
        let session_id = format!("terminal-n-{}", uuid::Uuid::new_v4().simple());
        store
            .operate(
                &session_id,
                MonitorOperation::Add {
                    definition: MonitorDefinition {
                        condition: MonitorCondition::PathExists {
                            path: "ready.flag".into(),
                        },
                        check_interval_ms: Some(10),
                        notify: NotifySchedule::OnMatch,
                        lifetime: MonitorLifetime::UntilMatch,
                    },
                },
                MonitorContext {
                    current_cursor: 0,
                    lifecycle: ObservedLifecycle::Running,
                    workspace_root: &workspace,
                    cwd: &cwd,
                    now_ms: 100,
                },
            )
            .unwrap();
        fs::write(cwd.join("ready.flag"), b"ready").unwrap();
        store
            .evaluate_terminal(
                &session_id,
                &TerminalObservation {
                    lifecycle: ObservedLifecycle::Running,
                    exit_code: None,
                    signal: None,
                    cursor_start: 0,
                    cursor_end: 0,
                    raw_gap: false,
                    output: Vec::new(),
                    screen_text: None,
                    workspace_root: workspace.clone(),
                    cwd: cwd.clone(),
                },
                110,
            )
            .unwrap();
        let inspected = store.inspect(&session_id, None, None, 256).unwrap();
        assert!(inspected.monitors.is_empty());
        assert_eq!(inspected.events.len(), 1);
        assert_eq!(inspected.events[0].reason, EventReason::Matched);

        let before = fs::read(store.path(&session_id).unwrap()).unwrap();
        assert!(
            store
                .operate(
                    &session_id,
                    MonitorOperation::Add {
                        definition: MonitorDefinition {
                            condition: MonitorCondition::PathExists {
                                path: "/etc/passwd".into(),
                            },
                            check_interval_ms: Some(10),
                            notify: NotifySchedule::OnMatch,
                            lifetime: MonitorLifetime::UntilMatch,
                        },
                    },
                    MonitorContext {
                        current_cursor: 0,
                        lifecycle: ObservedLifecycle::Running,
                        workspace_root: &workspace,
                        cwd: &cwd,
                        now_ms: 120,
                    },
                )
                .is_err()
        );
        assert_eq!(fs::read(store.path(&session_id).unwrap()).unwrap(), before);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn network_pollers_require_a_real_tcp_or_http_peer() {
        let tcp = TcpListener::bind("127.0.0.1:0").unwrap();
        let tcp_port = tcp.local_addr().unwrap().port();
        let tcp_peer = thread::spawn(move || tcp.accept().unwrap());
        assert!(tcp_ready("127.0.0.1", tcp_port));
        drop(tcp_peer.join().unwrap());

        let http = TcpListener::bind("127.0.0.1:0").unwrap();
        let http_port = http.local_addr().unwrap().port();
        let http_peer = thread::spawn(move || {
            let (mut stream, _) = http.accept().unwrap();
            let mut request = [0_u8; 1024];
            let count = stream.read(&mut request).unwrap();
            assert!(request[..count].starts_with(b"GET /health?full=1 HTTP/1.0\r\n"));
            stream.write_all(b"HTTP/1.0 200 OK\r\n\r\n").unwrap();
        });
        assert!(http_ready(&format!(
            "http://127.0.0.1:{http_port}/health?full=1"
        )));
        http_peer.join().unwrap();
        assert!(parse_http_target("https://example.com/").is_err());
        assert!(parse_http_target("http://user@example.com/").is_err());
    }

    #[test]
    fn custom_probe_enforces_exit_output_and_timeout_bounds() {
        let cwd = std::env::current_dir().unwrap();
        assert!(run_custom_probe("printf ok", &cwd));
        assert!(!run_custom_probe("exit 7", &cwd));
        assert!(!run_custom_probe("yes x | head -c 20000", &cwd));
        let started = Instant::now();
        assert!(!run_custom_probe("sleep 3", &cwd));
        assert!(started.elapsed() < Duration::from_millis(2_500));
    }
}
