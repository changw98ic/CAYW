//! Independent public-API probes for cayw-cells; all state lives in temporary paths.
#![cfg(test)]
mod support;
use async_trait::async_trait;
use cayw_ai::{CancellationToken, ToolDefinition};
use cayw_cells::*;
use cayw_codemode::{CapabilityError, CapabilityHost, Limits};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use support::{TestCellExec, backend};
use tokio::sync::Semaphore;

struct Host {
    calls: AtomicUsize,
    writes: AtomicUsize,
    block_write: AtomicBool,
    write_entered: Semaphore,
    write_release: Semaphore,
    cancel_observed: AtomicBool,
    clean_finish: AtomicBool,
}
impl Default for Host {
    fn default() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            writes: AtomicUsize::new(0),
            block_write: AtomicBool::new(false),
            write_entered: Semaphore::new(0),
            write_release: Semaphore::new(0),
            cancel_observed: AtomicBool::new(false),
            clean_finish: AtomicBool::new(false),
        }
    }
}
#[async_trait]
impl CapabilityHost for Host {
    fn capabilities(&self) -> Vec<ToolDefinition> {
        ["read", "write", "cancel_then_finish"]
            .into_iter()
            .map(|name| ToolDefinition {
                name: name.into(),
                description: "independent fixture".into(),
                parameters: json!({"type":"object","additionalProperties":true}),
            })
            .collect()
    }
    async fn call(
        &self,
        name: &str,
        _args: Value,
        cancel: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match name {
            "write" => {
                self.write_entered.add_permits(1);
                if self.block_write.load(Ordering::SeqCst) {
                    let permit = self.write_release.acquire().await.unwrap();
                    permit.forget();
                }
                self.writes.fetch_add(1, Ordering::SeqCst);
                Ok(json!("written"))
            }
            "cancel_then_finish" => {
                self.write_entered.add_permits(1);
                cancel.cancelled().await;
                self.cancel_observed.store(true, Ordering::SeqCst);
                tokio::task::yield_now().await;
                self.clean_finish.store(true, Ordering::SeqCst);
                Ok(json!({"settled":true}))
            }
            _ => Ok(json!("fallback-read")),
        }
    }
}

