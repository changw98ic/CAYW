//! Offline acceptance for WorkItems, idempotent dispatch, input states,
//! proactive rules and notification receipts.
//!
//! Every test uses a fresh temporary directory. No test reads a real user
//! directory, calls a model or touches the network.

use cayw_queue::{Queue, ResourcePolicy, TaskDispatch, TaskIntent, TaskPriority, TaskResources};
use cayw_runtime::{
    AcceptanceCheck, AcceptanceCriterion, InputState, NotificationDecision, NotificationStatus,
    ProactiveMode, ProactiveTriggerOutcome, WorkItemSpec, WorkItems, WorkLifecycle,
};
use std::{fs, path::PathBuf, sync::Arc};
use tempfile::TempDir;

struct Fixture {
    _temporary: TempDir,
    workspace: PathBuf,
    work_db: PathBuf,
    task_db: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let workspace = temporary.path().join("workspace");
        fs::create_dir_all(&workspace).expect("workspace");
        Self {
            work_db: temporary.path().join("main.db"),
            task_db: temporary.path().join("target.db"),
            workspace,
            _temporary: temporary,
        }
    }

    fn work_items(&self) -> WorkItems {
        WorkItems::open(&self.work_db).expect("work item store")
    }

    fn queue(&self) -> Queue {
        Queue::open_with_resources(&self.task_db, 4, 64, 8, &ResourcePolicy::default())
            .expect("queue")
    }
}

fn spec(criteria: Vec<AcceptanceCriterion>) -> WorkItemSpec {
    WorkItemSpec {
        work_id: Some("work-1".into()),
        owner_main_session_id: "main".into(),
        title: "finish the thing".into(),
        goal: "deliver the result".into(),
        acceptance_criteria: criteria,
        user_source_refs: vec!["main:1".into()],
        action_scope: vec!["workspace".into()],
        approval_refs: vec!["approval-1".into()],
        session_refs: vec![],
        task_refs: vec![],
        artifact_refs: vec![],
        waiting_conditions: vec![],
        pending_decisions: vec![],
        next_check_at: None,
    }
}

fn path_check(path: &str) -> AcceptanceCriterion {
    AcceptanceCriterion {
        criterion_id: "file".into(),
        description: "the result file exists".into(),
        check: Some(AcceptanceCheck::PathExists { path: path.into() }),
    }
}

fn user_condition() -> AcceptanceCriterion {
    AcceptanceCriterion {
        criterion_id: "taste".into(),
        description: "the user accepts the wording".into(),
        check: None,
    }
}

#[test]
fn lost_dispatch_response_is_recovered_by_key_without_a_second_task() {
    let fixture = Fixture::new();
    let queue = fixture.queue();
    let input = cayw_runtime::TaskInput {
        text: "do the dispatched work".into(),
        images: vec![],
    };
    let dispatch = TaskDispatch {
        work_id: "work-1".into(),
        dispatch_key: "dispatch-1".into(),
        pause_version: 0,
        stop_version: 0,
    };
    // The first response is "lost": the caller discards the returned record.
    let created = queue
        .enqueue_dispatched_with_resources(
            "target-session",
            &input,
            &TaskIntent::General,
            TaskPriority::Normal,
            &TaskResources::default(),
            64,
            Some(&dispatch),
        )
        .expect("first dispatch");
    let recovered = queue
        .find_by_dispatch_key("dispatch-1")
        .expect("lookup")
        .expect("the original task is found by key");
    assert_eq!(recovered.id, created.id);
    assert_eq!(recovered.work_id.as_deref(), Some("work-1"));
    assert_eq!(recovered.dispatch_key.as_deref(), Some("dispatch-1"));

    // A retried delivery of the same key and input reuses the original task
    // even while the session already has a live task.
    let again = queue
        .enqueue_dispatched_with_resources(
            "target-session",
            &input,
            &TaskIntent::General,
            TaskPriority::Normal,
            &TaskResources::default(),
            64,
            Some(&dispatch),
        )
        .expect("repeated delivery");
    assert_eq!(again.id, created.id);
    assert_eq!(queue.list("target-session").expect("tasks").len(), 1);

    // The same key with different input is a conflict, not a second task.
    let different = cayw_runtime::TaskInput {
        text: "different work".into(),
        images: vec![],
    };
    assert!(matches!(
        queue.enqueue_dispatched_with_resources(
            "target-session",
            &different,
            &TaskIntent::General,
            TaskPriority::Normal,
            &TaskResources::default(),
            64,
            Some(&dispatch),
        ),
        Err(cayw_queue::QueueError::DispatchConflict)
    ));
    assert_eq!(queue.list("target-session").expect("tasks").len(), 1);
}

