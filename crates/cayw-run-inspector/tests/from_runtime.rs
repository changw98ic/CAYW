//! Steps must come from real Core `EventRecord` / `AgentEvent` shapes — not
//! hand-built `RunStep` fixtures alone.

use cayw_agent::{AgentEvent, ToolResult};
use cayw_ai::ToolCall;
use cayw_core::{CoreEvent, EventRecord, PROTOCOL_VERSION};
use cayw_run_inspector::*;
use serde_json::json;

fn record(session: &str, cursor: u64, timestamp: u64, event: CoreEvent) -> EventRecord {
    EventRecord {
        protocol_version: PROTOCOL_VERSION,
        session_id: session.into(),
        cursor,
        timestamp,
        attempt: None,
        event,
    }
}

fn agent(event: AgentEvent) -> CoreEvent {
    CoreEvent::Agent {
        event: Box::new(event),
    }
}

fn call(id: &str, name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall::new(id, name, args)
}

#[test]
fn runtime_event_records_build_distinguishable_retry_paths() {
    let session = "live-sess";
    let dig_a = json!({"cmd": "echo A"});
    let dig_b = json!({"path": "missing.txt"});
    let dig_c = json!({"path": "out.txt", "content": "ok"});

    let records = vec![
        record(session, 1, 1_000, agent(AgentEvent::TurnStart)),
        record(
            session,
            2,
            1_010,
            agent(AgentEvent::ToolExecutionStart {
                call: call("s1", "bash", dig_a.clone()),
            }),
        ),
        record(
            session,
            3,
            1_060,
            agent(AgentEvent::ToolExecutionEnd {
                call: call("s1", "bash", dig_a.clone()),
                result: ToolResult::error("exit 1"),
                duration_ms: Some(50),
            }),
        ),
        record(
            session,
            4,
            1_100,
            agent(AgentEvent::ToolExecutionStart {
                call: call("s2", "bash", dig_a.clone()),
            }),
        ),
        record(
            session,
            5,
            1_140,
            agent(AgentEvent::ToolExecutionEnd {
                call: call("s2", "bash", dig_a.clone()),
                result: ToolResult::error("exit 1 again"),
                duration_ms: Some(40),
            }),
        ),
        record(session, 6, 2_000, agent(AgentEvent::TurnStart)),
        record(
            session,
            7,
            2_010,
            agent(AgentEvent::ToolExecutionStart {
                call: call("s3", "read", dig_b.clone()),
            }),
        ),
        record(
            session,
            8,
            2_030,
            agent(AgentEvent::ToolExecutionEnd {
                call: call("s3", "read", dig_b.clone()),
                result: ToolResult::error("not found"),
                duration_ms: Some(20),
            }),
        ),
        record(
            session,
            9,
            2_050,
            agent(AgentEvent::ToolExecutionStart {
                call: call("s4", "write", dig_c.clone()),
            }),
        ),
        record(
            session,
            10,
            2_080,
            agent(AgentEvent::ToolExecutionEnd {
                call: call("s4", "write", dig_c.clone()),
                result: ToolResult::text("wrote"),
                duration_ms: Some(30),
            }),
        ),
    ];

    let view = inspect_runtime_events(
        session,
        Some("fixture".into()),
        Some("task-fix".into()),
        &records,
    );
    assert_eq!(view.protocol_version, RUN_INSPECTOR_PROTOCOL_VERSION);
    assert_eq!(view.events_total, 10);
    assert_eq!(view.after, 0);
    assert_eq!(view.events_scanned, 10);
    assert!(!view.has_more);
    assert_eq!(view.next_after, 10);
    assert_eq!(view.snapshot.steps.len(), 4);
    assert_eq!(view.snapshot.steps[0].params_digest, params_digest(&dig_a));
    assert_eq!(view.snapshot.steps[0].evidence_cursor, Some(3));
    assert_eq!(view.snapshot.steps[0].outcome, StepOutcome::Failed);
    assert_eq!(
        view.snapshot.steps[0].error_summary.as_deref(),
        Some("exit 1")
    );
    assert_eq!(view.snapshot.steps[3].outcome, StepOutcome::Success);
    assert_eq!(view.snapshot.steps[3].turn, 2);

    let paths = &view.timeline.retry_paths;
    let path_s1 = paths
        .iter()
        .find(|path| path.failure_step_id == "s1")
        .expect("s1 path");
    let path_s3 = paths
        .iter()
        .find(|path| path.failure_step_id == "s3")
        .expect("s3 path");
    assert_ne!(path_s1.path_id, path_s3.path_id);
    assert_eq!(path_s1.recovery, RecoveryKind::SameParamsRetry);
    assert_eq!(path_s3.recovery, RecoveryKind::ChangedTool);
    assert_eq!(path_s1.evidence_cursors, vec![3, 5]);
    assert_eq!(path_s3.evidence_cursors, vec![8, 10]);
}