struct Reads {
    host: Arc<Host>,
    authorized: AtomicUsize,
    executed: AtomicUsize,
    deny: AtomicBool,
}
#[async_trait]
impl support::FixtureRead for Reads {
    async fn admit(
        &self,
        _: &CellIdentity,
        _: &Value,
        _: CancellationToken,
    ) -> Result<ReadStamp, CapabilityError> {
        self.authorized.fetch_add(1, Ordering::SeqCst);
        if self.deny.load(Ordering::SeqCst) {
            return Err(CapabilityError::new("denied by fixture host"));
        }
        Ok(ReadStamp {
            workspace: "fixture-held-root".into(),
            incarnation: "fixture-incarnation".into(),
            generation: self.host.writes.load(Ordering::SeqCst) as u64,
            semantics: "fixture-exact-read".into(),
        })
    }
    async fn capture(
        &self,
        stamp: &ReadStamp,
        _: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        self.executed.fetch_add(1, Ordering::SeqCst);
        let actual = self.host.writes.load(Ordering::SeqCst).to_string();
        if stamp.generation.to_string() != actual {
            return Err(CapabilityError::new("stale dependency"));
        }
        Ok(json!({"version":actual}))
    }
}
fn identity(session: &str) -> CellIdentity {
    CellIdentity {
        session_id: session.into(),
        task_id: "task".into(),
        attempt_id: "attempt".into(),
        permission_context: "same-user-policy-v1".into(),
    }
}
fn manager(
    id: CellIdentity,
    journal: Arc<SqliteJournal>,
    host: Arc<Host>,
    reads: Arc<ReadCoordinator>,
    limits: CellLimits,
) -> Arc<CellManager> {
    CellManager::new(
        id,
        journal,
        backend(host),
        reads,
        limits,
        Limits {
            timeout: Duration::from_secs(10),
            ..Limits::default()
        },
    )
    .unwrap()
}
fn shared_manager(
    id: CellIdentity,
    journal: Arc<SqliteJournal>,
    host: Arc<Host>,
    fixture: Arc<Reads>,
    reads: Arc<ReadCoordinator>,
    limits: CellLimits,
) -> Arc<CellManager> {
    CellManager::new(
        id,
        journal,
        support::shared_backend(host, fixture, reads.clone()),
        reads,
        limits,
        Limits {
            timeout: Duration::from_secs(10),
            ..Limits::default()
        },
    )
    .unwrap()
}
fn read_policies() -> Vec<ReadPolicy> {
    vec![ReadPolicy {
        capability: "read".into(),
        reuse: true,
        coalesce: true,
    }]
}
fn request(script: &str) -> ExecRequest {
    ExecRequest {
        script: script.into(),
        yield_time_ms: 1000,
        max_output_tokens: 128 * 1024,
    }
}
async fn done(cells: &CellManager, script: &str) -> CellPage {
    let mut page = cells
        .exec_fixture(request(script), CancellationToken::new())
        .await
        .unwrap();
    for _ in 0..100 {
        if page.status.is_terminal() {
            return page;
        }
        page = cells
            .wait(
                WaitRequest {
                    cell_id: page.cell_id,
                    cursor: page.next_cursor,
                    yield_time_ms: 1000,
                    max_output_tokens: 128 * 1024,
                    continue_execution: false,
                },
                CancellationToken::new(),
            )
            .await
            .unwrap();
    }
    panic!("cell did not settle");
}
fn setup() -> (
    tempfile::TempDir,
    Arc<SqliteJournal>,
    Arc<Host>,
    Arc<CellManager>,
) {
    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(SqliteJournal::open(dir.path().join("cells.db")).unwrap());
    let host = Arc::new(Host::default());
    let limits = CellLimits::default();
    let cells = manager(
        identity("session-a"),
        journal.clone(),
        host.clone(),
        ReadCoordinator::new(vec![], limits.clone()).unwrap(),
        limits,
    );
    (dir, journal, host, cells)
}

#[tokio::test]
async fn requirement_free_globals_cannot_be_overwritten() {
    let (_dir, _journal, host, cells) = setup();
    let page = done(
        &cells,
        "new_global = 42; new_global = 43; text(tostring(new_global)); cayw.call('write', {})",
    )
    .await;
    eprintln!(
        "global probe status={:?}, writes={}, outputs={:?}",
        page.status,
        host.writes.load(Ordering::SeqCst),
        cells.record(&page.cell_id).unwrap().outputs
    );
    cells.close().await;
    assert_eq!(
        page.status,
        CellStatus::Failed,
        "unrestricted fresh globals executed and were overwritten"
    );
}

#[tokio::test]
async fn requirement_internal_catalog_is_unreachable_from_script() {
    let (_dir, _journal, host, cells) = setup();
    let page=done(&cells,"local d = _G.cayw.describe('__cells_store'); text(d.name); local names = _G.cayw.discover(); text(tostring(#names))").await;
    eprintln!(
        "raw catalog status={:?}, real calls={}, outputs={:?}",
        page.status,
        host.calls.load(Ordering::SeqCst),
        cells.record(&page.cell_id).unwrap().outputs
    );
    cells.close().await;
    assert_eq!(
        page.status,
        CellStatus::Failed,
        "raw VM catalog exposes reserved bridge internals"
    );
}

#[tokio::test]
async fn requirement_prefixed_source_does_not_expose_private_bridge_binding() {
    let (_dir, journal, host, cells) = setup();
    let page=done(&cells,"__cells_call('__cells_store', {key='bypass', value={stored=true}, expected_version=0}); text('private binding usable')").await;
    eprintln!(
        "private binding status={:?}, stored={:?}, real calls={}",
        page.status,
        journal.load("session-a", "bypass").unwrap(),
        host.calls.load(Ordering::SeqCst)
    );
    cells.close().await;
    assert_eq!(
        page.status,
        CellStatus::Failed,
        "the source shares the prelude's lexical scope"
    );
}

