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
use support::backend;
use tokio::sync::Semaphore;

struct Host {
    writes: AtomicUsize,
    entered: Semaphore,
    release: Semaphore,
    observed: AtomicBool,
    settled: AtomicBool,
}
impl Default for Host {
    fn default() -> Self {
        Self {
            writes: AtomicUsize::new(0),
            entered: Semaphore::new(0),
            release: Semaphore::new(0),
            observed: AtomicBool::new(false),
            settled: AtomicBool::new(false),
        }
    }
}
#[async_trait]
impl CapabilityHost for Host {
    fn capabilities(&self) -> Vec<ToolDefinition> {
        ["write", "cooperative", "pending", "hold", "yield"]
            .into_iter()
            .map(|name| ToolDefinition {
                name: name.into(),
                description: String::new(),
                parameters: json!({"type":"object"}),
            })
            .collect()
    }
    async fn call(
        &self,
        name: &str,
        args: Value,
        cancel: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        match name {
            "write" => {
                self.writes.fetch_add(1, Ordering::SeqCst);
                Ok(json!("written"))
            }
            "cooperative" => {
                self.entered.add_permits(1);
                cancel.cancelled().await;
                self.observed.store(true, Ordering::SeqCst);
                tokio::task::yield_now().await;
                self.settled.store(true, Ordering::SeqCst);
                match args["reply"].as_str() {
                    Some("error") => Err(CapabilityError::new("known failed")),
                    Some("unknown") => Err(CapabilityError::unknown("lost receipt")),
                    _ => Ok(json!({"settled":true})),
                }
            }
            "hold" => {
                self.entered.add_permits(1);
                self.release.acquire().await.unwrap().forget();
                Ok(args)
            }
            "pending" | "yield" => {
                self.entered.add_permits(1);
                cancel.cancelled().await;
                self.observed.store(true, Ordering::SeqCst);
                std::future::pending().await
            }
            _ => unreachable!(),
        }
    }
}
fn identity() -> CellIdentity {
    CellIdentity {
        session_id: "s".into(),
        task_id: "t".into(),
        attempt_id: "a".into(),
        permission_context: "policy-v1".into(),
    }
}
fn request(script: &str) -> ExecRequest {
    ExecRequest {
        script: script.into(),
        yield_time_ms: 1000,
        max_output_tokens: 128 * 1024,
    }
}
fn wait(page: &CellPage, resume: bool) -> WaitRequest {
    WaitRequest {
        cell_id: page.cell_id.clone(),
        cursor: page.next_cursor,
        yield_time_ms: 1000,
        max_output_tokens: 128 * 1024,
        continue_execution: resume,
    }
}
async fn done(manager: &CellManager, mut page: CellPage) -> CellPage {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !page.status.is_terminal() {
            page = manager
                .wait(wait(&page, false), CancellationToken::new())
                .await
                .unwrap();
        }
        page
    })
    .await
    .unwrap()
}
fn setup(
    limits: CellLimits,
    vm: Limits,
) -> (
    tempfile::TempDir,
    Arc<SqliteJournal>,
    Arc<Host>,
    Arc<CellManager>,
) {
    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(SqliteJournal::open(dir.path().join("cells.db")).unwrap());
    let host = Arc::new(Host::default());
    let manager = CellManager::new(
        identity(),
        journal.clone(),
        backend(host.clone()),
        ReadCoordinator::new(vec![], limits.clone()).unwrap(),
        limits,
        vm,
    )
    .unwrap();
    (dir, journal, host, manager)
}

