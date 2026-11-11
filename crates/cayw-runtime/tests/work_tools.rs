//! Model-facing WorkItem entry points. All work uses a temporary directory, the
//! offline scripted provider and an in-process fake of the unique Client router.
//! No test reads a user directory, calls a real model or uses the network.

use async_trait::async_trait;
use cayw_ai::{AssistantEvent, AssistantStream, CancellationToken, Model, ModelRequest, Provider};
use cayw_runtime::{
    AcceptanceCheck, FileRules, Runtime, RuntimeConfig, TaskPriority, WorkControl,
    WorkDispatchReply, WorkDispatchRequest, WorkRouter, WorkRouterError, WorkTargetOutcome,
    WorkTaskQuery, fixture::script_response,
};
use serde_json::{Value, json};
use std::{path::Path, sync::Arc};
use tempfile::TempDir;

struct Script(String);
impl Provider for Script {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        Box::pin(futures::stream::iter([AssistantEvent::Done {
            message: script_response(
                &request,
                &self.0,
                &format!("work-{}", request.messages.len()),
            ),
        }]))
    }
}

fn open(root: &Path, provider: Arc<dyn Provider>, model: Model, config: RuntimeConfig) -> Runtime {
    std::fs::create_dir_all(root.join("workspace")).unwrap();
    Runtime::open(
        root.join("sessions.sqlite"),
        root.join("workspace"),
        provider,
        model,
        config,
    )
    .unwrap()
}

fn main_config(root: &Path, targets: Vec<String>) -> RuntimeConfig {
    RuntimeConfig {
        main_session: true,
        // A missing user rule file is the real empty loaded version. The main
        // session's built-in local work category is then allowed without adding
        // a fake wildcard rule.
        approval_rules: Some(Arc::new(FileRules::new(root.join("approval-rules.json")))),
        notification_targets: targets,
        ..Default::default()
    }
}

/// The same routing contract the local Client implements, but with one target
/// Runtime so the assertions can inspect the exact Task rows.
struct TargetRouter {
    target: Runtime,
    workspace: String,
}

#[async_trait]
impl WorkRouter for TargetRouter {
    async fn workspaces(&self) -> Result<Vec<Value>, WorkRouterError> {
        Ok(vec![json!({
            "id":self.workspace,
            "path":self.target.workspace().to_string_lossy(),
            "state":"open"
        })])
    }

    async fn sessions(&self, workspace: &str) -> Result<Vec<Value>, WorkRouterError> {
        assert_eq!(workspace, self.workspace);
        self.target
            .list()
            .map_err(|error| WorkRouterError::known(error.to_string()))?
            .into_iter()
            .map(|session| {
                serde_json::to_value(session)
                    .map_err(|error| WorkRouterError::known(error.to_string()))
            })
            .collect()
    }

    async fn create_session(&self, workspace: &str, label: &str) -> Result<Value, WorkRouterError> {
        assert_eq!(workspace, self.workspace);
        let session = self
            .target
            .create(Some(label.to_string()))
            .map_err(|error| WorkRouterError::known(error.to_string()))?;
        serde_json::to_value(session).map_err(|error| WorkRouterError::known(error.to_string()))
    }

    async fn dispatch(
        &self,
        request: WorkDispatchRequest,
    ) -> Result<WorkDispatchReply, WorkRouterError> {
        let session_id = request
            .session_id
            .clone()
            .ok_or_else(|| WorkRouterError::known("missing session"))?;
        self.target
            .get(&session_id)
            .map_err(|error| WorkRouterError::known(error.to_string()))?;
        let reused = self
            .target
            .task_by_dispatch_key(&request.dispatch_key)
            .map_err(|error| WorkRouterError::known(error.to_string()))?
            .is_some();
        let task = self
            .target
            .enqueue_dispatched(
                &session_id,
                cayw_runtime::TaskInput {
                    text: request.text,
                    images: vec![],
                },
                cayw_runtime::TaskIntent::General,
                TaskPriority::Normal,
                cayw_runtime::TaskResources {
                    declared_memory_bytes: request.declared_memory_bytes,
                    ..Default::default()
                },
                cayw_runtime::TaskDispatch {
                    work_id: request.work_id.clone(),
                    dispatch_key: request.dispatch_key.clone(),
                    pause_version: request.pause_version,
                    stop_version: request.stop_version,
                },
            )
            .map_err(|error| WorkRouterError::known(error.to_string()))?;
        Ok(WorkDispatchReply {
            workspace: self.workspace.clone(),
            session_id,
            task_id: task.id,
            state: task.state,
            work_id: request.work_id,
            dispatch_key: request.dispatch_key,
            reused,
        })
    }