#[tokio::test]
async fn requirement_nil_session_store_value_roundtrips() {
    let (_dir, journal, _host, cells) = setup();
    let page=done(&cells,"store('nothing',cayw.null,0); local value=load('nothing'); text(tostring(value.value == cayw.null))").await;
    eprintln!(
        "null store value={:?}, page={page:?}",
        journal.load("session-a", "nothing").unwrap()
    );
    cells.close().await;
    assert_eq!(page.status, CellStatus::Completed);
}

#[tokio::test]
async fn requirement_store_value_accepted_at_depth_limit_can_be_loaded() {
    let (_dir, journal, host, cells) = setup();
    cells.close().await;
    drop(cells);
    let limits = CellLimits {
        json_depth: 4,
        ..CellLimits::default()
    };
    let cells = manager(
        identity("session-a"),
        journal.clone(),
        host,
        ReadCoordinator::new(vec![], limits.clone()).unwrap(),
        limits,
    );
    let page=done(&cells,"store('deep',{a={b={c=1}}},0); local value = load('deep'); text(tostring(value.value.a.b.c))").await;
    let stored = journal.load("session-a", "deep").unwrap();
    eprintln!(
        "depth-limit accepted store={stored:?}, status={:?}, error={:?}",
        page.status, page.error
    );
    cells.close().await;
    assert_eq!(
        page.status,
        CellStatus::Completed,
        "a value accepted by store fails the load wrapper's stricter depth accounting"
    );
}

#[tokio::test]
async fn public_session_store_and_cell_lookup_are_isolated() {
    let (_dir, journal, host, a) = setup();
    let limits = CellLimits::default();
    let b = manager(
        identity("session-b"),
        journal,
        host,
        ReadCoordinator::new(vec![], limits.clone()).unwrap(),
        limits,
    );
    let pa = done(&a, "store('value',{secret='A'},0); text('created')").await;
    let pb = done(&b, "text(tostring(load('value') == cayw.null))").await;
    assert_eq!(pa.status, CellStatus::Completed);
    assert_eq!(pb.status, CellStatus::Completed);
    assert!(matches!(b.record(&pa.cell_id), Err(CellError::NotFound)));
    assert!(
        matches!(&b.record(&pb.cell_id).unwrap().outputs[0].output,CellOutput::Text{text} if text=="true")
    );
    a.close().await;
    b.close().await;
}

#[tokio::test]
async fn public_cache_is_isolated_and_each_cache_hit_is_authorized() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(SqliteJournal::open(dir.path().join("cells.db")).unwrap());
    let host = Arc::new(Host::default());
    let backend = Arc::new(Reads {
        host: host.clone(),
        authorized: AtomicUsize::new(0),
        executed: AtomicUsize::new(0),
        deny: AtomicBool::new(false),
    });
    let limits = CellLimits::default();
    let reads = ReadCoordinator::new(read_policies(), limits.clone()).unwrap();
    let a = shared_manager(
        identity("a"),
        journal.clone(),
        host.clone(),
        backend.clone(),
        reads.clone(),
        limits.clone(),
    );
    let b = shared_manager(identity("b"), journal, host, backend.clone(), reads, limits);
    assert_eq!(
        done(
            &a,
            "cayw.call('read', {a=1,b=2}); cayw.call('read', {b=2,a=1})"
        )
        .await
        .status,
        CellStatus::Completed
    );
    assert_eq!(backend.authorized.load(Ordering::SeqCst), 2);
    assert_eq!(backend.executed.load(Ordering::SeqCst), 1);
    assert_eq!(
        done(&b, "cayw.call('read', {a=1,b=2})").await.status,
        CellStatus::Completed
    );
    assert_eq!(backend.executed.load(Ordering::SeqCst), 2);
    backend.deny.store(true, Ordering::SeqCst);
    assert_eq!(
        done(&a, "cayw.call('read', {a=1,b=2})").await.status,
        CellStatus::Failed
    );
    assert_eq!(backend.authorized.load(Ordering::SeqCst), 4);
    assert_eq!(backend.executed.load(Ordering::SeqCst), 2);
    a.close().await;
    b.close().await;
}