#[test]
fn model_report_never_writes_a_passing_acceptance_record() {
    let fixture = Fixture::new();
    let work = fixture.work_items();
    let item = work
        .create(spec(vec![path_check("result.txt"), user_condition()]))
        .expect("WorkItem");
    assert_eq!(item.lifecycle, WorkLifecycle::Open);

    // A model saying it is done cannot create a passing record.
    assert!(
        work.record_model_report("work-1", "everything is complete")
            .is_err()
    );
    let unchanged = work.get("work-1").expect("get").expect("item");
    assert!(unchanged.acceptance_records.is_empty());
    assert_eq!(unchanged.lifecycle, WorkLifecycle::Open);

    fs::write(fixture.workspace.join("result.txt"), "done").expect("result");
    let observed = work
        .evaluate_acceptance("work-1", "file", &fixture.workspace)
        .expect("program check");
    assert_eq!(observed.acceptance_records.len(), 1);
    assert_eq!(
        observed.acceptance_records[0].source,
        cayw_runtime::AcceptanceSource::Program
    );
    // One criterion still has no record: the WorkItem stays open even though
    // the Task-level work could be finished.
    assert_eq!(observed.lifecycle, WorkLifecycle::Open);

    let failed = work
        .record_user_acceptance("work-1", "taste", false, "main:9")
        .expect("user answer");
    assert_eq!(failed.lifecycle, WorkLifecycle::Open);
    let completed = work
        .record_user_acceptance("work-1", "taste", true, "main:10")
        .expect("user answer");
    assert_eq!(completed.lifecycle, WorkLifecycle::Completed);
    assert!(completed.acceptance_complete());

    // A terminal acceptance state accepts no further model or program writes.
    assert!(work.record_model_report("work-1", "still done").is_err());
    assert!(
        work.evaluate_acceptance("work-1", "file", &fixture.workspace)
            .is_err()
    );
}

#[test]
fn duplicate_input_is_processed_once_and_included_is_not_handled() {
    let fixture = Fixture::new();
    let work = fixture.work_items();
    let first = work
        .ingest_input("plugin", "event-7", "main:3", "main")
        .expect("ingest");
    let duplicate = work
        .ingest_input("plugin", "event-7", "main:3", "main")
        .expect("deduplicated ingest");
    assert_eq!(duplicate.log_id, first.log_id);
    assert_eq!(duplicate.events.len(), 1);
    assert_eq!(duplicate.received_count(), 1);
    assert_eq!(duplicate.state, InputState::Received);
    // The log record itself is the input identity: another delivery route for
    // the same record does not create a second effective input.
    let other_route = work
        .ingest_input("webhook", "delivery-9", "main:3", "main")
        .expect("same log identity");
    assert_eq!(other_route.received_count(), 1);
    assert_eq!(other_route.events.len(), 1);

    let included = work
        .transition_input("main:3", InputState::Included, None, None)
        .expect("included");
    assert_eq!(included.state, InputState::Included);
    assert_eq!(included.received_count(), 1);
    assert!(
        !included
            .events
            .iter()
            .any(|event| event.state == InputState::Handled),
        "included must not imply handled"
    );

    assert!(
        work.transition_input("main:3", InputState::Handled, None, None)
            .is_err()
    );
    let handled = work
        .transition_input("main:3", InputState::Handled, None, Some("answer:main:4"))
        .expect("handled");
    assert_eq!(handled.state, InputState::Handled);

    // One source identity cannot silently move to another log record.
    assert!(
        work.ingest_input("plugin", "event-7", "main:5", "main")
            .is_err()
    );
}

