use cayw_ai::{
    AssistantEvent, AssistantMessage, AssistantStream, CancellationToken, Content, Message, Model,
    ModelRequest, Provider, StopReason, ToolCall,
};
use cayw_cells::{CellPage, CellStatus, IntentStatus, SqliteJournal};
use cayw_core::{CapabilityOutcome, CoreEvent};
use cayw_runtime::{ApprovalChoice, Runtime, RuntimeConfig};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::{mpsc, oneshot};

struct Round {
    request: ModelRequest,
    reply: oneshot::Sender<AssistantMessage>,
}
struct Controlled(mpsc::UnboundedSender<Round>);
impl Provider for Controlled {
    fn stream(&self, request: ModelRequest, cancel: CancellationToken) -> AssistantStream {
        let model = request.model.clone();
        let (reply, answer) = oneshot::channel();
        self.0.send(Round { request, reply }).unwrap();
        Box::pin(futures::stream::once(async move {
            tokio::select! {biased;
                _=cancel.cancelled()=>AssistantEvent::Error{message:AssistantMessage::new(&model).failure(StopReason::Aborted,"fixture cancelled")},
                result=answer=>AssistantEvent::Done{message:result.unwrap()},
            }
        }))
    }
}
async fn next(rx: &mut mpsc::UnboundedReceiver<Round>) -> Round {
    tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .unwrap()
        .unwrap()
}
fn tool(round: Round, id: &str, args: Value) {
    let mut reply = AssistantMessage::new(&round.request.model);
    reply
        .content
        .push(Content::ToolCall(ToolCall::new(id, "execute_code", args)));
    reply.stop_reason = StopReason::ToolUse;
    round.reply.send(reply).unwrap();
}
fn checked_page(round: &Round, failed: bool) -> CellPage {
    let Some(Message::ToolResult(result)) = round.request.messages.last() else {
        panic!("tool result required")
    };
    assert_eq!(result.is_error, failed, "{result:?}");
    assert!(!result.outcome_unknown, "{result:?}");
    serde_json::from_value(result.structured_content.clone().unwrap()).unwrap()
}
fn page(round: &Round) -> CellPage {
    checked_page(round, false)
}
fn finish(round: Round) {
    let mut reply = AssistantMessage::new(&round.request.model);
    reply.content.push(Content::text("done"));
    reply.stop_reason = StopReason::Stop;
    round.reply.send(reply).unwrap();
}
async fn approve(rt: &Runtime, id: &str) {
    let request = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(request) = rt.get(id).unwrap().pending_approvals.first() {
                break request.clone();
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    rt.approve(id, &request.id, ApprovalChoice::AllowOnce, "fixture-user")
        .unwrap();
}

// Skipping Backend on a ready hit loses the second approval/Core origin; publishing
// without the Cells commit loses exact dependencies. Both are checked here.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_original_cells_pay_two_grants_and_keep_exact_dependencies_for_one_capture() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("a.txt"), "frozen").unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut config = RuntimeConfig::default();
    config.host.frozen_reads = true;
    let rt = Runtime::open(
        dir.path().join("session.db"),
        &root,
        Arc::new(Controlled(tx)),
        Model::new("fixture", "shared-read"),
        config.clone(),
    )
    .unwrap();
    let id = rt.create(None).unwrap().id;
    let run = {
        let rt = rt.clone();
        let id = id.clone();
        tokio::spawn(async move { rt.prompt(&id, "two reads", CancellationToken::new()).await })
    };
    let script = "return cayw.call('fs.read',{path='a.txt'})";
    let fence = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join(".cayw-host-read-fence"))
        .unwrap();
    fence.lock().unwrap();
    let mut cells = Vec::new();
    let mut round = next(&mut rx).await;
    for (index, parent) in ["parent-a", "parent-b"].into_iter().enumerate() {
        tool(
            round,
            parent,
            json!({"action":"exec","script":script,"yield_time_ms":0,"max_output_tokens":4096}),
        );
        round = next(&mut rx).await;
        let launched = page(&round);
        cells.push(launched.cell_id.clone());
        approve(&rt, &id).await;
        tokio::time::timeout(Duration::from_secs(10), async {loop {
            if rt.events(&id,0,1000).unwrap().iter().filter(|e|matches!(&e.event,CoreEvent::CapabilityStarted{call} if call.name=="fs.read")).count() == index + 1 {break;}
            tokio::task::yield_now().await;
        }}).await.unwrap();
    }
    assert!(
        !rt.events(&id, 0, 1000)
            .unwrap()
            .iter()
            .any(|e| matches!(&e.event,CoreEvent::Host{kind,..} if kind=="runtime_shared_read"))
    );
    drop(fence);
    for cell in &cells {
        tool(
            round,
            &format!("wait-{cell}"),
            json!({"action":"wait","cell_id":cell,"cursor":0,"yield_time_ms":30000,"max_output_tokens":4096,"continue_execution":false}),
        );
        round = next(&mut rx).await;
        assert_eq!(page(&round).status, CellStatus::Completed);
    }
    finish(round);
    run.await.unwrap().unwrap();
    assert_ne!(cells[0], cells[1]);
    assert_eq!(rt.grants(&id).unwrap().len(), 2);
    let events = rt.events(&id, 0, 1000).unwrap();
    let calls: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.event {
            CoreEvent::CapabilitySettled { call } => Some(call),
            _ => None,
        })
        .collect();
    assert_eq!(calls.len(), 2);
    let parents: std::collections::BTreeSet<_> = calls
        .iter()
        .map(|call| call.parent_call_id.as_str())
        .collect();
    assert_eq!(
        parents,
        std::collections::BTreeSet::from(["parent-a", "parent-b"])
    );
    assert!(
        calls.iter().all(|c| c.key.sequence == 0
            && matches!(c.outcome, Some(CapabilityOutcome::Completed { .. })))
    );
    let reads: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.event {
            CoreEvent::Host { kind, data } if kind == "runtime_shared_read" => Some(data),
            _ => None,
        })
        .collect();
    assert_eq!(
        reads.len(),
        2,
        "every original read needs canonical evidence"
    );
    assert_eq!(reads[0]["role"], "leader");
    assert!(reads[1]["role"] == "follower" || reads[1]["role"] == "ready");
    assert_eq!(
        reads.iter().filter(|read| read["role"] == "leader").count(),
        1
    );
    assert_eq!(reads[0]["dependencies"], reads[1]["dependencies"]);
    assert_eq!(reads[0]["source"], reads[1]["source"]);
    assert_ne!(reads[0]["audit_id"], reads[1]["audit_id"]);
    let journal = SqliteJournal::open(dir.path().join("session.db")).unwrap();
    let identity = cayw_cells::CellIdentity {
        session_id: id,
        task_id: calls[0].task_id.clone(),
        attempt_id: calls[0].lease.attempt_id.clone(),
        permission_context: format!("{}:{}", calls[0].lease.worker_id, calls[0].lease.fence),
    };
    let records = journal.records(&identity, &config.cells).unwrap();
    assert_eq!(records.len(), 2);
    for record in records {
        assert_eq!(record.calls[0].status, IntentStatus::Succeeded);
        let dep = record.calls[0].dependencies.as_ref().unwrap();
        assert!(dep.fingerprint.starts_with("sha256:"));
        assert!(dep.versions.contains_key("generation"));
    }
}

