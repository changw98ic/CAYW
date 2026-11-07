//! Live Runtime event log → run-inspector (F06 wiring).

use cayw_ai::{AssistantEvent, AssistantStream, CancellationToken, Model, ModelRequest, Provider};
use cayw_run_inspector::{RUN_INSPECTOR_PROTOCOL_VERSION, StepOutcome};
use cayw_runtime::{Runtime, RuntimeConfig, ScopedPermission};
use futures::stream;
use serde_json::json;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

struct ScriptProvider {
    script: String,
    requests: Mutex<usize>,
}
impl ScriptProvider {
    fn new(script: &str) -> Self {
        Self {
            script: script.into(),
            requests: Mutex::new(0),
        }
    }
}
impl Provider for ScriptProvider {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        *self.requests.lock().unwrap() += 1;
        let answer = cayw_runtime::fixture::script_response(
            &request,
            &self.script,
            &format!("script-{}", request.messages.len()),
        );
        Box::pin(stream::iter(vec![AssistantEvent::Done { message: answer }]))
    }
}

fn runtime(temp: &tempfile::TempDir, provider: Arc<dyn Provider>) -> Runtime {
    Runtime::open(
        temp.path().join("session.db"),
        temp.path().join("workspace"),
        provider,
        Model::new("fixture", "run-inspector"),
        RuntimeConfig {
            max_run_time: Duration::from_secs(10),
            permissions: vec![
                ScopedPermission::exact(
                    "fs.write",
                    json!({"path":"result.txt","content":"runtime evidence"}),
                ),
                ScopedPermission::exact("fs.read", json!({"path":"result.txt"})),
            ],
            ..Default::default()
        },
    )
    .unwrap()
}

#[tokio::test]
async fn run_inspector_builds_steps_from_live_runtime_events() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("workspace")).unwrap();
    let provider = Arc::new(ScriptProvider::new(
        "cayw.call('fs.write', {path='result.txt', content='runtime evidence'})\nlocal r = cayw.call('fs.read', {path='result.txt'})\nreturn r",
    ));
    let rt = runtime(&temp, provider);
    let session = rt.create(Some("inspector live".into())).unwrap();
    let _ = rt
        .prompt(&session.id, "write and read", CancellationToken::new())
        .await
        .unwrap();

    let records = rt.events(&session.id, 0, 1000).unwrap();
    assert!(
        records.iter().any(|record| {
            matches!(
                &record.event,
                cayw_core::CoreEvent::Agent { event }
                    if matches!(event.as_ref(), cayw_agent::AgentEvent::ToolExecutionEnd { .. })
            )
        }),
        "expected live ToolExecutionEnd records, got {records:?}"
    );

    let view = rt.run_inspector(&session.id).unwrap();
    assert_eq!(view.protocol_version, RUN_INSPECTOR_PROTOCOL_VERSION);
    assert_eq!(view.snapshot.label.as_deref(), Some("inspector live"));
    assert!(
        !view.snapshot.steps.is_empty(),
        "translator must emit steps from live events"
    );
    assert!(
        view.snapshot
            .steps
            .iter()
            .all(|step| step.evidence_cursor.is_some()),
        "every live step should carry the End event cursor"
    );
    assert!(
        view.snapshot
            .steps
            .iter()
            .any(|step| step.outcome == StepOutcome::Success && step.tool == "execute_code"),
        "expected a successful execute_code step from the live run"
    );
    assert_eq!(view.timeline.step_count, view.snapshot.steps.len() as u64);
    assert_eq!(view.timeline.session_id, session.id);
}

#[tokio::test]
async fn run_inspector_rpc_matches_direct_entry() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("workspace")).unwrap();
    let provider = Arc::new(ScriptProvider::new(
        "cayw.call('fs.write', {path='result.txt', content='runtime evidence'})\nreturn true",
    ));
    let rt = runtime(&temp, provider);
    let session = rt.create(Some("rpc entry".into())).unwrap();
    let _ = rt
        .prompt(&session.id, "write", CancellationToken::new())
        .await
        .unwrap();

    let direct = rt.run_inspector(&session.id).unwrap();
    // Exercise the same folding path the RPC handler calls.
    let encoded = serde_json::to_value(&direct).unwrap();
    assert_eq!(encoded["protocol_version"], RUN_INSPECTOR_PROTOCOL_VERSION);
    assert!(!encoded["snapshot"]["steps"].as_array().unwrap().is_empty());
    assert_eq!(direct.timeline.failure_count, 0);
    assert!(direct.timeline.retry_paths.is_empty());
}

/// Append enough Host events that the durable cursor exceeds one Core page.
fn seed_long_session(rt: &Runtime, session_id: &str, host_events: usize) {
    let store = rt.store();
    let lease = store.begin_attempt(session_id, "inspector-page").unwrap();
    for i in 0..host_events {
        store
            .append_host_event(&lease, "inspector_filler", serde_json::json!({"i": i}))
            .unwrap();
    }
    store.finish_attempt(&lease).unwrap();
}

#[tokio::test]
async fn run_inspector_pages_beyond_first_1000_events() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("workspace")).unwrap();
    let provider = Arc::new(ScriptProvider::new("return true"));
    let rt = runtime(&temp, provider);
    let session = rt.create(Some("long session".into())).unwrap();

    // SessionCreated + AttemptStarted + N host + AttemptFinished > MAX_EVENT_PAGE.
    seed_long_session(&rt, &session.id, cayw_core::MAX_EVENT_PAGE);

    let view = rt.get(&session.id).unwrap();
    assert!(
        view.cursor > cayw_core::MAX_EVENT_PAGE as u64,
        "fixture must exceed one event page, got cursor {}",
        view.cursor
    );

    // Default entry pages the full log — must not silently stop at 1000.
    let full = rt.run_inspector(&session.id).unwrap();
    assert_eq!(full.protocol_version, RUN_INSPECTOR_PROTOCOL_VERSION);
    assert_eq!(full.events_total, view.cursor);
    assert_eq!(full.after, 0);
    assert_eq!(full.events_scanned, view.cursor);
    assert!(!full.has_more, "full scan must not claim truncation");
    assert_eq!(full.next_after, view.cursor);

    // Explicit first page still signals has_more and stops at the limit.
    let page1 = rt
        .run_inspector_page(&session.id, 0, Some(cayw_core::MAX_EVENT_PAGE))
        .unwrap();
    assert_eq!(page1.events_total, view.cursor);
    assert_eq!(page1.events_scanned, cayw_core::MAX_EVENT_PAGE as u64);
    assert!(page1.has_more);
    assert_eq!(page1.next_after, cayw_core::MAX_EVENT_PAGE as u64);

    // Page 2 continues from next_after through the remainder.
    let page2 = rt
        .run_inspector_page(
            &session.id,
            page1.next_after,
            Some(cayw_core::MAX_EVENT_PAGE),
        )
        .unwrap();
    assert_eq!(page2.after, page1.next_after);
    assert_eq!(
        page2.events_scanned,
        view.cursor - page1.next_after,
        "page 2 must cover the remainder"
    );
    assert!(!page2.has_more);
    assert_eq!(page2.next_after, view.cursor);
    assert_eq!(
        page1.events_scanned + page2.events_scanned,
        view.cursor,
        "page 1 + page 2 must equal the durable total"
    );
}
