//! Unified tool-result handles (F01/F03): store large bodies in the same
//! Device ContentStore as #18, retrieve by handle with pagination, search for
//! snippets, and soft-release without a separate executor.
//!
//! Oversized Host/tool results on the requests→Cells→Host path are automatically
//! externalized when ContentStore is configured and the JSON body exceeds
//! `RuntimeConfig::auto_externalize_threshold_bytes` (default 32 KiB). The model
//! receives a handle stub and reads pages via `result.get` / `result.search`.
//!
//! ContentStore has no delete API; release flips durable metadata so later
//! get/search/batch refuse the handle while the body object may remain for
//! quota accounting (aligned with cayw-content).
use crate::{Runtime, RuntimeError, cells::CallScope, content};
use async_trait::async_trait;
use cayw_ai::{CancellationToken, ToolDefinition};
use cayw_codemode::CapabilityError;
use cayw_content::{ContentHash, ContentStore, ExpectedRevision, Id, Record};
use cayw_core::{AttemptLease, CapabilityCallKey, CapabilityOutcome, CoreError, CoreEvent};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

const STORE: &str = "result.store";
const GET: &str = "result.get";
const BATCH_GET: &str = "result.batch_get";
const SEARCH: &str = "result.search";
const RELEASE: &str = "result.release";

/// Soft cap for one page returned to the model; full body stays in ContentStore.
const MAX_PAGE_BYTES: usize = 32 * 1024;
const MAX_SEARCH_MATCHES: usize = 32;
const MAX_BATCH: usize = 16;
const MAX_SNIPPET_BYTES: usize = 512;

