use cayw_agent::AgentEvent;
use cayw_core::{
    ApprovalDecision, ApprovalInput, CoreError, CoreEvent, SessionState, SessionStore,
};
use serde_json::json;
use std::{
    path::PathBuf,
    process::{Command, Stdio},
    sync::{Arc, Barrier},
};
use uuid::Uuid;

struct Database(PathBuf);
impl Database {
    fn new() -> Self {
        let directory =
            std::env::temp_dir().join(format!("cayw-core-ownership-{}", Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        Self(directory)
    }
    fn path(&self) -> PathBuf {
        self.0.join("session.sqlite")
    }
    fn open(&self) -> SessionStore {
        SessionStore::open(self.path()).unwrap()
    }
}
impl Drop for Database {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn approval_input() -> ApprovalInput {
    ApprovalInput {
        task_id: "task".into(),
        target: cayw_core::ApprovalTarget::ToolCall {
            call_id: "approval-parent".into(),
        },
        capability: "files.write".into(),
        scope: "note.txt".into(),
        reason: "host decision required".into(),
    }
}

fn plan_approval_parent(store: &SessionStore, lease: &cayw_core::AttemptLease) {
    use cayw_ai::{AssistantMessage, Content, Message, Model, StopReason, ToolCall};
    store
        .append_agent_event(lease, &AgentEvent::AgentStart)
        .unwrap();
    store
        .append_agent_event(lease, &AgentEvent::TurnStart)
        .unwrap();
    let mut assistant = AssistantMessage::new(&Model::new("fixture", "offline"));
    assistant.stop_reason = StopReason::ToolUse;
    assistant.content = vec![Content::ToolCall(ToolCall::new(
        "approval-parent",
        "write",
        json!({}),
    ))];
    let message = Message::Assistant(assistant);
    store
        .append_agent_event(
            lease,
            &AgentEvent::MessageStart {
                message: message.clone(),
            },
        )
        .unwrap();
    store
        .append_agent_event(lease, &AgentEvent::MessageEnd { message })
        .unwrap();
}

// This catches a read-only lease check followed by an unfenced audit write.
#[test]
fn every_worker_write_rejects_a_recovered_or_forged_lease_without_appending() {
    let database = Database::new();
    let first = database.open();
    let second = database.open();
    let id = first.create_session("ownership").unwrap().id;
    let old = first.begin_attempt(&id, "worker-a").unwrap();
    first.validate_lease(&old).unwrap();
    let recorded = first
        .append_host_event(
            &old,
            "capability_requested",
            json!({"capability":"files.write"}),
        )
        .unwrap();
    assert_eq!(recorded.attempt.as_ref(), Some(&old));
    second.recover(&id).unwrap();
    let current = second.begin_attempt(&id, "worker-b").unwrap();
    let before = first.snapshot(&id).unwrap();
    let mut forged = current.clone();
    forged.fence = old.fence;
    for lease in [&old, &forged] {
        assert!(matches!(
            first.validate_lease(lease),
            Err(CoreError::StaleLease)
        ));
        assert!(matches!(
            first.append_host_event(lease, "write", json!({})),
            Err(CoreError::StaleLease)
        ));
        assert!(matches!(
            first.append_agent_event(lease, &AgentEvent::AgentStart),
            Err(CoreError::StaleLease)
        ));
        assert!(matches!(
            first.request_approval(lease, approval_input()),
            Err(CoreError::StaleLease)
        ));
        assert!(matches!(
            first.mark_tool_started(lease, "anything"),
            Err(CoreError::StaleLease)
        ));
        assert!(matches!(
            first.finish_attempt(lease),
            Err(CoreError::StaleLease)
        ));
    }
    assert_eq!(first.snapshot(&id).unwrap(), before);
    assert!(first.events(&id, before.cursor, 10).unwrap().is_empty());
    assert!(first.append_host_event(&current, " ", json!({})).is_err());
    first
        .append_host_event(&current, "capability_checked", json!({"allowed":false}))
        .unwrap();
}

#[test]
fn host_control_decisions_require_exact_cursor_and_current_execution_identity() {
    let database = Database::new();
    let store = database.open();
    let initial = store.create_session("host decisions").unwrap();
    let decision = store
        .append_control_event(
            &initial.id,
            initial.cursor,
            None,
            "host",
            "module_bound",
            json!({"module":"workflow"}),
        )
        .unwrap();
    assert!(decision.attempt.is_none());
    assert!(
        matches!(decision.event, CoreEvent::Host { ref kind, ref data }
        if kind == "module_bound" && data["actor"] == "host")
    );
    assert!(
        store
            .append_control_event(
                &initial.id,
                initial.cursor,
                None,
                "host",
                "stale",
                json!({})
            )
            .is_err()
    );
    let lease = store.begin_attempt(&initial.id, "first").unwrap();
    let snapshot = store.snapshot(&initial.id).unwrap();
    assert!(matches!(
        store.append_control_event(
            &initial.id,
            snapshot.cursor,
            None,
            "host",
            "missing_identity",
            json!({})
        ),
        Err(CoreError::StaleLease)
    ));
    store
        .append_control_event(
            &initial.id,
            snapshot.cursor,
            Some(&lease),
            "host",
            "module_frozen",
            json!({}),
        )
        .unwrap();
    let other = database.open();
    other.recover(&initial.id).unwrap();
    let current = other.begin_attempt(&initial.id, "second").unwrap();
    let before = other.snapshot(&initial.id).unwrap();
    assert!(matches!(
        store.append_control_event(
            &initial.id,
            before.cursor,
            Some(&lease),
            "host",
            "old",
            json!({})
        ),
        Err(CoreError::StaleLease)
    ));
    assert!(
        store
            .append_control_event(
                &initial.id,
                before.cursor,
                Some(&current),
                " ",
                "empty_actor",
                json!({})
            )
            .is_err()
    );
    assert_eq!(store.snapshot(&initial.id).unwrap(), before);
    other
        .append_control_event(
            &initial.id,
            before.cursor,
            Some(&current),
            "host",
            "current",
            json!({}),
        )
        .unwrap();
}

// Resolved rows must stay queryable, but a later request never overrides a denial.
#[test]
fn approvals_remain_readable_after_decision_and_counts_never_grant_authority() {
    let database = Database::new();
    let store = database.open();
    let id = store.create_session("approvals").unwrap().id;
    let lease = store.begin_attempt(&id, "worker").unwrap();
    plan_approval_parent(&store, &lease);
    let first = store.request_approval(&lease, approval_input()).unwrap();
    let second = store.request_approval(&lease, approval_input()).unwrap();
    assert_eq!(store.approvals(&id).unwrap().len(), 2);
    assert_eq!(
        store.snapshot(&id).unwrap().state,
        SessionState::AwaitingApproval
    );
    assert!(matches!(
        store.finish_attempt(&lease),
        Err(CoreError::PendingApproval)
    ));
    let before = store.snapshot(&id).unwrap();
    assert!(
        store
            .decide_approval(&id, &first.id, ApprovalDecision::Approve, " ")
            .is_err()
    );
    assert_eq!(store.snapshot(&id).unwrap(), before);
    store
        .decide_approval(&id, &first.id, ApprovalDecision::Deny, "host:alice")
        .unwrap();
    store
        .decide_approval(&id, &second.id, ApprovalDecision::Approve, "host:bob")
        .unwrap();
    assert!(store.snapshot(&id).unwrap().pending_approvals.is_empty());
    drop(store);
    let store = database.open();
    let resolved = store.approval(&id, &first.id).unwrap();
    assert_eq!(
        resolved.decision.as_ref().unwrap().decision,
        ApprovalDecision::Deny
    );
    assert_eq!(resolved.decision.as_ref().unwrap().actor, "host:alice");
    assert!(matches!(
        store.approval(&id, "missing"),
        Err(CoreError::ApprovalNotFound(_))
    ));
    assert!(
        store
            .decide_approval(&id, &first.id, ApprovalDecision::Approve, "host")
            .is_err()
    );
    assert_eq!(store.approvals(&id).unwrap().len(), 2);
    store.recover(&id).unwrap();
}

#[test]
fn competing_host_decisions_commit_exactly_one_terminal() {
    let database = Database::new();
    let first = database.open();
    let second = database.open();
    let id = first.create_session("decision race").unwrap().id;
    let lease = first.begin_attempt(&id, "worker").unwrap();
    plan_approval_parent(&first, &lease);
    let approval = first.request_approval(&lease, approval_input()).unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let workers = [
        (first.clone(), ApprovalDecision::Approve),
        (second, ApprovalDecision::Deny),
    ]
    .into_iter()
    .map(|(store, decision)| {
        let id = id.clone();
        let approval = approval.clone();
        let barrier = barrier.clone();
        std::thread::spawn(move || {
            barrier.wait();
            store.decide_approval(&id, &approval.id, decision, "host")
        })
    })
    .collect::<Vec<_>>();
    let results = workers
        .into_iter()
        .map(|w| w.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert_eq!(results.iter().filter(|r| r.is_err()).count(), 1);
    let events = first.events(&id, 0, 100).unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|r| matches!(r.event, CoreEvent::ApprovalDecided { .. }))
            .count(),
        1
    );
}

// The child is a genuinely separate OS process, not another Arc or connection.
#[test]
fn child_process_fences_a_worker() {
    let Some(path) = std::env::var_os("CAYW_CORE_FENCE_DB") else {
        return;
    };
    let session_id = std::env::var("CAYW_CORE_FENCE_SESSION").unwrap();
    let store = SessionStore::open(path).unwrap();
    assert!(matches!(
        store.begin_attempt(&session_id, "child"),
        Err(CoreError::Busy)
    ));
    store.recover(&session_id).unwrap();
    let lease = store.begin_attempt(&session_id, "child").unwrap();
    store
        .append_host_event(&lease, "child_owns_attempt", json!({}))
        .unwrap();
}

#[test]
fn independent_process_recovery_invalidates_the_original_worker() {
    let database = Database::new();
    let store = database.open();
    let id = store.create_session("process fence").unwrap().id;
    let lease = store.begin_attempt(&id, "parent").unwrap();
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_process_fences_a_worker", "--nocapture"])
        .env("CAYW_CORE_FENCE_DB", database.path())
        .env("CAYW_CORE_FENCE_SESSION", &id)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(matches!(
        store.validate_lease(&lease),
        Err(CoreError::StaleLease)
    ));
    assert!(matches!(
        store.append_host_event(&lease, "old_worker", json!({})),
        Err(CoreError::StaleLease)
    ));
    assert_eq!(
        store.snapshot(&id).unwrap().attempt.unwrap().worker_id,
        "child"
    );
    assert!(
        store.events(&id, 0, 100).unwrap().iter().any(
            |r| matches!(&r.event, CoreEvent::Host { kind, .. } if kind == "child_owns_attempt")
        )
    );
}

#[test]
fn injected_projection_failure_rolls_back_the_event_insert() {
    let database = Database::new();
    let store = database.open();
    let id = store.create_session("rollback").unwrap().id;
    let lease = store.begin_attempt(&id, "worker").unwrap();
    let before = store.snapshot(&id).unwrap();
    let db = rusqlite::Connection::open(database.path()).unwrap();
    db.execute_batch("CREATE TRIGGER fail_projection BEFORE UPDATE ON core_sessions BEGIN SELECT RAISE(ABORT,'injected projection failure'); END;").unwrap();
    assert!(matches!(
        store.append_host_event(&lease, "write", json!({})),
        Err(CoreError::Database(_))
    ));
    assert_eq!(store.snapshot(&id).unwrap(), before);
    assert!(store.events(&id, before.cursor, 100).unwrap().is_empty());
    db.execute_batch("DROP TRIGGER fail_projection").unwrap();
    let event = store.append_host_event(&lease, "write", json!({})).unwrap();
    assert_eq!(event.cursor, before.cursor + 1);
}

#[test]
fn event_log_cannot_be_rewritten_or_deleted() {
    let database = Database::new();
    let store = database.open();
    let id = store.create_session("immutable log").unwrap().id;
    let db = rusqlite::Connection::open(database.path()).unwrap();
    let journal: String = db
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .unwrap();
    assert_eq!(journal, "wal");
    assert!(
        db.execute(
            "UPDATE core_events SET record='{}' WHERE session_id=?1",
            [&id]
        )
        .is_err()
    );
    assert!(
        db.execute("DELETE FROM core_events WHERE session_id=?1", [&id])
            .is_err()
    );
    assert_eq!(store.events(&id, 0, 100).unwrap().len(), 1);
}
