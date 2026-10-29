use super::*;
use crate::{QueueError, TaskInput, TaskIntent, resources::ResourcePolicy};
use rusqlite::params;

// Minimal stand-in for the host Runtime's audit table, which the queue writes
// inside the same transaction as the task terminal.
struct AuditDb {
    connection: Connection,
}
impl AuditDb {
    fn open(path: &Path) -> Result<Self, QueueError> {
        let connection = Connection::open(path)?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS runtime_audit (
                seq INTEGER PRIMARY KEY AUTOINCREMENT, session TEXT NOT NULL,
                task TEXT, kind TEXT NOT NULL, timestamp INTEGER NOT NULL, payload TEXT NOT NULL);
             CREATE INDEX IF NOT EXISTS runtime_audit_session ON runtime_audit(session,seq);",
        )?;
        Ok(Self { connection })
    }
    fn events(
        &self,
        session: &str,
        after: u64,
        limit: usize,
    ) -> Result<Vec<(String, serde_json::Value)>, QueueError> {
        let mut statement = self.connection.prepare(
            "SELECT kind,payload FROM runtime_audit WHERE session=?1 AND seq>?2 ORDER BY seq LIMIT ?3",
        )?;
        let rows = statement.query_map(params![session, sql_u64(after)?, limit as i64], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut events = vec![];
        for row in rows {
            let (kind, payload) = row?;
            events.push((kind, serde_json::from_str(&payload)?));
        }
        Ok(events)
    }
}

fn policy(bytes: u64) -> ResourcePolicy {
    ResourcePolicy {
        max_declared_memory_bytes: bytes,
    }
}
fn resources(bytes: u64) -> TaskResources {
    TaskResources {
        declared_memory_bytes: bytes,
        ..Default::default()
    }
}
fn open(path: &Path, bytes: u64) -> Queue {
    Queue::open_with_resources(path, 3, 8, 2, &policy(bytes)).unwrap()
}
fn enqueue(queue: &Queue, session: &str, bytes: u64, priority: TaskPriority) -> TaskRecord {
    queue
        .enqueue_with_resources(
            session,
            &TaskInput {
                text: session.into(),
                images: vec![],
            },
            &TaskIntent::General,
            priority,
            &resources(bytes),
            8,
        )
        .unwrap()
}
fn finish(queue: &Queue, lease: &TaskLease) {
    queue
        .finish::<QueueError>(lease, TaskTerminal::Completed, None, |_| Ok(()))
        .unwrap();
}
fn usage(queue: &Queue, bytes: u64, tasks: usize) {
    let usage = queue.resource_usage().unwrap();
    assert_eq!(usage.max_declared_memory_bytes, 100);
    assert_eq!(usage.reserved_declared_memory_bytes, bytes);
    assert_eq!(usage.holding_tasks, tasks);
}

#[test]
fn resources_share_one_durable_total_and_small_tasks_cannot_bypass_the_head() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("queue.sqlite");
    let _audit = AuditDb::open(&path).unwrap();
    let owner = open(&path, 100);
    let peer = open(&path, 100);
    let a = enqueue(&owner, "a", 60, TaskPriority::Normal);
    let la = owner.claim(&a.id).unwrap();
    assert!(owner.admit(&la, 3).unwrap());
    let b = enqueue(&peer, "b", 50, TaskPriority::High);
    let lb = peer.claim(&b.id).unwrap();
    let c = enqueue(&peer, "c", 10, TaskPriority::Normal);
    let lc = peer.claim(&c.id).unwrap();
    assert!(!peer.admit(&lb, 3).unwrap());
    assert!(!peer.admit(&lc, 3).unwrap());
    assert!(owner.admit(&la, 3).unwrap());
    usage(&owner, 60, 1);
    usage(&peer, 60, 1);
    assert_eq!(peer.get(&a.id).unwrap().resources, resources(60));
    assert_eq!(peer.get(&a.id).unwrap().session_id, "a");
    assert_eq!(
        peer.get(&a.id).unwrap().worker_id.as_deref(),
        Some(la.worker.as_str())
    );
    finish(&owner, &la);
    assert!(peer.admit(&lb, 3).unwrap());
    assert!(peer.admit(&lc, 3).unwrap());
    usage(&owner, 60, 2);
    finish(&peer, &lb);
    usage(&owner, 10, 1);
    finish(&peer, &lc);
    usage(&owner, 0, 0);
}

