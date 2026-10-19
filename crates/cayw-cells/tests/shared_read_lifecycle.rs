use async_trait::async_trait;
use cayw_ai::{CancellationToken, ToolDefinition};
use cayw_cells::*;
use cayw_codemode::{CapabilityError, Limits};
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

fn origin(cell: &str, sequence: u64) -> ReadOrigin {
    ReadOrigin {
        execution: CellExecution {
            identity: CellIdentity {
                session_id: "session".into(),
                task_id: "task".into(),
                attempt_id: "attempt".into(),
                permission_context: "principal:1".into(),
            },
            cell_id: cell.into(),
            parent_call_id: format!("parent-{cell}"),
            source_hash: format!("source-{cell}"),
        },
        sequence,
    }
}
fn stamp() -> ReadStamp {
    ReadStamp {
        workspace: "held-root".into(),
        incarnation: "incarnation".into(),
        generation: 1,
        semantics: "bounded-utf8".into(),
    }
}
fn coordinator(limits: CellLimits) -> Arc<ReadCoordinator> {
    ReadCoordinator::new(
        vec![ReadPolicy {
            capability: "read".into(),
            reuse: true,
            coalesce: true,
        }],
        limits,
    )
    .unwrap()
}
struct Source(Arc<AtomicUsize>);
#[async_trait]
impl ReadSource for Source {
    async fn capture(&mut self, _: CancellationToken) -> Result<Value, CapabilityError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(json!({"content":"frozen"}))
    }
}

