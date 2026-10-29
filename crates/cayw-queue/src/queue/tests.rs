use super::*;
use crate::{QueueError, TaskInput, TaskIntent};
use rusqlite::params;

// The host Runtime owns runtime_audit. Queue tests create the same minimal
// table and read the rows the queue writes.
#[derive(Debug)]
struct AuditRecord {
    kind: String,
    data: serde_json::Value,
}
struct AuditDb {
    connection: Connection,
}
impl AuditDb {
    fn open(path: &Path) -> Self {
        let connection = Connection::open(path).unwrap();
        connection
            .busy_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        connection
            .pragma_update(None, "synchronous", "FULL")
            .unwrap();
        connection
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS runtime_audit (
                    seq INTEGER PRIMARY KEY AUTOINCREMENT, session TEXT NOT NULL,
                    task TEXT, kind TEXT NOT NULL, timestamp INTEGER NOT NULL, payload TEXT NOT NULL);
                 CREATE INDEX IF NOT EXISTS runtime_audit_session ON runtime_audit(session,seq);",
            )
            .unwrap();
        Self { connection }
    }
    fn events(
        &self,
        session: &str,
        after: u64,
        limit: usize,
    ) -> Result<Vec<AuditRecord>, QueueError> {
        let mut statement = self.connection.prepare(
            "SELECT kind,payload FROM runtime_audit WHERE session=?1 AND seq>?2 ORDER BY seq LIMIT ?3",
        )?;
        let rows = statement.query_map(params![session, sql_u64(after)?, limit as i64], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut records = vec![];
        for row in rows {
            let (kind, payload) = row?;
            records.push(AuditRecord {
                kind,
                data: serde_json::from_str(&payload)?,
            });
        }
        Ok(records)
    }
}

fn settled(_: TaskTerminal) -> Result<(), QueueError> {
    Ok(())
}
fn unsettled(_: TaskTerminal) -> Result<(), QueueError> {
    panic!("fenced settlement ran")
}

#[test]
fn peer_cancellation_wins_before_finish_for_task_and_audit() {
    // Two real connections impose the failing ordering without timers.
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("queue.sqlite");
    let audit = AuditDb::open(&path);
    let owner = Queue::open(&path, 1, 4, 0).unwrap();
    let peer = Queue::open(&path, 1, 4, 0).unwrap();
    let task = owner
        .enqueue_input(
            "session",
            &TaskInput {
                text: "synthetic input".into(),
                images: vec![],
            },
            &TaskIntent::General,
            4,
        )
        .unwrap();
    let lease = owner.claim(&task.id).unwrap();
    assert!(owner.admit(&lease, 1).unwrap());
    owner.attach(&lease, "original-attempt").unwrap();
    assert!(
        peer.cancel(&owner.get(&task.id).unwrap().cancel_target())
            .unwrap()
    );
    owner
        .finish(&lease, TaskTerminal::Completed, None, settled)
        .unwrap();
    let terminal = peer.get(&task.id).unwrap();
    let records = audit.events("session", 0, 100).unwrap();
    assert_eq!(
        (terminal.state, records[0].kind.as_str()),
        (TaskState::Cancelled, "run_cancelled")
    );
    assert!(terminal.cancel_requested);
    assert_eq!(terminal.worker_id.as_ref(), Some(&lease.worker));
    assert_eq!(terminal.attempt_id.as_deref(), Some("original-attempt"));
    assert_eq!(terminal.fence, lease.fence);
    assert_eq!(records.len(), 1);
    assert!(
        owner
            .finish(&lease, TaskTerminal::Completed, None, unsettled)
            .is_err()
    );
    assert!(
        !peer
            .cancel(&owner.get(&task.id).unwrap().cancel_target())
            .unwrap()
    );
    assert_eq!(peer.get(&task.id).unwrap(), terminal);
    assert_eq!(audit.events("session", 0, 100).unwrap().len(), 1);
}

#[test]
fn finish_before_peer_cancel_is_immutable() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("queue.sqlite");
    let audit = AuditDb::open(&path);
    let owner = Queue::open(&path, 1, 4, 0).unwrap();
    let peer = Queue::open(&path, 1, 4, 0).unwrap();
    let task = owner
        .enqueue_input(
            "session",
            &TaskInput {
                text: "synthetic input".into(),
                images: vec![],
            },
            &TaskIntent::General,
            4,
        )
        .unwrap();
    let lease = owner.claim(&task.id).unwrap();
    assert!(owner.admit(&lease, 1).unwrap());
    owner
        .finish(&lease, TaskTerminal::Completed, None, settled)
        .unwrap();
    assert!(
        !peer
            .cancel(&owner.get(&task.id).unwrap().cancel_target())
            .unwrap()
    );
    assert_eq!(owner.get(&task.id).unwrap().state, TaskState::Completed);
    assert_eq!(
        audit.events("session", 0, 100).unwrap()[0].kind,
        "run_finished"
    );
}

#[test]
fn unknown_terminal_takes_priority_over_accepted_cancellation() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("queue.sqlite");
    let audit = AuditDb::open(&path);
    let owner = Queue::open(&path, 1, 4, 0).unwrap();
    let peer = Queue::open(&path, 1, 4, 0).unwrap();
    let task = owner
        .enqueue_input(
            "session",
            &TaskInput {
                text: "synthetic input".into(),
                images: vec![],
            },
            &TaskIntent::General,
            4,
        )
        .unwrap();
    let lease = owner.claim(&task.id).unwrap();
    assert!(owner.admit(&lease, 1).unwrap());
    assert!(
        peer.cancel(&owner.get(&task.id).unwrap().cancel_target())
            .unwrap()
    );
    owner
        .finish(&lease, TaskTerminal::OutcomeUnknown, None, settled)
        .unwrap();
    assert_eq!(
        owner.get(&task.id).unwrap().state,
        TaskState::OutcomeUnknown
    );
    assert_eq!(
        audit.events("session", 0, 100).unwrap()[0].kind,
        "run_outcome_unknown"
    );
}

