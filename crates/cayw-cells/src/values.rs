use crate::{
    BackendResponse, CellBackend, CellError, CellExecution, CellLimits, CellTerminal,
    SqliteJournal, StoreSnapshot, discard_json, validate_json,
};
use async_trait::async_trait;
use cayw_ai::{CancellationToken, ToolDefinition};
use cayw_codemode::{CapabilityError, CapabilityStop};
use serde_json::{Value, json};
use std::sync::Arc;

/// The canonical capability names used by the trusted `store` and `load`
/// helpers. A host cannot rename these operations and still receive the
/// session-scoped executor behavior.
pub const CELL_STORE: &str = "cells.store";
pub const CELL_LOAD: &str = "cells.load";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CellValueOperation {
    Store,
    Load,
}

impl CellValueOperation {
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            CELL_STORE => Some(Self::Store),
            CELL_LOAD => Some(Self::Load),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Store => CELL_STORE,
            Self::Load => CELL_LOAD,
        }
    }
}

/// Canonical model-visible definitions. Bridge merges these definitions into
/// the captured host catalog so identity collision cannot downgrade the
/// argument contract. They describe capabilities; they do not grant access.
pub fn definitions() -> Vec<ToolDefinition> {
    let key = json!({"type":"string","minLength":1,"maxLength":128});
    vec![
        ToolDefinition {
            name: CELL_STORE.into(),
            description: "Persist one bounded JSON value for the current session using an exact compare-and-set version. Version 0 creates a new key. The trusted backend must authorize the operation before it executes.".into(),
            parameters: json!({
                "type":"object",
                "properties":{
                    "key":key.clone(),
                    "value":{},
                    "expected_version":{"type":"integer","minimum":0}
                },
                "required":["key","value","expected_version"],
                "additionalProperties":false
            }),
        },
        ToolDefinition {
            name: CELL_LOAD.into(),
            description: "Load one session-scoped JSON value and its current version. The trusted backend must authorize the read before it executes; matching session identity alone is not authorization.".into(),
            parameters: json!({
                "type":"object",
                "properties":{"key":key},
                "required":["key"],
                "additionalProperties":false
            }),
        },
    ]
}

/// Merge the canonical value definitions into a captured catalog. Existing
/// definitions with the same names are replaced so a host cannot weaken the
/// store/load schemas.
pub(crate) fn with_definitions(mut catalog: Vec<ToolDefinition>) -> Vec<ToolDefinition> {
    for definition in definitions() {
        if let Some(existing) = catalog
            .iter_mut()
            .find(|existing| existing.name == definition.name)
        {
            *existing = definition;
        } else {
            catalog.push(definition);
        }
    }
    catalog
}

/// Trusted, session-scoped value executor. It never accepts a session argument:
/// the session comes only from the immutable [`CellExecution`] origin. The
/// caller must perform authorization before invoking it; the executor only
/// enforces structural limits and the journal contract.
#[derive(Clone)]
pub struct CellValueExecutor {
    journal: Arc<SqliteJournal>,
    limits: CellLimits,
}

impl CellValueExecutor {
    pub fn new(journal: Arc<SqliteJournal>, limits: CellLimits) -> Self {
        Self { journal, limits }
    }

    /// Execute one canonical value capability. `name` must be `cells.store` or
    /// `cells.load`; arguments are strictly checked and no extra fields (for
    /// example `session_id`) are accepted.
    pub fn invoke(
        &self,
        execution: &CellExecution,
        name: &str,
        arguments: Value,
    ) -> Result<Value, CapabilityError> {
        let operation = CellValueOperation::from_name(name)
            .ok_or_else(|| CapabilityError::new("not a cell value capability"))?;
        execution.identity.validate().map_err(capability_error)?;
        self.limits.validate().map_err(capability_error)?;
        let session = &execution.identity.session_id;
        if self.journal.is_frozen(session).map_err(capability_error)? {
            return Err(capability_error(CellError::Frozen));
        }
        let (depth, bytes) = match operation {
            // The store helper passes one object around the value. Count that
            // fixed envelope separately so a value accepted at json_depth can
            // still reach the journal.
            CellValueOperation::Store => (
                self.limits.json_depth.saturating_add(1),
                self.limits.store_bytes.saturating_add(256),
            ),
            CellValueOperation::Load => (self.limits.json_depth, self.limits.item_bytes),
        };
        if let Err(error) = validate_json(&arguments, depth, bytes) {
            discard_json(arguments);
            return Err(capability_error(error));
        }
        match operation {
            CellValueOperation::Store => self.store(session, arguments),
            CellValueOperation::Load => self.load(session, arguments),
        }
    }

