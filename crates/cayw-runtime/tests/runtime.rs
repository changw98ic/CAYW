use cayw_ai::{
    AssistantEvent, AssistantStream, CancellationToken, Message, Model, ModelRequest, Provider,
};
use cayw_core::SessionState;
use cayw_runtime::{ApprovalChoice, Runtime, RuntimeConfig, RuntimeError, ScopedPermission};
use futures::stream;
use serde_json::json;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

// These are provider fixtures. Every file effect below is performed by the real
// Luau -> Host -> workspace implementation, never by the provider fixture.
struct ScriptProvider {
    script: String,
    requests: Mutex<Vec<ModelRequest>>,
}
impl ScriptProvider {
    fn new(script: &str) -> Self {
        Self {
            script: script.into(),
            requests: Mutex::new(vec![]),
        }
    }
}
impl Provider for ScriptProvider {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let mut answer = cayw_runtime::fixture::script_response(
            &request,
            &self.script,
            &format!("script-{}", request.messages.len()),
        );
        answer.usage.total_tokens = 7;
        self.requests.lock().unwrap().push(request);
        Box::pin(stream::iter(vec![AssistantEvent::Done { message: answer }]))
    }
}
struct HangingProvider;
impl Provider for HangingProvider {
    fn stream(&self, _: ModelRequest, _: CancellationToken) -> AssistantStream {
        Box::pin(stream::pending())
    }
}
fn config() -> RuntimeConfig {
    RuntimeConfig {
        max_run_time: Duration::from_secs(10),
        ..Default::default()
    }
}
fn runtime(
    temp: &tempfile::TempDir,
    provider: Arc<dyn Provider>,
    config: RuntimeConfig,
) -> Runtime {
    Runtime::open(
        temp.path().join("session.db"),
        temp.path().join("workspace"),
        provider,
        Model::new("fixture", "runtime-tests"),
        config,
    )
    .unwrap()
}
async fn pending(runtime: &Runtime, id: &str) -> cayw_core::ApprovalRequest {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(pending) = runtime.get(id).unwrap().pending_approvals.first() {
                return pending.clone();
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn prompt_executes_real_luau_host_write_read_and_reopens_history() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("workspace")).unwrap();
    let provider = Arc::new(ScriptProvider::new(
        "cayw.call('fs.write', {path='result.txt', content='runtime evidence'})\nlocal r = cayw.call('fs.read', {path='result.txt'})\nreturn r",
    ));
    let mut settings = config();
    settings.permissions = vec![
        ScopedPermission::exact(
            "fs.write",
            json!({"path":"result.txt","content":"runtime evidence"}),
        ),
        ScopedPermission::exact("fs.read", json!({"path":"result.txt"})),
    ];
    let rt = runtime(&temp, provider.clone(), settings.clone());
    let session = rt.create(Some("file round trip".into())).unwrap();
    let result = rt
        .prompt(&session.id, "write and read", CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(temp.path().join("workspace/result.txt")).unwrap(),
        "runtime evidence"
    );
    assert!(
        result
            .messages
            .last()
            .unwrap()
            .text()
            .contains("runtime evidence")
    );
    let first = rt.get(&session.id).unwrap();
    assert_eq!(first.state, SessionState::Idle);
    assert!(
        first
            .messages
            .iter()
            .any(|m| matches!(m, Message::ToolResult(_)))
    );
    let events = rt.events(&session.id, 0, 1000).unwrap();
    assert!(
        events
            .windows(2)
            .all(|pair| pair[0].cursor + 1 == pair[1].cursor)
    );
    assert!(!rt.audit(&session.id, 0, 100).unwrap().is_empty());
    {
        let requests = provider.requests.lock().unwrap();
        let instruction = requests[0]
            .messages
            .iter()
            .find(|m| matches!(m, Message::System { .. }))
            .unwrap()
            .text();
        assert!(
            instruction.contains("action=exec")
                && instruction.contains("action=wait")
                && instruction.contains("tool_search(query,limit)")
                && instruction.contains("toolDefinition(name)")
                && instruction.contains("cayw.call(name, arguments)")
        );
        assert!(
            !instruction.contains("fs.write")
                && !instruction.contains("parameters")
                && !instruction.contains("host.call")
        );
    }
    drop(rt);
    let reopened = runtime(&temp, provider.clone(), settings);
    assert_eq!(reopened.get(&session.id).unwrap(), first);
    reopened
        .prompt(&session.id, "again", CancellationToken::new())
        .await
        .unwrap();
    assert!(
        provider
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|request| request.messages.len() > first.messages.len())
    );
}

#[tokio::test]
async fn default_gate_waits_for_explicit_allow_and_denial_never_writes() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("workspace")).unwrap();
    let rt = runtime(
        &temp,
        Arc::new(ScriptProvider::new(
            "return cayw.call('fs.write', {path='approved.txt', content='yes'})",
        )),
        config(),
    );
    let id = rt.create(None).unwrap().id;
    let task_rt = rt.clone();
    let task_id = id.clone();
    let task = tokio::spawn(async move {
        task_rt
            .prompt(&task_id, "write", CancellationToken::new())
            .await
    });
    let approval = pending(&rt, &id).await;
    assert!(!temp.path().join("workspace/approved.txt").exists());
    rt.approve(&id, &approval.id, ApprovalChoice::Deny, "test-user")
        .unwrap();
    task.await.unwrap().unwrap();
    assert!(!temp.path().join("workspace/approved.txt").exists());
    let task_rt = rt.clone();
    let task_id = id.clone();
    let task = tokio::spawn(async move {
        task_rt
            .prompt(&task_id, "write with approval", CancellationToken::new())
            .await
    });
    let approval = pending(&rt, &id).await;
    rt.approve(&id, &approval.id, ApprovalChoice::AllowOnce, "test-user")
        .unwrap();
    task.await.unwrap().unwrap();
    assert_eq!(
        std::fs::read_to_string(temp.path().join("workspace/approved.txt")).unwrap(),
        "yes"
    );
}

