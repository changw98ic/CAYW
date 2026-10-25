use cayw_ai::{AssistantEvent, AssistantStream, CancellationToken, Model, ModelRequest, Provider};
use cayw_core::{CapabilityOutcome, CoreEvent, SessionState};
use cayw_runtime::{Runtime, RuntimeConfig, ScopedPermission, TaskState};
use futures::stream;
use serde_json::json;
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

struct UnknownExternal(Arc<AtomicUsize>);
#[async_trait::async_trait]
impl cayw_host::ExternalHost for UnknownExternal {
    async fn call(
        &self,
        _: &str,
        _: serde_json::Value,
        _: CancellationToken,
    ) -> Result<serde_json::Value, cayw_codemode::CapabilityError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(cayw_codemode::CapabilityError::unknown(
            "synthetic original effect has no known outcome",
        ))
    }
}

struct ScriptProvider(&'static str);
impl Provider for ScriptProvider {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let message = cayw_runtime::fixture::script_response(
            &request,
            self.0,
            &format!("restriction-{}", request.messages.len()),
        );
        Box::pin(stream::iter([AssistantEvent::Done { message }]))
    }
}

fn open(temp: &tempfile::TempDir, script: &'static str, config: RuntimeConfig) -> Runtime {
    Runtime::open(
        temp.path().join("sessions.sqlite"),
        temp.path().join("workspace"),
        Arc::new(ScriptProvider(script)),
        Model::new("fixture", "restriction-runtime"),
        config,
    )
    .unwrap()
}

#[tokio::test]
async fn caught_known_error_can_continue_through_real_host_with_user_globals_and_coroutines() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let script = r#"
        count = 40
        function increment(value) return value + 1 end
        local worker = coroutine.create(function()
            coroutine.yield(increment(count))
            return increment(increment(count))
        end)
        local resumed, first = coroutine.resume(worker)
        assert(resumed and first == 41)
        local finished, second = coroutine.resume(worker)
        assert(finished and second == 42)
        local ok = pcall(function()
            return cayw.call('fs.read', {path='missing.txt'})
        end)
        assert(not ok)
        return cayw.call('fs.write', {path='result.txt', content='42'})
    "#;
    let config = RuntimeConfig {
        max_run_time: Duration::from_secs(10),
        permissions: vec![
            ScopedPermission::exact("fs.read", json!({"path":"missing.txt"})),
            ScopedPermission::exact("fs.write", json!({"path":"result.txt","content":"42"})),
        ],
        ..Default::default()
    };
    let runtime = open(&temp, script, config);
    let session = runtime.create(None).unwrap();
    let view = runtime
        .prompt(
            &session.id,
            "recover from a known missing file",
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(workspace.join("result.txt")).unwrap(),
        "42"
    );
    assert_eq!(view.state, SessionState::Idle);
    assert!(view.unresolved_capabilities.is_empty());
    assert_eq!(
        runtime.tasks(&session.id).unwrap()[0].state,
        TaskState::Completed
    );
    let events = runtime.events(&session.id, 0, 1000).unwrap();
    let settled = events
        .iter()
        .filter_map(|record| match &record.event {
            CoreEvent::CapabilitySettled { call } => Some(call),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(settled.len(), 2);
    assert!(settled.iter().any(|call| {
        call.name == "fs.read" && matches!(call.outcome, Some(CapabilityOutcome::Failed { .. }))
    }));
    assert!(settled.iter().any(|call| {
        call.name == "fs.write" && matches!(call.outcome, Some(CapabilityOutcome::Completed { .. }))
    }));
}

#[tokio::test]
async fn caught_host_call_budget_cannot_complete_the_runtime_task() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(workspace.join("source.txt"), "one allowed Host read").unwrap();
    let script = r#"
        cayw.call('fs.read', {path='source.txt'})
        pcall(function()
            cayw.call('fs.write', {path='blocked.txt', content='must not exist'})
        end)
        return 'must not complete after the Host call budget was exhausted'
    "#;
    let mut config = RuntimeConfig {
        max_run_time: Duration::from_secs(10),
        permissions: vec![
            ScopedPermission::exact("fs.read", json!({"path":"source.txt"})),
            ScopedPermission::exact(
                "fs.write",
                json!({"path":"blocked.txt","content":"must not exist"}),
            ),
        ],
        ..Default::default()
    };
    config.host.max_calls = 1;
    let runtime = open(&temp, script, config);
    let session = runtime.create(None).unwrap();
    let _ = runtime
        .prompt(
            &session.id,
            "respect the shared Host budget",
            CancellationToken::new(),
        )
        .await;
    assert!(!workspace.join("blocked.txt").exists());
    let events = runtime.events(&session.id, 0, 1000).unwrap();
    assert!(events.iter().any(|record| match &record.event {
        CoreEvent::CapabilitySettled { call } if call.name == "fs.write" => {
            matches!(&call.outcome, Some(CapabilityOutcome::NotExecuted { reason }) if reason.contains("budget"))
        }
        _ => false,
    }));
    assert!(matches!(
        runtime.tasks(&session.id).unwrap()[0].state,
        TaskState::Failed | TaskState::Cancelled
    ));
}

