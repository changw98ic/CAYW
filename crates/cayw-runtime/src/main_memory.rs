//! The main session's durable base memory and user preferences.
//!
//! The only authoritative location is `<main-session-workspace>/memory/`. Each
//! entry is one file with a small header and a body. The program writes entries
//! only after it verifies every cited source is a user-input conversation turn.
//! Tool output, web pages and execution results can never become a source.
//!
//! User edits are detected on the next read. The program appends one change
//! event and marks the entry `user_set`; background writes do not overwrite it.
//!
//! Forgotten cursor ranges are appended as new events. Original events and raw
//! logs are never rewritten or deleted. Later provider input, prefetch, summary,
//! compression and memory reads skip those ranges; reading by cursor still
//! returns the original records.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write,
    path::{Component, Path, PathBuf},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

use cayw_agent::AgentEvent;
use cayw_ai::Message;
use cayw_context::{
    CursorRange, DEFAULT_PROTOCOL_MARGIN, DreamPlan, SourceRange, SummaryRecord, input_budget,
    organize_dream, plan_main_session,
};
use cayw_core::{CoreEvent, EventRecord};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::RuntimeError;

pub const MEMORY_DIRECTORY: &str = "memory";
const EVENTS_FILE: &str = "events.jsonl";
const ENTRY_SUFFIX: &str = ".md";

/// Where one memory entry came from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceType {
    /// The user said it directly.
    #[default]
    Stated,
    /// Inferred from the user's conversation.
    Inferred,
    /// The user set or edited the entry directly.
    UserSet,
}

/// One durable memory or preference entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub entry_id: String,
    pub kind: String,
    pub source_type: SourceType,
    pub source_log_ids: Vec<String>,
    pub status: MemoryStatus,
    pub updated_at: u64,
    #[serde(default)]
    pub body: String,
    /// SHA-256 of the current file bytes. It is the entry version.
    #[serde(default)]
    pub version: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryStatus {
    Active,
    Inactive,
}

/// A verified user-input turn. Only [`user_evidence`] creates these, so a
/// caller cannot present tool output as a user source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserSource {
    session_id: String,
    cursor: u64,
    log_id: String,
    text: String,
}

impl UserSource {
    pub fn log_id(&self) -> &str {
        &self.log_id
    }
    pub fn session_id(&self) -> &str {
        &self.session_id
    }
    pub fn cursor(&self) -> u64 {
        self.cursor
    }
    pub fn text(&self) -> &str {
        &self.text
    }
}

#[derive(Debug, Clone, Default)]
pub struct UserEvidence {
    sources: BTreeMap<String, UserSource>,
}

impl UserEvidence {
    pub fn sources(&self) -> impl Iterator<Item = &UserSource> {
        self.sources.values()
    }
    fn contains(&self, log_id: &str) -> bool {
        self.sources.contains_key(log_id)
    }
}

/// Collect the only admissible memory sources from durable events: messages
/// whose role is the user. Assistant messages, tool results, host events and
/// every other record are ignored.
pub fn user_evidence(events: &[EventRecord]) -> UserEvidence {
    let mut evidence = UserEvidence::default();
    for record in events {
        let CoreEvent::Agent { event } = &record.event else {
            continue;
        };
        let user = match event.as_ref() {
            AgentEvent::MessageStart {
                message: message @ Message::User { .. },
            }
            | AgentEvent::MessageEnd {
                message: message @ Message::User { .. },
            } => message,
            _ => continue,
        };
        let log_id = format!("{}:{}", record.session_id, record.cursor);
        evidence.sources.insert(
            log_id.clone(),
            UserSource {
                session_id: record.session_id.clone(),
                cursor: record.cursor,
                log_id,
                text: user.text(),
            },
        );
    }
    evidence
}

/// Collect the exact messages whose durable events fall inside a forgotten
/// range. Provider input then drops the matching turns without rewriting the
/// event log. Later events with identical text stay available.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryOperation {
    #[default]
    Upsert,
    Disable,
}

/// A proposed background write. `source_log_ids` must all be verified user
/// input. The writer itself never emits `user_set`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryCandidate {
    #[serde(default)]
    pub operation: MemoryOperation,
    #[serde(default)]
    pub entry_id: Option<String>,
    pub kind: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub source_type: SourceType,
    #[serde(default)]
    pub source_log_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplyReport {
    pub written: Vec<String>,
    pub rejected: Vec<RejectedCandidate>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RejectedCandidate {
    pub entry_id: Option<String>,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryStatusView {
    pub memory_directory: String,
    pub entries: usize,
    pub active: usize,
    pub user_set: usize,
    pub damaged: usize,
    pub forgotten_ranges: Vec<SourceRange>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryScan {
    pub entries: Vec<MemoryEntry>,
    pub user_edits: Vec<String>,
    pub removed: Vec<String>,
    pub damaged: usize,
}

/// Where one queried source item came from. It is a label only: only `User`
/// items can be cited by a memory entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryOrigin {
    User,
    Assistant,
    Tool,
}

/// One durable event item returned by a scope query. `log_id` is the original
/// `(session_id, cursor)` identity; the query never rewrites or renumbers it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryEvent {
    pub log_id: String,
    pub session_id: String,
    pub cursor: u64,
    pub origin: MemoryOrigin,
    pub text: String,
}

/// A read-only scope query. `start_cursor` and `end_cursor` are inclusive;
/// `0` means the first event and `None` means through the last event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryQuery {
    pub session_id: String,
    #[serde(default)]
    pub start_cursor: u64,
    #[serde(default)]
    pub end_cursor: Option<u64>,
}

