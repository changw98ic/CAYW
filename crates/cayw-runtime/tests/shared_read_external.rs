use async_trait::async_trait;
use cayw_ai::{AssistantEvent, AssistantStream, CancellationToken, Model, ModelRequest, Provider};
use cayw_core::{CapabilityOutcome, CoreEvent, SessionState};
use cayw_host::ExternalHost;
use cayw_runtime::{Runtime, RuntimeConfig, ScopedPermission};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct Script;
impl Provider for Script {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        Box::pin(futures::stream::iter(vec![AssistantEvent::Done {
            message: cayw_runtime::fixture::script_response(
                &request,
                "cayw.call('fs.read',{path='a.txt'}); cayw.call('mcp.call',{server='fixture',tool='change',arguments={}}); cayw.call('fs.read',{path='a.txt'}); return cayw.call('fs.read',{path='a.txt'})",
                &format!("external-read-{}", request.messages.len()),
            ),
        }]))
    }
}
struct Mutate {
    path: std::path::PathBuf,
    calls: AtomicUsize,
}
#[async_trait]
impl ExternalHost for Mutate {
    async fn call(
        &self,
        name: &str,
        args: Value,
        _: CancellationToken,
    ) -> Result<Value, cayw_codemode::CapabilityError> {
        assert_eq!(name, "mcp.call");
        assert_eq!(
            args,
            json!({"server":"fixture","tool":"change","arguments":{}})
        );
        self.calls.fetch_add(1, Ordering::SeqCst);
        std::fs::write(&self.path, "external bytes").unwrap();
        Ok(json!({"changed":true}))
    }
}

// Reusing the initial value after an unclassified external operation would return
// the old bytes; disabling fs.read entirely would lose the two later terminals.
#[tokio::test]
async fn external_mutation_disables_reuse_but_each_original_read_still_completes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("a.txt"), "initial bytes").unwrap();
    let external = Arc::new(Mutate {
        path: root.join("a.txt"),
        calls: AtomicUsize::new(0),
    });
    let mut config = RuntimeConfig::default();
    config.host.frozen_reads = true;
    config.host.external = Some(external.clone());
    config.permissions = vec![
        ScopedPermission::exact("fs.read", json!({"path":"a.txt"})),
        ScopedPermission::exact(
            "mcp.call",
            json!({"server":"fixture","tool":"change","arguments":{}}),
        ),
    ];
    let rt = Runtime::open(
        dir.path().join("session.db"),
        &root,
        Arc::new(Script),
        Model::new("fixture", "external-then-read"),
        config,
    )
    .unwrap();
    let id = rt.create(None).unwrap().id;
    assert_eq!(
        rt.prompt(&id, "read around external change", CancellationToken::new())
            .await
            .unwrap()
            .state,
        SessionState::Idle
    );
    assert_eq!(external.calls.load(Ordering::SeqCst), 1);
    let events = rt.events(&id, 0, 1000).unwrap();
    let calls: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.event {
            CoreEvent::CapabilitySettled { call } => Some(call),
            _ => None,
        })
        .collect();
    assert_eq!(calls.len(), 4);
    for call in &calls {
        assert!(
            matches!(call.outcome, Some(CapabilityOutcome::Completed { .. })),
            "{call:?}"
        );
    }
    for (index, expected) in [
        (0, "initial bytes"),
        (2, "external bytes"),
        (3, "external bytes"),
    ] {
        let Some(CapabilityOutcome::Completed { result }) = &calls[index].outcome else {
            unreachable!()
        };
        assert_eq!(result["content"], expected);
    }
    let reads: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.event {
            CoreEvent::Host { kind, data } if kind == "runtime_shared_read" => Some(data),
            _ => None,
        })
        .collect();
    assert_eq!(reads.len(), 3);
    assert_eq!(reads[0]["role"], "leader");
    assert_eq!(reads[1]["role"], "uncached");
    assert_eq!(reads[2]["role"], "uncached");
    assert_eq!(reads[1]["stamp"], reads[2]["stamp"]);
    assert!(
        reads[1]["stamp"]["generation"].as_u64().unwrap()
            > reads[0]["stamp"]["generation"].as_u64().unwrap()
    );
    assert_ne!(reads[0]["result_sha256"], reads[1]["result_sha256"]);
    for (evidence, call) in reads.iter().zip([calls[0], calls[2], calls[3]]) {
        assert_eq!(evidence["key"], serde_json::to_value(&call.key).unwrap());
        assert_eq!(evidence["source"], evidence["origin"]);
        assert_eq!(
            evidence["audit_id"],
            format!("{}:{}", call.key.execution_id, call.key.sequence)
        );
    }
    rt.recover(&id).unwrap();
}
