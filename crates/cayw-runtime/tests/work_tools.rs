//! Model-facing WorkItem entry points. All work uses a temporary directory, the
//! offline scripted provider and an in-process fake of the unique Client router.
//! No test reads a user directory, calls a real model or uses the network.

use async_trait::async_trait;
use cayw_ai::{
    AssistantEvent, AssistantMessage, AssistantStream, CancellationToken, Content, Model,
    ModelRequest, Provider, StopReason, Usage, UsageStatus,
};
use cayw_runtime::{
    AcceptanceCheck, FileRules, Runtime, RuntimeConfig, TaskPriority, TaskState, WorkControl,
    WorkDispatchReply, WorkDispatchRequest, WorkItem, WorkItemSpec, WorkOwner, WorkRouter,
    WorkRouterError, WorkSessionNotice, WorkSessionNoticeReply, WorkStatusReport,
    WorkTargetOutcome, WorkTaskQuery, fixture::script_response,
};
use serde_json::{Value, json};
use std::{path::Path, sync::Arc, sync::Mutex, time::Duration};
use tempfile::TempDir;
use tokio::sync::Notify;

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

    async fn notify_session(
        &self,
        notice: WorkSessionNotice,
    ) -> Result<WorkSessionNoticeReply, WorkRouterError> {
        assert_eq!(notice.workspace, self.workspace);
        self.target
            .deliver_session_notice(&notice)
            .map_err(|error| WorkRouterError::known(error.to_string()))
    }
}

fn work_script() -> &'static str {
    r#"