// Explicit cancellation and dropping the prompt both stop the original admitted
// fence wait. No background capture may outlive the stopped invocation.
#[tokio::test]
async fn cancelled_or_dropped_prompt_stops_two_original_read_waits_without_capture_or_replay() {
    for drop_prompt in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("a.txt"), "frozen").unwrap();
        let path = dir.path().join("session.db");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut config = RuntimeConfig::default();
        config.host.frozen_reads = true;
        let rt = Runtime::open(
            &path,
            &root,
            Arc::new(Controlled(tx)),
            Model::new("fixture", "cancel-original-read"),
            config.clone(),
        )
        .unwrap();
        let id = rt.create(None).unwrap().id;
        let cancel = CancellationToken::new();
        let worker = {
            let rt = rt.clone();
            let id = id.clone();
            let cancel = cancel.clone();
            tokio::spawn(async move { rt.prompt(&id, "two cancelled reads", cancel).await })
        };
        let fence = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join(".cayw-host-read-fence"))
            .unwrap();
        fence.lock().unwrap();
        let mut round = next(&mut rx).await;
        for index in 0..2 {
            tool(
                round,
                &format!("cancel-parent-{index}"),
                json!({"action":"exec","script":"return cayw.call('fs.read',{path='a.txt'})","yield_time_ms":0,"max_output_tokens":4096}),
            );
            round = next(&mut rx).await;
            assert!(!page(&round).status.is_terminal());
            approve(&rt, &id).await;
            tokio::time::timeout(Duration::from_secs(10), async { loop {
                if rt.events(&id,0,1000).unwrap().iter().filter(|e|matches!(&e.event,CoreEvent::CapabilityStarted{call} if call.name=="fs.read")).count()==index+1 { break; }
                tokio::task::yield_now().await;
            }}).await.unwrap();
        }
        if drop_prompt {
            worker.abort();
            assert!(worker.await.unwrap_err().is_cancelled());
        } else {
            cancel.cancel();
            let _ = worker.await.unwrap();
        }
        drop(round);
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if rt.get(&id).unwrap().attempt.is_none() && rt.health().unwrap().running == 0 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        drop(fence);
        let events = rt.events(&id, 0, 1000).unwrap();
        assert_eq!(rt.grants(&id).unwrap().len(), 2);
        assert!(
            !events.iter().any(
                |e| matches!(&e.event,CoreEvent::Host{kind,..} if kind=="runtime_shared_read")
            )
        );
        let calls: Vec<_> = events
            .iter()
            .filter_map(|e| match &e.event {
                CoreEvent::CapabilitySettled { call } if call.name == "fs.read" => Some(call),
                _ => None,
            })
            .collect();
        assert_eq!(calls.len(), 2);
        assert!(
            calls
                .iter()
                .all(|call| !matches!(call.outcome, Some(CapabilityOutcome::Completed { .. })))
        );
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "frozen"
        );
        let never = Arc::new(Script {
            source: "return 1",
            calls: AtomicUsize::new(0),
        });
        drop(rt);
        let rt = Runtime::open(
            &path,
            &root,
            never.clone(),
            Model::new("fixture", "no-replay"),
            config,
        )
        .unwrap();
        rt.recover(&id).unwrap();
        assert_eq!(never.calls.load(Ordering::SeqCst), 0);
        assert!(
            !rt.events(&id, 0, 1000).unwrap().iter().any(
                |e| matches!(&e.event,CoreEvent::Host{kind,..} if kind=="runtime_shared_read")
            )
        );
    }
}

