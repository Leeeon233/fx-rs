//! Native append-only session storage.
//!
//! `events.jsonl` is the canonical history. `commit.json` atomically publishes
//! a durable log prefix; `session.json` is a rebuildable listing projection.
//! A failed append therefore leaves the previous watermark readable, while a
//! failed projection update does not lose an already committed session.

mod memory;

pub use memory::MemoryTool;

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use atomic_write_file::AtomicWriteFile;
use base64::prelude::{BASE64_STANDARD, Engine as _};
use fs4::TryLockError;
use fx_core::{
    BoxFuture, Session, SessionStore, SessionStoreError, SessionSummary, SessionTarget,
    StoredToolResult, TOOL_RESULT_DEFAULT_READ_BYTES, TOOL_RESULT_MAX_READ_BYTES, Tool,
    ToolContext, ToolEffect, ToolError, ToolOutput, ToolResultMatch, ToolResultPage,
    ToolResultStore, ToolResultStoreError,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

const SCHEMA_VERSION: u32 = 3;
const EVENT_SCHEMA_VERSION: u32 = 1;
const WATERMARK_SCHEMA_VERSION: u32 = 1;
const STORAGE_FORMAT: &str = "event_log_v1";
const EVENTS_FILE: &str = "events.jsonl";
const WATERMARK_FILE: &str = "commit.json";
const MANIFEST_FILE: &str = "session.json";
const LOCK_FILE: &str = "session.lock";
const EVENT_FRAME_MAX_BYTES: usize = 8 * 1024 * 1024;
const RAW_STATE_CHUNK_BYTES: usize = 4 * 1024 * 1024;
const MAX_STATE_BYTES: usize = 256 * 1024 * 1024;
const MAX_WATERMARK_BYTES: usize = 16 * 1024;
const MAX_MANIFEST_BYTES: usize = 64 * 1024;
const LOCK_TIMEOUT: Duration = Duration::from_secs(2);
const TOOL_RESULTS_DIRECTORY: &str = "tool-results";
const MAX_STORED_TOOL_RESULT_BYTES: usize = 8 * 1024 * 1024;
const MAX_TOOL_RESULT_MATCHES: usize = 50;

#[derive(Clone, Debug)]
pub struct SessionToolResultStore {
    directory: PathBuf,
}

impl SessionToolResultStore {
    pub fn directory(&self) -> &Path {
        &self.directory
    }
}

impl ToolResultStore for SessionToolResultStore {
    fn store(
        &self,
        tool_call_id: &str,
        tool_name: &str,
        content: &str,
    ) -> Result<StoredToolResult, ToolResultStoreError> {
        if content.len() > MAX_STORED_TOOL_RESULT_BYTES {
            return Err(ToolResultStoreError::TooLarge);
        }
        ensure_result_directory(&self.directory)?;
        let handle = result_handle(tool_call_id, tool_name, content);
        let path = self.directory.join(&handle);
        reject_unsafe_result_file(&path)?;
        let mut stage = AtomicWriteFile::open(&path).map_err(|error| result_io(&path, error))?;
        stage
            .write_all(content.as_bytes())
            .map_err(|error| result_io(&path, error))?;
        stage.commit().map_err(|error| result_io(&path, error))?;
        set_private_result_file(&path)?;
        Ok(StoredToolResult {
            handle,
            stored_bytes: content.len(),
        })
    }

    fn read_range(
        &self,
        handle: &str,
        start_byte: usize,
        byte_count: usize,
    ) -> Result<ToolResultPage, ToolResultStoreError> {
        let content = self.read(handle)?;
        let offset = start_byte.saturating_sub(1).min(content.len());
        let count = byte_count.min(TOOL_RESULT_MAX_READ_BYTES);
        let end = offset.saturating_add(count).min(content.len());
        let safe_start = utf8_forward_boundary(&content, offset);
        let safe_end = utf8_backward_boundary(&content, end).max(safe_start);
        Ok(ToolResultPage {
            content: content[safe_start..safe_end].to_owned(),
            start_byte: safe_start.saturating_add(1),
            end_byte: safe_end,
            total_bytes: content.len(),
        })
    }

    fn search(
        &self,
        handle: &str,
        query: &str,
        max_matches: usize,
    ) -> Result<Vec<ToolResultMatch>, ToolResultStoreError> {
        let query = query.trim();
        if query.is_empty() {
            return Err(ToolResultStoreError::InvalidQuery);
        }
        let content = self.read(handle)?;
        let mut result = Vec::new();
        let mut rendered_bytes = 0usize;
        for (index, line) in content.split('\n').enumerate() {
            if !line.contains(query) {
                continue;
            }
            let remaining = TOOL_RESULT_MAX_READ_BYTES.saturating_sub(rendered_bytes);
            if remaining == 0 {
                break;
            }
            let content = utf8_prefix(line, remaining).to_owned();
            rendered_bytes = rendered_bytes.saturating_add(content.len());
            result.push(ToolResultMatch {
                line: index + 1,
                content,
            });
            if result.len() >= max_matches.min(MAX_TOOL_RESULT_MATCHES) {
                break;
            }
        }
        Ok(result)
    }
}

impl SessionToolResultStore {
    fn read(&self, handle: &str) -> Result<String, ToolResultStoreError> {
        validate_result_handle(handle)?;
        let path = self.directory.join(handle);
        reject_unsafe_result_file(&path)?;
        let mut file = open_result_file(&path)?;
        let size = file
            .metadata()
            .map_err(|error| result_io(&path, error))?
            .len();
        if size > MAX_STORED_TOOL_RESULT_BYTES as u64 {
            return Err(ToolResultStoreError::TooLarge);
        }
        let mut bytes = Vec::with_capacity(size as usize);
        file.read_to_end(&mut bytes)
            .map_err(|error| result_io(&path, error))?;
        String::from_utf8(bytes).map_err(|_| {
            ToolResultStoreError::Unavailable(format!("{} is not valid UTF-8", path.display()))
        })
    }
}

#[derive(Clone)]
pub struct ReadToolResult {
    store: std::sync::Arc<dyn ToolResultStore>,
}

impl ReadToolResult {
    pub fn new(store: std::sync::Arc<dyn ToolResultStore>) -> Self {
        Self { store }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadToolResultInput {
    handle: String,
    #[serde(default = "first_byte")]
    start_byte: usize,
    #[serde(default = "default_result_bytes")]
    byte_count: usize,
    #[serde(default)]
    query: Option<String>,
}

const fn first_byte() -> usize {
    1
}

const fn default_result_bytes() -> usize {
    TOOL_RESULT_DEFAULT_READ_BYTES
}

impl Tool for ReadToolResult {
    fn name(&self) -> &str {
        "read_tool_result"
    }

    fn description(&self) -> &str {
        "Read a bounded UTF-8 byte range or search literal text in a session-scoped stored tool result."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "handle": { "type": "string" },
                "start_byte": { "type": "integer", "minimum": 1 },
                "byte_count": { "type": "integer", "minimum": 1, "maximum": TOOL_RESULT_MAX_READ_BYTES },
                "query": { "type": "string" }
            },
            "required": ["handle"],
            "additionalProperties": false
        })
    }

    fn effect(&self, _arguments: &Value) -> Result<ToolEffect, ToolError> {
        Ok(ToolEffect::Read)
    }

    fn execute<'a>(
        &'a self,
        _context: &'a ToolContext,
        arguments: Value,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            let mut input: ReadToolResultInput = serde_json::from_value(arguments)
                .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;
            input.handle = input.handle.trim().to_owned();
            if input.handle.is_empty() || input.start_byte == 0 || input.byte_count == 0 {
                return Err(ToolError::InvalidArguments(
                    "handle must be non-empty and byte offsets must be positive".into(),
                ));
            }
            let content = if let Some(query) = input.query {
                let query = query.trim();
                if query.is_empty() {
                    return Err(ToolError::InvalidArguments(
                        "query must not be empty".into(),
                    ));
                }
                let matches = self
                    .store
                    .search(&input.handle, query, MAX_TOOL_RESULT_MATCHES)
                    .map_err(|error| result_tool_error(&input.handle, error))?;
                let mut content = format!(
                    "<tool_result_query handle=\"{}\">\nquery: {}\n",
                    input.handle,
                    serde_json::to_string(query).expect("string JSON encoding cannot fail")
                );
                if matches.is_empty() {
                    content.push_str("(no matches)\n");
                } else {
                    for matched in matches {
                        content.push_str(&format!("{}|{}\n", matched.line, matched.content));
                    }
                }
                content.push_str("</tool_result_query>");
                content
            } else {
                let page = self
                    .store
                    .read_range(
                        &input.handle,
                        input.start_byte,
                        input.byte_count.min(TOOL_RESULT_MAX_READ_BYTES),
                    )
                    .map_err(|error| result_tool_error(&input.handle, error))?;
                format!(
                    "<tool_result handle=\"{}\" start_byte=\"{}\" end_byte=\"{}\" total_bytes=\"{}\">\n{}\n</tool_result>",
                    input.handle, page.start_byte, page.end_byte, page.total_bytes, page.content
                )
            };
            Ok(ToolOutput {
                original_bytes: content.len(),
                content,
                is_error: false,
                structured: None,
                truncated: false,
                durable_content: None,
            })
        })
    }
}

