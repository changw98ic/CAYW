use cayw_agent::{AgentEvent, ToolResult};
use cayw_ai::ToolCall;
use cayw_core::{
    AttemptLease, CapabilityCallKey, CapabilityCallRecord, CapabilityOutcome,
    CapabilityReconciliation, CapabilityStage, CoreEvent, EventRecord, ExecutionOutcome,
    ExecutionRecord, ExecutionState, ExecutionWaitRecord, PROTOCOL_VERSION, ReconciledOutcome,
    Reconciliation, ReconciliationOrigin, ToolReconciliationOrigin,
};
use cayw_run_inspector::*;
use serde_json::{Value, json};

fn lease() -> AttemptLease {
    AttemptLease {
        session_id: "session".into(),
        attempt_id: "attempt".into(),
        worker_id: "worker".into(),
        fence: 1,
    }
}

fn execution(state: ExecutionState, terminal: Option<ExecutionOutcome>) -> ExecutionRecord {
    ExecutionRecord {
        execution_id: "cell".into(),
        parent_call_id: "exec".into(),
        task_id: "task".into(),
        source_hash: "hash".into(),
        lease: lease(),
        state,
        terminal,
    }
}

fn record(cursor: u64, event: CoreEvent) -> EventRecord {
    EventRecord {
        protocol_version: PROTOCOL_VERSION,
        session_id: "session".into(),
        cursor,
        timestamp: cursor * 100,
        attempt: None,
        event,
    }
}

fn tool_end(cursor: u64, id: &str, result: ToolResult) -> EventRecord {
    record(
        cursor,
        CoreEvent::Agent {
            event: Box::new(AgentEvent::ToolExecutionEnd {
                call: ToolCall::new(
                    id,
                    "execute_code",
                    json!({"action":if id == "exec" {"exec"} else {"wait"}}),
                ),
                result,
                duration_ms: Some(50),
            }),
        },
    )
}

fn page() -> ToolResult {
    ToolResult {
        structured_content: Some(json!({"cell_id":"cell","status":"running","vm_alive":true})),
        ..ToolResult::text("running page")
    }
}

fn started() -> Vec<EventRecord> {
    vec![
        record(
            1,
            CoreEvent::ExecutionStarted {
                execution: execution(ExecutionState::Running, None),
            },
        ),
        tool_end(2, "exec", page()),
        record(
            3,
            CoreEvent::ExecutionWaitRegistered {
                binding: ExecutionWaitRecord {
                    call_id: "wait".into(),
                    execution_id: "cell".into(),
                    task_id: "task".into(),
                    lease: lease(),
                    arguments: json!({"action":"wait","cell_id":"cell","cursor":0}),
                },
            },
        ),
        tool_end(4, "wait", page()),
    ]
}

fn capability(name: &str, outcome: CapabilityOutcome) -> CapabilityCallRecord {
    CapabilityCallRecord {
        key: CapabilityCallKey {
            execution_id: "cell".into(),
            sequence: 0,
        },
        name: name.into(),
        arguments: json!({}),
        parent_call_id: "exec".into(),
        task_id: "task".into(),
        source_hash: "hash".into(),
        lease: lease(),
        stage: CapabilityStage::Settled,
        authorization: None,
        outcome: Some(outcome),
        external_provenance: None,
    }
}

fn outcome(step: &RunStep) -> Value {
    serde_json::to_value(step.outcome).unwrap()
}

#[test]
fn running_page_is_not_a_success() {
    let records = started();
    let view = inspect_runtime_events("session", None, None, &records);
    assert_eq!(view.snapshot.steps.len(), 2);
    assert!(view.snapshot.steps.iter().all(|s| outcome(s) == "running"));
}