cayw.call('work.list', {})
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
local accept_ok, accept_err = pcall(function()
  cayw.call('work.accept.program', {work_id=work.work_id, criterion_id='file'})
end)
assert(not accept_ok, tostring(accept_err))
local write_ok, write_err = pcall(function()
  cayw.call('fs.write', {path='should-not-exist.txt', text='no'})
end)
assert(not write_ok, tostring(write_err))
local report_ok = pcall(function()
  cayw.call('work.report', {work_id=work.work_id, text='the model says done'})
end)
local bad_target = pcall(function()
  cayw.call('work.notify.intent', {work_id=work.work_id, target='not-configured'})
end)
local intent = cayw.call('work.notify.intent', {work_id=work.work_id, target='desk'})
local notification_id = intent.notification.notification_id
local next = cayw.call('work.notify.next', {notification_id=notification_id})
local model_receipt_ok = pcall(function()
  cayw.call('work.notify.receipt', {
    notification_id=notification_id, status='delivered', receipt_ref='receipt-1'
  })
end)
assert(not model_receipt_ok, 'models must not be able to fabricate notification receipts')
local sent = cayw.call('work.notify.send', {notification_id=notification_id, text='ready'})
assert(sent.status == 'queued', 'local outbox write must stay queued, not delivered')
assert(type(sent.receipt_ref) ~= 'string', 'local outbox write must not fabricate receipt_ref')
local after_send = cayw.call('work.notify.next', {notification_id=notification_id})
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
  pause=pause, paused=paused, accept_ok=accept_ok, write_ok=write_ok, report_ok=report_ok,
  bad_target=bad_target, intent=intent, next=next, sent=sent,
  after_send=after_send, listed=listed, stop=stop,
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
    assert!(
        tasks[0].input.text.contains(&work_id),
        "{}",
        tasks[0].input.text
    );
    assert!(
        tasks[0]
            .input
            .text
            .contains("deliver out.txt in the target workspace"),
        "{}",
        tasks[0].input.text
    );
    assert!(
        tasks[0].input.text.contains("out.txt"),
        "{}",
        tasks[0].input.text
    );
    assert!(
        tasks[0].input.text.contains("write the result"),
        "{}",
        tasks[0].input.text
    );
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
    assert!(
        item.acceptance_records.is_empty(),
        "the main session must not record a review: {:?}",
        item.acceptance_records
    );
    assert!(!main.workspace().join("should-not-exist.txt").exists());

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
    assert!(settled("work.notify.send", "ready"));
    assert!(events.iter().any(|record| {
        record["event"]["type"] == "capability_settled"
            && record["event"]["call"]["name"] == "work.notify.list"
            && record["event"]["call"]["outcome"]["result"][0]["status"] == "queued"
            && record["event"]["call"]["outcome"]["result"][0]["receipt_ref"].is_null()
    }));
    assert!(events.iter().any(|record| {
        record["event"]["type"] == "capability_settled"
            && record["event"]["call"]["name"] == "work.notify.next"
            && record["event"]["call"]["outcome"]["result"]["decision"] == "await_delivery"
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
cayw.call('work.list', {})
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
    assert!(
        tasks[0]
            .input
            .text
            .contains("use the created target session"),
        "{}",
        tasks[0].input.text
    );
    assert!(
        tasks[0].input.text.contains("do the work"),
        "{}",
        tasks[0].input.text
    );
    assert!(
        tasks[0]
            .input
            .text
            .contains("The user judges this criterion"),
        "{}",
        tasks[0].input.text
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
async fn routed_main_session_turn_fails_without_work_create_and_dispatch() {
    struct TextOnly;
    impl Provider for TextOnly {
        fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
            let mut message = AssistantMessage::new(&request.model);
            message.content.push(Content::text("just talking"));
            message.stop_reason = StopReason::Stop;
            message.usage = Usage {
                input: 4,
                output: 4,
                total_tokens: 8,
                ..Default::default()
            };
            message.usage_status = UsageStatus::Reported;
            Box::pin(futures::stream::iter([AssistantEvent::Done { message }]))
        }
    }

    let temporary = TempDir::new().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let target = open(
        &root.join("target"),
        Arc::new(cayw_runtime::fixture::OfflineProvider::default()),
        Model::new("fixture", "target"),
        RuntimeConfig::default(),
    );
    let target_session = target.create(Some("Worker".into())).unwrap().id;
    let main = open(
        &root.join("main"),
        Arc::new(TextOnly),
        Model::new("fixture", "main"),
        main_config(&root, vec![]),
    );
    main.set_work_router(Arc::new(TargetRouter {
        target: target.clone(),
        workspace: "target".into(),
    }))
    .unwrap();
    let session = main.create(None).unwrap().id;
    let refused = main
        .prompt(&session, "hello", CancellationToken::new())
        .await
        .unwrap_err();
    assert!(
        refused.to_string().contains("check existing WorkItems"),
        "{refused}"
    );
    assert!(main.list_work_items().unwrap().is_empty());
    assert!(target.tasks(&target_session).unwrap().is_empty());
    let task = main.tasks(&session).unwrap().pop().unwrap();
    assert_eq!(task.state, cayw_runtime::TaskState::Failed);
    assert_eq!(task.work_id, None);
    assert_eq!(task.dispatch_key, None);
}

#[tokio::test]
async fn routed_turn_reuses_the_same_work_item_without_creating_another() {
    let temporary = TempDir::new().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let target = open(
        &root.join("target"),
        Arc::new(cayw_runtime::fixture::OfflineProvider::default()),
        Model::new("fixture", "target"),
        RuntimeConfig::default(),
    );
    let target_session = target.create(Some("Worker".into())).unwrap().id;
    let script = format!(
        r#"
local listed = cayw.call('work.list', {{}})
assert(#listed == 1, 'the existing WorkItem must be visible')
assert(listed[1].work_id == 'same-task', listed[1].work_id)
local dispatch = cayw.call('work.dispatch', {{
  work_id='same-task', workspace='target', session_id='{target_session}',
  text='continue the same task', dispatch_key='same-task-dispatch'
}})
return {{dispatch=dispatch, count=#listed}}
"#
    );
    let main = open(
        &root.join("main"),
        Arc::new(Script(script)),
        Model::new("fixture", "main"),
        main_config(&root, vec![]),
    );
    main.set_work_router(Arc::new(TargetRouter {
        target: target.clone(),
        workspace: "target".into(),
    }))
    .unwrap();
    let session = main.create(None).unwrap().id;
    main.create_work_item(cayw_runtime::WorkItemSpec {
        work_id: Some("same-task".into()),
        owner_main_session_id: session.clone(),
        title: "fps".into(),
        goal: "build the fps game".into(),
        acceptance_criteria: vec![cayw_runtime::AcceptanceCriterion {
            criterion_id: "play".into(),
            description: "the game runs in a browser".into(),
            check: None,
        }],
        user_source_refs: vec![],
        action_scope: vec![],
        approval_refs: vec![],
        session_refs: vec![],
        task_refs: vec![],
        artifact_refs: vec![],
        waiting_conditions: vec![],
        pending_decisions: vec![],
        next_check_at: None,
    })
    .unwrap();
    main.prompt(&session, "continue the fps game", CancellationToken::new())
        .await
        .unwrap();
    let items = main.list_work_items().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].work_id, "same-task");
    let tasks = target.tasks(&target_session).unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].work_id.as_deref(), Some("same-task"));
    assert_eq!(tasks[0].dispatch_key.as_deref(), Some("same-task-dispatch"));
    assert!(
        tasks[0].input.text.contains("build the fps game"),
        "{}",
        tasks[0].input.text
    );
    assert!(
        tasks[0].input.text.contains("continue the same task"),
        "{}",
        tasks[0].input.text
    );
}

#[tokio::test]
async fn routed_turn_refuses_create_and_dispatch_before_listing_existing_work() {
    let temporary = TempDir::new().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let target = open(
        &root.join("target"),
        Arc::new(cayw_runtime::fixture::OfflineProvider::default()),
        Model::new("fixture", "target"),
        RuntimeConfig::default(),
    );
    let target_session = target.create(Some("Worker".into())).unwrap().id;
    let script = format!(
        r#"
local create_ok, create_err = pcall(function()
  cayw.call('work.create', {{
    title='another',
    goal='should not be stored',
    acceptance_criteria={{ {{criterion_id='c', description='check'}} }}
  }})
end)
assert(not create_ok, 'create before work.list must be refused')
assert(string.find(tostring(create_err), 'work.list', 1, true) ~= nil, tostring(create_err))
local dispatch_ok, dispatch_err = pcall(function()
  cayw.call('work.dispatch', {{
    work_id='missing', workspace='target', session_id='{target_session}',
    text='skip the check', dispatch_key='skip-check'
  }})
end)
assert(not dispatch_ok, 'dispatch before work.list must be refused')
assert(string.find(tostring(dispatch_err), 'work.list', 1, true) ~= nil, tostring(dispatch_err))
return {{create_ok=create_ok, dispatch_ok=dispatch_ok}}
"#
    );
    let main = open(
        &root.join("main"),
        Arc::new(Script(script)),
        Model::new("fixture", "main"),
        main_config(&root, vec![]),
    );
    main.set_work_router(Arc::new(TargetRouter {
        target: target.clone(),
        workspace: "target".into(),
    }))
    .unwrap();
    let session = main.create(None).unwrap().id;
    let refused = main
        .prompt(&session, "start something else", CancellationToken::new())
        .await
        .unwrap_err();
    assert!(
        refused.to_string().contains("check existing WorkItems"),
        "{refused}"
    );
    assert!(main.list_work_items().unwrap().is_empty());
    assert!(target.tasks(&target_session).unwrap().is_empty());
    let events = serde_json::to_string(
        &main
            .events(&session, 0, 1000)
            .unwrap()
            .into_iter()
            .map(|record| serde_json::to_value(record).unwrap())
            .collect::<Vec<_>>(),
    )
    .unwrap();
    assert!(
        events.contains(
            "call work.list before creating or dispatching; reuse an existing WorkItem when it is the same task"
        ),
        "{events}"
    );
}

fn dispatch_page(session: &str) -> String {
    format!(
        r#"
cayw.call('work.list', {{}})
cayw.call('work.create', {{
  work_id='page-work',
  title='write the page',
  goal='create index.html at the workspace root',
  acceptance_criteria={{
    {{criterion_id='page-exists', description='index.html exists', check={{kind='path_exists', path='index.html'}}}}
  }}
}})
cayw.call('work.dispatch', {{
  work_id='page-work', workspace='target', session_id='{session}',
  text='write index.html', dispatch_key='dispatch-1'
}})
"#
    )
}

fn child_config(root: &Path) -> RuntimeConfig {
    RuntimeConfig {
        approval_rules: Some(Arc::new(FileRules::new(root.join("approval-rules.json")))),
        ..RuntimeConfig::default()
    }
}

fn plain_stop(model: &Model, text: &str) -> AssistantMessage {
    let mut message = AssistantMessage::new(model);
    message.content.push(Content::text(text));
    message.stop_reason = StopReason::Stop;
    message.usage = Usage {
        input: 1,
        output: 1,
        total_tokens: 2,
        ..Default::default()
    };
    message.usage_status = UsageStatus::Reported;
    message
}

struct MainOwner {
    main: Runtime,
}

#[async_trait]
impl WorkOwner for MainOwner {
    async fn work_item(&self, work_id: &str) -> Result<WorkItem, WorkRouterError> {
        self.main
            .work_item(work_id)
            .map_err(|error| WorkRouterError::known(error.to_string()))?
            .ok_or_else(|| WorkRouterError::known("WorkItem not found"))
    }

    async fn record_program_acceptance(
        &self,
        work_id: &str,
        criterion_id: &str,
        passed: bool,
        evidence: String,
    ) -> Result<WorkItem, WorkRouterError> {
        self.main
            .record_child_program_acceptance(work_id, criterion_id, passed, evidence)
            .map_err(|error| WorkRouterError::known(error.to_string()))
    }

    async fn report_status(
        &self,
        report: WorkStatusReport,
    ) -> Result<WorkSessionNoticeReply, WorkRouterError> {
        self.main
            .relay_task_status(report)
            .map_err(|error| WorkRouterError::known(error.to_string()))
    }
}

/// Dispatch script until a status notice arrives. The status turn only tells
/// the user; it does not dispatch again.
struct StatusMain {
    dispatch: String,
}

impl Provider for StatusMain {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let text = request
            .messages
            .iter()
            .map(|message| message.text())
            .collect::<Vec<_>>()
            .join("\n");
        let message = if text.contains("Task status updated") {
            plain_stop(&request.model, "told the user")
        } else {
            script_response(
                &request,
                &self.dispatch,
                &format!("status-{}", request.messages.len()),
            )
        };
        Box::pin(futures::stream::iter([AssistantEvent::Done { message }]))
    }
}

struct Done;

impl Provider for Done {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        Box::pin(futures::stream::iter([AssistantEvent::Done {
            message: plain_stop(&request.model, "done"),
        }]))
    }
}