#[test]
fn simultaneous_connections_cannot_reserve_more_than_the_total() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("queue.sqlite");
    let owner = open(&path, 100);
    let peer = open(&path, 100);
    let a = enqueue(&owner, "a", 60, TaskPriority::Normal);
    let b = enqueue(&peer, "b", 60, TaskPriority::Normal);
    let la = owner.claim(&a.id).unwrap();
    let lb = peer.claim(&b.id).unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let gate = barrier.clone();
    let first = std::thread::spawn(move || {
        gate.wait();
        owner.admit(&la, 3).unwrap()
    });
    let second = std::thread::spawn(move || {
        barrier.wait();
        peer.admit(&lb, 3).unwrap()
    });
    assert_eq!(
        usize::from(first.join().unwrap()) + usize::from(second.join().unwrap()),
        1
    );
    let reopened = open(&path, 100);
    usage(&reopened, 60, 1);
    assert_eq!(
        reopened
            .snapshot(8)
            .unwrap()
            .iter()
            .filter(|task| task.holds_slot)
            .count(),
        1
    );
}

#[test]
fn approval_park_releases_only_execution_admission_and_resume_rechecks_resources() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("queue.sqlite");
    let _audit = AuditDb::open(&path).unwrap();
    let owner = open(&path, 100);
    let peer = open(&path, 100);
    let a = enqueue(&owner, "a", 80, TaskPriority::Normal);
    let la = owner.claim(&a.id).unwrap();
    assert!(owner.admit(&la, 3).unwrap());
    owner.attach(&la, "attempt-a").unwrap();
    let identity = owner.get(&a.id).unwrap().cancel_target();
    owner.park(&la).unwrap();
    usage(&peer, 0, 0);
    assert_eq!(peer.get(&a.id).unwrap().resources, resources(80));
    let b = enqueue(&peer, "b", 50, TaskPriority::Normal);
    let lb = peer.claim(&b.id).unwrap();
    assert!(peer.admit(&lb, 3).unwrap());
    owner.ready(&la).unwrap();
    assert!(!owner.admit(&la, 3).unwrap());
    usage(&owner, 50, 1);
    finish(&peer, &lb);
    assert!(owner.admit(&la, 3).unwrap());
    assert_eq!(owner.get(&a.id).unwrap().cancel_target(), identity);
    usage(&owner, 80, 1);
    finish(&owner, &la);
}

#[test]
fn cleanup_and_terminal_persistence_failures_keep_original_resources_on_reopen() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("queue.sqlite");
    let _audit = AuditDb::open(&path).unwrap();
    let owner = open(&path, 100);
    let a = enqueue(&owner, "a", 80, TaskPriority::Normal);
    let la = owner.claim(&a.id).unwrap();
    assert!(owner.admit(&la, 3).unwrap());
    owner.attach(&la, "attempt-a").unwrap();
    lock(&owner.connection)
        .execute_batch(
            "CREATE TRIGGER fail_resource_terminal BEFORE UPDATE ON runtime_tasks
             WHEN NEW.holds_slot=0 AND OLD.holds_slot=1
             BEGIN SELECT RAISE(ABORT,'synthetic release persistence failure'); END;",
        )
        .unwrap();
    assert!(
        owner
            .finish::<QueueError>(&la, TaskTerminal::Completed, None, |_| Ok(()))
            .is_err()
    );
    usage(&owner, 80, 1);
    assert!(owner.park(&la).is_err());
    owner.block(&la, "synthetic unconfirmed cleanup").unwrap();
    let identity = owner.get(&a.id).unwrap();
    assert_eq!(identity.state, TaskState::Draining);
    drop(owner);
    let peer = open(&path, 100);
    assert_eq!(peer.get(&a.id).unwrap(), identity);
    usage(&peer, 80, 1);
    let b = enqueue(&peer, "b", 30, TaskPriority::Normal);
    let lb = peer.claim(&b.id).unwrap();
    assert!(!peer.admit(&lb, 3).unwrap());
    assert!(peer.recover("a").is_err());
    usage(&peer, 80, 1);
    assert_eq!(peer.get(&a.id).unwrap(), identity);
    lock(&peer.connection)
        .execute_batch("DROP TRIGGER fail_resource_terminal")
        .unwrap();
    // This queue primitive is invoked only after Runtime proved the original owner stopped.
    peer.recover("a").unwrap();
    usage(&peer, 0, 0);
    assert!(peer.admit(&la, 3).is_err());
    assert_eq!(peer.get(&a.id).unwrap().state, TaskState::Interrupted);
    assert!(peer.admit(&lb, 3).unwrap());
    usage(&peer, 30, 1);
}

