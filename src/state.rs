use std::cell::RefCell;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use fs2::FileExt;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tempfile::NamedTempFile;
use uuid::Uuid;

use crate::approval::{ApprovalOutcome, ApprovalRequest};
use crate::config::{Config, ConfigPathResolver, ConfigScope};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredMessage {
    pub role: String,
    pub content: Option<String>,
    pub tool_calls: Option<String>,
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionInfo {
    pub id: String,
    pub title: String,
    pub created_at: String,
    pub updated_at: String,
    pub last_cwd: String,
}

#[derive(Debug, Clone)]
pub struct CheckpointInfo {
    pub id: String,
    pub tool: String,
    pub created_at: String,
    pub restored: bool,
    pub paths: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct CheckpointEntryRow {
    pub seq: i64,
    pub path: String,
    pub kind: String,
    pub related_path: Option<String>,
    pub existed_before: bool,
    pub snapshot_file: Option<String>,
    pub original_sha256: Option<String>,
}

#[derive(Debug, Clone)]
pub struct KnowledgeRow {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub source_uri: Option<String>,
    pub content: String,
    pub importance: f32,
}

#[derive(Debug, Clone)]
pub struct VectorRow {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub source_uri: Option<String>,
    pub importance: f32,
    pub chunk_content: String,
    pub embedding: Vec<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SequencedMessage {
    pub seq: i64,
    pub message: StoredMessage,
}

/// Append-only facts about an agent turn. Messages remain the materialized
/// conversation view; events are the durable execution record used for
/// observability and crash recovery.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionEvent {
    pub seq: i64,
    pub kind: EventKind,
    pub turn_id: String,
    pub tool_call_id: Option<String>,
    pub data: Value,
    pub created_at: String,
}

/// Stable event vocabulary for the append-only session log.
///
/// The database keeps the slash-separated names for backwards-compatible
/// inspection and JSON export, while Rust code can no longer silently ignore a
/// misspelled event kind.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum EventKind {
    #[serde(rename = "turn/start")]
    TurnStart,
    #[serde(rename = "turn/end")]
    TurnEnd,
    #[serde(rename = "user/message")]
    UserMessage,
    #[serde(rename = "assistant/message")]
    AssistantMessage,
    #[serde(rename = "request/header")]
    RequestHeader,
    #[serde(rename = "tool/call")]
    ToolCall,
    #[serde(rename = "tool/result")]
    ToolResult,
    #[serde(rename = "approval/asked")]
    ApprovalAsked,
    #[serde(rename = "approval/decided")]
    ApprovalDecided,
}

impl EventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TurnStart => "turn/start",
            Self::TurnEnd => "turn/end",
            Self::UserMessage => "user/message",
            Self::AssistantMessage => "assistant/message",
            Self::RequestHeader => "request/header",
            Self::ToolCall => "tool/call",
            Self::ToolResult => "tool/result",
            Self::ApprovalAsked => "approval/asked",
            Self::ApprovalDecided => "approval/decided",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "turn/start" => Ok(Self::TurnStart),
            "turn/end" => Ok(Self::TurnEnd),
            "user/message" => Ok(Self::UserMessage),
            "assistant/message" => Ok(Self::AssistantMessage),
            "request/header" => Ok(Self::RequestHeader),
            "tool/call" => Ok(Self::ToolCall),
            "tool/result" => Ok(Self::ToolResult),
            "approval/asked" => Ok(Self::ApprovalAsked),
            "approval/decided" => Ok(Self::ApprovalDecided),
            other => bail!("Unknown session event kind: {other}"),
        }
    }
}

/// Exact model-facing request envelope persisted before each model call.
/// `messages` and `tools` are JSON values rather than agent types so the
/// durable state layer stays independent from the model transport module.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RequestSnapshot {
    pub iteration: u32,
    pub model: String,
    pub max_tokens: u64,
    pub stream: bool,
    pub tool_choice: Option<String>,
    pub messages: Value,
    pub tools: Value,
    pub input_sha256: String,
}

#[derive(Serialize)]
struct RequestFingerprint<'a> {
    model: &'a str,
    messages: &'a Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<&'a Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<&'a str>,
    max_tokens: u64,
    stream: bool,
}

pub fn request_snapshot_hash(snapshot: &RequestSnapshot) -> Result<String> {
    let tools = (!snapshot.tools.as_array().is_some_and(Vec::is_empty)).then_some(&snapshot.tools);
    let fingerprint = RequestFingerprint {
        model: &snapshot.model,
        messages: &snapshot.messages,
        tools,
        tool_choice: snapshot.tool_choice.as_deref(),
        max_tokens: snapshot.max_tokens,
        stream: snapshot.stream,
    };
    Ok(sha256(&serde_json::to_string(&fingerprint)?))
}

struct TurnInvariant {
    user_message: bool,
    last_request_iteration: Option<u32>,
    request_pending: bool,
    assistant_call_ids: HashMap<String, ()>,
    call_ids: HashMap<String, ()>,
    result_ids: HashMap<String, ()>,
}