impl MemoryQuery {
    pub fn new(session_id: impl Into<String>, start_cursor: u64, end_cursor: Option<u64>) -> Self {
        Self {
            session_id: session_id.into(),
            start_cursor,
            end_cursor,
        }
    }

    fn validate(&self) -> Result<(), RuntimeError> {
        if self.session_id.is_empty()
            || self.session_id.contains(':')
            || self.session_id.chars().any(char::is_control)
        {
            return Err(RuntimeError::InvalidInput(
                "query session id must be nonempty and must not contain ':'".into(),
            ));
        }
        if self
            .end_cursor
            .is_some_and(|end| end == 0 || end < self.start_cursor)
        {
            return Err(RuntimeError::InvalidInput(
                "query range needs a positive cursor span".into(),
            ));
        }
        Ok(())
    }
}

/// One memory entry returned by a query. The body is included because the query
/// is how a caller reads back a recorded decision or preference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryQueryEntry {
    pub entry_id: String,
    pub kind: String,
    pub source_type: SourceType,
    pub source_log_ids: Vec<String>,
    pub status: MemoryStatus,
    pub updated_at: u64,
    pub version: String,
    pub body: String,
}

impl From<&MemoryEntry> for MemoryQueryEntry {
    fn from(entry: &MemoryEntry) -> Self {
        Self {
            entry_id: entry.entry_id.clone(),
            kind: entry.kind.clone(),
            source_type: entry.source_type,
            source_log_ids: entry.source_log_ids.clone(),
            status: entry.status,
            updated_at: entry.updated_at,
            version: entry.version.clone(),
            body: entry.body.clone(),
        }
    }
}

/// Result of a scope query. Forgotten ranges are absent by default; the
/// ordinary cursor read (`session.events`) still returns the original records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryQueryView {
    pub session_id: String,
    pub start_cursor: u64,
    pub end_cursor: u64,
    pub events: Vec<MemoryEvent>,
    pub entries: Vec<MemoryQueryEntry>,
    pub damaged: usize,
}

/// Durable memory for one main-session workspace.
pub struct MainMemory {
    workspace: PathBuf,
    directory: PathBuf,
    state: Mutex<MemoryState>,
}

#[derive(Default)]
struct MemoryState {
    known_versions: BTreeMap<String, String>,
    entries: BTreeMap<String, MemoryEntry>,
    forgotten: Vec<SourceRange>,
    damaged: usize,
}

#[allow(dead_code)] // `at`, `reason` and the per-event audit fields are read from disk.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum StoredEvent {
    MemoryCreated {
        entry_id: String,
        version: String,
        at: u64,
    },
    MemoryUpdated {
        entry_id: String,
        version: String,
        at: u64,
    },
    UserEdited {
        entry_id: String,
        version: String,
        at: u64,
    },
    MemoryDisabled {
        entry_id: String,
        version: String,
        at: u64,
    },
    EntryRemoved {
        entry_id: String,
        at: u64,
    },
    Forget {
        range: SourceRange,
        reason: String,
        at: u64,
    },
}

