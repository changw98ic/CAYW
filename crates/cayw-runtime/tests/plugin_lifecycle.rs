#![cfg(feature = "plugins")]
use cayw_ai::{
    AssistantEvent, AssistantMessage, AssistantStream, CancellationToken, Content, Model,
    ModelRequest, Provider, ToolDefinition,
};
use cayw_core::{CoreEvent, SessionState};
use cayw_ipc::RpcHandler;
use cayw_plugins::{ExternalConfig, PluginContext, PluginKind, PluginManifest};
use cayw_runtime::{Runtime, RuntimeConfig, RuntimeError, ScopedPermission};
use serde_json::json;
use std::{path::PathBuf, sync::Arc, time::Duration};

struct Script(&'static str);
impl Provider for Script {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let message = if self.0.is_empty() {
            let mut message = AssistantMessage::new(&request.model);
            message.content.push(Content::text("offline result"));
            message
        } else {
            cayw_runtime::fixture::script_response(
                &request,
                self.0,
                &format!("plugin-{}", request.messages.len()),
            )
        };
        Box::pin(futures::stream::iter([AssistantEvent::Done { message }]))
    }
}
fn runtime(dir: &tempfile::TempDir, script: &'static str, permission: Option<&str>) -> Runtime {
    let config = RuntimeConfig {
        permissions: permission
            .map(|name| vec![ScopedPermission::exact(name, json!({}))])
            .unwrap_or_default(),
        ..Default::default()
    };
    Runtime::open(
        dir.path().join("sessions.db"),
        dir.path(),
        Arc::new(Script(script)),
        Model::new("fixture", "lifecycle"),
        config,
    )
    .unwrap()
}
fn external(mode: &str, gate: &std::path::Path) -> ExternalConfig {
    ExternalConfig::new(
        PathBuf::from(env!("CARGO_BIN_EXE_cayw-runtime-plugin-fixture")),
        vec![mode.into(), gate.to_str().unwrap().into()],
        PluginManifest {
            name: "process".into(),
            version: "1.0.0".into(),
            protocol_version: 1,
            kind: PluginKind::External,
            capabilities: vec![ToolDefinition {
                name: "process.echo".into(),
                description: "Offline process lifecycle fixture".into(),
                parameters: json!({"type":"object","additionalProperties":false}),
            }],
            permissions: Default::default(),
        },
    )
}