// Break caught: scripts can reach the bootstrap/raw API or create writable globals.
#[tokio::test]
async fn helpers_raw_catalog_and_fresh_global_spoofs_cannot_start_effects() {
    let (_dir, journal, host, manager) = setup(CellLimits::default(), Limits::default());
    for script in [
        "new_global=42; new_global=43; cayw.call('write',{})",
        "_G.new_global=42; cayw.call('write',{})",
        "rawset(_G,'new_global',42); cayw.call('write',{})",
        "__cells_call('__cells_store',{key='bypass',value=1,expected_version=0}); cayw.call('write',{})",
        "_G.cayw.describe('__cells_store'); cayw.call('write',{})",
        "_G.cayw.call('__cells_store',{key='bypass',value=1,expected_version=0}); cayw.call('write',{})",
        "cayw.call('__cells_yield',{}); cayw.call('write',{})",
        "ALL_TOOLS[1]='fake'; cayw.call('write',{})",
        "yield_control=function() end; cayw.call('write',{})",
    ] {
        let page = done(
            &manager,
            manager
                .exec(request(script), "trusted-parent", CancellationToken::new())
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(page.status, CellStatus::Failed, "{script}: {page:?}");
    }
    assert_eq!(host.writes.load(Ordering::SeqCst), 0);
    assert!(journal.load("s", "bypass").unwrap().is_none());
    let page=done(&manager,manager.exec(request("assert(_G.cayw==cayw); assert(#ALL_TOOLS==5); assert(#cayw.discover()==5); for _,name in ALL_TOOLS do assert(string.sub(name,1,8)~='__cells_') end; assert(not pcall and not xpcall and not coroutine and not debug and type(load)=='function' and not io)"),"trusted-parent",CancellationToken::new()).await.unwrap()).await;
    assert_eq!(
        page.status,
        CellStatus::Completed,
        "{:?}",
        manager.record(&page.cell_id).unwrap()
    );
    assert!(manager.record(&page.cell_id).unwrap().calls.is_empty());
    manager.close().await;
}

// Break caught: cayw.emit publishes only at terminal and overflows after a write.
#[tokio::test]
async fn every_emit_is_durable_at_yield_and_overflow_stops_the_next_write() {
    let (_dir, _journal, host, manager) = setup(
        CellLimits {
            output_items: 2,
            ..CellLimits::default()
        },
        Limits::default(),
    );
    let mut page=manager.exec(request("_G.cayw.emit('before'); yield_control(); _G.cayw.emit('second'); _G.cayw.emit('overflow'); cayw.call('write',{})"),"p",CancellationToken::new()).await.unwrap();
    while page.status != CellStatus::Yielded {
        page = manager
            .wait(wait(&page, false), CancellationToken::new())
            .await
            .unwrap();
    }
    let record = manager.record(&page.cell_id).unwrap();
    assert_eq!(record.outputs.len(), 1);
    assert!(matches!(&record.outputs[0].output,CellOutput::Json{value} if value=="before"));
    page = manager
        .wait(wait(&page, true), CancellationToken::new())
        .await
        .unwrap();
    let page = done(&manager, page).await;
    assert_eq!(page.status, CellStatus::Failed);
    assert_eq!(manager.record(&page.cell_id).unwrap().outputs.len(), 2);
    assert_eq!(host.writes.load(Ordering::SeqCst), 0);
    manager.close().await;
}

#[tokio::test]
async fn raw_emit_item_and_total_byte_limits_stop_following_effects() {
    for (limits, script) in [
        (
            CellLimits {
                item_bytes: 100,
                ..CellLimits::default()
            },
            "_G.cayw.emit(string.rep('x',200)); cayw.call('write',{})",
        ),
        (
            CellLimits {
                output_bytes: 70,
                ..CellLimits::default()
            },
            "cayw.emit('first'); _G.cayw.emit(string.rep('x',60)); cayw.call('write',{})",
        ),
    ] {
        let (_dir, _journal, host, manager) = setup(limits, Limits::default());
        let page = done(
            &manager,
            manager
                .exec(request(script), "p", CancellationToken::new())
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(page.status, CellStatus::Failed);
        assert_eq!(host.writes.load(Ordering::SeqCst), 0);
        manager.close().await;
    }
}

// Break caught: load counts the fixed snapshot envelope against logical value depth.
#[tokio::test]
async fn store_load_round_trip_at_small_and_default_depth_boundaries() {
    for depth in [4, 32] {
        let (_dir, journal, _host, manager) = setup(
            CellLimits {
                json_depth: depth,
                ..CellLimits::default()
            },
            Limits::default(),
        );
        let script = format!(
            "local value=1; for i=1,{} do value={{a=value}} end; store('deep',value,0); local loaded=load('deep').value; for i=1,{} do loaded=loaded.a end; text(tostring(loaded)); store('null',cayw.null,0); assert(load('null').value==cayw.null)",
            depth - 1,
            depth - 1
        );
        let page = done(
            &manager,
            manager
                .exec(request(&script), "p", CancellationToken::new())
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(page.status, CellStatus::Completed, "{depth}: {page:?}");
        assert!(journal.load("s", "deep").unwrap().is_some());
        let script = format!(
            "local value=1; for i=1,{depth} do value={{a=value}} end; store('over',value,0)"
        );
        let page = done(
            &manager,
            manager
                .exec(request(&script), "p", CancellationToken::new())
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(page.status, CellStatus::Failed);
        assert!(journal.load("s", "over").unwrap().is_none());
        manager.close().await;
    }
}

// Break caught: cancel/close abort before observing token and one async cleanup yield.
#[tokio::test]
async fn cancel_and_close_preserve_actual_host_settlement_before_terminal() {
    for close in [false, true] {
        for (reply, status, unknown) in [
            ("success", IntentStatus::Succeeded, false),
            ("error", IntentStatus::Failed, false),
            ("unknown", IntentStatus::OutcomeUnknown, true),
        ] {
            let (_dir, journal, host, manager) = setup(CellLimits::default(), Limits::default());
            let mut req = request(&format!(
                "cayw.call('cooperative',{{reply='{reply}'}}); cayw.call('write',{{}})"
            ));
            req.yield_time_ms = 1;
            let page = manager
                .exec(req, "origin", CancellationToken::new())
                .await
                .unwrap();
            host.entered.acquire().await.unwrap().forget();
            let page = if close {
                manager.close().await;
                manager
                    .wait(wait(&page, false), CancellationToken::new())
                    .await
                    .unwrap()
            } else {
                manager.cancel(&page.cell_id).await.unwrap()
            };
            assert!(host.observed.load(Ordering::SeqCst));
            assert!(host.settled.load(Ordering::SeqCst));
            assert!(!page.vm_alive);
            assert_eq!(page.outcome_unknown, unknown);
            let record = manager.record(&page.cell_id).unwrap();
            assert_eq!(record.calls[0].status, status);
            assert_eq!(record.calls.len(), 1);
            assert_eq!(journal.is_frozen("s").unwrap(), unknown);
            assert_eq!(host.writes.load(Ordering::SeqCst), 0);
            if status == IntentStatus::Succeeded {
                assert_eq!(record.calls[0].result, Some(json!({"settled":true})));
            }
            manager.close().await;
        }
    }
}

#[tokio::test]
async fn pending_host_deadline_settles_unknown_and_yield_cancel_is_known() {
    let (_dir, journal, host, manager) = setup(
        CellLimits::default(),
        Limits {
            shutdown_grace: Duration::from_millis(30),
            ..Limits::default()
        },
    );
    let mut req = request("cayw.call('pending',{})");
    req.yield_time_ms = 1;
    let page = manager
        .exec(req, "p", CancellationToken::new())
        .await
        .unwrap();
    host.entered.acquire().await.unwrap().forget();
    let page = tokio::time::timeout(Duration::from_secs(2), manager.cancel(&page.cell_id))
        .await
        .unwrap()
        .unwrap();
    assert!(host.observed.load(Ordering::SeqCst));
    assert_eq!(page.status, CellStatus::OutcomeUnknown);
    assert!(!page.vm_alive);
    assert!(journal.is_frozen("s").unwrap());
    manager.close().await;
    let (_dir, _journal, _host, manager) = setup(CellLimits::default(), Limits::default());
    let page = manager
        .exec(
            request("yield_control(); cayw.call('write',{})"),
            "p",
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(page.status, CellStatus::Yielded);
    let page = manager.cancel(&page.cell_id).await.unwrap();
    assert_eq!(page.status, CellStatus::Cancelled);
    assert!(!page.outcome_unknown);
    manager.close().await;
}

// Break caught: suspended wait consumes the VM budget, or resume resets CPU/host budgets.
#[tokio::test]
async fn controlled_yield_pauses_active_time_but_keeps_other_budgets() {
    let (_dir, _journal, host, manager) = setup(
        CellLimits::default(),
        Limits {
            timeout: Duration::from_millis(100),
            host_calls: 1,
            instruction_budget: u64::MAX,
            ..Limits::default()
        },
    );
    let mut page=manager.exec(request("local kept=7; yield_control(); assert(kept==7); cayw.call('write',{}); yield_control(); cayw.call('write',{})"),"p",CancellationToken::new()).await.unwrap();
    assert_eq!(page.status, CellStatus::Yielded);
    tokio::time::sleep(Duration::from_millis(200)).await;
    page = manager
        .wait(wait(&page, true), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(page.status, CellStatus::Yielded, "{page:?}");
    assert_eq!(host.writes.load(Ordering::SeqCst), 1);
    page = manager
        .wait(wait(&page, true), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(done(&manager, page).await.status, CellStatus::Failed);
    assert_eq!(host.writes.load(Ordering::SeqCst), 1);
    let page = manager
        .exec(
            request("yield_control(); while true do end; cayw.call('write',{})"),
            "p",
            CancellationToken::new(),
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let page = manager
        .wait(wait(&page, true), CancellationToken::new())
        .await
        .unwrap();
    let page = done(&manager, page).await;
    assert_eq!(page.status, CellStatus::Failed);
    assert!(
        manager
            .record(&page.cell_id)
            .unwrap()
            .error
            .unwrap()
            .contains("Timeout")
    );
    manager.close().await;
}

#[tokio::test]
async fn host_named_yield_still_consumes_absolute_call_deadline() {
    let (_dir, _journal, host, manager) = setup(
        CellLimits::default(),
        Limits {
            timeout: Duration::from_millis(40),
            shutdown_grace: Duration::from_millis(20),
            ..Limits::default()
        },
    );
    let page = done(
        &manager,
        manager
            .exec(
                request("cayw.call('yield',{})"),
                "p",
                CancellationToken::new(),
            )
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(page.status, CellStatus::OutcomeUnknown);
    assert!(host.observed.load(Ordering::SeqCst));
    manager.close().await;
}

#[tokio::test]
async fn close_deadline_records_pending_unknown_without_detaching_work() {
    let (_dir, journal, host, manager) = setup(
        CellLimits::default(),
        Limits {
            shutdown_grace: Duration::from_millis(30),
            ..Limits::default()
        },
    );
    let mut req = request("cayw.call('pending',{}); cayw.call('write',{})");
    req.yield_time_ms = 1;
    let page = manager
        .exec(req, "p", CancellationToken::new())
        .await
        .unwrap();
    host.entered.acquire().await.unwrap().forget();
    let closed = tokio::time::timeout(Duration::from_secs(2), manager.close())
        .await
        .unwrap();
    assert!(closed.outcome_unknown);
    assert_eq!(closed.cells.len(), 1);
    assert_eq!(closed.cells[0].execution.cell_id, page.cell_id);
    assert_eq!(closed.cells[0].terminal.status, CellStatus::OutcomeUnknown);
    let page = manager
        .wait(wait(&page, false), CancellationToken::new())
        .await
        .unwrap();
    assert!(!page.vm_alive);
    assert_eq!(page.status, CellStatus::OutcomeUnknown);
    assert!(host.observed.load(Ordering::SeqCst));
    assert!(journal.is_frozen("s").unwrap());
    assert_eq!(host.writes.load(Ordering::SeqCst), 0);
}

// The helper store has its own logical byte cap; the fixed receipt fits in the
// VM's shared payload budget without secretly reducing that logical store cap.
#[tokio::test]
async fn store_load_logical_byte_boundary_and_receipt_budget_are_separate() {
    let (_dir, journal, _host, manager) = setup(
        CellLimits {
            store_bytes: 64,
            ..CellLimits::default()
        },
        Limits::default(),
    );
    let page = done(
        &manager,
        manager
            .exec(
                request("store('fit',string.rep('x',62),0); text(tostring(#load('fit').value))"),
                "p",
                CancellationToken::new(),
            )
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(page.status, CellStatus::Completed);
    assert_eq!(
        journal.load("s", "fit").unwrap().unwrap().value,
        json!("x".repeat(62))
    );
    let page = done(
        &manager,
        manager
            .exec(
                request("store('over',string.rep('x',63),0)"),
                "p",
                CancellationToken::new(),
            )
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(page.status, CellStatus::Failed);
    assert!(journal.load("s", "over").unwrap().is_none());
    manager.close().await;
    let (_dir, journal, _host, manager) = setup(
        CellLimits {
            store_bytes: 64,
            ..CellLimits::default()
        },
        Limits {
            output_bytes: 90,
            ..Limits::default()
        },
    );
    journal
        .store("s", "fit", 0, json!("x".repeat(62)), manager.limits())
        .unwrap();
    let page = done(
        &manager,
        manager
            .exec(request("load('fit')"), "p", CancellationToken::new())
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(page.status, CellStatus::Failed);
    assert!(
        manager
            .record(&page.cell_id)
            .unwrap()
            .error
            .unwrap()
            .contains("OutputLimit")
    );
    assert_eq!(journal.load("s", "fit").unwrap().unwrap().version, 1);
    manager.close().await;
}

#[tokio::test]
async fn controlled_yield_keeps_output_instruction_and_memory_budgets() {
    for (vm, source) in [
        (
            Limits {
                instruction_budget: 1000,
                ..Limits::default()
            },
            "local n=0; for round=1,5 do for i=1,300 do n+=i end; yield_control() end; cayw.call('write',{})",
        ),
        (
            Limits {
                output_bytes: 400,
                ..Limits::default()
            },
            "cayw.emit(string.rep('x',250)); yield_control(); cayw.emit(string.rep('x',250)); cayw.call('write',{})",
        ),
        (
            Limits {
                memory_bytes: 2 * 1024 * 1024,
                ..Limits::default()
            },
            "local keep=string.rep('x',500000); yield_control(); local extra=string.rep('y',1800000); cayw.call('write',{})",
        ),
    ] {
        let (_dir, _journal, host, manager) = setup(CellLimits::default(), vm);
        let mut page = manager
            .exec(request(source), "p", CancellationToken::new())
            .await
            .unwrap();
        assert!(
            !page.status.is_terminal(),
            "expected at least one live yield: {page:?}"
        );
        for _ in 0..5 {
            if page.status.is_terminal() {
                break;
            }
            page = manager
                .wait(wait(&page, true), CancellationToken::new())
                .await
                .unwrap();
        }
        let page = done(&manager, page).await;
        assert_eq!(page.status, CellStatus::Failed, "{source}: {page:?}");
        assert_eq!(host.writes.load(Ordering::SeqCst), 0);
        manager.close().await;
    }
}

struct Hooks {
    journal: Arc<SqliteJournal>,
    host: Arc<Host>,
    fail_created: bool,
    fail_finished: bool,
    completed: AtomicUsize,
}
#[async_trait]
impl CellBackend for Hooks {
    fn capabilities(&self) -> Vec<ToolDefinition> {
        self.host.capabilities()
    }
    fn created(&self, e: &CellExecution) -> Result<(), CellError> {
        let records = self.journal.records(&e.identity, &CellLimits::default())?;
        let record = records
            .iter()
            .find(|r| r.cell_id == e.cell_id)
            .expect("journal intent precedes hook");
        assert_eq!(record.status, CellStatus::NotStarted);
        assert_eq!(record.execution(), *e);
        if self.fail_created {
            Err(CellError::Journal("created record unavailable".into()))
        } else {
            Ok(())
        }
    }
    async fn call(
        &self,
        e: &CellExecution,
        sequence: u64,
        name: &str,
        args: Value,
        cancel: CancellationToken,
    ) -> Result<BackendResponse, CapabilityError> {
        let records = self
            .journal
            .records(&e.identity, &CellLimits::default())
            .unwrap();
        let record = records.iter().find(|r| r.cell_id == e.cell_id).unwrap();
        assert_eq!(
            record.calls[sequence as usize - 1].status,
            IntentStatus::Pending
        );
        assert_eq!(record.execution(), *e);
        Ok(BackendResponse::new(
            self.host.call(name, args, cancel).await,
        ))
    }
    fn finished(&self, e: &CellExecution, t: &CellTerminal) -> Result<(), CellError> {
        let records = self.journal.records(&e.identity, &CellLimits::default())?;
        let record = records.iter().find(|r| r.cell_id == e.cell_id).unwrap();
        assert!(record.status.is_terminal());
        assert_eq!(record.status, t.status);
        self.completed.fetch_add(1, Ordering::SeqCst);
        if self.fail_finished {
            Err(CellError::Journal("terminal record unavailable".into()))
        } else {
            Ok(())
        }
    }
}
#[tokio::test]
async fn backend_lifecycle_is_ordered_and_failures_stay_frozen() {
    for (fail_created, fail_finished) in [(true, false), (false, true), (false, false)] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        let limits = CellLimits::default();
        let journal = Arc::new(SqliteJournal::open(&path).unwrap());
        let backend = Arc::new(Hooks {
            journal: journal.clone(),
            host: Arc::new(Host::default()),
            fail_created,
            fail_finished,
            completed: AtomicUsize::new(0),
        });
        let manager = CellManager::new(
            identity(),
            journal.clone(),
            backend.clone(),
            ReadCoordinator::new(vec![], limits.clone()).unwrap(),
            limits.clone(),
            Limits::default(),
        )
        .unwrap();
        let result = manager
            .exec(
                request("cayw.call('write',{})"),
                "p",
                CancellationToken::new(),
            )
            .await;
        if fail_created {
            assert!(result.is_err());
            assert_eq!(backend.host.writes.load(Ordering::SeqCst), 0);
        } else {
            let page = done(&manager, result.unwrap()).await;
            assert_eq!(
                page.status,
                if fail_finished {
                    CellStatus::OutcomeUnknown
                } else {
                    CellStatus::Completed
                }
            );
            assert!(!page.vm_alive);
            assert_eq!(backend.completed.load(Ordering::SeqCst), 1);
        }
        assert_eq!(
            journal.is_frozen("s").unwrap(),
            fail_created || fail_finished
        );
        if fail_created || fail_finished {
            assert!(matches!(
                manager
                    .exec(request("text('blocked')"), "p", CancellationToken::new())
                    .await,
                Err(CellError::Frozen)
            ));
        }
        manager.close().await;
        drop(manager);
        let reopened = SqliteJournal::open(path).unwrap();
        assert_eq!(
            reopened.is_frozen("s").unwrap(),
            fail_created || fail_finished
        );
    }
}

struct Origins {
    host: Arc<Host>,
    created: Mutex<Vec<CellExecution>>,
    calls: Mutex<Vec<(CellExecution, u64)>>,
    finished: Mutex<Vec<(CellExecution, CellTerminal)>>,
}
#[async_trait]
impl CellBackend for Origins {
    fn capabilities(&self) -> Vec<ToolDefinition> {
        self.host.capabilities()
    }
    fn created(&self, e: &CellExecution) -> Result<(), CellError> {
        self.created.lock().unwrap().push(e.clone());
        Ok(())
    }
    async fn call(
        &self,
        e: &CellExecution,
        sequence: u64,
        name: &str,
        args: Value,
        cancel: CancellationToken,
    ) -> Result<BackendResponse, CapabilityError> {
        self.calls.lock().unwrap().push((e.clone(), sequence));
        Ok(BackendResponse::new(
            self.host.call(name, args, cancel).await,
        ))
    }
    fn finished(&self, e: &CellExecution, t: &CellTerminal) -> Result<(), CellError> {
        self.finished.lock().unwrap().push((e.clone(), t.clone()));
        Ok(())
    }
}
#[tokio::test]
async fn concurrent_cells_keep_immutable_origins_when_they_settle_in_reverse() {
    let dir = tempfile::tempdir().unwrap();
    let limits = CellLimits::default();
    let backend = Arc::new(Origins {
        host: Arc::new(Host::default()),
        created: Mutex::default(),
        calls: Mutex::default(),
        finished: Mutex::default(),
    });
    let manager = CellManager::new(
        identity(),
        Arc::new(SqliteJournal::open(dir.path().join("db")).unwrap()),
        backend.clone(),
        ReadCoordinator::new(vec![], limits.clone()).unwrap(),
        limits,
        Limits::default(),
    )
    .unwrap();
    let first = manager
        .exec(
            request("yield_control(); cayw.call('write',{})"),
            "parent-first",
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let second = done(
        &manager,
        manager
            .exec(
                request("cayw.call('write',{})"),
                "parent-second",
                CancellationToken::new(),
            )
            .await
            .unwrap(),
    )
    .await;
    let first = manager
        .wait(wait(&first, true), CancellationToken::new())
        .await
        .unwrap();
    let first = done(&manager, first).await;
    let calls = backend.calls.lock().unwrap().clone();
    let finished = backend.finished.lock().unwrap().clone();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].0.parent_call_id, "parent-second");
    assert_eq!(calls[1].0.parent_call_id, "parent-first");
    assert_eq!(calls[0].0.cell_id, second.cell_id);
    assert_eq!(calls[1].0.cell_id, first.cell_id);
    assert_eq!(calls[0].1, 1);
    assert_eq!(calls[1].1, 1);
    assert_eq!(finished[0].0, calls[0].0);
    assert_eq!(finished[1].0, calls[1].0);
    assert_eq!(backend.created.lock().unwrap()[0], calls[1].0);
    assert_eq!(
        manager.record(&first.cell_id).unwrap().parent_call_id,
        "parent-first"
    );
    manager.close().await;
}

#[tokio::test]
async fn independent_connections_allow_other_tasks_but_reject_live_scope_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let limits = CellLimits::default();
    let a = Arc::new(SqliteJournal::open(&path).unwrap());
    let b = Arc::new(SqliteJournal::open(&path).unwrap());
    let host = Arc::new(Host::default());
    let first = CellManager::new(
        identity(),
        a.clone(),
        backend(host.clone()),
        ReadCoordinator::new(vec![], limits.clone()).unwrap(),
        limits.clone(),
        Limits::default(),
    )
    .unwrap();
    let page = first
        .exec(request("yield_control()"), "p", CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        b.records(&identity(), &limits).unwrap()[0].status,
        CellStatus::Yielded
    );
    assert!(
        CellManager::new(
            identity(),
            b.clone(),
            backend(host.clone()),
            ReadCoordinator::new(vec![], limits.clone()).unwrap(),
            limits.clone(),
            Limits::default()
        )
        .is_err()
    );
    assert!(
        b.recover_stopped(
            &ExecutorStopped {
                identity: identity(),
                evidence: "cannot override live lock".into()
            },
            &limits
        )
        .is_err()
    );
    let mut other = identity();
    other.task_id = "other-task".into();
    let second = CellManager::new(
        other,
        b,
        backend(host),
        ReadCoordinator::new(vec![], limits.clone()).unwrap(),
        limits,
        Limits::default(),
    )
    .unwrap();
    assert_eq!(
        done(
            &second,
            second
                .exec(
                    request("text('unrelated task')"),
                    "p",
                    CancellationToken::new()
                )
                .await
                .unwrap()
        )
        .await
        .status,
        CellStatus::Completed
    );
    assert_eq!(
        first.record(&page.cell_id).unwrap().status,
        CellStatus::Yielded
    );
    first.close().await;
    second.close().await;
}