impl MainMemory {
    /// Resolve the memory directory without creating it. Missing directories
    /// read as empty memory.
    pub fn open(workspace: &Path) -> Self {
        Self {
            workspace: workspace.to_path_buf(),
            directory: workspace.join(MEMORY_DIRECTORY),
            state: Mutex::new(MemoryState::default()),
        }
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    pub fn memory_directory(&self) -> &Path {
        &self.directory
    }

    /// True when a model file capability targets `memory/`. The main session's
    /// file capability is read-only for that directory.
    pub fn model_write_blocked(&self, capability: &str, arguments: &serde_json::Value) -> bool {
        if !mutating_capability(capability) {
            return false;
        }
        candidate_paths(arguments)
            .into_iter()
            .any(|path| self.is_memory_path(&path))
    }

    fn is_memory_path(&self, path: &str) -> bool {
        if path.is_empty() {
            return false;
        }
        let candidate = Path::new(path);
        if candidate.is_absolute() {
            return candidate.starts_with(&self.directory);
        }
        let mut normal = Vec::new();
        for component in candidate.components() {
            match component {
                Component::Normal(part) => normal.push(part.to_string_lossy().into_owned()),
                Component::ParentDir => {
                    normal.pop();
                }
                Component::CurDir | Component::RootDir | Component::Prefix(_) => {}
            }
        }
        normal
            .first()
            .is_some_and(|first| first == MEMORY_DIRECTORY)
    }

    /// Read every entry, detecting user edits and deletions. A changed file
    /// appends one event and is marked `user_set`; the body is preserved.
    pub fn scan(&self) -> Result<MemoryScan, RuntimeError> {
        let mut state = crate::lock(&self.state);
        reload_events(&self.directory, &mut state)?;
        let files = entry_files(&self.directory)?;
        let mut seen = BTreeSet::new();
        let mut entries = Vec::new();
        let mut user_edits = Vec::new();
        let mut removed = Vec::new();
        for path in files {
            let Some(entry_id) = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .map(str::to_owned)
            else {
                state.damaged = state.damaged.saturating_add(1);
                continue;
            };
            if !safe_entry_id(&entry_id) {
                state.damaged = state.damaged.saturating_add(1);
                continue;
            }
            let bytes = fs::read(&path)
                .map_err(|error| RuntimeError::Workspace(format!("read memory entry: {error}")))?;
            let Some(mut entry) = parse_entry(&bytes) else {
                state.damaged = state.damaged.saturating_add(1);
                continue;
            };
            if entry.entry_id != entry_id {
                state.damaged = state.damaged.saturating_add(1);
                continue;
            }
            seen.insert(entry_id.clone());
            let digest = sha256_hex(&bytes);
            let known = state.known_versions.get(&entry_id).cloned();
            if known.as_deref() != Some(digest.as_str()) {
                let first = known.is_none();
                entry.source_type = SourceType::UserSet;
                entry.updated_at = now_ms();
                let rewritten = serialize_entry(&entry);
                write_private(&path, &rewritten)?;
                let version = sha256_hex(&rewritten);
                append_event(
                    &self.directory,
                    serde_json::json!({
                        "type": if first { "memory_created" } else { "user_edited" },
                        "entry_id": entry_id,
                        "version": version,
                        "at": now_ms(),
                    }),
                )?;
                state
                    .known_versions
                    .insert(entry_id.clone(), version.clone());
                entry.version = version;
                if !first {
                    user_edits.push(entry_id.clone());
                }
            } else {
                entry.version = digest;
            }
            state.entries.insert(entry_id.clone(), entry.clone());
            entries.push(entry);
        }
        let stale = state
            .entries
            .keys()
            .filter(|id| !seen.contains(*id))
            .cloned()
            .collect::<Vec<_>>();
        for entry_id in stale {
            append_event(
                &self.directory,
                serde_json::json!({
                    "type":"entry_removed",
                    "entry_id": entry_id,
                    "at": now_ms(),
                }),
            )?;
            state.entries.remove(&entry_id);
            state.known_versions.remove(&entry_id);
            removed.push(entry_id);
        }
        Ok(MemoryScan {
            entries,
            user_edits,
            removed,
            damaged: state.damaged,
        })
    }

    pub fn entries(&self) -> Result<Vec<MemoryEntry>, RuntimeError> {
        Ok(self.scan()?.entries)
    }

    /// Active entries that may enter context. An entry whose provenance touches
    /// a forgotten range is not loaded; it stays on disk.
    pub fn visible_entries(&self) -> Result<Vec<MemoryEntry>, RuntimeError> {
        self.scan()?;
        let state = crate::lock(&self.state);
        Ok(state
            .entries
            .values()
            .filter(|entry| entry.status == MemoryStatus::Active)
            .filter(|entry| !entry_has_forgotten_source(entry, &state.forgotten))
            .cloned()
            .collect())
    }

    pub fn status(&self) -> Result<MemoryStatusView, RuntimeError> {
        self.scan()?;
        let state = crate::lock(&self.state);
        let entries = state.entries.values().collect::<Vec<_>>();
        Ok(MemoryStatusView {
            memory_directory: self.directory.display().to_string(),
            entries: entries.len(),
            active: entries
                .iter()
                .filter(|entry| entry.status == MemoryStatus::Active)
                .count(),
            user_set: entries
                .iter()
                .filter(|entry| entry.source_type == SourceType::UserSet)
                .count(),
            damaged: state.damaged,
            forgotten_ranges: state.forgotten.clone(),
        })
    }

    pub fn forgotten_ranges(&self) -> Result<Vec<SourceRange>, RuntimeError> {
        self.scan()?;
        let state = crate::lock(&self.state);
        Ok(state.forgotten.clone())
    }

    pub fn forgotten_cursor_ranges(
        &self,
        session_id: &str,
    ) -> Result<Vec<CursorRange>, RuntimeError> {
        Ok(self
            .forgotten_ranges()?
            .iter()
            .filter(|range| range.session_id == session_id)
            .filter_map(|range| CursorRange::new(range.start_cursor, range.end_cursor).ok())
            .collect())
    }

    /// Read back conversation, work and recorded decisions inside one source
    /// range. This is read-only: an entry file the user edited is reported as
    /// `user_set` from its current bytes but is not rewritten and no event is
    /// appended. A forgotten range is absent from the result; the ordinary
    /// cursor read still returns its original events.
    pub fn query(
        &self,
        query: &MemoryQuery,
        events: &[EventRecord],
    ) -> Result<MemoryQueryView, RuntimeError> {
        query.validate()?;
        let mut state = crate::lock(&self.state);
        reload_events(&self.directory, &mut state)?;
        let mut damaged = state.damaged;
        let entries = read_entries(&self.directory, &state.known_versions, &mut damaged)?;
        let session_end = events
            .iter()
            .filter(|record| record.session_id == query.session_id)
            .map(|record| record.cursor)
            .max()
            .unwrap_or(0);
        let end_cursor = query.end_cursor.unwrap_or(session_end);
        let mut sources = Vec::new();
        for record in events {
            if record.session_id != query.session_id
                || record.cursor < query.start_cursor
                || record.cursor > end_cursor
            {
                continue;
            }
            if state
                .forgotten
                .iter()
                .any(|range| range.covers_cursor(&record.session_id, record.cursor))
            {
                continue;
            }
            collect_query_sources(record, &mut sources);
        }
        let matched = entries
            .into_iter()
            .filter(|entry| {
                entry
                    .source_log_ids
                    .iter()
                    .any(|log_id| log_id_in_query(query, end_cursor, log_id))
            })
            .filter(|entry| !entry_has_forgotten_source(entry, &state.forgotten))
            .map(|entry| MemoryQueryEntry::from(&entry))
            .collect();
        Ok(MemoryQueryView {
            session_id: query.session_id.clone(),
            start_cursor: query.start_cursor,
            end_cursor,
            events: sources,
            entries: matched,
            damaged,
        })
    }

    /// Apply verified background writes. Every cited log id must belong to a
    /// user-input turn; `user_set` and forgotten sources are never overwritten.
    pub fn apply(
        &self,
        evidence: &UserEvidence,
        candidates: &[MemoryCandidate],
    ) -> Result<ApplyReport, RuntimeError> {
        self.scan()?;
        let mut report = ApplyReport {
            written: Vec::new(),
            rejected: Vec::new(),
        };
        for candidate in candidates {
            match self.apply_one(evidence, candidate) {
                Ok(Some(entry_id)) => report.written.push(entry_id),
                Ok(None) => {}
                Err(reason) => report.rejected.push(RejectedCandidate {
                    entry_id: candidate.entry_id.clone(),
                    reason: reason.to_owned(),
                }),
            }
        }
        Ok(report)
    }

    fn apply_one(
        &self,
        evidence: &UserEvidence,
        candidate: &MemoryCandidate,
    ) -> Result<Option<String>, &'static str> {
        if candidate.source_type == SourceType::UserSet {
            return Err("background writes cannot set user_set");
        }
        if candidate.source_log_ids.is_empty() {
            return Err("a memory write needs user-input source logs");
        }
        if candidate.kind.is_empty()
            || candidate.kind.contains('/')
            || candidate.kind.chars().any(char::is_control)
        {
            return Err("memory kind is invalid");
        }
        let state = crate::lock(&self.state);
        for log_id in &candidate.source_log_ids {
            if !evidence.contains(log_id) {
                return Err("memory source is not user input");
            }
            if state
                .forgotten
                .iter()
                .any(|range| range_covers_log_id(range, log_id))
            {
                return Err("memory source was forgotten");
            }
        }
        drop(state);
        let existing = candidate
            .entry_id
            .as_ref()
            .and_then(|entry_id| self.entry(entry_id));
        if existing
            .as_ref()
            .is_some_and(|entry| entry.source_type == SourceType::UserSet)
        {
            return Err("user_set entries are not overwritten by background writes");
        }
        let entry_id = candidate
            .entry_id
            .clone()
            .or_else(|| existing.as_ref().map(|entry| entry.entry_id.clone()))
            .unwrap_or_else(|| format!("mem-{}", uuid::Uuid::new_v4().simple()));
        if !safe_entry_id(&entry_id) {
            return Err("memory entry id is invalid");
        }
        let operation = candidate.operation;
        if operation == MemoryOperation::Disable && existing.is_none() {
            return Err("cannot disable a missing memory entry");
        }
        let mut entry = existing.unwrap_or(MemoryEntry {
            entry_id: entry_id.clone(),
            kind: candidate.kind.clone(),
            source_type: candidate.source_type,
            source_log_ids: Vec::new(),
            status: MemoryStatus::Active,
            updated_at: now_ms(),
            body: String::new(),
            version: String::new(),
        });
        match operation {
            MemoryOperation::Upsert => {
                if candidate.body.trim().is_empty() {
                    return Err("a memory entry needs a body");
                }
                entry.kind = candidate.kind.clone();
                entry.source_type = candidate.source_type;
                entry.source_log_ids = candidate.source_log_ids.clone();
                entry.status = MemoryStatus::Active;
                entry.body = candidate.body.clone();
                entry.updated_at = now_ms();
            }
            MemoryOperation::Disable => {
                entry.status = MemoryStatus::Inactive;
                entry.updated_at = now_ms();
            }
        }
        self.write_entry(&entry, operation)
            .map_err(|_| "memory entry could not be written")?;
        Ok(Some(entry_id))
    }