#[test]
fn a_failed_wait_is_not_hidden_by_the_running_vm() {
    let mut records = started();
    *records.last_mut().unwrap() = tool_end(
        4,
        "wait",
        ToolResult::error("cursor exceeds published output"),
    );
    let view = inspect_runtime_events("session", None, None, &records);
    assert_eq!(outcome(&view.snapshot.steps[0]), "running");
    assert_eq!(outcome(&view.snapshot.steps[1]), "failed");
}

#[test]
fn late_unknown_updates_original_exec_and_wait_across_pages() {
    let mut records = started();
    records.push(record(
        5,
        CoreEvent::CapabilitySettled {
            call: capability(
                "test.run",
                CapabilityOutcome::OutcomeUnknown {
                    error: "timed out; effects unknown".into(),
                },
            ),
        },
    ));
    records.push(record(
        6,
        CoreEvent::ExecutionFinished {
            execution: execution(
                ExecutionState::Interrupted,
                Some(ExecutionOutcome::Interrupted {
                    reason: "OutcomeUnknown: timed out".into(),
                }),
            ),
        },
    ));
    let full = snapshot_from_runtime_events("session", None, None, &records);
    assert!(full.steps.iter().all(|s| outcome(s) == "outcome_unknown"));
    assert!(full.steps.iter().all(|s| s.evidence_cursor == Some(6)));
    let first = snapshot_from_runtime_events_window("session", None, None, &records, 0, 4);
    let last = snapshot_from_runtime_events_window("session", None, None, &records, 4, 6);
    assert_eq!(
        last.steps.len(),
        2,
        "terminal-only page must update prior rows"
    );
    assert_eq!(
        merge_session_snapshots("session", None, None, &[last.clone(), first]),
        full,
        "older page must not overwrite a newer outcome"
    );
    assert_eq!(
        timelines_and_snapshots_from_runtime_events_window(&records, 4, 6).1[0],
        last
    );
}

#[test]
fn completed_vm_does_not_hide_a_failed_process_result() {
    for name in ["test.run", "fs.read"] {
        let mut records = started();
        records.push(record(5, CoreEvent::CapabilitySettled { call: capability(name, CapabilityOutcome::Completed { result:json!({"success":false,"exit_code":1,"termination":"exited","output_complete":true}) }) }));
        records.push(record(
            6,
            CoreEvent::ExecutionFinished {
                execution: execution(
                    ExecutionState::Completed,
                    Some(ExecutionOutcome::Completed {
                        result: json!(true),
                    }),
                ),
            },
        ));
        let view = inspect_runtime_events("session", None, None, &records);
        let expected = if name == "test.run" {
            "failed"
        } else {
            "success"
        };
        assert!(
            view.snapshot.steps.iter().all(|s| outcome(s) == expected),
            "{name}: {view:?}"
        );
        if name == "test.run" {
            assert!(
                view.snapshot.steps[0]
                    .error_summary
                    .as_ref()
                    .unwrap()
                    .contains("test.run")
            );
        }
    }
}

#[test]
fn unknown_flag_precedes_error_flag_without_a_cell_binding() {
    let records = vec![tool_end(
        1,
        "exec",
        ToolResult {
            outcome_unknown: true,
            ..ToolResult::error("unknown effects")
        },
    )];
    let view = inspect_runtime_events("session", None, None, &records);
    assert_eq!(outcome(&view.snapshot.steps[0]), "outcome_unknown");
}

#[test]
fn cancellation_and_interruption_keep_their_distinct_statuses() {
    for (state, terminal, expected) in [
        (
            ExecutionState::Cancelled,
            ExecutionOutcome::Cancelled {
                reason: "cancelled by caller".into(),
            },
            "cancelled",
        ),
        (
            ExecutionState::Interrupted,
            ExecutionOutcome::Interrupted {
                reason: "worker stopped".into(),
            },
            "interrupted",
        ),
    ] {
        let mut records = started();
        records.push(record(
            5,
            CoreEvent::ExecutionFinished {
                execution: execution(state, Some(terminal)),
            },
        ));
        let view = inspect_runtime_events("session", None, None, &records);
        assert!(
            view.snapshot
                .steps
                .iter()
                .all(|step| outcome(step) == expected)
        );
        assert_eq!(view.timeline.failure_count, 0);
        assert!(view.timeline.retry_paths.is_empty());
    }
}