// Expire only after the second original Core start has committed, so the ready
// value reaches its original authority recheck. A confirmed rejection must commit
// its own failed Cells intent and leave the first capture available to a later
// independently approved call.
#[tokio::test]
async fn refused_ready_read_commits_known_failure_and_keeps_later_original_read_available() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("a.txt"), "frozen").unwrap();
    let path = dir.path().join("session.db");
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut config = RuntimeConfig::default();
    config.host.frozen_reads = true;
    let rt = Runtime::open(
        &path,
        &root,
        Arc::new(Controlled(tx)),
        Model::new("fixture", "known-read-refusal"),
        config.clone(),
    )
    .unwrap();
    let id = rt.create(None).unwrap().id;
    let run = {
        let rt = rt.clone();
        let id = id.clone();
        tokio::spawn(async move {
            rt.prompt(&id, "three original reads", CancellationToken::new())
                .await
        })
    };
    let db = rusqlite::Connection::open(&path).unwrap();
    let mut round = next(&mut rx).await;
    for index in 0..3 {
        tool(
            round,
            &format!("original-{index}"),
            json!({"action":"exec","script":"return cayw.call('fs.read',{path='a.txt'})","yield_time_ms":0,"max_output_tokens":4096}),
        );
        round = next(&mut rx).await;
        let launched = page(&round);
        if index == 1 {
            db.execute_batch("CREATE TRIGGER expire_original_read AFTER INSERT ON core_events WHEN json_extract(NEW.record,'$.event.type')='capability_started' BEGIN UPDATE runtime_grants SET expires=0 WHERE capability='fs.read'; END;").unwrap();
        }
        approve(&rt, &id).await;
        tool(
            round,
            &format!("wait-original-{index}"),
            json!({"action":"wait","cell_id":launched.cell_id,"cursor":0,"yield_time_ms":30000,"max_output_tokens":4096,"continue_execution":false}),
        );
        round = next(&mut rx).await;
        assert_eq!(
            checked_page(&round, index == 1).status,
            if index == 1 {
                CellStatus::Failed
            } else {
                CellStatus::Completed
            }
        );
        if index == 1 {
            db.execute_batch("DROP TRIGGER expire_original_read")
                .unwrap();
        }
    }
    finish(round);
    assert_eq!(
        run.await.unwrap().unwrap().state,
        cayw_core::SessionState::Idle
    );
    assert_eq!(rt.health().unwrap().running, 0);
    assert_eq!(rt.grants(&id).unwrap().len(), 3);
    let events = rt.events(&id, 0, 1000).unwrap();
    let calls: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.event {
            CoreEvent::CapabilitySettled { call } if call.name == "fs.read" => Some(call),
            _ => None,
        })
        .collect();
    assert_eq!(calls.len(), 3);
    assert!(matches!(
        calls[0].outcome,
        Some(CapabilityOutcome::Completed { .. })
    ));
    assert!(matches!(
        calls[1].outcome,
        Some(CapabilityOutcome::Failed { .. })
    ));
    assert!(matches!(
        calls[2].outcome,
        Some(CapabilityOutcome::Completed { .. })
    ));
    let reads: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.event {
            CoreEvent::Host { kind, data } if kind == "runtime_shared_read" => Some(data),
            _ => None,
        })
        .collect();
    assert_eq!(reads.len(), 2);
    assert_eq!(reads[0]["role"], "leader");
    assert_eq!(reads[1]["role"], "ready");
    assert_eq!(reads[0]["source"], reads[1]["source"]);
    assert_ne!(reads[0]["audit_id"], reads[1]["audit_id"]);
    let journal = SqliteJournal::open(&path).unwrap();
    let identity = cayw_cells::CellIdentity {
        session_id: id.clone(),
        task_id: calls[1].task_id.clone(),
        attempt_id: calls[1].lease.attempt_id.clone(),
        permission_context: format!("{}:{}", calls[1].lease.worker_id, calls[1].lease.fence),
    };
    let records = journal.records(&identity, &config.cells).unwrap();
    let rejected = records
        .iter()
        .find(|record| record.cell_id == calls[1].key.execution_id)
        .unwrap();
    assert_eq!(rejected.calls[0].status, IntentStatus::Failed);
    assert!(rejected.calls[0].dependencies.is_none());
    assert!(!journal.is_frozen(&id).unwrap());
    rt.recover(&id).unwrap();
}

struct Script {
    source: &'static str,
    calls: AtomicUsize,
}
impl Provider for Script {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let message =
            cayw_runtime::fixture::script_response(&request, self.source, &format!("parent-{n}"));
        Box::pin(futures::stream::iter([AssistantEvent::Done { message }]))
    }
}
fn open_script(dir: &tempfile::TempDir, provider: Arc<Script>, config: RuntimeConfig) -> Runtime {
    Runtime::open(
        dir.path().join("sessions.db"),
        dir.path().join("root"),
        provider,
        Model::new("fixture", "shared-read-recovery"),
        config,
    )
    .unwrap()
}