    fn entry(&self, entry_id: &str) -> Option<MemoryEntry> {
        let state = crate::lock(&self.state);
        state.entries.get(entry_id).cloned()
    }

    fn write_entry(
        &self,
        entry: &MemoryEntry,
        operation: MemoryOperation,
    ) -> Result<(), RuntimeError> {
        fs::create_dir_all(&self.directory).map_err(|error| {
            RuntimeError::Workspace(format!("create memory directory: {error}"))
        })?;
        let bytes = serialize_entry(entry);
        let path = self
            .directory
            .join(format!("{}{ENTRY_SUFFIX}", entry.entry_id));
        write_private(&path, &bytes)?;
        let version = sha256_hex(&bytes);
        let kind = match operation {
            MemoryOperation::Upsert => "memory_updated",
            MemoryOperation::Disable => "memory_disabled",
        };
        append_event(
            &self.directory,
            serde_json::json!({
                "type": kind,
                "entry_id": entry.entry_id,
                "version": version,
                "at": entry.updated_at,
            }),
        )?;
        let mut state = crate::lock(&self.state);
        state
            .known_versions
            .insert(entry.entry_id.clone(), version.clone());
        let mut stored = entry.clone();
        stored.version = version;
        state.entries.insert(entry.entry_id.clone(), stored);
        Ok(())
    }