    fn store(&self, session: &str, arguments: Value) -> Result<Value, CapabilityError> {
        let object = arguments
            .as_object()
            .ok_or_else(|| CapabilityError::new("cells.store arguments require an object"))?;
        reject_extra_fields(object, &["key", "value", "expected_version"])?;
        let key = stored_key(object)?;
        let expected_version = object
            .get("expected_version")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                CapabilityError::new("cells.store expected_version requires an unsigned integer")
            })?;
        let value = object
            .get("value")
            .cloned()
            .ok_or_else(|| CapabilityError::new("cells.store value required"))?;
        // A failed commit can still have written the value. Report that case as
        // unknown so Bridge freezes the session instead of replaying the write.
        self.journal
            .store(session, key, expected_version, value, &self.limits)
            .map(|snapshot| json!({"version":snapshot.version}))
            .map_err(capability_error)
    }

    fn load(&self, session: &str, arguments: Value) -> Result<Value, CapabilityError> {
        let object = arguments
            .as_object()
            .ok_or_else(|| CapabilityError::new("cells.load arguments require an object"))?;
        reject_extra_fields(object, &["key"])?;
        let key = stored_key(object)?;
        let snapshot = self.journal.load(session, key).map_err(capability_error)?;
        let Some(snapshot) = snapshot else {
            return Ok(Value::Null);
        };
        validate_snapshot(snapshot, &self.limits)
    }
}

/// Explicit authorization for independent Cells backends. Implementations
/// receive the immutable cell origin and the exact value operation/arguments.
/// Returning `CapabilityError::unknown` means the authorization result itself
/// is uncertain; Bridge then freezes the session.
#[async_trait]
pub trait CellValueAuthorizer: Send + Sync {
    async fn authorize(
        &self,
        execution: &CellExecution,
        operation: CellValueOperation,
        arguments: &Value,
        cancel: CancellationToken,
    ) -> Result<(), CapabilityError>;
}

/// A CellBackend decorator that serves only the canonical value capabilities
/// and forwards every other capability to the original backend. Construction
/// requires an explicit authorizer, so an unwrapped backend remains the only
/// path and no value is allowed merely because its session id matches.
pub struct AuthorizedCellBackend {
    inner: Arc<dyn CellBackend>,
    executor: CellValueExecutor,
    authorizer: Arc<dyn CellValueAuthorizer>,
}

impl AuthorizedCellBackend {
    pub fn new(
        inner: Arc<dyn CellBackend>,
        executor: CellValueExecutor,
        authorizer: Arc<dyn CellValueAuthorizer>,
    ) -> Self {
        Self {
            inner,
            executor,
            authorizer,
        }
    }
}

#[async_trait]
impl CellBackend for AuthorizedCellBackend {
    fn capabilities(&self) -> Vec<ToolDefinition> {
        // The decorator serves the canonical value capabilities in addition to
        // forwarding every other capability to the inner backend.
        with_definitions(self.inner.capabilities())
    }

    fn created(&self, execution: &CellExecution) -> Result<(), CellError> {
        self.inner.created(execution)
    }

    async fn call(
        &self,
        execution: &CellExecution,
        sequence: u64,
        name: &str,
        arguments: Value,
        cancel: CancellationToken,
    ) -> Result<BackendResponse, CapabilityError> {
        let Some(operation) = CellValueOperation::from_name(name) else {
            return self
                .inner
                .call(execution, sequence, name, arguments, cancel)
                .await;
        };
        if cancel.is_cancelled() {
            return Ok(BackendResponse::new(Err(CapabilityError::stopped(
                CapabilityStop::Cancelled,
                "cell value operation cancelled before authorization",
            ))));
        }
        self.authorizer
            .authorize(execution, operation, &arguments, cancel.clone())
            .await?;
        if cancel.is_cancelled() {
            return Ok(BackendResponse::new(Err(CapabilityError::stopped(
                CapabilityStop::Cancelled,
                "cell value operation cancelled before execution",
            ))));
        }
        Ok(BackendResponse::new(
            self.executor.invoke(execution, name, arguments),
        ))
    }

    fn finished(
        &self,
        execution: &CellExecution,
        terminal: &CellTerminal,
    ) -> Result<(), CellError> {
        self.inner.finished(execution, terminal)
    }
}

/// Convenience constructor for the explicit decorator.
pub fn authorized_cell_backend(
    inner: Arc<dyn CellBackend>,
    executor: CellValueExecutor,
    authorizer: Arc<dyn CellValueAuthorizer>,
) -> Arc<dyn CellBackend> {
    Arc::new(AuthorizedCellBackend::new(inner, executor, authorizer))
}

fn reject_extra_fields(
    object: &serde_json::Map<String, Value>,
    allowed: &[&str],
) -> Result<(), CapabilityError> {
    if object
        .keys()
        .any(|key| !allowed.iter().any(|allowed| allowed == key))
    {
        return Err(CapabilityError::new(
            "cell value arguments contain an unknown field",
        ));
    }
    Ok(())
}

fn stored_key(object: &serde_json::Map<String, Value>) -> Result<&str, CapabilityError> {
    object
        .get("key")
        .and_then(Value::as_str)
        .ok_or_else(|| CapabilityError::new("cell value key requires a string"))
}

fn validate_snapshot(
    snapshot: StoreSnapshot,
    limits: &CellLimits,
) -> Result<Value, CapabilityError> {
    if let Err(error) = validate_json(&snapshot.value, limits.json_depth, limits.store_bytes) {
        discard_json(snapshot.value);
        return Err(capability_error(error));
    }
    let value = serde_json::to_value(snapshot).map_err(|error| capability_error(error.into()))?;
    if let Err(error) = validate_json(
        &value,
        limits.json_depth.saturating_add(1),
        limits.store_bytes.saturating_add(64),
    ) {
        discard_json(value);
        return Err(capability_error(error));
    }
    Ok(value)
}

fn capability_error(error: CellError) -> CapabilityError {
    error.capability_error()
}
