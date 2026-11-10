//! Pre-effect capability plan refusals are ordinary known errors inside
//! `execute_code`. They must not freeze the session, and a gap left in Core
//! must not poison later calls in the same cell.

use async_trait::async_trait;
use cayw_ai::{
    AssistantEvent, AssistantMessage, AssistantStream, CancellationToken, Content, Message, Model,
    ModelRequest, Provider, StopReason, Usage, UsageStatus,
};
use cayw_codemode::CapabilityError;
use cayw_core::{CapabilityOutcome, CoreEvent, SessionState};
use cayw_host::ExternalHost;
use cayw_runtime::{Runtime, RuntimeConfig, ScopedPermission, TaskState};
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

struct OkHost {
    calls: AtomicUsize,
}

#[async_trait]
impl ExternalHost for OkHost {
    async fn call(
        &self,
        name: &str,
        arguments: Value,
        _: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        assert_eq!(name, "search.query");
        assert_eq!(arguments, json!({"query":"ok"}));
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(json!({"ok":true}))
    }
}

struct ScriptProvider {
    script: &'static str,
    calls: AtomicUsize,
}

impl Provider for ScriptProvider {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let message = if last_user(&request).as_deref() == Some("follow up") {
            final_message(&request)
        } else {
            cayw_runtime::fixture::script_response(
                &request,
                self.script,
                &format!("plan-known-{}", self.calls.fetch_add(1, Ordering::SeqCst)),
            )
        };
        Box::pin(futures::stream::iter([AssistantEvent::Done { message }]))
    }
}

fn last_user(request: &ModelRequest) -> Option<String> {
    request
        .messages
        .iter()
        .rev()
        .find_map(|message| match message {
            Message::User { content, .. } => Some(cayw_ai::content_text(content)),
            _ => None,
        })
}

fn final_message(request: &ModelRequest) -> AssistantMessage {
    let mut message = AssistantMessage::new(&request.model);
    message.usage = Usage {
        input: 4,
        output: 4,
        total_tokens: 8,
        ..Default::default()
    };
    message.usage_status = UsageStatus::Reported;
    message.content.push(Content::text("follow up complete"));
    message.stop_reason = StopReason::Stop;
    message
}

fn open(root: &Path, script: &'static str, host: Arc<OkHost>) -> Runtime {
    std::fs::create_dir(root.join("workspace")).unwrap();
    let mut config = RuntimeConfig {
        permissions: vec![ScopedPermission::exact(
            "search.query",
            json!({"query":"ok"}),
        )],
        ..Default::default()
    };
    config.host.external = Some(host);
    Runtime::open(
        root.join("sessions.sqlite"),
        root.join("workspace"),
        Arc::new(ScriptProvider {
            script,
            calls: AtomicUsize::new(0),
        }),
        Model::new("fixture", "plan-known"),
        config,
    )
    .unwrap()
}

fn tool_result(rt: &Runtime, session: &str) -> cayw_ai::ToolResultMessage {
    rt.get(session)
        .unwrap()
        .messages
        .into_iter()
        .find_map(|message| match message {
            Message::ToolResult(result) => Some(result),
            _ => None,
        })
        .expect("execute_code has one paired tool result")
}

fn planned_names(rt: &Runtime, session: &str) -> Vec<String> {
    rt.events(session, 0, 1000)
        .unwrap()
        .into_iter()
        .filter_map(|event| match event.event {
            CoreEvent::CapabilityPlanned { call } => Some(call.name),
            _ => None,
        })
        .collect()
}

/// Reach the Core execution count limit without 4096 model turns. The limit is
/// checked before any execution is recorded, so the next cell creation is a
/// known refusal that never reaches Core.
fn fill_execution_limit(root: &Path, session: &str) {
    let db = rusqlite::Connection::open(root.join("sessions.sqlite")).unwrap();
    let projection: String = db
        .query_row(
            "SELECT projection FROM core_sessions WHERE id=?1",
            [session],
            |row| row.get(0),
        )
        .unwrap();
    let mut stored: Value = serde_json::from_str(&projection).unwrap();
    let executions = stored["executions"].as_array_mut().unwrap();
    for n in 0..4096 {
        executions.push(json!({
            "execution_id": format!("prior-{n}"),
            "parent_call_id": format!("prior-call-{n}"),
            "task_id": "prior-task",
            "source_hash": "sha256:prior",
            "lease": {
                "session_id": session,
                "attempt_id": "prior-attempt",
                "worker_id": "prior-worker",
                "fence": 1,
            },
            "state": "completed",
            "terminal": null,
        }));
    }
    db.execute(
        "UPDATE core_sessions SET projection=?1 WHERE id=?2",
        rusqlite::params![stored.to_string(), session],
    )
    .unwrap();
}

