use cayw_ai::{AssistantEvent, AssistantStream, CancellationToken, Model, ModelRequest, Provider};
use cayw_core::{CapabilityOutcome, CoreEvent};
use cayw_runtime::{ApprovalChoice, Runtime, RuntimeConfig, ScopedPermission};
use serde_json::json;
use std::{sync::Arc, time::Duration};

struct Script(&'static str);
impl Provider for Script {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        Box::pin(futures::stream::iter(vec![AssistantEvent::Done {
            message: cayw_runtime::fixture::script_response(
                &request,
                self.0,
                &format!("fixture-{}", request.messages.len()),
            ),
        }]))
    }
}
async fn started(rt: &Runtime, id: &str) -> cayw_core::CapabilityCallRecord {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(call) = rt
                .events(id, 0, 1000)
                .unwrap()
                .into_iter()
                .find_map(|event| match event.event {
                    CoreEvent::CapabilityStarted { call } => Some(call),
                    _ => None,
                })
            {
                return call;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap()
}

// The root lock holds the independently admitted original read before capture.
// Replacing its revoked/expired grant must never let that same read continue.
#[tokio::test]
async fn original_grant_is_rechecked_during_fence_wait_and_replacement_cannot_substitute() {
    for expired in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("a.txt"), "original").unwrap();
        let fence = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join(".cayw-host-read-fence"))
            .unwrap();
        fence.lock().unwrap();
        let path = dir.path().join("session.db");
        let mut config = RuntimeConfig::default();
        config.host.frozen_reads = true;
        let rt = Runtime::open(
            &path,
            &root,
            Arc::new(Script("return cayw.call('fs.read',{path='a.txt'})")),
            Model::new("fixture", "read-authority"),
            config,
        )
        .unwrap();
        let id = rt.create(None).unwrap().id;
        let worker_rt = rt.clone();
        let worker_id = id.clone();
        let worker = tokio::spawn(async move {
            worker_rt
                .prompt(&worker_id, "read", CancellationToken::new())
                .await
        });
        let approval = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(approval) = rt.get(&id).unwrap().pending_approvals.first() {
                    break approval.clone();
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        rt.approve(&id, &approval.id, ApprovalChoice::AllowTask, "fixture-user")
            .unwrap();
        let original = started(&rt, &id).await;
        let grant = rt
            .grants(&id)
            .unwrap()
            .into_iter()
            .find(|grant| grant.approval_id == approval.id)
            .unwrap();
        let db = rusqlite::Connection::open(&path).unwrap();
        if expired {
            db.execute(
                "UPDATE runtime_grants SET expires=0 WHERE id=?1",
                [&grant.id],
            )
            .unwrap();
        } else {
            db.execute(
                "UPDATE runtime_grants SET state='revoked' WHERE id=?1",
                [&grant.id],
            )
            .unwrap();
        }
        db.execute("INSERT INTO runtime_grants(id,session,attempt,approval,capability,scope,mode,actor,expires,state,consumed) SELECT 'replacement',session,attempt,'replacement-approval',capability,scope,mode,actor,?2,'active',0 FROM runtime_grants WHERE id=?1", rusqlite::params![grant.id, cayw_ai::timestamp_ms() as i64 + 300_000]).unwrap();
        let _result = tokio::time::timeout(Duration::from_secs(10), worker)
            .await
            .unwrap()
            .unwrap();
        drop(fence);
        let events = rt.events(&id, 0, 1000).unwrap();
        let terminals: Vec<_> = events
            .iter()
            .filter_map(|event| match &event.event {
                CoreEvent::CapabilitySettled { call } => Some(call),
                _ => None,
            })
            .collect();
        assert_eq!(terminals.len(), 1);
        assert_eq!(terminals[0].key, original.key);
        assert_eq!(terminals[0].authorization, original.authorization);
        assert!(
            matches!(terminals[0].outcome, Some(CapabilityOutcome::Failed { .. })),
            "original wait accepted replacement authority: {:?}",
            terminals[0]
        );
        assert!(!events.iter().any(|event| matches!(&event.event, CoreEvent::Host { kind, .. } if kind == "runtime_shared_read")));
        assert_eq!(rt.store().approvals(&id).unwrap().len(), 1);
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "original"
        );
    }
}