#[test]
fn pause_stops_new_capability_calls_and_late_results_do_not_redispatch() {
    let fixture = Fixture::new();
    let work = fixture.work_items();
    let item = work
        .create(spec(vec![path_check("result.txt")]))
        .expect("WorkItem");
    let queue = fixture.queue();
    let input = cayw_runtime::TaskInput {
        text: "long running work".into(),
        images: vec![],
    };
    let dispatch = TaskDispatch {
        work_id: item.work_id.clone(),
        dispatch_key: "pause-dispatch".into(),
        pause_version: item.pause_version,
        stop_version: item.stop_version,
    };
    let task = queue
        .enqueue_dispatched_with_resources(
            "target",
            &input,
            &TaskIntent::General,
            TaskPriority::Normal,
            &TaskResources::default(),
            64,
            Some(&dispatch),
        )
        .expect("dispatch");
    let lease = queue.claim(&task.id).expect("claim");

    let paused = work.pause(&item.work_id).expect("pause");
    assert_eq!(paused.lifecycle, WorkLifecycle::Paused);
    assert_eq!(paused.pause_version, 1);
    // The pushed version reaches the already-dispatched Task.
    assert_eq!(
        queue
            .pause_work(&item.work_id, paused.pause_version)
            .expect("push pause"),
        1
    );
    let stored = queue.get(&task.id).expect("task");
    assert!(stored.dispatch_paused);
    assert_eq!(stored.pause_version, 1);
    // Any later capability call checks the version and is refused.
    assert!(queue.check(&lease).is_err());

    // A late result is recorded as a fact and never dispatches again.
    let late = work
        .record_late_result(&item.work_id, &task.id, "log:target:42", 0, 0)
        .expect("late result");
    assert!(!late.redispatch_allowed);
    let after = work.get(&item.work_id).expect("get").expect("item");
    assert_eq!(after.late_results.len(), 1);
    assert_eq!(after.lifecycle, WorkLifecycle::Paused);
    assert_eq!(queue.list("target").expect("tasks").len(), 1);
}

#[test]
fn stop_writes_the_version_and_cancels_live_tasks() {
    let fixture = Fixture::new();
    let work = fixture.work_items();
    let item = work
        .create(spec(vec![path_check("result.txt")]))
        .expect("WorkItem");
    let queue = fixture.queue();
    let input = cayw_runtime::TaskInput {
        text: "work to stop".into(),
        images: vec![],
    };
    let dispatch = TaskDispatch {
        work_id: item.work_id.clone(),
        dispatch_key: "stop-dispatch".into(),
        pause_version: 0,
        stop_version: 0,
    };
    let task = queue
        .enqueue_dispatched_with_resources(
            "target",
            &input,
            &TaskIntent::General,
            TaskPriority::Normal,
            &TaskResources::default(),
            64,
            Some(&dispatch),
        )
        .expect("dispatch");
    let lease = queue.claim(&task.id).expect("claim");
    let stopped = work.stop(&item.work_id).expect("stop");
    assert_eq!(stopped.lifecycle, WorkLifecycle::Cancelled);
    assert_eq!(stopped.stop_version, 1);
    assert_eq!(
        queue
            .stop_work(&item.work_id, stopped.stop_version)
            .expect("push stop"),
        1
    );
    let stored = queue.get(&task.id).expect("task");
    assert!(stored.cancel_requested);
    assert!(stored.dispatch_paused);
    assert_eq!(stored.stop_version, 1);
    assert!(queue.check(&lease).is_err());

    let late = work
        .record_late_result(&item.work_id, &task.id, "log:target:43", 0, 0)
        .expect("late result");
    assert!(!late.redispatch_allowed);
    assert_eq!(queue.list("target").expect("tasks").len(), 1);
}