    /// Append a forget record for one source range. The original events and raw
    /// logs are not changed or deleted.
    pub fn forget(&self, range: SourceRange, reason: &str, at: u64) -> Result<(), RuntimeError> {
        range
            .validate()
            .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?;
        self.scan()?;
        append_event(
            &self.directory,
            serde_json::json!({
                "type":"forget",
                "range": range,
                "reason": reason,
                "at": at,
            }),
        )?;
        let mut state = crate::lock(&self.state);
        state.forgotten.push(range);
        Ok(())
    }

    /// A stable block of durable memory and preferences for provider input.
    /// Forgotten provenance is skipped without rewriting the entry file.
    pub fn context_block(&self) -> Result<Option<String>, RuntimeError> {
        let entries = self.visible_entries()?;
        if entries.is_empty() {
            return Ok(None);
        }
        let mut base = Vec::new();
        let mut preference = Vec::new();
        let mut other = Vec::new();
        for entry in entries {
            let line = format!("- {}", entry.body.trim());
            match entry.kind.as_str() {
                "base" => base.push(line),
                "preference" => preference.push(line),
                _ => other.push(line),
            }
        }
        let mut block = String::new();
        for (title, lines) in [
            ("[基础记忆]", base),
            ("[用户偏好]", preference),
            ("[记忆条目]", other),
        ] {
            if lines.is_empty() {
                continue;
            }
            block.push_str(title);
            block.push('\n');
            for line in lines {
                block.push_str(&line);
                block.push('\n');
            }
        }
        Ok((!block.is_empty()).then_some(block))
    }

    /// Organize references of existing summaries. No model call is made here;
    /// without explicit model text the Dream body stays empty.
    pub fn dream(
        &self,
        summaries: &[SummaryRecord],
        model_text: Option<&str>,
    ) -> Result<DreamPlan, RuntimeError> {
        let forgotten = self.forgotten_ranges()?;
        Ok(organize_dream(summaries, &forgotten, model_text))
    }
}

/// Provider input for the main session. It keeps durable memory, preferences,
/// the current request and active work, evicts older raw turns at the
/// replacement threshold, and never starts cumulative compression.
#[cfg(test)]
pub(crate) fn prepare_main_session_input(
    memory: &MainMemory,
    messages: &[Message],
    window: Option<u64>,
    output_reserve: u64,
    forgotten_messages: &[Message],
) -> Result<Vec<Message>, RuntimeError> {
    let spans = crate::session_context::units_from_ranges(messages, &[]);
    let mut forgotten = Vec::new();
    for span in &spans {
        if messages[span.msg_from..span.msg_to]
            .iter()
            .any(|message| forgotten_messages.contains(message))
        {
            forgotten.push(
                CursorRange::new(span.unit.start_cursor, span.unit.end_cursor)
                    .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?,
            );
        }
    }
    prepare_main_session_input_with_ranges(
        memory,
        messages,
        &[],
        window,
        output_reserve,
        &forgotten,
    )
}

pub(crate) fn prepare_main_session_input_with_ranges(
    memory: &MainMemory,
    messages: &[Message],
    ranges: &[CursorRange],
    window: Option<u64>,
    output_reserve: u64,
    forgotten: &[CursorRange],
) -> Result<Vec<Message>, RuntimeError> {
    if messages.is_empty() {
        return Ok(Vec::new());
    }
    let spans = crate::session_context::units_from_ranges(messages, ranges);
    let budget = match input_budget(window, output_reserve, DEFAULT_PROTOCOL_MARGIN) {
        Ok(budget) => budget,
        // The conservative window is only a fallback estimate. When it cannot
        // hold the requested output reserve, keep the non-forgotten input
        // rather than refusing a request the provider may accept.
        Err(_) if window.is_none() => {
            return Ok(main_session_fallback_input_with_spans(
                messages, &spans, forgotten,
            ));
        }
        Err(error) => return Err(RuntimeError::InvalidInput(error.to_string())),
    };
    let block = memory.context_block()?;
    let pinned_tokens = block
        .as_ref()
        .map(|text| u64::try_from(text.len()).unwrap_or(u64::MAX))
        .unwrap_or(0);
    let units = spans
        .iter()
        .map(|span| span.unit.clone())
        .collect::<Vec<_>>();
    let plan = plan_main_session(&cayw_context::MainSessionInput {
        units: &units,
        forgotten,
        pinned_tokens,
        budget,
    })
    .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?;
    let retained = plan
        .retained_ranges
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    let mut output = Vec::new();
    if let Some(block) = block {
        output.push(Message::system(block));
    }
    for span in &spans {
        let range = CursorRange {
            start_cursor: span.unit.start_cursor,
            end_cursor: span.unit.end_cursor,
        };
        if !retained.contains(&range) {
            continue;
        }
        output.extend(messages[span.msg_from..span.msg_to].iter().cloned());
    }
    let tokens = output
        .iter()
        .map(|message| u64::try_from(message.text().len()).unwrap_or(u64::MAX))
        .fold(0_u64, u64::saturating_add);
    if tokens > budget.input_limit {
        return if budget.window_was_default {
            Ok(main_session_fallback_input_with_spans(
                messages, &spans, forgotten,
            ))
        } else {
            Err(RuntimeError::Budget("context input"))
        };
    }
    Ok(output)
}

