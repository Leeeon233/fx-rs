//! Executor-neutral terminal-monitor state machine.
//!
//! This module deliberately contains no threads, clocks, sockets, filesystem
//! access, or process inspection. A native session, tmux recovery host, or
//! detached supervisor supplies observations and persists [`Monitor`] values;
//! all three therefore share validation, scheduling, matching, and event
//! semantics.

use serde::{Deserialize, Serialize};

pub const MINIMUM_SCHEDULE_MS: u64 = 10;
pub const MAXIMUM_SCHEDULE_MS: u64 = 24 * 60 * 60 * 1_000;
pub const MAXIMUM_LIFETIME_MS: u64 = 365 * 24 * 60 * 60 * 1_000;
pub const MAXIMUM_PATTERN_BYTES: usize = 256;
pub const MAXIMUM_MONITOR_ID_BYTES: usize = 128;
pub const MAXIMUM_AUTHORITY_TEXT_BYTES: usize = 4_096;
pub const MAXIMUM_COMMAND_BYTES: usize = 64 * 1_024;
const PATTERN_WORDS: usize = (MAXIMUM_PATTERN_BYTES + 1).div_ceil(64);

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MonitorSignal {
    Hangup,
    Interrupt,
    Quit,
    Terminate,
    Kill,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MonitorCondition {
    ProcessExit,
    ExitCode { exit_code: i32 },
    Signal { signal: MonitorSignal },
    OutputContains { pattern: String },
    OutputMatches { pattern: String },
    OutputQuiet { duration_ms: u64 },
    ScreenMatches { pattern: String },
    TcpReady { host: String, port: u16 },
    HttpReady { pattern: String },
    PathExists { path: String },
    PathChanged { path: String },
    PathSize { path: String, minimum_bytes: u64 },
    CustomProbe { command: String, cwd: String },
}

impl MonitorCondition {
    pub fn requires_polling(&self) -> bool {
        matches!(
            self,
            Self::TcpReady { .. }
                | Self::HttpReady { .. }
                | Self::PathExists { .. }
                | Self::PathChanged { .. }
                | Self::PathSize { .. }
                | Self::CustomProbe { .. }
        )
    }

    fn validate(&self) -> Result<(), MonitorError> {
        match self {
            Self::ProcessExit | Self::ExitCode { .. } | Self::Signal { .. } => Ok(()),
            Self::OutputContains { pattern }
            | Self::OutputMatches { pattern }
            | Self::ScreenMatches { pattern } => valid_text(
                pattern,
                MAXIMUM_PATTERN_BYTES,
                MonitorError::InvalidCondition,
            ),
            Self::OutputQuiet { duration_ms } => {
                validate_schedule(*duration_ms).map_err(|_| MonitorError::InvalidCondition)
            }
            Self::TcpReady { host, port } => {
                valid_text(
                    host,
                    MAXIMUM_AUTHORITY_TEXT_BYTES,
                    MonitorError::InvalidCondition,
                )?;
                if *port == 0 {
                    return Err(MonitorError::InvalidCondition);
                }
                Ok(())
            }
            Self::HttpReady { pattern } => valid_text(
                pattern,
                MAXIMUM_AUTHORITY_TEXT_BYTES,
                MonitorError::InvalidCondition,
            ),
            Self::PathExists { path }
            | Self::PathChanged { path }
            | Self::PathSize { path, .. } => valid_text(
                path,
                MAXIMUM_AUTHORITY_TEXT_BYTES,
                MonitorError::InvalidCondition,
            ),
            Self::CustomProbe { command, cwd } => {
                valid_text(
                    command,
                    MAXIMUM_COMMAND_BYTES,
                    MonitorError::InvalidCondition,
                )?;
                valid_text(
                    cwd,
                    MAXIMUM_AUTHORITY_TEXT_BYTES,
                    MonitorError::InvalidCondition,
                )
            }
        }
    }

    fn is_gap_sensitive(&self) -> bool {
        matches!(
            self,
            Self::OutputContains { .. }
                | Self::OutputMatches { .. }
                | Self::OutputQuiet { .. }
                | Self::ScreenMatches { .. }
        )
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NotifySchedule {
    OnMatch,
    OnStateChange,
    OnExit,
    EveryCheck,
    EveryNChecks { count: u32 },
    Interval { interval_ms: u64 },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MonitorLifetime {
    UntilMatch,
    UntilSessionEnd,
    Duration { duration_ms: u64 },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MonitorDefinition {
    pub condition: MonitorCondition,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check_interval_ms: Option<u64>,
    pub notify: NotifySchedule,
    pub lifetime: MonitorLifetime,
}

impl MonitorDefinition {
    pub fn validate(&self) -> Result<(), MonitorError> {
        self.condition.validate()?;
        match &self.notify {
            NotifySchedule::EveryNChecks { count } if *count == 0 || *count > 1_000_000 => {
                return Err(MonitorError::InvalidSchedule);
            }
            NotifySchedule::Interval { interval_ms } => validate_schedule(*interval_ms)?,
            _ => {}
        }
        if let MonitorLifetime::Duration { duration_ms } = self.lifetime
            && (duration_ms == 0 || duration_ms > MAXIMUM_LIFETIME_MS)
        {
            return Err(MonitorError::InvalidLifetime);
        }
        match (self.condition.requires_polling(), self.check_interval_ms) {
            (true, Some(interval)) => validate_schedule(interval),
            (true, None) => Err(MonitorError::MissingCheckSchedule),
            (false, Some(_)) => Err(MonitorError::UnexpectedCheckSchedule),
            (false, None) => Ok(()),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MonitorOperation {
    Add {
        definition: MonitorDefinition,
    },
    Update {
        monitor_id: String,
        definition: MonitorDefinition,
    },
    Pause {
        monitor_id: String,
    },
    Resume {
        monitor_id: String,
    },
    Remove {
        monitor_id: String,
    },
}

impl MonitorOperation {
    pub fn validate(&self) -> Result<(), MonitorError> {
        match self {
            Self::Add { definition } => definition.validate(),
            Self::Update {
                monitor_id,
                definition,
            } => {
                validate_monitor_id(monitor_id)?;
                definition.validate()
            }
            Self::Pause { monitor_id }
            | Self::Resume { monitor_id }
            | Self::Remove { monitor_id } => validate_monitor_id(monitor_id),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MonitorState {
    Active,
    Paused,
    Matched,
    Degraded,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EventReason {
    Matched,
    StateChanged,
    SessionExit,
    Check,
    Interval,
    Expired,
    Removed,
    Paused,
    Resumed,
    Updated,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Observation {
    Check,
    Output,
    Screen,
    Quiet,
    Exit,
    SessionExit,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MonitorRuntime {
    pub state: MonitorState,
    pub generation: u64,
    pub created_at_ms: i64,
    pub lifetime_deadline_ms: Option<i64>,
    pub next_check_ms: Option<i64>,
    pub next_notification_ms: Option<i64>,
    pub check_count: u64,
    pub notification_count: u64,
    pub last_event_id: u64,
    pub last_event_reason: Option<EventReason>,
    pub condition_matched: bool,
    pub path_baseline: Option<PathBaseline>,
    pub probe_cwd_fingerprint: Option<[u8; 32]>,
    matcher_states: [u64; PATTERN_WORDS],
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PathBaseline {
    pub exists: bool,
    pub size: u64,
    pub modified_ns: i128,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Monitor {
    pub monitor_id: String,
    pub definition: MonitorDefinition,
    pub runtime: MonitorRuntime,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Decision {
    pub notify: Option<EventReason>,
    pub remove: bool,
    pub state_changed: bool,
}

impl Monitor {
    pub fn new(
        monitor_id: impl Into<String>,
        definition: MonitorDefinition,
        now_ms: i64,
    ) -> Result<Self, MonitorError> {
        definition.validate()?;
        if now_ms < 0 {
            return Err(MonitorError::InvalidClock);
        }
        let monitor_id = monitor_id.into();
        validate_monitor_id(&monitor_id)?;
        let lifetime_deadline_ms = match definition.lifetime {
            MonitorLifetime::Duration { duration_ms } => Some(deadline(now_ms, duration_ms)?),
            MonitorLifetime::UntilMatch | MonitorLifetime::UntilSessionEnd => None,
        };
        let next_check_ms = match (&definition.condition, definition.check_interval_ms) {
            (_, Some(interval)) => Some(deadline(now_ms, interval)?),
            (MonitorCondition::OutputQuiet { duration_ms }, None) => {
                Some(deadline(now_ms, *duration_ms)?)
            }
            _ => None,
        };
        let next_notification_ms = match definition.notify {
            NotifySchedule::Interval { interval_ms } => Some(deadline(now_ms, interval_ms)?),
            _ => None,
        };
        Ok(Self {
            monitor_id,
            definition,
            runtime: MonitorRuntime {
                state: MonitorState::Active,
                generation: 1,
                created_at_ms: now_ms,
                lifetime_deadline_ms,
                next_check_ms,
                next_notification_ms,
                check_count: 0,
                notification_count: 0,
                last_event_id: 0,
                last_event_reason: None,
                condition_matched: false,
                path_baseline: None,
                probe_cwd_fingerprint: None,
                matcher_states: [0; PATTERN_WORDS],
            },
        })
    }

    pub fn observe(
        &mut self,
        observation: Observation,
        condition_matches: bool,
        now_ms: i64,
    ) -> Result<Decision, MonitorError> {
        if matches!(
            self.runtime.state,
            MonitorState::Paused | MonitorState::Degraded
        ) {
            return Ok(Decision::default());
        }
        if self.expired(now_ms)? {
            return Ok(Decision {
                notify: self.state_notification(EventReason::Expired),
                remove: true,
                state_changed: true,
            });
        }
        if observation == Observation::SessionExit {
            return Ok(Decision {
                notify: if self.definition.notify == NotifySchedule::OnExit {
                    Some(EventReason::SessionExit)
                } else {
                    self.state_notification(EventReason::SessionExit)
                },
                remove: true,
                state_changed: true,
            });
        }
        self.runtime.check_count = self
            .runtime
            .check_count
            .checked_add(1)
            .ok_or(MonitorError::CounterOverflow)?;
        if observation == Observation::Check
            && let Some(interval) = self.definition.check_interval_ms
        {
            let due = self.runtime.next_check_ms.unwrap_or(now_ms);
            self.runtime.next_check_ms = Some(advance_deadline(due, interval, now_ms)?);
        }
        if observation == Observation::Quiet
            && let MonitorCondition::OutputQuiet { duration_ms } = self.definition.condition
        {
            self.runtime.next_check_ms = Some(deadline(now_ms, duration_ms)?);
        }

        let newly_matched = condition_matches && !self.runtime.condition_matched;
        if newly_matched {
            self.runtime.condition_matched = true;
            self.runtime.state = MonitorState::Matched;
        }
        let notify = match self.definition.notify {
            NotifySchedule::OnMatch if newly_matched => Some(EventReason::Matched),
            NotifySchedule::OnStateChange if newly_matched => Some(EventReason::StateChanged),
            NotifySchedule::EveryCheck => Some(EventReason::Check),
            NotifySchedule::EveryNChecks { count }
                if self.runtime.check_count.is_multiple_of(u64::from(count)) =>
            {
                Some(EventReason::Check)
            }
            _ => None,
        };
        Ok(Decision {
            notify,
            remove: newly_matched && self.definition.lifetime == MonitorLifetime::UntilMatch,
            state_changed: newly_matched,
        })
    }

    pub fn timer_decision(&mut self, now_ms: i64) -> Result<Decision, MonitorError> {
        if self.expired(now_ms)? {
            return Ok(Decision {
                notify: self.state_notification(EventReason::Expired),
                remove: true,
                state_changed: true,
            });
        }
        if matches!(
            self.runtime.state,
            MonitorState::Paused | MonitorState::Degraded
        ) {
            return Ok(Decision::default());
        }
        let NotifySchedule::Interval { interval_ms } = self.definition.notify else {
            return Ok(Decision::default());
        };
        let due = self
            .runtime
            .next_notification_ms
            .ok_or(MonitorError::InvalidState)?;
        if now_ms < due {
            return Ok(Decision::default());
        }
        self.runtime.next_notification_ms = Some(advance_deadline(due, interval_ms, now_ms)?);
        Ok(Decision {
            notify: Some(EventReason::Interval),
            ..Decision::default()
        })
    }

    pub fn note_output(&mut self, now_ms: i64) -> Result<(), MonitorError> {
        if let MonitorCondition::OutputQuiet { duration_ms } = self.definition.condition {
            self.runtime.next_check_ms = Some(deadline(now_ms, duration_ms)?);
        }
        Ok(())
    }

    pub fn quiet_due(&self, now_ms: i64) -> bool {
        self.runtime.state != MonitorState::Paused
            && matches!(
                self.definition.condition,
                MonitorCondition::OutputQuiet { .. }
            )
            && self.runtime.next_check_ms.is_some_and(|due| now_ms >= due)
    }

    pub fn polling_due(&self, now_ms: i64) -> bool {
        !matches!(
            self.runtime.state,
            MonitorState::Paused | MonitorState::Degraded
        ) && self.definition.condition.requires_polling()
            && self.runtime.next_check_ms.is_some_and(|due| now_ms >= due)
    }

    pub fn next_deadline(&self) -> Option<i64> {
        let mut result = self.runtime.lifetime_deadline_ms;
        if !matches!(
            self.runtime.state,
            MonitorState::Paused | MonitorState::Degraded
        ) {
            result = earlier(result, self.runtime.next_check_ms);
            result = earlier(result, self.runtime.next_notification_ms);
        }
        result
    }

    pub fn pause(&mut self) -> bool {
        if matches!(
            self.runtime.state,
            MonitorState::Paused | MonitorState::Degraded
        ) {
            return false;
        }
        self.runtime.state = MonitorState::Paused;
        true
    }

    pub fn resume(&mut self, now_ms: i64) -> Result<bool, MonitorError> {
        if self.runtime.state != MonitorState::Paused {
            return Ok(false);
        }
        self.runtime.state = if self.runtime.condition_matched {
            MonitorState::Matched
        } else {
            MonitorState::Active
        };
        if let Some(interval) = self.definition.check_interval_ms {
            self.runtime.next_check_ms = Some(deadline(now_ms, interval)?);
        } else if let MonitorCondition::OutputQuiet { duration_ms } = self.definition.condition {
            self.runtime.next_check_ms = Some(deadline(now_ms, duration_ms)?);
        }
        if let NotifySchedule::Interval { interval_ms } = self.definition.notify {
            self.runtime.next_notification_ms = Some(deadline(now_ms, interval_ms)?);
        }
        Ok(true)
    }

    pub fn degrade_for_raw_gap(&mut self) -> Result<bool, MonitorError> {
        if !self.definition.condition.is_gap_sensitive()
            || self.runtime.state == MonitorState::Degraded
        {
            return Ok(false);
        }
        self.runtime.state = MonitorState::Degraded;
        self.runtime.matcher_states.fill(0);
        self.runtime.next_check_ms = None;
        self.runtime.next_notification_ms = None;
        self.bump_generation()?;
        Ok(true)
    }

    pub fn bump_generation(&mut self) -> Result<(), MonitorError> {
        self.runtime.generation = self
            .runtime
            .generation
            .checked_add(1)
            .ok_or(MonitorError::CounterOverflow)?;
        Ok(())
    }

    pub fn note_notification(
        &mut self,
        event_id: u64,
        reason: EventReason,
    ) -> Result<(), MonitorError> {
        if event_id == 0 || event_id <= self.runtime.last_event_id {
            return Err(MonitorError::InvalidEventId);
        }
        self.runtime.last_event_id = event_id;
        self.runtime.last_event_reason = Some(reason);
        self.runtime.notification_count = self
            .runtime
            .notification_count
            .checked_add(1)
            .ok_or(MonitorError::CounterOverflow)?;
        Ok(())
    }

    pub fn feed_output(&mut self, bytes: &[u8]) -> Result<bool, MonitorError> {
        let (pattern, wildcard) = match &self.definition.condition {
            MonitorCondition::OutputContains { pattern } => (pattern.as_bytes(), false),
            MonitorCondition::OutputMatches { pattern } => (pattern.as_bytes(), true),
            _ => return Ok(false),
        };
        pattern_feed(pattern, wildcard, &mut self.runtime.matcher_states, bytes)
    }

    pub fn validate(&self) -> Result<(), MonitorError> {
        validate_monitor_id(&self.monitor_id)?;
        self.definition.validate()?;
        if self.runtime.generation == 0
            || self.runtime.created_at_ms < 0
            || (self.runtime.last_event_id > 0 && self.runtime.notification_count == 0)
            || ((self.runtime.last_event_id == 0) != self.runtime.last_event_reason.is_none())
            || (self.runtime.condition_matched && self.runtime.state == MonitorState::Active)
        {
            return Err(MonitorError::InvalidState);
        }
        if self
            .runtime
            .lifetime_deadline_ms
            .is_some_and(|deadline| deadline <= self.runtime.created_at_ms)
        {
            return Err(MonitorError::InvalidState);
        }
        Ok(())
    }

    fn expired(&self, now_ms: i64) -> Result<bool, MonitorError> {
        if now_ms < 0 {
            return Err(MonitorError::InvalidClock);
        }
        Ok(self
            .runtime
            .lifetime_deadline_ms
            .is_some_and(|deadline| now_ms >= deadline))
    }

    fn state_notification(&self, reason: EventReason) -> Option<EventReason> {
        (self.definition.notify == NotifySchedule::OnStateChange).then_some(reason)
    }
}

pub fn stable_id(sequence: u64) -> Result<String, MonitorError> {
    if sequence == 0 {
        return Err(MonitorError::IdExhausted);
    }
    Ok(format!("monitor-{sequence}"))
}

pub fn pattern_matches(pattern: &[u8], wildcard: bool, bytes: &[u8]) -> Result<bool, MonitorError> {
    let mut states = [0; PATTERN_WORDS];
    pattern_feed(pattern, wildcard, &mut states, bytes)
}

fn pattern_feed(
    pattern: &[u8],
    wildcard: bool,
    states: &mut [u64; PATTERN_WORDS],
    bytes: &[u8],
) -> Result<bool, MonitorError> {
    if pattern.is_empty() || pattern.len() > MAXIMUM_PATTERN_BYTES {
        return Err(MonitorError::InvalidPattern);
    }
    set_bit(states, 0);
    epsilon_closure(pattern, wildcard, states);
    for byte in bytes {
        let mut next = [0; PATTERN_WORDS];
        set_bit(&mut next, 0);
        for (index, token) in pattern.iter().copied().enumerate() {
            if !bit_is_set(states, index) {
                continue;
            }
            if wildcard && token == b'*' {
                set_bit(&mut next, index);
            } else if token == *byte || (wildcard && token == b'?') {
                set_bit(&mut next, index + 1);
            }
        }
        epsilon_closure(pattern, wildcard, &mut next);
        *states = next;
        if bit_is_set(states, pattern.len()) {
            return Ok(true);
        }
    }
    Ok(bit_is_set(states, pattern.len()))
}

fn validate_schedule(interval_ms: u64) -> Result<(), MonitorError> {
    if !(MINIMUM_SCHEDULE_MS..=MAXIMUM_SCHEDULE_MS).contains(&interval_ms) {
        return Err(MonitorError::InvalidSchedule);
    }
    Ok(())
}

fn validate_monitor_id(value: &str) -> Result<(), MonitorError> {
    valid_text(
        value,
        MAXIMUM_MONITOR_ID_BYTES,
        MonitorError::InvalidMonitor,
    )
}

fn valid_text(value: &str, maximum: usize, error: MonitorError) -> Result<(), MonitorError> {
    if value.is_empty() || value.len() > maximum || value.contains('\0') {
        return Err(error);
    }
    Ok(())
}

fn deadline(now_ms: i64, duration_ms: u64) -> Result<i64, MonitorError> {
    let duration = i64::try_from(duration_ms).map_err(|_| MonitorError::DeadlineOverflow)?;
    now_ms
        .checked_add(duration)
        .ok_or(MonitorError::DeadlineOverflow)
}

fn advance_deadline(current: i64, interval_ms: u64, now_ms: i64) -> Result<i64, MonitorError> {
    let interval = i64::try_from(interval_ms).map_err(|_| MonitorError::DeadlineOverflow)?;
    if interval <= 0 {
        return Err(MonitorError::InvalidSchedule);
    }
    if current > now_ms {
        return Ok(current);
    }
    let elapsed = now_ms
        .checked_sub(current)
        .ok_or(MonitorError::DeadlineOverflow)?;
    let steps = elapsed / interval + 1;
    current
        .checked_add(
            steps
                .checked_mul(interval)
                .ok_or(MonitorError::DeadlineOverflow)?,
        )
        .ok_or(MonitorError::DeadlineOverflow)
}

fn earlier(left: Option<i64>, right: Option<i64>) -> Option<i64> {
    match (left, right) {
        (None, value) | (value, None) => value,
        (Some(left), Some(right)) => Some(left.min(right)),
    }
}

fn epsilon_closure(pattern: &[u8], wildcard: bool, states: &mut [u64; PATTERN_WORDS]) {
    if !wildcard {
        return;
    }
    for (index, token) in pattern.iter().copied().enumerate() {
        if token == b'*' && bit_is_set(states, index) {
            set_bit(states, index + 1);
        }
    }
}

fn set_bit(states: &mut [u64; PATTERN_WORDS], index: usize) {
    states[index / 64] |= 1_u64 << (index % 64);
}

fn bit_is_set(states: &[u64; PATTERN_WORDS], index: usize) -> bool {
    states[index / 64] & (1_u64 << (index % 64)) != 0
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MonitorError {
    InvalidCondition,
    InvalidLifetime,
    InvalidSchedule,
    MissingCheckSchedule,
    UnexpectedCheckSchedule,
    InvalidMonitor,
    InvalidState,
    InvalidClock,
    InvalidPattern,
    InvalidEventId,
    CounterOverflow,
    DeadlineOverflow,
    IdExhausted,
}

impl std::fmt::Display for MonitorError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{}",
            match self {
                Self::InvalidCondition => "invalid monitor condition",
                Self::InvalidLifetime => "invalid monitor lifetime",
                Self::InvalidSchedule => "invalid monitor schedule",
                Self::MissingCheckSchedule => "polling monitor requires check_interval_ms",
                Self::UnexpectedCheckSchedule =>
                    "event-driven monitor cannot set check_interval_ms",
                Self::InvalidMonitor => "invalid monitor",
                Self::InvalidState => "invalid monitor state",
                Self::InvalidClock => "invalid monitor clock",
                Self::InvalidPattern => "invalid monitor pattern",
                Self::InvalidEventId => "invalid monitor event id",
                Self::CounterOverflow => "monitor counter overflow",
                Self::DeadlineOverflow => "monitor deadline overflow",
                Self::IdExhausted => "monitor id exhausted",
            }
        )
    }
}

impl std::error::Error for MonitorError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn definition(
        condition: MonitorCondition,
        notify: NotifySchedule,
        lifetime: MonitorLifetime,
    ) -> MonitorDefinition {
        let check_interval_ms = condition.requires_polling().then_some(25);
        MonitorDefinition {
            condition,
            check_interval_ms,
            notify,
            lifetime,
        }
    }

    #[test]
    fn bounded_matcher_preserves_literal_and_wildcard_state_across_chunks() {
        let mut literal = Monitor::new(
            "monitor-1",
            definition(
                MonitorCondition::OutputContains {
                    pattern: "ready".into(),
                },
                NotifySchedule::OnMatch,
                MonitorLifetime::UntilMatch,
            ),
            0,
        )
        .unwrap();
        assert!(!literal.feed_output(b"re").unwrap());
        assert!(literal.feed_output(b"ady").unwrap());

        let mut wildcard = Monitor::new(
            "monitor-2",
            definition(
                MonitorCondition::OutputMatches {
                    pattern: "sta*t?d".into(),
                },
                NotifySchedule::OnMatch,
                MonitorLifetime::UntilMatch,
            ),
            0,
        )
        .unwrap();
        assert!(!wildcard.feed_output(b"xxsta").unwrap());
        assert!(!wildcard.feed_output(b"ble-t").unwrap());
        assert!(wildcard.feed_output(b"edyy").unwrap());
        assert!(pattern_matches(b"*status?ok*", true, b"prefix status-ok suffix").unwrap());
    }

    #[test]
    fn validation_rejects_schedule_floods_mismatches_and_overflow() {
        let mut polling = definition(
            MonitorCondition::PathExists {
                path: "/workspace/ready".into(),
            },
            NotifySchedule::OnMatch,
            MonitorLifetime::UntilMatch,
        );
        polling.check_interval_ms = Some(1);
        assert_eq!(polling.validate(), Err(MonitorError::InvalidSchedule));
        polling.check_interval_ms = None;
        assert_eq!(polling.validate(), Err(MonitorError::MissingCheckSchedule));

        let event = MonitorDefinition {
            condition: MonitorCondition::ProcessExit,
            check_interval_ms: Some(25),
            notify: NotifySchedule::OnMatch,
            lifetime: MonitorLifetime::UntilMatch,
        };
        assert_eq!(event.validate(), Err(MonitorError::UnexpectedCheckSchedule));
        assert_eq!(deadline(i64::MAX, 1), Err(MonitorError::DeadlineOverflow));
    }

    #[test]
    fn decisions_keep_match_exit_interval_and_lifetime_independent() {
        let mut matched = Monitor::new(
            "monitor-1",
            definition(
                MonitorCondition::ProcessExit,
                NotifySchedule::OnMatch,
                MonitorLifetime::UntilMatch,
            ),
            100,
        )
        .unwrap();
        assert_eq!(
            matched.observe(Observation::Exit, true, 110).unwrap(),
            Decision {
                notify: Some(EventReason::Matched),
                remove: true,
                state_changed: true,
            }
        );

        let mut on_exit = Monitor::new(
            "monitor-2",
            definition(
                MonitorCondition::ProcessExit,
                NotifySchedule::OnExit,
                MonitorLifetime::UntilSessionEnd,
            ),
            100,
        )
        .unwrap();
        assert_eq!(
            on_exit
                .observe(Observation::SessionExit, false, 110)
                .unwrap()
                .notify,
            Some(EventReason::SessionExit)
        );

        let mut interval = Monitor::new(
            "monitor-3",
            definition(
                MonitorCondition::ProcessExit,
                NotifySchedule::Interval { interval_ms: 25 },
                MonitorLifetime::UntilSessionEnd,
            ),
            100,
        )
        .unwrap();
        assert_eq!(
            interval.timer_decision(125).unwrap().notify,
            Some(EventReason::Interval)
        );
    }

    #[test]
    fn quiet_pause_resume_event_dedup_and_expiry_are_deterministic() {
        let mut quiet = Monitor::new(
            "monitor-9",
            definition(
                MonitorCondition::OutputQuiet { duration_ms: 50 },
                NotifySchedule::OnStateChange,
                MonitorLifetime::Duration { duration_ms: 100 },
            ),
            1_000,
        )
        .unwrap();
        quiet.note_output(1_025).unwrap();
        assert!(!quiet.quiet_due(1_074));
        assert!(quiet.quiet_due(1_075));
        assert!(quiet.pause());
        assert_eq!(quiet.next_deadline(), Some(1_100));
        assert!(quiet.resume(1_080).unwrap());
        quiet.note_notification(4, EventReason::Matched).unwrap();
        assert_eq!(
            quiet.note_notification(4, EventReason::Matched),
            Err(MonitorError::InvalidEventId)
        );
        assert!(quiet.timer_decision(1_100).unwrap().remove);
        assert_eq!(stable_id(42).unwrap(), "monitor-42");
    }

    #[test]
    fn raw_gaps_degrade_output_monitors_but_not_process_monitors() {
        let mut output = Monitor::new(
            "monitor-gap",
            definition(
                MonitorCondition::OutputContains {
                    pattern: "needle".into(),
                },
                NotifySchedule::OnMatch,
                MonitorLifetime::UntilSessionEnd,
            ),
            100,
        )
        .unwrap();
        assert!(output.degrade_for_raw_gap().unwrap());
        assert_eq!(output.runtime.state, MonitorState::Degraded);
        assert_eq!(output.runtime.generation, 2);

        let mut process = Monitor::new(
            "monitor-process",
            definition(
                MonitorCondition::ProcessExit,
                NotifySchedule::OnMatch,
                MonitorLifetime::UntilSessionEnd,
            ),
            100,
        )
        .unwrap();
        assert!(!process.degrade_for_raw_gap().unwrap());
        assert_eq!(process.runtime.state, MonitorState::Active);
    }

    #[test]
    fn monitor_json_uses_the_public_flat_kind_vocabulary() {
        let definition = definition(
            MonitorCondition::TcpReady {
                host: "127.0.0.1".into(),
                port: 3_000,
            },
            NotifySchedule::EveryNChecks { count: 2 },
            MonitorLifetime::Duration { duration_ms: 500 },
        );
        let value = serde_json::to_value(&definition).unwrap();
        assert_eq!(value["condition"]["kind"], "tcp_ready");
        assert_eq!(value["condition"]["port"], 3_000);
        assert_eq!(value["notify"]["kind"], "every_n_checks");
        assert_eq!(value["lifetime"]["kind"], "duration");
        assert_eq!(
            serde_json::from_value::<MonitorDefinition>(value).unwrap(),
            definition
        );
    }
}