#[tokio::test]
async fn requirement_cross_session_write_invalidates_inflight_dependency_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(SqliteJournal::open(dir.path().join("cells.db")).unwrap());
    let host = Arc::new(Host::default());
    let backend = Arc::new(Reads {
        host: host.clone(),
        authorized: AtomicUsize::new(0),
        executed: AtomicUsize::new(0),
        deny: AtomicBool::new(false),
    });
    let limits = CellLimits::default();
    let reads = ReadCoordinator::new(read_policies(), limits.clone()).unwrap();
    let a = shared_manager(
        identity("a"),
        journal.clone(),
        host.clone(),
        backend.clone(),
        reads.clone(),
        limits.clone(),
    );
    let b = shared_manager(
        identity("b"),
        journal,
        host.clone(),
        backend.clone(),
        reads,
        limits,
    );
    assert_eq!(
        done(&b, "cayw.call('read', {})").await.status,
        CellStatus::Completed
    );
    host.block_write.store(true, Ordering::SeqCst);
    let mut req = request("cayw.call('write', {})");
    req.yield_time_ms = 1;
    let write = a.exec_fixture(req, CancellationToken::new()).await.unwrap();
    host.write_entered.acquire().await.unwrap().forget();
    assert_eq!(
        done(&b, "cayw.call('read', {})").await.status,
        CellStatus::Completed
    );
    assert_eq!(backend.executed.load(Ordering::SeqCst), 2);
    host.write_release.add_permits(1);
    let final_write = a
        .wait(
            WaitRequest {
                cell_id: write.cell_id,
                cursor: 0,
                yield_time_ms: 1000,
                max_output_tokens: 4096,
                continue_execution: false,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(final_write.status, CellStatus::Completed);
    let after = done(&b, "cayw.call('read', {})").await;
    let observed = backend.executed.load(Ordering::SeqCst);
    eprintln!(
        "cross-session writes={}, read executions={observed}, final={after:?}",
        host.writes.load(Ordering::SeqCst)
    );
    a.close().await;
    b.close().await;
    assert_eq!(
        observed, 3,
        "host dependency version changed, requiring a fresh backend execution"
    );
}

#[tokio::test]
async fn requirement_stored_value_fitting_default_depth_can_be_loaded() {
    let (_dir, journal, _host, cells) = setup();
    let page = done(
        &cells,
        "local v=1; for i=1,31 do v={a=v} end; store('deep_default',v,0); load('deep_default')",
    )
    .await;
    assert!(journal.load("session-a", "deep_default").unwrap().is_some());
    eprintln!(
        "default-depth store accepted, load status={:?}, error={:?}",
        page.status, page.error
    );
    cells.close().await;
    assert_eq!(
        page.status,
        CellStatus::Completed,
        "default store/load depth limits differ by the snapshot envelope"
    );
}

#[tokio::test]
async fn requirement_cancelled_read_waiter_does_not_freeze_unrelated_writer_intents() {
    let (_dir, _journal, _host, cells) = setup();
    let page = cells
        .exec_fixture(request("yield_control()"), CancellationToken::new())
        .await
        .unwrap();
    let stopped = cells.cancel(&page.cell_id).await.unwrap();
    assert_eq!(stopped.status, CellStatus::Cancelled);
    assert!(!stopped.outcome_unknown);
    cells.close().await;
}

#[tokio::test]
async fn requirement_cancel_allows_host_to_settle_after_token_cancellation() {
    let (_dir, _journal, host, cells) = setup();
    let mut req = request("cayw.call('cancel_then_finish', {})");
    req.yield_time_ms = 1;
    let page = cells
        .exec_fixture(req, CancellationToken::new())
        .await
        .unwrap();
    host.write_entered.acquire().await.unwrap().forget();
    let stopped = cells.cancel(&page.cell_id).await.unwrap();
    let observed = host.cancel_observed.load(Ordering::SeqCst);
    let clean = host.clean_finish.load(Ordering::SeqCst);
    eprintln!(
        "cancel status={:?}, token observed={observed}, host settled={clean}, intent={:?}",
        stopped.status,
        cells.record(&page.cell_id).unwrap().calls
    );
    cells.close().await;
    assert!(
        observed && clean,
        "manager abort dropped the host future before polling cancellation cleanup"
    );
}

#[tokio::test]
async fn requirement_raw_emit_respects_output_limit_before_next_real_operation() {
    let (_dir, journal, host, cells) = setup();
    cells.close().await;
    drop(cells);
    let limits = CellLimits {
        output_items: 2,
        ..CellLimits::default()
    };
    let cells = manager(
        identity("session-a"),
        journal,
        host.clone(),
        ReadCoordinator::new(vec![], limits.clone()).unwrap(),
        limits,
    );
    let page = done(
        &cells,
        "_G.cayw.emit('a'); _G.cayw.emit('b'); _G.cayw.emit('overflow'); cayw.call('write', {})",
    )
    .await;
    let writes = host.writes.load(Ordering::SeqCst);
    eprintln!(
        "raw emit overflow status={:?}, writes={writes}, outputs={:?}",
        page.status,
        cells.record(&page.cell_id).unwrap().outputs
    );
    cells.close().await;
    assert_eq!(
        writes, 0,
        "the output item limit was only enforced after the write had happened"
    );
}

#[tokio::test]
async fn requirement_raw_emit_is_incrementally_published_before_yield() {
    let (_dir, _journal, _host, cells) = setup();
    let page = cells
        .exec_fixture(
            request("_G.cayw.emit('visible before yield'); yield_control()"),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    eprintln!("raw emit yield={page:?}");
    let count = page.outputs.len();
    cells.close().await;
    assert_eq!(
        count, 1,
        "raw emit bypasses the durable incremental output bridge"
    );
}

struct CrashEffectHost {
    effect_path: std::path::PathBuf,
    calls: AtomicUsize,
}
#[async_trait]
impl CapabilityHost for CrashEffectHost {
    fn capabilities(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "persist_effect".into(),
            description: "independent fsynced crash fixture".into(),
            parameters: json!({"type":"object","additionalProperties":false}),
        }]
    }
    async fn call(
        &self,
        _: &str,
        _: Value,
        _: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        use std::io::Write;
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.effect_path)
            .unwrap();
        writeln!(&mut file, "effect happened").unwrap();
        file.sync_all().unwrap();
        std::future::pending().await
    }
}
#[test]
#[ignore = "invoked by independent_crash_effect_is_durable_unknown_without_replay"]
fn crash_effect_child() {
    let dir =
        std::path::PathBuf::from(std::env::var_os("CAYW_CELLS_INDEPENDENT_CRASH_DIR").unwrap());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    runtime.block_on(async {
        let limits = CellLimits::default();
        let host = Arc::new(CrashEffectHost {
            effect_path: dir.join("effect.txt"),
            calls: AtomicUsize::new(0),
        });
        let cells = CellManager::new(
            identity("crash"),
            Arc::new(SqliteJournal::open(dir.join("cells.db")).unwrap()),
            backend(host),
            ReadCoordinator::new(vec![], limits.clone()).unwrap(),
            limits,
            Limits::default(),
        )
        .unwrap();
        let mut req = request("cayw.call('persist_effect', {})");
        req.yield_time_ms = 1;
        let page = cells
            .exec_fixture(req, CancellationToken::new())
            .await
            .unwrap();
        assert!(dir.join("effect.txt").exists());
        assert_eq!(
            cells.record(&page.cell_id).unwrap().calls[0].status,
            IntentStatus::Pending
        );
        std::fs::write(dir.join("cell-id.txt"), page.cell_id).unwrap();
        std::process::exit(74)
    });
}
#[test]
fn independent_crash_effect_is_durable_unknown_without_replay() {
    use std::process::{Child, Command};
    struct ChildGuard(Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let mut child = ChildGuard(
        Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "crash_effect_child", "--nocapture"])
            .env("CAYW_CELLS_INDEPENDENT_CRASH_DIR", dir.path())
            .spawn()
            .unwrap(),
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            assert_eq!(status.code(), Some(74));
            break;
        }
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    let cell_id = std::fs::read_to_string(dir.path().join("cell-id.txt")).unwrap();
    let effect = std::fs::read_to_string(dir.path().join("effect.txt")).unwrap();
    assert_eq!(effect, "effect happened\n");
    let journal = Arc::new(SqliteJournal::open(dir.path().join("cells.db")).unwrap());
    assert!(!journal.is_frozen("crash").unwrap());
    journal
        .recover_stopped(
            &ExecutorStopped {
                identity: identity("crash"),
                evidence: "parent observed actual child exit 74".into(),
            },
            &CellLimits::default(),
        )
        .unwrap();
    assert!(journal.is_frozen("crash").unwrap());
    let limits = CellLimits::default();
    let host = Arc::new(CrashEffectHost {
        effect_path: dir.path().join("effect.txt"),
        calls: AtomicUsize::new(0),
    });
    let mut id = identity("crash");
    id.attempt_id = "after-crash".into();
    let cells = CellManager::new(
        id,
        journal.clone(),
        backend(host.clone()),
        ReadCoordinator::new(vec![], limits.clone()).unwrap(),
        limits,
        Limits::default(),
    )
    .unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    runtime.block_on(async {
        let page = cells
            .wait(
                WaitRequest {
                    cell_id: cell_id.clone(),
                    cursor: 0,
                    yield_time_ms: 0,
                    max_output_tokens: 4096,
                    continue_execution: true,
                },
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(page.status, CellStatus::OutcomeUnknown);
        assert!(!page.vm_alive);
        assert_eq!(host.calls.load(Ordering::SeqCst), 0);
        assert!(matches!(
            cells
                .exec_fixture(request("text('must not run')"), CancellationToken::new())
                .await,
            Err(CellError::Frozen)
        ));
        let record = cells.record(&cell_id).unwrap();
        let evidence = ReconciliationEvidence {
            source_hash: record.source_hash,
            call_sequence: 1,
            expected_arguments: json!({}),
            observed_result: json!({"effect_file":effect}),
            evidence: "parent fs read of fsynced temporary effect.txt".into(),
            resolution: Resolution::Succeeded,
        };
        assert_eq!(
            cells.reconcile(&cell_id, evidence).unwrap().status,
            CellStatus::Reconciled
        );
        assert!(!journal.is_frozen("crash").unwrap());
        assert_eq!(host.calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("effect.txt")).unwrap(),
            "effect happened\n"
        );
        cells.close().await;
    });
}