    async fn task(
        &self,
        query: WorkTaskQuery,
    ) -> Result<Option<cayw_runtime::TaskRecord>, WorkRouterError> {
        if let Some(task_id) = query.task_id {
            return match self.target.task(&task_id) {
                Ok(task) => Ok(Some(task)),
                Err(cayw_runtime::RuntimeError::InvalidInput(message))
                    if message == "task not found" =>
                {
                    Ok(None)
                }
                Err(error) => Err(WorkRouterError::known(error.to_string())),
            };
        }
        let key = query
            .dispatch_key
            .ok_or_else(|| WorkRouterError::known("missing query"))?;
        self.target
            .task_by_dispatch_key(&key)
            .map_err(|error| WorkRouterError::known(error.to_string()))
    }

    async fn control(
        &self,
        action: WorkControl,
        work_id: &str,
        version: u64,
        workspaces: &[String],
    ) -> Result<Vec<WorkTargetOutcome>, WorkRouterError> {
        assert_eq!(workspaces, std::slice::from_ref(&self.workspace));
        let updated = match action {
            WorkControl::Pause => self.target.apply_work_pause(work_id, version),
            WorkControl::Stop => self.target.apply_work_stop(work_id, version),
        }
        .map_err(|error| WorkRouterError::known(error.to_string()))?;
        Ok(vec![WorkTargetOutcome {
            workspace: self.workspace.clone(),
            updated: Some(updated),
            error: None,
        }])
    }

    async fn evaluate_check(
        &self,
        workspace: &str,
        check: &AcceptanceCheck,
    ) -> Result<bool, WorkRouterError> {
        assert_eq!(workspace, self.workspace);
        check
            .evaluate(self.target.workspace())
            .map_err(|error| WorkRouterError::known(error.to_string()))
    }
}

fn work_script() -> &'static str {
    r#"
local work = cayw.call('work.create', {
  title='deliver the file',
  goal='deliver out.txt in the target workspace',
  acceptance_criteria={
    {criterion_id='file', description='out.txt exists', check={kind='path_exists', path='out.txt'}}
  }
})
local dispatch = cayw.call('work.dispatch', {
  work_id=work.work_id, workspace='target', session_id='TARGET_SESSION',
  text='write the result', dispatch_key='dispatch-1'
})
local dispatch_again = cayw.call('work.dispatch', {
  work_id=work.work_id, workspace='target', session_id='TARGET_SESSION',
  text='write the result', dispatch_key='dispatch-1'
})
local first = cayw.call('work.tasks', {work_id=work.work_id})
local pause = cayw.call('work.pause', {work_id=work.work_id})
local paused = cayw.call('work.tasks', {work_id=work.work_id})
local accept = cayw.call('work.accept.program', {work_id=work.work_id, criterion_id='file'})
local report_ok = pcall(function()
  cayw.call('work.report', {work_id=work.work_id, text='the model says done'})
end)
local bad_target = pcall(function()
  cayw.call('work.notify.intent', {work_id=work.work_id, target='not-configured'})
end)
local intent = cayw.call('work.notify.intent', {work_id=work.work_id, target='desk'})
local notification_id = intent.notification.notification_id
local next = cayw.call('work.notify.next', {notification_id=notification_id})
local receipt = cayw.call('work.notify.receipt', {
  notification_id=notification_id, status='delivered', receipt_ref='receipt-1'
})
local after_receipt = cayw.call('work.notify.next', {notification_id=notification_id})
local listed = cayw.call('work.notify.list', {work_id=work.work_id})
local stop = cayw.call('work.stop', {work_id=work.work_id})
local after_stop = cayw.call('work.tasks', {work_id=work.work_id})
local late = cayw.call('work.late_result', {
  work_id=work.work_id, task_id=dispatch.dispatch.task_id, result_ref='late-1',
  pause_version_at_dispatch=0, stop_version_at_dispatch=0
})
local fetched = cayw.call('work.get', {work_id=work.work_id})
return {
  work=work, dispatch=dispatch, dispatch_again=dispatch_again, first=first,
  pause=pause, paused=paused, accept=accept, report_ok=report_ok,
  bad_target=bad_target, intent=intent, next=next, receipt=receipt,
  after_receipt=after_receipt, listed=listed, stop=stop,
  after_stop=after_stop, late=late, fetched=fetched,
}
"#
}