async fn follow_up(rt: &Runtime, session: &str) {
    let view = rt
        .prompt(session, "follow up", CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(view.state, SessionState::Idle);
}

#[tokio::test]
async fn invalid_unknown_and_oversized_arguments_are_known_refusals() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let host = Arc::new(OkHost {
        calls: AtomicUsize::new(0),
    });
    let script = r"
local results = {}
local ok = pcall(function() return cayw.call('missing.capability', {}) end)
results[#results+1] = ok
local wide = {}
for i = 1, 5000 do wide[i] = i end
ok = pcall(function() return cayw.call('fs.read', wide) end)
results[#results+1] = ok
local big = string.rep('x', 65520)
ok = pcall(function() return cayw.call('fs.read', {path=big}) end)
results[#results+1] = ok
return results
";
    let rt = open(&root, script, host.clone());
    let session = rt.create(None).unwrap().id;
    let view = rt
        .prompt(&session, "known refusals", CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(view.state, SessionState::Idle, "{view:#?}");
    assert_eq!(host.calls.load(Ordering::SeqCst), 0);
    let result = tool_result(&rt, &session);
    assert!(!result.is_error, "{result:?}");
    assert!(!result.outcome_unknown, "{result:?}");
    assert!(
        planned_names(&rt, &session).is_empty(),
        "all three calls must be refused before Core planning"
    );
    assert!(view.unresolved_capabilities.is_empty());
    follow_up(&rt, &session).await;
}

#[tokio::test]
async fn known_plan_refusal_does_not_block_later_calls_in_the_same_cell() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let host = Arc::new(OkHost {
        calls: AtomicUsize::new(0),
    });
    let script = r"
local wide = {}
for i = 1, 5000 do wide[i] = i end
pcall(function() return cayw.call('fs.read', wide) end)
return cayw.call('search.query', {query='ok'})
";
    let rt = open(&root, script, host.clone());
    let session = rt.create(None).unwrap().id;
    let view = rt
        .prompt(&session, "known then known", CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(view.state, SessionState::Idle, "{view:#?}");
    assert_eq!(host.calls.load(Ordering::SeqCst), 1);
    let result = tool_result(&rt, &session);
    assert!(!result.is_error, "{result:?}");
    assert!(!result.outcome_unknown, "{result:?}");
    let planned = planned_names(&rt, &session);
    assert_eq!(planned, vec!["search.query".to_string()]);
    let settled: Vec<_> = rt
        .events(&session, 0, 1000)
        .unwrap()
        .into_iter()
        .filter_map(|event| match event.event {
            CoreEvent::CapabilitySettled { call } => Some(call),
            _ => None,
        })
        .collect();
    assert_eq!(settled.len(), 1, "only the later call reaches Core");
    assert!(matches!(
        settled[0].outcome,
        Some(CapabilityOutcome::Completed { .. })
    ));
    assert!(view.unresolved_capabilities.is_empty());
    follow_up(&rt, &session).await;
}

#[tokio::test]
async fn many_prior_executions_no_longer_block_a_new_cell() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let host = Arc::new(OkHost {
        calls: AtomicUsize::new(0),
    });
    let rt = open(
        &root,
        "return cayw.call('search.query', {query='ok'})",
        host.clone(),
    );
    let session = rt.create(None).unwrap().id;
    fill_execution_limit(&root, &session);
    let view = rt
        .prompt(&session, "execution limit", CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(view.state, SessionState::Idle, "{view:#?}");
    assert_eq!(
        host.calls.load(Ordering::SeqCst),
        1,
        "the removed execution ceiling must not refuse an admitted call"
    );
    let result = tool_result(&rt, &session);
    assert!(!result.is_error, "{result:?}");
    assert!(!result.outcome_unknown, "{result:?}");
    assert!(view.unresolved_capabilities.is_empty());
    assert_eq!(
        planned_names(&rt, &session),
        vec!["search.query".to_string()],
        "the admitted execution is planned exactly once"
    );
    follow_up(&rt, &session).await;
}

#[tokio::test]
async fn real_plan_journal_failure_stays_unknown() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let host = Arc::new(OkHost {
        calls: AtomicUsize::new(0),
    });
    let rt = open(
        &root,
        "return cayw.call('search.query', {query='ok'})",
        host.clone(),
    );
    let db = rusqlite::Connection::open(root.join("sessions.sqlite")).unwrap();
    db.execute_batch(
        "CREATE TRIGGER fail_plan BEFORE INSERT ON core_events
         WHEN NEW.record LIKE '%capability_planned%'
         BEGIN SELECT RAISE(ABORT,'injected plan write failure'); END;",
    )
    .unwrap();
    let session = rt.create(None).unwrap().id;
    let result = rt
        .prompt(&session, "plan failure", CancellationToken::new())
        .await;
    assert!(result.is_err(), "{result:?}");
    assert!(
        result.unwrap_err().outcome_unknown(),
        "a doubtful durable plan write must retain unknown state"
    );
    assert_eq!(host.calls.load(Ordering::SeqCst), 0);
    let tasks = rt.tasks(&session).unwrap();
    assert_eq!(tasks.len(), 1);
    assert!(
        matches!(
            tasks[0].state,
            TaskState::OutcomeUnknown | TaskState::Draining
        ),
        "{:?}",
        tasks[0].state
    );
    assert!(
        planned_names(&rt, &session).is_empty(),
        "the failed durable write never commits a planned event"
    );
}