#[derive(Clone, Debug)]
pub struct EventLogSessionStore {
    root: PathBuf,
}

impl EventLogSessionStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn generate_session_id() -> String {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        format!("{millis}-{}", Uuid::new_v4().simple())
    }

    pub fn tool_result_store(
        &self,
        session_id: &str,
    ) -> Result<SessionToolResultStore, SessionStoreError> {
        validate_session_id(session_id)?;
        Ok(SessionToolResultStore {
            directory: self.session_dir(session_id).join(TOOL_RESULTS_DIRECTORY),
        })
    }

    fn load_id(&self, id: &str) -> Result<Session, SessionStoreError> {
        validate_session_id(id)?;
        let directory = self.session_dir(id);
        ensure_existing_directory(&directory)?;
        let watermark_path = directory.join(WATERMARK_FILE);
        if path_is_file(&watermark_path)? {
            return replay_committed(&directory, id).map(|outcome| outcome.session);
        }
        load_legacy(&directory.join(MANIFEST_FILE), id)
    }

    fn save_sync(&self, session: &Session) -> Result<(), SessionStoreError> {
        validate_session(session)?;
        ensure_private_directory(&self.root)?;
        let directory = self.session_dir(&session.id);
        ensure_private_directory(&directory)?;
        let _lock = lock_session(&directory)?;

        let watermark_path = directory.join(WATERMARK_FILE);
        let events_path = directory.join(EVENTS_FILE);
        let existing = if path_is_file(&watermark_path)? {
            Some(replay_committed(&directory, &session.id)?)
        } else {
            load_legacy_optional(&directory.join(MANIFEST_FILE), &session.id)?.map(|legacy| {
                ReplayOutcome {
                    session: legacy,
                    watermark: Watermark {
                        schema_version: WATERMARK_SCHEMA_VERSION,
                        session_id: session.id.clone(),
                        log_generation: new_identifier(),
                        last_event_seq: 0,
                        last_event_id: zero_identifier(),
                        event_log_bytes: 0,
                        event_log_sha256: hex::encode(Sha256::digest([])),
                    },
                }
            })
        };
        let mut stored = redact_durable_session(session);
        stored.schema_version = SCHEMA_VERSION;
        if let Some(previous) = &existing {
            validate_replacement(&previous.session, &stored)?;
        }
        if existing
            .as_ref()
            .is_some_and(|prior| prior.session == stored)
        {
            write_manifest_best_effort(&directory, &stored, &prior_watermark(existing.as_ref()))?;
            return Ok(());
        }
        let encoded = serde_json::to_vec(&stored)
            .map_err(|error| SessionStoreError::Corrupt(format!("encode state: {error}")))?;
        if encoded.is_empty() || encoded.len() > MAX_STATE_BYTES {
            return Err(SessionStoreError::Unavailable(format!(
                "session state exceeds the {MAX_STATE_BYTES} byte limit"
            )));
        }

        reject_unsafe_file(&events_path)?;
        let committed_bytes = existing
            .as_ref()
            .map_or(0, |prior| prior.watermark.event_log_bytes);
        let generation = existing
            .as_ref()
            .map(|prior| prior.watermark.log_generation.clone())
            .unwrap_or_else(new_identifier);
        let first_seq = existing
            .as_ref()
            .map_or(1, |prior| prior.watermark.last_event_seq.saturating_add(1));
        if first_seq == 0 {
            return Err(SessionStoreError::Corrupt("event sequence overflow".into()));
        }

        let mut log = open_private_log(&events_path)?;
        let actual_bytes = log
            .metadata()
            .map_err(|error| unavailable(&events_path, error))?
            .len();
        if actual_bytes < committed_bytes {
            return Err(SessionStoreError::Corrupt(
                "event log is shorter than its committed watermark".into(),
            ));
        }
        log.set_len(committed_bytes)
            .map_err(|error| unavailable(&events_path, error))?;
        log.seek(SeekFrom::Start(committed_bytes))
            .map_err(|error| unavailable(&events_path, error))?;

        let mut log_hash = hash_file_prefix(&events_path, committed_bytes)?;
        let state_hash: [u8; 32] = Sha256::digest(&encoded).into();
        let state_hash_hex = hex::encode(state_hash);
        let replacement_id = new_identifier();
        let chunk_count = encoded.len().div_ceil(RAW_STATE_CHUNK_BYTES) as u64;
        let timestamp_ms = stored.updated_at_ms;
        let mut seq = first_seq;
        let mut last_event_id = new_identifier();

        let started = ReplacementStarted {
            replacement_id: &replacement_id,
            encoded_bytes: encoded.len() as u64,
            sha256: &state_hash_hex,
            chunk_count,
        };
        write_event(
            &mut log,
            &mut log_hash,
            EventHeader {
                generation: &generation,
                seq,
                event_id: &last_event_id,
                timestamp_ms,
                kind: "state_replacement_started",
            },
            &started,
        )?;

        for (index, chunk) in encoded.chunks(RAW_STATE_CHUNK_BYTES).enumerate() {
            seq = seq
                .checked_add(1)
                .ok_or_else(|| SessionStoreError::Corrupt("event sequence overflow".into()))?;
            last_event_id = new_identifier();
            let chunk_hash = hex::encode(Sha256::digest(chunk));
            let bytes = BASE64_STANDARD.encode(chunk);
            let payload = ReplacementChunk {
                replacement_id: &replacement_id,
                chunk_index: index as u64,
                raw_bytes: chunk.len() as u64,
                chunk_sha256: &chunk_hash,
                bytes: &bytes,
            };
            write_event(
                &mut log,
                &mut log_hash,
                EventHeader {
                    generation: &generation,
                    seq,
                    event_id: &last_event_id,
                    timestamp_ms,
                    kind: "state_replacement_chunk",
                },
                &payload,
            )?;
        }

        seq = seq
            .checked_add(1)
            .ok_or_else(|| SessionStoreError::Corrupt("event sequence overflow".into()))?;
        last_event_id = new_identifier();
        let committed = ReplacementCommitted {
            replacement_id: &replacement_id,
            encoded_bytes: encoded.len() as u64,
            sha256: &state_hash_hex,
            chunk_count,
        };
        write_event(
            &mut log,
            &mut log_hash,
            EventHeader {
                generation: &generation,
                seq,
                event_id: &last_event_id,
                timestamp_ms,
                kind: "state_replacement_committed",
            },
            &committed,
        )?;
        log.sync_all()
            .map_err(|error| unavailable(&events_path, error))?;
        let event_log_bytes = log
            .stream_position()
            .map_err(|error| unavailable(&events_path, error))?;
        let watermark = Watermark {
            schema_version: WATERMARK_SCHEMA_VERSION,
            session_id: stored.id.clone(),
            log_generation: generation,
            last_event_seq: seq,
            last_event_id,
            event_log_bytes,
            event_log_sha256: hex::encode(log_hash.finalize()),
        };
        atomic_write_json(&watermark_path, &watermark, MAX_WATERMARK_BYTES)?;

        // The watermark made the new state durable. A missing/stale manifest is
        // recoverable and load/list will replay the canonical prefix.
        let _ = write_manifest_best_effort(&directory, &stored, &watermark);
        Ok(())
    }

    fn list_sync(
        &self,
        workspace_root: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SessionSummary>, SessionStoreError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(unavailable(&self.root, error)),
        };
        let expected_workspace = workspace_root.map(normalize_workspace);
        let mut summaries = Vec::new();
        for entry in entries {
            let Ok(entry) = entry else { continue };
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() || file_type.is_symlink() {
                continue;
            }
            let Some(id) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if validate_session_id(&id).is_err() {
                continue;
            }
            let Ok(session) = self.load_id(&id) else {
                continue;
            };
            if expected_workspace
                .as_deref()
                .is_some_and(|root| normalize_workspace(&session.workspace_root) != root)
            {
                continue;
            }
            summaries.push(summary(&session));
        }
        summaries.sort_by(|left, right| {
            right
                .updated_at_ms
                .cmp(&left.updated_at_ms)
                .then_with(|| right.id.cmp(&left.id))
        });
        summaries.truncate(limit);
        Ok(summaries)
    }

    fn session_dir(&self, id: &str) -> PathBuf {
        self.root.join(id)
    }
}