#[tokio::test]
async fn public_output_page_fits_byte_budget_and_preserves_unread_items() {
    let (dir, journal, host, cells) = setup();
    cells.close().await;
    drop(cells);
    let limits = CellLimits {
        output_items: 2,
        output_bytes: 4096,
        item_bytes: 2048,
        ..CellLimits::default()
    };
    let cells = manager(
        identity("session-a"),
        journal.clone(),
        host,
        ReadCoordinator::new(vec![], limits.clone()).unwrap(),
        limits,
    );
    let page = done(
        &cells,
        "text(string.rep('中',500)); text('second'); text('overflow')",
    )
    .await;
    assert_eq!(page.status, CellStatus::Failed);
    let small = cells
        .wait(
            WaitRequest {
                cell_id: page.cell_id.clone(),
                cursor: 0,
                yield_time_ms: 0,
                max_output_tokens: 1024,
                continue_execution: false,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(small.next_cursor, 0);
    assert!(small.required_budget.unwrap() > 1500);
    assert!(serde_json::to_vec(&small).unwrap().len() <= 1024);
    let full = cells
        .wait(
            WaitRequest {
                cell_id: page.cell_id,
                cursor: 0,
                yield_time_ms: 0,
                max_output_tokens: 4096,
                continue_execution: false,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(full.outputs.len(), 2);
    assert_eq!(full.outputs[0].cursor, 1);
    assert_eq!(full.outputs[1].cursor, 2);
    cells.close().await;
    drop(cells);
    drop(journal);
    drop(dir);
}