#[tokio::test]
async fn invalid_prompt_and_future_drop_leave_no_running_session() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("workspace")).unwrap();
    let rt = runtime(&temp, Arc::new(HangingProvider), config());
    let id = rt.create(None).unwrap().id;
    assert!(matches!(
        rt.prompt(&id, "", CancellationToken::new()).await,
        Err(RuntimeError::InvalidInput(_))
    ));
    assert_eq!(rt.health().unwrap().running, 0);
    let task_rt = rt.clone();
    let task_id = id.clone();
    let task = tokio::spawn(async move {
        task_rt
            .prompt(&task_id, "hang", CancellationToken::new())
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while rt.get(&id).unwrap().state != SessionState::Running {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    task.abort();
    let _ = task.await;
    tokio::time::timeout(Duration::from_secs(5), rt.wait_idle(&id))
        .await
        .unwrap();
    assert_eq!(rt.health().unwrap().running, 0);
    assert_eq!(rt.get(&id).unwrap().state, SessionState::Idle);
    assert!(
        rt.audit(&id, 0, 100)
            .unwrap()
            .iter()
            .any(|record| record.kind == "run_cancelled")
    );
}

#[tokio::test]
async fn capability_approval_remains_bound_to_started_parent_and_durable_task() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("workspace")).unwrap();
    let rt = runtime(
        &temp,
        Arc::new(ScriptProvider::new(
            "return cayw.call('fs.write', {path='blocked.txt', content='no'})",
        )),
        config(),
    );
    let id = rt.create(None).unwrap().id;
    let worker_rt = rt.clone();
    let worker_id = id.clone();
    let task = tokio::spawn(async move {
        worker_rt
            .prompt(
                &worker_id,
                "request an ungranted write",
                CancellationToken::new(),
            )
            .await
    });
    let request = pending(&rt, &id).await;
    let snapshot = rt.get(&id).unwrap();
    let parent = snapshot
        .unresolved_calls
        .iter()
        .find(|call| call.state == cayw_core::CallState::Started)
        .unwrap();
    let cayw_core::ApprovalTarget::CapabilityCall { key } = &request.input.target else {
        panic!("capability approval must identify the exact internal call");
    };
    let execution = snapshot
        .executions
        .iter()
        .find(|execution| execution.execution_id == key.execution_id)
        .unwrap();
    assert_eq!(execution.parent_call_id, parent.call.id);
    assert_eq!(execution.task_id, request.input.task_id);
    assert_eq!(key.sequence, 0);
    assert_eq!(request.input.task_id, rt.tasks(&id).unwrap()[0].id);
    assert_ne!(request.input.task_id, request.attempt_id);
    assert!(!temp.path().join("workspace/blocked.txt").exists());
    rt.approve(&id, &request.id, ApprovalChoice::Deny, "test-user")
        .unwrap();
    let result = task.await.unwrap().unwrap();
    assert!(result.pending_approvals.is_empty());
    assert!(
        result
            .messages
            .iter()
            .any(|m| matches!(m,Message::ToolResult(r) if r.is_error))
    );
    assert!(!temp.path().join("workspace/blocked.txt").exists());
}

#[tokio::test]
async fn approval_cancellation_and_revocation_are_durable() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("workspace")).unwrap();
    let rt = runtime(
        &temp,
        Arc::new(ScriptProvider::new(
            "return cayw.call('fs.write', {path='cancelled.txt', content='no'})",
        )),
        config(),
    );
    let id = rt.create(None).unwrap().id;
    let task_rt = rt.clone();
    let task_id = id.clone();
    let task = tokio::spawn(async move {
        task_rt
            .prompt(&task_id, "request", CancellationToken::new())
            .await
    });
    let approval = pending(&rt, &id).await;
    rt.cancel(&id).unwrap();
    task.await.unwrap().unwrap();
    assert!(!temp.path().join("workspace/cancelled.txt").exists());
    assert!(
        rt.approve(&id, &approval.id, ApprovalChoice::AllowTask, "test-user")
            .is_err()
    );
    assert_eq!(rt.health().unwrap().running, 0);
}

#[tokio::test]
async fn token_budget_blocks_tool_execution_before_side_effect() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("workspace")).unwrap();
    let mut settings = config();
    settings.max_total_tokens = 3;
    settings.permissions.push(ScopedPermission::exact(
        "fs.write",
        json!({"path":"limit.txt","content":"no"}),
    ));
    let rt = runtime(
        &temp,
        Arc::new(ScriptProvider::new(
            "return cayw.call('fs.write', {path='limit.txt', content='no'})",
        )),
        settings,
    );
    let id = rt.create(None).unwrap().id;
    let answer = rt
        .prompt(&id, "too many tokens", CancellationToken::new())
        .await
        .unwrap();
    assert!(
        matches!(answer.messages.last(), Some(Message::Assistant(last)) if last.stop_reason.is_failure())
    );
    assert!(!temp.path().join("workspace/limit.txt").exists());
    assert_eq!(rt.health().unwrap().running, 0);
}