#[test]
fn shared_read_recovery_rejects_changed_full_original_grant() {
    use cayw_agent::AgentEvent;
    use cayw_ai::{AssistantMessage, Content, Message, StopReason, ToolCall};
    use cayw_core::{
        ApprovalDecision, CapabilityAuthorization, CapabilityCallKey, CapabilityInput,
        ExecutionInput,
    };
    for field in [None, Some("expires_at"), Some("mode")] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir(&root).unwrap();
        let model = Model::new("fixture", "grant-evidence");
        let rt = Runtime::open(
            dir.path().join("session.db"),
            &root,
            Arc::new(Script("return 1")),
            model.clone(),
            RuntimeConfig::default(),
        )
        .unwrap();
        let id = rt.create(None).unwrap().id;
        let store = rt.store();
        let lease = store.begin_attempt(&id, "original-worker").unwrap();
        store
            .append_agent_event(&lease, &AgentEvent::AgentStart)
            .unwrap();
        store
            .append_agent_event(&lease, &AgentEvent::TurnStart)
            .unwrap();
        let parent = ToolCall::new(
            "original-parent",
            "execute_code",
            json!({"action":"exec","script":"return cayw.call('fs.read',{path='a.txt'})"}),
        );
        let mut message = AssistantMessage::new(&model);
        message.stop_reason = StopReason::ToolUse;
        message.content.push(Content::ToolCall(parent.clone()));
        store
            .append_agent_event(
                &lease,
                &AgentEvent::MessageStart {
                    message: Message::Assistant(message.clone()),
                },
            )
            .unwrap();
        store
            .append_agent_event(
                &lease,
                &AgentEvent::MessageEnd {
                    message: Message::Assistant(message),
                },
            )
            .unwrap();
        store
            .append_agent_event(
                &lease,
                &AgentEvent::ToolExecutionStart {
                    call: parent.clone(),
                },
            )
            .unwrap();
        store.mark_tool_started(&lease, &parent.id).unwrap();
        store
            .begin_execution(
                &lease,
                ExecutionInput {
                    execution_id: "original-cell".into(),
                    parent_call_id: parent.id.clone(),
                    task_id: "original-task".into(),
                    source_hash: "original-source".into(),
                },
            )
            .unwrap();
        let key = CapabilityCallKey {
            execution_id: "original-cell".into(),
            sequence: 0,
        };
        let arguments = json!({"path":"a.txt"});
        store
            .plan_capability(
                &lease,
                CapabilityInput {
                    key: key.clone(),
                    name: "fs.read".into(),
                    arguments: arguments.clone(),
                },
            )
            .unwrap();
        let approval = store
            .request_approval(
                &lease,
                cayw_core::ApprovalInput {
                    task_id: "original-task".into(),
                    target: cayw_core::ApprovalTarget::CapabilityCall { key: key.clone() },
                    capability: "fs.read".into(),
                    scope: arguments.to_string(),
                    reason: "fixture exact original read".into(),
                },
            )
            .unwrap();
        store
            .decide_approval(&id, &approval.id, ApprovalDecision::Approve, "fixture-user")
            .unwrap();
        let grant = cayw_runtime::GrantRecord {
            id: "original-grant".into(),
            session_id: id.clone(),
            attempt_id: lease.attempt_id.clone(),
            approval_id: approval.id,
            capability: "fs.read".into(),
            scope: arguments.to_string(),
            mode: "allow_once".into(),
            actor: "fixture-user".into(),
            expires_at: cayw_ai::timestamp_ms() + 300_000,
            state: "active".into(),
            consumed: true,
        };
        store.append_host_event(&lease,"permission_admitted",json!({"key":key,"task_id":"original-task","grant_id":grant.id,"approval_id":grant.approval_id,"grant":grant})).unwrap();
        let authorization = CapabilityAuthorization {
            actor: grant.actor.clone(),
            target: key.clone(),
            task_id: "original-task".into(),
            capability: "fs.read".into(),
            scope: grant.scope.clone(),
            decision: ApprovalDecision::Approve,
            approval_id: Some(grant.approval_id.clone()),
            external_provenance: None,
        };
        store
            .start_capability(&lease, &key, serde_json::to_value(&authorization).unwrap())
            .unwrap();
        let result = json!({"path":"a.txt","content":"saved bytes","bytes":11,"sha256":cayw_bridge::content_sha256("saved bytes")});
        let fingerprint = cayw_cells::HostResult::value_hash(&result);
        let origin = json!({"execution":{"identity":{"session_id":id,"task_id":"original-task","attempt_id":lease.attempt_id,"permission_context":format!("{}:{}",lease.worker_id,lease.fence)},"cell_id":"original-cell","parent_call_id":parent.id,"source_hash":"original-source"},"sequence":1});
        let stamp = json!({"workspace":"held-workspace","incarnation":format!("sha256:{}",cayw_bridge::content_sha256("incarnation")),"generation":0,"semantics":format!("sha256:{}",cayw_bridge::content_sha256("semantics"))});
        let mut changed = serde_json::to_value(&grant).unwrap();
        if field == Some("expires_at") {
            changed["expires_at"] = json!(grant.expires_at + 1);
        }
        if field == Some("mode") {
            changed["mode"] = json!("allow_task");
        }
        let evidence = json!({"origin":origin,"lease":lease,"key":key,"task_id":"original-task","name":"fs.read","arguments":arguments,"authorization":authorization,"grant":changed,"request":null,"audit_id":"original-cell:0","source":origin,"role":"leader","stamp":stamp,"dependencies":{"fingerprint":fingerprint,"permission_context":origin["execution"]["identity"]["permission_context"],"versions":{"workspace":stamp["workspace"],"incarnation":stamp["incarnation"],"generation":"0","semantics":stamp["semantics"]}},"result_sha256":fingerprint});
        store
            .append_host_event(&lease, "runtime_shared_read", evidence)
            .unwrap();
        store
            .settle_capability(&lease, &key, CapabilityOutcome::Completed { result })
            .unwrap();
        let cursor = rt.get(&id).unwrap().cursor;
        if let Some(field) = field {
            assert!(
                rt.recover(&id).is_err(),
                "recovery accepted changed original grant {field}"
            );
            assert_eq!(rt.get(&id).unwrap().cursor, cursor);
        } else {
            rt.recover(&id).unwrap();
        }
    }
}