/// Checks the durable event state machine without mutating it.
///
/// An unfinished turn is allowed at the end of the log because a process can
/// die between two durable writes; `recover_session` closes that gap with
/// explicit interrupted results. A closed turn, however, must be balanced and
/// every request header must contain a complete model-visible envelope.
pub fn validate_session_events(events: &[SessionEvent]) -> Result<()> {
    let mut turns = HashMap::<String, TurnInvariant>::new();
    for (index, event) in events.iter().enumerate() {
        let expected_seq = index as i64 + 1;
        if event.seq != expected_seq {
            bail!(
                "Session event sequence is not contiguous: expected {expected_seq}, got {}",
                event.seq
            );
        }
        if event.turn_id.trim().is_empty() {
            bail!("Session event {} has an empty turn id", event.seq);
        }
        match event.kind {
            EventKind::TurnStart => {
                if turns
                    .insert(
                        event.turn_id.clone(),
                        TurnInvariant {
                            user_message: false,
                            last_request_iteration: None,
                            request_pending: false,
                            assistant_call_ids: HashMap::new(),
                            call_ids: HashMap::new(),
                            result_ids: HashMap::new(),
                        },
                    )
                    .is_some()
                {
                    bail!("Turn {} started more than once", event.turn_id);
                }
            }
            EventKind::TurnEnd => {
                let turn = turns
                    .remove(&event.turn_id)
                    .with_context(|| format!("Turn {} ended without a start", event.turn_id))?;
                if !turn.user_message {
                    bail!("Turn {} ended without a user message", event.turn_id);
                }
                for call_id in turn.assistant_call_ids.keys() {
                    if !turn.call_ids.contains_key(call_id) {
                        bail!(
                            "Turn {} ended without a durable tool/call for {call_id}",
                            event.turn_id
                        );
                    }
                }
                for call_id in turn.call_ids.keys() {
                    if !turn.result_ids.contains_key(call_id) {
                        bail!(
                            "Turn {} ended without a durable tool/result for {call_id}",
                            event.turn_id
                        );
                    }
                }
            }
            _ => {
                let turn = turns.get_mut(&event.turn_id).with_context(|| {
                    format!(
                        "Event {} ({}) belongs to a turn that is not open",
                        event.seq,
                        event.kind.as_str()
                    )
                })?;
                match event.kind {
                    EventKind::UserMessage => {
                        if turn.user_message {
                            bail!("Turn {} has multiple user messages", event.turn_id);
                        }
                        turn.user_message = true;
                    }
                    EventKind::AssistantMessage => {
                        if turn.last_request_iteration.is_some() {
                            if !turn.request_pending {
                                bail!(
                                    "Turn {} has an assistant response without a request header",
                                    event.turn_id
                                );
                            }
                            turn.request_pending = false;
                        }
                        if let Some(message) = stored_message_from_event(&event.data) {
                            for call in tool_calls_from_message(&message) {
                                if turn
                                    .assistant_call_ids
                                    .insert(call.id.clone(), ())
                                    .is_some()
                                {
                                    bail!("Turn {} reused tool-call id {}", event.turn_id, call.id);
                                }
                            }
                        } else {
                            bail!("Assistant event {} has no valid message", event.seq);
                        }
                    }
                    EventKind::RequestHeader => {
                        let snapshot: RequestSnapshot = serde_json::from_value(event.data.clone())
                            .with_context(|| {
                                format!("Request header event {} is malformed", event.seq)
                            })?;
                        if !snapshot.messages.is_array() || !snapshot.tools.is_array() {
                            bail!(
                                "Request header event {} has invalid envelope arrays",
                                event.seq
                            );
                        }
                        if turn
                            .last_request_iteration
                            .is_some_and(|last| snapshot.iteration <= last)
                        {
                            bail!(
                                "Turn {} has non-increasing request iterations",
                                event.turn_id
                            );
                        }
                        turn.last_request_iteration = Some(snapshot.iteration);
                        turn.request_pending = true;
                        if snapshot.input_sha256.len() != 64
                            || !snapshot
                                .input_sha256
                                .bytes()
                                .all(|byte| byte.is_ascii_hexdigit())
                        {
                            bail!(
                                "Request header event {} has an invalid input hash",
                                event.seq
                            );
                        }
                        let expected_hash = request_snapshot_hash(&snapshot)?;
                        if snapshot.input_sha256 != expected_hash {
                            bail!("Request header event {} has a stale input hash", event.seq);
                        }
                    }
                    EventKind::ToolCall => {
                        let call_id = event
                            .tool_call_id
                            .as_deref()
                            .context("Tool-call event has no tool-call id")?;
                        if turn.call_ids.insert(call_id.to_owned(), ()).is_some() {
                            bail!("Turn {} recorded tool call {call_id} twice", event.turn_id);
                        }
                    }
                    EventKind::ToolResult => {
                        let call_id = event
                            .tool_call_id
                            .as_deref()
                            .context("Tool-result event has no tool-call id")?;
                        if !turn.call_ids.contains_key(call_id) {
                            bail!(
                                "Turn {} recorded a result before tool call {call_id}",
                                event.turn_id
                            );
                        }
                        if turn.result_ids.insert(call_id.to_owned(), ()).is_some() {
                            bail!(
                                "Turn {} recorded tool result {call_id} twice",
                                event.turn_id
                            );
                        }
                    }
                    EventKind::ApprovalAsked | EventKind::ApprovalDecided => {
                        if let Some(call_id) = event.tool_call_id.as_deref()
                            && !turn.call_ids.contains_key(call_id)
                        {
                            bail!(
                                "Approval event {} references unknown tool call {call_id}",
                                event.seq
                            );
                        }
                    }
                    EventKind::TurnStart | EventKind::TurnEnd => unreachable!(),
                }
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
pub struct ToolResultMetadata<'a> {
    pub status: &'a str,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    pub presentation: Option<&'a Value>,
}

pub struct SummaryUpdate {
    pub content: String,
    pub through_seq: i64,
}

pub type EncodedChunk = (String, String, Vec<u8>, String, usize, usize);

pub struct KnowledgeInsert {
    pub item: KnowledgeRow,
    pub content_hash: String,
    pub chunks: Vec<EncodedChunk>,
}

pub struct StateStore {
    backend: Backend,
    path: PathBuf,
    database_owner_uid: Option<u32>,
    pending_audits: Vec<PendingAudit>,
    notice: Option<String>,
    checkpoints_dir: Option<PathBuf>,
}

enum Backend {
    Sqlite(Connection),
    Memory(Box<MemoryState>),
}

/// Lightweight state used when `storage.enabled = false`: a single session
/// is kept in either Redis or a JSON file on a tmpfs (RAM-backed) location.
/// Starting a new session replaces the previous one entirely.
struct MemoryState {
    session: Option<MemorySession>,
    persistence: MemoryPersistence,
}

enum MemoryPersistence {
    File(PathBuf),
    Redis {
        connection: RefCell<redis::Connection>,
        key: String,
    },
}

#[derive(Clone, Serialize, Deserialize)]
struct MemorySession {
    id: String,
    title: String,
    created_at: String,
    updated_at: String,
    last_cwd: String,
    messages: Vec<SequencedMessage>,
    #[serde(default)]
    events: Vec<SessionEvent>,
    summary: Option<String>,
    summary_through_seq: i64,
}

#[derive(Serialize, Deserialize)]
struct MemoryFile {
    version: u32,
    session: Option<MemorySession>,
}

const MEMORY_FILE_VERSION: u32 = 1;
const MEMORY_FILE_NAME: &str = "qin-session.json";
const MAX_MEMORY_STATE_BYTES: u64 = 128 * 1024 * 1024;

#[derive(Debug)]
struct InvalidRedisState(String);

impl fmt::Display for InvalidRedisState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for InvalidRedisState {}

/// Path of the tmpfs session file. `storage.data_dir` overrides the directory;
/// otherwise a RAM-backed location is preferred: $XDG_RUNTIME_DIR or /dev/shm
/// on Linux, falling back to the system temp directory.
pub(crate) fn memory_state_path(config: &Config) -> Result<PathBuf> {
    if !config.storage.data_dir.trim().is_empty() {
        return Ok(
            crate::config::absolute(PathBuf::from(&config.storage.data_dir))?
                .join(MEMORY_FILE_NAME),
        );
    }
    let directory = memory_state_directory();
    Ok(directory.join(MEMORY_FILE_NAME))
}

fn memory_state_directory() -> PathBuf {
    #[cfg(target_os = "linux")]
    {
        if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
            if !dir.is_empty() && Path::new(&dir).is_dir() {
                return PathBuf::from(dir).join(format!("qin-{}", effective_uid()));
            }
        }
        if Path::new("/dev/shm").is_dir() {
            return PathBuf::from("/dev/shm").join(format!("qin-{}", effective_uid()));
        }
    }
    std::env::temp_dir().join(format!("qin-{}", effective_uid()))
}

fn load_memory_session(path: &Path) -> Result<Option<MemorySession>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("Unable to inspect session state {}", path.display()));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!(
            "Refusing to read session state from a non-regular file: {}",
            path.display()
        );
    }
    if metadata.len() > MAX_MEMORY_STATE_BYTES {
        bail!("Session state exceeds the 128 MiB size limit");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.uid() != effective_uid() || metadata.permissions().mode() & 0o077 != 0 {
            bail!(
                "Session state must be owned by the current user and accessible only to that user: {}",
                path.display()
            );
        }
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    options
        .open(path)
        .with_context(|| format!("Unable to open session state {}", path.display()))?
        .take(MAX_MEMORY_STATE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_MEMORY_STATE_BYTES {
        bail!("Session state exceeds the 128 MiB size limit");
    }
    decode_memory_session(&bytes).with_context(|| {
        format!(
            "Session state is invalid or uses an unsupported version: {}",
            path.display()
        )
    })
}

fn load_redis_session(
    connection: &mut redis::Connection,
    key: &str,
) -> Result<Option<MemorySession>> {
    let exists: bool = redis::cmd("EXISTS").arg(key).query(connection)?;
    if !exists {
        return Ok(None);
    }
    let value_type: String = redis::cmd("TYPE").arg(key).query(connection)?;
    if value_type == "none" {
        return Ok(None);
    }
    if value_type != "string" {
        return Err(anyhow::Error::new(InvalidRedisState(format!(
            "Redis key {key} has type {value_type}; qin requires a string"
        ))));
    }
    let length: u64 = redis::cmd("STRLEN").arg(key).query(connection)?;
    if length > MAX_MEMORY_STATE_BYTES {
        return Err(anyhow::Error::new(InvalidRedisState(
            "Redis qin session state exceeds the 128 MiB size limit".into(),
        )));
    }
    let bytes: Vec<u8> = redis::cmd("GETRANGE")
        .arg(key)
        .arg(0)
        .arg(MAX_MEMORY_STATE_BYTES as i64)
        .query(connection)?;
    decode_redis_session(&bytes)
}

fn decode_memory_session(bytes: &[u8]) -> Result<Option<MemorySession>> {
    let file: MemoryFile = serde_json::from_slice(bytes)?;
    if file.version != MEMORY_FILE_VERSION {
        bail!("Unsupported qin session-state version: {}", file.version);
    }
    Ok(file.session)
}

fn decode_redis_session(bytes: &[u8]) -> Result<Option<MemorySession>> {
    decode_memory_session(bytes).map_err(|error| {
        anyhow::Error::new(InvalidRedisState(format!(
            "Redis contains invalid qin session state: {error:#}"
        )))
    })
}

fn select_redis_session(
    remote: Option<MemorySession>,
    local: Option<MemorySession>,
) -> (Option<MemorySession>, bool) {
    match (remote, local) {
        (None, None) => (None, false),
        (None, Some(local)) => (Some(local), true),
        (Some(remote), None) => (Some(remote), false),
        (Some(remote), Some(local)) if memory_session_is_newer(&local, &remote) => {
            (Some(local), true)
        }
        (Some(remote), Some(_)) => (Some(remote), false),
    }
}

fn memory_session_is_newer(candidate: &MemorySession, current: &MemorySession) -> bool {
    if candidate.updated_at != current.updated_at {
        return candidate.updated_at > current.updated_at;
    }
    if candidate.id != current.id {
        return false;
    }
    let candidate_seq = candidate.messages.last().map_or(0, |message| message.seq);
    let current_seq = current.messages.last().map_or(0, |message| message.seq);
    let candidate_event_seq = candidate.events.last().map_or(0, |event| event.seq);
    let current_event_seq = current.events.last().map_or(0, |event| event.seq);
    (
        candidate_seq,
        candidate_event_seq,
        candidate.summary_through_seq,
    ) > (current_seq, current_event_seq, current.summary_through_seq)
}

impl MemorySession {
    fn new(cwd: &Path, title: Option<&str>) -> Self {
        let now = now_timestamp();
        Self {
            id: Uuid::new_v4().to_string(),
            title: title.unwrap_or("New session").to_string(),
            created_at: now.clone(),
            updated_at: now,
            last_cwd: cwd.to_string_lossy().into_owned(),
            messages: Vec::new(),
            events: Vec::new(),
            summary: None,
            summary_through_seq: 0,
        }
    }

    fn info(&self) -> SessionInfo {
        SessionInfo {
            id: self.id.clone(),
            title: self.title.clone(),
            created_at: self.created_at.clone(),
            updated_at: self.updated_at.clone(),
            last_cwd: self.last_cwd.clone(),
        }
    }
}

fn now_timestamp() -> String {
    chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

pub struct SessionLock {
    _file: Option<fs::File>,
}

impl Drop for SessionLock {
    fn drop(&mut self) {
        if let Some(file) = &self._file {
            let _ = FileExt::unlock(file);
        }
    }
}

#[derive(Clone)]
struct PendingAudit {
    session_id: String,
    call_id: String,
    name: String,
    args: String,
    result: String,
    status: String,
    risk: String,
    exit_code: Option<i32>,
    duration_ms: u64,
}

const MAX_EVENT_DATA_BYTES: usize = 1024 * 1024;
/// Request envelopes carry the full model-visible context, which is
/// legitimately far larger than any single message: context_window may be
/// configured up to two million tokens. Unlike message events, a snapshot
/// must stay byte-exact so its input hash keeps matching on reload, so it is
/// never truncated and gets its own ceiling.
const MAX_SNAPSHOT_EVENT_DATA_BYTES: usize = 64 * 1024 * 1024;

fn event_data_limit(kind: EventKind) -> usize {
    if kind == EventKind::RequestHeader {
        MAX_SNAPSHOT_EVENT_DATA_BYTES
    } else {
        MAX_EVENT_DATA_BYTES
    }
}

fn encode_event_data(data: &Value, max_bytes: usize) -> Result<String> {
    let encoded = serde_json::to_string(data)?;
    if encoded.len() > max_bytes {
        bail!(
            "Session event data exceeds the size limit ({} > {max_bytes} bytes)",
            encoded.len()
        );
    }
    Ok(encoded)
}

/// Bounds an event payload to the size limit. Oversized message content is
/// truncated only in the event copy; the materialized messages table always
/// keeps the full content, so recovery never depends on the truncated text.
fn bound_event_data(mut data: Value, max_bytes: usize) -> Result<Value> {
    if serde_json::to_string(&data)?.len() <= max_bytes {
        return Ok(data);
    }
    if let Some(message) = data.get_mut("message").and_then(Value::as_object_mut) {
        if let Some(Value::String(content)) = message.get_mut("content") {
            content.truncate(char_boundary(content, max_bytes / 2));
            content.push_str("\n[content truncated in the event log]");
            message.insert("event_content_truncated".into(), Value::Bool(true));
        }
    }
    encode_event_data(&data, max_bytes)?;
    Ok(data)
}

fn char_boundary(value: &str, max_bytes: usize) -> usize {
    if value.len() <= max_bytes {
        return value.len();
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    end
}

fn append_event_tx(
    transaction: &rusqlite::Transaction<'_>,
    session_id: &str,
    kind: EventKind,
    turn_id: &str,
    tool_call_id: Option<&str>,
    data: &Value,
) -> Result<i64> {
    let seq: i64 = transaction.query_row(
        "SELECT COALESCE(MAX(seq),0)+1 FROM session_events WHERE session_id=?1",
        [session_id],
        |row| row.get(0),
    )?;
    let encoded = encode_event_data(data, event_data_limit(kind))?;
    transaction.execute(
        "INSERT INTO session_events(session_id,seq,kind,turn_id,tool_call_id,data) VALUES (?1,?2,?3,?4,?5,?6)",
        params![session_id, seq, kind.as_str(), turn_id, tool_call_id, encoded],
    )?;
    Ok(seq)
}

fn append_message_tx(
    transaction: &rusqlite::Transaction<'_>,
    session_id: &str,
    message: &StoredMessage,
) -> Result<i64> {
    let seq: i64 = transaction.query_row(
        "SELECT COALESCE(MAX(seq),0)+1 FROM messages WHERE session_id=?1",
        [session_id],
        |row| row.get(0),
    )?;
    transaction.execute(
        "INSERT INTO messages(session_id,seq,role,content,tool_calls,tool_call_id) VALUES (?1,?2,?3,?4,?5,?6)",
        params![
            session_id,
            seq,
            message.role,
            message.content,
            message.tool_calls,
            message.tool_call_id
        ],
    )?;
    Ok(seq)
}

fn append_memory_event(
    session: &mut MemorySession,
    kind: EventKind,
    turn_id: &str,
    tool_call_id: Option<&str>,
    data: Value,
) -> i64 {
    let seq = session.events.last().map_or(0, |event| event.seq) + 1;
    session.events.push(SessionEvent {
        seq,
        kind,
        turn_id: turn_id.into(),
        tool_call_id: tool_call_id.map(str::to_owned),
        data,
        created_at: now_timestamp(),
    });
    seq
}

fn append_memory_message(session: &mut MemorySession, message: &StoredMessage) -> i64 {
    let seq = session.messages.last().map_or(0, |entry| entry.seq) + 1;
    session.messages.push(SequencedMessage {
        seq,
        message: message.clone(),
    });
    seq
}

#[derive(Clone)]
struct RecoverableToolCall {
    id: String,
    name: String,
    arguments: String,
}

struct OpenTurn {
    turn_id: String,
    calls: BTreeMap<String, RecoverableToolCall>,
    call_events: BTreeMap<String, ()>,
    result_events: BTreeMap<String, ()>,
}

fn stored_message_from_event(data: &Value) -> Option<StoredMessage> {
    serde_json::from_value(data.get("message")?.clone()).ok()
}

fn event_message(message: &StoredMessage) -> StoredMessage {
    let mut event_message = message.clone();
    if let Some(raw) = message.tool_calls.as_deref()
        && let Ok(mut calls) = serde_json::from_str::<Value>(raw)
        && let Some(calls) = calls.as_array_mut()
    {
        for call in &mut *calls {
            if let Some(function) = call.get_mut("function").and_then(Value::as_object_mut)
                && function.contains_key("arguments")
            {
                function.insert("arguments".into(), Value::String("{}".into()));
            }
        }
        event_message.tool_calls = Some(calls_to_json(calls));
    }
    event_message
}

fn calls_to_json(calls: &[Value]) -> String {
    Value::Array(calls.to_vec()).to_string()
}

fn tool_calls_from_message(message: &StoredMessage) -> Vec<RecoverableToolCall> {
    let Some(raw) = message.tool_calls.as_deref() else {
        return Vec::new();
    };
    let Ok(Value::Array(calls)) = serde_json::from_str(raw) else {
        return Vec::new();
    };
    calls
        .into_iter()
        .filter_map(|call| {
            Some(RecoverableToolCall {
                id: call.get("id")?.as_str()?.to_owned(),
                name: call.get("function")?.get("name")?.as_str()?.to_owned(),
                arguments: call
                    .get("function")?
                    .get("arguments")?
                    .as_str()
                    .unwrap_or("{}")
                    .to_owned(),
            })
        })
        .collect()
}

impl StateStore {
    pub fn open(config: &Config, resolver: &ConfigPathResolver) -> Result<Self> {
        if !config.persistence_enabled() {
            if config.storage.redis.enabled {
                match Self::open_redis(config) {
                    Ok(store) => return Ok(store),
                    Err(error) if error.downcast_ref::<InvalidRedisState>().is_some() => {
                        return Err(error);
                    }
                    Err(error) => {
                        let path = memory_state_path(config)?;
                        let mut store = Self::open_file_memory(path)?;
                        store.notice = Some(format!(
                            "Redis session store unavailable; using the JSON fallback ({error:#})"
                        ));
                        return Ok(store);
                    }
                }
            }
            let path = memory_state_path(config)?;
            return Self::open_file_memory(path);
        }
        Self::open_sqlite(config, resolver)
    }

    fn open_file_memory(path: PathBuf) -> Result<Self> {
        let session = load_memory_session(&path)?;
        Ok(Self {
            backend: Backend::Memory(Box::new(MemoryState {
                session,
                persistence: MemoryPersistence::File(path.clone()),
            })),
            path,
            database_owner_uid: None,
            pending_audits: Vec::new(),
            notice: None,
            checkpoints_dir: None,
        })
    }

    fn open_redis(config: &Config) -> Result<Self> {
        let redis_config = &config.storage.redis;
        let client = redis::Client::open(redis_config.resolve_url()?)
            .context("Unable to create the Redis client")?;
        let mut connection = client
            .get_connection_with_timeout(Duration::from_millis(redis_config.connect_timeout_ms))
            .context("Unable to connect to Redis")?;
        let timeout = Duration::from_millis(redis_config.connect_timeout_ms);
        connection
            .set_read_timeout(Some(timeout))
            .context("Unable to configure the Redis read timeout")?;
        connection
            .set_write_timeout(Some(timeout))
            .context("Unable to configure the Redis write timeout")?;
        redis::cmd("PING")
            .query::<String>(&mut connection)
            .context("Redis did not respond to PING")?;
        let key = redis_config.key();
        let remote_session = load_redis_session(&mut connection, &key)?;
        let local_path = memory_state_path(config)?;
        let local_session = load_memory_session(&local_path)?;
        let (session, migrate_local) = select_redis_session(remote_session, local_session);
        if migrate_local {
            let file = MemoryFile {
                version: MEMORY_FILE_VERSION,
                session: session.clone(),
            };
            let bytes = serde_json::to_vec(&file)?;
            redis::cmd("SET")
                .arg(&key)
                .arg(bytes)
                .query::<()>(&mut connection)
                .context("Unable to migrate the newer local qin session to Redis")?;
        }
        match fs::remove_file(&local_path) {
            Ok(()) => {
                if let Some(parent) = local_path.parent() {
                    if let Err(error) = sync_state_directory(parent) {
                        return Err(anyhow::Error::new(InvalidRedisState(format!(
                            "The obsolete JSON fallback was removed, but its directory could not be synchronized: {error:#}"
                        ))));
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(anyhow::Error::new(InvalidRedisState(format!(
                    "Redis accepted the qin session, but the obsolete JSON fallback could not be removed from {}: {error}",
                    local_path.display()
                ))));
            }
        }
        Ok(Self {
            backend: Backend::Memory(Box::new(MemoryState {
                session,
                persistence: MemoryPersistence::Redis {
                    connection: RefCell::new(connection),
                    key: key.clone(),
                },
            })),
            path: PathBuf::from(format!("redis:{key}")),
            database_owner_uid: None,
            pending_audits: Vec::new(),
            notice: None,
            checkpoints_dir: None,
        })
    }

    fn open_sqlite(config: &Config, resolver: &ConfigPathResolver) -> Result<Self> {
        let requested_path = resolver.database_path(config)?;
        let parent = requested_path
            .parent()
            .context("The database path has no parent directory")?;
        let parent_existed = parent.exists();
        fs::create_dir_all(parent)
            .with_context(|| format!("Unable to create data directory {}", parent.display()))?;
        if !parent_existed && resolver.scope() != ConfigScope::Explicit {
            set_directory_permissions(parent, resolver.scope())?;
        }
        let canonical_parent = parent.canonicalize()?;
        validate_database_directory(&canonical_parent)?;
        let path = canonical_parent.join(
            requested_path
                .file_name()
                .context("The database path has no file name")?,
        );
        if config.storage.data_dir.trim().is_empty() {
            resolver.ensure_owner(&canonical_parent)?;
        }
        prepare_database_file(&path)?;
        let connection = Connection::open_with_flags(
            &path,
            OpenFlags::default() | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .with_context(|| format!("Unable to open database {}", path.display()))?;
        connection.busy_timeout(Duration::from_millis(config.storage.busy_timeout_ms))?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        let journal = choose_journal(config, &path);
        let actual_journal: String =
            connection.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
        if !actual_journal.eq_ignore_ascii_case(journal) {
            connection.pragma_update(None, "journal_mode", journal)?;
        }
        connection.pragma_update(
            None,
            "synchronous",
            if config.storage.write_profile == "durable" {
                "FULL"
            } else {
                "NORMAL"
            },
        )?;
        let mut store = Self {
            backend: Backend::Sqlite(connection),
            path,
            database_owner_uid: resolver.owner_uid(),
            pending_audits: Vec::new(),
            notice: None,
            checkpoints_dir: Some(canonical_parent.join("checkpoints")),
        };
        store.migrate()?;
        store.secure_database_files()?;
        Ok(store)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn backend_label(&self) -> &'static str {
        match &self.backend {
            Backend::Sqlite(_) => "sqlite",
            Backend::Memory(memory) => match &memory.persistence {
                MemoryPersistence::File(_) => "tmpfs-json",
                MemoryPersistence::Redis { .. } => "redis",
            },
        }
    }

    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    fn connection(&self) -> &Connection {
        match &self.backend {
            Backend::Sqlite(connection) => connection,
            Backend::Memory(_) => unreachable!("sqlite backend expected"),
        }
    }

    fn connection_mut(&mut self) -> &mut Connection {
        match &mut self.backend {
            Backend::Sqlite(connection) => connection,
            Backend::Memory(_) => unreachable!("sqlite backend expected"),
        }
    }

    /// Atomically persists the tmpfs session file (memory backend only).
    fn save_memory_state(&self) -> Result<()> {
        let Backend::Memory(memory) = &self.backend else {
            return Ok(());
        };
        let file = MemoryFile {
            version: MEMORY_FILE_VERSION,
            session: memory.session.clone(),
        };
        let bytes = serde_json::to_vec(&file)?;
        if bytes.len() as u64 > MAX_MEMORY_STATE_BYTES {
            bail!("Session state exceeds the 128 MiB size limit");
        }
        match &memory.persistence {
            MemoryPersistence::File(path) => {
                let parent = path
                    .parent()
                    .context("The memory state path has no parent directory")?;
                ensure_private_memory_directory(parent)?;
                match fs::symlink_metadata(path) {
                    Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                        bail!(
                            "Refusing to replace a non-regular session-state path: {}",
                            path.display()
                        );
                    }
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
                let mut temporary = NamedTempFile::new_in(parent).with_context(|| {
                    format!(
                        "Unable to create temporary session state in {}",
                        parent.display()
                    )
                })?;
                temporary.write_all(&bytes)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    temporary
                        .as_file()
                        .set_permissions(fs::Permissions::from_mode(0o600))?;
                }
                temporary.as_file().sync_all()?;
                temporary
                    .persist(path)
                    .map_err(|error| error.error)
                    .with_context(|| {
                        format!("Unable to persist session state to {}", path.display())
                    })?;
                sync_state_directory(parent)?;
                Ok(())
            }
            MemoryPersistence::Redis { connection, key } => {
                let mut connection = connection.borrow_mut();
                redis::cmd("SET")
                    .arg(key)
                    .arg(bytes)
                    .query::<()>(&mut *connection)
                    .context("Unable to persist the qin session to Redis")?;
                Ok(())
            }
        }
    }

    pub fn lock_session(&self, session_id: &str) -> Result<SessionLock> {
        let identity = sha256(&format!("{}\0{session_id}", self.path.display()));
        let directory = std::env::temp_dir().join(format!("qin-locks-{}", effective_uid()));
        ensure_private_lock_directory(&directory)?;
        let path = directory.join(format!("{}.lock", &identity[..40]));
        let file = open_private_lock(&path)?;
        file.try_lock_exclusive().with_context(|| {
            format!(
                "Session {session_id} is in use by another qin process; lock file: {}",
                path.display()
            )
        })?;
        Ok(SessionLock { _file: Some(file) })
    }

    fn migrate(&mut self) -> Result<()> {
        self.connection().execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS schema_migrations (
                version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );
            CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'active',
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                initial_cwd TEXT NOT NULL,
                last_cwd TEXT NOT NULL,
                compacted_summary TEXT
            );
            CREATE TABLE IF NOT EXISTS messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                seq INTEGER NOT NULL,
                role TEXT NOT NULL,
                content TEXT,
                tool_calls TEXT,
                tool_call_id TEXT,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                UNIQUE(session_id, seq)
            );
            CREATE INDEX IF NOT EXISTS idx_messages_session_seq ON messages(session_id, seq);
            CREATE INDEX IF NOT EXISTS idx_messages_session_role ON messages(session_id, role);
            CREATE TABLE IF NOT EXISTS app_state (
                key TEXT PRIMARY KEY, value TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS tool_executions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                tool_call_id TEXT NOT NULL,
                name TEXT NOT NULL,
                args_redacted_json TEXT NOT NULL,
                result_text TEXT,
                status TEXT NOT NULL,
                risk TEXT NOT NULL,
                exit_code INTEGER,
                duration_ms INTEGER,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );
            CREATE TABLE IF NOT EXISTS knowledge_items (
                id TEXT PRIMARY KEY,
                kind TEXT NOT NULL,
                title TEXT NOT NULL,
                source_uri TEXT,
                content TEXT NOT NULL,
                content_hash TEXT NOT NULL,
                importance REAL NOT NULL DEFAULT 0.5,
                enabled INTEGER NOT NULL DEFAULT 1,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                UNIQUE(kind, content_hash)
            );
            CREATE INDEX IF NOT EXISTS idx_knowledge_items_kind ON knowledge_items(kind, enabled);
            CREATE TABLE IF NOT EXISTS knowledge_chunks (
                id TEXT PRIMARY KEY,
                item_id TEXT NOT NULL REFERENCES knowledge_items(id) ON DELETE CASCADE,
                chunk_no INTEGER NOT NULL,
                content TEXT NOT NULL,
                content_hash TEXT NOT NULL,
                embedding_blob BLOB NOT NULL,
                vector_encoding TEXT NOT NULL,
                dimensions INTEGER NOT NULL,
                token_count INTEGER NOT NULL,
                UNIQUE(item_id, chunk_no)
            );
            CREATE INDEX IF NOT EXISTS idx_knowledge_chunks_item ON knowledge_chunks(item_id);
            INSERT OR IGNORE INTO schema_migrations(version) VALUES (1);
            "#,
        )?;
        let version: i64 = self.connection().query_row(
            "SELECT COALESCE(MAX(version),0) FROM schema_migrations",
            [],
            |row| row.get(0),
        )?;
        if version < 2 {
            let transaction = self.connection_mut().transaction()?;
            transaction.execute(
                "ALTER TABLE sessions ADD COLUMN compacted_through_seq INTEGER NOT NULL DEFAULT 0",
                [],
            )?;
            transaction.execute("INSERT INTO schema_migrations(version) VALUES (2)", [])?;
            transaction.commit()?;
        }
        if version < 3 {
            let transaction = self.connection_mut().transaction()?;
            transaction.execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS checkpoints (
                    id TEXT PRIMARY KEY,
                    session_id TEXT NOT NULL,
                    tool_call_id TEXT NOT NULL,
                    tool TEXT NOT NULL,
                    restored INTEGER NOT NULL DEFAULT 0,
                    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
                );
                CREATE INDEX IF NOT EXISTS idx_checkpoints_created ON checkpoints(created_at);
                CREATE TABLE IF NOT EXISTS checkpoint_entries (
                    checkpoint_id TEXT NOT NULL REFERENCES checkpoints(id) ON DELETE CASCADE,
                    seq INTEGER NOT NULL,
                    path TEXT NOT NULL,
                    kind TEXT NOT NULL,
                    related_path TEXT,
                    existed_before INTEGER NOT NULL,
                    snapshot_file TEXT,
                    original_sha256 TEXT,
                    PRIMARY KEY (checkpoint_id, seq)
                );
                "#,
            )?;
            transaction.execute("INSERT INTO schema_migrations(version) VALUES (3)", [])?;
            transaction.commit()?;
        }
        if version < 4 {
            let transaction = self.connection_mut().transaction()?;
            transaction.execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS session_events (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                    seq INTEGER NOT NULL,
                    kind TEXT NOT NULL,
                    turn_id TEXT NOT NULL,
                    tool_call_id TEXT,
                    data TEXT NOT NULL,
                    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                    UNIQUE(session_id, seq)
                );
                CREATE INDEX IF NOT EXISTS idx_session_events_session_seq
                    ON session_events(session_id, seq);
                CREATE INDEX IF NOT EXISTS idx_session_events_turn
                    ON session_events(session_id, turn_id, seq);
                "#,
            )?;
            transaction.execute("INSERT INTO schema_migrations(version) VALUES (4)", [])?;
            transaction.commit()?;
        }
        Ok(())
    }

    pub fn current_session(&self) -> Result<Option<String>> {
        if let Backend::Memory(memory) = &self.backend {
            return Ok(memory.session.as_ref().map(|session| session.id.clone()));
        }
        Ok(self
            .connection()
            .query_row(
                "SELECT value FROM app_state WHERE key='current_session'",
                [],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn ensure_current_session(&mut self, cwd: &Path) -> Result<String> {
        if let Some(id) = self.current_session()? {
            if self.session_exists(&id)? {
                return Ok(id);
            }
        }
        self.new_session(cwd, None)
    }

    pub fn new_session(&mut self, cwd: &Path, title: Option<&str>) -> Result<String> {
        if matches!(self.backend, Backend::Memory(_)) {
            // A new session fully replaces the previous one.
            let session = MemorySession::new(cwd, title);
            let id = session.id.clone();
            let Backend::Memory(memory) = &mut self.backend else {
                unreachable!()
            };
            memory.session = Some(session);
            self.save_memory_state()?;
            return Ok(id);
        }
        let id = Uuid::new_v4().to_string();
        let title = title.unwrap_or("New session");
        let cwd = cwd.to_string_lossy();
        let transaction = self.connection_mut().transaction()?;
        transaction.execute(
            "INSERT INTO sessions(id,title,initial_cwd,last_cwd) VALUES (?1,?2,?3,?3)",
            params![id, title, cwd],
        )?;
        transaction.execute(
            "INSERT INTO app_state(key,value) VALUES ('current_session',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![id],
        )?;
        transaction.commit()?;
        self.secure_database_files()?;
        Ok(id)
    }

    pub fn use_session(&mut self, id_or_prefix: &str) -> Result<String> {
        let id = self.resolve_session_id(id_or_prefix)?;
        if matches!(self.backend, Backend::Memory(_)) {
            // The single in-memory session is always the current one.
            return Ok(id);
        }
        self.connection().execute(
            "INSERT INTO app_state(key,value) VALUES ('current_session',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [&id],
        )?;
        Ok(id)
    }

    fn session_exists(&self, id: &str) -> Result<bool> {
        if let Backend::Memory(memory) = &self.backend {
            return Ok(memory.session.as_ref().is_some_and(|s| s.id == id));
        }
        Ok(self.connection().query_row(
            "SELECT EXISTS(SELECT 1 FROM sessions WHERE id=?1)",
            [id],
            |row| row.get(0),
        )?)
    }

    pub fn resolve_session_id(&self, id_or_prefix: &str) -> Result<String> {
        let value = id_or_prefix.trim();
        if value.is_empty() {
            bail!("The session identifier cannot be empty");
        }
        if let Backend::Memory(memory) = &self.backend {
            return match memory.session.as_ref() {
                Some(session) if session.id == value || session.id.starts_with(value) => {
                    Ok(session.id.clone())
                }
                _ => bail!("Session does not exist: {value}"),
            };
        }
        let mut statement = self.connection().prepare(
            "SELECT id FROM sessions WHERE id=?1 OR substr(id,1,length(?1))=?1 ORDER BY id LIMIT 2",
        )?;
        let matches = statement
            .query_map([value], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        match matches.as_slice() {
            [] => bail!("Session does not exist: {value}"),
            [id] => Ok(id.clone()),
            _ => bail!("Session identifier is ambiguous: {value}"),
        }
    }

    pub fn checkpoints_dir(&self) -> Option<&Path> {
        self.checkpoints_dir.as_deref()
    }

    /// Checkpoint metadata lives in SQLite; the lightweight memory backends
    /// (tmpfs JSON / Redis) do not support checkpoints.
    pub fn checkpoints_supported(&self) -> bool {
        !matches!(self.backend, Backend::Memory(_))
    }

    pub fn insert_checkpoint(
        &self,
        id: &str,
        session_id: &str,
        tool_call_id: &str,
        tool: &str,
    ) -> Result<()> {
        if !self.checkpoints_supported() {
            return Ok(());
        }
        self.connection().execute(
            "INSERT INTO checkpoints(id,session_id,tool_call_id,tool) VALUES (?1,?2,?3,?4)",
            params![id, session_id, tool_call_id, tool],
        )?;
        Ok(())
    }

    pub fn insert_checkpoint_entry(
        &self,
        checkpoint_id: &str,
        entry: &CheckpointEntryRow,
    ) -> Result<()> {
        if !self.checkpoints_supported() {
            return Ok(());
        }
        self.connection().execute(
            "INSERT INTO checkpoint_entries(checkpoint_id,seq,path,kind,related_path,existed_before,snapshot_file,original_sha256) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                checkpoint_id,
                entry.seq,
                entry.path,
                entry.kind,
                entry.related_path,
                entry.existed_before as i64,
                entry.snapshot_file,
                entry.original_sha256
            ],
        )?;
        Ok(())
    }

    pub fn checkpoint_entries(&self, checkpoint_id: &str) -> Result<Vec<CheckpointEntryRow>> {
        let mut statement = self.connection().prepare(
            "SELECT seq,path,kind,related_path,existed_before,snapshot_file,original_sha256 FROM checkpoint_entries WHERE checkpoint_id=?1 ORDER BY seq",
        )?;
        let rows = statement.query_map([checkpoint_id], |row| {
            Ok(CheckpointEntryRow {
                seq: row.get(0)?,
                path: row.get(1)?,
                kind: row.get(2)?,
                related_path: row.get(3)?,
                existed_before: row.get::<_, i64>(4)? != 0,
                snapshot_file: row.get(5)?,
                original_sha256: row.get(6)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn list_checkpoints(&self, limit: usize) -> Result<Vec<CheckpointInfo>> {
        if !self.checkpoints_supported() {
            return Ok(Vec::new());
        }
        let mut statement = self.connection().prepare(
            "SELECT id,tool,created_at,restored FROM checkpoints ORDER BY rowid DESC LIMIT ?1",
        )?;
        let rows = statement.query_map([limit as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)? != 0,
            ))
        })?;
        let mut infos = Vec::new();
        for row in rows {
            let (id, tool, created_at, restored) = row?;
            let entries = self.checkpoint_entries(&id)?;
            infos.push(CheckpointInfo {
                id,
                tool,
                created_at,
                restored,
                paths: entries.into_iter().map(|entry| entry.path).collect(),
            });
        }
        Ok(infos)
    }

    pub fn latest_checkpoint_id(&self) -> Result<Option<String>> {
        if !self.checkpoints_supported() {
            return Ok(None);
        }
        Ok(self
            .connection()
            .query_row(
                "SELECT id FROM checkpoints WHERE restored=0 ORDER BY rowid DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn resolve_checkpoint_id(&self, id_or_prefix: &str) -> Result<String> {
        let like = format!("{id_or_prefix}%");
        let mut statement = self
            .connection()
            .prepare("SELECT id FROM checkpoints WHERE id LIKE ?1")?;
        let ids = statement.query_map([like], |row| row.get::<_, String>(0))?;
        let ids = ids.collect::<rusqlite::Result<Vec<_>>>()?;
        match ids.as_slice() {
            [] => bail!("Checkpoint does not exist: {id_or_prefix}"),
            [id] => Ok(id.clone()),
            _ => bail!("Checkpoint identifier is ambiguous: {id_or_prefix}"),
        }
    }

    pub fn checkpoint_restored(&self, id: &str) -> Result<bool> {
        Ok(self.connection().query_row(
            "SELECT restored FROM checkpoints WHERE id=?1",
            [id],
            |row| row.get::<_, i64>(0),
        )? != 0)
    }

    pub fn mark_checkpoint_restored(&self, id: &str) -> Result<()> {
        self.connection()
            .execute("UPDATE checkpoints SET restored=1 WHERE id=?1", [id])?;
        Ok(())
    }

    /// Removes all checkpoints beyond the newest `keep`; returns the pruned
    /// checkpoint ids so the caller can remove their snapshot directories.
    pub fn prune_checkpoints(&self, keep: u32) -> Result<Vec<String>> {
        if !self.checkpoints_supported() {
            return Ok(Vec::new());
        }
        let mut statement = self
            .connection()
            .prepare("SELECT id FROM checkpoints ORDER BY rowid DESC LIMIT -1 OFFSET ?1")?;
        let ids = statement
            .query_map([keep as i64], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for id in &ids {
            self.connection()
                .execute("DELETE FROM checkpoints WHERE id=?1", [id])?;
        }
        Ok(ids)
    }

    fn checkpoint_ids_for_session(&self, session_id: &str) -> Result<Vec<String>> {
        if !self.checkpoints_supported() {
            return Ok(Vec::new());
        }
        let mut statement = self
            .connection()
            .prepare("SELECT id FROM checkpoints WHERE session_id=?1")?;
        Ok(statement
            .query_map([session_id], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn delete_session(
        &mut self,
        id_or_prefix: &str,
        cwd: &Path,
    ) -> Result<(String, Option<String>)> {
        let id = self.resolve_session_id(id_or_prefix)?;
        let was_current = self.current_session()?.as_deref() == Some(id.as_str());
        if matches!(self.backend, Backend::Memory(_)) {
            // Dropping the only in-memory session always activates a fresh one.
            let replacement = MemorySession::new(cwd, None);
            let new_current = was_current.then(|| replacement.id.clone());
            let Backend::Memory(memory) = &mut self.backend else {
                unreachable!()
            };
            memory.session = Some(replacement);
            self.save_memory_state()?;
            return Ok((id, new_current));
        }
        let checkpoint_ids = self.checkpoint_ids_for_session(&id)?;
        let transaction = self.connection_mut().transaction()?;
        transaction.execute("DELETE FROM tool_executions WHERE session_id=?1", [&id])?;
        transaction.execute("DELETE FROM checkpoints WHERE session_id=?1", [&id])?;
        transaction.execute("DELETE FROM sessions WHERE id=?1", [&id])?;
        let mut new_current = None;
        if was_current {
            let replacement = Uuid::new_v4().to_string();
            let cwd = cwd.to_string_lossy();
            transaction.execute(
                "INSERT INTO sessions(id,title,initial_cwd,last_cwd) VALUES (?1,'New session',?2,?2)",
                params![replacement, cwd],
            )?;
            transaction.execute(
                "INSERT INTO app_state(key,value) VALUES ('current_session',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                [&replacement],
            )?;
            new_current = Some(replacement);
        }
        transaction.commit()?;
        if let Some(root) = &self.checkpoints_dir {
            for checkpoint_id in checkpoint_ids {
                let directory = root.join(&checkpoint_id);
                if directory.exists() {
                    let _ = fs::remove_dir_all(&directory);
                }
            }
        }
        self.secure_database_files()?;
        Ok((id, new_current))
    }

    pub fn list_sessions(&self, limit: usize) -> Result<Vec<SessionInfo>> {
        if let Backend::Memory(memory) = &self.backend {
            return Ok(memory
                .session
                .as_ref()
                .map(|session| vec![session.info()])
                .unwrap_or_default());
        }
        let mut statement = self.connection().prepare(
            "SELECT id,title,created_at,updated_at,last_cwd FROM sessions ORDER BY updated_at DESC LIMIT ?1",
        )?;
        let rows = statement.query_map([limit as i64], |row| {
            Ok(SessionInfo {
                id: row.get(0)?,
                title: row.get(1)?,
                created_at: row.get(2)?,
                updated_at: row.get(3)?,
                last_cwd: row.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn load_messages(&self, session_id: &str) -> Result<Vec<StoredMessage>> {
        if let Backend::Memory(memory) = &self.backend {
            return Ok(memory
                .session
                .as_ref()
                .filter(|session| session.id == session_id)
                .map(|session| {
                    session
                        .messages
                        .iter()
                        .map(|entry| entry.message.clone())
                        .collect()
                })
                .unwrap_or_default());
        }
        let mut statement = self.connection().prepare(
            "SELECT role,content,tool_calls,tool_call_id FROM messages WHERE session_id=?1 ORDER BY seq",
        )?;
        let rows = statement.query_map([session_id], |row| {
            Ok(StoredMessage {
                role: row.get(0)?,
                content: row.get(1)?,
                tool_calls: row.get(2)?,
                tool_call_id: row.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn load_context_messages(&self, session_id: &str) -> Result<Vec<SequencedMessage>> {
        if let Backend::Memory(memory) = &self.backend {
            return Ok(memory
                .session
                .as_ref()
                .filter(|session| session.id == session_id)
                .map(|session| {
                    session
                        .messages
                        .iter()
                        .filter(|entry| entry.seq > session.summary_through_seq)
                        .cloned()
                        .collect()
                })
                .unwrap_or_default());
        }
        let mut statement = self.connection().prepare(
            "SELECT seq,role,content,tool_calls,tool_call_id FROM messages WHERE session_id=?1 AND seq > COALESCE((SELECT compacted_through_seq FROM sessions WHERE id=?1),0) ORDER BY seq",
        )?;
        let rows = statement.query_map([session_id], |row| {
            Ok(SequencedMessage {
                seq: row.get(0)?,
                message: StoredMessage {
                    role: row.get(1)?,
                    content: row.get(2)?,
                    tool_calls: row.get(3)?,
                    tool_call_id: row.get(4)?,
                },
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Returns the durable append-only execution log for a session.
    pub fn load_events(&self, session_id: &str) -> Result<Vec<SessionEvent>> {
        if let Backend::Memory(memory) = &self.backend {
            return Ok(memory
                .session
                .as_ref()
                .filter(|session| session.id == session_id)
                .map(|session| session.events.clone())
                .unwrap_or_default());
        }
        let mut statement = self.connection().prepare(
            "SELECT seq,kind,turn_id,tool_call_id,data,created_at FROM session_events WHERE session_id=?1 ORDER BY seq",
        )?;
        let rows = statement.query_map([session_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
            ))
        })?;
        let raw = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        raw.into_iter()
            .map(|(seq, kind, turn_id, tool_call_id, data, created_at)| {
                Ok(SessionEvent {
                    seq,
                    kind: EventKind::parse(&kind)?,
                    turn_id,
                    tool_call_id,
                    data: serde_json::from_str(&data)
                        .with_context(|| "Session event data is invalid JSON")?,
                    created_at,
                })
            })
            .collect()
    }

    pub fn validate_session(&self, session_id: &str) -> Result<()> {
        validate_session_events(&self.load_events(session_id)?)
    }

    /// Persists the exact request envelope immediately before it is handed to
    /// the model transport. This is intentionally a separate event from the
    /// materialized message history: compaction and runtime context can change
    /// the request without changing the durable conversation view.
    pub fn append_request_snapshot(
        &mut self,
        session_id: &str,
        turn_id: &str,
        snapshot: &RequestSnapshot,
    ) -> Result<()> {
        let expected_hash = request_snapshot_hash(snapshot)?;
        if snapshot.input_sha256 != expected_hash {
            bail!(
                "Request snapshot hash does not match its request envelope (stored {}, expected {})",
                snapshot.input_sha256,
                expected_hash
            );
        }
        self.append_durable_event(
            session_id,
            EventKind::RequestHeader,
            turn_id,
            None,
            serde_json::to_value(snapshot)?,
        )
    }

    /// Records the start of one approval request without duplicating tool
    /// arguments. The tool-call id links the request to the already logged
    /// invocation, while the fresh approval id pairs it with its decision.
    pub fn append_approval_asked(&mut self, request: &ApprovalRequest<'_>) -> Result<()> {
        let reason = crate::event::sanitize_terminal(&crate::event::redact(
            &request.reason.chars().take(2_048).collect::<String>(),
        ));
        self.append_durable_event(
            request.session_id,
            EventKind::ApprovalAsked,
            request.turn_id,
            Some(request.tool_call_id),
            json!({
                "approval_id": request.approval_id,
                "tool_name": request.tool_name,
                "reason": reason,
                "high_risk": request.high_risk,
                "allow_all": request.allow_all,
            }),
        )
    }

    /// Records the matching closed approval outcome. Callers may continue only
    /// for the scope represented by `allowed-once` or `allowed-for-task`.
    pub fn append_approval_decided(
        &mut self,
        session_id: &str,
        turn_id: &str,
        tool_call_id: &str,
        approval_id: &str,
        outcome: ApprovalOutcome,
    ) -> Result<()> {
        self.append_durable_event(
            session_id,
            EventKind::ApprovalDecided,
            turn_id,
            Some(tool_call_id),
            json!({
                "approval_id": approval_id,
                "outcome": outcome.as_str(),
            }),
        )
    }

    fn append_durable_event(
        &mut self,
        session_id: &str,
        kind: EventKind,
        turn_id: &str,
        tool_call_id: Option<&str>,
        data: Value,
    ) -> Result<()> {
        let data = bound_event_data(data, event_data_limit(kind))?;
        if matches!(self.backend, Backend::Memory(_)) {
            {
                let Backend::Memory(memory) = &mut self.backend else {
                    unreachable!()
                };
                let Some(session) = memory
                    .session
                    .as_mut()
                    .filter(|session| session.id == session_id)
                else {
                    bail!("Session does not exist: {session_id}");
                };
                append_memory_event(session, kind, turn_id, tool_call_id, data);
                session.updated_at = now_timestamp();
            }
            self.save_memory_state()?;
            return Ok(());
        }
        let transaction = self.connection_mut().transaction()?;
        append_event_tx(&transaction, session_id, kind, turn_id, tool_call_id, &data)?;
        transaction.execute(
            "UPDATE sessions SET updated_at=CURRENT_TIMESTAMP WHERE id=?1",
            [session_id],
        )?;
        transaction.commit()?;
        self.secure_database_files()?;
        Ok(())
    }

    /// Durably starts a turn and materializes the user's message in one
    /// backend transaction. Callers should hold the session lock first.
    pub fn start_turn(
        &mut self,
        session_id: &str,
        turn_id: &str,
        message: &StoredMessage,
        cwd: &Path,
    ) -> Result<()> {
        let start_data =
            bound_event_data(json!({"cwd": cwd.to_string_lossy()}), MAX_EVENT_DATA_BYTES)?;
        let message_data = bound_event_data(json!({"message": message}), MAX_EVENT_DATA_BYTES)?;
        if matches!(self.backend, Backend::Memory(_)) {
            {
                let Backend::Memory(memory) = &mut self.backend else {
                    unreachable!()
                };
                let Some(session) = memory
                    .session
                    .as_mut()
                    .filter(|session| session.id == session_id)
                else {
                    bail!("Session does not exist: {session_id}");
                };
                append_memory_event(session, EventKind::TurnStart, turn_id, None, start_data);
                append_memory_event(session, EventKind::UserMessage, turn_id, None, message_data);
                append_memory_message(session, message);
                session.last_cwd = cwd.to_string_lossy().into_owned();
                session.updated_at = now_timestamp();
            }
            self.save_memory_state()?;
            return Ok(());
        }
        let transaction = self.connection_mut().transaction()?;
        append_event_tx(
            &transaction,
            session_id,
            EventKind::TurnStart,
            turn_id,
            None,
            &start_data,
        )?;
        append_event_tx(
            &transaction,
            session_id,
            EventKind::UserMessage,
            turn_id,
            None,
            &message_data,
        )?;
        append_message_tx(&transaction, session_id, message)?;
        transaction.execute(
            "UPDATE sessions SET status='active',updated_at=CURRENT_TIMESTAMP,last_cwd=?2 WHERE id=?1",
            params![session_id, cwd.to_string_lossy()],
        )?;
        transaction.commit()?;
        self.secure_database_files()?;
        Ok(())
    }

    /// Appends an assistant response before the next tool is started.
    pub fn append_assistant_message(
        &mut self,
        session_id: &str,
        turn_id: &str,
        message: &StoredMessage,
    ) -> Result<()> {
        self.append_turn_message(
            session_id,
            turn_id,
            EventKind::AssistantMessage,
            None,
            message,
        )
    }

    fn append_turn_message(
        &mut self,
        session_id: &str,
        turn_id: &str,
        kind: EventKind,
        tool_call_id: Option<&str>,
        message: &StoredMessage,
    ) -> Result<()> {
        let data = bound_event_data(
            json!({"message": event_message(message)}),
            MAX_EVENT_DATA_BYTES,
        )?;
        if matches!(self.backend, Backend::Memory(_)) {
            {
                let Backend::Memory(memory) = &mut self.backend else {
                    unreachable!()
                };
                let Some(session) = memory
                    .session
                    .as_mut()
                    .filter(|session| session.id == session_id)
                else {
                    bail!("Session does not exist: {session_id}");
                };
                append_memory_event(session, kind, turn_id, tool_call_id, data);
                append_memory_message(session, message);
                session.updated_at = now_timestamp();
            }
            self.save_memory_state()?;
            return Ok(());
        }
        let transaction = self.connection_mut().transaction()?;
        append_event_tx(&transaction, session_id, kind, turn_id, tool_call_id, &data)?;
        append_message_tx(&transaction, session_id, message)?;
        transaction.execute(
            "UPDATE sessions SET updated_at=CURRENT_TIMESTAMP WHERE id=?1",
            [session_id],
        )?;
        transaction.commit()?;
        self.secure_database_files()?;
        Ok(())
    }

    /// Records the intent to execute a tool before control crosses into the
    /// tool implementation. This is the recovery boundary for side effects.
    pub fn append_tool_call(
        &mut self,
        session_id: &str,
        turn_id: &str,
        call_id: &str,
        name: &str,
        arguments: &str,
    ) -> Result<()> {
        let data = bound_event_data(
            json!({
                "name": name,
                "arguments_sha256": sha256(arguments),
                "arguments_bytes": arguments.len(),
            }),
            MAX_EVENT_DATA_BYTES,
        )?;
        if matches!(self.backend, Backend::Memory(_)) {
            {
                let Backend::Memory(memory) = &mut self.backend else {
                    unreachable!()
                };
                let Some(session) = memory
                    .session
                    .as_mut()
                    .filter(|session| session.id == session_id)
                else {
                    bail!("Session does not exist: {session_id}");
                };
                append_memory_event(session, EventKind::ToolCall, turn_id, Some(call_id), data);
                session.updated_at = now_timestamp();
            }
            self.save_memory_state()?;
            return Ok(());
        }
        let transaction = self.connection_mut().transaction()?;
        append_event_tx(
            &transaction,
            session_id,
            EventKind::ToolCall,
            turn_id,
            Some(call_id),
            &data,
        )?;
        transaction.execute(
            "UPDATE sessions SET updated_at=CURRENT_TIMESTAMP WHERE id=?1",
            [session_id],
        )?;
        transaction.commit()?;
        self.secure_database_files()?;
        Ok(())
    }

    /// Atomically records a tool result and materializes the corresponding
    /// tool message. The interrupted status is intentionally non-retryable:
    /// the external side effect may already have happened.
    pub fn append_tool_result(
        &mut self,
        session_id: &str,
        turn_id: &str,
        call_id: &str,
        message: &StoredMessage,
        metadata: ToolResultMetadata<'_>,
    ) -> Result<()> {
        let data = bound_event_data(
            json!({
                "message": message,
                "status": metadata.status,
                "exit_code": metadata.exit_code,
                "duration_ms": metadata.duration_ms,
                "presentation": metadata.presentation,
            }),
            MAX_EVENT_DATA_BYTES,
        )?;
        if matches!(self.backend, Backend::Memory(_)) {
            {
                let Backend::Memory(memory) = &mut self.backend else {
                    unreachable!()
                };
                let Some(session) = memory
                    .session
                    .as_mut()
                    .filter(|session| session.id == session_id)
                else {
                    bail!("Session does not exist: {session_id}");
                };
                append_memory_event(session, EventKind::ToolResult, turn_id, Some(call_id), data);
                append_memory_message(session, message);
                session.updated_at = now_timestamp();
            }
            self.save_memory_state()?;
            return Ok(());
        }
        let transaction = self.connection_mut().transaction()?;
        append_event_tx(
            &transaction,
            session_id,
            EventKind::ToolResult,
            turn_id,
            Some(call_id),
            &data,
        )?;
        append_message_tx(&transaction, session_id, message)?;
        transaction.execute(
            "UPDATE sessions SET updated_at=CURRENT_TIMESTAMP WHERE id=?1",
            [session_id],
        )?;
        transaction.commit()?;
        self.secure_database_files()?;
        Ok(())
    }

    /// Closes a turn with an explicit outcome. A failed turn is still a
    /// complete durable record, while a missing end event means the process
    /// died between two persistence points and must be recovered.
    pub fn finish_turn(
        &mut self,
        session_id: &str,
        turn_id: &str,
        status: &str,
        error: Option<&str>,
    ) -> Result<()> {
        let data = bound_event_data(
            json!({"status": status, "error": error}),
            MAX_EVENT_DATA_BYTES,
        )?;
        if matches!(self.backend, Backend::Memory(_)) {
            {
                let Backend::Memory(memory) = &mut self.backend else {
                    unreachable!()
                };
                let Some(session) = memory
                    .session
                    .as_mut()
                    .filter(|session| session.id == session_id)
                else {
                    bail!("Session does not exist: {session_id}");
                };
                append_memory_event(session, EventKind::TurnEnd, turn_id, None, data);
                session.updated_at = now_timestamp();
            }
            self.save_memory_state()?;
            return Ok(());
        }
        let transaction = self.connection_mut().transaction()?;
        append_event_tx(
            &transaction,
            session_id,
            EventKind::TurnEnd,
            turn_id,
            None,
            &data,
        )?;
        transaction.execute(
            "UPDATE sessions SET status=?2,updated_at=CURRENT_TIMESTAMP WHERE id=?1",
            params![session_id, status],
        )?;
        transaction.commit()?;
        self.secure_database_files()?;
        Ok(())
    }

    /// Repairs open turns after the caller has acquired the session lock.
    /// Missing tool results become explicit `interrupted` records; qin never
    /// silently replays an operation that may have already changed the world.
    pub fn recover_session(&mut self, session_id: &str) -> Result<usize> {
        let events = self.load_events(session_id)?;
        // Assistant/message events blank tool-call arguments (the full text
        // already lives in the materialized messages), so a re-emitted
        // tool/call event re-reads the real arguments from there to keep its
        // arguments hash truthful.
        let recorded_calls = self
            .load_messages(session_id)?
            .iter()
            .flat_map(tool_calls_from_message)
            .map(|call| (call.id.clone(), call))
            .collect::<BTreeMap<_, _>>();
        let mut open_turns = BTreeMap::<String, OpenTurn>::new();
        for event in &events {
            match event.kind {
                EventKind::TurnStart => {
                    open_turns.entry(event.turn_id.clone()).or_insert(OpenTurn {
                        turn_id: event.turn_id.clone(),
                        calls: BTreeMap::new(),
                        call_events: BTreeMap::new(),
                        result_events: BTreeMap::new(),
                    });
                }
                EventKind::AssistantMessage => {
                    if let Some(turn) = open_turns.get_mut(&event.turn_id)
                        && let Some(message) = stored_message_from_event(&event.data)
                    {
                        for call in tool_calls_from_message(&message) {
                            turn.calls.insert(call.id.clone(), call);
                        }
                    }
                }
                EventKind::ToolCall => {
                    if let Some(turn) = open_turns.get_mut(&event.turn_id)
                        && let Some(call_id) = event.tool_call_id.as_deref()
                    {
                        turn.call_events.insert(call_id.to_owned(), ());
                    }
                }
                EventKind::ToolResult => {
                    if let Some(turn) = open_turns.get_mut(&event.turn_id)
                        && let Some(call_id) = event.tool_call_id.as_deref()
                    {
                        turn.result_events.insert(call_id.to_owned(), ());
                    }
                }
                EventKind::TurnEnd => {
                    open_turns.remove(&event.turn_id);
                }
                _ => {}
            }
        }

        let mut repaired = 0;
        for turn in open_turns.into_values() {
            for call in turn.calls.values() {
                if turn.result_events.contains_key(&call.id) {
                    continue;
                }
                if !turn.call_events.contains_key(&call.id) {
                    let recorded = recorded_calls.get(&call.id).unwrap_or(call);
                    self.append_tool_call(
                        session_id,
                        &turn.turn_id,
                        &recorded.id,
                        &recorded.name,
                        &recorded.arguments,
                    )?;
                }
                let message = StoredMessage {
                    role: "tool".into(),
                    content: Some(format!(
                        "Tool execution was interrupted before qin could record its outcome: {}. The external state is unknown; verify it before retrying.",
                        call.name
                    )),
                    tool_calls: None,
                    tool_call_id: Some(call.id.clone()),
                };
                self.append_tool_result(
                    session_id,
                    &turn.turn_id,
                    &call.id,
                    &message,
                    ToolResultMetadata {
                        status: "interrupted",
                        exit_code: None,
                        duration_ms: 0,
                        presentation: None,
                    },
                )?;
                repaired += 1;
            }
            self.finish_turn(
                session_id,
                &turn.turn_id,
                "interrupted",
                Some("The previous qin process ended before the turn was closed"),
            )?;
            repaired += 1;
        }
        self.validate_session(session_id)?;
        Ok(repaired)
    }

    #[cfg(test)]
    pub fn append_messages(
        &mut self,
        session_id: &str,
        messages: &[StoredMessage],
        cwd: &Path,
    ) -> Result<()> {
        self.append_messages_with_summary(session_id, messages, cwd, None)
    }

    pub fn append_messages_with_summary(
        &mut self,
        session_id: &str,
        messages: &[StoredMessage],
        cwd: &Path,
        summary: Option<&SummaryUpdate>,
    ) -> Result<()> {
        if messages.is_empty() && self.pending_audits.is_empty() && summary.is_none() {
            return Ok(());
        }
        if matches!(self.backend, Backend::Memory(_)) {
            self.pending_audits.clear();
            {
                let Backend::Memory(memory) = &mut self.backend else {
                    unreachable!()
                };
                let Some(session) = memory
                    .session
                    .as_mut()
                    .filter(|session| session.id == session_id)
                else {
                    bail!("Session does not exist: {session_id}");
                };
                let start = session.messages.last().map_or(0, |entry| entry.seq) + 1;
                for (index, message) in messages.iter().enumerate() {
                    session.messages.push(SequencedMessage {
                        seq: start + index as i64,
                        message: message.clone(),
                    });
                }
                if let Some(update) = summary {
                    session.summary = Some(update.content.clone());
                    session.summary_through_seq =
                        session.summary_through_seq.max(update.through_seq);
                }
                session.last_cwd = cwd.to_string_lossy().into_owned();
                session.updated_at = now_timestamp();
            }
            self.save_memory_state()?;
            return Ok(());
        }
        let audits = self.pending_audits.clone();
        let transaction = self.connection_mut().transaction()?;
        let next: i64 = transaction.query_row(
            "SELECT COALESCE(MAX(seq),0)+1 FROM messages WHERE session_id=?1",
            [session_id],
            |row| row.get(0),
        )?;
        for (index, message) in messages.iter().enumerate() {
            transaction.execute(
                "INSERT INTO messages(session_id,seq,role,content,tool_calls,tool_call_id) VALUES (?1,?2,?3,?4,?5,?6)",
                params![session_id, next + index as i64, message.role, message.content, message.tool_calls, message.tool_call_id],
            )?;
        }
        for audit in &audits {
            transaction.execute(
                "INSERT INTO tool_executions(session_id,tool_call_id,name,args_redacted_json,result_text,status,risk,exit_code,duration_ms) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                params![audit.session_id,audit.call_id,audit.name,audit.args,audit.result,audit.status,audit.risk,audit.exit_code,audit.duration_ms as i64],
            )?;
        }
        transaction.execute(
            "UPDATE sessions SET updated_at=CURRENT_TIMESTAMP,last_cwd=?2 WHERE id=?1",
            params![session_id, cwd.to_string_lossy()],
        )?;
        if let Some(summary) = summary {
            transaction.execute(
                "UPDATE sessions SET compacted_summary=?2,compacted_through_seq=MAX(compacted_through_seq,?3) WHERE id=?1",
                params![session_id, summary.content, summary.through_seq],
            )?;
        }
        transaction.commit()?;
        self.pending_audits.clear();
        self.secure_database_files()?;
        Ok(())
    }

    pub fn summary(&self, session_id: &str) -> Result<Option<String>> {
        if let Backend::Memory(memory) = &self.backend {
            return Ok(memory
                .session
                .as_ref()
                .filter(|session| session.id == session_id)
                .and_then(|session| session.summary.clone()));
        }
        Ok(self
            .connection()
            .query_row(
                "SELECT compacted_summary FROM sessions WHERE id=?1",
                [session_id],
                |row| row.get(0),
            )
            .optional()?
            .flatten())
    }

    pub fn user_turn_count(&self, session_id: &str) -> Result<u32> {
        if let Backend::Memory(memory) = &self.backend {
            return Ok(memory
                .session
                .as_ref()
                .filter(|session| session.id == session_id)
                .map_or(0, |session| {
                    session
                        .messages
                        .iter()
                        .filter(|entry| entry.message.role == "user")
                        .count() as u32
                }));
        }
        Ok(self.connection().query_row(
            "SELECT COUNT(*) FROM messages WHERE session_id=?1 AND role='user'",
            [session_id],
            |row| row.get::<_, i64>(0),
        )? as u32)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn audit_tool(
        &mut self,
        session_id: &str,
        call_id: &str,
        name: &str,
        args: &str,
        result: &str,
        status: &str,
        risk: &str,
        exit_code: Option<i32>,
        duration_ms: u64,
    ) -> Result<()> {
        if matches!(self.backend, Backend::Memory(_)) {
            // Tool audit records are a persistence-only feature.
            return Ok(());
        }
        self.pending_audits.push(PendingAudit {
            session_id: session_id.into(),
            call_id: call_id.into(),
            name: name.into(),
            args: args.into(),
            result: result.into(),
            status: status.into(),
            risk: risk.into(),
            exit_code,
            duration_ms,
        });
        Ok(())
    }

    pub fn upsert_knowledge_batch(&mut self, inserts: &[KnowledgeInsert]) -> Result<usize> {
        if inserts.is_empty() || matches!(self.backend, Backend::Memory(_)) {
            return Ok(0);
        }
        let transaction = self.connection_mut().transaction()?;
        let mut added = 0;
        for insert in inserts {
            let existing: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM knowledge_items WHERE kind=?1 AND content_hash=?2)",
                params![insert.item.kind, insert.content_hash],
                |row| row.get(0),
            )?;
            if existing {
                continue;
            }
            transaction.execute(
                "INSERT INTO knowledge_items(id,kind,title,source_uri,content,content_hash,importance) VALUES (?1,?2,?3,?4,?5,?6,?7)",
                params![insert.item.id,insert.item.kind,insert.item.title,insert.item.source_uri,insert.item.content,insert.content_hash,insert.item.importance],
            )?;
            for (index, (id, content, blob, encoding, dimensions, tokens)) in
                insert.chunks.iter().enumerate()
            {
                transaction.execute(
                    "INSERT INTO knowledge_chunks(id,item_id,chunk_no,content,content_hash,embedding_blob,vector_encoding,dimensions,token_count) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                    params![id,insert.item.id,index as i64,content,sha256(content),blob,encoding,*dimensions as i64,*tokens as i64],
                )?;
            }
            added += 1;
        }
        transaction.commit()?;
        self.secure_database_files()?;
        Ok(added)
    }

    pub fn has_knowledge_hash(&self, kind: &str, hash: &str) -> Result<bool> {
        if matches!(self.backend, Backend::Memory(_)) {
            return Ok(false);
        }
        Ok(self.connection().query_row(
            "SELECT EXISTS(SELECT 1 FROM knowledge_items WHERE kind=?1 AND content_hash=?2)",
            params![kind, hash],
            |row| row.get(0),
        )?)
    }

    pub fn delete_knowledge(&self, id: &str) -> Result<bool> {
        if matches!(self.backend, Backend::Memory(_)) {
            return Ok(false);
        }
        Ok(self
            .connection()
            .execute("DELETE FROM knowledge_items WHERE id=?1", [id])?
            > 0)
    }

    pub fn list_knowledge(&self, kind: Option<&str>) -> Result<Vec<KnowledgeRow>> {
        if matches!(self.backend, Backend::Memory(_)) {
            return Ok(Vec::new());
        }
        let sql = if kind.is_some() {
            "SELECT id,kind,title,source_uri,content,importance FROM knowledge_items WHERE enabled=1 AND kind=?1 ORDER BY updated_at DESC"
        } else {
            "SELECT id,kind,title,source_uri,'' AS content,importance FROM knowledge_items WHERE enabled=1 ORDER BY updated_at DESC"
        };
        let mut statement = self.connection().prepare(sql)?;
        let mapper = |row: &rusqlite::Row<'_>| {
            Ok(KnowledgeRow {
                id: row.get(0)?,
                kind: row.get(1)?,
                title: row.get(2)?,
                source_uri: row.get(3)?,
                content: row.get(4)?,
                importance: row.get(5)?,
            })
        };
        let mut result = Vec::new();
        if let Some(kind) = kind {
            let rows = statement.query_map([kind], mapper)?;
            result.extend(rows.collect::<rusqlite::Result<Vec<_>>>()?);
        } else {
            let rows = statement.query_map([], mapper)?;
            result.extend(rows.collect::<rusqlite::Result<Vec<_>>>()?);
        }
        Ok(result)
    }

    pub fn has_knowledge(&self) -> Result<bool> {
        if matches!(self.backend, Backend::Memory(_)) {
            return Ok(false);
        }
        Ok(self.connection().query_row(
            "SELECT EXISTS(SELECT 1 FROM knowledge_items WHERE enabled=1)",
            [],
            |row| row.get(0),
        )?)
    }

    pub fn visit_vector_rows(
        &self,
        kind: Option<&str>,
        mut visitor: impl FnMut(VectorRow),
    ) -> Result<()> {
        if matches!(self.backend, Backend::Memory(_)) {
            return Ok(());
        }
        let mut statement = self.connection().prepare(
            "SELECT i.id,i.kind,i.title,i.source_uri,i.importance,c.content,c.embedding_blob,c.vector_encoding,c.dimensions FROM knowledge_items i JOIN knowledge_chunks c ON c.item_id=i.id WHERE i.enabled=1 AND (?1 IS NULL OR i.kind=?1)"
        )?;
        let rows = statement.query_map([kind], |row| {
            let blob: Vec<u8> = row.get(6)?;
            let encoding: String = row.get(7)?;
            let dimensions: usize = row.get::<_, i64>(8)? as usize;
            Ok(VectorRow {
                id: row.get(0)?,
                kind: row.get(1)?,
                title: row.get(2)?,
                source_uri: row.get(3)?,
                importance: row.get(4)?,
                chunk_content: row.get(5)?,
                embedding: decode_vector(&blob, &encoding, dimensions),
            })
        })?;
        for row in rows {
            visitor(row?);
        }
        Ok(())
    }

    pub fn checkpoint(&mut self) -> Result<()> {
        if matches!(self.backend, Backend::Memory(_)) {
            return Ok(());
        }
        if !self.pending_audits.is_empty() {
            let audits = self.pending_audits.clone();
            let transaction = self.connection_mut().transaction()?;
            for audit in &audits {
                transaction.execute(
                    "INSERT INTO tool_executions(session_id,tool_call_id,name,args_redacted_json,result_text,status,risk,exit_code,duration_ms) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                    params![audit.session_id,audit.call_id,audit.name,audit.args,audit.result,audit.status,audit.risk,audit.exit_code,audit.duration_ms as i64],
                )?;
            }
            transaction.commit()?;
            self.pending_audits.clear();
        }
        let journal: String =
            self.connection()
                .pragma_query_value(None, "journal_mode", |row| row.get(0))?;
        if journal.eq_ignore_ascii_case("wal") {
            self.connection()
                .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        }
        self.connection().cache_flush()?;
        self.secure_database_files()?;
        Ok(())
    }

    fn secure_database_files(&self) -> Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for path in [
                self.path.clone(),
                PathBuf::from(format!("{}-wal", self.path.display())),
                PathBuf::from(format!("{}-shm", self.path.display())),
            ] {
                match fs::symlink_metadata(&path) {
                    Ok(metadata) => {
                        if metadata.file_type().is_symlink() || !metadata.is_file() {
                            bail!(
                                "Refusing to change permissions on a non-regular database file: {}",
                                path.display()
                            );
                        }
                        if metadata.permissions().mode() & 0o777 != 0o600 {
                            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
                        }
                        if let Some(uid) = self.database_owner_uid {
                            crate::config::set_owner(&path, uid)?;
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(error)
                            .with_context(|| format!("Unable to inspect {}", path.display()));
                    }
                }
            }
        }
        Ok(())
    }
}

impl Drop for StateStore {
    fn drop(&mut self) {
        if matches!(self.backend, Backend::Memory(_)) {
            return;
        }
        let audits = std::mem::take(&mut self.pending_audits);
        if !audits.is_empty() {
            if let Ok(transaction) = self.connection_mut().transaction() {
                for audit in audits {
                    let _ = transaction.execute("INSERT INTO tool_executions(session_id,tool_call_id,name,args_redacted_json,result_text,status,risk,exit_code,duration_ms) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",params![audit.session_id,audit.call_id,audit.name,audit.args,audit.result,audit.status,audit.risk,audit.exit_code,audit.duration_ms as i64]);
                }
                let _ = transaction.commit();
            }
        }
        let _ = self.secure_database_files();
    }
}

#[cfg(unix)]
fn set_directory_permissions(path: &Path, scope: ConfigScope) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = if scope == ConfigScope::User {
        0o700
    } else {
        0o755
    };
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}
#[cfg(not(unix))]
fn set_directory_permissions(_path: &Path, _scope: ConfigScope) -> Result<()> {
    Ok(())
}

fn prepare_database_file(path: &Path) -> Result<()> {
    if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        bail!(
            "Refusing to open a database through a symbolic link: {}",
            path.display()
        );
    }
    if !path.exists() {
        let mut options = OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(path) {
            Ok(file) => file.sync_all()?,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("Unable to create database {}", path.display()));
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn validate_database_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = fs::metadata(path)?;
    if !metadata.is_dir() || metadata.permissions().mode() & 0o022 != 0 {
        bail!(
            "The database directory must not be writable by group or other users: {}",
            path.display()
        );
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_database_directory(_path: &Path) -> Result<()> {
    Ok(())
}

fn open_private_lock(path: &Path) -> std::io::Result<fs::File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW).mode(0o600);
    }
    options.open(path)
}

fn ensure_private_lock_directory(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!(
            "The session-lock path is not a private directory: {}",
            path.display()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn ensure_private_memory_directory(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!(
            "The session-state path is not a private directory: {}",
            path.display()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.uid() != effective_uid() {
            bail!(
                "The session-state directory is not owned by the current user: {}",
                path.display()
            );
        }
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(unix)]
fn sync_state_directory(path: &Path) -> Result<()> {
    fs::File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_state_directory(_path: &Path) -> Result<()> {
    Ok(())
}

fn effective_uid() -> u32 {
    #[cfg(unix)]
    {
        // SAFETY: geteuid has no preconditions and does not modify memory.
        unsafe { libc::geteuid() }
    }
    #[cfg(not(unix))]
    {
        0
    }
}

fn choose_journal(config: &Config, path: &Path) -> &'static str {
    match config.storage.journal_mode.to_ascii_lowercase().as_str() {
        "wal" => "WAL",
        "persist" => "PERSIST",
        "delete" => "DELETE",
        _ if is_openwrt() || path.starts_with("/etc") => "PERSIST",
        _ => "WAL",
    }
}

fn is_openwrt() -> bool {
    Path::new("/etc/openwrt_release").exists() || Path::new("/etc/openwrt_version").exists()
}

pub fn sha256(content: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(content.as_bytes()))
}

fn decode_vector(blob: &[u8], encoding: &str, dimensions: usize) -> Vec<f32> {
    if encoding == "f16" {
        if blob.len() != dimensions.saturating_mul(2) {
            return Vec::new();
        }
        blob.chunks_exact(2)
            .take(dimensions)
            .map(|bytes| half::f16::from_le_bytes([bytes[0], bytes[1]]).to_f32())
            .collect()
    } else {
        if encoding != "f32" || blob.len() != dimensions.saturating_mul(4) {
            return Vec::new();
        }
        blob.chunks_exact(4)
            .take(dimensions)
            .map(|bytes| f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ConfigPathResolver;

    fn store() -> (tempfile::TempDir, StateStore) {
        let dir = tempfile::tempdir().unwrap();
        let resolver =
            ConfigPathResolver::new(Some(dir.path().join("config.toml")), false).unwrap();
        let mut config = Config::default();
        config.storage.enabled = true;
        config.storage.database = "test.db".into();
        let store = StateStore::open(&config, &resolver).unwrap();
        (dir, store)
    }

    fn memory_store() -> (tempfile::TempDir, StateStore) {
        let dir = tempfile::tempdir().unwrap();
        reopen_memory_store(dir)
    }

    /// storage.enabled defaults to false: a tmpfs session file replaces
    /// SQLite; data_dir redirects it into the test tempdir for isolation.
    fn reopen_memory_store(dir: tempfile::TempDir) -> (tempfile::TempDir, StateStore) {
        let resolver =
            ConfigPathResolver::new(Some(dir.path().join("config.toml")), false).unwrap();
        let mut config = Config::default();
        config.storage.data_dir = dir.path().to_string_lossy().into_owned();
        let store = StateStore::open(&config, &resolver).unwrap();
        (dir, store)
    }

    fn user_message(content: &str) -> StoredMessage {
        StoredMessage {
            role: "user".into(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    fn assistant_tool_message(call_id: &str) -> StoredMessage {
        StoredMessage {
            role: "assistant".into(),
            content: None,
            tool_calls: Some(
                json!([{
                    "id": call_id,
                    "type": "function",
                    "function": {
                        "name": "write_file",
                        "arguments": "{\"path\":\"note.txt\",\"content\":\"hello\"}"
                    }
                }])
                .to_string(),
            ),
            tool_call_id: None,
        }
    }

    fn memory_session_with_timestamp(timestamp: &str) -> MemorySession {
        let mut session = MemorySession::new(Path::new("/tmp"), None);
        session.updated_at = timestamp.into();
        session
    }

    #[test]
    fn newer_local_fallback_wins_when_redis_returns() {
        let remote = memory_session_with_timestamp("2026-08-13 10:00:00");
        let local = memory_session_with_timestamp("2026-08-13 10:01:00");
        let local_id = local.id.clone();
        let (selected, migrate) = select_redis_session(Some(remote), Some(local));
        assert!(migrate);
        assert_eq!(selected.unwrap().id, local_id);
    }

    #[test]
    fn longer_local_fallback_wins_with_same_timestamp() {
        let remote = memory_session_with_timestamp("2026-08-13 10:00:00");
        let mut local = remote.clone();
        local.messages.push(SequencedMessage {
            seq: 1,
            message: user_message("written during outage"),
        });
        let (selected, migrate) = select_redis_session(Some(remote), Some(local));
        assert!(migrate);
        assert_eq!(selected.unwrap().messages.len(), 1);
    }

    #[test]
    fn invalid_redis_state_is_not_an_availability_error() {
        let error = decode_redis_session(b"not-json").err().unwrap();
        assert!(error.downcast_ref::<InvalidRedisState>().is_some());
    }

    #[test]
    fn request_snapshots_are_typed_and_session_invariants_are_rebuilt() {
        let (_dir, mut store) = store();
        let session = store
            .new_session(Path::new("/tmp"), Some("request-snapshot"))
            .unwrap();
        let turn = "turn-request-snapshot";
        store
            .start_turn(&session, turn, &user_message("inspect"), Path::new("/tmp"))
            .unwrap();
        let mut snapshot = RequestSnapshot {
            iteration: 0,
            model: "test-model".into(),
            max_tokens: 256,
            stream: false,
            tool_choice: Some("auto".into()),
            messages: json!([{"role":"user","content":"inspect"}]),
            tools: json!([]),
            input_sha256: String::new(),
        };
        snapshot.input_sha256 = request_snapshot_hash(&snapshot).unwrap();
        store
            .append_request_snapshot(&session, turn, &snapshot)
            .unwrap();
        store
            .append_assistant_message(
                &session,
                turn,
                &StoredMessage {
                    role: "assistant".into(),
                    content: Some("done".into()),
                    tool_calls: None,
                    tool_call_id: None,
                },
            )
            .unwrap();
        store
            .finish_turn(&session, turn, "completed", None)
            .unwrap();
        store.validate_session(&session).unwrap();
        let events = store.load_events(&session).unwrap();
        assert!(
            events
                .iter()
                .any(|event| event.kind == EventKind::RequestHeader)
        );
        assert_eq!(events[2].kind, EventKind::RequestHeader);
    }

    #[test]
    fn oversized_request_snapshots_stay_byte_exact() {
        let (_dir, mut store) = store();
        let session = store
            .new_session(Path::new("/tmp"), Some("snapshot"))
            .unwrap();
        let turn = "turn-big-snapshot";
        store
            .start_turn(&session, turn, &user_message("inspect"), Path::new("/tmp"))
            .unwrap();
        let big_content = "x".repeat(MAX_EVENT_DATA_BYTES + 1);
        let mut snapshot = RequestSnapshot {
            iteration: 0,
            model: "test-model".into(),
            max_tokens: 256,
            stream: false,
            tool_choice: Some("auto".into()),
            messages: json!([{"role":"user","content":big_content.clone()}]),
            tools: json!([]),
            input_sha256: String::new(),
        };
        snapshot.input_sha256 = request_snapshot_hash(&snapshot).unwrap();
        store
            .append_request_snapshot(&session, turn, &snapshot)
            .unwrap();
        let events = store.load_events(&session).unwrap();
        let header = events
            .iter()
            .find(|event| event.kind == EventKind::RequestHeader)
            .unwrap();
        // The envelope is neither truncated nor rejected past 1 MiB, so its
        // input hash keeps matching on reload.
        assert!(header.data.get("event_content_truncated").is_none());
        assert_eq!(
            header.data["messages"][0]["content"].as_str().map(str::len),
            Some(big_content.len())
        );
        store
            .finish_turn(&session, turn, "completed", None)
            .unwrap();
        store.validate_session(&session).unwrap();
    }

    #[test]
    fn approval_events_are_paired_and_linked_to_the_tool_call() {
        let (_dir, mut store) = store();
        let session = store
            .new_session(Path::new("/tmp"), Some("approval"))
            .unwrap();
        store
            .start_turn(
                &session,
                "turn-approval",
                &user_message("edit the file"),
                Path::new("/tmp"),
            )
            .unwrap();
        store
            .append_approval_asked(&ApprovalRequest {
                session_id: &session,
                turn_id: "turn-approval",
                tool_call_id: "call-approval",
                approval_id: "approval-1",
                tool_name: "shell",
                reason: "Allow this command? [y/N/All] ",
                high_risk: false,
                allow_all: true,
            })
            .unwrap();
        store
            .append_approval_decided(
                &session,
                "turn-approval",
                "call-approval",
                "approval-1",
                ApprovalOutcome::AllowedForTask,
            )
            .unwrap();

        let events = store.load_events(&session).unwrap();
        assert_eq!(events[2].kind, EventKind::ApprovalAsked);
        assert_eq!(events[3].kind, EventKind::ApprovalDecided);
        assert_eq!(events[2].tool_call_id.as_deref(), Some("call-approval"));
        assert_eq!(events[3].tool_call_id.as_deref(), Some("call-approval"));
        assert_eq!(events[2].data["approval_id"], "approval-1");
        assert_eq!(events[2].data["allow_all"], true);
        assert_eq!(events[3].data["outcome"], "allowed-for-task");
    }

    #[test]
    fn approval_events_survive_the_memory_session_backend() {
        let (dir, mut store) = memory_store();
        let session = store.ensure_current_session(Path::new("/tmp")).unwrap();
        store
            .start_turn(
                &session,
                "turn-memory-approval",
                &user_message("edit the file"),
                Path::new("/tmp"),
            )
            .unwrap();
        store
            .append_approval_asked(&ApprovalRequest {
                session_id: &session,
                turn_id: "turn-memory-approval",
                tool_call_id: "call-memory-approval",
                approval_id: "approval-memory-1",
                tool_name: "apply_patch",
                reason: "modify file",
                high_risk: false,
                allow_all: false,
            })
            .unwrap();
        store
            .append_approval_decided(
                &session,
                "turn-memory-approval",
                "call-memory-approval",
                "approval-memory-1",
                ApprovalOutcome::Rejected,
            )
            .unwrap();
        drop(store);

        let (_dir, reopened) = reopen_memory_store(dir);
        let events = reopened.load_events(&session).unwrap();
        assert_eq!(events[2].kind, EventKind::ApprovalAsked);
        assert_eq!(events[3].data["outcome"], "rejected");
    }

    #[test]
    fn event_log_is_incremental_and_recovers_open_tool_without_replaying_it() {
        let (_dir, mut store) = store();
        let session = store
            .new_session(Path::new("/tmp"), Some("events"))
            .unwrap();
        let turn = "turn-1";
        store
            .start_turn(
                &session,
                turn,
                &user_message("write a note"),
                Path::new("/tmp"),
            )
            .unwrap();
        store
            .append_assistant_message(&session, turn, &assistant_tool_message("call-1"))
            .unwrap();
        store
            .append_tool_call(
                &session,
                turn,
                "call-1",
                "write_file",
                r#"{"path":"note.txt","content":"hello"}"#,
            )
            .unwrap();

        let events = store.load_events(&session).unwrap();
        let kinds = events
            .iter()
            .map(|event| event.kind.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            kinds,
            vec![
                "turn/start",
                "user/message",
                "assistant/message",
                "tool/call"
            ]
        );
        assert!(events[3].data.get("arguments").is_none());
        assert!(events[3].data.get("arguments_sha256").is_some());

        assert_eq!(store.recover_session(&session).unwrap(), 2);
        let events = store.load_events(&session).unwrap();
        assert_eq!(
            events.last().map(|event| event.kind.as_str()),
            Some("turn/end")
        );
        assert_eq!(store.recover_session(&session).unwrap(), 0);
        let messages = store.load_messages(&session).unwrap();
        assert_eq!(messages.len(), 3);
        assert!(
            messages[2]
                .content
                .as_deref()
                .is_some_and(|content| content.contains("external state is unknown"))
        );
    }

    #[test]
    fn oversized_message_content_is_truncated_in_the_event_copy_only() {
        let (_dir, mut store) = store();
        let session = store
            .new_session(Path::new("/tmp"), Some("events"))
            .unwrap();
        let turn = "turn-big";
        store
            .start_turn(&session, turn, &user_message("go"), Path::new("/tmp"))
            .unwrap();
        store
            .append_assistant_message(&session, turn, &assistant_tool_message("call-big"))
            .unwrap();
        let big = "x".repeat(MAX_EVENT_DATA_BYTES * 2);
        let result = StoredMessage {
            role: "tool".into(),
            content: Some(big.clone()),
            tool_calls: None,
            tool_call_id: Some("call-big".into()),
        };
        store
            .append_tool_result(
                &session,
                turn,
                "call-big",
                &result,
                ToolResultMetadata {
                    status: "completed",
                    exit_code: Some(0),
                    duration_ms: 1,
                    presentation: None,
                },
            )
            .unwrap();

        // The materialized conversation keeps the full content.
        let messages = store.load_messages(&session).unwrap();
        assert_eq!(
            messages.last().and_then(|m| m.content.as_deref()),
            Some(big.as_str())
        );
        // The event copy is truncated and marked instead of failing the turn.
        let events = store.load_events(&session).unwrap();
        let event = events
            .iter()
            .find(|event| event.kind == EventKind::ToolResult)
            .unwrap();
        let content = event.data["message"]["content"].as_str().unwrap();
        assert!(content.len() < big.len());
        assert!(content.contains("content truncated in the event log"));
        assert_eq!(
            event.data["message"]["event_content_truncated"],
            Value::Bool(true)
        );
    }

    #[test]
    fn recovery_reemits_a_missing_tool_call_with_the_truthful_arguments_hash() {
        let (_dir, mut store) = store();
        let session = store
            .new_session(Path::new("/tmp"), Some("events"))
            .unwrap();
        let turn = "turn-crash-before-call";
        store
            .start_turn(&session, turn, &user_message("write"), Path::new("/tmp"))
            .unwrap();
        store
            .append_assistant_message(&session, turn, &assistant_tool_message("call-1"))
            .unwrap();
        // Simulated crash: the process died before the tool/call event.

        assert_eq!(store.recover_session(&session).unwrap(), 2);
        let events = store.load_events(&session).unwrap();
        let call_event = events
            .iter()
            .find(|event| event.kind == EventKind::ToolCall)
            .unwrap();
        let real_arguments = r#"{"path":"note.txt","content":"hello"}"#;
        assert_eq!(
            call_event.data["arguments_sha256"].as_str(),
            Some(sha256(real_arguments).as_str())
        );
        assert_eq!(
            call_event.data["arguments_bytes"].as_i64(),
            Some(real_arguments.len() as i64)
        );
        let messages = store.load_messages(&session).unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(store.recover_session(&session).unwrap(), 0);
    }

    #[test]
    fn memory_event_log_survives_restart_before_recovery() {
        let (dir, mut store) = memory_store();
        let session = store.ensure_current_session(Path::new("/tmp")).unwrap();
        store
            .start_turn(
                &session,
                "turn-memory",
                &user_message("inspect"),
                Path::new("/tmp"),
            )
            .unwrap();
        store
            .append_assistant_message(
                &session,
                "turn-memory",
                &assistant_tool_message("call-memory"),
            )
            .unwrap();
        store
            .append_tool_call(&session, "turn-memory", "call-memory", "write_file", "{}")
            .unwrap();
        drop(store);

        let (_dir, mut reopened) = reopen_memory_store(dir);
        assert_eq!(reopened.load_events(&session).unwrap().len(), 4);
        assert_eq!(reopened.recover_session(&session).unwrap(), 2);
        assert_eq!(reopened.recover_session(&session).unwrap(), 0);
    }

    #[test]
    fn memory_mode_persists_one_session_across_processes() {
        let (dir, mut store) = memory_store();
        let first = store.new_session(Path::new("/tmp"), Some("first")).unwrap();
        store
            .append_messages(&first, &[user_message("hello")], Path::new("/tmp"))
            .unwrap();
        assert_eq!(store.load_messages(&first).unwrap().len(), 1);
        store.lock_session(&first).unwrap();
        drop(store);

        // A new process (a fresh StateStore) restores the previous session.
        let (dir, mut store) = reopen_memory_store(dir);
        assert_eq!(
            store.current_session().unwrap().as_deref(),
            Some(first.as_str())
        );
        assert_eq!(store.load_messages(&first).unwrap().len(), 1);
        assert!(dir.path().join("qin-session.json").is_file());

        // Starting a new session wipes the previous one completely.
        let second = store
            .new_session(Path::new("/tmp"), Some("second"))
            .unwrap();
        assert_ne!(first, second);
        assert_eq!(
            store.current_session().unwrap().as_deref(),
            Some(second.as_str())
        );
        assert!(store.load_messages(&first).unwrap().is_empty());
        assert!(store.resolve_session_id(&first).is_err());
        assert_eq!(store.list_sessions(10).unwrap().len(), 1);
    }

    #[test]
    fn unavailable_redis_falls_back_to_the_json_memory_store() {
        let dir = tempfile::tempdir().unwrap();
        let resolver =
            ConfigPathResolver::new(Some(dir.path().join("config.toml")), false).unwrap();
        let mut config = Config::default();
        config.storage.data_dir = dir.path().to_string_lossy().into_owned();
        config.storage.redis.enabled = true;
        config.storage.redis.url = "not-a-redis-url".into();
        let mut store = StateStore::open(&config, &resolver).unwrap();
        assert_eq!(store.backend_label(), "tmpfs-json");
        assert!(store.notice().is_some());
        let id = store.ensure_current_session(Path::new("/tmp")).unwrap();
        store
            .append_messages(&id, &[user_message("fallback")], Path::new("/tmp"))
            .unwrap();
        assert!(dir.path().join(MEMORY_FILE_NAME).is_file());
    }

    #[cfg(unix)]
    #[test]
    fn memory_state_refuses_symbolic_links() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.json");
        fs::write(
            &target,
            serde_json::to_vec(&MemoryFile {
                version: MEMORY_FILE_VERSION,
                session: None,
            })
            .unwrap(),
        )
        .unwrap();
        let link = dir.path().join(MEMORY_FILE_NAME);
        symlink(&target, &link).unwrap();
        assert!(load_memory_session(&link).is_err());
    }

    #[test]
    fn memory_mode_applies_summaries_and_disables_knowledge() {
        let (dir, mut store) = memory_store();
        let id = store.ensure_current_session(Path::new("/tmp")).unwrap();
        store
            .append_messages(
                &id,
                &[user_message("m1"), user_message("m2"), user_message("m3")],
                Path::new("/tmp"),
            )
            .unwrap();
        store
            .append_messages_with_summary(
                &id,
                &[user_message("m4")],
                Path::new("/tmp"),
                Some(&SummaryUpdate {
                    content: "summary text".into(),
                    through_seq: 2,
                }),
            )
            .unwrap();
        assert_eq!(store.summary(&id).unwrap().as_deref(), Some("summary text"));
        let context = store.load_context_messages(&id).unwrap();
        assert_eq!(context.len(), 2);
        assert_eq!(context[0].seq, 3);
        assert_eq!(store.user_turn_count(&id).unwrap(), 4);

        // Summaries survive a process restart as well.
        drop(store);
        let (_dir, mut store) = reopen_memory_store(dir);
        assert_eq!(store.summary(&id).unwrap().as_deref(), Some("summary text"));
        assert_eq!(store.load_context_messages(&id).unwrap().len(), 2);

        // Knowledge APIs are inert in memory mode.
        assert!(!store.has_knowledge().unwrap());
        assert!(store.list_knowledge(None).unwrap().is_empty());
        assert!(!store.delete_knowledge("anything").unwrap());
        store.checkpoint().unwrap();
    }

    #[test]
    fn creates_and_restores_session() {
        let (_dir, mut store) = store();
        let id = store.new_session(Path::new("/tmp"), Some("test")).unwrap();
        store
            .append_messages(
                &id,
                &[StoredMessage {
                    role: "user".into(),
                    content: Some("hello".into()),
                    tool_calls: None,
                    tool_call_id: None,
                }],
                Path::new("/tmp"),
            )
            .unwrap();
        assert_eq!(
            store.current_session().unwrap().as_deref(),
            Some(id.as_str())
        );
        assert_eq!(store.load_messages(&id).unwrap().len(), 1);
    }

    #[test]
    fn deletes_sessions_by_displayed_prefix_and_creates_a_new_active_session() {
        let (_dir, mut store) = store();
        let first = store.new_session(Path::new("/tmp"), Some("first")).unwrap();
        let second = store
            .new_session(Path::new("/tmp"), Some("second"))
            .unwrap();
        store
            .append_messages(
                &second,
                &[StoredMessage {
                    role: "user".into(),
                    content: Some("remove me".into()),
                    tool_calls: None,
                    tool_call_id: None,
                }],
                Path::new("/tmp"),
            )
            .unwrap();

        let (deleted, replacement) = store
            .delete_session(&second[..8], Path::new("/replacement"))
            .unwrap();
        assert_eq!(deleted, second);
        let replacement = replacement.unwrap();
        assert_ne!(replacement, first);
        assert_eq!(
            store.current_session().unwrap().as_deref(),
            Some(replacement.as_str())
        );
        assert!(store.load_messages(&second).unwrap().is_empty());
        let (deleted, new_current) = store
            .delete_session(&first[..8], Path::new("/tmp"))
            .unwrap();
        assert_eq!(deleted, first);
        assert_eq!(new_current, None);
        assert_eq!(store.current_session().unwrap(), Some(replacement));
        assert_eq!(store.list_sessions(10).unwrap().len(), 1);
        assert!(store.delete_session("missing", Path::new("/tmp")).is_err());
    }

    #[test]
    fn session_lock_is_exclusive_and_released() {
        let (_dir, store) = store();
        let first = store.lock_session("abc").unwrap();
        assert!(store.lock_session("abc").is_err());
        drop(first);
        assert!(store.lock_session("abc").is_ok());
    }

    #[test]
    fn summary_boundary_keeps_full_history_but_limits_context() {
        let (_dir, mut store) = store();
        let id = store
            .new_session(Path::new("/tmp"), Some("summary"))
            .unwrap();
        let messages = ["one", "two", "three"]
            .into_iter()
            .map(|content| StoredMessage {
                role: "user".into(),
                content: Some(content.into()),
                tool_calls: None,
                tool_call_id: None,
            })
            .collect::<Vec<_>>();
        store
            .append_messages_with_summary(
                &id,
                &messages,
                Path::new("/tmp"),
                Some(&SummaryUpdate {
                    content: "first two".into(),
                    through_seq: 2,
                }),
            )
            .unwrap();
        assert_eq!(store.load_messages(&id).unwrap().len(), 3);
        let context = store.load_context_messages(&id).unwrap();
        assert_eq!(context.len(), 1);
        assert_eq!(context[0].message.content.as_deref(), Some("three"));
    }

    #[test]
    fn rejects_database_symlinks() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let dir = tempfile::tempdir().unwrap();
            let target = dir.path().join("target.db");
            fs::write(&target, []).unwrap();
            symlink(&target, dir.path().join("link.db")).unwrap();
            let resolver =
                ConfigPathResolver::new(Some(dir.path().join("config.toml")), false).unwrap();
            let mut config = Config::default();
            config.storage.enabled = true;
            config.storage.database = "link.db".into();
            assert!(StateStore::open(&config, &resolver).is_err());
        }
    }
}
