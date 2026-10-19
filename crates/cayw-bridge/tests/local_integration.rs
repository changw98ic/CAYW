#[path = "../examples/support/mod.rs"]
mod support;
use cayw_ai::{CancellationToken, Message, Model};
use cayw_bridge::*;
use cayw_codemode::CapabilityHost;
use cayw_host::{Host, HostConfig};
use serde_json::json;
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use support::*;

async fn setup() -> (Workspace, LocalClient, Arc<Stats>, Registry, Arc<Policy>) {
    let workspace = Workspace::new();
    let stats = Arc::new(Stats::default());
    let client = LocalClient::spawn(
        Path::new(env!("CARGO_BIN_EXE_cayw-bridge-local-fixture")),
        &workspace.0,
        stats.clone(),
    )
    .await
    .unwrap();
    let manifest = client.manifest().await.unwrap();
    let policy = Arc::new(Policy::default());
    let registry = Registry::new(Limits::default(), policy.clone()).unwrap();
    registry
        .register(identity("fixture-1"), manifest, Box::new(client.handle()))
        .unwrap();
    (workspace, client, stats, registry, policy)
}
async fn observed(counter: &AtomicUsize) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while counter.load(Ordering::Acquire) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn rejecting_duplicate_real_connection_wrappers_preserves_the_accepted_child() {
    use async_trait::async_trait;
    use cayw_codemode::CapabilityError;
    use std::sync::Barrier;
    struct Racing {
        inner: LocalHandle,
        barrier: Arc<Barrier>,
        delay: bool,
    }
    #[async_trait]
    impl SourceHandle for Racing {
        fn ownership_key(&self) -> &str {
            self.barrier.wait();
            if self.delay {
                std::thread::sleep(Duration::from_millis(2));
            }
            self.inner.ownership_key()
        }
        async fn request(
            &self,
            request: SourceRequest,
            cancel: CancellationToken,
        ) -> Result<SourceReply, CapabilityError> {
            self.inner.request(request, cancel).await
        }
        fn disconnect(&self) {
            self.inner.disconnect();
        }
    }
    let workspace = Workspace::new();
    let stats = Arc::new(Stats::default());
    let client = LocalClient::spawn(
        Path::new(env!("CARGO_BIN_EXE_cayw-bridge-local-fixture")),
        &workspace.0,
        stats.clone(),
    )
    .await
    .unwrap();
    let ordinary = client.manifest().await.unwrap();
    let mut heavy = ordinary.clone();
    let properties: serde_json::Map<_, _> = (0..20)
        .map(|i| (format!("f{i}"), json!({"type":"integer","minimum":0})))
        .collect();
    heavy.tools=(0..128).map(|i|ToolSpec {name:format!("heavy{i}"),description:"compile metadata".into(),input_schema:json!({"type":"object","properties":properties,"additionalProperties":false}),output_schema:None}).collect();
    let registry = Registry::new(Limits::default(), Arc::new(Policy::default())).unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let slow = Racing {
        inner: client.handle(),
        barrier: barrier.clone(),
        delay: false,
    };
    let fast = Racing {
        inner: client.handle(),
        barrier,
        delay: true,
    };
    let a = registry.clone();
    let a = std::thread::spawn(move || a.register(identity("fixture-1"), heavy, Box::new(slow)));
    let b = registry.clone();
    let b = std::thread::spawn(move || b.register(identity("fixture-1"), ordinary, Box::new(fast)));
    let a = a.join().unwrap();
    let b = b.join().unwrap();
    assert_ne!(a.is_ok(), b.is_ok());
    assert_eq!(stats.disconnected.load(Ordering::Acquire), 0);
    // Exercise the same accepted physical child even when the heavy manifest won.
    registry
        .consumer()
        .scoped(
            "skills.read",
            invocation(1),
            Arc::new(FixtureJournal::default()),
        )
        .unwrap()
        .call(
            "skills.read",
            json!({"name":"local/addition"}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(stats.skill_reads.load(Ordering::Acquire), 1);
    assert_eq!(stats.spawned.load(Ordering::Acquire), 1);
    registry.disconnect("local").unwrap();
    client.wait_closed().await;
    assert_eq!(stats.disconnected.load(Ordering::Acquire), 1);
}

#[tokio::test]
async fn one_real_connection_two_luau_consumers_and_application_skill_handle() {
    let (workspace, client, stats, registry, policy) = setup().await;
    let gate = Arc::new(Gate::default());
    let audit = Arc::new(Audit::default());
    let first = mode(host(
        &workspace.0,
        consumer(&registry),
        gate.clone(),
        audit.clone(),
    ));
    let second = mode(host(
        &workspace.0,
        consumer(&registry),
        gate.clone(),
        audit.clone(),
    ));
    let discovery = first
        .execute(
            r#"
        local names = cayw.call("mcp.discover", {server="local"})
        local spec = cayw.call("mcp.describe", {server="local",tool="add"})
        local skills = cayw.call("skills.list", {})
        local skill = cayw.call("skills.read", {name="local/addition"})
        return {names=names,spec=spec,skills=skills,skill=skill}
    "#,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(discovery.calls.len(), 4);
    assert_eq!(discovery.return_value["skill"]["value"], SKILL);
    assert_eq!(
        discovery.return_value["skills"]["skills"][0]["content_sha256"],
        content_sha256(SKILL)
    );
    assert_eq!(
        discovery.return_value["spec"]["source"]["source"]["version"],
        "fixture-1"
    );
    assert_eq!(
        discovery.return_value["spec"]["source"]["manifest_sha256"]
            .as_str()
            .unwrap()
            .len(),
        64
    );
    assert_eq!(stats.tool_calls.load(Ordering::Relaxed), 0);
    let result = second
        .execute(
            r#"return cayw.call("mcp.call",{server="local",tool="add",arguments={a=2,b=3}})"#,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(result.return_value["value"]["sum"], 5);
    let pid = result.return_value["value"]["server_pid"].clone();
    let another = first
        .execute(
            r#"return cayw.call("mcp.call",{server="local",tool="add",arguments={a=8,b=9}})"#,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(another.return_value["value"]["server_pid"], pid);
    assert_eq!(another.return_value["value"]["initializations"], 1);
    assert_eq!(stats.spawned.load(Ordering::Relaxed), 1);
    assert_eq!(stats.initializations.load(Ordering::Relaxed), 1);
    assert_eq!(stats.skill_reads.load(Ordering::Relaxed), 1);
    assert_eq!(gate.calls.lock().unwrap().len(), 6);
    assert_eq!(audit.records.lock().unwrap().len(), 12);
    assert_eq!(
        policy
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.operation == Operation::Invoke)
            .count(),
        2
    );
    registry.disconnect("local").unwrap();
    client.wait_closed().await;
    assert_eq!(stats.disconnected.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn host_gate_denies_discovery_skill_read_and_tool_execution_without_touching_source() {
    let (workspace, client, stats, registry, policy) = setup().await;
    let gate = Arc::new(Gate::default());
    let audit = Arc::new(Audit::default());
    let mode = mode(host(
        &workspace.0,
        consumer(&registry),
        gate.clone(),
        audit.clone(),
    ));
    for (name, args) in [
        ("mcp.discover", "{}"),
        ("skills.read", r#"{name="local/addition"}"#),
        (
            "mcp.call",
            r#"{server="local",tool="add",arguments={a=1,b=2}}"#,
        ),
    ] {
        gate.denied.lock().unwrap().insert(name.into());
        let error = mode
            .execute(
                &format!("return cayw.call({name:?},{args})"),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(!error.report.outcome_unknown);
        gate.denied.lock().unwrap().clear();
    }
    assert!(policy.calls.lock().unwrap().is_empty());
    assert!(audit.records.lock().unwrap().is_empty());
    assert_eq!(stats.skill_reads.load(Ordering::Relaxed), 0);
    assert_eq!(stats.tool_calls.load(Ordering::Relaxed), 0);
    registry.disconnect("local").unwrap();
    client.wait_closed().await;
}

#[tokio::test]
async fn registry_filters_names_schema_and_content_separately_and_discovery_never_grants_invoke() {
    let (workspace, client, stats, registry, policy) = setup().await;
    let gate = Arc::new(Gate::default());
    let audit = Arc::new(Audit::default());
    let host = host(
        &workspace.0,
        consumer(&registry),
        gate.clone(),
        audit.clone(),
    );
    policy
        .hidden
        .lock()
        .unwrap()
        .insert((Visibility::ToolSchema as u8, "add".into()));
    let listed = host_call(&host, "mcp.discover", json!({"server":"local"}))
        .await
        .unwrap();
    assert!(
        listed["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["name"] == "add")
    );
    assert!(listed["tools"][0].get("tool").is_none());
    assert!(
        host_call(
            &host,
            "mcp.describe",
            json!({"server":"local","tool":"add"})
        )
        .await
        .is_err()
    );
    assert!(
        host_call(
            &host,
            "mcp.call",
            json!({"server":"local","tool":"add","arguments":{"a":1,"b":2}})
        )
        .await
        .is_err()
    );
    policy.hidden.lock().unwrap().clear();
    policy.denied.lock().unwrap().insert("Invoke".into());
    host_call(
        &host,
        "mcp.describe",
        json!({"server":"local","tool":"add"}),
    )
    .await
    .unwrap();
    assert!(
        host_call(
            &host,
            "mcp.call",
            json!({"server":"local","tool":"add","arguments":{"a":1,"b":2}})
        )
        .await
        .is_err()
    );
    policy
        .hidden
        .lock()
        .unwrap()
        .insert((Visibility::ToolName as u8, "wait".into()));
    let names = host_call(&host, "mcp.discover", json!({})).await.unwrap();
    assert_eq!(names["tools"].as_array().unwrap().len(), 1);
    policy
        .hidden
        .lock()
        .unwrap()
        .insert((Visibility::SkillContent as u8, "addition".into()));
    assert_eq!(
        host_call(&host, "skills.list", json!({})).await.unwrap()["skills"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(
        host_call(&host, "skills.read", json!({"name":"local/addition"}))
            .await
            .is_err()
    );
    policy
        .hidden
        .lock()
        .unwrap()
        .insert((Visibility::SkillName as u8, "addition".into()));
    assert!(
        host_call(&host, "skills.list", json!({})).await.unwrap()["skills"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    policy.denied.lock().unwrap().insert("Discover".into());
    assert!(host_call(&host, "mcp.discover", json!({})).await.is_err());
    assert_eq!(stats.tool_calls.load(Ordering::Relaxed), 0);
    assert_eq!(stats.skill_reads.load(Ordering::Relaxed), 0);
    assert_eq!(gate.calls.lock().unwrap().len(), 10);
    assert_eq!(audit.records.lock().unwrap().len(), 20);
    registry.disconnect("local").unwrap();
    client.wait_closed().await;
}

#[tokio::test]
async fn canonical_cancel_is_sent_progress_is_observational_and_late_response_is_ignored() {
    let (workspace, client, stats, registry, policy) = setup().await;
    let host = host(
        &workspace.0,
        consumer(&registry),
        Arc::new(Gate::default()),
        Arc::new(Audit::default()),
    );
    let cancel = CancellationToken::new();
    let call = host.call(
        "mcp.call",
        json!({"server":"local","tool":"wait","arguments":{}}),
        cancel.clone(),
    );
    tokio::pin!(call);
    tokio::select! {
        result=&mut call => panic!("wait completed without cancellation: {result:?}"),
        _=observed(&stats.progress) => {}
    }
    assert_eq!(
        policy
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.operation == Operation::Invoke)
            .count(),
        1
    );
    let denied_gate = Arc::new(Gate::default());
    denied_gate.denied.lock().unwrap().insert("mcp.call".into());
    let denied = mode(support::host(
        &workspace.0,
        consumer(&registry),
        denied_gate.clone(),
        Arc::new(Audit::default()),
    ));
    assert!(
        denied
            .execute(
                r#"return cayw.call("mcp.call",{server="local",tool="add",arguments={a=1,b=2}})"#,
                CancellationToken::new()
            )
            .await
            .is_err()
    );
    assert_eq!(stats.tool_calls.load(Ordering::Relaxed), 1);
    assert_eq!(denied_gate.calls.lock().unwrap().as_slice(), &["mcp.call"]);
    cancel.cancel();
    let error = call.await.unwrap_err();
    assert!(error.outcome_unknown);
    observed(&stats.cancellations).await;
    observed(&stats.ignored_late).await;
    let snapshot = registry.snapshot("local").unwrap();
    assert!(snapshot.outcome_unknown);
    assert_eq!(snapshot.unresolved.len(), 1);
    assert!(
        host_call(
            &host,
            "mcp.call",
            json!({"server":"local","tool":"add","arguments":{"a":1,"b":2}})
        )
        .await
        .unwrap_err()
        .outcome_unknown
    );
    assert_eq!(stats.tool_calls.load(Ordering::Relaxed), 1);
    registry.disconnect("local").unwrap();
    client.wait_closed().await;
}

#[tokio::test]
async fn disconnect_reconnect_and_revoke_fence_old_consumers_and_never_replay() {
    let (workspace, client, stats, registry, _) = setup().await;
    let old = consumer(&registry);
    let first = old
        .call(
            "mcp.call",
            json!({"server":"local","tool":"add","arguments":{"a":1,"b":2}}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    registry.disconnect("local").unwrap();
    client.wait_closed().await;
    let new_stats = Arc::new(Stats::default());
    let replacement = LocalClient::spawn(
        Path::new(env!("CARGO_BIN_EXE_cayw-bridge-local-fixture")),
        &workspace.0,
        new_stats.clone(),
    )
    .await
    .unwrap();
    let manifest = replacement.manifest().await.unwrap();
    let provenance = registry
        .reconnect(
            identity("fixture-2"),
            manifest,
            Box::new(replacement.handle()),
        )
        .unwrap();
    assert!(provenance.generation > first["source"]["generation"].as_u64().unwrap());
    assert!(
        old.call("mcp.discover", json!({}), CancellationToken::new())
            .await
            .is_err()
    );
    let fresh = consumer(&registry);
    let second = fresh
        .call(
            "mcp.call",
            json!({"server":"local","tool":"add","arguments":{"a":3,"b":4}}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(second["source"]["source"]["version"], "fixture-2");
    assert_ne!(second["request"]["id"], first["request"]["id"]);
    assert_eq!(stats.tool_calls.load(Ordering::Relaxed), 1);
    assert_eq!(new_stats.tool_calls.load(Ordering::Relaxed), 1);
    registry.revoke("local").unwrap();
    replacement.wait_closed().await;
    assert!(
        fresh
            .call(
                "skills.read",
                json!({"name":"local/addition"}),
                CancellationToken::new()
            )
            .await
            .is_err()
    );
    assert_eq!(
        registry.snapshot("local").unwrap().status,
        SourceStatus::Revoked
    );
    registry.disconnect("local").unwrap();
    assert_eq!(
        registry.snapshot("local").unwrap().status,
        SourceStatus::Revoked
    );
}

#[tokio::test]
async fn real_source_disconnect_or_revoke_during_host_execution_retains_audit_unknown() {
    for revoke in [false, true] {
        let (workspace, client, stats, registry, _) = setup().await;
        let audit = Arc::new(Audit::default());
        let host = host(
            &workspace.0,
            consumer(&registry),
            Arc::new(Gate::default()),
            audit.clone(),
        );
        let mut pending = Box::pin(host.call(
            "mcp.call",
            json!({"server":"local","tool":"wait","arguments":{}}),
            CancellationToken::new(),
        ));
        tokio::select! {
            result = &mut pending => panic!("unexpected response {result:?}"),
            _ = observed(&stats.progress) => {}
        }
        if revoke {
            registry.revoke("local").unwrap();
        } else {
            registry.disconnect("local").unwrap();
        }
        assert!(pending.await.unwrap_err().outcome_unknown);
        client.wait_closed().await;
        assert!(host.outcome_unknown());
        let snapshot = registry.snapshot("local").unwrap();
        assert!(snapshot.outcome_unknown);
        assert_eq!(snapshot.unresolved.len(), 1);
        assert_eq!(stats.tool_calls.load(Ordering::Relaxed), 1);
        assert!(
            audit
                .records
                .lock()
                .unwrap()
                .iter()
                .any(|r| r.ends_with(":unknown"))
        );
    }
}

#[tokio::test]
async fn dropping_real_host_execution_cancels_transport_and_retains_both_audits() {
    let (workspace, client, stats, registry, _) = setup().await;
    let audit = Arc::new(Audit::default());
    let host = host(
        &workspace.0,
        consumer(&registry),
        Arc::new(Gate::default()),
        audit.clone(),
    );
    let mut pending = Box::pin(host.call(
        "mcp.call",
        json!({"server":"local","tool":"wait","arguments":{}}),
        CancellationToken::new(),
    ));
    tokio::select! {
        result = &mut pending => panic!("unexpected response {result:?}"),
        _ = observed(&stats.progress) => {}
    }
    drop(pending);
    observed(&stats.cancellations).await;
    observed(&stats.ignored_late).await;
    assert!(host.outcome_unknown());
    assert!(registry.snapshot("local").unwrap().outcome_unknown);
    assert_eq!(registry.snapshot("local").unwrap().unresolved.len(), 1);
    assert!(
        audit
            .records
            .lock()
            .unwrap()
            .iter()
            .any(|r| r.starts_with("drop:") && r.ends_with(":unknown"))
    );
    registry.disconnect("local").unwrap();
    client.wait_closed().await;
}

#[tokio::test]
async fn plain_host_and_agent_remain_usable_without_bridge() {
    use cayw_agent::Agent;
    use cayw_ai::{
        AssistantEvent, AssistantMessage, AssistantStream, Content, ModelRequest, Provider,
        StopReason,
    };
    use futures::{StreamExt, stream};
    struct Offline;
    impl Provider for Offline {
        fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
            let mut message = AssistantMessage::new(&request.model);
            message.content = vec![Content::text("plain agent")];
            message.stop_reason = StopReason::Stop;
            stream::iter([AssistantEvent::Done { message }]).boxed()
        }
    }
    let workspace = Workspace::new();
    let host = Arc::new(
        Host::new(
            &workspace.0,
            Arc::new(Gate::default()),
            Arc::new(Audit::default()),
            HostConfig::default(),
        )
        .unwrap(),
    );
    assert!(!host.capabilities().iter().any(|c| c.name == "mcp.call"));
    assert!(
        mode(host)
            .execute("return 2+3", CancellationToken::new())
            .await
            .is_ok()
    );
    let agent = Agent::new(Arc::new(Offline), Model::new("offline", "scripted"));
    let result = agent
        .prompt(Message::user("plain core path"))
        .await
        .unwrap();
    assert_eq!(result.last_message.stop_reason, StopReason::Stop);
    assert!(!agent.snapshot().is_running);
    let store = cayw_core::SessionStore::open(workspace.0.join("plain-core.sqlite3")).unwrap();
    let session = store
        .create_session("plain core without external sources")
        .unwrap();
    assert_eq!(
        store.snapshot(&session.id).unwrap().label,
        "plain core without external sources"
    );
    assert!(
        store
            .snapshot(&session.id)
            .unwrap()
            .unresolved_calls
            .is_empty()
    );
}