#[test]
fn failed_authority_settlement_does_not_return_reserved_memory() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("queue.sqlite");
    let audit = AuditDb::open(&path).unwrap();
    let owner = open(&path, 100);
    let a = enqueue(&owner, "a", 70, TaskPriority::Normal);
    let la = owner.claim(&a.id).unwrap();
    assert!(owner.admit(&la, 3).unwrap());
    assert!(
        owner
            .finish(
                &la,
                TaskTerminal::Completed,
                None,
                |_| -> Result<(), QueueError> {
                    Err(QueueError::Worker(
                        "synthetic authority settlement failure".into(),
                    ))
                }
            )
            .is_err()
    );
    usage(&owner, 70, 1);
    assert_eq!(owner.get(&a.id).unwrap().state, TaskState::Running);
    assert!(audit.events("a", 0, 100).unwrap().is_empty());
}

#[test]
fn invalid_and_unsupported_requests_leave_no_accepted_task_or_reservation() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("queue.sqlite");
    let owner = open(&path, 100);
    for request in [
        resources(101),
        TaskResources {
            hard_memory_limit_bytes: Some(0),
            ..resources(1)
        },
        TaskResources {
            cpu_quota_percent: Some(0),
            ..resources(1)
        },
        TaskResources {
            gpu_units: 1,
            ..resources(1)
        },
        TaskResources {
            io_bandwidth_bytes_per_sec: Some(0),
            ..resources(1)
        },
    ] {
        let error = owner
            .enqueue_with_resources(
                "rejected",
                &TaskInput {
                    text: "input".into(),
                    images: vec![],
                },
                &TaskIntent::General,
                TaskPriority::High,
                &request,
                8,
            )
            .unwrap_err();
        assert!(matches!(error, QueueError::InvalidInput(_)));
        assert!(owner.list("rejected").unwrap().is_empty());
        usage(&owner, 0, 0);
    }
    let child = owner
        .try_start_child("child", "fixed input", &TaskIntent::General, 8, 3)
        .unwrap();
    assert_eq!(
        owner.get(&child.id).unwrap().resources,
        TaskResources::default()
    );
    usage(&owner, 0, 1);
}

#[test]
fn accepted_cancellation_retains_resources_until_the_original_terminal_commits() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("queue.sqlite");
    let _audit = AuditDb::open(&path).unwrap();
    let owner = open(&path, 100);
    let peer = open(&path, 100);
    let a = enqueue(&owner, "a", 90, TaskPriority::Normal);
    let la = owner.claim(&a.id).unwrap();
    assert!(owner.admit(&la, 3).unwrap());
    owner.attach(&la, "attempt-a").unwrap();
    let target = owner.get(&a.id).unwrap().cancel_target();
    assert!(peer.cancel(&target).unwrap());
    usage(&peer, 90, 1);
    let b = enqueue(&peer, "b", 20, TaskPriority::Normal);
    let lb = peer.claim(&b.id).unwrap();
    assert!(!peer.admit(&lb, 3).unwrap());
    let mut wrong = target.clone();
    wrong.worker_id = Some("other-worker".into());
    assert!(matches!(
        peer.cancel(&wrong),
        Err(QueueError::StaleCancellation)
    ));
    usage(&peer, 90, 1);
    owner
        .finish::<QueueError>(&la, TaskTerminal::Cancelled, None, |_| Ok(()))
        .unwrap();
    assert_eq!(peer.get(&a.id).unwrap().cancel_target(), target);
    usage(&peer, 0, 0);
    assert!(peer.admit(&lb, 3).unwrap());
}

#[test]
fn commit_failure_rolls_back_the_resource_release_and_keeps_original_identity() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("queue.sqlite");
    let audit = AuditDb::open(&path).unwrap();
    let owner = open(&path, 100);
    let a = enqueue(&owner, "a", 85, TaskPriority::Normal);
    let la = owner.claim(&a.id).unwrap();
    assert!(owner.admit(&la, 3).unwrap());
    owner.attach(&la, "attempt-a").unwrap();
    let before = owner.get(&a.id).unwrap();
    lock(&owner.connection).execute_batch(
        "PRAGMA foreign_keys=ON;
         CREATE TABLE resource_parent(id INTEGER PRIMARY KEY);
         CREATE TABLE resource_deferred(parent INTEGER REFERENCES resource_parent(id) DEFERRABLE INITIALLY DEFERRED);
         CREATE TRIGGER fail_resource_commit AFTER INSERT ON runtime_audit WHEN NEW.kind='run_finished'
         BEGIN INSERT INTO resource_deferred VALUES(1); END;"
    ).unwrap();
    let settled = std::cell::Cell::new(false);
    assert!(
        owner
            .finish::<QueueError>(&la, TaskTerminal::Completed, None, |_| {
                settled.set(true);
                Ok(())
            })
            .is_err()
    );
    assert!(
        settled.get(),
        "failure must happen after settlement, at COMMIT"
    );
    assert_eq!(owner.get(&a.id).unwrap(), before);
    usage(&owner, 85, 1);
    assert!(audit.events("a", 0, 100).unwrap().is_empty());
    drop(owner);
    let reopened = open(&path, 100);
    assert_eq!(reopened.get(&a.id).unwrap(), before);
    usage(&reopened, 85, 1);
}