#[tokio::test]
async fn main_session_manages_workitems_and_dispatches_through_the_client_router() {
    let temporary = TempDir::new().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let target_root = root.join("target");
    let target = open(
        &target_root,
        Arc::new(cayw_runtime::fixture::OfflineProvider::default()),
        Model::new("fixture", "target"),
        RuntimeConfig::default(),
    );
    let target_session = target.create(Some("Worker".into())).unwrap().id;
    std::fs::write(target.workspace().join("out.txt"), "delivered\n").unwrap();

    let script = work_script().replace("TARGET_SESSION", &target_session);
    let main = open(
        &root.join("main"),
        Arc::new(Script(script)),
        Model::new("fixture", "main"),
        main_config(&root, vec!["desk".into()]),
    );
    main.set_work_router(Arc::new(TargetRouter {
        target: target.clone(),
        workspace: "target".into(),
    }))
    .unwrap();
    let session = main.create(None).unwrap().id;
    main.prompt(&session, "manage work", CancellationToken::new())
        .await
        .unwrap();

    let work_id = main.list_work_items().unwrap()[0].work_id.clone();
    // The target Task is still exactly one, despite the repeated dispatch key.
    let tasks = target.tasks(&target_session).unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].work_id.as_deref(), Some(work_id.as_str()));
    assert_eq!(tasks[0].dispatch_key.as_deref(), Some("dispatch-1"));
    assert!(tasks[0].dispatch_paused);
    assert!(tasks[0].cancel_requested);
    assert_eq!(tasks[0].state, cayw_runtime::TaskState::Cancelled);

    let item = main.work_item(&work_id).unwrap().unwrap();
    assert_eq!(item.task_refs.len(), 1);
    assert_eq!(item.task_refs[0].task_id, tasks[0].id);
    assert_eq!(item.pause_version, 1);
    assert_eq!(item.stop_version, 1);
    assert_eq!(item.lifecycle, cayw_runtime::WorkLifecycle::Cancelled);
    assert_eq!(item.late_results.len(), 1);
    assert!(!item.late_results[0].redispatch_allowed);
    let acceptance = item
        .acceptance_records
        .iter()
        .find(|record| record.criterion_id == "file")
        .expect("program acceptance was observed");
    assert_eq!(item.acceptance_records.len(), 1);
    assert_eq!(acceptance.source, cayw_runtime::AcceptanceSource::Program);
    assert_eq!(acceptance.result, cayw_runtime::AcceptanceResult::Passed);

    let events = main
        .events(&session, 0, 1000)
        .unwrap()
        .into_iter()
        .map(|record| serde_json::to_value(record).unwrap())
        .collect::<Vec<_>>();
    let settled = |name: &str, argument: &str| {
        events.iter().any(|record| {
            record["event"]["type"] == "capability_settled"
                && record["event"]["call"]["name"] == name
                && record["event"]["call"]["arguments"]
                    .to_string()
                    .contains(argument)
        })
    };
    let failed = |name: &str, argument: &str| {
        events.iter().any(|record| {
            record["event"]["type"] == "capability_settled"
                && record["event"]["call"]["name"] == name
                && record["event"]["call"]["arguments"]
                    .to_string()
                    .contains(argument)
                && record["event"]["call"]["outcome"]["type"] == "failed"
        })
    };
    assert!(failed("work.report", "the model says done"));
    assert!(failed("work.notify.intent", "not-configured"));
    assert!(settled("work.notify.receipt", "receipt-1"));
    assert!(events.iter().any(|record| {
        record["event"]["type"] == "capability_settled"
            && record["event"]["call"]["name"] == "work.notify.next"
            && record["event"]["call"]["outcome"]["result"]["decision"] == "complete"
    }));
    assert_eq!(
        events
            .iter()
            .filter(|record| {
                record["event"]["type"] == "capability_settled"
                    && record["event"]["call"]["name"] == "work.tasks"
            })
            .count(),
        3
    );
}