#[tokio::test]
async fn caught_cell_output_budget_stops_the_runtime_task() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let script = r#"
        text('retained')
        pcall(function() text('over budget') end)
        cayw.call('fs.write', {path='blocked.txt', content='must not exist'})
    "#;
    let mut config = RuntimeConfig {
        max_run_time: Duration::from_secs(10),
        permissions: vec![ScopedPermission::exact(
            "fs.write",
            json!({"path":"blocked.txt","content":"must not exist"}),
        )],
        ..Default::default()
    };
    config.cells.output_items = 1;
    let runtime = open(&temp, script, config);
    let session = runtime.create(None).unwrap();
    let _ = runtime
        .prompt(
            &session.id,
            "stop at the original cell output budget",
            CancellationToken::new(),
        )
        .await;
    assert_eq!(
        runtime.tasks(&session.id).unwrap()[0].state,
        TaskState::Failed
    );
    assert!(!workspace.join("blocked.txt").exists());
    let events = runtime.events(&session.id, 0, 1000).unwrap();
    assert!(!events.iter().any(|record| matches!(
        &record.event,
        CoreEvent::CapabilitySettled { call } if call.name == "fs.write"
    )));
    assert!(
        runtime
            .get(&session.id)
            .unwrap()
            .unresolved_capabilities
            .is_empty()
    );
}

#[tokio::test]
async fn nested_protected_coroutine_cannot_hide_unknown_or_allow_another_effect() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let script = r#"
        local worker = coroutine.create(function()
            coroutine.yield('ready')
            return 'ready for the main script'
        end)
        local resumed, value = coroutine.resume(worker)
        assert(resumed and value == 'ready')
        pcall(function()
            pcall(function()
                cayw.call('search.query', {query='synthetic unknown'})
            end)
            coroutine.resume(worker)
            cayw.call('fs.write', {path='blocked.txt', content='must not exist'})
        end)
        return 'must not complete after an unknown effect'
    "#;
    let mut config = RuntimeConfig {
        max_run_time: Duration::from_secs(10),
        permissions: vec![
            ScopedPermission::exact("search.query", json!({"query":"synthetic unknown"})),
            ScopedPermission::exact(
                "fs.write",
                json!({"path":"blocked.txt","content":"must not exist"}),
            ),
        ],
        ..Default::default()
    };
    let calls = Arc::new(AtomicUsize::new(0));
    config.host.external = Some(Arc::new(UnknownExternal(calls.clone())));
    let runtime = open(&temp, script, config);
    let session = runtime.create(None).unwrap();
    let _ = runtime
        .prompt(
            &session.id,
            "retain the original unknown",
            CancellationToken::new(),
        )
        .await;
    let view = runtime.get(&session.id).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(view.state, SessionState::NeedsReconciliation);
    assert_eq!(view.unresolved_capabilities.len(), 1);
    assert_eq!(view.unresolved_capabilities[0].name, "search.query");
    assert!(matches!(
        &view.unresolved_capabilities[0].outcome,
        Some(CapabilityOutcome::OutcomeUnknown { .. })
    ));
    assert!(!workspace.join("blocked.txt").exists());
    assert_eq!(
        runtime.tasks(&session.id).unwrap()[0].state,
        TaskState::OutcomeUnknown
    );
    assert!(matches!(
        runtime.enqueue(&session.id, "must remain stopped"),
        Err(cayw_runtime::RuntimeError::NeedsReconciliation)
    ));
}

#[tokio::test]
async fn caught_runtime_call_budget_cannot_complete_or_write_another_effect() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(workspace.join("source.txt"), "one allowed read").unwrap();
    let script = r#"
        cayw.call('fs.read', {path='source.txt'})
        pcall(function()
            cayw.call('fs.write', {path='blocked.txt', content='must not exist'})
        end)
        return 'must not complete after the budget was exhausted'
    "#;
    let config = RuntimeConfig {
        max_run_time: Duration::from_secs(10),
        max_capability_calls: 1,
        permissions: vec![
            ScopedPermission::exact("fs.read", json!({"path":"source.txt"})),
            ScopedPermission::exact(
                "fs.write",
                json!({"path":"blocked.txt","content":"must not exist"}),
            ),
        ],
        ..Default::default()
    };
    let runtime = open(&temp, script, config);
    let session = runtime.create(None).unwrap();
    let _ = runtime
        .prompt(
            &session.id,
            "stop when the task call budget is exhausted",
            CancellationToken::new(),
        )
        .await;
    assert!(!workspace.join("blocked.txt").exists());
    let events = runtime.events(&session.id, 0, 1000).unwrap();
    assert!(events.iter().any(|record| match &record.event {
        CoreEvent::CapabilitySettled { call } if call.name == "fs.write" => {
            matches!(&call.outcome, Some(CapabilityOutcome::NotExecuted { reason }) if reason.contains("capability count"))
        }
        _ => false,
    }));
    let task = runtime.tasks(&session.id).unwrap().remove(0);
    assert!(matches!(
        task.state,
        TaskState::Failed | TaskState::Cancelled
    ));
    assert!(
        runtime
            .get(&session.id)
            .unwrap()
            .unresolved_capabilities
            .is_empty()
    );
}
