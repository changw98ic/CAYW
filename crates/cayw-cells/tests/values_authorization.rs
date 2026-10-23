//! Store/load must traverse the original CellBackend call path. Session
//! identity alone is not permission to reuse a value across tasks or turns.
mod support;

use async_trait::async_trait;
use cayw_ai::{CancellationToken, ToolDefinition};
use cayw_cells::*;
use cayw_codemode::{CapabilityError, CapabilityHost, Limits};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use support::{TestCellExec, backend};

struct StrictHost {
    calls: AtomicUsize,
}

impl Default for StrictHost {
    fn default() -> Self {
        Self {
            calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl CapabilityHost for StrictHost {
    fn capabilities(&self) -> Vec<ToolDefinition> {
        Vec::new()
    }

    async fn call(
        &self,
        _: &str,
        _: Value,
        _: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(CapabilityError::new(
            "fixture host does not authorize stored values",
        ))
    }
}

/// A test-only trusted backend. It explicitly decides whether a value
/// operation is allowed, then performs the journal operation itself.
struct ValueBackend {
    journal: Arc<SqliteJournal>,
    limits: CellLimits,
    allow: AtomicBool,
    calls: Mutex<Vec<String>>,
}

impl ValueBackend {
    fn new(journal: Arc<SqliteJournal>, limits: CellLimits) -> Self {
        Self {
            journal,
            limits,
            allow: AtomicBool::new(true),
            calls: Mutex::new(Vec::new()),
        }
    }

    fn deny(&self) {
        self.allow.store(false, Ordering::SeqCst);
    }
}

fn value_definitions() -> Vec<ToolDefinition> {
    ["cells.store", "cells.load"]
        .into_iter()
        .map(|name| ToolDefinition {
            name: name.into(),
            description: "fixture cell value".into(),
            parameters: json!({"type":"object"}),
        })
        .collect()
}

#[async_trait]
impl CellBackend for ValueBackend {
    fn capabilities(&self) -> Vec<ToolDefinition> {
        value_definitions()
    }

    fn created(&self, _: &CellExecution) -> Result<(), CellError> {
        Ok(())
    }

    async fn call(
        &self,
        execution: &CellExecution,
        _: u64,
        name: &str,
        arguments: Value,
        _: CancellationToken,
    ) -> Result<BackendResponse, CapabilityError> {
        self.calls.lock().unwrap().push(name.into());
        if !self.allow.load(Ordering::SeqCst) {
            return Err(CapabilityError::new(
                "stored value reuse requires explicit authorization",
            ));
        }
        match name {
            "cells.store" => {
                let key = arguments["key"]
                    .as_str()
                    .ok_or_else(|| CapabilityError::new("key requires a string"))?;
                let version = arguments["expected_version"]
                    .as_u64()
                    .ok_or_else(|| CapabilityError::new("expected_version requires an integer"))?;
                let value = arguments
                    .get("value")
                    .cloned()
                    .ok_or_else(|| CapabilityError::new("value required"))?;
                let snapshot = self
                    .journal
                    .store(
                        &execution.identity.session_id,
                        key,
                        version,
                        value,
                        &self.limits,
                    )
                    .map_err(|error| CapabilityError::new(error.to_string()))?;
                Ok(BackendResponse::new(Ok(
                    json!({"version": snapshot.version}),
                )))
            }
            "cells.load" => {
                let key = arguments["key"]
                    .as_str()
                    .ok_or_else(|| CapabilityError::new("key requires a string"))?;
                let snapshot = self
                    .journal
                    .load(&execution.identity.session_id, key)
                    .map_err(|error| CapabilityError::new(error.to_string()))?;
                Ok(BackendResponse::new(Ok(
                    serde_json::to_value(snapshot).unwrap_or(Value::Null)
                )))
            }
            _ => Err(CapabilityError::new("unknown fixture value capability")),
        }
    }

    fn finished(&self, _: &CellExecution, _: &CellTerminal) -> Result<(), CellError> {
        Ok(())
    }
}

fn identity(session: &str, task: &str) -> CellIdentity {
    CellIdentity {
        session_id: session.into(),
        task_id: task.into(),
        attempt_id: "attempt".into(),
        permission_context: "principal/policy-v1".into(),
    }
}

fn manager(
    id: CellIdentity,
    journal: Arc<SqliteJournal>,
    backend: Arc<dyn CellBackend>,
    limits: CellLimits,
) -> Arc<CellManager> {
    CellManager::new(
        id,
        journal,
        backend,
        ReadCoordinator::new(vec![], limits.clone()).unwrap(),
        limits,
        Limits {
            timeout: Duration::from_secs(30),
            host_calls: 256,
            ..Limits::default()
        },
    )
    .unwrap()
}

fn request(script: &str) -> ExecRequest {
    ExecRequest {
        script: script.into(),
        yield_time_ms: 10_000,
        max_output_tokens: 128 * 1024,
    }
}

async fn done(manager: &CellManager, script: &str) -> CellPage {
    let mut page = manager
        .exec_fixture(request(script), CancellationToken::new())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !page.status.is_terminal() {
            page = manager
                .wait(
                    WaitRequest {
                        cell_id: page.cell_id,
                        cursor: page.next_cursor,
                        yield_time_ms: 10_000,
                        max_output_tokens: 128 * 1024,
                        continue_execution: false,
                    },
                    CancellationToken::new(),
                )
                .await
                .unwrap();
        }
        page
    })
    .await
    .unwrap()
}

fn call_names(record: &CellRecord) -> Vec<&str> {
    record.calls.iter().map(|call| call.name.as_str()).collect()
}

#[tokio::test]
async fn store_and_load_are_recorded_backend_capabilities() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(SqliteJournal::open(dir.path().join("cells.db")).unwrap());
    let limits = CellLimits::default();
    let backend = Arc::new(ValueBackend::new(journal.clone(), limits.clone()));
    let cells = manager(
        identity("session", "task"),
        journal.clone(),
        backend.clone(),
        limits,
    );
    let page = done(
        &cells,
        "store('state',{count=1},0); local a=load('state'); store('state',{count=2},a.version); return load('state')",
    )
    .await;
    assert_eq!(page.status, CellStatus::Completed, "{page:?}");
    let record = cells.record(&page.cell_id).unwrap();
    assert_eq!(
        call_names(&record),
        ["cells.store", "cells.load", "cells.store", "cells.load"]
    );
    assert!(
        record
            .calls
            .iter()
            .all(|call| call.status == IntentStatus::Succeeded)
    );
    assert_eq!(
        *backend.calls.lock().unwrap(),
        ["cells.store", "cells.load", "cells.store", "cells.load"]
    );
    assert_eq!(
        journal.load("session", "state").unwrap().unwrap().version,
        2
    );
    cells.close().await;
}