#[test]
fn unclaimed_cancel_commits_one_terminal_without_a_worker() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("queue.sqlite");
    let audit = AuditDb::open(&path);
    let owner = Queue::open(&path, 1, 4, 0).unwrap();
    let peer = Queue::open(&path, 1, 4, 0).unwrap();
    let task = owner
        .enqueue_input(
            "session",
            &TaskInput {
                text: "synthetic input".into(),
                images: vec![],
            },
            &TaskIntent::General,
            4,
        )
        .unwrap();
    assert!(
        peer.cancel(&owner.get(&task.id).unwrap().cancel_target())
            .unwrap()
    );
    assert!(
        !peer
            .cancel(&owner.get(&task.id).unwrap().cancel_target())
            .unwrap()
    );
    assert!(owner.claim(&task.id).is_err());
    assert_eq!(owner.get(&task.id).unwrap().state, TaskState::Cancelled);
    assert_eq!(owner.counts().unwrap(), (0, 0));
    let records = audit.events("session", 0, 100).unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].kind, "run_cancelled");
    assert!(records[0].data["worker_id"].is_null());
}

#[test]
fn fenced_or_wrong_worker_cannot_append_terminal_or_audit() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("queue.sqlite");
    let audit = AuditDb::open(&path);
    let owner = Queue::open(&path, 1, 4, 0).unwrap();
    let peer = Queue::open(&path, 1, 4, 0).unwrap();
    let task = owner
        .enqueue_input(
            "session",
            &TaskInput {
                text: "synthetic input".into(),
                images: vec![],
            },
            &TaskIntent::General,
            4,
        )
        .unwrap();
    let lease = owner.claim(&task.id).unwrap();
    assert!(owner.admit(&lease, 1).unwrap());
    let wrong = TaskLease {
        worker: "wrong-worker".into(),
        ..lease.clone()
    };
    assert!(
        owner
            .finish(&wrong, TaskTerminal::Completed, None, unsettled)
            .is_err()
    );
    peer.recover("session").unwrap();
    assert!(
        owner
            .finish(&lease, TaskTerminal::Completed, None, unsettled)
            .is_err()
    );
    assert_eq!(owner.get(&task.id).unwrap().state, TaskState::Interrupted);
    assert!(audit.events("session", 0, 100).unwrap().is_empty());
}

#[test]
fn failed_authority_settlement_rolls_back_task_and_audit() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("queue.sqlite");
    let audit = AuditDb::open(&path);
    let owner = Queue::open(&path, 1, 4, 0).unwrap();
    let task = owner
        .enqueue_input(
            "session",
            &TaskInput {
                text: "synthetic input".into(),
                images: vec![],
            },
            &TaskIntent::General,
            4,
        )
        .unwrap();
    let lease = owner.claim(&task.id).unwrap();
    assert!(owner.admit(&lease, 1).unwrap());
    assert!(
        owner
            .finish(
                &lease,
                TaskTerminal::Completed,
                None,
                |_| -> Result<(), QueueError> {
                    Err(QueueError::Worker("synthetic authority failure".into()))
                }
            )
            .is_err()
    );
    assert_eq!(owner.get(&task.id).unwrap().state, TaskState::Running);
    assert_eq!(owner.counts().unwrap(), (1, 0));
    assert!(audit.events("session", 0, 100).unwrap().is_empty());
}

#[test]
fn queued_peer_cancel_can_only_record_one_terminal() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("queue.sqlite");
    let audit = AuditDb::open(&path);
    let owner = Queue::open(&path, 1, 4, 0).unwrap();
    let peer = Queue::open(&path, 1, 4, 0).unwrap();
    let task = owner
        .enqueue_input(
            "session",
            &TaskInput {
                text: "synthetic input".into(),
                images: vec![],
            },
            &TaskIntent::General,
            4,
        )
        .unwrap();
    let lease = owner.claim(&task.id).unwrap();
    assert!(
        peer.cancel(&owner.get(&task.id).unwrap().cancel_target())
            .unwrap()
    );
    owner
        .finish(&lease, TaskTerminal::Cancelled, None, settled)
        .unwrap();
    assert!(
        owner
            .finish(&lease, TaskTerminal::Cancelled, None, settled)
            .is_err()
    );
    assert_eq!(audit.events("session", 0, 100).unwrap().len(), 1);
}

