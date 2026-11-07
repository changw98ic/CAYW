mod support;
use async_trait::async_trait;
use cayw_agent::AgentTool;
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
use support::{TestCellExec, backend, backend_with_values, values_wrap};
use tempfile::TempDir;
use tokio::sync::Semaphore;

struct Host {
    calls: AtomicUsize,
    entered: Semaphore,
    release: Semaphore,
    dropped: Arc<AtomicBool>,
    write_value: AtomicUsize,
}
impl Default for Host {
    fn default() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            entered: Semaphore::new(0),
            release: Semaphore::new(0),
            dropped: Arc::new(AtomicBool::new(false)),
            write_value: AtomicUsize::new(0),
        }
    }
}
struct MarkDrop(Arc<AtomicBool>);
impl Drop for MarkDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}
#[async_trait]
impl CapabilityHost for Host {
    fn capabilities(&self) -> Vec<ToolDefinition> {
        ["read", "write", "block", "uncertain"]
            .into_iter()
            .map(|name| ToolDefinition {
                name: name.into(),
                description: format!("host {name}"),
                parameters: json!({"type":"object","additionalProperties":true}),
            })
            .collect()
    }
    async fn call(
        &self,
        name: &str,
        args: Value,
        cancel: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match name {
            "block" => {
                let _mark = MarkDrop(self.dropped.clone());
                self.entered.add_permits(1);
                tokio::select! {
                    _ = cancel.cancelled() => Err(CapabilityError::unknown("cancelled after dispatch")),
                    permit = self.release.acquire() => { permit.unwrap().forget(); Ok(json!("released")) },
                }
            }
            "uncertain" => Err(CapabilityError::unknown("lost operation receipt")),
            "write" => {
                self.write_value.fetch_add(1, Ordering::SeqCst);
                Ok(json!("written"))
            }
            _ => Ok(args),
        }
    }
}
fn identity(session: &str) -> CellIdentity {
    CellIdentity {
        session_id: session.into(),
        task_id: "task".into(),
        attempt_id: "attempt".into(),
        permission_context: "principal/policy-v1".into(),
    }
}
fn manager(
    journal: Arc<SqliteJournal>,
    host: Arc<Host>,
    reads: Arc<ReadCoordinator>,
    limits: CellLimits,
    session: &str,
) -> Arc<CellManager> {
    manager_with_host(journal, host, reads, limits, session)
}
fn manager_with_host(
    journal: Arc<SqliteJournal>,
    host: Arc<dyn CapabilityHost>,
    reads: Arc<ReadCoordinator>,
    limits: CellLimits,
    session: &str,
) -> Arc<CellManager> {
    let backend = backend_with_values(host, journal.clone(), limits.clone());
    CellManager::new(
        identity(session),
        journal,
        backend,
        reads,
        limits,
        Limits {
            timeout: Duration::from_secs(30),
            host_calls: 256,
            ..Limits::default()
        },
    )
    .unwrap()
}
fn shared_manager(
    journal: Arc<SqliteJournal>,
    host: Arc<Host>,
    fixture: Arc<Reads>,
    limits: CellLimits,
    session: &str,
) -> Arc<CellManager> {
    let reads = ReadCoordinator::new(
        vec![ReadPolicy {
            capability: "read".into(),
            reuse: true,
            coalesce: true,
        }],
        limits.clone(),
    )
    .unwrap();
    let backend = values_wrap(
        support::shared_backend(host, fixture, reads.clone()),
        journal.clone(),
        limits.clone(),
    );
    CellManager::new(
        identity(session),
        journal,
        backend,
        reads,
        limits,
        Limits {
            timeout: Duration::from_secs(30),
            host_calls: 256,
            ..Limits::default()
        },
    )
    .unwrap()
}
fn setup() -> (TempDir, Arc<SqliteJournal>, Arc<Host>, Arc<CellManager>) {
    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(SqliteJournal::open(dir.path().join("cells.db")).unwrap());
    let host = Arc::new(Host::default());
    let limits = CellLimits::default();
    let reads = ReadCoordinator::new(vec![], limits.clone()).unwrap();
    let cells = manager(journal.clone(), host.clone(), reads, limits, "session");
    (dir, journal, host, cells)
}
fn exec(script: &str) -> ExecRequest {
    ExecRequest {
        script: script.into(),
        yield_time_ms: 10_000,
        max_output_tokens: 128 * 1024,
    }
}
fn wait(page: &CellPage, continue_execution: bool) -> WaitRequest {
    WaitRequest {
        cell_id: page.cell_id.clone(),
        cursor: page.next_cursor,
        yield_time_ms: 10_000,
        max_output_tokens: 128 * 1024,
        continue_execution,
    }
}
async fn terminal(cells: &CellManager, mut page: CellPage) -> CellPage {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !page.status.is_terminal() {
            page = cells
                .wait(wait(&page, false), CancellationToken::new())
                .await
                .unwrap();
        }
        page
    })
    .await
    .unwrap()
}
async fn wait_entered(host: &Host) {
    tokio::time::timeout(Duration::from_secs(5), host.entered.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
}

#[tokio::test]
async fn yield_is_owned_and_resumed_only_by_explicit_wait() {
    let (_dir, _journal, host, cells) = setup();
    let first = cells
        .exec_fixture(
            exec("text('before'); yield_control(); text('after'); cayw.call('write', {})"),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let mut page = first;
    while page.status != CellStatus::Yielded {
        page = cells
            .wait(wait(&page, false), CancellationToken::new())
            .await
            .unwrap();
    }
    assert_eq!(host.calls.load(Ordering::SeqCst), 0);
    let polled = cells
        .wait(wait(&page, false), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(polled.status, CellStatus::Yielded);
    assert!(polled.outputs.is_empty());
    let continued = cells
        .wait(wait(&page, true), CancellationToken::new())
        .await
        .unwrap();
    let done = terminal(&cells, continued).await;
    assert_eq!(done.status, CellStatus::Completed);
    let record = cells.record(&page.cell_id).unwrap();
    assert_eq!(record.outputs.len(), 2);
    assert_eq!(host.calls.load(Ordering::SeqCst), 1);
    let replay = cells
        .wait(
            WaitRequest {
                cursor: 0,
                ..wait(&done, false)
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(replay.outputs.len(), 2);
    assert_eq!(host.calls.load(Ordering::SeqCst), 1);
    cells.close().await;
}

#[tokio::test]
async fn continue_wait_with_stale_cursor_waits_for_resumed_progress() {
    let (_dir, _journal, host, cells) = setup();
    let first = cells
        .exec_fixture(
            exec("text('before'); yield_control(); cayw.call('block', {})"),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let mut page = first;
    while page.status != CellStatus::Yielded {
        page = cells
            .wait(wait(&page, false), CancellationToken::new())
            .await
            .unwrap();
    }
    assert!(
        page.next_cursor > 0,
        "yield page must already publish output"
    );
    let wait_stale = cells.wait(
        WaitRequest {
            cell_id: page.cell_id.clone(),
            cursor: 0,
            yield_time_ms: 10_000,
            max_output_tokens: 128 * 1024,
            continue_execution: true,
        },
        CancellationToken::new(),
    );
    tokio::pin!(wait_stale);
    // Poll the continue-wait so resume runs, then observe the blocked host call.
    tokio::select! {
        biased;
        result = &mut wait_stale => panic!("stale-cursor continue returned before host entered: {result:?}"),
        _ = wait_entered(&host) => {}
    }
    assert_eq!(host.calls.load(Ordering::SeqCst), 1);
    // The resumed Host call is blocked until release; observe Pending without
    // depending on how long this machine takes to schedule the task.
    assert!(
        std::future::poll_fn(|cx| {
            std::task::Poll::Ready(wait_stale.as_mut().poll(cx).is_pending())
        })
        .await
    );
    host.release.add_permits(1);
    let done = wait_stale.await.unwrap();
    assert!(done.status.is_terminal());
    assert!(!done.vm_alive);
    cells.close().await;
}

#[tokio::test]
async fn timed_exec_returns_without_cancelling_and_wait_does_not_repeat_host_call() {
    let (_dir, _journal, host, cells) = setup();
    let page = cells
        .exec_fixture(
            ExecRequest {
                yield_time_ms: 1,
                ..exec("local result = cayw.call('block', {}); text(result)")
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    wait_entered(&host).await;
    assert!(!host.dropped.load(Ordering::Acquire));
    assert!(!page.status.is_terminal());
    host.release.add_permits(1);
    let done = terminal(&cells, page).await;
    assert_eq!(done.status, CellStatus::Completed);
    assert_eq!(host.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        cells.record(&done.cell_id).unwrap().calls[0].status,
        IntentStatus::Succeeded
    );
    cells.close().await;
}

#[tokio::test]
async fn exit_stops_following_real_operations_and_is_not_an_unknown_effect() {
    let (_dir, _journal, host, cells) = setup();
    let page = cells
        .exec_fixture(
            exec("text('before'); exit(7); cayw.call('write', {}); text('after')"),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let done = terminal(&cells, page).await;
    assert_eq!(done.status, CellStatus::Exited);
    assert!(!done.outcome_unknown);
    assert_eq!(host.calls.load(Ordering::SeqCst), 0);
    assert!(matches!(
        cells
            .record(&done.cell_id)
            .unwrap()
            .outputs
            .last()
            .unwrap()
            .output,
        CellOutput::Exit { code: 7 }
    ));
    cells.close().await;
}

#[tokio::test]
async fn store_load_are_durable_cas_snapshots_and_isolated_by_session() {
    let (dir, journal, host, cells) = setup();
    let page = cells.exec_fixture(exec("store('state', {count=1}, 0); local a=load('state'); a.value.count=99; local b=load('state'); text(tostring(b.value.count))"),CancellationToken::new()).await.unwrap();
    let done = terminal(&cells, page).await;
    assert_eq!(done.status, CellStatus::Completed);
    assert_eq!(
        journal.load("session", "state").unwrap().unwrap().value,
        json!({"count":1})
    );
    assert!(matches!(
        journal.store("session", "state", 0, json!(2), cells.limits()),
        Err(CellError::Conflict)
    ));
    assert!(journal.load("other", "state").unwrap().is_none());
    cells.close().await;
    drop(cells);
    drop(journal);
    let journal = Arc::new(SqliteJournal::open(dir.path().join("cells.db")).unwrap());
    assert_eq!(
        journal.load("session", "state").unwrap().unwrap().version,
        1
    );
    let reads = ReadCoordinator::new(vec![], CellLimits::default()).unwrap();
    let cells = manager(
        journal,
        host.clone(),
        reads,
        CellLimits::default(),
        "session",
    );
    let recovered = cells
        .wait(
            WaitRequest {
                cell_id: done.cell_id,
                cursor: 0,
                yield_time_ms: 0,
                max_output_tokens: 4096,
                continue_execution: false,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(recovered.status, CellStatus::Completed);
    assert_eq!(host.calls.load(Ordering::SeqCst), 0);
    cells.close().await;
}

#[tokio::test]
async fn unknown_freezes_survives_restart_and_requires_matching_evidence() {
    let (dir, journal, host, cells) = setup();
    let page = cells
        .exec_fixture(
            exec("cayw.call('uncertain', {key='x'}); cayw.call('write', {})"),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let done = terminal(&cells, page).await;
    assert_eq!(done.status, CellStatus::OutcomeUnknown);
    assert!(journal.is_frozen("session").unwrap());
    assert!(matches!(
        cells
            .exec_fixture(exec("text('forbidden')"), CancellationToken::new())
            .await,
        Err(CellError::Frozen)
    ));
    cells.close().await;
    drop(cells);
    drop(journal);
    let journal = Arc::new(SqliteJournal::open(dir.path().join("cells.db")).unwrap());
    let cells = manager(
        journal.clone(),
        host.clone(),
        ReadCoordinator::new(vec![], CellLimits::default()).unwrap(),
        CellLimits::default(),
        "session",
    );
    let record = cells.record(&done.cell_id).unwrap();
    assert!(matches!(
        cells
            .exec_fixture(exec("text('still forbidden')"), CancellationToken::new())
            .await,
        Err(CellError::Frozen)
    ));
    let mut evidence = ReconciliationEvidence {
        source_hash: record.source_hash,
        call_sequence: 1,
        expected_arguments: json!({"key":"wrong"}),
        observed_result: json!({"receipt":"actual-123"}),
        evidence: "host observation record #123".into(),
        resolution: Resolution::Succeeded,
    };
    assert!(matches!(
        cells.reconcile(&record.cell_id, evidence.clone()),
        Err(CellError::Conflict)
    ));
    evidence.expected_arguments = json!({"key":"x"});
    evidence.evidence.clear();
    assert!(matches!(
        cells.reconcile(&record.cell_id, evidence.clone()),
        Err(CellError::Invalid(_))
    ));
    evidence.evidence = "host observation record #123".into();
    assert_eq!(
        cells.reconcile(&record.cell_id, evidence).unwrap().status,
        CellStatus::Reconciled
    );
    assert!(!journal.is_frozen("session").unwrap());
    assert_eq!(host.calls.load(Ordering::SeqCst), 1);
    let next = terminal(
        &cells,
        cells
            .exec_fixture(exec("text('allowed')"), CancellationToken::new())
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(next.status, CellStatus::Completed);
    assert_eq!(host.calls.load(Ordering::SeqCst), 1);
    cells.close().await;
}

#[tokio::test]
async fn cancel_close_and_drop_stop_owned_tasks_and_preserve_unknown() {
    for action in ["cancel", "close", "drop"] {
        let (dir, journal, host, cells) = setup();
        let page = cells
            .exec_fixture(
                ExecRequest {
                    yield_time_ms: 1,
                    ..exec("cayw.call('block', {})")
                },
                CancellationToken::new(),
            )
            .await
            .unwrap();
        wait_entered(&host).await;
        if action == "cancel" {
            let cancelled = cells.cancel(&page.cell_id).await.unwrap();
            assert_eq!(cancelled.status, CellStatus::OutcomeUnknown);
            cells.close().await;
        } else if action == "close" {
            cells.close().await;
        }
        drop(cells);
        tokio::time::timeout(Duration::from_secs(5), async {
            while !host.dropped.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        // Let task cleanup release the process lock before reopening.
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if journal.is_frozen("session").unwrap() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        drop(journal);
        let reopened = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(journal) = SqliteJournal::open(dir.path().join("cells.db")) {
                    break journal;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(reopened.is_frozen("session").unwrap());
    }
}

#[tokio::test]
async fn typed_output_mime_base64_and_notify_are_strict() {
    let (_dir, _journal, _host, cells) = setup();
    let page = cells.exec_fixture(exec("text('你好'); image('image/png','AQID'); audio('audio/wav','AQID'); notify('working',1,2); cayw.emit({ok=true})"),CancellationToken::new()).await.unwrap();
    let done = terminal(&cells, page).await;
    assert_eq!(done.status, CellStatus::Completed);
    let record = cells.record(&done.cell_id).unwrap();
    assert_eq!(record.outputs.len(), 5);
    assert!(matches!(&record.outputs[0].output,CellOutput::Text { text } if text == "你好"));
    assert!(matches!(
        &record.outputs[1].output,
        CellOutput::Image { .. }
    ));
    assert!(matches!(
        &record.outputs[2].output,
        CellOutput::Audio { .. }
    ));
    assert!(matches!(
        &record.outputs[3].output,
        CellOutput::Notify { .. }
    ));
    for source in [
        "image('text/html','AQID')",
        "image('image/png','AB==')",
        "audio('audio/wav','not base64')",
        "notify('bad',3,2)",
        "exit(false)",
        "tool_search(false,1)",
        "cayw.call('__cells_exit', {code=0})",
    ] {
        let done = terminal(
            &cells,
            cells
                .exec_fixture(exec(source), CancellationToken::new())
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(
            done.status,
            CellStatus::Failed,
            "{source}: {:?}",
            cells.record(&done.cell_id).unwrap()
        );
    }
    cells.close().await;
}

#[tokio::test]
async fn media_decoded_byte_boundary_and_notify_length_stop_before_host_effects() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(SqliteJournal::open(dir.path().join("cells.db")).unwrap());
    let host = Arc::new(Host::default());
    let limits = CellLimits {
        media_bytes: 8,
        ..CellLimits::default()
    };
    let cells = manager(
        journal,
        host.clone(),
        ReadCoordinator::new(vec![], limits.clone()).unwrap(),
        limits,
        "session",
    );
    let page = cells
        .exec_fixture(
            exec("image('image/png','AAECAwQFBgc='); audio('audio/wav','AAECAwQFBgc='); notify(string.rep('x',512),1,2); cayw.call('write',{})"),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let done = terminal(&cells, page).await;
    assert_eq!(
        done.status,
        CellStatus::Completed,
        "{:?}",
        cells.record(&done.cell_id).unwrap()
    );
    assert_eq!(cells.record(&done.cell_id).unwrap().outputs.len(), 3);
    assert_eq!(host.calls.load(Ordering::SeqCst), 1);
    for source in [
        "image('image/png','AAECAwQFBgcI')",
        "image('image/png','')",
        "audio('audio/mp3','AAECAwQFBgc=')",
        "notify(string.rep('x',513),1,2)",
    ] {
        let script = format!("{source}; cayw.call('write',{{}})");
        let page = cells
            .exec_fixture(exec(&script), CancellationToken::new())
            .await
            .unwrap();
        let done = terminal(&cells, page).await;
        assert_eq!(
            done.status,
            CellStatus::Failed,
            "{source}: {:?}",
            cells.record(&done.cell_id).unwrap()
        );
        assert!(
            cells.record(&done.cell_id).unwrap().outputs.is_empty(),
            "{source}"
        );
    }
    assert_eq!(host.calls.load(Ordering::SeqCst), 1);
    let done = terminal(
        &cells,
        cells
            .exec_fixture(exec("text('after')"), CancellationToken::new())
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(done.status, CellStatus::Completed);
    assert_eq!(host.calls.load(Ordering::SeqCst), 1);
    cells.close().await;
}

#[tokio::test]
async fn capacity_output_and_store_limits_are_enforced_without_cursor_overwrite() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(SqliteJournal::open(dir.path().join("cells.db")).unwrap());
    let host = Arc::new(Host::default());
    let limits = CellLimits {
        max_running: 1,
        output_items: 2,
        output_bytes: 16_384,
        item_bytes: 4096,
        media_bytes: 8,
        store_bytes: 100,
        store_keys: 1,
        json_depth: 4,
        ..CellLimits::default()
    };
    let cells = manager(
        journal.clone(),
        host.clone(),
        ReadCoordinator::new(vec![], limits.clone()).unwrap(),
        limits.clone(),
        "session",
    );
    let page = cells
        .exec_fixture(
            exec("text(string.rep('x',2000)); text('second'); text('overflow')"),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let done = terminal(&cells, page).await;
    assert_eq!(done.status, CellStatus::Failed);
    let request = WaitRequest {
        cell_id: done.cell_id.clone(),
        cursor: 0,
        yield_time_ms: 0,
        max_output_tokens: 1024,
        continue_execution: false,
    };
    let small = cells
        .wait(request.clone(), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(small.next_cursor, 0);
    assert!(small.required_budget.unwrap() > 2000);
    assert!(serde_json::to_vec(&small).unwrap().len() <= 1024);
    let large = cells
        .wait(
            WaitRequest {
                max_output_tokens: 4096,
                ..request
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(large.outputs.len(), 2);
    assert_eq!(large.outputs[0].cursor, 1);
    assert_eq!(large.outputs[1].cursor, 2);
    assert!(
        journal
            .store("session", "a", 0, json!({"a":{"b":{"c":{"d":1}}}}), &limits)
            .is_err()
    );
    assert!(
        journal
            .store("session", "a", 0, json!("x".repeat(101)), &limits)
            .is_err()
    );
    journal.store("session", "a", 0, json!(1), &limits).unwrap();
    assert!(matches!(
        journal.store("session", "b", 0, json!(1), &limits),
        Err(CellError::Capacity(ref diag)) if diag.kind == CapacityKind::StoreKeys
    ));
    let page = cells
        .exec_fixture(
            ExecRequest {
                yield_time_ms: 1,
                ..exec("yield_control()")
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(matches!(
        cells
            .exec_fixture(exec("text('overflow')"), CancellationToken::new())
            .await,
        Err(CellError::Capacity(ref diag)) if diag.kind == CapacityKind::MaxRunning
    ));
    let cancelled = cells.cancel(&page.cell_id).await.unwrap();
    assert_eq!(cancelled.status, CellStatus::Cancelled);
    cells.close().await;
}

#[derive(Default)]
struct SearchHost {
    calls: AtomicUsize,
}
#[async_trait]
impl CapabilityHost for SearchHost {
    fn capabilities(&self) -> Vec<ToolDefinition> {
        (0..20)
            .map(|index| ToolDefinition {
                name: format!("cap{index:02}"),
                description: format!("description only needle {index}"),
                parameters: json!({"type":"object"}),
            })
            .collect()
    }
    async fn call(
        &self,
        name: &str,
        arguments: Value,
        _: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(json!({"name":name,"arguments":arguments}))
    }
}

#[tokio::test]
async fn tool_search_default_and_explicit_limits_truncate_description_matches() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(SqliteJournal::open(dir.path().join("cells.db")).unwrap());
    let host = Arc::new(SearchHost::default());
    let limits = CellLimits::default();
    let cells = manager_with_host(
        journal,
        host.clone(),
        ReadCoordinator::new(vec![], limits.clone()).unwrap(),
        limits,
        "session",
    );
    let page = cells
        .exec_fixture(
            exec("local default = tool_search('needle'); local three = tool_search('needle', 3); local wide = tool_search('needle', 64); return {default_count=#default, three_count=#three, wide_count=#wide, first=three[1].name, last=three[3].name, description=three[1].description}"),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let done = terminal(&cells, page).await;
    let record = cells.record(&done.cell_id).unwrap();
    assert_eq!(done.status, CellStatus::Completed, "{record:?}");
    let value = match &record.outputs[0].output {
        CellOutput::Json { value } => value,
        output => panic!("expected the returned search proof, got {output:?}"),
    };
    assert_eq!(value["default_count"], 16);
    assert_eq!(value["three_count"], 3);
    assert_eq!(value["wide_count"], 20);
    assert_eq!(value["first"], "cap00");
    assert_eq!(value["last"], "cap02");
    assert_eq!(value["description"], "description only needle 0");
    assert!(!value["first"].as_str().unwrap().contains("needle"));
    assert_eq!(host.calls.load(Ordering::SeqCst), 0);
    for source in [
        "tool_search('needle',0)",
        "tool_search('needle',65)",
        "tool_search('needle',1.5)",
        "tool_search('needle',-1)",
        "tool_search('needle','3')",
    ] {
        let script = format!("{source}; cayw.call('write',{{}})");
        let done = terminal(
            &cells,
            cells
                .exec_fixture(exec(&script), CancellationToken::new())
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(
            done.status,
            CellStatus::Failed,
            "{source}: {:?}",
            cells.record(&done.cell_id).unwrap()
        );
    }
    assert_eq!(host.calls.load(Ordering::SeqCst), 0);
    cells.close().await;
}

#[tokio::test]
async fn single_agent_entry_schema_has_only_exec_and_wait_and_no_session_override() {
    let (_dir, _journal, _host, cells) = setup();
    let tool = CellTool::new(cells.clone());
    let definition = tool.definition();
    assert_eq!(definition.name, "execute_code");
    let validator = cayw_ai::ArgumentValidator::new(&definition.parameters).unwrap();
    assert!(validator.validate(&json!({"action":"exec","script":"text('ok')","yield_time_ms":0,"max_output_tokens":1024})).is_ok());
    let raw = json!({"action":"exec","script":"return '00123'","yield_time_ms":"30000","max_output_tokens":"8192.0"});
    let prepared = tool.prepare_arguments(raw.clone()).unwrap();
    assert_eq!(
        prepared,
        json!({"action":"exec","script":"return '00123'","yield_time_ms":30000,"max_output_tokens":8192})
    );
    assert!(validator.validate(&prepared).is_ok());
    assert_eq!(raw["max_output_tokens"], "8192.0");
    let wait = tool.prepare_arguments(json!({"action":"wait","cell_id":"00123","cursor":"9007199254740993","yield_time_ms":"3e4","max_output_tokens":"8192","continue_execution":false})).unwrap();
    assert_eq!(wait["cursor"], json!(9007199254740993_u64));
    assert_eq!(wait["cell_id"], "00123");
    assert_eq!(wait["yield_time_ms"], 30000);
    assert_eq!(wait["continue_execution"], false);
    assert!(validator.validate(&wait).is_ok());
    for value in ["-1", "8192.5", "18446744073709551616", "NaN", "", "true"] {
        let arguments = json!({"action":"exec","script":"return 1","yield_time_ms":0,"max_output_tokens":value});
        assert!(
            tool.prepare_arguments(arguments).is_err(),
            "accepted {value}"
        );
    }
    for (field, value) in [
        ("yield_time_ms", "30001"),
        ("max_output_tokens", "1023"),
        ("max_output_tokens", "131073"),
    ] {
        let mut arguments =
            json!({"action":"exec","script":"return 1","yield_time_ms":0,"max_output_tokens":1024});
        arguments[field] = json!(value);
        let prepared = tool.prepare_arguments(arguments).unwrap();
        assert!(
            validator.validate(&prepared).is_err(),
            "accepted {prepared}"
        );
    }
    for arguments in [
        json!({"action":"exec","script":"return 1","yield_time_ms":-1,"max_output_tokens":1024}),
        json!({"action":"exec","script":"return 1","yield_time_ms":30001,"max_output_tokens":1024}),
        json!({"action":"exec","script":"return 1","yield_time_ms":0,"max_output_tokens":1023}),
        json!({"action":"exec","script":"return 1","yield_time_ms":true,"max_output_tokens":1024}),
        json!({"action":"wait","cell_id":"x","cursor":"0","yield_time_ms":0,"max_output_tokens":1024}),
        json!({"action":"wait","cell_id":"x","cursor":0,"yield_time_ms":0,"max_output_tokens":1024,"continue_execution":"true"}),
    ] {
        assert!(
            validator.validate(&arguments).is_err(),
            "accepted {arguments}"
        );
    }
    assert!(validator.validate(&json!({"action":"wait","cell_id":"x","cursor":0,"yield_time_ms":0,"max_output_tokens":1024,"continue_execution":false})).is_ok());
    assert!(validator.validate(&json!({"script":"return 1"})).is_err());
    assert!(validator.validate(&json!({"action":"exec","script":"return 1","yield_time_ms":0,"max_output_tokens":1024,"session_id":"other"})).is_err());
    assert!(
        validator
            .validate(&json!({"action":"cancel","cell_id":"x"}))
            .is_err()
    );
    cells.close().await;
}

struct Reads {
    authorized: AtomicUsize,
    executed: AtomicUsize,
    version: AtomicUsize,
    deny: AtomicBool,
    unknown: AtomicBool,
    authorize_block: AtomicBool,
    block: AtomicBool,
    entered: Semaphore,
    released: Semaphore,
}
impl Default for Reads {
    fn default() -> Self {
        Self {
            authorized: AtomicUsize::new(0),
            executed: AtomicUsize::new(0),
            version: AtomicUsize::new(1),
            deny: AtomicBool::new(false),
            unknown: AtomicBool::new(false),
            authorize_block: AtomicBool::new(false),
            block: AtomicBool::new(false),
            entered: Semaphore::new(0),
            released: Semaphore::new(0),
        }
    }
}
#[async_trait]
impl support::FixtureRead for Reads {
    async fn admit(
        &self,
        _identity: &CellIdentity,
        _arguments: &Value,
        cancel: CancellationToken,
    ) -> Result<ReadStamp, CapabilityError> {
        self.authorized.fetch_add(1, Ordering::SeqCst);
        if self.authorize_block.load(Ordering::Acquire) {
            self.entered.add_permits(1);
            cancel.cancelled().await;
            return Err(CapabilityError::new("authorization cancelled"));
        }
        if self.deny.load(Ordering::Acquire) {
            return Err(CapabilityError::new("host gate denied"));
        }
        Ok(ReadStamp {
            workspace: "fixture-held-root".into(),
            incarnation: "fixture-incarnation".into(),
            generation: self.version.load(Ordering::SeqCst) as u64,
            semantics: "fixture-exact-read".into(),
        })
    }
    async fn capture(
        &self,
        stamp: &ReadStamp,
        cancel: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        self.executed.fetch_add(1, Ordering::SeqCst);
        self.entered.add_permits(1);
        if self.block.load(Ordering::Acquire) {
            tokio::select! {
                _ = cancel.cancelled() => return Err(CapabilityError::new("read cancelled")),
                permit = self.released.acquire() => permit.unwrap().forget(),
            }
        }
        if self.unknown.load(Ordering::Acquire) {
            return Err(CapabilityError::unknown(
                "read backend lost its observation",
            ));
        }
        Ok(json!({"version":stamp.generation.to_string()}))
    }
}

#[tokio::test]
async fn trusted_read_reuse_authorizes_each_caller_and_write_invalidates_before_dispatch() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(SqliteJournal::open(dir.path().join("cells.db")).unwrap());
    let host = Arc::new(Host::default());
    let reads = Arc::new(Reads::default());
    let limits = CellLimits::default();
    let cells = shared_manager(journal, host.clone(), reads.clone(), limits, "session");
    let done = terminal(
        &cells,
        cells
            .exec_fixture(
                exec("cayw.call('read',{b=2,a=1}); cayw.call('read',{a=1,b=2})"),
                CancellationToken::new(),
            )
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(done.status, CellStatus::Completed);
    assert_eq!(reads.authorized.load(Ordering::SeqCst), 2);
    assert_eq!(reads.executed.load(Ordering::SeqCst), 1);
    reads.deny.store(true, Ordering::Release);
    let denied = terminal(
        &cells,
        cells
            .exec_fixture(
                exec("cayw.call('read',{a=1,b=2})"),
                CancellationToken::new(),
            )
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(denied.status, CellStatus::Failed);
    assert_eq!(reads.executed.load(Ordering::SeqCst), 1);
    reads.deny.store(false, Ordering::Release);
    let done = terminal(
        &cells,
        cells
            .exec_fixture(
                exec("cayw.call('write',{}); cayw.call('read',{a=1,b=2})"),
                CancellationToken::new(),
            )
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(done.status, CellStatus::Completed);
    assert_eq!(reads.executed.load(Ordering::SeqCst), 2);
    reads.version.store(2, Ordering::Release);
    terminal(
        &cells,
        cells
            .exec_fixture(
                exec("cayw.call('read',{a=1,b=2})"),
                CancellationToken::new(),
            )
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(reads.executed.load(Ordering::SeqCst), 3);
    assert_eq!(host.calls.load(Ordering::SeqCst), 1);
    assert!(
        cells.record(&done.cell_id).unwrap().calls[1]
            .dependencies
            .is_some()
    );
    cells.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn simultaneous_reads_coalesce_but_ordinary_host_calls_never_do() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(SqliteJournal::open(dir.path().join("cells.db")).unwrap());
    let host = Arc::new(Host::default());
    let reads = Arc::new(Reads::default());
    reads.block.store(true, Ordering::Release);
    let limits = CellLimits::default();
    let cells = shared_manager(journal, host, reads.clone(), limits, "session");
    let first = cells
        .exec_fixture(
            ExecRequest {
                yield_time_ms: 1,
                ..exec("cayw.call('read',{})")
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    reads.entered.acquire().await.unwrap().forget();
    let second = cells
        .exec_fixture(
            ExecRequest {
                yield_time_ms: 1,
                ..exec("cayw.call('read',{})")
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while reads.authorized.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(reads.executed.load(Ordering::SeqCst), 1);
    reads.released.add_permits(1);
    assert_eq!(terminal(&cells, first).await.status, CellStatus::Completed);
    assert_eq!(terminal(&cells, second).await.status, CellStatus::Completed);
    cells.close().await;
    let (_dir, _journal, host, cells) = setup();
    terminal(
        &cells,
        cells
            .exec_fixture(
                exec("cayw.call('read',{}); cayw.call('read',{})"),
                CancellationToken::new(),
            )
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(host.calls.load(Ordering::SeqCst), 2);
    cells.close().await;
}

#[tokio::test]
async fn restart_classifies_unstarted_interrupted_and_pending_across_tasks_without_replay() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cells.db");
    drop(SqliteJournal::open(&path).unwrap());
    // Explicit persisted crash fixture: no VM/process recovery is fabricated.
    let db = rusqlite::Connection::open(&path).unwrap();
    for (id, task, status, pending) in [
        ("not-started", "task", CellStatus::NotStarted, false),
        ("lost-stack", "task", CellStatus::Yielded, false),
        ("pending-effect", "another-task", CellStatus::Running, true),
    ] {
        let mut owner = identity("session");
        owner.task_id = task.into();
        let record = CellRecord {
            cell_id: id.into(),
            identity: owner,
            source: "cayw.call('write', {})".into(),
            source_hash: "fixture-source-hash".into(),
            parent_call_id: "fixture-parent".into(),
            status,
            calls: if pending {
                vec![CapabilityIntent {
                    sequence: 1,
                    name: "write".into(),
                    arguments: json!({}),
                    dependencies: None,
                    status: IntentStatus::Pending,
                    result: None,
                    result_omitted: false,
                    result_hash: None,
                    error: None,
                }]
            } else {
                Vec::new()
            },
            outputs: Vec::new(),
            output_bytes: 0,
            error: None,
            stop: None,
            backend_error: None,
            reconciliation: Vec::new(),
            host_settlements: Vec::new(),
            revision: 1,
        };
        db.execute(
            "INSERT INTO cells_v1(id,session,task,revision,body) VALUES(?1,?2,?3,1,?4)",
            rusqlite::params![id, "session", task, serde_json::to_string(&record).unwrap()],
        )
        .unwrap();
    }
    drop(db);
    let journal = Arc::new(SqliteJournal::open(&path).unwrap());
    // Merely opening storage must not rewrite potentially live records.
    assert!(!journal.is_frozen("session").unwrap());
    for task in ["task", "another-task"] {
        let mut owner = identity("session");
        owner.task_id = task.into();
        journal
            .recover_stopped(
                &ExecutorStopped {
                    identity: owner,
                    evidence: "fixture executor was never running".into(),
                },
                &CellLimits::default(),
            )
            .unwrap();
    }
    assert!(journal.is_frozen("session").unwrap());
    let host = Arc::new(Host::default());
    let limits = CellLimits::default();
    let cells = manager(
        journal,
        host.clone(),
        ReadCoordinator::new(vec![], limits.clone()).unwrap(),
        limits,
        "session",
    );
    assert_eq!(
        cells.record("not-started").unwrap().status,
        CellStatus::NotStarted
    );
    assert_eq!(
        cells.record("lost-stack").unwrap().status,
        CellStatus::Interrupted
    );
    let page = cells
        .wait(
            WaitRequest {
                cell_id: "not-started".into(),
                cursor: 0,
                yield_time_ms: 0,
                max_output_tokens: 4096,
                continue_execution: true,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(page.status, CellStatus::NotStarted);
    assert_eq!(host.calls.load(Ordering::SeqCst), 0);
    assert!(matches!(
        cells
            .exec_fixture(exec("text('blocked')"), CancellationToken::new())
            .await,
        Err(CellError::Frozen)
    ));
    cells.close().await;
}

#[tokio::test]
async fn continuous_exec_wait_paginate_and_release_reclaims_runtime_capacity_without_dropping_history()
 {
    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(SqliteJournal::open(dir.path().join("cells.db")).unwrap());
    let host = Arc::new(Host::default());
    let limits = CellLimits {
        max_running: 1,
        max_cells: 16,
        output_items: 10,
        ..CellLimits::default()
    };
    let session = "session-continuous";
    let id = identity(session);
    let reads = ReadCoordinator::new(vec![], limits.clone()).unwrap();
    let cells = manager(
        journal.clone(),
        host.clone(),
        reads,
        limits.clone(),
        session,
    );

    // Run 5 sequential execution rounds on a manager configured with max_running = 1.
    // If runtime execution capacity (semaphore permit) leaked or was not reclaimed
    // after termination, iteration 1..4 would immediately fail with MaxRunning.
    let iterations = 5;
    for i in 0..iterations {
        let script = format!(
            r#"
            local key = 'ref_entry_{i}'
            cayw.call('cells.store', {{ key = key, expected_version = 0, value = {{ index = {i}, text = 'payload' }} }})
            text('output-item-1-for-{i}')
            text('output-item-2-for-{i}')
            text('output-item-3-for-{i}')
            local loaded = cayw.call('cells.load', {{ key = key }})
            cayw.call('cells.store', {{ key = key, expected_version = 1, value = cayw.null }})
            return {{ round = {i}, loaded_index = loaded.index }}
            "#
        );

        let initial_page = cells
            .exec_fixture(
                ExecRequest {
                    script,
                    yield_time_ms: 0,
                    max_output_tokens: 1024,
                },
                CancellationToken::new(),
            )
            .await
            .unwrap();

        // Paginate output incrementally from cursor 0 using small token budget
        let mut cursor = initial_page.next_cursor;
        let mut collected_texts = Vec::new();
        for item in &initial_page.outputs {
            if let CellOutput::Text { text } = &item.output {
                collected_texts.push(text.clone());
            }
        }
        let mut current_page = initial_page;

        while !current_page.status.is_terminal() || current_page.vm_alive {
            let wait_req = WaitRequest {
                cell_id: current_page.cell_id.clone(),
                cursor,
                yield_time_ms: 5000,
                max_output_tokens: 1024,
                continue_execution: false,
            };
            current_page = cells
                .wait(wait_req, CancellationToken::new())
                .await
                .unwrap();
            for item in &current_page.outputs {
                if let CellOutput::Text { text } = &item.output {
                    collected_texts.push(text.clone());
                }
            }
            if current_page.next_cursor > cursor {
                cursor = current_page.next_cursor;
            }
        }

        let record = cells.record(&current_page.cell_id).unwrap();
        assert_eq!(
            current_page.status,
            CellStatus::Completed,
            "round {i} failed: {record:?}"
        );
        assert!(!current_page.vm_alive);
        assert!(
            collected_texts.contains(&format!("output-item-1-for-{i}")),
            "round {i} text 1 missing"
        );
        assert!(
            collected_texts.contains(&format!("output-item-2-for-{i}")),
            "round {i} text 2 missing"
        );
        assert!(
            collected_texts.contains(&format!("output-item-3-for-{i}")),
            "round {i} text 3 missing"
        );

        // Replay full output via pagination from cursor 0
        let replay_page = cells
            .wait(
                WaitRequest {
                    cell_id: current_page.cell_id.clone(),
                    cursor: 0,
                    yield_time_ms: 0,
                    max_output_tokens: 128 * 1024,
                    continue_execution: false,
                },
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(replay_page.outputs.len(), 4); // 3 text outputs + 1 returned json output

        // Verify the record is durable and succeeded
        let record = cells.record(&current_page.cell_id).unwrap();
        assert_eq!(record.status, CellStatus::Completed);
    }

    // Crucial check: distinguish runtime slot quota (max_running: 1) from persistent history (max_cells: 16).
    // All 5 cell records must exist in full fidelity in the SQLite journal.
    let records = journal.records(&id, &limits).unwrap();
    assert_eq!(
        records.len(),
        iterations,
        "Every execution must retain its complete persistent history; no records may be dropped to reclaim capacity"
    );
    for rec in &records {
        assert_eq!(rec.status, CellStatus::Completed);
        assert_eq!(rec.outputs.len(), 4); // 3 texts + 1 return value
        assert_eq!(rec.calls.len(), 3); // 1 store + 1 load + 1 release store
    }

    cells.close().await;
}

#[tokio::test]
async fn dropping_an_invocation_future_cancels_its_cell_and_bounds_its_task() {
    let (_dir, journal, host, cells) = setup();
    let mut execution =
        Box::pin(cells.exec_fixture(exec("cayw.call('block', {})"), CancellationToken::new()));
    tokio::select! {
        biased;
        result = &mut execution => panic!("expected blocked execution, got {result:?}"),
        _ = wait_entered(&host) => {},
    }
    drop(execution);
    tokio::time::timeout(Duration::from_secs(5), async {
        while !host.dropped.load(Ordering::Acquire) || !journal.is_frozen("session").unwrap() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(journal.is_frozen("session").unwrap());
    assert!(host.dropped.load(Ordering::Acquire));
    cells.close().await;
}

#[tokio::test]
async fn sandbox_has_no_io_or_global_override_and_catalog_schemas_are_demanded() {
    let (_dir, _journal, host, cells) = setup();
    let page = cells.exec_fixture(exec("if io or os or require or getfenv or setfenv then error('unsafe globals') end; local d=toolDefinition('write'); text(d.name); local names=tool_search('read'); text(names[1].name); text(tostring(#ALL_TOOLS))"),CancellationToken::new()).await.unwrap();
    let done = terminal(&cells, page).await;
    assert_eq!(
        done.status,
        CellStatus::Completed,
        "{:?}",
        cells.record(&done.cell_id).unwrap()
    );
    assert_eq!(host.calls.load(Ordering::SeqCst), 0);
    for source in [
        "_G.cayw = {}; cayw.call('write', {})",
        "cayw.call = function() end; text('bad')",
    ] {
        let done = terminal(
            &cells,
            cells
                .exec_fixture(exec(source), CancellationToken::new())
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(
            done.status,
            CellStatus::Failed,
            "{source}: {:?}",
            cells.record(&done.cell_id).unwrap()
        );
    }
    assert_eq!(host.calls.load(Ordering::SeqCst), 0);
    cells.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_during_coalesced_read_rejects_stale_result_for_both_callers() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(SqliteJournal::open(dir.path().join("cells.db")).unwrap());
    let host = Arc::new(Host::default());
    let reads = Arc::new(Reads::default());
    reads.block.store(true, Ordering::Release);
    let limits = CellLimits::default();
    let cells = shared_manager(journal, host.clone(), reads.clone(), limits, "session");
    let first = cells
        .exec_fixture(
            ExecRequest {
                yield_time_ms: 1,
                ..exec("cayw.call('read',{}); cayw.call('write',{})")
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    reads.entered.acquire().await.unwrap().forget();
    let second = cells
        .exec_fixture(
            ExecRequest {
                yield_time_ms: 1,
                ..exec("cayw.call('read',{}); cayw.call('write',{})")
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while reads.authorized.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let writer = terminal(
        &cells,
        cells
            .exec_fixture(exec("cayw.call('write',{})"), CancellationToken::new())
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(writer.status, CellStatus::Completed);
    reads.released.add_permits(1);
    assert_eq!(terminal(&cells, first).await.status, CellStatus::Failed);
    assert_eq!(terminal(&cells, second).await.status, CellStatus::Failed);
    assert_eq!(host.calls.load(Ordering::SeqCst), 1);
    cells.close().await;
}

#[tokio::test]
async fn unknown_read_result_freezes_and_is_not_retained_for_reuse() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(SqliteJournal::open(dir.path().join("cells.db")).unwrap());
    let host = Arc::new(Host::default());
    let reads = Arc::new(Reads::default());
    reads.unknown.store(true, Ordering::Release);
    let limits = CellLimits::default();
    let cells = shared_manager(journal.clone(), host, reads.clone(), limits, "session");
    let done = terminal(
        &cells,
        cells
            .exec_fixture(exec("cayw.call('read',{})"), CancellationToken::new())
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(done.status, CellStatus::OutcomeUnknown);
    assert!(journal.is_frozen("session").unwrap());
    assert!(matches!(
        cells
            .exec_fixture(exec("cayw.call('read',{})"), CancellationToken::new())
            .await,
        Err(CellError::Frozen)
    ));
    let record = cells.record(&done.cell_id).unwrap();
    cells
        .reconcile(
            &done.cell_id,
            ReconciliationEvidence {
                source_hash: record.source_hash,
                call_sequence: 1,
                expected_arguments: json!({}),
                observed_result: json!({"version":"1"}),
                evidence: "trusted backend receipt 42".into(),
                resolution: Resolution::Succeeded,
            },
        )
        .unwrap();
    reads.unknown.store(false, Ordering::Release);
    let next = terminal(
        &cells,
        cells
            .exec_fixture(exec("cayw.call('read',{})"), CancellationToken::new())
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(next.status, CellStatus::Completed);
    assert_eq!(reads.executed.load(Ordering::SeqCst), 2);
    cells.close().await;
}

#[tokio::test]
async fn cancellation_during_independent_authorization_has_a_durable_intent() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(SqliteJournal::open(dir.path().join("cells.db")).unwrap());
    let reads = Arc::new(Reads::default());
    reads.authorize_block.store(true, Ordering::Release);
    let limits = CellLimits::default();
    let cells = shared_manager(
        journal.clone(),
        Arc::new(Host::default()),
        reads.clone(),
        limits,
        "session",
    );
    let page = cells
        .exec_fixture(
            ExecRequest {
                yield_time_ms: 1,
                ..exec("cayw.call('read',{})")
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    reads.entered.acquire().await.unwrap().forget();
    let done = cells.cancel(&page.cell_id).await.unwrap();
    assert_eq!(done.status, CellStatus::Cancelled);
    assert_eq!(
        cells.record(&done.cell_id).unwrap().calls[0].status,
        IntentStatus::Failed
    );
    assert!(!journal.is_frozen("session").unwrap());
    assert_eq!(reads.executed.load(Ordering::SeqCst), 0);
    cells.close().await;
}

#[tokio::test]
async fn vm_instruction_budget_and_cell_script_budget_stop_execution() {
    let dir = tempfile::tempdir().unwrap();
    let limits = CellLimits::default();
    let host = Arc::new(Host::default());
    let cells = CellManager::new(
        identity("session"),
        Arc::new(SqliteJournal::open(dir.path().join("cells.db")).unwrap()),
        backend(host.clone()),
        ReadCoordinator::new(vec![], limits.clone()).unwrap(),
        limits.clone(),
        Limits {
            instruction_budget: 128,
            ..Limits::default()
        },
    )
    .unwrap();
    assert!(matches!(
        cells
            .exec_fixture(
                exec(&"x".repeat(limits.script_bytes + 1)),
                CancellationToken::new()
            )
            .await,
        Err(CellError::Invalid(_))
    ));
    let done = terminal(
        &cells,
        cells
            .exec_fixture(
                exec("while true do end; cayw.call('write', {})"),
                CancellationToken::new(),
            )
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(done.status, CellStatus::Failed);
    assert_eq!(host.calls.load(Ordering::SeqCst), 0);
    cells.close().await;
}

#[tokio::test]
async fn manager_ownership_and_session_task_lookup_are_enforced() {
    let (_dir, journal, host, cells) = setup();
    let limits = CellLimits::default();
    let other = manager(
        journal.clone(),
        host.clone(),
        ReadCoordinator::new(vec![], limits.clone()).unwrap(),
        limits.clone(),
        "other",
    );
    let page = cells
        .exec_fixture(exec("return 1"), CancellationToken::new())
        .await
        .unwrap();
    assert!(matches!(
        other.record(&page.cell_id),
        Err(CellError::NotFound)
    ));
    assert!(
        CellManager::new(
            identity("session"),
            journal,
            backend(host),
            ReadCoordinator::new(vec![], limits.clone()).unwrap(),
            limits,
            Limits::default()
        )
        .is_err()
    );
    assert_ne!(
        page.cell_id,
        other
            .exec_fixture(exec("return 1"), CancellationToken::new())
            .await
            .unwrap()
            .cell_id
    );
    cells.close().await;
    other.close().await;
}