#[tokio::test]
async fn a_main_session_can_create_a_target_session_and_dispatch_to_it() {
    let temporary = TempDir::new().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let target = open(
        &root.join("target"),
        Arc::new(cayw_runtime::fixture::OfflineProvider::default()),
        Model::new("fixture", "target"),
        RuntimeConfig::default(),
    );
    assert!(target.list().unwrap().is_empty());
    let script = r#"
local created = cayw.call('work.session.create', {workspace='target', label='Worker'})
local sessions = cayw.call('work.sessions', {workspace='target'})
local work = cayw.call('work.create', {
  title='delegate',
  goal='use the created target session',
  acceptance_criteria={{criterion_id='taste', description='the user accepts the result'}}
})
local dispatch = cayw.call('work.dispatch', {
  work_id=work.work_id, workspace='target', session_id=created.id,
  text='do the work', dispatch_key='created-session-dispatch'
})
return {created=created, sessions=sessions, work=work, dispatch=dispatch}
"#;
    let main = open(
        &root.join("main"),
        Arc::new(Script(script.into())),
        Model::new("fixture", "main"),
        main_config(&root, vec!["desk".into()]),
    );
    main.set_work_router(Arc::new(TargetRouter {
        target: target.clone(),
        workspace: "target".into(),
    }))
    .unwrap();
    let session = main.create(None).unwrap().id;
    main.prompt(&session, "delegate", CancellationToken::new())
        .await
        .unwrap();
    let sessions = target.list().unwrap();
    assert_eq!(sessions.len(), 1);
    let tasks = target.tasks(&sessions[0].id).unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(
        tasks[0].dispatch_key.as_deref(),
        Some("created-session-dispatch")
    );
    let item = main.list_work_items().unwrap().pop().unwrap();
    assert_eq!(item.task_refs.len(), 1);
    assert_eq!(
        item.task_refs[0].session_id.as_deref(),
        Some(sessions[0].id.as_str())
    );
}

#[tokio::test]
async fn work_capabilities_are_absent_outside_the_main_session() {
    let temporary = TempDir::new().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let runtime = open(
        &root,
        Arc::new(cayw_runtime::fixture::OfflineProvider::default()),
        Model::new("fixture", "plain"),
        RuntimeConfig::default(),
    );
    let names = runtime
        .capabilities()
        .unwrap()
        .into_iter()
        .map(|definition| definition.name)
        .collect::<Vec<_>>();
    assert!(names.iter().all(|name| !name.starts_with("work.")));
    assert!(names.iter().all(|name| !name.starts_with("memory.")));
    assert!(names.iter().all(|name| name != "notify.send"));
    let error = runtime.set_work_router(Arc::new(TargetRouter {
        target: runtime.clone(),
        workspace: "local".into(),
    }));
    assert!(error.is_err());
}