// Publishing from capture rather than the original Cells acknowledgment makes
// this follower finish before the explicit acknowledgment below.
#[tokio::test]
async fn publication_waits_for_original_acknowledgment_and_cancelled_waiter_leaves_leader() {
    let reads = coordinator(CellLimits::default());
    let captures = Arc::new(AtomicUsize::new(0));
    let mut source = Source(captures.clone());
    let leader = reads
        .read(
            origin("leader", 1),
            "read",
            &json!({}),
            stamp(),
            &mut source,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let cancel = CancellationToken::new();
    let mut follower_source = Source(captures.clone());
    let args = json!({});
    let follower = reads.read(
        origin("follower", 3),
        "read",
        &args,
        stamp(),
        &mut follower_source,
        cancel.clone(),
    );
    tokio::pin!(follower);
    assert!(
        futures::poll!(&mut follower).is_pending(),
        "uncommitted leader bytes became visible"
    );
    cancel.cancel();
    assert!(!follower.await.err().unwrap().outcome_unknown);
    assert_eq!(captures.load(Ordering::SeqCst), 1);
    leader.acknowledge();
    let ready = reads
        .read(
            origin("ready", 2),
            "read",
            &json!({}),
            stamp(),
            &mut source,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(ready.role, ReadRole::Ready);
    assert_eq!(ready.source, origin("leader", 1));
    ready.acknowledge();
    assert_eq!(captures.load(Ordering::SeqCst), 1);
}

// A known original refusal after capture/ready delivery is still a confirmed
// failure. It must not freeze other admitted reads as an abandoned commit.
#[tokio::test]
async fn known_candidate_rejection_releases_only_original_publication() {
    let reads = coordinator(CellLimits::default());
    let captures = Arc::new(AtomicUsize::new(0));
    let mut source = Source(captures.clone());
    let first = reads
        .read(
            origin("leader", 1),
            "read",
            &json!({}),
            stamp(),
            &mut source,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let mut waiting_source = Source(captures.clone());
    let args = json!({});
    let waiter = reads.read(
        origin("waiter", 1),
        "read",
        &args,
        stamp(),
        &mut waiting_source,
        CancellationToken::new(),
    );
    tokio::pin!(waiter);
    assert!(futures::poll!(&mut waiter).is_pending());
    first.reject("original read grant expired");
    assert!(!waiter.await.err().unwrap().outcome_unknown);
    let leader = reads
        .read(
            origin("next", 1),
            "read",
            &args,
            stamp(),
            &mut source,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(leader.role, ReadRole::Leader);
    leader.acknowledge();
    let ready = reads
        .read(
            origin("cancelled-ready", 1),
            "read",
            &args,
            stamp(),
            &mut source,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(ready.role, ReadRole::Ready);
    ready.reject("original caller cancelled");
    let later = reads
        .read(
            origin("later", 1),
            "read",
            &args,
            stamp(),
            &mut source,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(later.role, ReadRole::Ready);
    later.acknowledge();
    assert_eq!(captures.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn dropped_candidate_wakes_followers_and_rejects_future_hits_without_recapture() {
    let reads = coordinator(CellLimits::default());
    let captures = Arc::new(AtomicUsize::new(0));
    let mut source = Source(captures.clone());
    let candidate = reads
        .read(
            origin("leader", 1),
            "read",
            &json!({}),
            stamp(),
            &mut source,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let mut follower_source = Source(captures.clone());
    let args = json!({});
    let follower = reads.read(
        origin("follower", 1),
        "read",
        &args,
        stamp(),
        &mut follower_source,
        CancellationToken::new(),
    );
    tokio::pin!(follower);
    assert!(futures::poll!(&mut follower).is_pending());
    drop(candidate);
    assert!(follower.await.is_err());
    assert!(
        reads
            .read(
                origin("later", 1),
                "read",
                &json!({}),
                stamp(),
                &mut source,
                CancellationToken::new()
            )
            .await
            .is_err()
    );
    assert_eq!(captures.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cancelled_ready_hit_and_invalidated_generation_never_deliver_old_value() {
    let reads = coordinator(CellLimits::default());
    let captures = Arc::new(AtomicUsize::new(0));
    let mut source = Source(captures.clone());
    reads
        .read(
            origin("leader", 1),
            "read",
            &json!({}),
            stamp(),
            &mut source,
            CancellationToken::new(),
        )
        .await
        .unwrap()
        .acknowledge();
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(
        reads
            .read(
                origin("cancelled", 1),
                "read",
                &json!({}),
                stamp(),
                &mut source,
                cancel
            )
            .await
            .is_err()
    );
    assert_eq!(captures.load(Ordering::SeqCst), 1);
    reads.invalidate("session");
    let next = reads
        .read(
            origin("next", 1),
            "read",
            &json!({}),
            stamp(),
            &mut source,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(next.role, ReadRole::Leader);
    next.acknowledge();
    assert_eq!(captures.load(Ordering::SeqCst), 2);
}

// A leader stopped during capture must wake the original followers with a known
// failure. No waiting follower is promoted to run the capture a second time.
#[tokio::test]
async fn cancelled_leader_capture_wakes_waiters_without_promotion() {
    struct Blocking {
        captures: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl ReadSource for Blocking {
        async fn capture(&mut self, cancel: CancellationToken) -> Result<Value, CapabilityError> {
            self.captures.fetch_add(1, Ordering::SeqCst);
            cancel.cancelled().await;
            Err(CapabilityError::new("original capture cancelled"))
        }
    }
    let reads = coordinator(CellLimits::default());
    let captures = Arc::new(AtomicUsize::new(0));
    let mut blocked = Blocking {
        captures: captures.clone(),
    };
    let args = json!({});
    let cancel = CancellationToken::new();
    let leader = reads.read(
        origin("leader", 1),
        "read",
        &args,
        stamp(),
        &mut blocked,
        cancel.clone(),
    );
    tokio::pin!(leader);
    assert!(futures::poll!(&mut leader).is_pending());
    let mut source = Source(captures.clone());
    let follower = reads.read(
        origin("follower", 1),
        "read",
        &args,
        stamp(),
        &mut source,
        CancellationToken::new(),
    );
    tokio::pin!(follower);
    assert!(futures::poll!(&mut follower).is_pending());
    cancel.cancel();
    assert!(!leader.await.err().unwrap().outcome_unknown);
    assert!(!follower.await.err().unwrap().outcome_unknown);
    assert_eq!(captures.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn dropped_capture_wakes_waiters_without_detached_capture() {
    struct Blocking(Arc<AtomicUsize>);
    #[async_trait]
    impl ReadSource for Blocking {
        async fn capture(&mut self, _: CancellationToken) -> Result<Value, CapabilityError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            std::future::pending().await
        }
    }
    let reads = coordinator(CellLimits::default());
    let captures = Arc::new(AtomicUsize::new(0));
    let mut blocked = Blocking(captures.clone());
    let args = json!({});
    let mut leader = Box::pin(reads.read(
        origin("leader", 1),
        "read",
        &args,
        stamp(),
        &mut blocked,
        CancellationToken::new(),
    ));
    assert!(futures::poll!(&mut leader).is_pending());
    let mut source = Source(captures.clone());
    let follower = reads.read(
        origin("follower", 1),
        "read",
        &args,
        stamp(),
        &mut source,
        CancellationToken::new(),
    );
    tokio::pin!(follower);
    assert!(futures::poll!(&mut follower).is_pending());
    drop(leader);
    assert!(!follower.await.err().unwrap().outcome_unknown);
    assert_eq!(captures.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn full_cache_uses_original_uncached_capture_and_never_replaces_a_live_owner() {
    let reads = coordinator(CellLimits {
        cache_entries: 1,
        ..Default::default()
    });
    let captures = Arc::new(AtomicUsize::new(0));
    let mut source = Source(captures.clone());
    let first = reads
        .read(
            origin("first", 1),
            "read",
            &json!({"path":"first"}),
            stamp(),
            &mut source,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(first.role, ReadRole::Leader);
    let second = reads
        .read(
            origin("second", 1),
            "read",
            &json!({"path":"second"}),
            stamp(),
            &mut source,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(second.role, ReadRole::Uncached);
    second.acknowledge();
    first.acknowledge();
    let ready = reads
        .read(
            origin("ready", 1),
            "read",
            &json!({"path":"first"}),
            stamp(),
            &mut source,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(ready.role, ReadRole::Ready);
    ready.acknowledge();
    assert_eq!(captures.load(Ordering::SeqCst), 2);
}

// A policy that permits later reuse but forbids coalescing must not replace an
// unacknowledged leader for the same key with a new publication owner.
#[tokio::test]
async fn noncoalescing_policy_uses_uncached_capture_without_replacing_original_leader() {
    let reads = ReadCoordinator::new(
        vec![ReadPolicy {
            capability: "read".into(),
            reuse: true,
            coalesce: false,
        }],
        CellLimits::default(),
    )
    .unwrap();
    let captures = Arc::new(AtomicUsize::new(0));
    let mut source = Source(captures.clone());
    let first = reads
        .read(
            origin("first", 1),
            "read",
            &json!({}),
            stamp(),
            &mut source,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let second = reads
        .read(
            origin("second", 1),
            "read",
            &json!({}),
            stamp(),
            &mut source,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(first.role, ReadRole::Leader);
    assert_eq!(second.role, ReadRole::Uncached);
    second.acknowledge();
    first.acknowledge();
    let ready = reads
        .read(
            origin("ready", 1),
            "read",
            &json!({}),
            stamp(),
            &mut source,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(ready.role, ReadRole::Ready);
    assert_eq!(ready.source, origin("first", 1));
    ready.acknowledge();
    assert_eq!(captures.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn oversized_and_deep_capture_never_reaches_waiters_or_a_ready_entry() {
    struct Invalid {
        calls: Arc<AtomicUsize>,
        deep: bool,
    }
    #[async_trait]
    impl ReadSource for Invalid {
        async fn capture(&mut self, _: CancellationToken) -> Result<Value, CapabilityError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.deep {
                let mut value = Value::Null;
                for _ in 0..10000 {
                    value = Value::Array(vec![value]);
                }
                Ok(value)
            } else {
                Ok(json!("x".repeat(2 * 1024 * 1024 + 1)))
            }
        }
    }
    for deep in [false, true] {
        let reads = coordinator(CellLimits::default());
        let calls = Arc::new(AtomicUsize::new(0));
        let mut source = Invalid {
            calls: calls.clone(),
            deep,
        };
        assert!(
            !reads
                .read(
                    origin("invalid", 1),
                    "read",
                    &json!({}),
                    stamp(),
                    &mut source,
                    CancellationToken::new()
                )
                .await
                .err()
                .unwrap()
                .outcome_unknown
        );
        let mut good = Source(calls.clone());
        let next = reads
            .read(
                origin("next", 1),
                "read",
                &json!({}),
                stamp(),
                &mut good,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(next.role, ReadRole::Leader);
        next.acknowledge();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
}

struct Backend {
    journal: Arc<SqliteJournal>,
    reads: Arc<ReadCoordinator>,
    captures: Arc<AtomicUsize>,
    origins: Mutex<Vec<ReadOrigin>>,
    acknowledged: Arc<AtomicUsize>,
    abandoned: Arc<AtomicUsize>,
}
struct Completion {
    candidate: Option<ReadCandidate>,
    origin: ReadOrigin,
    journal: Arc<SqliteJournal>,
    acknowledged: Arc<AtomicUsize>,
    abandoned: Arc<AtomicUsize>,
}
impl BackendCompletion for Completion {
    fn acknowledge(mut self: Box<Self>) {
        let records = self
            .journal
            .records(&self.origin.execution.identity, &CellLimits::default())
            .unwrap();
        let record = records
            .iter()
            .find(|record| record.cell_id == self.origin.execution.cell_id)
            .unwrap();
        let call = &record.calls[self.origin.sequence as usize - 1];
        assert_eq!(
            call.status,
            IntentStatus::Succeeded,
            "acknowledged before the original Cells terminal committed"
        );
        assert_eq!(
            call.dependencies.as_ref(),
            Some(&self.candidate.as_ref().unwrap().dependencies)
        );
        self.candidate.take().unwrap().acknowledge();
        self.acknowledged.fetch_add(1, Ordering::SeqCst);
    }
}
impl Drop for Completion {
    fn drop(&mut self) {
        if self.candidate.is_some() {
            self.abandoned.fetch_add(1, Ordering::SeqCst);
        }
    }
}
#[async_trait]
impl CellBackend for Backend {
    fn capabilities(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "read".into(),
            description: "independently admitted fixture read".into(),
            parameters: json!({"type":"object","additionalProperties":false}),
        }]
    }
    fn created(&self, _: &CellExecution) -> Result<(), CellError> {
        Ok(())
    }
    async fn call(
        &self,
        execution: &CellExecution,
        sequence: u64,
        name: &str,
        arguments: Value,
        cancel: CancellationToken,
    ) -> Result<BackendResponse, CapabilityError> {
        let origin = ReadOrigin {
            execution: execution.clone(),
            sequence,
        };
        self.origins.lock().unwrap().push(origin.clone());
        let candidate = self
            .reads
            .read(
                origin.clone(),
                name,
                &arguments,
                stamp(),
                &mut Source(self.captures.clone()),
                cancel,
            )
            .await?;
        Ok(BackendResponse::owned(
            Ok(candidate.value().clone()),
            Some(candidate.dependencies.clone()),
            Box::new(Completion {
                candidate: Some(candidate),
                origin,
                journal: self.journal.clone(),
                acknowledged: self.acknowledged.clone(),
                abandoned: self.abandoned.clone(),
            }),
        ))
    }
    fn finished(&self, _: &CellExecution, _: &CellTerminal) -> Result<(), CellError> {
        Ok(())
    }
}
fn manager(path: &std::path::Path) -> (Arc<CellManager>, Arc<Backend>) {
    let journal = Arc::new(SqliteJournal::open(path).unwrap());
    let limits = CellLimits::default();
    let reads = coordinator(limits.clone());
    let backend = Arc::new(Backend {
        journal: journal.clone(),
        reads: reads.clone(),
        captures: Arc::new(AtomicUsize::new(0)),
        origins: Mutex::default(),
        acknowledged: Arc::new(AtomicUsize::new(0)),
        abandoned: Arc::new(AtomicUsize::new(0)),
    });
    (
        CellManager::new(
            origin("unused", 1).execution.identity,
            journal,
            backend.clone(),
            reads,
            limits,
            Limits::default(),
        )
        .unwrap(),
        backend,
    )
}
async fn execute(cells: &CellManager, source: &str, parent: &str) -> CellPage {
    let mut page = cells
        .exec(
            ExecRequest {
                script: source.into(),
                yield_time_ms: 10_000,
                max_output_tokens: 4096,
            },
            parent,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    while !page.status.is_terminal() {
        page = cells
            .wait(
                WaitRequest {
                    cell_id: page.cell_id,
                    cursor: page.next_cursor,
                    yield_time_ms: 10_000,
                    max_output_tokens: 4096,
                    continue_execution: false,
                },
                CancellationToken::new(),
            )
            .await
            .unwrap();
    }
    page
}
#[tokio::test]
async fn bridge_acknowledges_each_original_backend_only_after_its_terminal_and_dependencies_commit()
{
    let dir = tempfile::tempdir().unwrap();
    let (cells, backend) = manager(&dir.path().join("cells.db"));
    assert_eq!(
        execute(
            &cells,
            "cayw.call('read',{}); return cayw.call('read',{})",
            "original-parent"
        )
        .await
        .status,
        CellStatus::Completed
    );
    assert_eq!(backend.captures.load(Ordering::SeqCst), 1);
    assert_eq!(backend.acknowledged.load(Ordering::SeqCst), 2);
    assert_eq!(backend.abandoned.load(Ordering::SeqCst), 0);
    {
        let origins = backend.origins.lock().unwrap();
        assert_eq!(origins.len(), 2);
        assert_eq!(origins[0].sequence, 1);
        assert_eq!(origins[1].sequence, 2);
        assert_eq!(origins[0].execution, origins[1].execution);
        assert_eq!(origins[0].execution.parent_call_id, "original-parent");
    }
    cells.close().await;
}
#[tokio::test]
async fn rejected_original_cells_commit_abandons_completion_and_never_publishes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cells.db");
    let (cells, backend) = manager(&path);
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("CREATE TRIGGER reject_original_read BEFORE UPDATE ON cells_v1 WHEN json_extract(NEW.body,'$.calls[0].status')='succeeded' BEGIN SELECT RAISE(ABORT,'fixture original Cells terminal rejected'); END;").unwrap();
    let page = execute(&cells, "return cayw.call('read',{})", "original-parent").await;
    assert_eq!(page.status, CellStatus::OutcomeUnknown);
    assert_eq!(backend.captures.load(Ordering::SeqCst), 1);
    assert_eq!(backend.acknowledged.load(Ordering::SeqCst), 0);
    assert_eq!(backend.abandoned.load(Ordering::SeqCst), 1);
    assert!(backend.journal.is_frozen("session").unwrap());
    assert!(
        backend
            .reads
            .read(
                origin("later", 1),
                "read",
                &json!({}),
                stamp(),
                &mut Source(backend.captures.clone()),
                CancellationToken::new()
            )
            .await
            .is_err()
    );
    assert_eq!(backend.captures.load(Ordering::SeqCst), 1);
    cells.close().await;
}