impl SessionStore for EventLogSessionStore {
    fn load<'a>(
        &'a self,
        target: SessionTarget,
        workspace_root: &'a str,
    ) -> BoxFuture<'a, Result<Session, SessionStoreError>> {
        Box::pin(async move {
            match target {
                SessionTarget::Id(id) => self.load_id(&id),
                SessionTarget::Last => self
                    .list_sync(Some(workspace_root), 1)?
                    .into_iter()
                    .next()
                    .ok_or_else(|| SessionStoreError::NotFound("last".into()))
                    .and_then(|summary| self.load_id(&summary.id)),
            }
        })
    }

    fn save<'a>(&'a self, session: &'a Session) -> BoxFuture<'a, Result<(), SessionStoreError>> {
        Box::pin(async move { self.save_sync(session) })
    }

    fn list<'a>(
        &'a self,
        workspace_root: Option<&'a str>,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<SessionSummary>, SessionStoreError>> {
        Box::pin(async move { self.list_sync(workspace_root, limit) })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Watermark {
    schema_version: u32,
    session_id: String,
    log_generation: String,
    last_event_seq: u64,
    last_event_id: String,
    event_log_bytes: u64,
    event_log_sha256: String,
}

#[derive(Debug, Deserialize)]
struct RawEnvelope {
    schema_version: u32,
    log_generation: String,
    seq: u64,
    event_id: String,
    timestamp_ms: i64,
    kind: String,
    payload: Value,
}

#[derive(Serialize)]
struct Envelope<'a, T> {
    schema_version: u32,
    log_generation: &'a str,
    seq: u64,
    event_id: &'a str,
    timestamp_ms: i64,
    kind: &'a str,
    payload: &'a T,
}

struct EventHeader<'a> {
    generation: &'a str,
    seq: u64,
    event_id: &'a str,
    timestamp_ms: i64,
    kind: &'a str,
}

#[derive(Serialize)]
struct ReplacementStarted<'a> {
    replacement_id: &'a str,
    encoded_bytes: u64,
    sha256: &'a str,
    chunk_count: u64,
}

#[derive(Deserialize, Serialize)]
struct ReplacementChunk<'a> {
    replacement_id: &'a str,
    chunk_index: u64,
    raw_bytes: u64,
    chunk_sha256: &'a str,
    bytes: &'a str,
}

#[derive(Serialize)]
struct ReplacementCommitted<'a> {
    replacement_id: &'a str,
    encoded_bytes: u64,
    sha256: &'a str,
    chunk_count: u64,
}

#[derive(Deserialize)]
struct OwnedReplacementStarted {
    replacement_id: String,
    encoded_bytes: u64,
    sha256: String,
    chunk_count: u64,
}

#[derive(Deserialize)]
struct OwnedReplacementChunk {
    replacement_id: String,
    chunk_index: u64,
    raw_bytes: u64,
    chunk_sha256: String,
    bytes: String,
}

#[derive(Deserialize)]
struct OwnedReplacementCommitted {
    replacement_id: String,
    encoded_bytes: u64,
    sha256: String,
    chunk_count: u64,
}

#[derive(Debug, Serialize)]
struct Manifest<'a> {
    schema_version: u32,
    storage_format: &'static str,
    id: &'a str,
    created_at_ms: i64,
    updated_at_ms: i64,
    origin_workspace_root: &'a str,
    workspace_root: &'a str,
    title: Option<&'a str>,
    preview: Option<String>,
    history_len: usize,
    has_managed_children: bool,
    log_generation: &'a str,
    last_event_seq: u64,
    event_log_bytes: u64,
    event_log_sha256: &'a str,
}

struct PendingReplacement {
    id: String,
    encoded_bytes: usize,
    sha256: [u8; 32],
    chunk_count: usize,
    chunks: Vec<u8>,
    next_chunk: usize,
}

struct ReplayOutcome {
    session: Session,
    watermark: Watermark,
}