struct HoldMain {
    dispatch: String,
    started: Arc<Notify>,
    release: Arc<Notify>,
    calls: Arc<Mutex<Vec<String>>>,
}

impl Provider for HoldMain {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let text = request
            .messages
            .iter()
            .map(|message| message.text())
            .collect::<Vec<_>>()
            .join("\n");
        let index = {
            let mut calls = self.calls.lock().unwrap();
            calls.push(text.clone());
            calls.len()
        };
        if text.contains("Task status updated") {
            let message = plain_stop(&request.model, "told the user");
            return Box::pin(futures::stream::iter([AssistantEvent::Done { message }]));
        }
        if index == 1 {
            let message = script_response(&request, &self.dispatch, "hold-dispatch");
            return Box::pin(futures::stream::iter([AssistantEvent::Done { message }]));
        }
        let started = self.started.clone();
        let release = self.release.clone();
        let model = request.model.clone();
        Box::pin(futures::stream::once(async move {
            started.notify_one();
            if index == 2 {
                release.notified().await;
            }
            AssistantEvent::Done {
                message: plain_stop(&model, "holding"),
            }
        }))
    }
}

const CHILD_REVIEW: &str = r#"
local wrong_ok, wrong_err = pcall(function()
  cayw.call('work.accept.program', {work_id='other-work', criterion_id='page-exists'})
end)
assert(not wrong_ok, 'a different work id must be refused')
assert(string.find(tostring(wrong_err), 'not dispatched', 1, true) ~= nil, tostring(wrong_err))
local accept = cayw.call('work.accept.program', {work_id='page-work', criterion_id='page-exists'})
assert(accept.passed == false, 'the missing page must fail')
"#;