#[tokio::test]
async fn values_larger_than_item_bytes_are_not_omitted() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(SqliteJournal::open(dir.path().join("cells.db")).unwrap());
    let limits = CellLimits::default();
    let backend = support::backend_with_values(
        Arc::new(StrictHost::default()),
        journal.clone(),
        limits.clone(),
    );
    let cells = manager(
        identity("session", "task"),
        journal.clone(),
        backend,
        limits,
    );
    let page = done(
        &cells,
        "local value=string.rep('x',102400); store('large',value,0); local loaded=load('large'); text(tostring(#loaded.value))",
    )
    .await;
    assert_eq!(page.status, CellStatus::Completed, "{page:?}");
    let record = cells.record(&page.cell_id).unwrap();
    assert_eq!(call_names(&record), ["cells.store", "cells.load"]);
    assert!(!record.calls[1].result_omitted);
    assert!(record.calls[1].result.is_some());
    assert_eq!(
        journal
            .load("session", "large")
            .unwrap()
            .unwrap()
            .value
            .as_str()
            .unwrap()
            .len(),
        102_400
    );
    assert!(matches!(
        &record.outputs[0].output,
        CellOutput::Text { text } if text == "102400"
    ));
    cells.close().await;
}

#[tokio::test]
async fn cross_task_value_reuse_requires_backend_authorization() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(SqliteJournal::open(dir.path().join("cells.db")).unwrap());
    let limits = CellLimits::default();

    let first_backend = Arc::new(ValueBackend::new(journal.clone(), limits.clone()));
    let first = manager(
        identity("session", "task-a"),
        journal.clone(),
        first_backend.clone(),
        limits.clone(),
    );
    let stored = done(&first, "store('state',{count=1},0)").await;
    assert_eq!(stored.status, CellStatus::Completed, "{stored:?}");
    first.close().await;
    drop(first);

    let denied_backend = Arc::new(ValueBackend::new(journal.clone(), limits.clone()));
    denied_backend.deny();
    let denied = manager(
        identity("session", "task-b"),
        journal.clone(),
        denied_backend.clone(),
        limits.clone(),
    );
    let blocked = done(
        &denied,
        "local value=load('state'); text(tostring(value.value.count))",
    )
    .await;
    assert_eq!(blocked.status, CellStatus::Failed, "{blocked:?}");
    assert!(denied.record(&blocked.cell_id).unwrap().outputs.is_empty());
    assert_eq!(
        denied_backend.calls.lock().unwrap().as_slice(),
        ["cells.load"]
    );
    denied.close().await;
    drop(denied);

    let allowed_backend = Arc::new(ValueBackend::new(journal.clone(), limits.clone()));
    let allowed = manager(
        identity("session", "task-c"),
        journal,
        allowed_backend.clone(),
        limits,
    );
    let reused = done(
        &allowed,
        "local value=load('state'); text(tostring(value.value.count))",
    )
    .await;
    assert_eq!(reused.status, CellStatus::Completed, "{reused:?}");
    assert!(matches!(
        &allowed.record(&reused.cell_id).unwrap().outputs[0].output,
        CellOutput::Text { text } if text == "1"
    ));
    allowed.close().await;
}