fn replay_committed(
    directory: &Path,
    expected_id: &str,
) -> Result<ReplayOutcome, SessionStoreError> {
    let watermark_path = directory.join(WATERMARK_FILE);
    reject_unsafe_file(&watermark_path)?;
    let watermark: Watermark = read_json_bounded(&watermark_path, MAX_WATERMARK_BYTES)?;
    validate_watermark(&watermark, expected_id)?;
    let events_path = directory.join(EVENTS_FILE);
    reject_unsafe_file(&events_path)?;
    let log = File::open(&events_path).map_err(|error| unavailable(&events_path, error))?;
    let metadata = log
        .metadata()
        .map_err(|error| unavailable(&events_path, error))?;
    if !metadata.is_file() || metadata.len() < watermark.event_log_bytes {
        return Err(SessionStoreError::Corrupt(
            "event log does not contain the committed prefix".into(),
        ));
    }

    let mut reader = BufReader::new(log.take(watermark.event_log_bytes));
    let mut digest = Sha256::new();
    let mut consumed = 0u64;
    let mut next_seq = 1u64;
    let mut last_event_id = zero_identifier();
    let mut pending: Option<PendingReplacement> = None;
    let mut session: Option<Session> = None;
    loop {
        let mut line = Vec::new();
        let count = reader
            .read_until(b'\n', &mut line)
            .map_err(|error| unavailable(&events_path, error))?;
        if count == 0 {
            break;
        }
        if line.len() > EVENT_FRAME_MAX_BYTES || !line.ends_with(b"\n") {
            return Err(SessionStoreError::Corrupt(
                "invalid event frame boundary".into(),
            ));
        }
        consumed = consumed
            .checked_add(line.len() as u64)
            .ok_or_else(|| SessionStoreError::Corrupt("event byte offset overflow".into()))?;
        digest.update(&line);
        let envelope: RawEnvelope = serde_json::from_slice(&line[..line.len() - 1])
            .map_err(|error| SessionStoreError::Corrupt(format!("event frame: {error}")))?;
        if envelope.schema_version != EVENT_SCHEMA_VERSION
            || envelope.log_generation != watermark.log_generation
            || envelope.seq != next_seq
            || !valid_identifier(&envelope.event_id)
            || envelope.timestamp_ms < 0
        {
            return Err(SessionStoreError::Corrupt(
                "invalid event identity or sequence".into(),
            ));
        }
        next_seq = next_seq
            .checked_add(1)
            .ok_or_else(|| SessionStoreError::Corrupt("event sequence overflow".into()))?;
        last_event_id = envelope.event_id;
        apply_event(&mut pending, &mut session, &envelope.kind, envelope.payload)?;
    }
    if consumed != watermark.event_log_bytes
        || next_seq.saturating_sub(1) != watermark.last_event_seq
        || last_event_id != watermark.last_event_id
        || hex::encode(digest.finalize()) != watermark.event_log_sha256
        || pending.is_some()
    {
        return Err(SessionStoreError::Corrupt(
            "event log does not match its committed watermark".into(),
        ));
    }
    let mut session = session.ok_or_else(|| {
        SessionStoreError::Corrupt("event log has no committed session state".into())
    })?;
    if session.id != expected_id {
        return Err(SessionStoreError::Corrupt(
            "session state id does not match its directory".into(),
        ));
    }
    session.schema_version = SCHEMA_VERSION;
    Ok(ReplayOutcome { session, watermark })
}

fn apply_event(
    pending: &mut Option<PendingReplacement>,
    session: &mut Option<Session>,
    kind: &str,
    payload: Value,
) -> Result<(), SessionStoreError> {
    match kind {
        "state_replacement_started" => {
            if pending.is_some() {
                return Err(SessionStoreError::Corrupt(
                    "nested state replacement".into(),
                ));
            }
            let payload: OwnedReplacementStarted =
                serde_json::from_value(payload).map_err(|error| {
                    SessionStoreError::Corrupt(format!("replacement start: {error}"))
                })?;
            let encoded_bytes = usize::try_from(payload.encoded_bytes)
                .ok()
                .filter(|bytes| *bytes > 0 && *bytes <= MAX_STATE_BYTES)
                .ok_or_else(|| SessionStoreError::Corrupt("invalid replacement size".into()))?;
            let chunk_count = usize::try_from(payload.chunk_count)
                .ok()
                .filter(|count| *count == encoded_bytes.div_ceil(RAW_STATE_CHUNK_BYTES))
                .ok_or_else(|| SessionStoreError::Corrupt("invalid replacement chunks".into()))?;
            *pending = Some(PendingReplacement {
                id: checked_identifier(payload.replacement_id)?,
                encoded_bytes,
                sha256: decode_digest(&payload.sha256)?,
                chunk_count,
                chunks: Vec::with_capacity(encoded_bytes),
                next_chunk: 0,
            });
        }
        "state_replacement_chunk" => {
            let payload: OwnedReplacementChunk =
                serde_json::from_value(payload).map_err(|error| {
                    SessionStoreError::Corrupt(format!("replacement chunk: {error}"))
                })?;
            let pending = pending.as_mut().ok_or_else(|| {
                SessionStoreError::Corrupt("replacement chunk without start".into())
            })?;
            if payload.replacement_id != pending.id
                || payload.chunk_index != pending.next_chunk as u64
                || pending.next_chunk >= pending.chunk_count
            {
                return Err(SessionStoreError::Corrupt(
                    "replacement chunk identity or order mismatch".into(),
                ));
            }
            let decoded = BASE64_STANDARD.decode(payload.bytes).map_err(|error| {
                SessionStoreError::Corrupt(format!("replacement chunk base64: {error}"))
            })?;
            if decoded.is_empty()
                || decoded.len() > RAW_STATE_CHUNK_BYTES
                || decoded.len() as u64 != payload.raw_bytes
                || decode_digest(&payload.chunk_sha256)? != Sha256::digest(&decoded).as_slice()
                || pending.chunks.len().saturating_add(decoded.len()) > pending.encoded_bytes
            {
                return Err(SessionStoreError::Corrupt(
                    "replacement chunk content mismatch".into(),
                ));
            }
            pending.chunks.extend_from_slice(&decoded);
            pending.next_chunk += 1;
        }
        "state_replacement_committed" => {
            let payload: OwnedReplacementCommitted =
                serde_json::from_value(payload).map_err(|error| {
                    SessionStoreError::Corrupt(format!("replacement commit: {error}"))
                })?;
            let replacement = pending.take().ok_or_else(|| {
                SessionStoreError::Corrupt("replacement commit without start".into())
            })?;
            if payload.replacement_id != replacement.id
                || payload.encoded_bytes != replacement.encoded_bytes as u64
                || payload.chunk_count != replacement.chunk_count as u64
                || decode_digest(&payload.sha256)? != replacement.sha256
                || replacement.next_chunk != replacement.chunk_count
                || replacement.chunks.len() != replacement.encoded_bytes
                || Sha256::digest(&replacement.chunks).as_slice() != replacement.sha256
            {
                return Err(SessionStoreError::Corrupt(
                    "replacement commit content mismatch".into(),
                ));
            }
            let decoded: Session = serde_json::from_slice(&replacement.chunks)
                .map_err(|error| SessionStoreError::Corrupt(format!("session state: {error}")))?;
            validate_session(&decoded)?;
            *session = Some(decoded);
        }
        other => {
            return Err(SessionStoreError::Corrupt(format!(
                "unsupported event kind `{other}`"
            )));
        }
    }
    Ok(())
}

