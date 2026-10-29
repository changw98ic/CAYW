//! Local authoritative text. The original CallScope supplies every owner; model
//! arguments contain only a logical name, scope and exact observed revision.
use crate::{Inner, Runtime, RuntimeError, cells::CallScope, coordination};
use async_trait::async_trait;
use cayw_ai::{CancellationToken, ToolDefinition};
use cayw_codemode::CapabilityError;
use cayw_content::{ContentHash, ContentStore, ExpectedRevision, Id, Limits, Record};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::Duration,
};

const READ: &str = "content.read";
const PUT: &str = "content.put";
const MAX_TEXT_BYTES: u64 = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContentConfig {
    pub directory: PathBuf,
    pub device_id: String,
    #[serde(default)]
    pub limits: Limits,
}
impl ContentConfig {
    pub fn validate(&self) -> Result<(), RuntimeError> {
        Id::new(&self.device_id)?;
        self.limits.validate()?;
        if !self.directory.is_absolute()
            || self.directory.to_str().is_none()
            || self
                .directory
                .components()
                .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
        {
            return Err(RuntimeError::InvalidInput(
                "content directory must be an explicit absolute path without parent components"
                    .into(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentScope {
    #[default]
    Session,
    Task,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContentView {
    pub name: String,
    pub scope: ContentScope,
    pub session_id: String,
    pub task_id: Option<String>,
    pub record: Record,
    pub revision_hex: String,
    pub content_hash_hex: String,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Binding {
    /// The database never recorded a content decision.
    Absent,
    /// A trusted host opened this database with local content disabled.
    Disabled,
    Enabled {
        config: ContentConfig,
        root_device: u64,
        root_inode: u64,
    },
}

fn binding_connection(database: &Path) -> Result<Connection, RuntimeError> {
    let db = Connection::open(database)?;
    db.busy_timeout(Duration::from_secs(5))?;
    db.pragma_update(None, "synchronous", "FULL")?;
    Ok(db)
}

fn ensure_binding_table(db: &Connection) -> Result<(), RuntimeError> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS runtime_content_config (
        singleton INTEGER PRIMARY KEY CHECK(singleton=1), identity_json TEXT NOT NULL);",
    )?;
    Ok(())
}

fn read_binding_in(db: &Connection) -> Result<(Option<String>, Binding), RuntimeError> {
    let raw: Option<String> = db
        .query_row(
            "SELECT identity_json FROM runtime_content_config WHERE singleton=1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let binding = parse_binding(raw.as_deref())?;
    Ok((raw, binding))
}

fn read_binding(database: &Path) -> Result<Binding, RuntimeError> {
    if !database.exists() {
        return Ok(Binding::Absent);
    }
    let db = binding_connection(database)?;
    let has_table: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_content_config')",
        [],
        |row| row.get(0),
    )?;
    if !has_table {
        return Ok(Binding::Absent);
    }
    Ok(read_binding_in(&db)?.1)
}

fn parse_binding(encoded: Option<&str>) -> Result<Binding, RuntimeError> {
    let Some(encoded) = encoded else {
        return Ok(Binding::Absent);
    };
    let value: Value = serde_json::from_str(encoded).map_err(|_| binding_error())?;
    if value.is_null() {
        return Ok(Binding::Disabled);
    }
    let config: ContentConfig =
        serde_json::from_value(value.get("config").cloned().ok_or_else(binding_error)?)
            .map_err(|_| binding_error())?;
    let root_device = value
        .get("root_device")
        .and_then(Value::as_u64)
        .ok_or_else(binding_error)?;
    let root_inode = value
        .get("root_inode")
        .and_then(Value::as_u64)
        .ok_or_else(binding_error)?;
    Ok(Binding::Enabled {
        config,
        root_device,
        root_inode,
    })
}

fn binding_matches(binding: &Binding, requested: Option<&ContentConfig>) -> bool {
    match (binding, requested) {
        (Binding::Absent | Binding::Disabled, None) => true,
        (Binding::Enabled { config, .. }, Some(requested)) => config == requested,
        _ => false,
    }
}

fn write_binding(db: &Connection, encoded: &str) -> Result<(), RuntimeError> {
    db.execute(
        "INSERT INTO runtime_content_config(singleton,identity_json) VALUES(1,?1)
         ON CONFLICT(singleton) DO UPDATE SET identity_json=excluded.identity_json",
        [encoded],
    )?;
    Ok(())
}

/// A policy change may not race durable work. Live rows include queued work
/// that a worker could still claim under the previous policy.
fn require_settled(db: &Connection) -> Result<(), RuntimeError> {
    let has_tasks: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_tasks')",
        [],
        |row| row.get(0),
    )?;
    if has_tasks {
        let live: i64 = db.query_row(
            "SELECT count(*) FROM runtime_tasks WHERE holds_slot=1 OR state IN ('queued','running','waiting_approval','draining')",
            [],
            |row| row.get(0),
        )?;
        if live != 0 {
            return Err(RuntimeError::InvalidInput(
                "content policy can change only when no task is live; settle or recover live tasks first".into(),
            ));
        }
    }
    Ok(())
}

fn open_store_exact(
    config: &ContentConfig,
    expected_root: Option<(u64, u64)>,
) -> Result<ContentStore, RuntimeError> {
    let store = ContentStore::open_private(
        &config.directory,
        Id::new(&config.device_id)?,
        config.limits,
        expected_root,
    )?;
    check_private(&store)?;
    Ok(store)
}

/// Same root and device, different quota. The store itself adopts the limits
/// only when every retained record and object still fits them.
fn open_store_amending(
    config: &ContentConfig,
    root: (u64, u64),
) -> Result<ContentStore, RuntimeError> {
    let store = ContentStore::open_private_amending_limits(
        &config.directory,
        Id::new(&config.device_id)?,
        config.limits,
        root,
    )?;
    check_private(&store)?;
    Ok(store)
}

fn encode_identity(
    config: Option<&ContentConfig>,
    store: Option<&ContentStore>,
) -> Result<String, RuntimeError> {
    Ok(serde_json::to_string(&identity(config, store)?)?)
}

/// Re-reads the binding under the exclusive policy lock. A different binding
/// is accepted only for a first enablement (previously absent/disabled) or a
/// same-root/device quota change, and only when no task is live.
///
/// Returns `Some(())` when this call applied the change and `None` when
/// another connection had already made the requested binding effective. Both
/// outcomes must be followed by a fresh binding read: the exclusive lock is
/// released before the opener regains its lifetime shared lock.
fn amend_binding(
    database: &Path,
    config: Option<&ContentConfig>,
) -> Result<Option<()>, RuntimeError> {
    let mut db = binding_connection(database)?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    ensure_binding_table(&tx)?;
    let (_, prior) = read_binding_in(&tx)?;
    if binding_matches(&prior, config) {
        tx.commit()?;
        return Ok(None);
    }
    let (requested, expected_root, amend_limits) = match (&prior, config) {
        (_, None) => return Err(binding_error()),
        (Binding::Absent | Binding::Disabled, Some(config)) => (config, None, false),
        (
            Binding::Enabled {
                config: previous,
                root_device,
                root_inode,
            },
            Some(config),
        ) => {
            if previous.directory != config.directory || previous.device_id != config.device_id {
                return Err(binding_error());
            }
            (
                config,
                Some((*root_device, *root_inode)),
                previous.limits != config.limits,
            )
        }
    };
    require_settled(&tx)?;
    let store = if amend_limits {
        open_store_amending(requested, expected_root.ok_or_else(binding_error)?)?
    } else {
        open_store_exact(requested, expected_root)?
    };
    let encoded = encode_identity(Some(requested), Some(&store))?;
    write_binding(&tx, &encoded)?;
    tx.commit()?;
    Ok(Some(()))
}

/// Applies an already-matching (or absent) binding without changing policy.
fn open_binding(
    database: &Path,
    config: Option<&ContentConfig>,
) -> Result<Option<Arc<ContentStore>>, RuntimeError> {
    let mut db = binding_connection(database)?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    ensure_binding_table(&tx)?;
    let (raw, prior) = read_binding_in(&tx)?;
    if !binding_matches(&prior, config) {
        return Err(binding_error());
    }
    let store = match config {
        None => None,
        Some(config) => {
            let expected_root = match &prior {
                Binding::Enabled {
                    root_device,
                    root_inode,
                    ..
                } => Some((*root_device, *root_inode)),
                Binding::Absent | Binding::Disabled => None,
            };
            Some(Arc::new(open_store_exact(config, expected_root)?))
        }
    };
    let encoded = encode_identity(config, store.as_deref())?;
    if raw.as_deref() != Some(encoded.as_str()) {
        write_binding(&tx, &encoded)?;
    }
    tx.commit()?;
    Ok(store)
}

/// Opens once, then persists the actual directory identity. All later IO uses
/// this held ContentStore; the configured pathname is never reopened by a call.
pub(crate) fn open(
    database: &Path,
    workspace: &Path,
    config: Option<&ContentConfig>,
) -> Result<Option<Arc<ContentStore>>, RuntimeError> {
    if let Some(config) = config {
        config.validate()?;
        if config.directory.starts_with(workspace) || workspace.starts_with(&config.directory) {
            return Err(RuntimeError::InvalidInput("content directory and model workspace must be separate, non-overlapping directories".into()));
        }
    }
    if !binding_matches(&read_binding(database)?, config) {
        // A first enablement after `None` and a same-root/device quota change
        // are trusted host policy changes, not silent replacements. They
        // require every Runtime connection to have stopped so no old in-memory
        // store can admit work with the previous policy.
        coordination::amend_policy(database, || amend_binding(database, config))?;
    }
    // Re-read after the exclusive lock is released and shared ownership is
    // restored. A competing amendment may have won that transition. Runtime
    // also checks every opened policy after its last configuration amendment.
    open_binding(database, config)
}

pub(crate) fn check_binding(
    database: &Path,
    config: Option<&ContentConfig>,
    store: Option<&ContentStore>,
) -> Result<(), RuntimeError> {
    let expected = parse_binding(Some(&encode_identity(config, store)?))?;
    if read_binding(database)? != expected {
        return Err(binding_error());
    }
    if let Some(store) = store {
        check_private(store)?;
        store.validate_configuration()?;
    }
    Ok(())
}

fn binding_error() -> RuntimeError {
    RuntimeError::InvalidInput(
        "content binding cannot be replaced: the original directory and device are fixed, and enabled content cannot be disabled".into(),
    )
}
fn check_private(store: &ContentStore) -> Result<(), RuntimeError> {
    let metadata = store.root_metadata()?;
    if metadata.mode & 0o022 != 0 {
        return Err(RuntimeError::InvalidInput(
            "content directory must not be writable by group or other users".into(),
        ));
    }
    Ok(())
}
fn identity(
    config: Option<&ContentConfig>,
    store: Option<&ContentStore>,
) -> Result<Value, RuntimeError> {
    match (config, store) {
        (Some(config), Some(store)) => {
            let root = store.root_metadata()?;
            Ok(json!({"config":config,"root_device":root.device,"root_inode":root.inode}))
        }
        (None, None) => Ok(Value::Null),
        _ => Err(RuntimeError::InvalidInput(
            "content configuration and opened store disagree".into(),
        )),
    }
}
pub(crate) fn policy_identity(inner: &Inner) -> Result<Value, RuntimeError> {
    identity(inner.config.content.as_ref(), inner.content.as_deref())
}

pub(crate) fn definitions(config: Option<&ContentConfig>) -> Vec<ToolDefinition> {
    let Some(config) = config else {
        return vec![];
    };
    let name = json!({"type":"string","minLength":1,"maxLength":64,"pattern":"^[A-Za-z0-9_-]+$"});
    let scope = json!({"enum":["session","task"]});
    vec![
        ToolDefinition {
            name:READ.into(),
            description:"Read authoritative local UTF-8 text for the current Session or current Task. Returns null if absent. Owners and directory are fixed by the host; no path or owner override is accepted.".into(),
            parameters:json!({"type":"object","additionalProperties":false,"required":["name"],"properties":{"name":name,"scope":scope}}),
        },
        ToolDefinition {
            name:PUT.into(),
            description:"Create or update authoritative local UTF-8 text. Creation requires expected_version=0 and no expected_revision. Updates require the exact version and lowercase revision returned by content.read. A conflict never overwrites newer content. Unknown writes must be inspected and reconciled, never replayed automatically.".into(),
            parameters:json!({"type":"object","additionalProperties":false,"required":["name","text","expected_version"],"properties":{
                "name":name,"scope":scope,"text":{"type":"string","maxLength":config.limits.max_content_bytes.min(MAX_TEXT_BYTES)},
                "expected_version":{"type":"integer","minimum":0},
                "expected_revision":{"type":"string","minLength":64,"maxLength":64,"pattern":"^[0-9a-f]{64}$"}
            }}),
        },
    ]
}
pub(crate) fn is_tool(name: &str) -> bool {
    matches!(name, READ | PUT)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadInput {
    name: Id,
    #[serde(default)]
    scope: ContentScope,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PutInput {
    name: Id,
    #[serde(default)]
    scope: ContentScope,
    text: String,
    expected_version: u64,
    expected_revision: Option<String>,
}
impl PutInput {
    fn expected(&self) -> Result<ExpectedRevision, CapabilityError> {
        match (self.expected_version, self.expected_revision.as_deref()) {
            (0, None) => Ok(ExpectedRevision::Absent),
            (0, Some(_)) | (_, None) => Err(known(
                "creation requires version 0 without a revision; updates require both observed version and revision",
            )),
            (version, Some(revision)) => Ok(ExpectedRevision::Present {
                version,
                revision: ContentHash::from_hex(revision).map_err(known)?,
            }),
        }
    }
}
fn record_name(
    device: &str,
    session: &str,
    task: Option<&str>,
    name: &Id,
) -> Result<Id, RuntimeError> {
    let bytes = serde_json::to_vec(&(
        "cayw-runtime-content-v1",
        device,
        session,
        task,
        name.as_str(),
    ))?;
    Ok(Id::new(ContentHash::of(&bytes).to_string())?)
}
fn view(
    session: &str,
    task: Option<&str>,
    name: &Id,
    record: Record,
    bytes: Vec<u8>,
) -> Result<ContentView, RuntimeError> {
    let text = String::from_utf8(bytes).map_err(|_| cayw_content::Error::Integrity)?;
    Ok(ContentView {
        name: name.to_string(),
        scope: if task.is_some() {
            ContentScope::Task
        } else {
            ContentScope::Session
        },
        session_id: session.into(),
        task_id: task.map(str::to_owned),
        revision_hex: record.revision.to_string(),
        content_hash_hex: record.content_hash.to_string(),
        record,
        text,
    })
}

impl Runtime {
    /// Trusted host inspection, including while an original write needs recovery.
    /// Model reads use content.read and its ordinary exact permission instead.
    pub fn content_inspect(
        &self,
        session: &str,
        task: Option<&str>,
        name: &str,
    ) -> Result<Option<ContentView>, RuntimeError> {
        self.get(session)?;
        if let Some(task) = task
            && self.task(task)?.session_id != session
        {
            return Err(RuntimeError::InvalidInput(
                "content Task does not belong to the requested Session".into(),
            ));
        }
        let name = Id::new(name)?;
        let store = self.inner.content.as_ref().ok_or_else(|| {
            RuntimeError::InvalidInput("local content storage is not configured".into())
        })?;
        check_private(store)?;
        let key = record_name(store.device_id().as_str(), session, task, &name)?;
        store
            .try_read(&key)?
            .map(|(record, bytes)| view(session, task, &name, record, bytes))
            .transpose()
    }
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
fn known(error: impl std::fmt::Display) -> CapabilityError {
    CapabilityError::new(error.to_string())
}
fn content_error(error: cayw_content::Error) -> CapabilityError {
    match error {
        cayw_content::Error::CommitOutcomeUnknown(_) => CapabilityError::unknown(error.to_string()),
        cayw_content::Error::Conflict { actual, .. } => known(format!(
            "content revision conflict; actual revision: {actual:?}"
        )),
        _ => known(error),
    }
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
            return Err(known(
                "content service differs from the original capability",
            ));
        }
        let inner = &self.scope.context.inner;
        let store = inner
            .content
            .as_ref()
            .ok_or_else(|| known("local content storage is not configured"))?;
        let (name, scope, put) = match capability {
            READ => {
                let input: ReadInput = serde_json::from_value(arguments).map_err(known)?;
                (input.name, input.scope, None)
            }
            PUT => {
                let input: PutInput = serde_json::from_value(arguments).map_err(known)?;
                let expected = input.expected()?;
                if input.text.len() as u64 > store.limits().max_content_bytes.min(MAX_TEXT_BYTES) {
                    return Err(known("content text exceeds its UTF-8 byte budget"));
                }
                (input.name, input.scope, Some((expected, input.text)))
            }
            _ => return Err(known("not a content capability")),
        };
        let session = &self.scope.context.lease.session_id;
        let task = (scope == ContentScope::Task).then_some(self.scope.context.task.id.as_str());
        let key = record_name(store.device_id().as_str(), session, task, &name).map_err(known)?;
        loop {
            self.scope.check_authority(&cancel)?;
            check_private(store).map_err(known)?;
            // No await occurs after a lock is acquired until the synchronous
            // transaction returns. Cancellation cannot detach a live commit.
            let result = match &put {
                Some((expected, text)) => store
                    .try_put(&key, *expected, text.as_bytes())
                    .map(|record| Some((record, text.as_bytes().to_vec()))),
                None => store.try_read(&key),
            };
            match result {
                Err(cayw_content::Error::Busy) => tokio::select! { biased;
                    _ = cancel.cancelled() => return Err(known("content operation cancelled before acquiring its transaction lock")),
                    _ = tokio::time::sleep(Duration::from_millis(20)) => {},
                },
                Err(error) => return Err(content_error(error)),
                Ok(value) => {
                    return serde_json::to_value(
                        value
                            .map(|(record, bytes)| view(session, task, &name, record, bytes))
                            .transpose()
                            .map_err(known)?,
                    )
                    .map_err(known);
                }
            }
        }
    }
}
