//! Real Runtime -> Luau -> Host -> Bridge dispatch for resource capabilities.
use async_trait::async_trait;
use cayw_ai::{AssistantEvent, AssistantStream, CancellationToken, Model, ModelRequest, Provider};
use cayw_bridge::{
    AccessRequest, DispatchOutcome, Limits, Manifest, Operation, Provenance, Registry,
    RegistryPolicy, ResourceSpec, SourceHandle, SourceIdentity, SourceOperation, SourceReply,
    SourceRequest, Visibility,
};
use cayw_codemode::CapabilityError;
use cayw_core::{
    CapabilityCallRecord, CapabilityOutcome, CapabilityStage, CoreEvent, SessionState,
};
use cayw_runtime::{ApprovalChoice, BridgeDispatchEvent, Runtime, RuntimeConfig, ScopedPermission};
use serde_json::json;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

const URI: &str = "fixture://local/manual";
const MIME: &str = "text/markdown";
const BODY: &str = "# Local manual\nFixture resource body.\n";

struct Policy;
#[async_trait]
impl RegistryPolicy for Policy {
    fn visible(&self, _: Visibility, _: &Provenance, _: &str) -> bool {
        true
    }
    async fn authorize(
        &self,
        _: &AccessRequest,
        _: CancellationToken,
    ) -> Result<(), CapabilityError> {
        Ok(())
    }
}

#[derive(Default)]
struct State {
    requests: Mutex<Vec<SourceRequest>>,
    disconnected: AtomicUsize,
}
struct Source {
    state: Arc<State>,
    owner: &'static str,
}
#[async_trait]
impl SourceHandle for Source {
    fn ownership_key(&self) -> &str {
        self.owner
    }
    async fn request(
        &self,
        request: SourceRequest,
        _: CancellationToken,
    ) -> Result<SourceReply, CapabilityError> {
        self.state.requests.lock().unwrap().push(request.clone());
        let value = match &request.operation {
            SourceOperation::ReadResource { uri } => {
                assert_eq!(uri, URI);
                json!({"uri":uri,"mime_type":MIME,"text":BODY})
            }
            other => panic!("unexpected source operation: {other:?}"),
        };
        Ok(SourceReply {
            key: request.key,
            value,
        })
    }
    fn disconnect(&self) {
        self.state.disconnected.fetch_add(1, Ordering::AcqRel);
    }
}