#[test]
fn legacy_text_column_migrates_once_into_task_input_json() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("queue.sqlite");
    let legacy = Connection::open(&path).unwrap();
    legacy
        .execute_batch(
            "CREATE TABLE runtime_queue_config (
                    singleton INTEGER PRIMARY KEY CHECK(singleton=1),
                    concurrency INTEGER NOT NULL, capacity INTEGER NOT NULL);
                 CREATE TABLE runtime_tasks (
                    ticket INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT NOT NULL UNIQUE,
                    session TEXT NOT NULL, text TEXT NOT NULL, state TEXT NOT NULL,
                    queued_at INTEGER NOT NULL, worker TEXT, attempt TEXT,
                    fence INTEGER NOT NULL DEFAULT 0,
                    cancel_requested INTEGER NOT NULL DEFAULT 0, error TEXT,
                    intent TEXT NOT NULL DEFAULT '{\"kind\":\"general\"}');
                 INSERT INTO runtime_queue_config VALUES (1,1,4);
                 INSERT INTO runtime_tasks(id,session,text,state,queued_at,intent)
                 VALUES ('legacy','session','legacy text','queued',1,'{\"kind\":\"general\"}');",
        )
        .unwrap();
    drop(legacy);

    let queue = Queue::open(&path, 1, 4, 0).unwrap();
    let tasks = queue.list("session").unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(
        tasks[0].input,
        TaskInput {
            text: "legacy text".into(),
            images: vec![],
        }
    );
    let db = Connection::open(&path).unwrap();
    let text_columns: i64 = db
        .query_row(
            "SELECT count(*) FROM pragma_table_info('runtime_tasks') WHERE name='text'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(text_columns, 0);
    let input_columns: i64 = db
        .query_row(
            "SELECT count(*) FROM pragma_table_info('runtime_tasks') WHERE name='input_json'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(input_columns, 1);
    drop(db);
    drop(queue);

    // Reopening performs no second migration and keeps the same record.
    let reopened = Queue::open(&path, 1, 4, 0).unwrap();
    assert_eq!(reopened.list("session").unwrap().len(), 1);
}

#[test]
fn try_start_child_starts_running_without_waiting_and_enforces_limits() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("queue.sqlite");
    let _audit = AuditDb::open(&path);
    let queue = Queue::open(&path, 2, 2, 0).unwrap();

    let first = queue
        .try_start_child("child-a", "first child", &TaskIntent::General, 2, 1)
        .unwrap();
    assert_eq!(first.fence, 1);
    let record = queue.get(&first.id).unwrap();
    assert_eq!(record.state, TaskState::Running);
    assert_eq!(record.worker_id.as_deref(), Some(first.worker.as_str()));
    assert_eq!(
        record.input,
        TaskInput {
            text: "first child".into(),
            images: vec![],
        }
    );
    assert!(matches!(
        queue.try_start_child("child-b", "blocked", &TaskIntent::General, 2, 1),
        Err(QueueError::QueueFull)
    ));
    assert!(matches!(
        queue.try_start_child("child-a", "duplicate", &TaskIntent::General, 2, 2),
        Err(QueueError::Busy)
    ));
    queue
        .finish(&first, TaskTerminal::Completed, None, settled)
        .unwrap();

    let second = queue
        .try_start_child("child-b", "second child", &TaskIntent::General, 2, 2)
        .unwrap();
    let third = queue
        .try_start_child("child-c", "third child", &TaskIntent::General, 2, 2)
        .unwrap();
    assert_eq!(second.fence, 1);
    assert_eq!(third.fence, 1);
    assert!(matches!(
        queue.try_start_child("child-d", "fourth child", &TaskIntent::General, 2, 2),
        Err(QueueError::QueueFull)
    ));
}

fn task_input(text: &str) -> TaskInput {
    TaskInput {
        text: text.into(),
        images: vec![],
    }
}
fn open_queue(path: &Path, concurrency: usize, capacity: usize, fairness_after: usize) -> Queue {
    // Mirror the host Runtime's coordinated adoption without its file locks.
    let resources = ResourcePolicy::default();
    let requested = QueuePolicy::new(concurrency, capacity, fairness_after, &resources).unwrap();
    if read_policy(path)
        .unwrap()
        .is_some_and(|stored| stored != requested)
    {
        adopt_policy(path, requested).unwrap();
    }
    Queue::open_with_resources(path, concurrency, capacity, fairness_after, &resources).unwrap()
}
fn enqueue(queue: &Queue, session: &str, priority: TaskPriority, capacity: usize) -> TaskRecord {
    queue
        .enqueue_scheduled(
            session,
            &task_input(session),
            &TaskIntent::General,
            priority,
            capacity,
        )
        .unwrap()
}
fn claim(queue: &Queue, task: &TaskRecord) -> TaskLease {
    queue.claim(&task.id).unwrap()
}
fn admit(queue: &Queue, lease: &TaskLease, concurrency: usize) -> bool {
    queue.admit(lease, concurrency).unwrap()
}
fn complete(queue: &Queue, lease: &TaskLease) {
    queue
        .finish(lease, TaskTerminal::Completed, None, settled)
        .unwrap();
}

#[test]
fn priority_serializes_snake_case_and_defaults_to_normal() {
    assert_eq!(TaskPriority::default(), TaskPriority::Normal);
    assert!(TaskPriority::Low < TaskPriority::Normal);
    assert!(TaskPriority::Normal < TaskPriority::High);
    assert_eq!(
        serde_json::to_string(&TaskPriority::Low).unwrap(),
        "\"low\""
    );
    assert_eq!(
        serde_json::to_string(&TaskPriority::Normal).unwrap(),
        "\"normal\""
    );
    assert_eq!(
        serde_json::to_string(&TaskPriority::High).unwrap(),
        "\"high\""
    );
}

#[test]
fn priority_order_prefers_high_then_normal_then_fifo() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("queue.sqlite");
    let queue = open_queue(&path, 4, 6, 100);
    let low = enqueue(&queue, "low", TaskPriority::Low, 6);
    let first = enqueue(&queue, "first", TaskPriority::Normal, 6);
    let second = enqueue(&queue, "second", TaskPriority::Normal, 6);
    let high = enqueue(&queue, "high", TaskPriority::High, 6);
    let low = claim(&queue, &low);
    let first = claim(&queue, &first);
    let second = claim(&queue, &second);
    let high = claim(&queue, &high);

    assert!(!admit(&queue, &low, 4));
    assert!(!admit(&queue, &first, 4));
    assert!(!admit(&queue, &second, 4));
    assert!(admit(&queue, &high, 4));
    assert!(!admit(&queue, &low, 4));
    assert!(!admit(&queue, &second, 4));
    assert!(admit(&queue, &first, 4));
    assert!(!admit(&queue, &low, 4));
    assert!(admit(&queue, &second, 4));
    assert!(admit(&queue, &low, 4));
    assert_eq!(queue.dispatch_count().unwrap(), 4);
}