#[test]
fn notifications_save_intent_before_receipt_and_never_blindly_resend() {
    let fixture = Fixture::new();
    let work = fixture.work_items();
    let item = work
        .create(spec(vec![path_check("result.txt")]))
        .expect("WorkItem");
    let targets = vec!["user-desktop".to_string()];
    let notification = work
        .prepare_notification(
            &item.work_id,
            item.work_version,
            "user-desktop",
            Some("result:1"),
            &[],
            &targets,
        )
        .expect("intent");
    assert_eq!(notification.status, NotificationStatus::Pending);
    assert_eq!(
        work.notification_decision(&notification.notification_id)
            .expect("decision"),
        NotificationDecision::Send
    );

    // A target outside the user configuration is refused.
    assert!(
        work.prepare_notification(
            &item.work_id,
            item.work_version,
            "model-invented",
            None,
            &[],
            &targets,
        )
        .is_err()
    );

    work.mark_notification_unknown(&notification.notification_id)
        .expect("unknown");
    assert_eq!(
        work.notification_decision(&notification.notification_id)
            .expect("decision"),
        NotificationDecision::QueryReceipt
    );
    // An unknown status cannot be written as a service receipt.
    assert!(
        work.record_notification_receipt(
            &notification.notification_id,
            NotificationStatus::Unknown,
            "receipt",
        )
        .is_err()
    );
    let delivered = work
        .record_notification_receipt(
            &notification.notification_id,
            NotificationStatus::Delivered,
            "receipt-1",
        )
        .expect("receipt");
    assert_eq!(delivered.status, NotificationStatus::Delivered);
    assert_eq!(
        work.notification_decision(&notification.notification_id)
            .expect("decision"),
        NotificationDecision::Complete
    );

    // Work completion and delivery are separate facts: completing the work
    // never changes the notification, and delivery never completes the work.
    fs::write(fixture.workspace.join("result.txt"), "done").expect("result");
    let completed = work
        .evaluate_acceptance(&item.work_id, "file", &fixture.workspace)
        .expect("program check");
    assert_eq!(completed.lifecycle, WorkLifecycle::Completed);
    let still_delivered = work
        .notification(&notification.notification_id)
        .expect("get")
        .expect("notification");
    assert_eq!(still_delivered.status, NotificationStatus::Delivered);
}

#[test]
fn proactive_modes_merge_by_rule_and_condition_without_a_usage_cap() {
    let fixture = Fixture::new();
    let work = Arc::new(fixture.work_items());
    assert_eq!(
        work.trigger_proactive(ProactiveMode::Off, "rule-a", 1, "work-1", 10)
            .expect("off"),
        ProactiveTriggerOutcome::Ignored
    );
    let suggested = work
        .trigger_proactive(ProactiveMode::Observe, "rule-a", 1, "work-1", 10)
        .expect("observe");
    assert!(matches!(
        suggested,
        ProactiveTriggerOutcome::Suggested { .. }
    ));
    assert!(matches!(
        work.trigger_proactive(ProactiveMode::Observe, "rule-a", 1, "work-1", 10)
            .expect("merged"),
        ProactiveTriggerOutcome::Duplicate { .. }
    ));

    // A huge usage number is recorded, never refused as a cap.
    let dispatched = work
        .trigger_proactive(ProactiveMode::Auto, "rule-b", 7, "work-1", 10_000_000_000)
        .expect("auto");
    let ProactiveTriggerOutcome::Dispatched { dispatch_key, .. } = dispatched else {
        panic!("auto mode must produce dispatch intent");
    };

    // Concurrent triggers of the same condition merge onto one record.
    let mut handles = Vec::new();
    for _ in 0..8 {
        let work = Arc::clone(&work);
        handles.push(std::thread::spawn(move || {
            work.trigger_proactive(ProactiveMode::Auto, "rule-c", 3, "work-1", 1)
                .expect("concurrent trigger")
        }));
    }
    let outcomes: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().expect("thread"))
        .collect();
    let dispatched_count = outcomes
        .iter()
        .filter(|outcome| matches!(outcome, ProactiveTriggerOutcome::Dispatched { .. }))
        .count();
    let duplicate_count = outcomes
        .iter()
        .filter(|outcome| matches!(outcome, ProactiveTriggerOutcome::Duplicate { .. }))
        .count();
    assert_eq!(dispatched_count, 1);
    assert_eq!(duplicate_count, 7);
    assert!(dispatch_key.ends_with("work-1"));
}