fn manifest() -> Manifest {
    Manifest {
        tools: Vec::new(),
        skills: Vec::new(),
        resources: vec![ResourceSpec {
            uri: URI.into(),
            description: "Fixture manual".into(),
            mime_type: MIME.into(),
        }],
        prompts: Vec::new(),
        search: false,
    }
}
fn registry(state: Arc<State>) -> Registry {
    let registry = Registry::new(Limits::default(), Arc::new(Policy)).unwrap();
    registry
        .register(
            SourceIdentity {
                id: "local".into(),
                version: "1.0".into(),
                origin: "fixture resource source".into(),
            },
            manifest(),
            Box::new(Source {
                state,
                owner: "physical-resource-handle",
            }),
        )
        .unwrap();
    registry
}
fn config(registry: &Registry, allow_read: bool) -> RuntimeConfig {
    let mut permissions = vec![ScopedPermission::exact("mcp.resources.list", json!({}))];
    if allow_read {
        permissions.push(ScopedPermission::exact(
            "mcp.resources.read",
            json!({"server":"local","uri":URI}),
        ));
    }
    RuntimeConfig {
        bridge: Some(registry.consumer()),
        max_run_time: Duration::from_secs(15),
        permissions,
        ..Default::default()
    }
}
struct Script(String);
impl Provider for Script {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let message = cayw_runtime::fixture::script_response(&request, &self.0, "resources-1");
        Box::pin(futures::stream::iter([AssistantEvent::Done { message }]))
    }
}
fn setup() -> tempfile::TempDir {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("workspace")).unwrap();
    temp
}
fn open(temp: &tempfile::TempDir, settings: RuntimeConfig, script: &str) -> Runtime {
    Runtime::open(
        temp.path().join("session.db"),
        temp.path().join("workspace"),
        Arc::new(Script(script.into())),
        Model::new("fixture", "runtime-resources"),
        settings,
    )
    .unwrap()
}
fn calls(rt: &Runtime, id: &str) -> Vec<CapabilityCallRecord> {
    rt.events(id, 0, 1000)
        .unwrap()
        .into_iter()
        .filter_map(|event| match event.event {
            CoreEvent::CapabilitySettled { call }
            | CoreEvent::CapabilityReconciled { call, .. } => Some(call),
            _ => None,
        })
        .collect()
}
fn dispatches(rt: &Runtime, id: &str) -> Vec<BridgeDispatchEvent> {
    let mut cursor = 0;
    let mut output = Vec::new();
    loop {
        let page = rt.bridge_dispatches(id, cursor, 4).unwrap();
        if page.next_cursor == cursor {
            break;
        }
        cursor = page.next_cursor;
        output.extend(page.events);
    }
    output
}
async fn pending(rt: &Runtime, id: &str) -> cayw_core::ApprovalRequest {
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            if let Some(approval) = rt.get(id).unwrap().pending_approvals.first() {
                return approval.clone();
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn runtime_lists_and_reads_the_pinned_resource_through_its_original_dispatch() {
    let temp = setup();
    let state = Arc::new(State::default());
    let registry = registry(state.clone());
    let settings = config(&registry, true);
    let pins = settings.bridge.as_ref().unwrap().binding_snapshot();
    let script = format!(
        "local l=cayw.call('mcp.resources.list',{{}})\nlocal r=cayw.call('mcp.resources.read',{{server='local',uri='{URI}'}})\nreturn {{list=l,read=r}}"
    );
    let rt = open(&temp, settings.clone(), &script);
    let names = rt
        .capabilities()
        .unwrap()
        .into_iter()
        .map(|definition| definition.name)
        .collect::<Vec<_>>();
    assert!(names.iter().any(|name| name == "mcp.resources.list"));
    assert!(names.iter().any(|name| name == "mcp.resources.read"));

    let id = rt.create(None).unwrap().id;
    assert_eq!(
        rt.prompt(&id, "read the fixed resource", CancellationToken::new())
            .await
            .unwrap()
            .state,
        SessionState::Idle
    );

    // Listing is metadata-only. Only the read reaches the injected source.
    {
        let requests = state.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(matches!(
            &requests[0].operation,
            SourceOperation::ReadResource { uri } if uri == URI
        ));
    }
    let calls = calls(&rt, &id);
    assert_eq!(calls.len(), 2);
    assert_eq!(
        calls
            .iter()
            .filter(|call| matches!(call.outcome, Some(CapabilityOutcome::Completed { .. })))
            .count(),
        2
    );
    assert!(
        calls
            .iter()
            .all(|call| call.external_provenance == Some(serde_json::to_value(&pins).unwrap()))
    );

    let records = dispatches(&rt, &id);
    assert_eq!(
        records.len(),
        2,
        "one journaled read has exactly a prepare and a terminal transition: {records:#?}"
    );
    assert!(
        records
            .iter()
            .all(|event| event.record.capability == "mcp.resources.read"),
        "catalog listing must not create a source dispatch record: {records:#?}"
    );
    assert!(records.iter().any(|event| event.outcome.is_none()));
    let record = records
        .iter()
        .find(|event| event.outcome.is_some())
        .expect("durable resource terminal");
    assert_eq!(record.record.capability, "mcp.resources.read");
    assert_eq!(record.record.key.operation, Operation::ReadResource);
    assert_eq!(record.record.key.target, URI);
    assert_eq!(record.record.arguments, json!({}));
    assert_eq!(record.record.key.source, pins.sources["local"]);
    assert_eq!(record.origin.bindings, pins);
    let call = calls
        .iter()
        .find(|call| call.name == "mcp.resources.read")
        .unwrap();
    assert_eq!(record.origin.key, call.key);
    assert_eq!(record.origin.lease, call.lease);
    assert_eq!(record.origin.task_id, call.task_id);
    assert_eq!(record.origin.parent_call_id, call.parent_call_id);
    assert_eq!(record.origin.source_hash, call.source_hash);
    assert_eq!(record.record.context.sequence, call.key.sequence + 1);
    match &record.outcome {
        Some(DispatchOutcome::Succeeded { value }) => {
            assert_eq!(value["request"]["operation"], "read_resource");
            assert_eq!(value["request"]["target"], URI);
            assert_eq!(value["source"]["source"]["id"], "local");
            assert_eq!(value["value"]["uri"], URI);
            assert_eq!(value["value"]["mime_type"], MIME);
            assert_eq!(value["value"]["text"], BODY);
        }
        other => panic!("resource read has no durable success: {other:?}"),
    }
    assert_eq!(rt.recover(&id).unwrap().state, SessionState::Idle);
    assert_eq!(state.disconnected.load(Ordering::Acquire), 0);

    drop(rt);
    let reopened = open(&temp, settings, "return 1");
    assert_eq!(dispatches(&reopened, &id).len(), 2);
    assert_eq!(state.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn resource_dispatch_record_must_bind_the_original_server_and_uri() {
    let temp = setup();
    let state = Arc::new(State::default());
    let registry = registry(state.clone());
    let settings = config(&registry, true);
    let script = format!("return cayw.call('mcp.resources.read',{{server='local',uri='{URI}'}})");
    let rt = open(&temp, settings, script.as_str());
    let id = rt.create(None).unwrap().id;
    assert_eq!(
        rt.prompt(&id, "read the fixed resource", CancellationToken::new())
            .await
            .unwrap()
            .state,
        SessionState::Idle
    );
    let db = rusqlite::Connection::open(temp.path().join("session.db")).unwrap();
    // Deliberately bypass the append-only guard for this corruption fixture.
    // The original Core event remains the only source of dispatch truth.
    db.execute_batch("DROP TRIGGER core_events_no_update;")
        .unwrap();
    let changed = db
        .execute(
            "UPDATE core_events SET record=json_set(record,'$.event.data.record.key.target','fixture://local/other') WHERE json_extract(record,'$.event.kind')='runtime_bridge_prepared'",
            [],
        )
        .unwrap();
    assert_eq!(changed, 1);
    assert!(
        rt.bridge_dispatches(&id, 0, 1000).is_err(),
        "a read record whose target is not the original URI must be rejected"
    );
    assert!(rt.recover(&id).is_err());
    assert_eq!(state.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn denied_resource_read_reaches_no_source_and_records_no_dispatch() {
    let temp = setup();
    let state = Arc::new(State::default());
    let registry = registry(state.clone());
    let settings = config(&registry, false);
    let script = format!("return cayw.call('mcp.resources.read',{{server='local',uri='{URI}'}})");
    let rt = open(&temp, settings, &script);
    let id = rt.create(None).unwrap().id;
    let worker_rt = rt.clone();
    let worker_id = id.clone();
    let worker = tokio::spawn(async move {
        worker_rt
            .prompt(
                &worker_id,
                "read without permission",
                CancellationToken::new(),
            )
            .await
    });
    let approval = pending(&rt, &id).await;
    rt.approve(&id, &approval.id, ApprovalChoice::Deny, "fixture-host")
        .unwrap();
    assert_eq!(worker.await.unwrap().unwrap().state, SessionState::Idle);
    assert!(state.requests.lock().unwrap().is_empty());
    assert!(dispatches(&rt, &id).is_empty());
    let call = calls(&rt, &id)
        .into_iter()
        .find(|call| call.name == "mcp.resources.read")
        .unwrap();
    assert_eq!(call.stage, CapabilityStage::Settled);
    assert!(matches!(
        call.outcome,
        Some(CapabilityOutcome::NotExecuted { .. })
    ));
}