fn write_event<T: Serialize>(
    file: &mut File,
    hash: &mut Sha256,
    header: EventHeader<'_>,
    payload: &T,
) -> Result<(), SessionStoreError> {
    let envelope = Envelope {
        schema_version: EVENT_SCHEMA_VERSION,
        log_generation: header.generation,
        seq: header.seq,
        event_id: header.event_id,
        timestamp_ms: header.timestamp_ms,
        kind: header.kind,
        payload,
    };
    let mut frame = serde_json::to_vec(&envelope)
        .map_err(|error| SessionStoreError::Corrupt(format!("encode event: {error}")))?;
    frame.push(b'\n');
    if frame.len() > EVENT_FRAME_MAX_BYTES {
        return Err(SessionStoreError::Unavailable(
            "encoded event exceeds the 8 MiB frame limit".into(),
        ));
    }
    file.write_all(&frame)
        .map_err(|error| SessionStoreError::Unavailable(format!("append event log: {error}")))?;
    hash.update(&frame);
    Ok(())
}

fn write_manifest_best_effort(
    directory: &Path,
    session: &Session,
    watermark: &Watermark,
) -> Result<(), SessionStoreError> {
    let origin = session
        .origin_workspace_root
        .as_deref()
        .unwrap_or(&session.workspace_root);
    let manifest = Manifest {
        schema_version: SCHEMA_VERSION,
        storage_format: STORAGE_FORMAT,
        id: &session.id,
        created_at_ms: session.created_at_ms,
        updated_at_ms: session.updated_at_ms,
        origin_workspace_root: origin,
        workspace_root: &session.workspace_root,
        title: session.title.as_deref(),
        preview: preview(session),
        history_len: session.history.len(),
        has_managed_children: false,
        log_generation: &watermark.log_generation,
        last_event_seq: watermark.last_event_seq,
        event_log_bytes: watermark.event_log_bytes,
        event_log_sha256: &watermark.event_log_sha256,
    };
    atomic_write_json(
        &directory.join(MANIFEST_FILE),
        &manifest,
        MAX_MANIFEST_BYTES,
    )
}

fn atomic_write_json(
    path: &Path,
    value: &impl Serialize,
    limit: usize,
) -> Result<(), SessionStoreError> {
    reject_unsafe_file(path)?;
    let bytes = serde_json::to_vec(value)
        .map_err(|error| SessionStoreError::Corrupt(format!("encode projection: {error}")))?;
    if bytes.is_empty() || bytes.len() > limit {
        return Err(SessionStoreError::Corrupt(
            "encoded projection exceeds its size limit".into(),
        ));
    }
    let mut stage = AtomicWriteFile::open(path).map_err(|error| unavailable(path, error))?;
    set_private_file(stage.as_file(), path)?;
    stage
        .write_all(&bytes)
        .map_err(|error| unavailable(path, error))?;
    stage.commit().map_err(|error| unavailable(path, error))
}

fn hash_file_prefix(path: &Path, bytes: u64) -> Result<Sha256, SessionStoreError> {
    let mut hash = Sha256::new();
    if bytes == 0 {
        return Ok(hash);
    }
    let file = File::open(path).map_err(|error| unavailable(path, error))?;
    let mut source = file.take(bytes);
    let copied = std::io::copy(&mut source, &mut HashWriter(&mut hash))
        .map_err(|error| unavailable(path, error))?;
    if copied != bytes {
        return Err(SessionStoreError::Corrupt(
            "event log is shorter than its committed watermark".into(),
        ));
    }
    Ok(hash)
}

struct HashWriter<'a>(&'a mut Sha256);

