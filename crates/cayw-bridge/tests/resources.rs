//! Resource bridge red tests: explicit manifest metadata, catalog listing and
//! journaled reads through the existing Operation/SourceOperation path.
#[path = "../examples/support/mod.rs"]
mod support;
use async_trait::async_trait;
use cayw_ai::CancellationToken;
use cayw_bridge::*;
use cayw_codemode::CapabilityError;
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use tokio::sync::Notify;

const URI: &str = "fixture://source/manual";
const MIME: &str = "text/markdown";
const BODY: &str = "# Manual\nBounded resource body.\n";

#[derive(Default)]
struct State {
    calls: AtomicUsize,
    entered: Notify,
    release: Notify,
    wait: AtomicBool,
    reply: Mutex<Option<Value>>,
    observed: Mutex<Option<SourceOperation>>,
}
struct Handle {
    key: &'static str,
    mime: &'static str,
    state: Arc<State>,
}
#[async_trait]
impl SourceHandle for Handle {
    fn ownership_key(&self) -> &str {
        self.key
    }
    async fn request(
        &self,
        request: SourceRequest,
        _: CancellationToken,
    ) -> Result<SourceReply, CapabilityError> {
        self.state.calls.fetch_add(1, Ordering::SeqCst);
        *self.state.observed.lock().unwrap() = Some(request.operation.clone());
        self.state.entered.notify_one();
        if self.state.wait.load(Ordering::Acquire) {
            self.state.release.notified().await;
        }
        let value = match &request.operation {
            SourceOperation::GetPrompt { .. } => {
                return Err(cayw_codemode::CapabilityError::new("unexpected"));
            }
            SourceOperation::ReadResource { uri } => self
                .state
                .reply
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| json!({"uri":uri,"mime_type":self.mime,"text":BODY})),
            _ => json!({}),
        };
        Ok(SourceReply {
            key: request.key,
            value,
        })
    }
    fn disconnect(&self) {}
}

fn identity(id: &str, version: &str) -> SourceIdentity {
    SourceIdentity {
        id: id.into(),
        version: version.into(),
        origin: "application:resource-fixture".into(),
    }
}
fn resource(uri: &str) -> ResourceSpec {
    ResourceSpec {
        uri: uri.into(),
        description: "Fixed application resource".into(),
        mime_type: MIME.into(),
    }
}
fn manifest(uri: &str) -> Manifest {
    Manifest {
        resources: vec![resource(uri)],
        ..Manifest::default()
    }
}
fn make_registry(limits: Limits, policy: Arc<support::Policy>) -> Registry {
    Registry::new(limits, policy).unwrap()
}
fn handle(key: &'static str, state: Arc<State>) -> Box<dyn SourceHandle> {
    Box::new(Handle {
        key,
        mime: MIME,
        state,
    })
}
fn runtime(registry: &Registry) -> support::FixtureRuntime {
    support::consumer(registry)
}
async fn read(
    bridge: &support::FixtureRuntime,
    server: &str,
    uri: &str,
) -> Result<Value, CapabilityError> {
    bridge
        .call(
            "mcp.resources.read",
            json!({"server":server,"uri":uri}),
            CancellationToken::new(),
        )
        .await
}

#[test]
fn manifest_requires_explicit_resources_and_round_trips_the_new_contract() {
    let source = manifest(URI);
    let encoded = serde_json::to_value(&source).unwrap();
    let decoded: Manifest = serde_json::from_value(encoded.clone()).unwrap();
    assert_eq!(decoded.resources, source.resources);
    let mut legacy = encoded;
    legacy.as_object_mut().unwrap().remove("resources");
    assert!(
        serde_json::from_value::<Manifest>(legacy).is_err(),
        "a manifest without explicit resources must not inherit an implicit empty list"
    );
}