// Preserve the normal planner's forget rules even when the estimated window
// cannot fit the input. Keep whole units and leave active/pinned work unchanged.
#[cfg(test)]
fn main_session_fallback_input(messages: &[Message], forgotten: &[Message]) -> Vec<Message> {
    let spans = crate::session_context::units_from_ranges(messages, &[]);
    let forgotten_ranges = spans
        .iter()
        .filter(|span| {
            messages[span.msg_from..span.msg_to]
                .iter()
                .any(|message| forgotten.contains(message))
        })
        .filter_map(|span| CursorRange::new(span.unit.start_cursor, span.unit.end_cursor).ok())
        .collect::<Vec<_>>();
    main_session_fallback_input_with_spans(messages, &spans, &forgotten_ranges)
}

fn main_session_fallback_input_with_spans(
    messages: &[Message],
    spans: &[crate::session_context::UnitSpan],
    forgotten: &[CursorRange],
) -> Vec<Message> {
    let mut output = Vec::new();
    for span in spans {
        let source = &messages[span.msg_from..span.msg_to];
        let replaceable = matches!(
            span.unit.kind,
            cayw_context::UnitKind::CompletedTurn | cayw_context::UnitKind::ToolExchange
        ) && !span.unit.pin_original
            && !span.unit.outcome_unknown;
        let overlaps_forgotten = forgotten.iter().any(|range| {
            span.unit.start_cursor <= range.end_cursor && range.start_cursor <= span.unit.end_cursor
        });
        if replaceable && overlaps_forgotten {
            continue;
        }
        output.extend_from_slice(source);
    }
    output
}

fn reload_events(directory: &Path, state: &mut MemoryState) -> Result<(), RuntimeError> {
    state.known_versions.clear();
    state.forgotten.clear();
    state.damaged = 0;
    let path = directory.join(EVENTS_FILE);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            state.damaged = state.damaged.saturating_add(1);
            return Err(RuntimeError::Workspace(format!(
                "read memory events: {error}"
            )));
        }
    };
    for line in bytes.split(|byte| *byte == b'\n') {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let Ok(event) = serde_json::from_slice::<StoredEvent>(line) else {
            state.damaged = state.damaged.saturating_add(1);
            continue;
        };
        match event {
            StoredEvent::MemoryCreated {
                entry_id, version, ..
            }
            | StoredEvent::MemoryUpdated {
                entry_id, version, ..
            }
            | StoredEvent::UserEdited {
                entry_id, version, ..
            }
            | StoredEvent::MemoryDisabled {
                entry_id, version, ..
            } => {
                state.known_versions.insert(entry_id, version);
            }
            StoredEvent::EntryRemoved { entry_id, .. } => {
                state.known_versions.remove(&entry_id);
            }
            StoredEvent::Forget { range, .. } => state.forgotten.push(range),
        }
    }
    Ok(())
}