#[test]
fn waiting_entry_is_promoted_after_fairness_window() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("queue.sqlite");
    let _audit = AuditDb::open(&path);
    let queue = open_queue(&path, 1, 6, 2);
    let low = enqueue(&queue, "low", TaskPriority::Low, 6);
    let low = claim(&queue, &low);
    let ready_order = queue.get(&low.id).unwrap().ready_order;
    assert!(ready_order.is_some());

    for session in ["high-one", "high-two"] {
        let high = enqueue(&queue, session, TaskPriority::High, 6);
        let high = claim(&queue, &high);
        assert!(admit(&queue, &high, 1));
        complete(&queue, &high);
    }
    assert_eq!(queue.dispatch_count().unwrap(), 2);

    // New high-priority arrivals cannot overtake an entry that has
    // already waited the full fairness window.
    let late_three = enqueue(&queue, "high-three", TaskPriority::High, 6);
    let late_three = claim(&queue, &late_three);
    let late_four = enqueue(&queue, "high-four", TaskPriority::High, 6);
    let late_four = claim(&queue, &late_four);
    assert!(!admit(&queue, &late_three, 1));
    assert!(!admit(&queue, &late_four, 1));
    assert!(admit(&queue, &low, 1));
    let admitted = queue.get(&low.id).unwrap();
    assert!(admitted.holds_slot);
    assert_eq!(admitted.dispatches, 1);
    assert_eq!(admitted.ready_order, ready_order);
    assert_eq!(queue.dispatch_count().unwrap(), 3);
}

#[test]
fn fairness_state_and_dispatch_count_survive_reopen() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("queue.sqlite");
    let _audit = AuditDb::open(&path);
    let queue = open_queue(&path, 1, 6, 2);
    let low = enqueue(&queue, "low", TaskPriority::Low, 6);
    let low = claim(&queue, &low);
    let ready = queue.get(&low.id).unwrap();
    drop(queue);

    let queue = open_queue(&path, 1, 6, 2);
    let reopened = queue.get(&low.id).unwrap();
    assert_eq!(reopened.ready_order, ready.ready_order);
    assert_eq!(reopened.ready_since_dispatch, ready.ready_since_dispatch);
    assert_eq!(queue.dispatch_count().unwrap(), 0);
    for session in ["high-one", "high-two"] {
        let high = enqueue(&queue, session, TaskPriority::High, 6);
        let high = claim(&queue, &high);
        assert!(admit(&queue, &high, 1));
        complete(&queue, &high);
    }
    assert_eq!(queue.dispatch_count().unwrap(), 2);
    let late = enqueue(&queue, "high-three", TaskPriority::High, 6);
    let late = claim(&queue, &late);
    assert!(!admit(&queue, &late, 1));
    assert!(admit(&queue, &low, 1));
}

#[test]
fn reclaim_preserves_ready_order_and_fences_the_old_worker() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("queue.sqlite");
    let queue = open_queue(&path, 2, 4, 100);
    let task = enqueue(&queue, "reclaim", TaskPriority::Normal, 4);
    let first = claim(&queue, &task);
    let before = queue.get(&task.id).unwrap();
    assert!(before.ready_order.is_some());
    let second = claim(&queue, &task);
    let after = queue.get(&task.id).unwrap();
    assert_eq!(after.ready_order, before.ready_order);
    assert_eq!(after.ready_since_dispatch, before.ready_since_dispatch);
    assert_eq!(after.fence, first.fence + 1);
    assert_eq!(after.worker_id.as_deref(), Some(second.worker.as_str()));
    assert!(queue.admit(&first, 2).is_err());
    assert!(admit(&queue, &second, 2));
    assert!(queue.claim(&task.id).is_err());
}

#[test]
fn child_start_cannot_bypass_ready_work_and_counts_as_dispatch() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("queue.sqlite");
    let _audit = AuditDb::open(&path);
    let queue = open_queue(&path, 2, 4, 100);
    assert_eq!(queue.dispatch_count().unwrap(), 0);
    let child = queue
        .try_start_child("child-a", "child", &TaskIntent::General, 4, 2)
        .unwrap();
    assert_eq!(queue.dispatch_count().unwrap(), 1);
    let record = queue.get(&child.id).unwrap();
    assert!(record.holds_slot);
    assert_eq!(record.dispatches, 1);
    complete(&queue, &child);

    let queued = enqueue(&queue, "waiting", TaskPriority::Normal, 4);
    let _lease = claim(&queue, &queued);
    assert!(matches!(
        queue.try_start_child("child-b", "blocked", &TaskIntent::General, 4, 2),
        Err(QueueError::Busy)
    ));
    assert_eq!(queue.dispatch_count().unwrap(), 1);
}