#[tokio::test]
async fn resource_manifest_is_explicit_validated_and_pinned_into_the_source_digest() {
    let policy = Arc::new(support::Policy::default());
    let source = manifest(URI);
    let registry = make_registry(Limits::default(), policy);
    let provenance = registry
        .register(
            identity("source", "1"),
            source.clone(),
            handle("resource-good", Arc::default()),
        )
        .unwrap();
    let pinned = registry.consumer().binding_snapshot();
    assert_eq!(
        provenance.manifest_sha256,
        content_sha256(&serde_json::to_string(&source).unwrap())
    );
    assert_eq!(
        pinned.sources["source"].manifest_sha256,
        provenance.manifest_sha256
    );

    let mut changed = source.clone();
    changed.resources[0].description = "different description".into();
    let other = make_registry(Limits::default(), Arc::new(support::Policy::default()));
    let changed_digest = other
        .register(
            identity("source", "1"),
            changed,
            handle("resource-changed", Arc::default()),
        )
        .unwrap()
        .manifest_sha256;
    assert_ne!(provenance.manifest_sha256, changed_digest);

    let reject = |candidate: Manifest, limits: Limits| {
        let registry = make_registry(limits, Arc::new(support::Policy::default()));
        registry.register(
            identity("source", "1"),
            candidate,
            handle("resource-reject", Arc::default()),
        )
    };
    let mut duplicate = manifest(URI);
    duplicate.resources.push(resource(URI));
    assert!(reject(duplicate, Limits::default()).is_err());
    for uri in [
        String::new(),
        "no-scheme".into(),
        "fixture://".into(),
        "fixture://x y".into(),
        "fixture://x\n".into(),
        format!("fixture://{}", "x".repeat(2048)),
    ] {
        let mut bad = manifest(URI);
        bad.resources[0].uri = uri;
        assert!(reject(bad, Limits::default()).is_err());
    }
    for mime in [
        String::new(),
        "text".into(),
        "text/".into(),
        "/plain".into(),
        "text/plain; charset=utf-8".into(),
        "text/plain\n".into(),
    ] {
        let mut bad = manifest(URI);
        bad.resources[0].mime_type = mime;
        assert!(reject(bad, Limits::default()).is_err());
    }
    let mut long_description = manifest(URI);
    long_description.resources[0].description = "x".repeat(8193);
    assert!(reject(long_description, Limits::default()).is_err());
    let mut too_many = manifest(URI);
    too_many.resources.push(resource("fixture://source/second"));
    assert!(
        reject(
            too_many,
            Limits {
                max_resources_per_source: 1,
                ..Limits::default()
            }
        )
        .is_err()
    );

    let empty = make_registry(Limits::default(), Arc::new(support::Policy::default()));
    empty
        .register(
            identity("source", "1"),
            Manifest::default(),
            handle("resource-empty", Arc::default()),
        )
        .unwrap();
    assert!(
        runtime(&empty)
            .call("mcp.resources.list", json!({}), CancellationToken::new())
            .await
            .unwrap()["resources"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn resources_list_uses_the_catalog_path_and_respects_source_filters_and_visibility() {
    let policy = Arc::new(support::Policy::default());
    let registry = make_registry(Limits::default(), policy.clone());
    registry
        .register(
            identity("source", "1"),
            manifest(URI),
            handle("resource-list-one", Arc::default()),
        )
        .unwrap();
    registry
        .register(
            identity("second", "1"),
            manifest("fixture://second/handbook"),
            handle("resource-list-two", Arc::default()),
        )
        .unwrap();
    let bridge = runtime(&registry);
    let listed = bridge
        .call("mcp.resources.list", json!({}), CancellationToken::new())
        .await
        .unwrap();
    let resources = listed["resources"].as_array().unwrap();
    assert_eq!(resources.len(), 2);
    let first = resources
        .iter()
        .find(|item| item["uri"] == URI)
        .expect("source resource listed");
    assert_eq!(first["description"], "Fixed application resource");
    assert_eq!(first["mime_type"], MIME);
    assert_eq!(first["source"]["source"]["id"], "source");
    assert_eq!(first["source"]["generation"], 1);
    let filtered = bridge
        .call(
            "mcp.resources.list",
            json!({"server":"second"}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(filtered["resources"].as_array().unwrap().len(), 1);
    assert_eq!(filtered["resources"][0]["uri"], "fixture://second/handbook");
    assert!(
        bridge
            .call(
                "mcp.resources.list",
                json!({"server":"missing"}),
                CancellationToken::new()
            )
            .await
            .is_err()
    );
    for args in [json!({"extra":1}), json!({"server":7})] {
        assert!(
            bridge
                .call("mcp.resources.list", args, CancellationToken::new())
                .await
                .is_err()
        );
    }

    policy
        .hidden
        .lock()
        .unwrap()
        .insert((Visibility::ResourceUri as u8, URI.into()));
    let hidden = bridge
        .call(
            "mcp.resources.list",
            json!({"server":"source"}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(hidden["resources"].as_array().unwrap().is_empty());
    assert!(
        !read(&bridge, "source", URI)
            .await
            .unwrap_err()
            .outcome_unknown
    );
    policy.hidden.lock().unwrap().clear();
    policy
        .hidden
        .lock()
        .unwrap()
        .insert((Visibility::ResourceContent as u8, URI.into()));
    assert_eq!(
        bridge
            .call(
                "mcp.resources.list",
                json!({"server":"source"}),
                CancellationToken::new()
            )
            .await
            .unwrap()["resources"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(
        !read(&bridge, "source", URI)
            .await
            .unwrap_err()
            .outcome_unknown
    );
    let observed = bridge.journal.records.lock().unwrap().len();
    assert_eq!(
        observed, 0,
        "denied reads must not prepare a dispatch record"
    );
}

#[tokio::test]
async fn resource_read_uses_source_operation_dispatch_record_and_original_policy() {
    let policy = Arc::new(support::Policy::default());
    let registry = make_registry(Limits::default(), policy.clone());
    let state = Arc::new(State::default());
    registry
        .register(
            identity("source", "1"),
            manifest(URI),
            handle("resource-dispatch", state.clone()),
        )
        .unwrap();
    let bridge = runtime(&registry);
    let scope = bridge
        .bridge
        .scoped(
            "mcp.resources.read",
            support::invocation(41),
            bridge.journal.clone(),
        )
        .unwrap();
    let result = scope
        .call(
            "mcp.resources.read",
            json!({"server":"source","uri":URI}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(result["value"]["uri"], URI);
    assert_eq!(result["value"]["mime_type"], MIME);
    assert_eq!(result["value"]["text"], BODY);
    assert_eq!(result["request"]["operation"], "read_resource");
    assert_eq!(result["request"]["target"], URI);
    assert_eq!(result["source"]["source"]["id"], "source");
    match state.observed.lock().unwrap().as_ref().unwrap() {
        SourceOperation::GetPrompt { .. } => panic!("unexpected"),
        SourceOperation::ReadResource { uri } => assert_eq!(uri, URI),
        other => panic!("unexpected source operation {other:?}"),
    }
    let calls = policy.calls.lock().unwrap();
    let request = calls
        .iter()
        .find(|request| request.operation == Operation::ReadResource)
        .expect("resource policy authorization");
    assert_eq!(request.target, URI);
    assert_eq!(request.source.source.id, "source");
    drop(calls);
    let rows = bridge.journal.records.lock().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["kind"], "started");
    assert_eq!(rows[0]["record"]["capability"], "mcp.resources.read");
    assert_eq!(rows[0]["record"]["context"]["sequence"], 41);
    assert_eq!(rows[0]["record"]["key"]["operation"], "read_resource");
    assert_eq!(rows[0]["record"]["key"]["target"], URI);
    assert_eq!(rows[0]["record"]["arguments"], json!({}));
    assert_eq!(rows[1]["kind"], "settled");
    assert_eq!(rows[1]["outcome"]["state"], "succeeded");
    assert_eq!(rows[1]["outcome"]["value"], result);
    drop(rows);
    assert_eq!(registry.snapshot("source").unwrap().operations, 1);
    assert!(!registry.snapshot("source").unwrap().outcome_unknown);
}

#[tokio::test]
async fn resource_read_rejects_unknown_uri_and_bad_arguments_before_dispatch() {
    let registry = make_registry(Limits::default(), Arc::new(support::Policy::default()));
    let state = Arc::new(State::default());
    registry
        .register(
            identity("source", "1"),
            manifest(URI),
            handle("resource-reject", state.clone()),
        )
        .unwrap();
    let bridge = runtime(&registry);
    let error = read(&bridge, "source", "fixture://source/missing")
        .await
        .unwrap_err();
    assert!(!error.outcome_unknown);
    for args in [
        json!({"uri":URI}),
        json!({"server":"source"}),
        json!({"server":"source","uri":URI,"extra":1}),
        json!({"server":"source","uri":7}),
        json!({"server":"bad/id","uri":URI}),
        json!({"server":"source","uri":"not a uri"}),
    ] {
        assert!(
            bridge
                .call("mcp.resources.read", args, CancellationToken::new())
                .await
                .is_err()
        );
    }
    assert_eq!(state.calls.load(Ordering::SeqCst), 0);
    assert!(bridge.journal.records.lock().unwrap().is_empty());
    assert!(!registry.snapshot("source").unwrap().outcome_unknown);
}

#[tokio::test]
async fn resource_read_validates_uri_mime_exactly_one_content_and_payload_bound() {
    let cases = [
        json!({"uri":"fixture://source/other","mime_type":MIME,"text":BODY}),
        json!({"uri":URI,"mime_type":"text/plain","text":BODY}),
        json!({"uri":URI,"mime_type":MIME,"text":BODY,"blob_base64":"QQ=="}),
        json!({"uri":URI,"mime_type":MIME}),
        json!({"uri":URI,"mime_type":MIME,"extra":"field","text":BODY}),
    ];
    for reply in cases {
        let registry = make_registry(Limits::default(), Arc::new(support::Policy::default()));
        let state = Arc::new(State::default());
        *state.reply.lock().unwrap() = Some(reply);
        registry
            .register(
                identity("source", "1"),
                manifest(URI),
                handle("resource-invalid-reply", state.clone()),
            )
            .unwrap();
        let bridge = runtime(&registry);
        let error = read(&bridge, "source", URI).await.unwrap_err();
        assert!(!error.outcome_unknown, "read-only rejection must be known");
        assert_eq!(state.calls.load(Ordering::SeqCst), 1);
        let rows = bridge.journal.records.lock().unwrap();
        assert_eq!(rows.last().unwrap()["outcome"]["state"], "failed");
        assert!(!registry.snapshot("source").unwrap().outcome_unknown);
        assert!(registry.snapshot("source").unwrap().unresolved.is_empty());
    }
    let registry = make_registry(
        Limits {
            max_resource_bytes: 4,
            ..Limits::default()
        },
        Arc::new(support::Policy::default()),
    );
    let state = Arc::new(State::default());
    registry
        .register(
            identity("source", "1"),
            manifest(URI),
            handle("resource-oversize", state.clone()),
        )
        .unwrap();
    let error = read(&runtime(&registry), "source", URI).await.unwrap_err();
    assert!(!error.outcome_unknown);
    assert_eq!(state.calls.load(Ordering::SeqCst), 1);

    let registry = make_registry(Limits::default(), Arc::new(support::Policy::default()));
    let state = Arc::new(State::default());
    *state.reply.lock().unwrap() = Some(json!({"uri":URI,"mime_type":MIME,"blob_base64":"QUJD"}));
    registry
        .register(
            identity("source", "1"),
            manifest(URI),
            handle("resource-blob", state),
        )
        .unwrap();
    let result = read(&runtime(&registry), "source", URI).await.unwrap();
    assert_eq!(result["value"]["blob_base64"], "QUJD");
    assert!(result["value"].get("text").is_none());
}

#[tokio::test]
async fn resource_read_cancellation_reconnect_and_late_results_are_fenced() {
    let registry = make_registry(Limits::default(), Arc::new(support::Policy::default()));
    let state = Arc::new(State::default());
    state.wait.store(true, Ordering::Release);
    registry
        .register(
            identity("source", "1"),
            manifest(URI),
            handle("resource-cancel", state.clone()),
        )
        .unwrap();
    let bridge = runtime(&registry);
    let cancel = CancellationToken::new();
    let call = bridge.call(
        "mcp.resources.read",
        json!({"server":"source","uri":URI}),
        cancel.clone(),
    );
    tokio::pin!(call);
    tokio::select! {
        result = &mut call => panic!("resource read completed without release: {result:?}"),
        _ = state.entered.notified() => {}
    }
    cancel.cancel();
    state.release.notify_one();
    let error = call.await.unwrap_err();
    assert!(
        !error.outcome_unknown,
        "a cancelled read has no side effect"
    );
    assert_eq!(state.calls.load(Ordering::SeqCst), 1);
    assert!(!registry.snapshot("source").unwrap().outcome_unknown);
    {
        let rows = bridge.journal.records.lock().unwrap();
        assert_eq!(rows.last().unwrap()["outcome"]["state"], "failed");
    }

    let registry = make_registry(Limits::default(), Arc::new(support::Policy::default()));
    let state = Arc::new(State::default());
    state.wait.store(true, Ordering::Release);
    registry
        .register(
            identity("source", "1"),
            manifest(URI),
            handle("resource-switch-old", state.clone()),
        )
        .unwrap();
    let old = runtime(&registry);
    let mut pending = Box::pin(old.call(
        "mcp.resources.read",
        json!({"server":"source","uri":URI}),
        CancellationToken::new(),
    ));
    tokio::select! {
        result = &mut pending => panic!("resource read completed before switch: {result:?}"),
        _ = state.entered.notified() => {}
    }
    registry.disconnect("source").unwrap();
    let replacement = Arc::new(State::default());
    registry
        .reconnect(
            identity("source", "2"),
            manifest(URI),
            handle("resource-switch-new", replacement.clone()),
        )
        .unwrap();
    state.release.notify_one();
    let error = pending.await.unwrap_err();
    assert!(!error.outcome_unknown);
    assert_eq!(state.calls.load(Ordering::SeqCst), 1);
    assert_eq!(replacement.calls.load(Ordering::SeqCst), 0);
    let snapshot = registry.snapshot("source").unwrap();
    assert_eq!(snapshot.provenance.generation, 3);
    assert!(!snapshot.outcome_unknown);
    assert!(snapshot.unresolved.is_empty());
    assert!(
        old.call("mcp.resources.list", json!({}), CancellationToken::new())
            .await
            .is_err(),
        "old consumer pins the previous generation"
    );
    let fresh = runtime(&registry);
    let result = read(&fresh, "source", URI).await.unwrap();
    assert_eq!(result["value"]["text"], BODY);
    assert_eq!(replacement.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn local_mcp_fixture_lists_and_reads_a_manifest_pinned_resource() {
    let workspace = support::Workspace::new();
    let stats = Arc::new(support::Stats::default());
    let client = support::LocalClient::spawn(
        std::path::Path::new(env!("CARGO_BIN_EXE_cayw-bridge-local-fixture")),
        &workspace.0,
        stats.clone(),
    )
    .await
    .unwrap();
    let manifest = client.manifest().await.unwrap();
    assert_eq!(manifest.resources[0].uri, "fixture://local/addition");
    assert_eq!(manifest.resources[0].mime_type, "text/markdown");
    let registry = make_registry(Limits::default(), Arc::new(support::Policy::default()));
    registry
        .register(
            support::identity("resource-fixture"),
            manifest,
            Box::new(client.handle()),
        )
        .unwrap();
    let bridge = runtime(&registry);
    let listed = bridge
        .call(
            "mcp.resources.list",
            json!({"server":"local"}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(listed["resources"][0]["uri"], "fixture://local/addition");
    let result = read(&bridge, "local", "fixture://local/addition")
        .await
        .unwrap();
    assert_eq!(result["value"]["text"], support::SKILL);
    assert_eq!(result["value"]["mime_type"], "text/markdown");
    assert_eq!(stats.resource_reads.load(Ordering::SeqCst), 1);
    registry.disconnect("local").unwrap();
    client.wait_closed().await;
}