#[tokio::test]
async fn child_session_reviews_its_own_work_and_reports_status() {
    let temporary = TempDir::new().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let child = open(
        &root.join("child"),
        Arc::new(Script(CHILD_REVIEW.into())),
        Model::new("fixture", "child"),
        child_config(&root.join("child")),
    );
    let child_session = child.create(Some("Worker".into())).unwrap().id;
    let main = open(
        &root.join("main"),
        Arc::new(StatusMain {
            dispatch: dispatch_page(&child_session),
        }),
        Model::new("fixture", "main"),
        main_config(&root, vec![]),
    );
    main.set_work_router(Arc::new(TargetRouter {
        target: child.clone(),
        workspace: "target".into(),
    }))
    .unwrap();
    let session = main.create(None).unwrap().id;
    main.prompt(&session, "dispatch the page", CancellationToken::new())
        .await
        .unwrap();
    child
        .set_work_owner(Arc::new(MainOwner { main: main.clone() }))
        .unwrap();
    let dispatched = child.tasks(&child_session).unwrap();
    assert_eq!(dispatched.len(), 1);
    child
        .resume(&dispatched[0].id, CancellationToken::new())
        .await
        .unwrap();
    main.wait_idle(&session).await;

    let child_tasks = child.tasks(&child_session).unwrap();
    assert_eq!(
        child_tasks.len(),
        1,
        "a failed check must not start another child task"
    );
    assert_eq!(
        child.task(&child_tasks[0].id).unwrap().state,
        cayw_runtime::TaskState::Completed
    );
    let item = main.work_item("page-work").unwrap().unwrap();
    assert_eq!(item.acceptance_records.len(), 1);
    assert_eq!(
        item.acceptance_records[0].result,
        cayw_runtime::AcceptanceResult::Failed
    );
    assert_eq!(
        item.acceptance_records[0].source,
        cayw_runtime::AcceptanceSource::Program
    );
    let notice = main
        .tasks(&session)
        .unwrap()
        .into_iter()
        .find(|task| {
            task.dispatch_key
                .as_deref()
                .is_some_and(|key| key.starts_with("status-notice:"))
        })
        .expect("the idle main session receives the task status");
    assert!(notice.input.text.contains("Task status updated"));
    assert!(notice.input.text.contains("completed"));
    assert!(!notice.input.text.contains("Program check"));
}