impl Write for HashWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn read_json_bounded<T: for<'de> Deserialize<'de>>(
    path: &Path,
    limit: usize,
) -> Result<T, SessionStoreError> {
    let mut file = File::open(path).map_err(|error| unavailable(path, error))?;
    let metadata = file.metadata().map_err(|error| unavailable(path, error))?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > limit as u64 {
        return Err(SessionStoreError::Corrupt(format!(
            "invalid bounded file at {}",
            path.display()
        )));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.read_to_end(&mut bytes)
        .map_err(|error| unavailable(path, error))?;
    serde_json::from_slice(&bytes)
        .map_err(|error| SessionStoreError::Corrupt(format!("{}: {error}", path.display())))
}

fn load_legacy(path: &Path, expected_id: &str) -> Result<Session, SessionStoreError> {
    load_legacy_optional(path, expected_id)?
        .ok_or_else(|| SessionStoreError::NotFound(expected_id.to_owned()))
}

fn load_legacy_optional(
    path: &Path,
    expected_id: &str,
) -> Result<Option<Session>, SessionStoreError> {
    if !path_is_file(path)? {
        return Ok(None);
    }
    reject_unsafe_file(path)?;
    let value: Value = read_json_bounded(path, MAX_STATE_BYTES)?;
    let schema = value
        .get("schema_version")
        .and_then(Value::as_u64)
        .ok_or_else(|| SessionStoreError::Corrupt("legacy schema is missing".into()))?;
    if schema == SCHEMA_VERSION as u64 {
        return Err(SessionStoreError::Corrupt(
            "schema-v3 manifest is missing its commit watermark".into(),
        ));
    }
    if !matches!(schema, 1 | 2) {
        return Err(SessionStoreError::UnsupportedSchema(schema as u32));
    }
    let mut session: Session = serde_json::from_value(value)
        .map_err(|error| SessionStoreError::Corrupt(format!("legacy session: {error}")))?;
    if session.id != expected_id {
        return Err(SessionStoreError::Corrupt(
            "legacy session id does not match its directory".into(),
        ));
    }
    validate_session(&session)?;
    session.schema_version = SCHEMA_VERSION;
    Ok(Some(session))
}

fn validate_session(session: &Session) -> Result<(), SessionStoreError> {
    validate_session_id(&session.id)?;
    if !matches!(session.schema_version, 1 | 2 | SCHEMA_VERSION) {
        return Err(SessionStoreError::UnsupportedSchema(session.schema_version));
    }
    if session.created_at_ms < 0
        || session.updated_at_ms < session.created_at_ms
        || !Path::new(&session.workspace_root).is_absolute()
        || session.workspace_root.len() > 4096
        || session
            .origin_workspace_root
            .as_deref()
            .is_some_and(|root| !Path::new(root).is_absolute() || root.len() > 4096)
        || session
            .preferences
            .model
            .as_deref()
            .is_some_and(|model| !valid_model_preference(model))
    {
        return Err(SessionStoreError::Corrupt(
            "session contains invalid durable fields".into(),
        ));
    }
    Ok(())
}

fn valid_model_preference(model: &str) -> bool {
    let bytes = model.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 1024
        && !bytes[0].is_ascii_whitespace()
        && !bytes[bytes.len() - 1].is_ascii_whitespace()
        && !bytes.iter().any(u8::is_ascii_control)
}

fn validate_replacement(previous: &Session, next: &Session) -> Result<(), SessionStoreError> {
    if previous.id != next.id
        || previous.created_at_ms != next.created_at_ms
        || previous.origin_workspace_root != next.origin_workspace_root
        || next.updated_at_ms < previous.updated_at_ms
    {
        return Err(SessionStoreError::Corrupt(
            "session replacement changes immutable identity or time ordering".into(),
        ));
    }
    Ok(())
}

fn redact_durable_session(session: &Session) -> Session {
    let mut stored = session.clone();
    if let Some(title) = &mut stored.title {
        *title = fx_core::redact_secrets(title).into_owned();
    }

    let mut identifiers = HashMap::new();
    for message in &stored.history {
        if let Some(id) = &message.tool_call_id {
            retain_sensitive_identifier(&mut identifiers, id);
        }
        for call in &message.tool_calls {
            retain_sensitive_identifier(&mut identifiers, &call.id);
            if let Some(id) = &call.provisional_id {
                retain_sensitive_identifier(&mut identifiers, id);
            }
        }
    }

    for message in &mut stored.history {
        if let Some(content) = &mut message.content {
            *content = fx_core::redact_secrets(content).into_owned();
        }
        if let Some(id) = &mut message.tool_call_id
            && let Some(replacement) = identifiers.get(id)
        {
            *id = replacement.clone();
        }
        for call in &mut message.tool_calls {
            if let Some(replacement) = identifiers.get(&call.id) {
                call.id = replacement.clone();
            }
            if let Some(id) = &mut call.provisional_id
                && let Some(replacement) = identifiers.get(id)
            {
                *id = replacement.clone();
            }
            call.arguments_json = fx_core::redact_secrets(&call.arguments_json).into_owned();
            if let Some(result) = &mut call.provider_result {
                *result = fx_core::redact_secrets(result).into_owned();
            }
        }
    }
    stored
}

fn retain_sensitive_identifier(identifiers: &mut HashMap<String, String>, identifier: &str) {
    if identifiers.contains_key(identifier)
        || matches!(
            fx_core::redact_secrets(identifier),
            std::borrow::Cow::Borrowed(_)
        )
    {
        return;
    }
    let digest = Sha256::digest(identifier.as_bytes());
    identifiers.insert(
        identifier.to_owned(),
        format!("call_{}", hex::encode(&digest[..16])),
    );
}

fn validate_watermark(watermark: &Watermark, expected_id: &str) -> Result<(), SessionStoreError> {
    if watermark.schema_version != WATERMARK_SCHEMA_VERSION
        || watermark.session_id != expected_id
        || !valid_identifier(&watermark.log_generation)
        || !valid_identifier(&watermark.last_event_id)
        || watermark.last_event_seq == 0
        || watermark.event_log_bytes == 0
        || decode_digest(&watermark.event_log_sha256).is_err()
    {
        return Err(SessionStoreError::Corrupt(
            "invalid commit watermark".into(),
        ));
    }
    Ok(())
}

pub fn validate_session_id(id: &str) -> Result<(), SessionStoreError> {
    if id.is_empty()
        || id.len() > 255
        || matches!(id, "." | "..")
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(SessionStoreError::InvalidId(id.to_owned()));
    }
    Ok(())
}

fn summary(session: &Session) -> SessionSummary {
    SessionSummary {
        id: session.id.clone(),
        workspace_root: Some(session.workspace_root.clone()),
        origin_workspace_root: session.origin_workspace_root.clone(),
        title: session.title.clone(),
        preview: preview(session),
        created_at_ms: session.created_at_ms,
        updated_at_ms: session.updated_at_ms,
        history_len: session.history.len(),
        has_managed_children: false,
    }
}

fn preview(session: &Session) -> Option<String> {
    let source = session.title.as_deref().or_else(|| {
        session
            .history
            .iter()
            .filter_map(|message| message.content.as_deref())
            .find(|content| !content.trim().is_empty())
    })?;
    Some(source.chars().take(200).collect())
}

fn normalize_workspace(root: &str) -> String {
    let trimmed = root.trim_end_matches(std::path::MAIN_SEPARATOR);
    if trimmed.is_empty() {
        std::path::MAIN_SEPARATOR.to_string()
    } else {
        trimmed.to_owned()
    }
}

fn new_identifier() -> String {
    Uuid::new_v4().simple().to_string()
}

fn zero_identifier() -> String {
    "00000000000000000000000000000000".into()
}

fn checked_identifier(value: String) -> Result<String, SessionStoreError> {
    if valid_identifier(&value) {
        Ok(value)
    } else {
        Err(SessionStoreError::Corrupt(
            "invalid event identifier".into(),
        ))
    }
}

fn valid_identifier(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn decode_digest(value: &str) -> Result<[u8; 32], SessionStoreError> {
    let mut digest = [0u8; 32];
    hex::decode_to_slice(value, &mut digest)
        .map_err(|_| SessionStoreError::Corrupt("invalid SHA-256 digest".into()))?;
    Ok(digest)
}

fn prior_watermark(existing: Option<&ReplayOutcome>) -> Watermark {
    existing
        .expect("prior watermark only requested for an existing session")
        .watermark
        .clone()
}

fn ensure_private_directory(path: &Path) -> Result<(), SessionStoreError> {
    fs::create_dir_all(path).map_err(|error| unavailable(path, error))?;
    ensure_existing_directory(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|error| unavailable(path, error))?;
    }
    Ok(())
}

fn ensure_existing_directory(path: &Path) -> Result<(), SessionStoreError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            SessionStoreError::NotFound(path.display().to_string())
        } else {
            unavailable(path, error)
        }
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(SessionStoreError::Corrupt(format!(
            "session path is not a real directory: {}",
            path.display()
        )));
    }
    Ok(())
}

fn reject_unsafe_file(path: &Path) -> Result<(), SessionStoreError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(SessionStoreError::Corrupt(format!(
                "session file is not a regular file: {}",
                path.display()
            )))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(unavailable(path, error)),
    }
}

fn path_is_file(path: &Path) -> Result<bool, SessionStoreError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(SessionStoreError::Corrupt(
            format!("session file is a symbolic link: {}", path.display()),
        )),
        Ok(metadata) => Ok(metadata.is_file()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(unavailable(path, error)),
    }
}

fn lock_session(directory: &Path) -> Result<File, SessionStoreError> {
    let path = directory.join(LOCK_FILE);
    reject_unsafe_file(&path)?;
    let file = open_private_file(&path, true)?;
    let deadline = Instant::now() + LOCK_TIMEOUT;
    loop {
        match fs4::FileExt::try_lock(&file) {
            Ok(()) => return Ok(file),
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(TryLockError::WouldBlock) => {
                return Err(SessionStoreError::Unavailable(format!(
                    "timed out waiting for session lock at {}",
                    path.display()
                )));
            }
            Err(TryLockError::Error(error)) => return Err(unavailable(&path, error)),
        }
    }
}

fn open_private_log(path: &Path) -> Result<File, SessionStoreError> {
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(path)
        .map_err(|error| unavailable(path, error))?;
    set_private_file(&file, path)?;
    Ok(file)
}