#[test]
fn timelines_group_multiple_sessions_from_shared_event_page() {
    let a = "A";
    let b = "B";
    let args = json!({"k": 1});
    let records = vec![
        record(a, 1, 100, agent(AgentEvent::TurnStart)),
        record(
            a,
            2,
            110,
            agent(AgentEvent::ToolExecutionStart {
                call: call("a1", "read", args.clone()),
            }),
        ),
        record(
            a,
            3,
            120,
            agent(AgentEvent::ToolExecutionEnd {
                call: call("a1", "read", args.clone()),
                result: ToolResult::text("ok"),
                duration_ms: Some(10),
            }),
        ),
        record(b, 1, 200, agent(AgentEvent::TurnStart)),
        record(
            b,
            2,
            210,
            agent(AgentEvent::ToolExecutionStart {
                call: call("b1", "read", args.clone()),
            }),
        ),
        record(
            b,
            3,
            215,
            agent(AgentEvent::ToolExecutionEnd {
                call: call("b1", "read", args),
                result: ToolResult::text(""),
                duration_ms: Some(5),
            }),
        ),
    ];
    let timelines = timelines_from_runtime_events(&records);
    assert_eq!(timelines.len(), 2);
    assert_eq!(timelines[0].session_id, "A");
    assert_eq!(timelines[1].session_id, "B");
    assert_eq!(timelines[0].failure_count, 0);
    assert_eq!(timelines[1].step_count, 1);
    let snap_b = snapshot_from_runtime_events(b, None, None, &records);
    assert_eq!(snap_b.steps[0].outcome, StepOutcome::Empty);
}

#[test]
fn late_tool_past_first_page_is_folded_when_all_records_supplied() {
    let session = "paged-sess";
    let mut records = Vec::new();
    let mut cursor = 0u64;
    // Simulate >1000 Core events before a real tool pair.
    for _ in 0..=cayw_core::MAX_EVENT_PAGE {
        cursor += 1;
        records.push(record(
            session,
            cursor,
            cursor * 10,
            agent(AgentEvent::TurnStart),
        ));
    }
    cursor += 1;
    let start_cursor = cursor;
    records.push(record(
        session,
        cursor,
        cursor * 10,
        agent(AgentEvent::ToolExecutionStart {
            call: call("late", "bash", json!({"cmd": "echo late"})),
        }),
    ));
    cursor += 1;
    records.push(record(
        session,
        cursor,
        cursor * 10,
        agent(AgentEvent::ToolExecutionEnd {
            call: call("late", "bash", json!({"cmd": "echo late"})),
            result: ToolResult::text("late"),
            duration_ms: Some(1),
        }),
    ));
    assert!(records.len() > cayw_core::MAX_EVENT_PAGE);

    let truncated =
        inspect_runtime_events(session, None, None, &records[..cayw_core::MAX_EVENT_PAGE]);
    assert!(
        truncated
            .snapshot
            .steps
            .iter()
            .all(|step| step.step_id != "late"),
        "first-page-only slice must miss the late tool"
    );

    let full = inspect_runtime_events_page(
        session,
        None,
        None,
        &records,
        RunInspectorPage {
            events_total: cursor,
            after: 0,
            events_scanned: records.len() as u64,
            has_more: false,
            next_after: cursor,
        },
    );
    assert!(!full.has_more);
    assert_eq!(full.events_total, cursor);
    assert_eq!(full.events_scanned, records.len() as u64);
    let late = full
        .snapshot
        .steps
        .iter()
        .find(|step| step.step_id == "late")
        .expect("full record set must fold the late tool");
    assert_eq!(late.evidence_cursor, Some(cursor));
    assert!(late.evidence_cursor.unwrap() > start_cursor);
    assert_eq!(late.outcome, StepOutcome::Success);

    // Windowed page starting after the first 1000 events recovers the late tool.
    let page2_records: Vec<_> = records
        .iter()
        .filter(|record| record.cursor > cayw_core::MAX_EVENT_PAGE as u64)
        .cloned()
        .collect();
    let page2 = inspect_runtime_events_page(
        session,
        None,
        None,
        &page2_records,
        RunInspectorPage {
            events_total: cursor,
            after: cayw_core::MAX_EVENT_PAGE as u64,
            events_scanned: page2_records.len() as u64,
            has_more: false,
            next_after: cursor,
        },
    );
    assert_eq!(page2.after, cayw_core::MAX_EVENT_PAGE as u64);
    assert!(
        page2
            .snapshot
            .steps
            .iter()
            .any(|step| step.step_id == "late"),
        "page-2 window must include the late tool"
    );
}