// Removing per-response Runtime byte charging would let the third ready read
// finish. The allowance covers two complete results plus all three arguments.
#[tokio::test]
async fn ready_read_charges_each_original_runtime_response_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("a.txt"), "frozen").unwrap();
    let arguments = json!({"path":"a.txt"});
    let value = json!({"path":"a.txt","content":"frozen","bytes":6,"sha256":cayw_bridge::content_sha256("frozen")});
    let mut config = RuntimeConfig::default();
    config.host.frozen_reads = true;
    config.permissions = vec![ScopedPermission::exact("fs.read", arguments.clone())];
    config.max_capability_bytes = 3 * serde_json::to_vec(&arguments).unwrap().len()
        + 2 * serde_json::to_vec(&json!({"ok":true,"value":value}))
            .unwrap()
            .len()
        - 1;
    let rt = Runtime::open(dir.path().join("session.db"),&root,
        Arc::new(Script("cayw.call('fs.read',{path='a.txt'}); cayw.call('fs.read',{path='a.txt'}); return cayw.call('fs.read',{path='a.txt'})")),
        Model::new("fixture","read-response-budget"),config).unwrap();
    let id = rt.create(None).unwrap().id;
    let _ = rt
        .prompt(&id, "bounded ready bytes", CancellationToken::new())
        .await;
    let events = rt.events(&id, 0, 1000).unwrap();
    let calls: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.event {
            CoreEvent::CapabilitySettled { call } if call.name == "fs.read" => Some(call),
            _ => None,
        })
        .collect();
    assert_eq!(calls.len(), 3);
    assert!(matches!(
        calls[0].outcome,
        Some(CapabilityOutcome::Completed { .. })
    ));
    assert!(matches!(
        calls[1].outcome,
        Some(CapabilityOutcome::Completed { .. })
    ));
    assert!(matches!(
        calls[2].outcome,
        Some(CapabilityOutcome::NotExecuted { .. })
    ));
    let roles: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.event {
            CoreEvent::Host { kind, data } if kind == "runtime_shared_read" => {
                Some(data["role"].clone())
            }
            _ => None,
        })
        .collect();
    assert_eq!(roles, vec![json!("leader"), json!("ready")]);
}

// A warm cache value is still a separate Runtime and Host admission. Bypassing
// either counter would allow all three identical reads to complete.
#[tokio::test]
async fn warm_read_reuse_spends_each_original_host_and_runtime_call_budget() {
    for host_limit in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("a.txt"), "frozen").unwrap();
        let mut config = RuntimeConfig::default();
        config.host.frozen_reads = true;
        config.permissions = vec![ScopedPermission::exact("fs.read", json!({"path":"a.txt"}))];
        if host_limit {
            config.host.max_calls = 2;
        } else {
            config.max_capability_calls = 2;
        }
        let rt = Runtime::open(dir.path().join("session.db"), &root, Arc::new(Script("cayw.call('fs.read',{path='a.txt'}); cayw.call('fs.read',{path='a.txt'}); return cayw.call('fs.read',{path='a.txt'})")), Model::new("fixture", "read-budget"), config).unwrap();
        let id = rt.create(None).unwrap().id;
        let _result = rt
            .prompt(&id, "bounded reads", CancellationToken::new())
            .await;
        let events = rt.events(&id, 0, 1000).unwrap();
        let calls: Vec<_> = events
            .iter()
            .filter_map(|event| match &event.event {
                CoreEvent::CapabilitySettled { call } => Some(call),
                _ => None,
            })
            .collect();
        assert_eq!(calls.len(), 3);
        assert_eq!(
            calls
                .iter()
                .filter(|call| matches!(call.outcome, Some(CapabilityOutcome::Completed { .. })))
                .count(),
            2
        );
        assert!(matches!(
            calls[2].outcome,
            Some(CapabilityOutcome::NotExecuted { .. })
        ));
        let reads: Vec<_> = events
            .iter()
            .filter_map(|event| match &event.event {
                CoreEvent::Host { kind, data } if kind == "runtime_shared_read" => Some(data),
                _ => None,
            })
            .collect();
        assert_eq!(reads.len(), 2);
        assert_eq!(reads[0]["role"], "leader");
        assert_eq!(reads[1]["role"], "ready");
        assert_ne!(reads[0]["audit_id"], reads[1]["audit_id"]);
    }
}