// The original read succeeded in Core; Cells rejects that exact terminal commit.
// Recovery must repair the saved dependency without opening a now-missing file.
#[tokio::test]
async fn original_cells_failure_recovers_exact_dependencies_without_reread_or_parent_guess() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("root")).unwrap();
    let path = dir.path().join("root/a.txt");
    std::fs::write(&path, "original bytes").unwrap();
    let provider = Arc::new(Script {
        source: "return cayw.call('fs.read',{path='a.txt'})",
        calls: AtomicUsize::new(0),
    });
    let mut config = RuntimeConfig::default();
    config.host.frozen_reads = true;
    config
        .permissions
        .push(cayw_runtime::ScopedPermission::exact(
            "fs.read",
            json!({"path":"a.txt"}),
        ));
    let rt = open_script(&dir, provider.clone(), config.clone());
    let id = rt.create(None).unwrap().id;
    let db = rusqlite::Connection::open(dir.path().join("sessions.db")).unwrap();
    db.execute_batch("CREATE TRIGGER reject_original_cells BEFORE UPDATE ON cells_v1 WHEN json_extract(NEW.body,'$.calls[0].status')='succeeded' BEGIN SELECT RAISE(ABORT,'original read Cells commit rejected'); END;").unwrap();
    assert!(
        rt.prompt(&id, "read once", CancellationToken::new())
            .await
            .is_err()
    );
    assert_eq!(rt.health().unwrap().running, 1);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    let events = rt.events(&id, 0, 1000).unwrap();
    let evidence = events
        .iter()
        .find_map(|e| match &e.event {
            CoreEvent::Host { kind, data } if kind == "runtime_shared_read" => Some(data.clone()),
            _ => None,
        })
        .unwrap();
    let known = events
        .iter()
        .find_map(|e| match &e.event {
            CoreEvent::CapabilitySettled { call }
                if matches!(call.outcome, Some(CapabilityOutcome::Completed { .. })) =>
            {
                Some(call.clone())
            }
            _ => None,
        })
        .unwrap();
    assert!(
        matches!(&known.outcome,Some(CapabilityOutcome::Completed{result}) if result["content"]=="original bytes")
    );
    std::fs::remove_file(&path).unwrap();
    db.execute_batch("DROP TRIGGER reject_original_cells;")
        .unwrap();
    drop(rt);
    let rt = open_script(&dir, provider.clone(), config);
    let recovered = rt.recover(&id).unwrap();
    assert_eq!(
        recovered.state,
        cayw_core::SessionState::NeedsReconciliation
    );
    assert_eq!(recovered.unresolved_calls.len(), 1);
    assert!(recovered.unresolved_capabilities.is_empty());
    assert!(rt.enqueue(&id, "parent still requires evidence").is_err());
    let body: String = db
        .query_row(
            "SELECT body FROM cells_v1 WHERE id=?1",
            [&known.key.execution_id],
            |r| r.get(0),
        )
        .unwrap();
    let record: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(record["calls"][0]["status"], "succeeded");
    assert_eq!(record["calls"][0]["dependencies"], evidence["dependencies"]);
    assert_eq!(record["calls"][0]["result"]["content"], "original bytes");
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert!(!path.exists());
    rt.recover(&id).unwrap();
    let retried: String = db
        .query_row(
            "SELECT body FROM cells_v1 WHERE id=?1",
            [&known.key.execution_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(retried, body);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
}

// Reject actual canonical writes at each original read boundary. No failed
// journal path may publish bytes or create a replacement Provider invocation.
#[tokio::test]
async fn original_read_canonical_write_failures_keep_unknown_parent_and_never_replay() {
    for stage in ["start", "evidence", "terminal"] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("root")).unwrap();
        let file = dir.path().join("root/a.txt");
        std::fs::write(&file, "original bytes").unwrap();
        let provider = Arc::new(Script {
            source: "return cayw.call('fs.read',{path='a.txt'})",
            calls: AtomicUsize::new(0),
        });
        let mut config = RuntimeConfig::default();
        config.host.frozen_reads = true;
        config
            .permissions
            .push(cayw_runtime::ScopedPermission::exact(
                "fs.read",
                json!({"path":"a.txt"}),
            ));
        let rt = open_script(&dir, provider.clone(), config.clone());
        let id = rt.create(None).unwrap().id;
        let db = rusqlite::Connection::open(dir.path().join("sessions.db")).unwrap();
        let condition = match stage {
            "start" => "json_extract(NEW.record,'$.event.type')='capability_started'",
            "evidence" => {
                "json_extract(NEW.record,'$.event.type')='host' AND json_extract(NEW.record,'$.event.kind')='runtime_shared_read'"
            }
            "terminal" => "json_extract(NEW.record,'$.event.type')='capability_settled'",
            _ => unreachable!(),
        };
        db.execute_batch(&format!("CREATE TRIGGER reject_original_read_canonical BEFORE INSERT ON core_events WHEN {condition} BEGIN SELECT RAISE(ABORT,'fixture original read canonical write rejected'); END;")).unwrap();
        let _ = rt
            .prompt(
                &id,
                "read with failed canonical record",
                CancellationToken::new(),
            )
            .await;
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        let view = rt.get(&id).unwrap();
        assert_eq!(
            view.state,
            cayw_core::SessionState::NeedsReconciliation,
            "{stage}"
        );
        assert_eq!(view.unresolved_calls.len(), 1, "{stage}");
        assert!(rt.enqueue(&id, "must inspect original parent").is_err());
        let events = rt.events(&id, 0, 1000).unwrap();
        assert!(!events.iter().any(|event| matches!(&event.event,CoreEvent::CapabilitySettled{call} if call.name=="fs.read"&&matches!(call.outcome,Some(CapabilityOutcome::Completed{..})))), "{stage}");
        let captures = events.iter().filter(|event| matches!(&event.event,CoreEvent::Host{kind,..} if kind=="runtime_shared_read")).count();
        assert_eq!(captures, usize::from(stage == "terminal"), "{stage}");
        db.execute_batch("DROP TRIGGER reject_original_read_canonical")
            .unwrap();
        std::fs::remove_file(&file).unwrap();
        drop(rt);
        let rt = open_script(&dir, provider.clone(), config);
        let recovered = rt.recover(&id).unwrap();
        assert_eq!(
            recovered.state,
            cayw_core::SessionState::NeedsReconciliation,
            "{stage}"
        );
        assert_eq!(recovered.unresolved_calls.len(), 1, "{stage}");
        assert!(rt.enqueue(&id, "do not replay").is_err());
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        assert!(!file.exists());
    }
}

#[tokio::test]
async fn warm_read_spends_each_original_runtime_call_budget() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("root")).unwrap();
    std::fs::write(dir.path().join("root/a.txt"), "frozen").unwrap();
    let provider = Arc::new(Script {
        source: "cayw.call('fs.read',{path='a.txt'}); return cayw.call('fs.read',{path='a.txt'})",
        calls: AtomicUsize::new(0),
    });
    let mut config = RuntimeConfig {
        max_capability_calls: 1,
        ..Default::default()
    };
    config.host.frozen_reads = true;
    config
        .permissions
        .push(cayw_runtime::ScopedPermission::exact(
            "fs.read",
            json!({"path":"a.txt"}),
        ));
    let rt = open_script(&dir, provider, config);
    let id = rt.create(None).unwrap().id;
    let _ = rt.prompt(&id, "budget", CancellationToken::new()).await;
    let events = rt.events(&id, 0, 1000).unwrap();
    let calls: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.event {
            CoreEvent::CapabilitySettled { call } => Some(call),
            _ => None,
        })
        .collect();
    assert_eq!(calls.len(), 2);
    assert!(matches!(
        calls[0].outcome,
        Some(CapabilityOutcome::Completed { .. })
    ));
    assert!(matches!(
        calls[1].outcome,
        Some(CapabilityOutcome::NotExecuted { .. })
    ));
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(&e.event,CoreEvent::Host{kind,..} if kind=="runtime_shared_read"))
            .count(),
        1
    );
}