#[test]
fn park_releases_slot_but_keeps_accepted_capacity_and_readies_again() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("queue.sqlite");
    let queue = open_queue(&path, 1, 1, 2);
    let task = enqueue(&queue, "approval", TaskPriority::Normal, 1);
    let lease = claim(&queue, &task);
    assert!(admit(&queue, &lease, 1));
    assert_eq!(queue.counts().unwrap(), (1, 0));
    queue.park(&lease).unwrap();
    let parked = queue.get(&task.id).unwrap();
    assert_eq!(parked.state, TaskState::WaitingApproval);
    assert!(!parked.holds_slot);
    assert!(parked.ready_order.is_none());
    assert!(parked.ready_since_dispatch.is_none());
    assert_eq!(queue.counts().unwrap(), (0, 0));
    // Waiting approval keeps the accepted capacity but is not executable.
    assert!(matches!(
        queue.enqueue_scheduled(
            "other",
            &task_input("other"),
            &TaskIntent::General,
            TaskPriority::Normal,
            1
        ),
        Err(QueueError::QueueFull)
    ));
    assert!(queue.admit(&lease, 1).is_err());
    let wrong = TaskLease {
        worker: "wrong-worker".into(),
        ..lease.clone()
    };
    assert!(queue.park(&wrong).is_err());
    assert!(queue.ready(&wrong).is_err());
    queue.ready(&lease).unwrap();
    let reordered = queue.get(&task.id).unwrap();
    assert_eq!(reordered.state, TaskState::Queued);
    assert!(reordered.ready_order.is_some());
    assert_eq!(
        reordered.ready_since_dispatch,
        Some(queue.dispatch_count().unwrap())
    );
    queue.ready(&lease).unwrap();
    assert_eq!(
        queue.get(&task.id).unwrap().ready_order,
        reordered.ready_order
    );
    assert!(admit(&queue, &lease, 1));
    assert_eq!(queue.get(&task.id).unwrap().dispatches, 2);
    queue.park(&lease).unwrap();
}

#[test]
fn cancelling_waiting_approval_blocks_readmission() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("queue.sqlite");
    let queue = open_queue(&path, 1, 1, 2);
    let peer = open_queue(&path, 1, 1, 2);
    let task = enqueue(&queue, "approval", TaskPriority::Normal, 1);
    let lease = claim(&queue, &task);
    assert!(admit(&queue, &lease, 1));
    queue.park(&lease).unwrap();
    assert!(
        peer.cancel(&queue.get(&task.id).unwrap().cancel_target())
            .unwrap()
    );
    assert!(matches!(queue.ready(&lease), Err(QueueError::Cancelled)));
    let record = queue.get(&task.id).unwrap();
    assert_eq!(record.state, TaskState::WaitingApproval);
    assert!(record.cancel_requested);
    assert_eq!(queue.counts().unwrap(), (0, 0));
}

#[test]
fn failed_storage_does_not_consume_dispatch_or_drop_task() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("queue.sqlite");
    let queue = open_queue(&path, 1, 4, 2);
    let task = enqueue(&queue, "synthetic", TaskPriority::Normal, 4);
    let lease = claim(&queue, &task);
    lock(&queue.connection)
        .execute_batch(
            "CREATE TRIGGER fail_admit BEFORE UPDATE ON runtime_tasks
                 WHEN NEW.state='running'
                 BEGIN SELECT RAISE(ABORT,'synthetic admit failure'); END;",
        )
        .unwrap();
    assert!(queue.admit(&lease, 1).is_err());
    assert_eq!(queue.dispatch_count().unwrap(), 0);
    let after = queue.get(&task.id).unwrap();
    assert_eq!(after.state, TaskState::Queued);
    assert!(!after.holds_slot);
    assert_eq!(after.dispatches, 0);
    lock(&queue.connection)
        .execute_batch("DROP TRIGGER fail_admit")
        .unwrap();
    assert!(admit(&queue, &lease, 1));
    assert_eq!(queue.dispatch_count().unwrap(), 1);

    lock(&queue.connection)
        .execute_batch(
            "CREATE TRIGGER fail_enqueue BEFORE INSERT ON runtime_tasks
                 BEGIN SELECT RAISE(ABORT,'synthetic enqueue failure'); END;",
        )
        .unwrap();
    assert!(
        queue
            .enqueue_scheduled(
                "other",
                &task_input("other"),
                &TaskIntent::General,
                TaskPriority::Low,
                4
            )
            .is_err()
    );
    lock(&queue.connection)
        .execute_batch("DROP TRIGGER fail_enqueue")
        .unwrap();
    assert!(queue.list("other").unwrap().is_empty());
    assert_eq!(queue.dispatch_count().unwrap(), 1);
}

#[test]
fn counter_overflow_fails_closed_without_wrapping() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("queue.sqlite");
    let queue = open_queue(&path, 1, 4, 2);
    let task = enqueue(&queue, "synthetic", TaskPriority::Normal, 4);
    let lease = claim(&queue, &task);
    lock(&queue.connection)
        .execute(
            "UPDATE runtime_queue_config SET dispatch_count=?1 WHERE singleton=1",
            [i64::MAX],
        )
        .unwrap();
    assert!(queue.admit(&lease, 1).is_err());
    assert_eq!(queue.dispatch_count().unwrap(), i64::MAX as u64);
    let after = queue.get(&task.id).unwrap();
    assert_eq!(after.state, TaskState::Queued);
    assert!(!after.holds_slot);
    assert_eq!(after.dispatches, 0);
}