struct DenyValues {
    calls: AtomicUsize,
}

#[async_trait]
impl CellValueAuthorizer for DenyValues {
    async fn authorize(
        &self,
        _: &CellExecution,
        _: CellValueOperation,
        _: &Value,
        _: CancellationToken,
    ) -> Result<(), CapabilityError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(CapabilityError::new("fixture policy denied stored values"))
    }
}

#[tokio::test]
async fn explicit_authorizer_denies_before_the_executor() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(SqliteJournal::open(dir.path().join("cells.db")).unwrap());
    let limits = CellLimits::default();
    let authorizer = Arc::new(DenyValues {
        calls: AtomicUsize::new(0),
    });
    let backend = Arc::new(AuthorizedCellBackend::new(
        backend(Arc::new(StrictHost::default())),
        CellValueExecutor::new(journal.clone(), limits.clone()),
        authorizer.clone(),
    ));
    let cells = manager(
        identity("session", "task"),
        journal.clone(),
        backend,
        limits,
    );
    let page = done(&cells, "store('state',1,0)").await;
    assert_eq!(page.status, CellStatus::Failed, "{page:?}");
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 1);
    assert!(journal.load("session", "state").unwrap().is_none());
    cells.close().await;
}

#[tokio::test]
async fn unhandled_backend_defaults_to_deny() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(SqliteJournal::open(dir.path().join("cells.db")).unwrap());
    let limits = CellLimits::default();
    let host = Arc::new(StrictHost::default());
    let cells = manager(
        identity("session", "task"),
        journal.clone(),
        backend(host.clone()),
        limits,
    );
    let page = done(&cells, "store('state',1,0)").await;
    assert_eq!(page.status, CellStatus::Failed, "{page:?}");
    assert_eq!(host.calls.load(Ordering::SeqCst), 1);
    assert!(journal.load("session", "state").unwrap().is_none());
    cells.close().await;
}

#[tokio::test]
async fn catalog_exposes_canonical_value_definitions() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(SqliteJournal::open(dir.path().join("cells.db")).unwrap());
    let limits = CellLimits::default();
    let host = Arc::new(StrictHost::default());
    let cells = manager(identity("session", "task"), journal, backend(host), limits);
    let page = done(
        &cells,
        "local store_seen=false; local load_seen=false;
         for _, name in ALL_TOOLS do
           if name=='cells.store' then store_seen=true end
           if name=='cells.load' then load_seen=true end
         end
         local definition=toolDefinition('cells.store');
         text(tostring(store_seen and load_seen and definition.name=='cells.store' and definition.parameters.additionalProperties==false))",
    )
    .await;
    assert_eq!(page.status, CellStatus::Completed, "{page:?}");
    assert!(matches!(
        &cells.record(&page.cell_id).unwrap().outputs[0].output,
        CellOutput::Text { text } if text == "true"
    ));
    cells.close().await;
}

#[tokio::test]
async fn value_arguments_reject_extra_fields_and_session_overrides() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(SqliteJournal::open(dir.path().join("cells.db")).unwrap());
    let limits = CellLimits::default();
    let backend = Arc::new(ValueBackend::new(journal.clone(), limits.clone()));
    let cells = manager(identity("session", "task"), journal, backend, limits);
    for script in [
        "cayw.call('cells.store',{key='state',value=1,expected_version=0,extra=true})",
        "cayw.call('cells.load',{key='state',session_id='other'})",
        "cayw.call('cells.store',{key='state',value=1,expected_version=-1})",
    ] {
        let page = done(&cells, script).await;
        assert_eq!(page.status, CellStatus::Failed, "{script}: {page:?}");
    }
    cells.close().await;
}