fn open_private_file(path: &Path, create: bool) -> Result<File, SessionStoreError> {
    let mut options = OpenOptions::new();
    options.create(create).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(path)
        .map_err(|error| unavailable(path, error))?;
    set_private_file(&file, path)?;
    Ok(file)
}

fn set_private_file(file: &File, path: &Path) -> Result<(), SessionStoreError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|error| unavailable(path, error))?;
    }
    #[cfg(not(unix))]
    let _ = (file, path);
    Ok(())
}

fn unavailable(path: &Path, error: std::io::Error) -> SessionStoreError {
    SessionStoreError::Unavailable(format!("{}: {error}", path.display()))
}

fn ensure_result_directory(path: &Path) -> Result<(), ToolResultStoreError> {
    let parent = path.parent().ok_or_else(|| {
        ToolResultStoreError::Unavailable("tool-result directory has no parent".into())
    })?;
    let parent_metadata = fs::symlink_metadata(parent).map_err(|error| result_io(parent, error))?;
    if parent_metadata.file_type().is_symlink() || !parent_metadata.is_dir() {
        return Err(ToolResultStoreError::Unavailable(format!(
            "session path is not a real directory: {}",
            parent.display()
        )));
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(ToolResultStoreError::Unavailable(format!(
                "tool-result path is not a real directory: {}",
                path.display()
            )));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(path).map_err(|error| result_io(path, error))?;
        }
        Err(error) => return Err(result_io(path, error)),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|error| result_io(path, error))?;
    }
    Ok(())
}

fn reject_unsafe_result_file(path: &Path) -> Result<(), ToolResultStoreError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(ToolResultStoreError::Unavailable(format!(
                "tool-result path is not a regular file: {}",
                path.display()
            )))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(result_io(path, error)),
    }
}

fn set_private_result_file(path: &Path) -> Result<(), ToolResultStoreError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|error| result_io(path, error))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn open_result_file(path: &Path) -> Result<File, ToolResultStoreError> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    options.open(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            ToolResultStoreError::NotFound
        } else {
            result_io(path, error)
        }
    })
}

fn result_handle(tool_call_id: &str, tool_name: &str, content: &str) -> String {
    let call_digest = Sha256::digest(tool_call_id.as_bytes());
    let content_digest = Sha256::digest(content.as_bytes());
    let safe_tool = safe_handle_part(tool_name);
    format!(
        "result-{safe_tool}-{}-{}.txt",
        hex::encode(&call_digest[..8]),
        hex::encode(&content_digest[..8])
    )
}

fn safe_handle_part(value: &str) -> String {
    let mut result = value
        .bytes()
        .take(48)
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-') {
                char::from(byte)
            } else {
                '-'
            }
        })
        .collect::<String>();
    if result.is_empty() {
        result.push_str("call");
    }
    result
}

fn validate_result_handle(handle: &str) -> Result<(), ToolResultStoreError> {
    if handle.is_empty()
        || handle.len() > 160
        || handle.contains("..")
        || !handle
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        return Err(ToolResultStoreError::InvalidHandle);
    }
    Ok(())
}

fn result_io(path: &Path, error: std::io::Error) -> ToolResultStoreError {
    ToolResultStoreError::Unavailable(format!("{}: {error}", path.display()))
}

fn result_tool_error(handle: &str, error: ToolResultStoreError) -> ToolError {
    match error {
        ToolResultStoreError::NotFound => ToolError::Execution(format!(
            "read_tool_result failed for handle {handle}: no exact match exists in the active session store; copy the handle exactly from the tool result preview"
        )),
        error => ToolError::Execution(format!(
            "read_tool_result failed for handle {handle}: {error}"
        )),
    }
}