#[test]
fn recover_keeps_queued_without_attempt_and_interrupts_attempted_work() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("queue.sqlite");
    let queue = open_queue(&path, 1, 4, 2);
    let ready = enqueue(&queue, "ready", TaskPriority::Normal, 4);
    let ready_lease = claim(&queue, &ready);
    let running = enqueue(&queue, "running", TaskPriority::High, 4);
    let running_lease = claim(&queue, &running);
    assert!(admit(&queue, &running_lease, 1));
    queue.attach(&running_lease, "attempt-running").unwrap();

    let old_target = queue.get(&ready.id).unwrap().cancel_target();
    queue.recover("ready").unwrap();
    let kept = queue.get(&ready.id).unwrap();
    assert_eq!(kept.state, TaskState::Queued);
    assert!(!kept.cancel_requested);
    assert!(kept.worker_id.is_none());
    assert_eq!(kept.fence, ready_lease.fence + 1);
    assert!(kept.ready_order.is_some());
    assert!(kept.ready_since_dispatch.is_some());
    assert!(queue.check(&ready_lease).is_err());
    assert!(queue.admit(&ready_lease, 1).is_err());
    assert!(
        queue
            .finish(&ready_lease, TaskTerminal::Completed, None, unsettled)
            .is_err()
    );
    assert!(matches!(
        queue.cancel(&old_target),
        Err(QueueError::StaleCancellation)
    ));
    // Repeating recovery does not advance the fence again.
    queue.recover("ready").unwrap();
    assert_eq!(queue.get(&ready.id).unwrap().fence, ready_lease.fence + 1);
    let retaken = claim(&queue, &ready);
    assert_ne!(retaken.worker, ready_lease.worker);
    assert_eq!(retaken.fence, ready_lease.fence + 2);
    let reclaimed = queue.get(&ready.id).unwrap();
    assert_eq!(reclaimed.ready_order, kept.ready_order);
    assert_eq!(reclaimed.ready_since_dispatch, kept.ready_since_dispatch);
    assert_eq!(
        reclaimed.worker_id.as_deref(),
        Some(retaken.worker.as_str())
    );

    queue.recover("running").unwrap();
    let interrupted = queue.get(&running.id).unwrap();
    assert_eq!(interrupted.state, TaskState::Interrupted);
    assert!(interrupted.cancel_requested);
    assert!(!interrupted.holds_slot);
    assert_eq!(interrupted.fence, running_lease.fence + 1);
    assert_eq!(queue.counts().unwrap(), (0, 1));
}

#[test]
fn snapshot_is_bounded_to_live_tasks() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("queue.sqlite");
    let _audit = AuditDb::open(&path);
    let queue = open_queue(&path, 1, 4, 2);
    assert!(matches!(
        queue.snapshot(0),
        Err(QueueError::InvalidInput(_))
    ));
    assert!(matches!(
        queue.snapshot(1001),
        Err(QueueError::InvalidInput(_))
    ));
    let first = enqueue(&queue, "first", TaskPriority::Normal, 4);
    let second = enqueue(&queue, "second", TaskPriority::High, 4);
    assert_eq!(queue.snapshot(1).unwrap().len(), 1);
    let live = queue.snapshot(10).unwrap();
    assert_eq!(live.len(), 2);
    assert_eq!(live[1].priority, TaskPriority::High);
    assert!(live[1].ready_order.is_none());
    let lease = claim(&queue, &first);
    assert!(queue.snapshot(10).unwrap()[0].ready_order.is_some());
    complete(&queue, &lease);
    assert_eq!(queue.snapshot(10).unwrap().len(), 1);
    assert_eq!(queue.snapshot(10).unwrap()[0].id, second.id);
}

#[test]
fn legacy_schema_migrates_priority_slots_and_ready_order() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("queue.sqlite");
    let legacy = Connection::open(&path).unwrap();
    legacy
        .execute_batch(
            "CREATE TABLE runtime_queue_config (
                    singleton INTEGER PRIMARY KEY CHECK(singleton=1),
                    concurrency INTEGER NOT NULL, capacity INTEGER NOT NULL);
                 CREATE TABLE runtime_tasks (
                    ticket INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT NOT NULL UNIQUE,
                    session TEXT NOT NULL, input_json TEXT NOT NULL, state TEXT NOT NULL,
                    queued_at INTEGER NOT NULL, worker TEXT, attempt TEXT,
                    fence INTEGER NOT NULL DEFAULT 0,
                    cancel_requested INTEGER NOT NULL DEFAULT 0, error TEXT,
                    intent TEXT NOT NULL DEFAULT '{\"kind\":\"general\"}');
                 INSERT INTO runtime_queue_config VALUES (1,1,4);
                 INSERT INTO runtime_tasks(id,session,input_json,state,queued_at,worker,attempt,fence,intent) VALUES
                   ('run','run','{\"text\":\"run\",\"images\":[]}','running',1,'worker-run',NULL,1,'{\"kind\":\"general\"}'),
                   ('old','old','{\"text\":\"old\",\"images\":[]}','queued',2,'worker-old',NULL,1,'{\"kind\":\"general\"}'),
                   ('new','new','{\"text\":\"new\",\"images\":[]}','queued',3,'worker-new',NULL,1,'{\"kind\":\"general\"}'),
                   ('idle','idle','{\"text\":\"idle\",\"images\":[]}','queued',4,NULL,NULL,0,'{\"kind\":\"general\"}');",
        )
        .unwrap();
    drop(legacy);
    let _audit = AuditDb::open(&path);

    let queue = open_queue(&path, 1, 4, 0);
    assert_eq!(queue.dispatch_count().unwrap(), 0);
    let running = queue.get("run").unwrap();
    assert_eq!(running.priority, TaskPriority::Normal);
    assert!(running.holds_slot);
    assert_eq!(running.dispatches, 1);
    let old = queue.get("old").unwrap();
    let new = queue.get("new").unwrap();
    assert!(old.ready_order.is_some());
    assert!(new.ready_order.is_some());
    assert!(old.ready_order < new.ready_order);
    assert_eq!(old.ready_since_dispatch, Some(0));
    let idle = queue.get("idle").unwrap();
    assert!(idle.ready_order.is_none());
    assert!(!idle.holds_slot);
    assert_eq!(idle.priority, TaskPriority::Normal);
    let reclaimed = claim(&queue, &idle);
    assert!(queue.get(&idle.id).unwrap().ready_order > new.ready_order);
    assert_eq!(queue.get(&idle.id).unwrap().ready_since_dispatch, Some(0));
    for (id, worker) in [
        ("run", "worker-run"),
        ("old", "worker-old"),
        ("new", "worker-new"),
    ] {
        queue
            .finish(
                &TaskLease {
                    id: id.into(),
                    worker: worker.into(),
                    fence: 1,
                },
                TaskTerminal::Completed,
                None,
                settled,
            )
            .unwrap();
    }
    assert!(admit(&queue, &reclaimed, 1));
}