// Fixed canonical fixtures use only the append-only Core SDK. Unlike the real
// crash/repair test above, these deliberately have no VM and no byte capture.
// Each altered event follows its matching start and precedes any Core terminal,
// so rejecting its fields cannot be masked by a duplicate/late-event rejection.
async fn rejected_evidence(field: &str, with_grant: bool) {
    use cayw_agent::AgentEvent;
    use cayw_core::{
        ApprovalDecision, CapabilityAuthorization, CapabilityCallKey, CapabilityInput,
        ExecutionInput,
    };
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("root")).unwrap();
    let provider = Arc::new(Script {
        source: "return 1",
        calls: AtomicUsize::new(0),
    });
    let mut config = RuntimeConfig::default();
    config.host.frozen_reads = true;
    let rt = open_script(&dir, provider.clone(), config.clone());
    let id = rt.create(None).unwrap().id;
    let store = rt.store();
    let lease = store.begin_attempt(&id, "fixture-worker").unwrap();
    store
        .append_agent_event(&lease, &AgentEvent::AgentStart)
        .unwrap();
    store
        .append_agent_event(&lease, &AgentEvent::TurnStart)
        .unwrap();
    let mut reply = AssistantMessage::new(&Model::new("fixture", "evidence"));
    reply.stop_reason = StopReason::ToolUse;
    let parent = ToolCall::new(
        "original-parent",
        "execute_code",
        json!({"action":"exec","script":"return cayw.call('fs.read',{path='a.txt'})"}),
    );
    reply.content.push(Content::ToolCall(parent.clone()));
    for event in [
        AgentEvent::MessageStart {
            message: Message::Assistant(reply.clone()),
        },
        AgentEvent::MessageEnd {
            message: Message::Assistant(reply),
        },
        AgentEvent::ToolExecutionStart {
            call: parent.clone(),
        },
    ] {
        store.append_agent_event(&lease, &event).unwrap();
    }
    store.mark_tool_started(&lease, &parent.id).unwrap();
    store
        .begin_execution(
            &lease,
            ExecutionInput {
                execution_id: "original-cell".into(),
                parent_call_id: parent.id.clone(),
                task_id: "original-task".into(),
                source_hash: "original-source".into(),
            },
        )
        .unwrap();
    let key = CapabilityCallKey {
        execution_id: "original-cell".into(),
        sequence: 0,
    };
    let arguments = json!({"path":"a.txt"});
    store
        .plan_capability(
            &lease,
            CapabilityInput {
                key: key.clone(),
                name: "fs.read".into(),
                arguments: arguments.clone(),
            },
        )
        .unwrap();
    let actor = if with_grant {
        "fixture-host"
    } else {
        "runtime-config"
    };
    let authorization = CapabilityAuthorization {
        actor: actor.into(),
        target: key.clone(),
        task_id: "original-task".into(),
        capability: "fs.read".into(),
        scope: arguments.to_string(),
        decision: ApprovalDecision::Approve,
        approval_id: None,
        external_provenance: None,
    };
    let grant = with_grant.then(|| cayw_runtime::GrantRecord {
        id: "original-grant".into(),
        session_id: id.clone(),
        attempt_id: lease.attempt_id.clone(),
        approval_id: "original-approval".into(),
        capability: "fs.read".into(),
        scope: arguments.to_string(),
        mode: "allow_task".into(),
        actor: actor.into(),
        expires_at: cayw_ai::timestamp_ms() + 300000,
        state: "active".into(),
        consumed: false,
    });
    if let Some(grant) = &grant {
        store.append_host_event(&lease,"permission_admitted",json!({"key":key,"task_id":"original-task","grant_id":grant.id,"approval_id":grant.approval_id,"grant":grant})).unwrap();
    }
    store
        .start_capability(&lease, &key, serde_json::to_value(&authorization).unwrap())
        .unwrap();
    let identity = cayw_cells::CellIdentity {
        session_id: id.clone(),
        task_id: "original-task".into(),
        attempt_id: lease.attempt_id.clone(),
        permission_context: format!("{}:{}", lease.worker_id, lease.fence),
    };
    let origin = cayw_cells::ReadOrigin {
        execution: cayw_cells::CellExecution {
            identity: identity.clone(),
            cell_id: key.execution_id.clone(),
            parent_call_id: parent.id,
            source_hash: "original-source".into(),
        },
        sequence: 1,
    };
    let hash = format!("sha256:{}", "a".repeat(64));
    let stamp =
        json!({"workspace":"fixture-root","incarnation":hash,"generation":0,"semantics":hash});
    let dependencies = json!({"fingerprint":hash,"permission_context":identity.permission_context,"versions":{"workspace":"fixture-root","incarnation":hash,"generation":"0","semantics":hash}});
    let mut data = json!({"origin":origin,"lease":lease,"key":key,"task_id":"original-task","name":"fs.read","arguments":arguments,"authorization":authorization,"grant":grant,"request":null,"audit_id":"original-cell:0","source":origin,"role":"leader","stamp":stamp,"dependencies":dependencies,"result_sha256":hash});
    match field {
        "lease" => data["lease"]["fence"] = json!(999999),
        "task" => data["task_id"] = json!("replacement-task"),
        "parent" => data["origin"]["execution"]["parent_call_id"] = json!("replacement-parent"),
        "source" => data["origin"]["execution"]["source_hash"] = json!("replacement-source"),
        "sequence" => data["origin"]["sequence"] = json!(0),
        "arguments" => data["arguments"]["path"] = json!("other.txt"),
        "authorization" => data["authorization"]["actor"] = json!("replacement-host"),
        "dependencies" => data["dependencies"]["versions"]["generation"] = json!("999"),
        "stamp" => data["stamp"]["workspace"] = json!("replacement-root"),
        "digest" => data["result_sha256"] = json!(format!("sha256:{}", "0".repeat(64))),
        "receipt" => data["request"] = json!({"request_id":"replacement-request"}),
        "role" => data["role"] = json!("follower"),
        "expires_at" => data["grant"]["expires_at"] = json!(0),
        "mode" => data["grant"]["mode"] = json!("replacement_mode"),
        "valid_expiry" => {
            data["grant"]["expires_at"] = json!(data["grant"]["expires_at"].as_u64().unwrap() + 1)
        }
        "valid_mode" => {
            data["grant"]["mode"] = json!("allow_once");
            data["grant"]["consumed"] = json!(true);
        }
        _ => unreachable!(),
    }
    store
        .append_host_event(&lease, "runtime_shared_read", data)
        .unwrap();
    let before = rt.get(&id).unwrap();
    assert!(rt.recover(&id).is_err(), "accepted altered {field}");
    assert_eq!(
        rt.get(&id).unwrap(),
        before,
        "mutated canonical history for {field}"
    );
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
}
#[tokio::test]
async fn altered_shared_read_evidence_is_rejected_before_canonical_history_changes() {
    for field in [
        "lease",
        "task",
        "parent",
        "source",
        "sequence",
        "arguments",
        "authorization",
        "dependencies",
        "stamp",
        "digest",
        "receipt",
        "role",
    ] {
        rejected_evidence(field, false).await;
    }
}
#[tokio::test]
async fn altered_original_grant_snapshot_is_rejected_before_canonical_history_changes() {
    for field in ["expires_at", "mode", "valid_expiry", "valid_mode"] {
        rejected_evidence(field, true).await;
    }
}