/// Default Host/tool result size that triggers automatic ContentStore
/// externalization (UTF-8 JSON bytes). Configurable via RuntimeConfig;
/// `0` disables. Matches one result.get page so inline stubs stay compact.
pub const DEFAULT_AUTO_EXTERNALIZE_THRESHOLD_BYTES: usize = 32 * 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultScope {
    #[default]
    Session,
    Task,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Encoding {
    Utf8,
    Json,
    Base64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Meta {
    v: u32,
    handle: String,
    content_hash: String,
    bytes: u64,
    encoding: Encoding,
    released: bool,
    scope: ResultScope,
    session_id: String,
    task_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResultHandleView {
    pub handle: String,
    pub scope: ResultScope,
    pub session_id: String,
    pub task_id: Option<String>,
    pub content_hash: String,
    pub bytes: u64,
    pub encoding: String,
    pub released: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResultPage {
    pub handle: String,
    pub content_hash: String,
    pub total_bytes: u64,
    pub offset: u64,
    pub next_offset: u64,
    pub bytes_returned: u64,
    pub has_more: bool,
    pub text: String,
    pub released: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResultSnippet {
    pub offset: u64,
    pub line: u64,
    pub text: String,
}

pub(crate) fn definitions(config: Option<&content::ContentConfig>) -> Vec<ToolDefinition> {
    let Some(config) = config else {
        return vec![];
    };
    let handle = json!({"type":"string","minLength":1,"maxLength":64,"pattern":"^[A-Za-z0-9_-]+$"});
    let scope = json!({"enum":["session","task"]});
    let max_text = config.limits.max_content_bytes.min(1024 * 1024);
    vec![
        ToolDefinition {
            name: STORE.into(),
            description: "Externalize one UTF-8 or JSON tool result into the Device ContentStore and return a durable handle. Use when a full body should stay out of the model context. Owners and directory are fixed by the host.".into(),
            parameters: json!({"type":"object","additionalProperties":false,"required":["text"],"properties":{
                "text":{"type":"string","maxLength":max_text},
                "encoding":{"enum":["utf8","json","base64"]},
                "scope":scope,
                "handle":handle
            }}),
        },
        ToolDefinition {
            name: GET.into(),
            description: "Read one page of a previously stored result by handle. offset/limit are UTF-8 byte ranges clamped to character boundaries. Released handles are refused.".into(),
            parameters: json!({"type":"object","additionalProperties":false,"required":["handle"],"properties":{
                "handle":handle,"scope":scope,
                "offset":{"type":"integer","minimum":0},
                "limit":{"type":"integer","minimum":1,"maximum":MAX_PAGE_BYTES}
            }}),
        },
        ToolDefinition {
            name: BATCH_GET.into(),
            description: "Fetch metadata (and optional first page) for up to 16 result handles in one call. Missing or released handles appear as errors in the same order.".into(),
            parameters: json!({"type":"object","additionalProperties":false,"required":["handles"],"properties":{
                "handles":{"type":"array","minItems":1,"maxItems":MAX_BATCH,"items":handle},
                "scope":scope,
                "include_page":{"type":"boolean"},
                "limit":{"type":"integer","minimum":1,"maximum":MAX_PAGE_BYTES}
            }}),
        },
        ToolDefinition {
            name: SEARCH.into(),
            description: "Search the stored UTF-8 body for a literal query and return matching line snippets with byte offsets. Released handles are refused.".into(),
            parameters: json!({"type":"object","additionalProperties":false,"required":["handle","query"],"properties":{
                "handle":handle,"scope":scope,
                "query":{"type":"string","minLength":1,"maxLength":256},
                "limit":{"type":"integer","minimum":1,"maximum":MAX_SEARCH_MATCHES}
            }}),
        },
        ToolDefinition {
            name: RELEASE.into(),
            description: "Soft-release a result handle. Later get/search/batch refuse it. ContentStore has no delete; the body object may remain for quota accounting (#18).".into(),
            parameters: json!({"type":"object","additionalProperties":false,"required":["handle"],"properties":{
                "handle":handle,"scope":scope
            }}),
        },
    ]
}

pub(crate) fn is_tool(name: &str) -> bool {
    matches!(name, STORE | GET | BATCH_GET | SEARCH | RELEASE)
}

fn should_externalize(threshold: usize, text_len: usize, max_content: usize) -> bool {
    threshold > 0 && text_len > threshold && text_len <= max_content
}

#[cfg(test)]
fn externalize_value(
    store: &ContentStore,
    session: &str,
    task_id: &str,
    threshold: usize,
    value: Value,
) -> Result<Value, CapabilityError> {
    let text = match serde_json::to_string(&value) {
        Ok(text) => text,
        Err(_) => return Ok(value),
    };
    let max = store.limits().max_content_bytes.min(1024 * 1024) as usize;
    if !should_externalize(threshold, text.len(), max) {
        return Ok(value);
    }
    let mut stored = store_result(
        store,
        session,
        task_id,
        json!({
            "text": text,
            "encoding": "json",
            "scope": "session",
        }),
    )?;
    if let Some(obj) = stored.as_object_mut() {
        obj.insert("externalized".into(), Value::Bool(true));
    }
    Ok(stored)
}

const STORAGE_PREPARED: &str = "runtime_result_storage_prepared";
const STORAGE_TERMINAL: &str = "runtime_result_storage_terminal";
const STORAGE_RECONCILED: &str = "runtime_result_storage_reconciled";

/// Immutable identity recorded before either content write. This is a separate
/// operation from the original, already successful Host capability.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResultStorageIdentity {
    pub lease: AttemptLease,
    pub key: CapabilityCallKey,
    pub task_id: String,
    pub device_id: String,
    pub handle: String,
    pub content_hash: String,
    pub bytes: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultStorageState {
    Prepared,
    Stored,
    Failed,
    OutcomeUnknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResultStoragePending {
    pub identity: ResultStorageIdentity,
    pub state: ResultStorageState,
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultStorageObservation {
    Absent,
    BodyOnly,
    Complete,
}

/// Actual validated content, available only to the trusted embedding host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResultStorageContent {
    pub record: Record,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResultStorageInspection {
    pub pending: ResultStoragePending,
    pub observation: ResultStorageObservation,
    pub body: Option<ResultStorageContent>,
    pub metadata: Option<ResultStorageContent>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StorageDecision {
    identity: ResultStorageIdentity,
    observation: ResultStorageObservation,
    body: Option<Record>,
    metadata: Option<Record>,
    evidence: String,
}

struct StorageEntry {
    pending: ResultStoragePending,
    reconciled: bool,
}
impl StorageEntry {
    fn unresolved(&self) -> bool {
        !self.reconciled
            && matches!(
                self.pending.state,
                ResultStorageState::Prepared | ResultStorageState::OutcomeUnknown
            )
    }
}

impl ResultStorageIdentity {
    fn meta(&self) -> Meta {
        Meta {
            v: 1,
            handle: self.handle.clone(),
            content_hash: self.content_hash.clone(),
            bytes: self.bytes,
            encoding: Encoding::Json,
            released: false,
            scope: ResultScope::Session,
            session_id: self.lease.session_id.clone(),
            task_id: None,
        }
    }
    fn keys(&self) -> Result<(Id, Id), RuntimeError> {
        let handle = Id::new(&self.handle)?;
        Ok((
            body_key(&self.device_id, &self.lease.session_id, None, &handle)?,
            meta_key(&self.device_id, &self.lease.session_id, None, &handle)?,
        ))
    }
}

/// All writes are synchronous after the prepared commit. Cancellation is signaled
/// by Completion only after Cells has saved the original Host success.
pub(crate) async fn maybe_externalize(
    scope: &Arc<CallScope>,
    value: Value,
) -> Result<Value, CapabilityError> {
    let threshold = scope.context.inner.config.auto_externalize_threshold_bytes;
    if threshold == 0 || is_tool(&scope.name) || content::is_tool(&scope.name) {
        return Ok(value);
    }
    let Some(store) = scope.context.inner.content.as_ref() else {
        return Ok(value);
    };
    let text = serde_json::to_vec(&value).map_err(known)?;
    if !should_externalize(
        threshold,
        text.len(),
        store.limits().max_content_bytes.min(1024 * 1024) as usize,
    ) {
        return Ok(value);
    }
    let identity = ResultStorageIdentity {
        lease: scope.context.lease.clone(),
        key: scope.key.clone(),
        task_id: scope.context.task.id.clone(),
        device_id: store.device_id().to_string(),
        handle: new_handle()?.to_string(),
        content_hash: ContentHash::of(&text).to_string(),
        bytes: text.len() as u64,
    };
    let record = |kind: &str, data: Value| {
        scope
            .context
            .inner
            .store
            .append_host_event(&identity.lease, kind, data)
            .map_err(|error| {
                CapabilityError::unknown(format!("result storage recording failed: {error}"))
            })?;
        scope.context.inner.changed.notify_waiters();
        Ok::<(), CapabilityError>(())
    };
    record(
        STORAGE_PREPARED,
        serde_json::to_value(&identity).map_err(known)?,
    )?;
    #[cfg(feature = "fault-injection")]
    cayw_ai::faults::hit("runtime.result_storage.prepared", json!(identity));
    let result = (|| {
        let (body, metadata) = identity.keys().map_err(known)?;
        store
            .try_put(&body, ExpectedRevision::Absent, &text)
            .map_err(content_error)?;
        let bytes = serde_json::to_vec(&identity.meta()).map_err(known)?;
        store
            .try_put(&metadata, ExpectedRevision::Absent, &bytes)
            .map_err(content_error)?;
        let mut view = serde_json::to_value(view_from_meta(&identity.meta())).map_err(known)?;
        view["stored"] = Value::Bool(true);
        view["externalized"] = Value::Bool(true);
        Ok::<_, CapabilityError>(view)
    })();
    #[cfg(feature = "fault-injection")]
    cayw_ai::faults::hit("runtime.result_storage.before_terminal", json!(identity));
    let terminal = ResultStoragePending {
        identity: identity.clone(),
        state: match &result {
            Ok(_) => ResultStorageState::Stored,
            Err(error) if error.outcome_unknown => ResultStorageState::OutcomeUnknown,
            Err(_) => ResultStorageState::Failed,
        },
        error: result
            .as_ref()
            .err()
            .map(|error| crate::cells::bounded(&error.message)),
    };
    record(
        STORAGE_TERMINAL,
        serde_json::to_value(terminal).map_err(known)?,
    )?;
    match result {
        Ok(stored) => Ok(stored),
        Err(error) if error.outcome_unknown => {
            scope
                .context
                .unknown
                .store(true, std::sync::atomic::Ordering::Release);
            Err(error)
        }
        Err(_) => Ok(value),
    }
}

fn storage_corrupt(message: &str) -> RuntimeError {
    CoreError::CorruptStore(message.into()).into()
}

fn storage_log(
    runtime: &Runtime,
    session: &str,
    end: u64,
) -> Result<std::collections::BTreeMap<String, StorageEntry>, RuntimeError> {
    let mut entries = std::collections::BTreeMap::<String, StorageEntry>::new();
    let mut calls = std::collections::BTreeMap::new();
    let mut cursor = 0;
    while cursor < end {
        let page = runtime.inner.store.events(
            session,
            cursor,
            (end - cursor).min(cayw_core::MAX_EVENT_PAGE as u64) as usize,
        )?;
        if page.is_empty() {
            return Err(storage_corrupt("result storage event prefix is incomplete"));
        }
        for event in page {
            if event.session_id != session || event.cursor != cursor + 1 {
                return Err(storage_corrupt("result storage event cursor differs"));
            }
            cursor = event.cursor;
            match event.event {
                CoreEvent::CapabilitySettled { call } => {
                    calls.insert(call.key.clone(), call);
                }
                CoreEvent::Host { kind, data } if kind == STORAGE_PREPARED => {
                    let identity: ResultStorageIdentity = serde_json::from_value(data)?;
                    let call = calls.get(&identity.key).ok_or_else(|| {
                        storage_corrupt("result storage has no original successful call")
                    })?;
                    let Some(CapabilityOutcome::Completed { result }) = &call.outcome else {
                        return Err(storage_corrupt(
                            "result storage original call is not successful",
                        ));
                    };
                    let bytes = serde_json::to_vec(result)?;
                    identity.keys()?;
                    if identity.lease.session_id != session
                        || event.attempt.as_ref() != Some(&identity.lease)
                        || call.lease != identity.lease
                        || call.task_id != identity.task_id
                        || identity.content_hash != ContentHash::of(&bytes).to_string()
                        || identity.bytes != bytes.len() as u64
                    {
                        return Err(storage_corrupt(
                            "result storage differs from its original identity or content",
                        ));
                    }
                    let handle = identity.handle.clone();
                    if entries
                        .insert(
                            handle,
                            StorageEntry {
                                pending: ResultStoragePending {
                                    identity,
                                    state: ResultStorageState::Prepared,
                                    error: None,
                                },
                                reconciled: false,
                            },
                        )
                        .is_some()
                    {
                        return Err(storage_corrupt("duplicate result storage preparation"));
                    }
                }
                CoreEvent::Host { kind, data } if kind == STORAGE_TERMINAL => {
                    let terminal: ResultStoragePending = serde_json::from_value(data)?;
                    let entry = entries.get_mut(&terminal.identity.handle).ok_or_else(|| {
                        storage_corrupt("result storage terminal has no preparation")
                    })?;
                    if entry.pending.identity != terminal.identity
                        || entry.pending.state != ResultStorageState::Prepared
                        || entry.reconciled
                        || event.attempt.as_ref() != Some(&terminal.identity.lease)
                        || terminal.state == ResultStorageState::Prepared
                        || (terminal.state == ResultStorageState::Stored)
                            != terminal.error.is_none()
                    {
                        return Err(storage_corrupt(
                            "result storage terminal changed its preparation",
                        ));
                    }
                    entry.pending = terminal;
                }
                CoreEvent::Host { kind, data } if kind == STORAGE_RECONCILED => {
                    let actor = data["actor"].as_str().unwrap_or_default();
                    let decision: StorageDecision =
                        serde_json::from_value(data["decision"].clone())?;
                    let entry = entries.get_mut(&decision.identity.handle).ok_or_else(|| {
                        storage_corrupt("result storage decision has no preparation")
                    })?;
                    if actor.is_empty()
                        || decision.evidence.trim().is_empty()
                        || entry.pending.identity != decision.identity
                        || !entry.unresolved()
                    {
                        return Err(storage_corrupt(
                            "result storage decision changed its original pending identity",
                        ));
                    }
                    validate_storage_records(
                        &decision.identity,
                        decision.observation,
                        decision.body.as_ref(),
                        decision.metadata.as_ref(),
                    )?;
                    entry.reconciled = true;
                }
                _ => {}
            }
        }
    }
    Ok(entries)
}

pub(crate) fn has_pending_storage(
    runtime: &Runtime,
    session: &str,
    cursor: u64,
) -> Result<bool, RuntimeError> {
    Ok(storage_log(runtime, session, cursor)?
        .values()
        .any(StorageEntry::unresolved))
}

pub(crate) fn owns_storage_uncertainty(
    context: &crate::execution::RunContext,
    snapshot: &cayw_core::SessionSnapshot,
    parent: &str,
    waited: Option<&str>,
) -> Result<bool, RuntimeError> {
    let runtime = Runtime {
        inner: context.inner.clone(),
    };
    Ok(
        storage_log(&runtime, &context.lease.session_id, snapshot.cursor)?
            .values()
            .any(|entry| {
                let identity = &entry.pending.identity;
                entry.unresolved()
                    && identity.lease == context.lease
                    && identity.task_id == context.task.id
                    && snapshot.executions.iter().any(|execution| {
                        execution.execution_id == identity.key.execution_id
                            && execution.lease == identity.lease
                            && execution.task_id == identity.task_id
                            && (execution.parent_call_id == parent
                                || waited == Some(execution.execution_id.as_str()))
                    })
            }),
    )
}

fn validate_storage_records(
    identity: &ResultStorageIdentity,
    observation: ResultStorageObservation,
    body: Option<&Record>,
    metadata: Option<&Record>,
) -> Result<(), RuntimeError> {
    let (body_key, metadata_key) = identity.keys()?;
    let metadata_bytes = serde_json::to_vec(&identity.meta())?;
    for (record, key, hash, bytes) in [
        (
            body,
            body_key,
            identity.content_hash.clone(),
            identity.bytes,
        ),
        (
            metadata,
            metadata_key,
            ContentHash::of(&metadata_bytes).to_string(),
            metadata_bytes.len() as u64,
        ),
    ] {
        if let Some(record) = record
            && (record.name != key
                || record.device_id.as_str() != identity.device_id
                || record.version != 1
                || record.parent_revision.is_some()
                || record.restored_from.is_some()
                || record.content_hash.to_string() != hash
                || record.content_len != bytes)
        {
            return Err(RuntimeError::InvalidInput(
                "result storage record differs from the prepared key, owner or content".into(),
            ));
        }
    }
    let actual = match (body.is_some(), metadata.is_some()) {
        (false, false) => ResultStorageObservation::Absent,
        (true, false) => ResultStorageObservation::BodyOnly,
        (true, true) => ResultStorageObservation::Complete,
        (false, true) => {
            return Err(RuntimeError::InvalidInput(
                "result metadata exists without its prepared body".into(),
            ));
        }
    };
    if actual != observation {
        return Err(RuntimeError::InvalidInput(
            "result storage observation differs from its records".into(),
        ));
    }
    Ok(())
}

impl Runtime {
    /// Read-only pending facts. Prepared without a terminal remains pending after
    /// a crash; known failures and prior explicit host decisions do not block.
    pub fn pending_result_storage(
        &self,
        session: &str,
    ) -> Result<Vec<ResultStoragePending>, RuntimeError> {
        let snapshot = self.inner.store.snapshot(session)?;
        Ok(storage_log(self, session, snapshot.cursor)?
            .into_values()
            .filter(StorageEntry::unresolved)
            .map(|entry| entry.pending)
            .collect())
    }

    fn inspect_result_storage_owned(
        &self,
        expected: &ResultStorageIdentity,
        facts: &crate::recovery::Facts,
        owners: &crate::recovery::Owners,
    ) -> Result<ResultStorageInspection, RuntimeError> {
        self.verify_original_task(
            &expected.lease.session_id,
            &expected.lease,
            &expected.task_id,
            facts,
        )?;
        let entries = storage_log(self, &expected.lease.session_id, facts.snapshot.cursor)?;
        let entry = entries
            .get(&expected.handle)
            .filter(|entry| entry.unresolved() && entry.pending.identity == *expected)
            .ok_or_else(|| {
                RuntimeError::InvalidInput(
                    "result storage identity is not the exact pending preparation".into(),
                )
            })?;
        let call = facts
            .calls
            .get(&expected.key)
            .ok_or_else(|| storage_corrupt("original result call is absent"))?;
        let execution = facts
            .snapshot
            .executions
            .iter()
            .find(|execution| execution.execution_id == expected.key.execution_id)
            .ok_or_else(|| storage_corrupt("original result execution is absent"))?;
        let identity = crate::recovery::cell_identity(&expected.lease, &expected.task_id);
        let scope = owners
            .cells
            .iter()
            .find(|scope| scope.identity() == &identity)
            .ok_or_else(|| storage_corrupt("original stopped result executor is absent"))?;
        let record = scope
            .record(&cayw_cells::CellExecution {
                identity,
                cell_id: execution.execution_id.clone(),
                parent_call_id: execution.parent_call_id.clone(),
                source_hash: execution.source_hash.clone(),
            })
            .map_err(crate::recovery::cell_error)?;
        crate::recovery::verify_execution(&record.execution(), execution)?;
        let sequence = expected
            .key
            .sequence
            .checked_add(1)
            .ok_or_else(|| storage_corrupt("result call sequence overflow"))?;
        let intent = record
            .calls
            .iter()
            .find(|intent| intent.sequence == sequence)
            .ok_or_else(|| storage_corrupt("original result Cells intent is absent"))?;
        let Some(CapabilityOutcome::Completed { result }) = &call.outcome else {
            return Err(storage_corrupt(
                "original result call is no longer successful",
            ));
        };
        if call.lease != expected.lease
            || call.task_id != expected.task_id
            || call.name != intent.name
            || call.arguments != intent.arguments
            || intent.status != cayw_cells::IntentStatus::Succeeded
            || intent.result_hash.as_deref()
                != Some(cayw_cells::HostResult::value_hash(result).as_str())
            || intent.result.as_ref().is_some_and(|value| value != result)
        {
            return Err(RuntimeError::InvalidInput("original Core and Cells success must agree before storage reconciliation; recover execution records first".into()));
        }
        let store = self
            .inner
            .content
            .as_ref()
            .filter(|store| store.device_id().as_str() == expected.device_id)
            .ok_or_else(|| {
                RuntimeError::InvalidInput("prepared result Device is unavailable".into())
            })?;
        let (body, metadata) = expected.keys()?;
        let read = |key: &Id| {
            store
                .try_read(key)
                .map(|value| value.map(|(record, bytes)| ResultStorageContent { record, bytes }))
                .map_err(|error| RuntimeError::Workspace(error.to_string()))
        };
        let body = read(&body)?;
        let metadata = read(&metadata)?;
        let observation = match (&body, &metadata) {
            (None, None) => ResultStorageObservation::Absent,
            (Some(_), None) => ResultStorageObservation::BodyOnly,
            (Some(_), Some(_)) => ResultStorageObservation::Complete,
            (None, Some(_)) => {
                return Err(RuntimeError::InvalidInput(
                    "result metadata has no body".into(),
                ));
            }
        };
        validate_storage_records(
            expected,
            observation,
            body.as_ref().map(|content| &content.record),
            metadata.as_ref().map(|content| &content.record),
        )?;
        if body.as_ref().is_some_and(|content| {
            ContentHash::of(&content.bytes).to_string() != expected.content_hash
        }) || metadata.as_ref().is_some_and(|content| {
            serde_json::from_slice::<Meta>(&content.bytes).ok().as_ref() != Some(&expected.meta())
        }) {
            return Err(RuntimeError::InvalidInput(
                "observed result bytes differ from their preparation".into(),
            ));
        }
        Ok(ResultStorageInspection {
            pending: entry.pending.clone(),
            observation,
            body,
            metadata,
        })
    }

    fn require_pending_storage(
        &self,
        expected: &ResultStorageIdentity,
        cursor: u64,
    ) -> Result<(), RuntimeError> {
        let entries = storage_log(self, &expected.lease.session_id, cursor)?;
        if entries
            .get(&expected.handle)
            .is_some_and(|entry| entry.unresolved() && entry.pending.identity == *expected)
        {
            return Ok(());
        }
        Err(RuntimeError::InvalidInput(
            "result storage identity is not the exact pending preparation".into(),
        ))
    }

    /// Host-only inspection holds original stopped Task/Cells ownership and
    /// requires Device process cleanup to have been confirmed already.
    pub fn inspect_result_storage(
        &self,
        expected: &ResultStorageIdentity,
    ) -> Result<ResultStorageInspection, RuntimeError> {
        let _coordinate = self.inner.coordination.enter()?;
        let facts = crate::recovery::Facts::read(self, &expected.lease.session_id)?;
        self.require_pending_storage(expected, facts.snapshot.cursor)?;
        let owners = self.stopped_owners(&expected.lease.session_id, &facts)?;
        self.inspect_result_storage_owned(expected, &facts, &owners)
    }

    /// Append a separate host decision; never replay content writes or replace
    /// the original tool terminal. The mandatory callback verifies independent
    /// host evidence for these exact observed records while ownership is held.
    pub fn reconcile_result_storage(
        &self,
        expected: &ResultStorageInspection,
        actor: &cayw_requests::HostActor,
        evidence: &str,
        verify: impl FnOnce(&ResultStorageInspection) -> Result<(), String>,
    ) -> Result<crate::SessionView, RuntimeError> {
        if evidence.trim().is_empty() || evidence.len() > 4096 {
            return Err(RuntimeError::InvalidInput(
                "result storage requires 1..4096 bytes of independent host evidence".into(),
            ));
        }
        let _coordinate = self.inner.coordination.enter()?;
        let id = &expected.pending.identity.lease.session_id;
        let facts = crate::recovery::Facts::read(self, id)?;
        self.require_pending_storage(&expected.pending.identity, facts.snapshot.cursor)?;
        let owners = self.stopped_owners(id, &facts)?;
        let actual =
            self.inspect_result_storage_owned(&expected.pending.identity, &facts, &owners)?;
        if actual != *expected {
            return Err(RuntimeError::InvalidInput(
                "result storage observation changed; inspect its exact state again".into(),
            ));
        }
        verify(&actual).map_err(|error| {
            RuntimeError::InvalidInput(format!(
                "independent result storage evidence rejected: {error}"
            ))
        })?;
        if self.inspect_result_storage_owned(&expected.pending.identity, &facts, &owners)? != actual
        {
            return Err(RuntimeError::InvalidInput(
                "result storage changed during host verification".into(),
            ));
        }
        let (body_key, metadata_key) = actual.pending.identity.keys()?;
        self.inner
            .content
            .as_ref()
            .ok_or_else(|| {
                RuntimeError::InvalidInput("prepared result Device is unavailable".into())
            })?
            .confirm_observed(&[
                (
                    body_key,
                    actual.body.as_ref().map(|content| content.record.clone()),
                ),
                (
                    metadata_key,
                    actual
                        .metadata
                        .as_ref()
                        .map(|content| content.record.clone()),
                ),
            ])?;
        let decision = StorageDecision {
            identity: actual.pending.identity,
            observation: actual.observation,
            body: actual.body.map(|content| content.record),
            metadata: actual.metadata.map(|content| content.record),
            evidence: evidence.into(),
        };
        self.inner.store.append_control_event(
            id,
            facts.snapshot.cursor,
            facts.snapshot.attempt.as_ref(),
            actor.as_str(),
            STORAGE_RECONCILED,
            serde_json::to_value(decision)?,
        )?;
        self.inner.changed.notify_waiters();
        self.get(id)
    }
}

fn known(error: impl std::fmt::Display) -> CapabilityError {
    CapabilityError::new(error.to_string())
}

fn is_busy(error: &CapabilityError) -> bool {
    error.message.contains("busy")
}

fn content_error(error: cayw_content::Error) -> CapabilityError {
    match error {
        cayw_content::Error::CommitOutcomeUnknown(_) => CapabilityError::unknown(error.to_string()),
        cayw_content::Error::Conflict { actual, .. } => known(format!(
            "result revision conflict; actual revision: {actual:?}"
        )),
        _ => known(error),
    }
}

fn new_handle() -> Result<Id, CapabilityError> {
    let mut bytes = [0u8; 16];
    getrandom_fill(&mut bytes)?;
    let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    Id::new(hex).map_err(known)
}

fn getrandom_fill(bytes: &mut [u8]) -> Result<(), CapabilityError> {
    use std::fs::File;
    use std::io::Read;
    File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(bytes))
        .map_err(|_| known("result handle entropy unavailable"))
}

fn body_key(
    device: &str,
    session: &str,
    task: Option<&str>,
    handle: &Id,
) -> Result<Id, RuntimeError> {
    let bytes = serde_json::to_vec(&(
        "cayw-runtime-result-body-v1",
        device,
        session,
        task,
        handle.as_str(),
    ))?;
    Ok(Id::new(ContentHash::of(&bytes).to_string())?)
}

fn meta_key(
    device: &str,
    session: &str,
    task: Option<&str>,
    handle: &Id,
) -> Result<Id, RuntimeError> {
    let bytes = serde_json::to_vec(&(
        "cayw-runtime-result-meta-v1",
        device,
        session,
        task,
        handle.as_str(),
    ))?;
    Ok(Id::new(ContentHash::of(&bytes).to_string())?)
}

fn owners<'a>(
    scope: ResultScope,
    _session: &'a str,
    task_id: &'a str,
) -> (Option<&'a str>, ResultScope) {
    match scope {
        ResultScope::Session => (None, ResultScope::Session),
        ResultScope::Task => (Some(task_id), ResultScope::Task),
    }
}

fn encoding_label(encoding: &Encoding) -> &'static str {
    match encoding {
        Encoding::Utf8 => "utf8",
        Encoding::Json => "json",
        Encoding::Base64 => "base64",
    }
}

fn parse_encoding(value: Option<&str>) -> Result<Encoding, CapabilityError> {
    match value.unwrap_or("utf8") {
        "utf8" => Ok(Encoding::Utf8),
        "json" => Ok(Encoding::Json),
        "base64" => Ok(Encoding::Base64),
        _ => Err(known("encoding must be utf8, json, or base64")),
    }
}

fn validate_payload(text: &str, encoding: &Encoding) -> Result<(), CapabilityError> {
    if text.is_empty() {
        return Err(known("result text must not be empty"));
    }
    match encoding {
        Encoding::Json => {
            serde_json::from_str::<Value>(text)
                .map_err(|_| known("result text is not valid JSON"))?;
        }
        Encoding::Base64 => {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD
                .decode(text.as_bytes())
                .map_err(|_| known("result text is not valid base64"))?;
        }
        Encoding::Utf8 => {}
    }
    Ok(())
}

/// Clamp a byte offset to a UTF-8 character boundary at or before `offset`.
pub fn utf8_floor(bytes: &[u8], offset: usize) -> usize {
    let mut at = offset.min(bytes.len());
    // EOF is a valid boundary — never index at `len`.
    if at == bytes.len() {
        return at;
    }
    while at > 0 && bytes[at] & 0b1100_0000 == 0b1000_0000 {
        at -= 1;
    }
    at
}

/// Clamp a byte end to a UTF-8 character boundary at or before `end`.
pub fn utf8_ceil_end(bytes: &[u8], start: usize, end: usize) -> usize {
    let mut at = end.min(bytes.len()).max(start);
    while at > start && at < bytes.len() && bytes[at] & 0b1100_0000 == 0b1000_0000 {
        at -= 1;
    }
    at
}

pub fn page_bytes(bytes: &[u8], offset: u64, limit: usize) -> (u64, u64, bool, String) {
    let start = utf8_floor(bytes, offset as usize);
    if start >= bytes.len() {
        let eof = bytes.len() as u64;
        return (eof, eof, false, String::new());
    }
    let mut end = utf8_ceil_end(bytes, start, start.saturating_add(limit));
    // Limit smaller than one multibyte char would otherwise yield an empty
    // page with next==start and has_more, stalling pagination — advance past
    // that character instead.
    if end == start {
        end = start + 1;
        while end < bytes.len() && bytes[end] & 0b1100_0000 == 0b1000_0000 {
            end += 1;
        }
    }
    let slice = &bytes[start..end];
    let text = String::from_utf8_lossy(slice).into_owned();
    let next = end as u64;
    let has_more = end < bytes.len();
    (start as u64, next, has_more, text)
}

pub fn search_bytes(bytes: &[u8], query: &str, limit: usize) -> Vec<ResultSnippet> {
    if query.is_empty() || limit == 0 {
        return vec![];
    }
    let text = String::from_utf8_lossy(bytes);
    let mut matches = Vec::new();
    let mut byte_offset = 0usize;
    for (line_no, line) in text.split_inclusive('\n').enumerate() {
        if line.contains(query) {
            let snippet = if line.len() > MAX_SNIPPET_BYTES {
                let end = utf8_ceil_end(line.as_bytes(), 0, MAX_SNIPPET_BYTES);
                format!("{}…", &line[..end].trim_end_matches('\n'))
            } else {
                line.trim_end_matches('\n').to_string()
            };
            matches.push(ResultSnippet {
                offset: byte_offset as u64,
                line: (line_no as u64) + 1,
                text: snippet,
            });
            if matches.len() >= limit {
                break;
            }
        }
        byte_offset += line.len();
    }
    matches
}

fn view_from_meta(meta: &Meta) -> ResultHandleView {
    ResultHandleView {
        handle: meta.handle.clone(),
        scope: meta.scope,
        session_id: meta.session_id.clone(),
        task_id: meta.task_id.clone(),
        content_hash: meta.content_hash.clone(),
        bytes: meta.bytes,
        encoding: encoding_label(&meta.encoding).into(),
        released: meta.released,
    }
}

fn read_meta(store: &ContentStore, key: &Id) -> Result<Option<(Record, Meta)>, CapabilityError> {
    match store.try_read(key) {
        Err(cayw_content::Error::Busy) => Err(known("result metadata store is busy")),
        Err(error) => Err(content_error(error)),
        Ok(None) => Ok(None),
        Ok(Some((record, bytes))) => {
            let meta: Meta =
                serde_json::from_slice(&bytes).map_err(|_| known("result metadata is corrupt"))?;
            Ok(Some((record, meta)))
        }
    }
}

fn read_body(store: &ContentStore, key: &Id) -> Result<Option<Vec<u8>>, CapabilityError> {
    match store.try_read(key) {
        Err(cayw_content::Error::Busy) => Err(known("result body store is busy")),
        Err(error) => Err(content_error(error)),
        Ok(None) => Ok(None),
        Ok(Some((_record, bytes))) => Ok(Some(bytes)),
    }
}

fn require_live(meta: &Meta) -> Result<(), CapabilityError> {
    if meta.released {
        return Err(known("result handle has been released"));
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StoreInput {
    text: String,
    #[serde(default)]
    encoding: Option<String>,
    #[serde(default)]
    scope: ResultScope,
    handle: Option<Id>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GetInput {
    handle: Id,
    #[serde(default)]
    scope: ResultScope,
    #[serde(default)]
    offset: u64,
    #[serde(default = "default_page_limit")]
    limit: usize,
}

fn default_page_limit() -> usize {
    4096
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BatchGetInput {
    handles: Vec<Id>,
    #[serde(default)]
    scope: ResultScope,
    #[serde(default)]
    include_page: bool,
    #[serde(default = "default_page_limit")]
    limit: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchInput {
    handle: Id,
    query: String,
    #[serde(default)]
    scope: ResultScope,
    #[serde(default = "default_search_limit")]
    limit: usize,
}

fn default_search_limit() -> usize {
    8
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReleaseInput {
    handle: Id,
    #[serde(default)]
    scope: ResultScope,
}

pub(crate) async fn invoke(
    scope: &Arc<CallScope>,
    base: &cayw_host::Host,
    arguments: Value,
    cancel: CancellationToken,
) -> Result<Value, CapabilityError> {
    let definition = definitions(scope.context.inner.config.content.as_ref())
        .into_iter()
        .find(|definition| definition.name == scope.name)
        .ok_or_else(|| known("local content storage is not configured"))?;
    let host = crate::bridge::scoped_host(base, scope)?;
    let service = Service { scope };
    let child = cancel.child_token();
    let _cancel_on_drop = child.clone().drop_guard();
    let run = host.call_service(&definition, arguments, child.clone(), &service);
    crate::requests::watch(scope, child, run).await
}

struct Service<'a> {
    scope: &'a Arc<CallScope>,
}

#[async_trait]
impl cayw_host::ExternalHost for Service<'_> {
    async fn call(
        &self,
        capability: &str,
        arguments: Value,
        cancel: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        if capability != self.scope.name {
            return Err(known("result service differs from the original capability"));
        }
        let inner = &self.scope.context.inner;
        let store = inner
            .content
            .as_ref()
            .ok_or_else(|| known("local content storage is not configured"))?;
        let session = self.scope.context.lease.session_id.as_str();
        let task_id = self.scope.context.task.id.as_str();
        loop {
            self.scope.check_authority(&cancel)?;
            if let Err(error) = check_private(store) {
                return Err(known(error));
            }
            let outcome = match capability {
                STORE => store_result(store, session, task_id, arguments.clone()),
                GET => get_result(store, session, task_id, arguments.clone()),
                BATCH_GET => batch_get(store, session, task_id, arguments.clone()),
                SEARCH => search_result(store, session, task_id, arguments.clone()),
                RELEASE => release_result(store, session, task_id, arguments.clone()),
                _ => return Err(known("not a result capability")),
            };
            match outcome {
                Err(error) if is_busy(&error) => {
                    tokio::select! { biased;
                        _ = cancel.cancelled() => return Err(known("result operation cancelled before acquiring its transaction lock")),
                        _ = tokio::time::sleep(Duration::from_millis(20)) => {},
                    }
                }
                other => return other,
            }
        }
    }
}

fn check_private(store: &ContentStore) -> Result<(), String> {
    let metadata = store.root_metadata().map_err(|e| e.to_string())?;
    if metadata.mode & 0o022 != 0 {
        return Err("content directory must not be writable by group or other users".into());
    }
    Ok(())
}

fn store_result(
    store: &ContentStore,
    session: &str,
    task_id: &str,
    arguments: Value,
) -> Result<Value, CapabilityError> {
    let input: StoreInput = serde_json::from_value(arguments).map_err(known)?;
    let encoding = parse_encoding(input.encoding.as_deref())?;
    validate_payload(&input.text, &encoding)?;
    let max = store.limits().max_content_bytes.min(1024 * 1024);
    if input.text.len() as u64 > max {
        return Err(known("result text exceeds its UTF-8 byte budget"));
    }
    let (task, scope) = owners(input.scope, session, task_id);
    let handle = match input.handle {
        Some(handle) => handle,
        None => new_handle()?,
    };
    let body = body_key(store.device_id().as_str(), session, task, &handle).map_err(known)?;
    let meta_name = meta_key(store.device_id().as_str(), session, task, &handle).map_err(known)?;
    if read_meta(store, &meta_name)?.is_some() {
        return Err(known("result handle already exists"));
    }
    let bytes = input.text.as_bytes();
    let content_hash = ContentHash::of(bytes).to_string();
    let record = match store.try_put(&body, ExpectedRevision::Absent, bytes) {
        Err(cayw_content::Error::Busy) => return Err(known("result body store is busy")),
        Err(error) => return Err(content_error(error)),
        Ok(record) => record,
    };
    let meta = Meta {
        v: 1,
        handle: handle.to_string(),
        content_hash: content_hash.clone(),
        bytes: bytes.len() as u64,
        encoding,
        released: false,
        scope,
        session_id: session.into(),
        task_id: task.map(str::to_owned),
    };
    let meta_bytes = serde_json::to_vec(&meta).map_err(known)?;
    match store.try_put(&meta_name, ExpectedRevision::Absent, &meta_bytes) {
        Err(cayw_content::Error::Busy) => return Err(known("result metadata store is busy")),
        Err(error) => return Err(content_error(error)),
        Ok(_) => {}
    }
    let _ = record;
    let view = view_from_meta(&meta);
    serde_json::to_value(view).map_err(known).map(|mut value| {
        if let Some(obj) = value.as_object_mut() {
            obj.insert("stored".into(), Value::Bool(true));
        }
        value
    })
}

fn load_live(
    store: &ContentStore,
    session: &str,
    task_id: &str,
    scope: ResultScope,
    handle: &Id,
) -> Result<(Meta, Vec<u8>), CapabilityError> {
    let (task, _) = owners(scope, session, task_id);
    let meta_name = meta_key(store.device_id().as_str(), session, task, handle).map_err(known)?;
    let body = body_key(store.device_id().as_str(), session, task, handle).map_err(known)?;
    let Some((_record, meta)) = read_meta(store, &meta_name)? else {
        return Err(known("result handle not found"));
    };
    require_live(&meta)?;
    let Some(bytes) = read_body(store, &body)? else {
        return Err(known("result body not found"));
    };
    let actual = ContentHash::of(&bytes).to_string();
    if meta.v != 1
        || meta.handle != handle.as_str()
        || meta.session_id != session
        || meta.task_id.as_deref() != task
        || meta.scope != scope
    {
        return Err(known("result metadata differs from its owner"));
    }
    if actual != meta.content_hash || bytes.len() as u64 != meta.bytes {
        return Err(known("result body differs from its hash or byte length"));
    }
    let text =
        std::str::from_utf8(&bytes).map_err(|_| known("result body is not UTF-8 encoded"))?;
    if !text.is_empty() || meta.encoding == Encoding::Json {
        validate_payload(text, &meta.encoding)?;
    }
    Ok((meta, bytes))
}

fn get_result(
    store: &ContentStore,
    session: &str,
    task_id: &str,
    arguments: Value,
) -> Result<Value, CapabilityError> {
    let input: GetInput = serde_json::from_value(arguments).map_err(known)?;
    if input.limit == 0 || input.limit > MAX_PAGE_BYTES {
        return Err(known("result page limit out of range"));
    }
    let (meta, bytes) = load_live(store, session, task_id, input.scope, &input.handle)?;
    let (offset, next_offset, has_more, text) = page_bytes(&bytes, input.offset, input.limit);
    serde_json::to_value(ResultPage {
        handle: meta.handle,
        content_hash: meta.content_hash,
        total_bytes: meta.bytes,
        offset,
        next_offset,
        bytes_returned: text.len() as u64,
        has_more,
        text,
        released: false,
    })
    .map_err(known)
}

fn batch_get(
    store: &ContentStore,
    session: &str,
    task_id: &str,
    arguments: Value,
) -> Result<Value, CapabilityError> {
    let input: BatchGetInput = serde_json::from_value(arguments).map_err(known)?;
    if input.handles.is_empty() || input.handles.len() > MAX_BATCH {
        return Err(known("result batch size out of range"));
    }
    if input.limit == 0 || input.limit > MAX_PAGE_BYTES {
        return Err(known("result page limit out of range"));
    }
    let mut items = Vec::with_capacity(input.handles.len());
    for handle in &input.handles {
        let entry = match load_live(store, session, task_id, input.scope, handle) {
            Ok((meta, bytes)) => {
                let mut item = json!({"ok":true,"handle":view_from_meta(&meta)});
                if input.include_page {
                    let (offset, next_offset, has_more, text) = page_bytes(&bytes, 0, input.limit);
                    item["page"] = json!(ResultPage {
                        handle: meta.handle,
                        content_hash: meta.content_hash,
                        total_bytes: meta.bytes,
                        offset,
                        next_offset,
                        bytes_returned: text.len() as u64,
                        has_more,
                        text,
                        released: false,
                    });
                }
                item
            }
            Err(error) => json!({"ok":false,"handle":handle.to_string(),"error":error.message}),
        };
        items.push(entry);
    }
    Ok(json!({"items":items}))
}

fn search_result(
    store: &ContentStore,
    session: &str,
    task_id: &str,
    arguments: Value,
) -> Result<Value, CapabilityError> {
    let input: SearchInput = serde_json::from_value(arguments).map_err(known)?;
    if input.query.is_empty() || input.query.len() > 256 {
        return Err(known("result search query out of range"));
    }
    if input.limit == 0 || input.limit > MAX_SEARCH_MATCHES {
        return Err(known("result search limit out of range"));
    }
    let (meta, bytes) = load_live(store, session, task_id, input.scope, &input.handle)?;
    let matches = search_bytes(&bytes, &input.query, input.limit);
    Ok(json!({
        "handle": meta.handle,
        "content_hash": meta.content_hash,
        "total_bytes": meta.bytes,
        "query": input.query,
        "matches": matches,
        "truncated": matches.len() >= input.limit,
    }))
}

fn release_result(
    store: &ContentStore,
    session: &str,
    task_id: &str,
    arguments: Value,
) -> Result<Value, CapabilityError> {
    let input: ReleaseInput = serde_json::from_value(arguments).map_err(known)?;
    let (task, _) = owners(input.scope, session, task_id);
    let meta_name =
        meta_key(store.device_id().as_str(), session, task, &input.handle).map_err(known)?;
    let Some((record, mut meta)) = read_meta(store, &meta_name)? else {
        return Err(known("result handle not found"));
    };
    if meta.released {
        return serde_json::to_value(view_from_meta(&meta)).map_err(known);
    }
    meta.released = true;
    let meta_bytes = serde_json::to_vec(&meta).map_err(known)?;
    match store.try_put(&meta_name, record.expected(), &meta_bytes) {
        Err(cayw_content::Error::Busy) => return Err(known("result metadata store is busy")),
        Err(error) => return Err(content_error(error)),
        Ok(_) => {}
    }
    serde_json::to_value(view_from_meta(&meta)).map_err(known)
}

impl crate::Runtime {
    /// Trusted host inspection of a result handle (including released).
    pub fn result_inspect(
        &self,
        session: &str,
        task: Option<&str>,
        handle: &str,
    ) -> Result<Option<ResultHandleView>, RuntimeError> {
        self.get(session)?;
        if let Some(task) = task
            && self.task(task)?.session_id != session
        {
            return Err(RuntimeError::InvalidInput(
                "result Task does not belong to the requested Session".into(),
            ));
        }
        let handle = Id::new(handle)?;
        let store = self.inner.content.as_ref().ok_or_else(|| {
            RuntimeError::InvalidInput("local content storage is not configured".into())
        })?;
        let meta_name = meta_key(store.device_id().as_str(), session, task, &handle)?;
        match store.try_read(&meta_name)? {
            None => Ok(None),
            Some((_record, bytes)) => {
                let meta: Meta = serde_json::from_slice(&bytes)
                    .map_err(|_| RuntimeError::InvalidInput("result metadata is corrupt".into()))?;
                Ok(Some(view_from_meta(&meta)))
            }
        }
    }
}

/// Check if a capability call is reading a session-scoped result that belongs
/// to the current session. Used by permissions to allow subsequent tasks in the
/// same session to read existing session results without prompting for new approval.
pub(crate) fn is_session_result_read(
    inner: &crate::Inner,
    session: &str,
    name: &str,
    args: &Value,
) -> bool {
    if !matches!(name, GET | SEARCH | BATCH_GET | RELEASE) {
        return false;
    }
    let Some(store) = inner.content.as_ref() else {
        return false;
    };
    match name {
        GET | SEARCH | RELEASE => {
            let Some(handle_str) = args.get("handle").and_then(Value::as_str) else {
                return false;
            };
            let Ok(handle) = Id::new(handle_str) else {
                return false;
            };
            if args.get("scope").and_then(Value::as_str) == Some("task") {
                return false;
            }
            let Ok(meta_name) = meta_key(store.device_id().as_str(), session, None, &handle) else {
                return false;
            };
            matches!(read_meta(store, &meta_name), Ok(Some((_, meta))) if meta.session_id == session)
        }
        BATCH_GET => {
            let Some(handles) = args.get("handles").and_then(Value::as_array) else {
                return false;
            };
            if handles.is_empty() || args.get("scope").and_then(Value::as_str) == Some("task") {
                return false;
            }
            let mut found_any = false;
            for h in handles {
                let Some(handle_str) = h.as_str() else {
                    return false;
                };
                let Ok(handle) = Id::new(handle_str) else {
                    return false;
                };
                let Ok(meta_name) = meta_key(store.device_id().as_str(), session, None, &handle)
                else {
                    return false;
                };
                if let Ok(Some((_, meta))) = read_meta(store, &meta_name) {
                    if meta.session_id != session {
                        return false;
                    }
                    found_any = true;
                }
            }
            found_any
        }
        _ => false,
    }
}

#[cfg(test)]
mod unit {
    use super::{page_bytes, search_bytes, utf8_floor};

    #[cfg(target_os = "macos")]
    use super::*;
    #[cfg(target_os = "macos")]
    use cayw_content::{ContentStore, Limits};
    #[cfg(target_os = "macos")]
    use serde_json::json;
    #[cfg(target_os = "macos")]
    use tempfile::TempDir;

    #[cfg(target_os = "macos")]
    fn open_store() -> (TempDir, ContentStore) {
        // ContentStore::open walks with open_dir_nofollow; macOS temp paths often
        // go through /tmp or /var/folders symlinks, which yield ENOTDIR (Not a
        // directory). Create under a canonicalized temp root like cayw-content tests.
        let root = TempDir::new_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let store =
            ContentStore::open(root.path(), Id::new("device-a").unwrap(), Limits::default())
                .unwrap();
        (root, store)
    }

    #[test]
    fn pagination_respects_utf8_boundaries() {
        let text = "a😀b";
        let bytes = text.as_bytes();
        assert_eq!(utf8_floor(bytes, 2), 1);
        let (offset, next, has_more, page) = page_bytes(bytes, 1, 4);
        assert_eq!(offset, 1);
        assert!(page.starts_with('😀') || page == "😀");
        assert!(next > offset);
        assert!(has_more || next == bytes.len() as u64);
    }

    #[test]
    fn pagination_eof_offset_returns_empty_page() {
        let bytes = b"abc";
        // offset == len and offset > len must not panic; return empty EOF page.
        for offset in [3u64, 4, 100] {
            let (start, next, has_more, page) = page_bytes(bytes, offset, 8);
            assert_eq!(start, 3, "offset={offset}");
            assert_eq!(next, 3, "offset={offset}");
            assert!(!has_more, "offset={offset}");
            assert_eq!(page, "", "offset={offset}");
        }
        assert_eq!(utf8_floor(bytes, 3), 3);
        assert_eq!(utf8_floor(bytes, 99), 3);
    }

    #[test]
    fn pagination_advances_when_limit_smaller_than_multibyte_char() {
        let text = "a😀b";
        let bytes = text.as_bytes();
        // 😀 is 4 bytes starting at offset 1; limit=1 must still advance past it.
        let (offset, next, has_more, page) = page_bytes(bytes, 1, 1);
        assert_eq!(offset, 1);
        assert_eq!(page, "😀");
        assert_eq!(next, 5);
        assert!(has_more);
        let (offset2, next2, has_more2, page2) = page_bytes(bytes, next, 8);
        assert_eq!(offset2, 5);
        assert_eq!(page2, "b");
        assert_eq!(next2, 6);
        assert!(!has_more2);
    }

    #[test]
    fn auto_externalize_default_threshold_is_32_kib() {
        assert_eq!(super::DEFAULT_AUTO_EXTERNALIZE_THRESHOLD_BYTES, 32 * 1024);
    }

    #[test]
    fn should_externalize_respects_threshold_and_store_cap() {
        assert!(!super::should_externalize(0, 100, 1024));
        assert!(!super::should_externalize(64, 64, 1024));
        assert!(!super::should_externalize(64, 32, 1024));
        assert!(super::should_externalize(64, 65, 1024));
        assert!(!super::should_externalize(64, 2048, 1024));
    }

    #[test]
    fn search_returns_line_snippets() {
        let body = b"alpha\nbeta error here\ngamma\nerror again\n";
        let hits = search_bytes(body, "error", 8);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].line, 2);
        assert!(hits[0].text.contains("error"));
        assert_eq!(hits[1].line, 4);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn content_store_backed_handle_pages_searches_and_releases() {
        let (_root, store) = open_store();
        let body = "line-one\nneedle alpha\nline-three\nneedle beta\ntrail";
        let stored = store_result(
            &store,
            "sess-a",
            "task-a",
            json!({"text": body, "encoding": "utf8", "handle": "demo1"}),
        )
        .unwrap();
        assert_eq!(stored["handle"], "demo1");
        assert_eq!(stored["bytes"], body.len() as u64);

        let page = get_result(
            &store,
            "sess-a",
            "task-a",
            json!({"handle": "demo1", "offset": 0, "limit": 16}),
        )
        .unwrap();
        assert_eq!(page["has_more"], true);
        let rest = get_result(
            &store,
            "sess-a",
            "task-a",
            json!({"handle": "demo1", "offset": page["next_offset"], "limit": 64}),
        )
        .unwrap();
        assert_eq!(rest["has_more"], false);
        assert_eq!(
            format!(
                "{}{}",
                page["text"].as_str().unwrap(),
                rest["text"].as_str().unwrap()
            ),
            body
        );

        let found = search_result(
            &store,
            "sess-a",
            "task-a",
            json!({"handle": "demo1", "query": "needle", "limit": 8}),
        )
        .unwrap();
        assert_eq!(found["matches"].as_array().unwrap().len(), 2);

        let batch = batch_get(
            &store,
            "sess-a",
            "task-a",
            json!({"handles": ["demo1", "missing"], "include_page": true, "limit": 8}),
        )
        .unwrap();
        assert_eq!(batch["items"][0]["ok"], true);
        assert_eq!(batch["items"][1]["ok"], false);

        let released =
            release_result(&store, "sess-a", "task-a", json!({"handle": "demo1"})).unwrap();
        assert_eq!(released["released"], true);
        let err = get_result(
            &store,
            "sess-a",
            "task-a",
            json!({"handle": "demo1", "offset": 0, "limit": 8}),
        )
        .unwrap_err();
        assert!(err.message.contains("released"));
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn task_scope_does_not_collide_with_session_scope() {
        let (_root, store) = open_store();
        store_result(
            &store,
            "sess-a",
            "task-a",
            json!({"text": "task-body", "scope": "task", "handle": "same"}),
        )
        .unwrap();
        store_result(
            &store,
            "sess-a",
            "task-a",
            json!({"text": "session-body", "scope": "session", "handle": "same"}),
        )
        .unwrap();
        let task_page = get_result(
            &store,
            "sess-a",
            "task-a",
            json!({"handle": "same", "scope": "task", "offset": 0, "limit": 32}),
        )
        .unwrap();
        let session_page = get_result(
            &store,
            "sess-a",
            "task-a",
            json!({"handle": "same", "scope": "session", "offset": 0, "limit": 32}),
        )
        .unwrap();
        assert_eq!(task_page["text"], "task-body");
        assert_eq!(session_page["text"], "session-body");
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn over_threshold_host_result_is_stored_and_retrievable_by_handle() {
        let (_root, store) = open_store();
        let threshold = 64usize;
        let body = json!({"payload": "x".repeat(200), "n": 42});
        let stub = externalize_value(&store, "sess-a", "task-a", threshold, body.clone()).unwrap();
        assert_eq!(stub["externalized"], true);
        assert_eq!(stub["stored"], true);
        assert_eq!(stub["encoding"], "json");
        assert!(stub["bytes"].as_u64().unwrap() > threshold as u64);
        let handle = stub["handle"].as_str().unwrap();

        let page = get_result(
            &store,
            "sess-a",
            "task-a",
            json!({"handle": handle, "offset": 0, "limit": 4096}),
        )
        .unwrap();
        assert_eq!(page["has_more"], false);
        let recovered: Value = serde_json::from_str(page["text"].as_str().unwrap()).unwrap();
        assert_eq!(recovered, body);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn under_threshold_host_result_passes_through_unchanged() {
        let (_root, store) = open_store();
        let body = json!({"ok": true, "n": 1});
        let out = externalize_value(&store, "sess-a", "task-a", 1024, body.clone()).unwrap();
        assert_eq!(out, body);
        assert!(out.get("externalized").is_none());
        assert!(out.get("handle").is_none());
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn zero_threshold_disables_auto_externalize() {
        let (_root, store) = open_store();
        let body = json!({"payload": "y".repeat(500)});
        let out = externalize_value(&store, "sess-a", "task-a", 0, body.clone()).unwrap();
        assert_eq!(out, body);
    }
}