#[test]
fn child_capacity_precheck_counts_live_and_slots_consistently() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("queue.sqlite");
    let _audit = AuditDb::open(&path);
    let queue = open_queue(&path, 1, 3, 100);
    queue.check_child_capacity(3, 1).unwrap();

    let parked = enqueue(&queue, "parked", TaskPriority::Normal, 3);
    let parked_lease = claim(&queue, &parked);
    assert!(admit(&queue, &parked_lease, 1));
    queue.park(&parked_lease).unwrap();
    assert!(queue.check_child_capacity(3, 1).is_ok());
    assert!(matches!(
        queue.check_child_capacity(1, 1),
        Err(QueueError::QueueFull)
    ));

    let draining = enqueue(&queue, "draining", TaskPriority::Normal, 3);
    let draining_lease = claim(&queue, &draining);
    assert!(admit(&queue, &draining_lease, 1));
    queue.block(&draining_lease, "synthetic cleanup").unwrap();
    let record = queue.get(&draining.id).unwrap();
    assert_eq!(record.state, TaskState::Draining);
    assert!(record.holds_slot);
    assert!(matches!(
        queue.check_child_capacity(3, 1),
        Err(QueueError::QueueFull)
    ));
    assert!(queue.check_child_capacity(3, 2).is_ok());

    let ready = enqueue(&queue, "ready", TaskPriority::High, 3);
    let _ready_lease = claim(&queue, &ready);
    assert!(matches!(
        queue.check_child_capacity(3, 2),
        Err(QueueError::Busy)
    ));
}

#[test]
fn legacy_config_adopts_explicit_fairness() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("queue.sqlite");
    let legacy = Connection::open(&path).unwrap();
    legacy
        .execute_batch(
            "CREATE TABLE runtime_queue_config (
                    singleton INTEGER PRIMARY KEY CHECK(singleton=1),
                    concurrency INTEGER NOT NULL, capacity INTEGER NOT NULL);
                 INSERT INTO runtime_queue_config VALUES (1,1,4);",
        )
        .unwrap();
    drop(legacy);

    // The runtime default is 8. An old database has no fairness column, so
    // it must adopt the explicit value instead of keeping a zero default.
    let queue = open_queue(&path, 1, 4, 8);
    assert_eq!(queue.dispatch_count().unwrap(), 0);
    drop(queue);
    // With the explicit policy already persisted and no live task, a
    // trusted opener may change the fairness window.
    let changed = open_queue(&path, 1, 4, 0);
    assert_eq!(changed.dispatch_count().unwrap(), 0);
    drop(changed);
    // A later opener requests the changed value and keeps it.
    open_queue(&path, 1, 4, 0);
}

#[test]
fn negative_ready_fields_are_rejected_not_wrapped() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("queue.sqlite");
    let _audit = AuditDb::open(&path);
    let queue = open_queue(&path, 1, 4, 2);
    let task = enqueue(&queue, "corrupt-order", TaskPriority::Normal, 4);
    lock(&queue.connection)
        .execute(
            "UPDATE runtime_tasks SET ready_since_dispatch=0,ready_order=-1 WHERE id=?1",
            [&task.id],
        )
        .unwrap();
    assert!(queue.get(&task.id).is_err());
    assert!(queue.claim(&task.id).is_err());
    assert!(queue.snapshot(10).is_err());

    let other = enqueue(&queue, "corrupt-since", TaskPriority::Normal, 4);
    lock(&queue.connection)
        .execute(
            "UPDATE runtime_tasks SET ready_since_dispatch=-2,ready_order=1 WHERE id=?1",
            [&other.id],
        )
        .unwrap();
    assert!(queue.claim(&other.id).is_err());
}

#[test]
fn recover_fence_overflow_fails_closed() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("queue.sqlite");
    let _audit = AuditDb::open(&path);
    let queue = open_queue(&path, 1, 4, 2);
    let task = enqueue(&queue, "max-fence", TaskPriority::Normal, 4);
    let lease = claim(&queue, &task);
    assert!(admit(&queue, &lease, 1));
    lock(&queue.connection)
        .execute(
            "UPDATE runtime_tasks SET fence=?1 WHERE id=?2",
            params![i64::MAX, &task.id],
        )
        .unwrap();
    assert!(queue.recover("max-fence").is_err());
    let record = queue.get(&task.id).unwrap();
    assert_eq!(record.state, TaskState::Running);
    assert_eq!(record.fence, i64::MAX as u64);
    assert!(record.holds_slot);

    // The detach path for a queued dead claim is also range checked.
    let queued = enqueue(&queue, "max-queued", TaskPriority::Normal, 4);
    let queued_lease = claim(&queue, &queued);
    let before = queue.get(&queued.id).unwrap();
    lock(&queue.connection)
        .execute(
            "UPDATE runtime_tasks SET fence=?1 WHERE id=?2",
            params![i64::MAX, &queued.id],
        )
        .unwrap();
    assert!(queue.recover("max-queued").is_err());
    let after = queue.get(&queued.id).unwrap();
    assert_eq!(after.state, TaskState::Queued);
    assert_eq!(
        after.worker_id.as_deref(),
        Some(queued_lease.worker.as_str())
    );
    assert_eq!(after.fence, i64::MAX as u64);
    assert_eq!(after.ready_order, before.ready_order);
    assert_eq!(after.ready_since_dispatch, before.ready_since_dispatch);
}