fn entry_files(directory: &Path) -> Result<Vec<PathBuf>, RuntimeError> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(RuntimeError::Workspace(format!(
                "read memory directory: {error}"
            )));
        }
    };
    let mut files = Vec::new();
    for entry in entries {
        let entry =
            entry.map_err(|error| RuntimeError::Workspace(format!("memory entry: {error}")))?;
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) == Some("md")
            && !path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with('.'))
        {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

fn parse_entry(bytes: &[u8]) -> Option<MemoryEntry> {
    let text = std::str::from_utf8(bytes).ok()?;
    let (header, body) = text.split_once("\n\n")?;
    let mut entry_id = None;
    let mut kind = None;
    let mut source_type = None;
    let mut source_log_ids = None;
    let mut status = None;
    let mut updated_at = None;
    for line in header.lines() {
        let (key, value) = line.split_once(':')?;
        let value = value.trim();
        match key.trim() {
            "entry_id" => entry_id = Some(value.to_owned()),
            "kind" => kind = Some(value.to_owned()),
            "source_type" => {
                source_type = Some(match value {
                    "stated" => SourceType::Stated,
                    "inferred" => SourceType::Inferred,
                    "user_set" => SourceType::UserSet,
                    _ => return None,
                })
            }
            "source_log_ids" => {
                source_log_ids = Some(
                    value
                        .split(',')
                        .map(str::trim)
                        .filter(|item| !item.is_empty())
                        .map(str::to_owned)
                        .collect::<Vec<_>>(),
                )
            }
            "status" => {
                status = Some(match value {
                    "active" => MemoryStatus::Active,
                    "inactive" => MemoryStatus::Inactive,
                    _ => return None,
                })
            }
            "updated_at" => updated_at = Some(value.parse::<u64>().ok()?),
            _ => return None,
        }
    }
    Some(MemoryEntry {
        entry_id: entry_id?,
        kind: kind?,
        source_type: source_type?,
        source_log_ids: source_log_ids?,
        status: status?,
        updated_at: updated_at?,
        body: body.to_owned(),
        version: String::new(),
    })
}

fn serialize_entry(entry: &MemoryEntry) -> Vec<u8> {
    format!(
        "entry_id: {}\nkind: {}\nsource_type: {}\nsource_log_ids: {}\nstatus: {}\nupdated_at: {}\n\n{}",
        entry.entry_id,
        entry.kind,
        match entry.source_type {
            SourceType::Stated => "stated",
            SourceType::Inferred => "inferred",
            SourceType::UserSet => "user_set",
        },
        entry.source_log_ids.join(","),
        match entry.status {
            MemoryStatus::Active => "active",
            MemoryStatus::Inactive => "inactive",
        },
        entry.updated_at,
        entry.body
    )
    .into_bytes()
}

fn append_event(directory: &Path, event: serde_json::Value) -> Result<(), RuntimeError> {
    fs::create_dir_all(directory)
        .map_err(|error| RuntimeError::Workspace(format!("create memory directory: {error}")))?;
    let path = directory.join(EVENTS_FILE);
    let mut line = serde_json::to_vec(&event).map_err(RuntimeError::Json)?;
    line.push(b'\n');
    let mut options = fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&path)
        .map_err(|error| RuntimeError::Workspace(format!("open memory events: {error}")))?;
    file.write_all(&line)
        .and_then(|_| file.sync_all())
        .map_err(|error| RuntimeError::Workspace(format!("append memory event: {error}")))
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<(), RuntimeError> {
    let parent = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(parent)
        .map_err(|error| RuntimeError::Workspace(format!("create memory directory: {error}")))?;
    let temporary = parent.join(format!(
        ".{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary)
        .map_err(|error| RuntimeError::Workspace(format!("write memory entry: {error}")))?;
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|error| RuntimeError::Workspace(format!("write memory entry: {error}")))?;
    fs::rename(&temporary, path)
        .map_err(|error| RuntimeError::Workspace(format!("rename memory entry: {error}")))
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn safe_entry_id(entry_id: &str) -> bool {
    !entry_id.is_empty()
        && entry_id.len() <= 128
        && entry_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn entry_has_forgotten_source(entry: &MemoryEntry, forgotten: &[SourceRange]) -> bool {
    entry.source_log_ids.iter().any(|log_id| {
        forgotten
            .iter()
            .any(|range| range_covers_log_id(range, log_id))
    })
}

fn range_covers_log_id(range: &SourceRange, log_id: &str) -> bool {
    let Some((session, cursor)) = log_id.rsplit_once(':') else {
        return false;
    };
    let Ok(cursor) = cursor.parse::<u64>() else {
        return false;
    };
    range.covers_cursor(session, cursor)
}

/// Read entry files without writing anything. A file that differs from the
/// recorded version (or has no recorded version yet) is the user's current
/// content, so it is reported as `user_set`; the read never rewrites it.
fn read_entries(
    directory: &Path,
    known_versions: &BTreeMap<String, String>,
    damaged: &mut usize,
) -> Result<Vec<MemoryEntry>, RuntimeError> {
    let mut entries = Vec::new();
    for path in entry_files(directory)? {
        let Some(entry_id) = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .map(str::to_owned)
        else {
            *damaged = damaged.saturating_add(1);
            continue;
        };
        if !safe_entry_id(&entry_id) {
            *damaged = damaged.saturating_add(1);
            continue;
        }
        let bytes = fs::read(&path)
            .map_err(|error| RuntimeError::Workspace(format!("read memory entry: {error}")))?;
        let Some(mut entry) = parse_entry(&bytes) else {
            *damaged = damaged.saturating_add(1);
            continue;
        };
        if entry.entry_id != entry_id {
            *damaged = damaged.saturating_add(1);
            continue;
        }
        let digest = sha256_hex(&bytes);
        if known_versions.get(&entry_id) != Some(&digest) {
            entry.source_type = SourceType::UserSet;
        }
        entry.version = digest;
        entries.push(entry);
    }
    entries.sort_by(|left, right| left.entry_id.cmp(&right.entry_id));
    Ok(entries)
}

/// Collect message and tool items from one durable event. Message start/end
/// pairs produce the same item and are deduplicated; host-only events add
/// nothing.
fn collect_query_sources(record: &EventRecord, out: &mut Vec<MemoryEvent>) {
    let CoreEvent::Agent { event } = &record.event else {
        return;
    };
    let log_id = format!("{}:{}", record.session_id, record.cursor);
    let mut push = |origin: MemoryOrigin, text: String| {
        if text.is_empty() {
            return;
        }
        let item = MemoryEvent {
            log_id: log_id.clone(),
            session_id: record.session_id.clone(),
            cursor: record.cursor,
            origin,
            text,
        };
        if !out.contains(&item) {
            out.push(item);
        }
    };
    match event.as_ref() {
        AgentEvent::MessageStart { message } | AgentEvent::MessageEnd { message } => {
            if let Some((origin, text)) = message_query_source(message) {
                push(origin, text);
            }
        }
        AgentEvent::TurnEnd {
            message,
            tool_results,
        } => {
            push(
                MemoryOrigin::Assistant,
                Message::Assistant(message.clone()).text(),
            );
            for result in tool_results {
                push(
                    MemoryOrigin::Tool,
                    Message::ToolResult(result.clone()).text(),
                );
            }
        }
        _ => {}
    }
}

fn message_query_source(message: &Message) -> Option<(MemoryOrigin, String)> {
    match message {
        Message::User { .. } => Some((MemoryOrigin::User, message.text())),
        Message::Assistant(_) => Some((MemoryOrigin::Assistant, message.text())),
        Message::ToolResult(_) => Some((MemoryOrigin::Tool, message.text())),
        Message::System(_) | Message::Custom { .. } => None,
    }
}

fn log_id_in_query(query: &MemoryQuery, end_cursor: u64, log_id: &str) -> bool {
    let Some((session, cursor)) = log_id.rsplit_once(':') else {
        return false;
    };
    if session != query.session_id {
        return false;
    }
    let Ok(cursor) = cursor.parse::<u64>() else {
        return false;
    };
    cursor >= query.start_cursor && cursor <= end_cursor
}

fn mutating_capability(name: &str) -> bool {
    matches!(
        name,
        "fs.write"
            | "fs.patch"
            | "fs.mkdir"
            | "fs.move"
            | "fs.remove"
            | "fs.prepare_change_batch"
            | "fs.submit_change_batch"
            | "fs.recover_change_batch"
    )
}

fn candidate_paths(arguments: &serde_json::Value) -> Vec<String> {
    let mut paths = Vec::new();
    for key in ["path", "source", "destination", "from", "to"] {
        if let Some(path) = arguments.get(key).and_then(serde_json::Value::as_str) {
            paths.push(path.to_owned());
        }
    }
    for key in ["changes", "operations"] {
        if let Some(list) = arguments.get(key).and_then(serde_json::Value::as_array) {
            for change in list {
                for field in ["path", "source", "destination"] {
                    if let Some(path) = change.get(field).and_then(serde_json::Value::as_str) {
                        paths.push(path.to_owned());
                    }
                }
            }
        }
    }
    paths
}

#[cfg(test)]
mod tests {
    use super::*;
    use cayw_ai::{AssistantMessage, Content, Model};

    #[test]
    fn provider_input_skips_forgotten_turn_and_injects_durable_memory() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let memory = MainMemory::open(directory.path());
        let user = record("s", 1, Message::user("remember my preference"));
        let evidence = user_evidence(std::slice::from_ref(&user));
        memory
            .apply(
                &evidence,
                &[MemoryCandidate {
                    operation: MemoryOperation::Upsert,
                    entry_id: None,
                    kind: "preference".into(),
                    body: "User prefers short answers".into(),
                    source_type: SourceType::Stated,
                    source_log_ids: vec!["s:1".into()],
                }],
            )
            .expect("apply");

        let old = Message::user("old secret");
        let mut answer = AssistantMessage::new(&Model::new("fixture", "model"));
        answer.content.push(Content::text("old answer"));
        let messages = vec![
            old.clone(),
            Message::Assistant(answer),
            Message::user("current request"),
        ];
        let output = prepare_main_session_input(
            &memory,
            &messages,
            Some(100_000),
            50,
            std::slice::from_ref(&old),
        )
        .expect("provider input");
        assert!(output.iter().any(|m| m.text().contains("用户偏好")));
        assert!(output.iter().any(|m| m.text().contains("short answers")));
        assert!(!output.iter().any(|m| m.text().contains("old secret")));
        assert!(!output.iter().any(|m| m.text().contains("old answer")));
        assert_eq!(output.last().unwrap().text(), "current request");
    }

    #[test]
    fn default_window_fallbacks_keep_forget_filtering() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let memory = MainMemory::open(directory.path());
        let old = Message::user("old secret");
        let mut answer = AssistantMessage::new(&Model::new("fixture", "model"));
        answer.content.push(Content::text("old answer"));
        let kept = Message::user("unforgotten history");
        for (output_reserve, current) in [
            (
                cayw_context::CONSERVATIVE_CONTEXT_WINDOW,
                Message::user("current request"),
            ),
            (1, Message::user("x".repeat(8_192))),
        ] {
            let messages = vec![
                old.clone(),
                Message::Assistant(answer.clone()),
                kept.clone(),
                current.clone(),
            ];
            let output = prepare_main_session_input(
                &memory,
                &messages,
                None,
                output_reserve,
                std::slice::from_ref(&old),
            )
            .expect("default estimate must not refuse the request");
            assert_eq!(output, vec![kept.clone(), current]);

            let unchanged =
                prepare_main_session_input(&memory, &messages, None, output_reserve, &[])
                    .expect("default estimate must not refuse unfiltered input");
            assert_eq!(unchanged, messages);
        }
    }

    #[test]
    fn fallback_forget_preserves_tool_pairing_and_unknown_status() {
        let mut assistant = AssistantMessage::new(&Model::new("fixture", "model"));
        assistant
            .content
            .push(Content::ToolCall(cayw_ai::ToolCall::new(
                "call-1",
                "read",
                serde_json::json!({}),
            )));
        for outcome_unknown in [false, true] {
            let result = Message::ToolResult(cayw_ai::ToolResultMessage {
                tool_call_id: "call-1".into(),
                tool_name: "read".into(),
                content: vec![Content::text("forgotten tool output")],
                details: None,
                structured_content: None,
                usage: None,
                is_error: false,
                outcome_unknown,
                timestamp: 1,
                duration_ms: Some(1),
            });
            let current = Message::user("current request");
            let messages = vec![
                Message::Assistant(assistant.clone()),
                result.clone(),
                current.clone(),
            ];
            let output = main_session_fallback_input(&messages, &[result]);
            if outcome_unknown {
                assert_eq!(output, messages);
            } else {
                assert_eq!(output, vec![current]);
            }
        }
    }

    fn record(session: &str, cursor: u64, message: Message) -> EventRecord {
        EventRecord {
            protocol_version: cayw_core::PROTOCOL_VERSION,
            session_id: session.into(),
            cursor,
            timestamp: cursor,
            attempt: None,
            event: CoreEvent::Agent {
                event: Box::new(AgentEvent::MessageStart { message }),
            },
        }
    }
}