#[tokio::test]
async fn actual_idle_process_exit_freezes_rpc_get_list_and_event_observers() {
    let dir = tempfile::tempdir().unwrap();
    let gate = dir.path().join("exit");
    let rt = runtime(&dir, "", None);
    rt.load_external(
        external("idle-exit", &gate),
        PluginContext::new(["process.echo".into()], []),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let id = rt.create(None).unwrap().id;
    rt.bind_plugin(&id, "process").unwrap();
    let cursor = rt.get(&id).unwrap().cursor;
    tokio::fs::write(&gate, b"exit").await.unwrap();
    let events = rt
        .wait_events(
            &id,
            cursor,
            100,
            Duration::from_secs(5),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(events.iter().any(|record| matches!(&record.event, CoreEvent::Host { kind, data } if kind == "plugin_frozen" && data["decision"]["binding"]["reason"].as_str().is_some_and(|reason| reason.contains("process stopped")))));
    let view = rt
        .call(
            "session.get",
            json!({"session_id":id}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(view["frozen"], true);
    assert_eq!(view["binding"]["frozen"], true);
    let list = rt
        .call("session.list", json!({}), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        list.as_array()
            .unwrap()
            .iter()
            .find(|view| view["id"] == id)
            .unwrap()["binding"],
        view["binding"]
    );
    assert!(matches!(
        rt.prompt(&id, "blocked", CancellationToken::new()).await,
        Err(RuntimeError::Frozen(_))
    ));
}

#[tokio::test]
async fn actual_process_exit_during_capability_preserves_unknown_and_frozen_binding() {
    let dir = tempfile::tempdir().unwrap();
    let rt = runtime(
        &dir,
        "return cayw.call('process.echo',{})",
        Some("process.echo"),
    );
    rt.load_external(
        external("call-exit", &dir.path().join("unused")),
        PluginContext::new(["process.echo".into()], []),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let id = rt.create(None).unwrap().id;
    rt.bind_plugin(&id, "process").unwrap();
    let view = rt
        .prompt(&id, "actual process crash", CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(view.state, SessionState::NeedsReconciliation);
    assert!(view.frozen);
    assert!(
        view.binding
            .unwrap()
            .reason
            .unwrap()
            .contains("process stopped")
    );
    assert!(rt.events(&id, 0, 1000).unwrap().iter().any(
        |record| matches!(&record.event, CoreEvent::CapabilitySettled { call } if matches!(call.outcome, Some(cayw_core::CapabilityOutcome::OutcomeUnknown { .. })))
    ));
}

#[cfg(feature = "strongflow")]
mod strongflow {
    use super::*;
    use cayw_ai::{Message, StopReason, ToolCall};
    use cayw_plugins::strongflow::{
        ExecutionEvidence, PlanStep, StepStatus, StrongFlow, WorkflowPlan,
    };
    use std::collections::BTreeSet;
    fn plan(flow: &StrongFlow) {
        flow.create_plan(WorkflowPlan {
            id: "plan".into(),
            steps: vec![PlanStep {
                id: "test".into(),
                depends_on: BTreeSet::new(),
                acceptance_checks: BTreeSet::from(["actual-tests".into()]),
            }],
        })
        .unwrap();
    }
    fn evidence() -> ExecutionEvidence {
        ExecutionEvidence {
            source_hash: "a".repeat(64),
            build_id: "offline-test-build".into(),
            configuration: json!({"synthetic":true}),
            platform: std::env::consts::OS.into(),
            input: json!({"fixture":"durable-host-evidence"}),
            command: vec!["offline-test".into()],
            actual_result: json!({"exit_code":0}),
            checks: BTreeSet::from(["actual-tests".into()]),
            passed: true,
        }
    }
    #[tokio::test]
    async fn full_checkpoint_requires_restored_obligations_and_explicit_host_resume() {
        let dir = tempfile::tempdir().unwrap();
        let rt = runtime(&dir, "", None);
        let flow = Arc::new(StrongFlow::new());
        plan(&flow);
        rt.load_native(flow.clone(), PluginContext::default())
            .unwrap();
        let id = rt.create(None).unwrap().id;
        let first = rt.bind_strongflow(&id).unwrap();
        assert_eq!(
            first.workflow_state().unwrap().workflows["plan"].statuses["test"],
            StepStatus::Pending
        );
        flow.start_step("plan", "test").unwrap();
        flow.submit_evidence("plan", "test", evidence()).unwrap();
        let checkpoint = rt.checkpoint_strongflow(&id).unwrap();
        let expected = checkpoint.workflow_state().unwrap().clone();
        assert_eq!(
            expected.workflows["plan"].statuses["test"],
            StepStatus::Passed
        );
        rt.unload_plugin("strongflow").unwrap();
        let empty = rt
            .load_native(Arc::new(StrongFlow::new()), PluginContext::default())
            .unwrap();
        assert!(rt.get(&id).unwrap().frozen);
        assert!(rt.resume_strongflow(&id, &empty).is_err());
        assert!(rt.get(&id).unwrap().frozen);
        rt.unload_plugin("strongflow").unwrap();
        let restored = rt
            .load_native(
                Arc::new(StrongFlow::restore(expected.clone()).unwrap()),
                PluginContext::default(),
            )
            .unwrap();
        assert!(rt.get(&id).unwrap().frozen);
        let resumed = rt.resume_strongflow(&id, &restored).unwrap();
        assert_eq!(resumed.workflow_state(), Some(&expected));
        let view = rt.get(&id).unwrap();
        assert!(!view.frozen);
        assert_eq!(
            view.binding.as_ref().unwrap().registry_id,
            restored.registry_id
        );
        rt.prompt(&id, "resumed with evidence", CancellationToken::new())
            .await
            .unwrap();
        drop(rt);
        let rt = runtime(&dir, "", None);
        let restored = rt
            .load_native(
                Arc::new(StrongFlow::restore(expected.clone()).unwrap()),
                PluginContext::default(),
            )
            .unwrap();
        let frozen = rt.get(&id).unwrap();
        assert!(frozen.frozen);
        let before = frozen.cursor;
        rt.resume_strongflow(&id, &restored).unwrap();
        assert!(!rt.get(&id).unwrap().frozen);
        assert!(rt.events(&id, before, 100).unwrap().iter().any(
            |record| matches!(&record.event, CoreEvent::Host {kind,..} if kind == "plugin_resumed")
        ));
    }
    #[tokio::test]
    async fn frozen_unknown_session_stays_frozen_after_recover_and_rpc_reconcile() {
        use cayw_agent::AgentEvent;
        use cayw_core::{ReconciledOutcome, Reconciliation};
        let dir = tempfile::tempdir().unwrap();
        let rt = runtime(&dir, "", None);
        let flow = Arc::new(StrongFlow::new());
        plan(&flow);
        rt.load_native(flow, PluginContext::default()).unwrap();
        let id = rt.create(None).unwrap().id;
        rt.bind_strongflow(&id).unwrap();
        let lease = rt.store().begin_attempt(&id, "lost-host-worker").unwrap();
        rt.store()
            .append_agent_event(&lease, &AgentEvent::AgentStart)
            .unwrap();
        rt.store()
            .append_agent_event(&lease, &AgentEvent::TurnStart)
            .unwrap();
        let call = ToolCall::new("lost", "execute_code", json!({"script":"return nil"}));
        let mut answer = AssistantMessage::new(&Model::new("fixture", "unknown"));
        answer.content.push(Content::ToolCall(call.clone()));
        answer.stop_reason = StopReason::ToolUse;
        for event in [
            AgentEvent::MessageStart {
                message: Message::Assistant(answer.clone()),
            },
            AgentEvent::MessageEnd {
                message: Message::Assistant(answer),
            },
            AgentEvent::ToolExecutionStart { call },
        ] {
            rt.store().append_agent_event(&lease, &event).unwrap();
        }
        rt.store().mark_tool_started(&lease, "lost").unwrap();
        rt.unload_plugin("strongflow").unwrap();
        let recovered = rt.recover(&id).unwrap();
        assert!(recovered.frozen);
        assert_eq!(recovered.state, SessionState::NeedsReconciliation);
        let reconciliation = Reconciliation {
            call_id: "lost".into(),
            actor: "fixture-host".into(),
            evidence: "Synthetic fixture stopped before dispatch".into(),
            outcome: ReconciledOutcome::NotExecuted {
                reason: "test setup made no host call".into(),
            },
        };
        let reconciled = rt
            .call(
                "session.reconcile",
                json!({"session_id":id,"reconciliation":reconciliation}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(reconciled["frozen"], true);
        assert_eq!(reconciled["state"], "idle");
        assert_eq!(
            reconciled["binding"],
            serde_json::to_value(&recovered).unwrap()["binding"]
        );
        assert!(matches!(
            rt.enqueue(&id, "reconciliation must not thaw workflow"),
            Err(RuntimeError::Frozen(_))
        ));
    }
    #[tokio::test]
    async fn resume_storage_failure_keeps_source_frozen_and_workflow_requirements_intact() {
        let dir = tempfile::tempdir().unwrap();
        let rt = runtime(&dir, "", None);
        let flow = Arc::new(StrongFlow::new());
        plan(&flow);
        rt.load_native(flow.clone(), PluginContext::default())
            .unwrap();
        let id = rt.create(None).unwrap().id;
        let state = rt
            .bind_strongflow(&id)
            .unwrap()
            .workflow_state()
            .unwrap()
            .clone();
        rt.unload_plugin("strongflow").unwrap();
        let before = rt.get(&id).unwrap();
        let target = rt
            .load_native(
                Arc::new(StrongFlow::restore(state).unwrap()),
                PluginContext::default(),
            )
            .unwrap();
        let db = rusqlite::Connection::open(dir.path().join("sessions.db")).unwrap();
        db.execute_batch("CREATE TRIGGER test_resume_failure BEFORE INSERT ON core_events WHEN json_extract(NEW.record,'$.event.kind')='plugin_resumed' BEGIN SELECT RAISE(ABORT,'synthetic resume storage failure'); END;").unwrap();
        assert!(rt.resume_strongflow(&id, &target).is_err());
        assert_eq!(rt.get(&id).unwrap(), before);
        assert!(rt.enqueue(&id, "must remain frozen").is_err());
    }
    #[tokio::test]
    async fn unload_captures_host_changes_before_freezing_original_obligations() {
        let dir = tempfile::tempdir().unwrap();
        let rt = runtime(&dir, "", None);
        let flow = Arc::new(StrongFlow::new());
        plan(&flow);
        rt.load_native(flow.clone(), PluginContext::default())
            .unwrap();
        let id = rt.create(None).unwrap().id;
        rt.bind_strongflow(&id).unwrap();
        flow.start_step("plan", "test").unwrap();
        flow.submit_evidence("plan", "test", evidence()).unwrap();
        let expected = flow.export_state();
        rt.unload_plugin("strongflow").unwrap();
        let frozen = rt.get(&id).unwrap();
        assert_eq!(
            frozen.binding.unwrap().workflow_state.unwrap(),
            serde_json::to_value(expected).unwrap()
        );
    }
    #[tokio::test]
    async fn checkpoint_storage_failure_prevents_second_capability_and_keeps_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let config = RuntimeConfig {
            permissions: vec![ScopedPermission::exact(
                "strongflow.describe",
                json!({"workflow_id":"plan"}),
            )],
            ..Default::default()
        };
        let rt = Runtime::open(dir.path().join("sessions.db"), dir.path(), Arc::new(Script("cayw.call('strongflow.describe',{workflow_id='plan'}); return cayw.call('strongflow.describe',{workflow_id='plan'})")), Model::new("fixture", "checkpoint-failure"), config).unwrap();
        let flow = Arc::new(StrongFlow::new());
        plan(&flow);
        rt.load_native(flow, PluginContext::new(["strongflow.describe".into()], []))
            .unwrap();
        let id = rt.create(None).unwrap().id;
        rt.bind_strongflow(&id).unwrap();
        let db = rusqlite::Connection::open(dir.path().join("sessions.db")).unwrap();
        db.execute_batch("CREATE TRIGGER test_checkpoint_failure BEFORE INSERT ON core_events WHEN json_extract(NEW.record,'$.event.kind')='plugin_checkpoint' BEGIN SELECT RAISE(ABORT,'synthetic checkpoint storage failure'); END;").unwrap();
        let _ = rt
            .prompt(&id, "failed workflow checkpoint", CancellationToken::new())
            .await;
        assert_eq!(
            rt.get(&id).unwrap().state,
            SessionState::NeedsReconciliation
        );
        let events = rt.events(&id, 0, 1000).unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|record| matches!(&record.event, CoreEvent::CapabilityStarted { .. }))
                .count(),
            1
        );
        let terminal = events
            .iter()
            .find_map(|record| match &record.event {
                CoreEvent::CapabilitySettled { call } => Some(call),
                _ => None,
            })
            .unwrap();
        assert!(matches!(
            terminal.outcome,
            Some(cayw_core::CapabilityOutcome::OutcomeUnknown { .. })
        ));
        assert!(
            rt.enqueue(&id, "blocked after persistence failure")
                .is_err()
        );
    }
}

fn settled_calls(rt: &Runtime, id: &str) -> Vec<cayw_core::CapabilityCallRecord> {
    rt.events(id, 0, 1000)
        .unwrap()
        .into_iter()
        .filter_map(|record| match record.event {
            CoreEvent::CapabilitySettled { call } => Some(call),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn stopped_external_plugin_keeps_the_session_frozen_until_a_confirmed_fork() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("workspace")).unwrap();
    let mut config = RuntimeConfig::default();
    config.permissions = vec![
        ScopedPermission::exact(
            "fs.write",
            json!({"path":"cayw-demo.txt","content":"headless runtime demo"}),
        ),
        ScopedPermission::exact("fs.read", json!({"path":"cayw-demo.txt"})),
    ];
    let rt = Runtime::open(
        dir.path().join("sessions.db"),
        dir.path().join("workspace"),
        Arc::new(Script(
            "cayw.call('fs.write', {path='cayw-demo.txt', content='headless runtime demo'})\nlocal result = cayw.call('fs.read', {path='cayw-demo.txt'})\nreturn result",
        )),
        Model::new("fixture", "lifecycle"),
        config,
    )
    .unwrap();
    rt.load_external(
        external("idle-exit", &dir.path().join("unused")),
        PluginContext::new(["process.echo".into()], []),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let id = rt.create(None).unwrap().id;
    rt.bind_plugin(&id, "process").unwrap();
    rt.unload_plugin("process").unwrap();
    let view = rt
        .call(
            "session.get",
            json!({"session_id":id}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(view["frozen"], true);
    assert!(
        view["binding"]["reason"]
            .as_str()
            .unwrap()
            .contains("unloaded")
    );
    assert!(matches!(
        rt.prompt(&id, "blocked", CancellationToken::new()).await,
        Err(RuntimeError::Frozen(_))
    ));
    assert!(rt.fork(&id, "unconfirmed", false).is_err());
    let wrong = external("idle-exit", &dir.path().join("other"));
    let mut mismatched = wrong;
    mismatched.expected_manifest.version = "2.0.0".into();
    assert!(
        rt.load_external(
            mismatched,
            PluginContext::default(),
            CancellationToken::new()
        )
        .await
        .is_err()
    );
    rt.load_external(
        external("idle-exit", &dir.path().join("again")),
        PluginContext::new(["process.echo".into()], []),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(rt.get(&id).unwrap().frozen);
    let fork = rt.fork(&id, "ordinary", true).unwrap();
    assert_ne!(fork.id, id);
    assert!(fork.binding.is_none());
    let result = rt
        .prompt(&fork.id, "ordinary fork", CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(result.state, SessionState::Idle);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("workspace/cayw-demo.txt")).unwrap(),
        "headless runtime demo"
    );
    assert!(matches!(
        rt.prompt(&id, "source stays frozen", CancellationToken::new())
            .await,
        Err(RuntimeError::Frozen(_))
    ));
}

#[tokio::test]
async fn missing_external_capability_fails_on_the_original_call_without_a_process() {
    use cayw_ai::Message;
    use cayw_core::ExecutionOutcome;
    let dir = tempfile::tempdir().unwrap();
    let rt = runtime(&dir, "return cayw.call('process.echo',{})", None);
    let id = rt.create(None).unwrap().id;
    let view = rt
        .prompt(&id, "missing capability", CancellationToken::new())
        .await
        .unwrap();
    assert!(!view.frozen);
    assert!(view.unresolved_calls.is_empty());
    assert!(rt.list_plugins().is_empty());
    let execution = view
        .executions
        .iter()
        .find(|execution| execution.parent_call_id == "plugin-3")
        .expect("the original model call must own the failed execution");
    assert!(matches!(
        &execution.terminal,
        Some(ExecutionOutcome::Failed { error }) if error.contains("Unknown capability: process.echo")
    ));
    let tool = view
        .messages
        .iter()
        .find_map(|message| match message {
            Message::ToolResult(result) if result.tool_call_id == "plugin-3" => Some(result),
            _ => None,
        })
        .unwrap();
    assert!(tool.is_error);
    assert!(!tool.outcome_unknown);
    assert_eq!(
        tool.structured_content.as_ref().unwrap()["error"],
        "UnknownCapability: Unknown capability: process.echo"
    );
}

#[tokio::test]
async fn unauthorized_external_call_does_not_start_work_and_keeps_the_call_id() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("called");
    let rt = runtime(&dir, "return cayw.call('process.echo',{})", None);
    rt.load_external(
        external("touch", &marker),
        PluginContext::new(["process.echo".into()], []),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let id = rt.create(None).unwrap().id;
    rt.bind_plugin(&id, "process").unwrap();
    rt.prompt(&id, "denied", CancellationToken::new())
        .await
        .unwrap();
    assert!(!marker.exists());
    let call = settled_calls(&rt, &id)
        .into_iter()
        .find(|call| call.name == "process.echo")
        .unwrap();
    assert!(call.parent_call_id.starts_with("plugin-"));
    assert!(matches!(
        call.outcome,
        Some(
            cayw_core::CapabilityOutcome::Failed { .. }
                | cayw_core::CapabilityOutcome::NotExecuted { .. }
        )
    ));
}

#[tokio::test]
async fn external_call_over_the_capability_budget_stays_on_the_original_call() {
    let dir = tempfile::tempdir().unwrap();
    let config = RuntimeConfig {
        permissions: vec![ScopedPermission::exact("process.echo", json!({}))],
        max_capability_calls: 1,
        ..Default::default()
    };
    let rt = Runtime::open(
        dir.path().join("sessions.db"),
        dir.path(),
        Arc::new(Script(
            "cayw.call('process.echo',{}); return cayw.call('process.echo',{})",
        )),
        Model::new("fixture", "lifecycle"),
        config,
    )
    .unwrap();
    rt.load_external(
        external("echo", &dir.path().join("unused")),
        PluginContext::new(["process.echo".into()], []),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let id = rt.create(None).unwrap().id;
    rt.bind_plugin(&id, "process").unwrap();
    let _ = rt
        .prompt(&id, "over budget", CancellationToken::new())
        .await;
    let calls = settled_calls(&rt, &id);
    let limited = calls.iter().find(|call| {
        call.name == "process.echo"
            && call.parent_call_id.starts_with("plugin-")
            && matches!(
                &call.outcome,
                Some(cayw_core::CapabilityOutcome::Failed { error } | cayw_core::CapabilityOutcome::NotExecuted { reason: error })
                if error.contains("budget") || error.contains("capability count")
            )
    });
    assert!(limited.is_some(), "{calls:?}");
}

#[tokio::test]
async fn cancelled_and_crashed_external_calls_remain_tied_to_the_original_call() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("pid");
    let rt = runtime(
        &dir,
        "return cayw.call('process.echo',{})",
        Some("process.echo"),
    );
    rt.load_external(
        external("hang", &marker),
        PluginContext::new(["process.echo".into()], []),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let id = rt.create(None).unwrap().id;
    rt.bind_plugin(&id, "process").unwrap();
    let running = {
        let rt = rt.clone();
        let id = id.clone();
        tokio::spawn(async move {
            rt.prompt(&id, "cancel the plugin call", CancellationToken::new())
                .await
        })
    };
    let pid = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(value) = tokio::fs::read_to_string(&marker).await
                && let Ok(pid) = value.parse::<u32>()
            {
                break pid;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(rt.cancel(&id).unwrap());
    let _ = running.await.unwrap();
    let call = settled_calls(&rt, &id)
        .into_iter()
        .find(|call| call.name == "process.echo")
        .unwrap();
    assert!(call.parent_call_id.starts_with("plugin-"));
    assert!(matches!(
        call.outcome,
        Some(cayw_core::CapabilityOutcome::OutcomeUnknown { .. })
    ));
    #[cfg(unix)]
    {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let alive = std::process::Command::new("/bin/ps")
                .args(["-p", &pid.to_string()])
                .output()
                .unwrap();
            if !String::from_utf8_lossy(&alive.stdout).contains(&pid.to_string()) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "cancelled plugin process stayed alive"
            );
            tokio::task::yield_now().await;
        }
    }
    let crashed = runtime(
        &dir,
        "return cayw.call('process.echo',{})",
        Some("process.echo"),
    );
    crashed
        .load_external(
            external("call-exit", &dir.path().join("unused")),
            PluginContext::new(["process.echo".into()], []),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let crash_id = crashed.create(None).unwrap().id;
    crashed.bind_plugin(&crash_id, "process").unwrap();
    crashed
        .prompt(&crash_id, "crash", CancellationToken::new())
        .await
        .unwrap();
    let crash = settled_calls(&crashed, &crash_id)
        .into_iter()
        .find(|call| call.name == "process.echo")
        .unwrap();
    assert!(crash.parent_call_id.starts_with("plugin-"));
    assert!(matches!(
        crash.outcome,
        Some(cayw_core::CapabilityOutcome::OutcomeUnknown { .. })
    ));
}