#[test]
fn recover_terminates_cancelled_claimed_queue_with_one_audit() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("queue.sqlite");
    let audit = AuditDb::open(&path);
    let queue = open_queue(&path, 1, 4, 2);
    let peer = open_queue(&path, 1, 4, 2);

    let task = enqueue(&queue, "cancelled", TaskPriority::Normal, 4);
    let lease = claim(&queue, &task);
    assert!(
        peer.cancel(&queue.get(&task.id).unwrap().cancel_target())
            .unwrap()
    );
    let cancelled = queue.get(&task.id).unwrap();
    assert_eq!(cancelled.state, TaskState::Queued);
    assert!(cancelled.cancel_requested);

    // The original worker is externally proven stopped. Recovery must end
    // the claimed cancellation instead of leaving it occupying capacity.
    queue.recover("cancelled").unwrap();
    let terminal = queue.get(&task.id).unwrap();
    assert_eq!(terminal.state, TaskState::Cancelled);
    assert!(terminal.cancel_requested);
    assert!(!terminal.holds_slot);
    assert_eq!(terminal.fence, cancelled.fence + 1);
    assert_eq!(queue.counts().unwrap(), (0, 0));
    let records = audit.events("cancelled", 0, 100).unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].kind, "run_cancelled");
    assert_eq!(records[0].data["worker_id"], lease.worker);

    queue.recover("cancelled").unwrap();
    assert_eq!(audit.events("cancelled", 0, 100).unwrap().len(), 1);
    assert_eq!(queue.get(&task.id).unwrap(), terminal);

    // A non-cancelled queued entry without an attempt is retained.
    let kept = enqueue(&queue, "kept", TaskPriority::Normal, 4);
    let kept_lease = claim(&queue, &kept);
    queue.recover("kept").unwrap();
    let retained = queue.get(&kept.id).unwrap();
    assert_eq!(retained.state, TaskState::Queued);
    assert!(retained.worker_id.is_none());
    assert_eq!(retained.fence, kept_lease.fence + 1);
    assert!(retained.ready_order.is_some());
    assert!(audit.events("kept", 0, 100).unwrap().is_empty());
}

#[test]
fn snapshot_page_uses_ticket_cursor_without_duplicates() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("queue.sqlite");
    let _audit = AuditDb::open(&path);
    let queue = open_queue(&path, 1, 8, 2);
    let mut ids = Vec::new();
    for name in ["a", "b", "c", "d", "e"] {
        ids.push(enqueue(&queue, name, TaskPriority::Normal, 8).id);
    }

    let mut seen = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let page = queue.snapshot_page(after.as_deref(), 2).unwrap();
        if page.is_empty() {
            break;
        }
        after = Some(page.last().unwrap().id.clone());
        seen.extend(page.into_iter().map(|task| task.id));
    }
    assert_eq!(seen, ids);
    assert_eq!(
        seen.iter().collect::<std::collections::HashSet<_>>().len(),
        seen.len()
    );

    assert!(matches!(
        queue.snapshot_page(Some("missing"), 2),
        Err(QueueError::InvalidInput(_))
    ));
    assert!(matches!(
        queue.snapshot_page(None, 0),
        Err(QueueError::InvalidInput(_))
    ));
    assert!(matches!(
        queue.snapshot_page(None, 1001),
        Err(QueueError::InvalidInput(_))
    ));

    // Terminal rows keep their ticket, so a completed id remains usable.
    let terminal = enqueue(&queue, "terminal", TaskPriority::Normal, 8);
    let lease = claim(&queue, &terminal);
    complete(&queue, &lease);
    assert!(
        queue
            .snapshot_page(Some(&terminal.id), 10)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn recovered_dead_claim_is_detached_and_unrelated_work_runs() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("queue.sqlite");
    let _audit = AuditDb::open(&path);
    let queue = open_queue(&path, 2, 4, 100);
    let dormant = enqueue(&queue, "dormant", TaskPriority::High, 4);
    let dormant_lease = claim(&queue, &dormant);
    let old_target = queue.get(&dormant.id).unwrap().cancel_target();
    queue.recover("dormant").unwrap();
    let retained = queue.get(&dormant.id).unwrap();

    // Input, priority, and the ready position survive recovery.
    assert_eq!(retained.state, TaskState::Queued);
    assert_eq!(retained.input, dormant.input);
    assert_eq!(retained.priority, TaskPriority::High);
    assert!(retained.ready_order.is_some());
    assert!(retained.ready_since_dispatch.is_some());
    // The dead claim is detached and fenced.
    assert!(retained.worker_id.is_none());
    assert_eq!(retained.fence, dormant_lease.fence + 1);
    assert!(queue.check(&dormant_lease).is_err());
    assert!(queue.admit(&dormant_lease, 2).is_err());
    assert!(
        queue
            .finish(&dormant_lease, TaskTerminal::Completed, None, unsettled)
            .is_err()
    );
    assert!(matches!(
        queue.cancel(&old_target),
        Err(QueueError::StaleCancellation)
    ));
    // Repeating recovery does not advance the fence again.
    queue.recover("dormant").unwrap();
    assert_eq!(
        queue.get(&dormant.id).unwrap().fence,
        dormant_lease.fence + 1
    );

    // A free slot is usable even though the dormant task keeps capacity.
    let unrelated = enqueue(&queue, "unrelated", TaskPriority::Normal, 4);
    let unrelated_lease = claim(&queue, &unrelated);
    assert!(admit(&queue, &unrelated_lease, 2));

    // A new claim keeps the fairness position and advances the fence.
    let retaken = claim(&queue, &dormant);
    assert_eq!(retaken.fence, dormant_lease.fence + 2);
    let reclaimed = queue.get(&dormant.id).unwrap();
    assert_eq!(reclaimed.ready_order, retained.ready_order);
    assert_eq!(
        reclaimed.ready_since_dispatch,
        retained.ready_since_dispatch
    );
    assert_eq!(
        reclaimed.worker_id.as_deref(),
        Some(retaken.worker.as_str())
    );
}