#[test]
fn reopened_policy_adopts_a_trusted_change_only_when_settled() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("queue.sqlite");
    let owner = open(&path, 100);
    drop(owner);
    // Invalid values are rejected before any policy is written.
    for bytes in [0, u64::MAX] {
        assert!(matches!(
            Queue::open_with_resources(&path, 3, 8, 2, &policy(bytes)),
            Err(QueueError::InvalidInput(_))
        ));
    }

    // A complete trusted policy is adopted after a safe stop and remains
    // the one canonical policy for later connections.
    let requested = QueuePolicy::new(3, 8, 2, &policy(101)).unwrap();
    assert_eq!(adopt_policy(&path, requested).unwrap(), Some(()));
    let adopted = open(&path, 101);
    assert_eq!(
        adopted.resource_usage().unwrap().max_declared_memory_bytes,
        101
    );
    let task = enqueue(&adopted, "live", 1, TaskPriority::Normal);
    assert_eq!(task.state, TaskState::Queued);
    drop(adopted);
    // A durable live task blocks replacing the effective policy.
    assert!(matches!(
        adopt_policy(&path, QueuePolicy::new(3, 8, 2, &policy(102)).unwrap()),
        Err(QueueError::InvalidInput(_))
    ));
    let restored = open(&path, 101);
    assert_eq!(restored.get(&task.id).unwrap().state, TaskState::Queued);
    assert_eq!(
        restored.resource_usage().unwrap().max_declared_memory_bytes,
        101
    );
    drop(restored);

    let large = root.path().join("large.sqlite");
    let owner = open(&large, i64::MAX as u64);
    let a = enqueue(&owner, "a", i64::MAX as u64, TaskPriority::Normal);
    let la = owner.claim(&a.id).unwrap();
    assert!(owner.admit(&la, 3).unwrap());
    let b = enqueue(&owner, "b", 1, TaskPriority::Normal);
    let lb = owner.claim(&b.id).unwrap();
    assert!(!owner.admit(&lb, 3).unwrap());
    assert_eq!(
        owner
            .resource_usage()
            .unwrap()
            .reserved_declared_memory_bytes,
        i64::MAX as u64
    );
}

#[test]
fn migration_adds_zero_declarations_without_claiming_or_replaying_legacy_tasks() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("queue.sqlite");
    let legacy = Connection::open(&path).unwrap();
    legacy.execute_batch("CREATE TABLE runtime_queue_config (
        singleton INTEGER PRIMARY KEY, concurrency INTEGER NOT NULL, capacity INTEGER NOT NULL,
        fairness_after INTEGER NOT NULL, dispatch_count INTEGER NOT NULL, next_ready_order INTEGER NOT NULL);
        INSERT INTO runtime_queue_config VALUES (1,3,8,2,1,1);
        CREATE TABLE runtime_tasks (
        ticket INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT NOT NULL UNIQUE, session TEXT NOT NULL,
        input_json TEXT NOT NULL, state TEXT NOT NULL, queued_at INTEGER NOT NULL,
        worker TEXT, attempt TEXT, fence INTEGER NOT NULL DEFAULT 0, cancel_requested INTEGER NOT NULL DEFAULT 0,
        error TEXT, intent TEXT NOT NULL DEFAULT '{\"kind\":\"general\"}', priority TEXT NOT NULL DEFAULT 'normal',
        holds_slot INTEGER NOT NULL DEFAULT 0, ready_order INTEGER, ready_since_dispatch INTEGER,
        dispatches INTEGER NOT NULL DEFAULT 0);
        INSERT INTO runtime_tasks(id,session,input_json,state,queued_at,worker,attempt,fence,holds_slot,dispatches)
        VALUES ('old','legacy','{\"text\":\"original\",\"images\":[]}','running',1,'original-worker','original-attempt',7,1,1);").unwrap();
    drop(legacy);
    let owner = open(&path, 100);
    let original = owner.get("old").unwrap();
    assert_eq!(original.resources, TaskResources::default());
    assert_eq!(original.worker_id.as_deref(), Some("original-worker"));
    assert_eq!(original.attempt_id.as_deref(), Some("original-attempt"));
    assert_eq!(original.fence, 7);
    assert_eq!(original.state, TaskState::Running);
    usage(&owner, 0, 1);
    drop(owner);
    assert_eq!(open(&path, 100).get("old").unwrap(), original);
}