#[test]
fn tool_reconciliation_updates_the_step_without_rewriting_original_evidence() {
    let original = tool_end(
        1,
        "exec",
        ToolResult {
            outcome_unknown: true,
            ..ToolResult::error("response lost")
        },
    );
    let reconciliation = record(
        2,
        CoreEvent::Reconciled {
            reconciliation: Reconciliation {
                call_id: "exec".into(),
                expected: Box::new(ToolReconciliationOrigin {
                    lease: lease(),
                    task_id: "task".into(),
                    call: ToolCall::new("exec", "execute_code", json!({"action":"exec"})),
                    execution: None,
                }),
                actor: "verified-host".into(),
                evidence: "exact receipt".into(),
                outcome: ReconciledOutcome::Completed {
                    result: ToolResult::text("verified completed"),
                },
            },
        },
    );
    let records = vec![original, reconciliation];
    let first = snapshot_from_runtime_events_window("session", None, None, &records, 0, 1);
    let last = snapshot_from_runtime_events_window("session", None, None, &records, 1, 2);
    assert_eq!(outcome(&first.steps[0]), "outcome_unknown");
    assert_eq!(outcome(&last.steps[0]), "success");
    assert_eq!(last.steps[0].evidence_cursor, Some(2));
    assert!(
        last.steps[0]
            .error_summary
            .as_ref()
            .unwrap()
            .contains("reconciled")
    );
    let merged = merge_session_snapshots("session", None, None, &[first, last]);
    assert_eq!(merged.steps.len(), 1);
    assert_eq!(
        merged,
        snapshot_from_runtime_events("session", None, None, &records)
    );
}

#[test]
fn reconciled_child_does_not_turn_an_interrupted_vm_into_success() {
    let mut records = started();
    let mut call = capability(
        "test.run",
        CapabilityOutcome::OutcomeUnknown {
            error: "response lost".into(),
        },
    );
    records.push(record(
        5,
        CoreEvent::CapabilitySettled { call: call.clone() },
    ));
    records.push(record(
        6,
        CoreEvent::ExecutionFinished {
            execution: execution(
                ExecutionState::Interrupted,
                Some(ExecutionOutcome::Interrupted {
                    reason: "OutcomeUnknown: response lost".into(),
                }),
            ),
        },
    ));
    let known = CapabilityOutcome::Failed {
        error: "verified partial side effects; process stopped".into(),
    };
    call.outcome = Some(known.clone());
    records.push(record(
        7,
        CoreEvent::CapabilityReconciled {
            reconciliation: CapabilityReconciliation {
                key: call.key.clone(),
                expected: Box::new(ReconciliationOrigin::from(&call)),
                actor: "verified-host".into(),
                evidence: "exact receipt".into(),
                outcome: known,
                external_provenance: None,
            },
            call,
        },
    ));
    let full = snapshot_from_runtime_events("session", None, None, &records);
    assert!(full.steps.iter().all(|step| outcome(step) == "interrupted"));
    assert!(
        full.steps
            .iter()
            .all(|step| step.evidence_cursor == Some(7))
    );
    assert!(full.steps.iter().all(|step| {
        step.error_summary
            .as_ref()
            .unwrap()
            .contains("reconciled at cursor 7")
    }));
    let first = snapshot_from_runtime_events_window("session", None, None, &records, 0, 6);
    let last = snapshot_from_runtime_events_window("session", None, None, &records, 6, 7);
    assert_eq!(last.steps.len(), 2);
    assert_eq!(
        merge_session_snapshots("session", None, None, &[first, last]),
        full
    );
}