// Request tools use the same unclassified Host service boundary. Each later
// protected read retains its exact receipt and completes as an uncached call.
#[tokio::test]
async fn protected_reads_keep_each_original_receipt_through_uncached_host_calls() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("a.txt"), "frozen").unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut config = RuntimeConfig::default();
    config.host.frozen_reads = true;
    config
        .permissions
        .push(cayw_runtime::ScopedPermission::exact(
            "fs.read",
            json!({"path":"a.txt"}),
        ));
    config.request_policy = Some(cayw_runtime::RequestPolicy {
        permission_policy_id: "fixed-read-policy".into(),
        operations: vec![cayw_requests::OperationScope::exact(
            "read-exact",
            "fs.read",
            json!({"path":"a.txt"}),
            "workspace-policy",
        )],
    });
    let rt = Runtime::open(
        dir.path().join("session.db"),
        &root,
        Arc::new(Controlled(tx)),
        Model::new("fixture", "request-read"),
        config,
    )
    .unwrap();
    let id = rt.create(None).unwrap().id;
    let run = {
        let rt = rt.clone();
        let id = id.clone();
        tokio::spawn(async move {
            rt.prompt(&id, "protected reads", CancellationToken::new())
                .await
        })
    };
    let script = "local input={purpose='read the requested file',gap='exact read is unavailable',operation_id='read-exact',capability='fs.read',arguments={path='a.txt'}}; cayw.call('capability.request',input); cayw.call('capability.request',input); yield_control(); cayw.call('fs.read',{path='a.txt'}); return cayw.call('fs.read',{path='a.txt'})";
    tool(
        next(&mut rx).await,
        "request-parent",
        json!({"action":"exec","script":script,"yield_time_ms":30000,"max_output_tokens":4096}),
    );
    let round = next(&mut rx).await;
    let launched = page(&round);
    assert_eq!(launched.status, CellStatus::Yielded);
    let task = rt.tasks(&id).unwrap().remove(0);
    let request = rt
        .request_events(&task.id, 0, 64)
        .unwrap()
        .into_iter()
        .rev()
        .find_map(|e| {
            e.request_id
                .map(|id| rt.request_status(&task.id, &id).unwrap())
        })
        .unwrap();
    assert_eq!(
        request.state,
        cayw_requests::RequestState::AwaitingEvaluation
    );
    assert_eq!(request.valid_request_count, 2);
    let evaluation = rt
        .request_evaluate(&task.id, &request.id, "fixture-host")
        .unwrap();
    let held = dir.path().join("held-root");
    std::fs::rename(&root, &held).unwrap();
    rt.capabilities()
        .expect("catalog must retain its opened root");
    assert_eq!(rt.request_operations().unwrap().len(), 1);
    rt.request_approve(
        &task.id,
        &evaluation,
        Duration::from_secs(300),
        "original exact read receipt",
    )
    .unwrap();
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("a.txt"), "replacement").unwrap();
    tool(
        round,
        "continue-read",
        json!({"action":"wait","cell_id":launched.cell_id,"cursor":launched.next_cursor,"yield_time_ms":30000,"max_output_tokens":4096,"continue_execution":true}),
    );
    let round = next(&mut rx).await;
    assert_eq!(page(&round).status, CellStatus::Completed);
    finish(round);
    run.await.unwrap().unwrap();
    let events = rt.events(&id, 0, 1000).unwrap();
    let reads: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.event {
            CoreEvent::Host { kind, data } if kind == "runtime_shared_read" => Some(data),
            _ => None,
        })
        .collect();
    assert_eq!(reads.len(), 2);
    assert_eq!(reads[0]["role"], "uncached");
    assert_eq!(reads[1]["role"], "uncached");
    assert_eq!(reads[0]["source"], reads[0]["origin"]);
    assert_eq!(reads[1]["source"], reads[1]["origin"]);
    assert_eq!(reads[0]["request"]["request_id"], request.id);
    assert_eq!(reads[0]["request"], reads[1]["request"]);
    assert_eq!(reads[0]["origin"]["sequence"], 3);
    assert_eq!(reads[1]["origin"]["sequence"], 4);
    assert_eq!(
        std::fs::read_to_string(root.join("a.txt")).unwrap(),
        "replacement"
    );
    assert!(held.join(".cayw-host-read-state").exists());
    assert!(!root.join(".cayw-host-read-state").exists());
    for event in &events {
        if let CoreEvent::CapabilitySettled { call } = &event.event
            && call.name == "fs.read"
        {
            assert!(
                matches!(&call.outcome, Some(CapabilityOutcome::Completed { result }) if result["content"] == "frozen")
            );
        }
    }
    let admissions: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.event {
            CoreEvent::Host { kind, data } if kind == "task_request_admitted" => Some(data),
            _ => None,
        })
        .collect();
    assert_eq!(admissions.len(), 2);
    for (read, admission) in reads.iter().zip(admissions) {
        assert_eq!(read["request"], admission["receipt"]);
        assert_eq!(read["key"], admission["key"]);
    }
}