#[tokio::test]
async fn child_completion_steers_a_running_main_session() {
    let temporary = TempDir::new().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let calls = Arc::new(Mutex::new(Vec::new()));
    let child = open(
        &root.join("child"),
        Arc::new(Done),
        Model::new("fixture", "child"),
        RuntimeConfig::default(),
    );
    let child_session = child.create(Some("Worker".into())).unwrap().id;
    let main = open(
        &root.join("main"),
        Arc::new(HoldMain {
            dispatch: dispatch_page(&child_session),
            started: started.clone(),
            release: release.clone(),
            calls: calls.clone(),
        }),
        Model::new("fixture", "main"),
        main_config(&root, vec![]),
    );
    main.set_work_router(Arc::new(TargetRouter {
        target: child.clone(),
        workspace: "target".into(),
    }))
    .unwrap();
    let session = main.create(None).unwrap().id;
    let main_run = main.clone();
    let session_run = session.clone();
    let running = tokio::spawn(async move {
        main_run
            .prompt(&session_run, "dispatch the page", CancellationToken::new())
            .await
    });
    started.notified().await;
    child
        .set_work_owner(Arc::new(MainOwner { main: main.clone() }))
        .unwrap();
    let dispatched = child.tasks(&child_session).unwrap();
    assert_eq!(dispatched.len(), 1);
    child
        .resume(&dispatched[0].id, CancellationToken::new())
        .await
        .unwrap();
    release.notify_one();
    running.await.unwrap().unwrap();

    let seen = calls.lock().unwrap().join("\n");
    assert!(
        seen.contains("Task status updated"),
        "the running main session did not receive the status: {seen}"
    );
    assert!(seen.contains("completed"), "{seen}");
    assert!(!seen.contains("Program check"), "{seen}");
    assert_eq!(child.tasks(&child_session).unwrap().len(), 1);
    assert_eq!(main.tasks(&session).unwrap().len(), 1);
}