#[tokio::test]
async fn main_session_exposes_memory_capabilities_in_codemode() {
    let temporary = TempDir::new().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let script = r#"
local status_before = cayw.call('memory.status', {})
local queried = cayw.call('memory.query', {start_cursor=1})
local user_log_id = queried.events[1].log_id
local user_cursor = queried.events[1].cursor
local applied = cayw.call('memory.apply', {
  candidates={
    {
      kind='preference',
      body='Prefer concise Rust helpers',
      source_type='stated',
      source_log_ids={user_log_id}
    }
  }
})
local entries = cayw.call('memory.entries', {})
assert(#entries == 1, 'expected 1 visible memory entry before forget')
assert(entries[1].body == 'Prefer concise Rust helpers', 'expected entry body in memory.entries')
local dreamed = cayw.call('memory.dream', {})
local forgotten = cayw.call('memory.forget', {
  start_cursor=user_cursor, end_cursor=user_cursor, reason='user requested removal'
})
local after_forget = cayw.call('memory.query', {start_cursor=1})
assert(#after_forget.entries == 0, 'forgotten source must be omitted from memory.query')
return {
  status_before=status_before,
  applied=applied,
  entries=entries,
  queried=queried,
  dreamed=dreamed,
  forgotten=forgotten,
  after_forget=after_forget
}
"#;
    let main = open(
        &root.join("main"),
        Arc::new(Script(script.into())),
        Model::new("fixture", "main"),
        main_config(&root, vec![]),
    );
    let session = main.create(None).unwrap().id;
    main.prompt(
        &session,
        "Prefer concise Rust helpers",
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(main.memory_entries().unwrap().is_empty());
    let status = main.memory_status().unwrap();
    assert_eq!(status.entries, 1);
    assert_eq!(status.forgotten_ranges.len(), 1);
}

#[tokio::test]
async fn rpc_work_pause_stop_proactive_trigger_and_notify_send_route_through_target_workspace() {
    use cayw_ipc::RpcHandler;
    let temporary = TempDir::new().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let target = open(
        &root.join("target"),
        Arc::new(cayw_runtime::fixture::OfflineProvider::default()),
        Model::new("fixture", "target"),
        RuntimeConfig {
            permissions: vec![
                cayw_runtime::ScopedPermission::exact(
                    "fs.write",
                    json!({"path":"cayw-demo.txt","content":"headless runtime demo"}),
                ),
                cayw_runtime::ScopedPermission::exact("fs.read", json!({"path":"cayw-demo.txt"})),
            ],
            ..Default::default()
        },
    );
    let target_session = target.create(Some("Worker".into())).unwrap().id;
    let main = open(
        &root.join("main"),
        Arc::new(cayw_runtime::fixture::OfflineProvider::default()),
        Model::new("fixture", "main"),
        main_config(&root, vec!["desk".into()]),
    );
    main.set_work_router(Arc::new(TargetRouter {
        target: target.clone(),
        workspace: "target".into(),
    }))
    .unwrap();
    let main_session = main.create(None).unwrap().id;
    let created = main
        .call(
            "work.create",
            json!({
                "spec": {
                    "owner_main_session_id": main_session,
                    "title": "routed work",
                    "goal": "verify rpc pause/stop/proactive/notify",
                    "acceptance_criteria": [],
                    "session_refs": [{"workspace": "target", "session_id": target_session}],
                }
            }),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let work_id = created["work_id"].as_str().unwrap().to_string();

    // Triggering proactive in Auto mode enqueues a dispatched task in the target workspace.
    let triggered = main
        .call(
            "proactive.trigger",
            json!({
                "mode": "auto",
                "rule_id": "rule-auto",
                "condition_version": 1,
                "work_id": work_id,
                "usage_tokens": 100
            }),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(triggered["kind"], "dispatched");
    let target_tasks = target.tasks(&target_session).unwrap();
    assert_eq!(target_tasks.len(), 1);

    // RPC work.pause pushes pause_version to the target workspace queue.
    let paused = main
        .call(
            "work.pause",
            json!({"work_id": work_id}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(paused["lifecycle"], "paused");
    assert_eq!(paused["targets"][0]["workspace"], "target");
    assert!(target.tasks(&target_session).unwrap()[0].dispatch_paused);

    // RPC work.stop pushes stop_version and cancels the target workspace task.
    let stopped = main
        .call(
            "work.stop",
            json!({"work_id": work_id}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(stopped["lifecycle"], "cancelled");
    assert_eq!(stopped["targets"][0]["workspace"], "target");
    assert!(target.tasks(&target_session).unwrap()[0].cancel_requested);

    // RPC notify.send prepares and records delivery receipt for a configured target.
    let sent = main
        .call(
            "notify.send",
            json!({
                "work_id": work_id,
                "target": "desk",
                "content_ref": "done:1"
            }),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(sent["status"], "delivered");
    assert!(
        sent["receipt_ref"]
            .as_str()
            .unwrap()
            .starts_with("receipt:")
    );
}

#[test]
fn dispatched_runtime_retry_preserves_work_and_session_identity() {
    let root = TempDir::new().unwrap();
    let runtime = open(
        root.path(),
        Arc::new(cayw_runtime::fixture::OfflineProvider::default()),
        Model::new("fixture", "target"),
        RuntimeConfig::default(),
    );
    let first_session = runtime.create(None).unwrap().id;
    let second_session = runtime.create(None).unwrap().id;
    let dispatch = cayw_runtime::TaskDispatch {
        work_id: "work-a".into(),
        dispatch_key: "dispatch-a".into(),
        pause_version: 0,
        stop_version: 0,
    };
    let enqueue = |session: &str, dispatch: cayw_runtime::TaskDispatch| {
        runtime.enqueue_dispatched(
            session,
            cayw_runtime::TaskInput {
                text: "same input".into(),
                images: vec![],
            },
            cayw_runtime::TaskIntent::General,
            TaskPriority::default(),
            cayw_runtime::TaskResources::default(),
            dispatch,
        )
    };
    let original = enqueue(&first_session, dispatch.clone()).unwrap();
    assert_eq!(
        enqueue(&first_session, dispatch.clone()).unwrap().id,
        original.id
    );
    assert!(matches!(
        enqueue(&second_session, dispatch.clone()),
        Err(cayw_runtime::RuntimeError::InvalidInput(_))
    ));
    let different_work = cayw_runtime::TaskDispatch {
        work_id: "work-b".into(),
        ..dispatch.clone()
    };
    assert!(matches!(
        enqueue(&first_session, different_work),
        Err(cayw_runtime::RuntimeError::InvalidInput(_))
    ));
    let stored = runtime
        .task_by_dispatch_key(&dispatch.dispatch_key)
        .unwrap()
        .unwrap();
    assert_eq!(stored.session_id, first_session);
    assert_eq!(stored.work_id.as_deref(), Some("work-a"));
    assert!(runtime.tasks(&second_session).unwrap().is_empty());
}