// A protected read waiting on the actual OS fence must retain the original
// receipt. Natural expiry or host revocation cannot turn into a successful read.
#[tokio::test]
async fn original_request_receipt_expires_or_is_revoked_while_read_waits_for_fence() {
    for revoke in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("a.txt"), "frozen").unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut config = RuntimeConfig::default();
        config.host.frozen_reads = true;
        config
            .permissions
            .push(cayw_runtime::ScopedPermission::exact(
                "fs.read",
                json!({"path":"a.txt"}),
            ));
        config.request_policy = Some(cayw_runtime::RequestPolicy {
            permission_policy_id: "receipt-wait-policy".into(),
            operations: vec![cayw_requests::OperationScope::exact(
                "read-exact",
                "fs.read",
                json!({"path":"a.txt"}),
                "workspace-policy",
            )],
        });
        let rt = Runtime::open(
            dir.path().join("session.db"),
            &root,
            Arc::new(Controlled(tx)),
            Model::new("fixture", "receipt-wait"),
            config,
        )
        .unwrap();
        let id = rt.create(None).unwrap().id;
        let run = {
            let rt = rt.clone();
            let id = id.clone();
            tokio::spawn(async move {
                rt.prompt(&id, "protected read", CancellationToken::new())
                    .await
            })
        };
        let script = "local input={purpose='read the requested file',gap='exact read is unavailable',operation_id='read-exact',capability='fs.read',arguments={path='a.txt'}}; cayw.call('capability.request',input); cayw.call('capability.request',input); yield_control(); return cayw.call('fs.read',{path='a.txt'})";
        tool(
            next(&mut rx).await,
            "receipt-parent",
            json!({"action":"exec","script":script,"yield_time_ms":30000,"max_output_tokens":4096}),
        );
        let round = next(&mut rx).await;
        let launched = page(&round);
        assert_eq!(launched.status, CellStatus::Yielded);
        let task = rt.tasks(&id).unwrap().remove(0);
        let request = rt
            .request_events(&task.id, 0, 64)
            .unwrap()
            .into_iter()
            .rev()
            .find_map(|e| {
                e.request_id
                    .map(|id| rt.request_status(&task.id, &id).unwrap())
            })
            .unwrap();
        let evaluation = rt
            .request_evaluate(&task.id, &request.id, "fixture-host")
            .unwrap();
        rt.request_approve(
            &task.id,
            &evaluation,
            Duration::from_secs(if revoke { 300 } else { 2 }),
            "original read receipt",
        )
        .unwrap();
        // Both request service calls have settled before this lock is taken.
        let fence = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join(".cayw-host-read-fence"))
            .unwrap();
        fence.lock().unwrap();
        tool(
            round,
            "resume-protected-read",
            json!({"action":"wait","cell_id":launched.cell_id,"cursor":launched.next_cursor,"yield_time_ms":0,"max_output_tokens":4096,"continue_execution":true}),
        );
        let round = next(&mut rx).await;
        tokio::time::timeout(Duration::from_secs(10), async {loop {if rt.events(&id,0,1000).unwrap().iter().any(|e|matches!(&e.event,CoreEvent::CapabilityStarted{call} if call.name=="fs.read")) {break;} tokio::task::yield_now().await;}}).await.unwrap();
        let original = rt
            .events(&id, 0, 1000)
            .unwrap()
            .into_iter()
            .find_map(|e| match e.event {
                CoreEvent::Host { kind, data } if kind == "task_request_admitted" => Some(data),
                _ => None,
            })
            .unwrap();
        assert_eq!(original["receipt"]["request_id"], request.id);
        if revoke {
            rt.request_revoke(
                &task.id,
                &request.id,
                "fixture-host",
                "withdraw original receipt",
            )
            .unwrap();
        }
        tokio::time::timeout(Duration::from_secs(10), async {loop {if rt.events(&id,0,1000).unwrap().iter().any(|e|matches!(&e.event,CoreEvent::CapabilitySettled{call} if call.name=="fs.read"&&matches!(call.outcome,Some(CapabilityOutcome::Failed{..})))){break;}tokio::task::yield_now().await;}}).await.unwrap();
        drop(fence);
        if revoke {
            drop(round);
            let _ = run.await.unwrap();
        } else {
            tool(
                round,
                "wait-expired-read",
                json!({"action":"wait","cell_id":launched.cell_id,"cursor":launched.next_cursor,"yield_time_ms":30000,"max_output_tokens":4096,"continue_execution":false}),
            );
            let round = next(&mut rx).await;
            assert_eq!(checked_page(&round, true).status, CellStatus::Failed);
            finish(round);
            run.await.unwrap().unwrap();
        }
        let events = rt.events(&id, 0, 1000).unwrap();
        assert_eq!(
            events
                .iter()
                .filter(
                    |e| matches!(&e.event,CoreEvent::Host{kind,..} if kind=="task_request_admitted")
                )
                .count(),
            1
        );
        assert!(
            !events.iter().any(
                |e| matches!(&e.event,CoreEvent::Host{kind,..} if kind=="runtime_shared_read")
            )
        );
        assert_eq!(
            rt.request_status(&task.id, &request.id).unwrap().state,
            if revoke {
                cayw_requests::RequestState::Revoked
            } else {
                cayw_requests::RequestState::Expired
            }
        );
        assert_eq!(rt.health().unwrap().running, 0);
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "frozen"
        );
    }
}

// A cooperating writer with a different Runtime and database must invalidate a
// capture even when it replaces a file with exactly the same bytes.
#[tokio::test]
async fn independently_opened_runtime_identical_write_invalidates_same_attempt() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("a.txt"), "frozen").unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut config = RuntimeConfig::default();
    config.host.frozen_reads = true;
    config
        .permissions
        .push(cayw_runtime::ScopedPermission::exact(
            "fs.read",
            json!({"path":"a.txt"}),
        ));
    let rt = Runtime::open(
        dir.path().join("reader.db"),
        &root,
        Arc::new(Controlled(tx)),
        Model::new("fixture", "independent-reader"),
        config,
    )
    .unwrap();
    let id = rt.create(None).unwrap().id;
    let run = {
        let rt = rt.clone();
        let id = id.clone();
        tokio::spawn(async move {
            rt.prompt(
                &id,
                "read around independent write",
                CancellationToken::new(),
            )
            .await
        })
    };
    tool(
        next(&mut rx).await,
        "before-write",
        json!({"action":"exec","script":"return cayw.call('fs.read',{path='a.txt'})","yield_time_ms":30000,"max_output_tokens":4096}),
    );
    let round = next(&mut rx).await;
    assert_eq!(page(&round).status, CellStatus::Completed);
    let before = rt
        .events(&id, 0, 1000)
        .unwrap()
        .into_iter()
        .find_map(|e| match e.event {
            CoreEvent::CapabilitySettled { call } => match call.outcome {
                Some(CapabilityOutcome::Completed { result }) => Some(result),
                _ => None,
            },
            _ => None,
        })
        .unwrap();
    let hash = before["sha256"].as_str().unwrap();
    let script = format!(
        "return cayw.call('fs.write',{{path='a.txt',content='frozen',overwrite=true,expected_sha256='{hash}'}})"
    );
    struct Writer(String);
    impl Provider for Writer {
        fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
            let reply =
                cayw_runtime::fixture::script_response(&request, &self.0, "independent-write");
            Box::pin(futures::stream::iter([AssistantEvent::Done {
                message: reply,
            }]))
        }
    }
    let mut writer_config = RuntimeConfig::default();
    writer_config
        .permissions
        .push(cayw_runtime::ScopedPermission::exact(
            "fs.write",
            json!({"path":"a.txt","content":"frozen","overwrite":true,"expected_sha256":hash}),
        ));
    let writer = Runtime::open(
        dir.path().join("writer.db"),
        &root,
        Arc::new(Writer(script)),
        Model::new("fixture", "independent-writer"),
        writer_config,
    )
    .unwrap();
    let other = writer.create(None).unwrap().id;
    let written = writer
        .prompt(
            &other,
            "replace with identical content",
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(written.state, cayw_core::SessionState::Idle);
    assert!(writer.events(&other,0,1000).unwrap().iter().any(|e|matches!(&e.event,CoreEvent::CapabilitySettled{call} if call.name=="fs.write"&&matches!(call.outcome,Some(CapabilityOutcome::Completed{..})))));
    tool(
        round,
        "after-write",
        json!({"action":"exec","script":"return cayw.call('fs.read',{path='a.txt'})","yield_time_ms":30000,"max_output_tokens":4096}),
    );
    let round = next(&mut rx).await;
    assert_eq!(page(&round).status, CellStatus::Completed);
    finish(round);
    run.await.unwrap().unwrap();
    let reads: Vec<_> = rt
        .events(&id, 0, 1000)
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.event {
            CoreEvent::Host { kind, data } if kind == "runtime_shared_read" => Some(data),
            _ => None,
        })
        .collect();
    assert_eq!(reads.len(), 2);
    assert_eq!(reads[0]["role"], "leader");
    assert_eq!(reads[1]["role"], "leader");
    assert_eq!(reads[0]["lease"], reads[1]["lease"]);
    assert_eq!(
        reads[0]["stamp"]["workspace"],
        reads[1]["stamp"]["workspace"]
    );
    assert_eq!(
        reads[0]["stamp"]["incarnation"],
        reads[1]["stamp"]["incarnation"]
    );
    assert_eq!(
        reads[1]["stamp"]["generation"].as_u64().unwrap(),
        reads[0]["stamp"]["generation"].as_u64().unwrap() + 1
    );
    assert_eq!(reads[0]["result_sha256"], reads[1]["result_sha256"]);
    assert_ne!(reads[0]["source"], reads[1]["source"]);
    assert_eq!(reads[0]["source"], reads[0]["origin"]);
    assert_eq!(reads[1]["source"], reads[1]["origin"]);
}