fn utf8_prefix(value: &str, max_bytes: usize) -> &str {
    let mut end = value.len().min(max_bytes);
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

fn utf8_forward_boundary(value: &str, mut offset: usize) -> usize {
    while offset < value.len() && !value.is_char_boundary(offset) {
        offset += 1;
    }
    offset
}

fn utf8_backward_boundary(value: &str, mut offset: usize) -> usize {
    while offset > 0 && !value.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

#[cfg(test)]
mod tests {
    use super::*;
    use fx_core::{ChatMessage, Role, ToolArgumentIntegrity, ToolCall, ToolExecutionProvenance};

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("fx-store-test-{}", new_identifier()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn session(id: &str, workspace: &Path, updated: i64, text: &str) -> Session {
        Session {
            schema_version: SCHEMA_VERSION,
            id: id.into(),
            created_at_ms: 10,
            updated_at_ms: updated,
            workspace_root: workspace.display().to_string(),
            origin_workspace_root: None,
            title: None,
            preferences: fx_core::SessionPreferences::default(),
            history: vec![ChatMessage::text(Role::User, text)],
        }
    }

    #[test]
    fn tool_results_are_session_scoped_stable_and_utf8_bounded() {
        let directory = TestDirectory::new();
        let workspace = directory.0.join("workspace");
        let sessions = EventLogSessionStore::new(directory.0.join("sessions"));
        pollster::block_on(sessions.save(&session("result-session", &workspace, 20, "go")))
            .unwrap();
        let results = sessions.tool_result_store("result-session").unwrap();
        let text = format!("é first\nneedle line\n{}", "x".repeat(20_000));

        let first = results
            .store("sk-secret-call-id", "mcp/server", &text)
            .unwrap();
        let again = results
            .store("sk-secret-call-id", "mcp/server", &text)
            .unwrap();
        assert_eq!(first, again);
        assert!(!first.handle.contains("secret"));
        assert_eq!(first.stored_bytes, text.len());

        let page = results.read_range(&first.handle, 2, 8).unwrap();
        assert!(page.content.is_char_boundary(0));
        assert_eq!(page.start_byte, 3);
        assert!(page.content.contains(" first"));
        assert_eq!(page.total_bytes, text.len());

        let matches = results.search(&first.handle, "needle", 50).unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].line, 2);
        assert_eq!(matches[0].content, "needle line");
        assert_eq!(
            results.read_range("../escape", 1, 10),
            Err(ToolResultStoreError::InvalidHandle)
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(results.directory())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(results.directory().join(&first.handle))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn read_tool_result_supports_range_and_literal_query() {
        let directory = TestDirectory::new();
        let workspace = directory.0.join("workspace");
        let sessions = EventLogSessionStore::new(directory.0.join("sessions"));
        pollster::block_on(sessions.save(&session("reader", &workspace, 20, "go"))).unwrap();
        let results = std::sync::Arc::new(sessions.tool_result_store("reader").unwrap());
        let stored = results
            .store("call", "web_fetch", "alpha\nbeta needle\ngamma")
            .unwrap();
        let tool = ReadToolResult::new(results);
        let context = ToolContext::new(workspace);

        let range = pollster::block_on(tool.execute(
            &context,
            serde_json::json!({"handle": stored.handle.clone(), "start_byte": 1, "byte_count": 5}),
        ))
        .unwrap();
        assert!(range.content.contains("alpha"));
        assert!(range.content.contains("total_bytes=\"23\""));

        let query = pollster::block_on(tool.execute(
            &context,
            serde_json::json!({"handle": stored.handle, "query": "needle"}),
        ))
        .unwrap();
        assert!(query.content.contains("2|beta needle"));
    }

    #[test]
    fn durable_sessions_mask_secrets_and_preserve_tool_call_pairing() {
        let directory = TestDirectory::new();
        let workspace = directory.0.join("workspace");
        fs::create_dir(&workspace).unwrap();
        let store = EventLogSessionStore::new(directory.0.join("sessions"));
        let secret_id = "sk-tool-call-abcdefghijklmnop";
        let mut value = session("redacted", &workspace, 20, "API_KEY=user-private-value");
        value.title = Some("TOKEN=title-private-value".into());
        value.history.push(ChatMessage {
            role: Role::Assistant,
            content: None,
            tool_call_id: None,
            tool_name: None,
            tool_calls: vec![ToolCall {
                id: secret_id.into(),
                name: "terminal".into(),
                arguments_json: r#"{"command":"TOKEN=argument-private-value"}"#.into(),
                argument_integrity: ToolArgumentIntegrity::Valid,
                provisional_id: Some(secret_id.into()),
                provider_result: Some(r#"{"api_key":"provider-private-value"}"#.into()),
                provenance: ToolExecutionProvenance::FxLocal,
            }],
            permission_feedback: false,
            cache_policy: fx_core::CachePolicy::Default,
        });
        value.history.push(ChatMessage {
            role: Role::Tool,
            content: Some("PASSWORD=result-private-value".into()),
            tool_call_id: Some(secret_id.into()),
            tool_name: Some("terminal".into()),
            tool_calls: Vec::new(),
            permission_feedback: false,
            cache_policy: fx_core::CachePolicy::Default,
        });

        pollster::block_on(store.save(&value)).unwrap();
        let loaded = pollster::block_on(store.load(
            SessionTarget::Id("redacted".into()),
            &workspace.display().to_string(),
        ))
        .unwrap();
        let encoded = serde_json::to_string(&loaded).unwrap();
        for secret in [
            "user-private-value",
            "title-private-value",
            "argument-private-value",
            "provider-private-value",
            "result-private-value",
            secret_id,
        ] {
            assert!(!encoded.contains(secret));
        }
        let call = &loaded.history[1].tool_calls[0];
        assert_eq!(call.id, call.provisional_id.as_deref().unwrap());
        assert_eq!(
            loaded.history[2].tool_call_id.as_deref(),
            Some(call.id.as_str())
        );
        assert!(encoded.contains("[redacted]"));
    }

    #[test]
    fn saves_loads_lists_and_selects_latest_by_workspace() {
        let directory = TestDirectory::new();
        let workspace = directory.0.join("workspace");
        let store = EventLogSessionStore::new(directory.0.join("sessions"));
        let first = session("first", &workspace, 20, "first prompt");
        let second = session("second", &workspace, 30, "second prompt");
        pollster::block_on(store.save(&first)).unwrap();
        pollster::block_on(store.save(&second)).unwrap();

        assert_eq!(
            pollster::block_on(store.load(SessionTarget::Id("first".into()), "ignored")).unwrap(),
            first
        );
        let listed =
            pollster::block_on(store.list(Some(&workspace.display().to_string()), 10)).unwrap();
        assert_eq!(
            listed
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            ["second", "first"]
        );
        assert_eq!(listed[0].preview.as_deref(), Some("second prompt"));
        assert_eq!(
            pollster::block_on(store.load(SessionTarget::Last, &workspace.display().to_string()))
                .unwrap()
                .id,
            "second"
        );
    }

    #[test]
    fn uncommitted_tail_is_ignored_then_removed_before_next_append() {
        let directory = TestDirectory::new();
        let workspace = directory.0.join("workspace");
        let store = EventLogSessionStore::new(directory.0.join("sessions"));
        let first = session("tail", &workspace, 20, "one");
        pollster::block_on(store.save(&first)).unwrap();
        let log_path = store.session_dir("tail").join(EVENTS_FILE);
        let mut log = OpenOptions::new().append(true).open(&log_path).unwrap();
        log.write_all(b"uncommitted garbage").unwrap();
        log.sync_all().unwrap();
        assert_eq!(store.load_id("tail").unwrap(), first);

        let second = session("tail", &workspace, 21, "two");
        pollster::block_on(store.save(&second)).unwrap();
        assert_eq!(store.load_id("tail").unwrap(), second);
        assert!(
            !fs::read(log_path)
                .unwrap()
                .windows(b"uncommitted garbage".len())
                .any(|window| window == b"uncommitted garbage")
        );
    }

    #[test]
    fn canonical_log_recovers_when_manifest_projection_is_missing() {
        let directory = TestDirectory::new();
        let workspace = directory.0.join("workspace");
        let store = EventLogSessionStore::new(directory.0.join("sessions"));
        let value = session("recover", &workspace, 20, "survives");
        pollster::block_on(store.save(&value)).unwrap();
        fs::remove_file(store.session_dir("recover").join(MANIFEST_FILE)).unwrap();

        assert_eq!(store.load_id("recover").unwrap(), value);
        assert_eq!(
            pollster::block_on(store.list(None, 10)).unwrap()[0].id,
            "recover"
        );
    }

    #[test]
    fn legacy_snapshot_is_read_and_migrated_on_save() {
        let directory = TestDirectory::new();
        let workspace = directory.0.join("workspace");
        let store = EventLogSessionStore::new(directory.0.join("sessions"));
        let session_dir = store.session_dir("legacy");
        fs::create_dir_all(&session_dir).unwrap();
        let mut legacy = session("legacy", &workspace, 20, "old");
        legacy.schema_version = 1;
        fs::write(
            session_dir.join(MANIFEST_FILE),
            serde_json::to_vec(&legacy).unwrap(),
        )
        .unwrap();

        let loaded = store.load_id("legacy").unwrap();
        assert_eq!(loaded.schema_version, SCHEMA_VERSION);
        let mut changed = loaded;
        changed.updated_at_ms = 21;
        changed
            .history
            .push(ChatMessage::text(Role::Assistant, "new"));
        pollster::block_on(store.save(&changed)).unwrap();
        assert_eq!(store.load_id("legacy").unwrap(), changed);
        assert!(session_dir.join(WATERMARK_FILE).is_file());
    }

    #[test]
    fn rejects_unsafe_ids_and_tampered_committed_bytes() {
        assert!(matches!(
            validate_session_id("../escape"),
            Err(SessionStoreError::InvalidId(_))
        ));
        let directory = TestDirectory::new();
        let workspace = directory.0.join("workspace");
        let store = EventLogSessionStore::new(directory.0.join("sessions"));
        pollster::block_on(store.save(&session("tamper", &workspace, 20, "one"))).unwrap();
        let path = store.session_dir("tamper").join(EVENTS_FILE);
        let mut bytes = fs::read(&path).unwrap();
        bytes[10] ^= 1;
        fs::write(path, bytes).unwrap();
        assert!(matches!(
            store.load_id("tamper"),
            Err(SessionStoreError::Corrupt(_))
        ));
    }
}
