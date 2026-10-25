//! Unified tool-result handles (F01/F03): store large bodies in the same
//! Device ContentStore as #18, retrieve by handle with pagination, search for
//! snippets, and soft-release without a separate executor.
//!
//! ContentStore has no delete API; release flips durable metadata so later
//! get/search/batch refuse the handle while the body object may remain for
//! quota accounting (aligned with cayw-content).
use crate::{RuntimeError, cells::CallScope, content};
use async_trait::async_trait;
use cayw_ai::{CancellationToken, ToolDefinition};
use cayw_codemode::CapabilityError;
use cayw_content::{ContentHash, ContentStore, ExpectedRevision, Id, Record};
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultScope {
    #[default]
    Session,
    Task,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Encoding {
    Utf8,
    Json,
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
                "encoding":{"enum":["utf8","json"]},
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
    }
}

fn parse_encoding(value: Option<&str>) -> Result<Encoding, CapabilityError> {
    match value.unwrap_or("utf8") {
        "utf8" => Ok(Encoding::Utf8),
        "json" => Ok(Encoding::Json),
        _ => Err(known("encoding must be utf8 or json")),
    }
}

fn validate_payload(text: &str, encoding: &Encoding) -> Result<(), CapabilityError> {
    if text.is_empty() {
        return Err(known("result text must not be empty"));
    }
    if matches!(encoding, Encoding::Json) {
        serde_json::from_str::<Value>(text).map_err(|_| known("result text is not valid JSON"))?;
    }
    Ok(())
}

/// Clamp a byte offset to a UTF-8 character boundary at or before `offset`.
pub fn utf8_floor(bytes: &[u8], offset: usize) -> usize {
    let mut at = offset.min(bytes.len());
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
    let end = utf8_ceil_end(bytes, start, start.saturating_add(limit));
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
    if actual != meta.content_hash {
        return Err(known("result body hash mismatch"));
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
        let root = TempDir::new().unwrap();
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
}