#[tokio::test]
async fn reading_one_work_item_status_finishes_without_dispatch() {
    let temporary = TempDir::new().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let child = open(
        &root.join("child"),
        Arc::new(cayw_runtime::fixture::OfflineProvider::default()),
        Model::new("fixture", "child"),
        RuntimeConfig::default(),
    );
    let script = r#"
cayw.call('work.list', {})
local work = cayw.call('work.create', {
  title='status only',
  goal='read the status',
  acceptance_criteria={{criterion_id='taste', description='the user accepts the result'}}
})
cayw.call('work.get', {work_id=work.work_id})
"#;
    let main = open(
        &root.join("main"),
        Arc::new(Script(script.into())),
        Model::new("fixture", "main"),
        main_config(&root, vec![]),
    );
    main.set_work_router(Arc::new(TargetRouter {
        target: child.clone(),
        workspace: "target".into(),
    }))
    .unwrap();
    let session = main.create(None).unwrap().id;
    main.prompt(&session, "what is the status", CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(main.list_work_items().unwrap().len(), 1);
    assert!(
        child
            .list()
            .unwrap()
            .iter()
            .all(|session| child.tasks(&session.id).unwrap().is_empty())
    );
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

    // RPC notify.send writes to the local outbox with truthful `queued` status
    // and no fabricated receipt_ref; host `notify.receipt` records real delivery.
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
    assert_eq!(sent["status"], "queued");
    assert!(sent["receipt_ref"].is_null());
    let next = main
        .call(
            "notify.next",
            json!({"notification_id": sent["notification_id"]}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(next, "await_delivery");
    let delivered = main
        .call(
            "notify.receipt",
            json!({
                "notification_id": sent["notification_id"],
                "status": "delivered",
                "receipt_ref": "channel-msg-42"
            }),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(delivered["status"], "delivered");
    assert_eq!(delivered["receipt_ref"], "channel-msg-42");
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

fn due_spec(session: &str, work_id: &str, title: &str, due: u64) -> WorkItemSpec {
    WorkItemSpec {
        work_id: Some(work_id.into()),
        owner_main_session_id: session.into(),
        title: title.into(),
        goal: "report when the check time arrives".into(),
        acceptance_criteria: vec![],
        user_source_refs: vec![],
        action_scope: vec![],
        approval_refs: vec![],
        session_refs: vec![],
        task_refs: vec![],
        artifact_refs: vec![],
        waiting_conditions: vec![],
        pending_decisions: vec![],
        next_check_at: Some(due),
    }
}

struct DueTalk;

impl Provider for DueTalk {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let mut message = AssistantMessage::new(&request.model);
        message.content.push(Content::text("told the user"));
        message.stop_reason = StopReason::Stop;
        message.usage = Usage {
            input: 1,
            output: 1,
            total_tokens: 2,
            ..Default::default()
        };
        message.usage_status = UsageStatus::Reported;
        Box::pin(futures::stream::iter([AssistantEvent::Done { message }]))
    }
}

#[tokio::test(flavor = "current_thread")]
async fn due_check_tells_the_user_and_does_not_dispatch() {
    let temporary = TempDir::new().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let child = open(
        &root.join("child"),
        Arc::new(cayw_runtime::fixture::OfflineProvider::default()),
        Model::new("fixture", "child"),
        RuntimeConfig::default(),
    );
    let main = open(
        &root.join("main"),
        Arc::new(DueTalk),
        Model::new("fixture", "main"),
        main_config(&root, vec![]),
    );
    main.set_work_router(Arc::new(TargetRouter {
        target: child.clone(),
        workspace: "target".into(),
    }))
    .unwrap();
    let session = main.create(Some("Main".into())).unwrap().id;
    main.create_work_item(due_spec(&session, "due-later", "later", u64::MAX))
        .unwrap();
    let cancelled = main
        .create_work_item(due_spec(&session, "due-stopped", "stopped", 1))
        .unwrap();
    main.stop_work_item(&cancelled.work_id).unwrap();
    let paused = main
        .create_work_item(due_spec(&session, "due-paused", "paused", 1))
        .unwrap();
    main.pause_work_item(&paused.work_id).unwrap();
    main.create_work_item(due_spec(&session, "due-open", "open", 1))
        .unwrap();

    let reports = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let reports: Vec<_> = main
                .tasks(&session)
                .unwrap()
                .into_iter()
                .filter(|task| {
                    task.dispatch_key
                        .as_deref()
                        .is_some_and(|key| key.starts_with("due-report:"))
                })
                .collect();
            let open = main.work_item("due-open").unwrap().unwrap();
            let paused = main.work_item("due-paused").unwrap().unwrap();
            if reports.len() == 2
                && reports
                    .iter()
                    .all(|task| task.state == TaskState::Completed)
                && open.next_check_at.is_none()
                && paused.next_check_at.is_none()
            {
                return reports;
            }
            if reports.iter().any(|task| task.state == TaskState::Failed) {
                panic!(
                    "due report failed: {reports:?} error={:?}",
                    main.due_report_error()
                );
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "due reports did not finish: {:?} error={:?}",
            main.tasks(&session).unwrap(),
            main.due_report_error()
        );
    });

    assert!(reports.iter().all(|task| {
        task.input.text.contains("Scheduled work check is due")
            && task.input.text.contains("Tell the user this status")
            && !task.input.text.contains("Program check")
    }));
    assert!(
        reports
            .iter()
            .any(|task| task.input.text.contains("due-open")
                && task.input.text.contains("lifecycle: open"))
    );
    assert!(
        reports
            .iter()
            .any(|task| task.input.text.contains("due-paused")
                && task.input.text.contains("lifecycle: paused"))
    );
    assert_eq!(
        main.work_item("due-later").unwrap().unwrap().next_check_at,
        Some(u64::MAX)
    );
    assert_eq!(
        main.work_item("due-stopped")
            .unwrap()
            .unwrap()
            .next_check_at,
        Some(1)
    );
    assert!(
        child
            .list()
            .unwrap()
            .iter()
            .all(|session| child.tasks(&session.id).unwrap().is_empty())
    );
    assert!(main.due_report_error().is_none());
    main.shutdown_background();
    child.shutdown_background();
}