// Holding the real OS fence puts an admitted original read to sleep. Altering
// only its persisted grant must prevent both capture and later ready delivery.
#[tokio::test]
async fn original_once_grant_cannot_expire_or_be_replaced_during_fence_wait() {
    for revoke in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("a.txt"), "frozen").unwrap();
        let fence = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join(".cayw-host-read-fence"))
            .unwrap();
        fence.lock().unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut config = RuntimeConfig::default();
        config.host.frozen_reads = true;
        config.grant_lifetime = Duration::from_secs(300);
        let rt = Runtime::open(
            dir.path().join("session.db"),
            &root,
            Arc::new(Controlled(tx)),
            Model::new("fixture", "grant-wait"),
            config,
        )
        .unwrap();
        let id = rt.create(None).unwrap().id;
        let run = {
            let rt = rt.clone();
            let id = id.clone();
            tokio::spawn(async move {
                rt.prompt(&id, "read with original grant", CancellationToken::new())
                    .await
            })
        };
        tool(
            next(&mut rx).await,
            "original-parent",
            json!({"action":"exec","script":"return cayw.call('fs.read',{path='a.txt'})","yield_time_ms":0,"max_output_tokens":4096}),
        );
        let round = next(&mut rx).await;
        let cell = page(&round).cell_id;
        approve(&rt, &id).await;
        tokio::time::timeout(Duration::from_secs(10),async{loop{if rt.events(&id,0,1000).unwrap().iter().any(|e|matches!(&e.event,CoreEvent::CapabilityStarted{call} if call.name=="fs.read")){break}tokio::task::yield_now().await;}}).await.unwrap();
        let original = rt
            .grants(&id)
            .unwrap()
            .into_iter()
            .find(|g| g.consumed)
            .unwrap();
        assert_eq!(original.mode, "allow_once");
        let db = rusqlite::Connection::open(dir.path().join("session.db")).unwrap();
        if revoke {
            assert_eq!(
                db.execute(
                    "UPDATE runtime_grants SET state='revoked' WHERE id=?1",
                    [&original.id]
                )
                .unwrap(),
                1
            );
        } else {
            assert_eq!(
                db.execute(
                    "UPDATE runtime_grants SET expires=0 WHERE id=?1",
                    [&original.id]
                )
                .unwrap(),
                1
            );
        }
        db.execute("INSERT INTO runtime_grants(id,session,attempt,approval,capability,scope,mode,actor,expires,state,consumed) VALUES ('replacement',?1,?2,'replacement-approval',?3,?4,'allow_task','replacement-host',?5,'active',0)",rusqlite::params![id,original.attempt_id,original.capability,original.scope,i64::try_from(cayw_ai::timestamp_ms()+300000).unwrap()]).unwrap();
        // Keep the file lock held until the original child reports a known failure.
        tokio::time::timeout(Duration::from_secs(10),async{loop{if rt.events(&id,0,1000).unwrap().iter().any(|e|matches!(&e.event,CoreEvent::CapabilitySettled{call} if call.name=="fs.read"&&matches!(call.outcome,Some(CapabilityOutcome::Failed{..})))){break}tokio::task::yield_now().await;}}).await.unwrap();
        drop(fence);
        tool(
            round,
            "wait-original",
            json!({"action":"wait","cell_id":cell,"cursor":0,"yield_time_ms":30000,"max_output_tokens":4096,"continue_execution":false}),
        );
        let round = next(&mut rx).await;
        assert_eq!(checked_page(&round, true).status, CellStatus::Failed);
        finish(round);
        run.await.unwrap().unwrap();
        let events = rt.events(&id, 0, 1000).unwrap();
        assert!(
            !events.iter().any(
                |e| matches!(&e.event,CoreEvent::Host{kind,..} if kind=="runtime_shared_read")
            )
        );
        let admissions: Vec<_> = events
            .iter()
            .filter_map(|e| match &e.event {
                CoreEvent::Host { kind, data } if kind == "permission_admitted" => Some(data),
                _ => None,
            })
            .collect();
        assert_eq!(admissions.len(), 1);
        assert_eq!(admissions[0]["grant_id"], original.id);
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "frozen"
        );
    }
}
