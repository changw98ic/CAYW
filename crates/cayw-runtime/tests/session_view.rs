use cayw_ai::{
    AssistantEvent, AssistantMessage, AssistantStream, CancellationToken, Content, Model,
    ModelRequest, Provider,
};
use cayw_ipc::RpcHandler;
use cayw_runtime::{Runtime, RuntimeConfig};
use serde_json::{Value, json};
use std::sync::Arc;

struct Chat;
impl Provider for Chat {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let mut message = AssistantMessage::new(&request.model);
        message.content.push(Content::text("ordinary chat"));
        Box::pin(futures::stream::iter([AssistantEvent::Done { message }]))
    }
}

fn ordinary(view: &Value) {
    assert!(view["id"].is_string());
    assert!(view["label"].is_string());
    assert_eq!(view["state"], "idle");
    assert_eq!(view["frozen"], false);
    assert!(view.as_object().unwrap().contains_key("binding"));
    assert_eq!(view["binding"], Value::Null);
}

#[tokio::test]
async fn sdk_and_rpc_session_results_share_the_ordinary_view() {
    let dir = tempfile::tempdir().unwrap();
    let rt = Runtime::open(
        dir.path().join("sessions.db"),
        dir.path(),
        Arc::new(Chat),
        Model::new("fixture", "session-view"),
        RuntimeConfig::default(),
    )
    .unwrap();
    let created = rt.create(Some("ordinary".into())).unwrap();
    ordinary(&serde_json::to_value(&created).unwrap());
    let id = created.id;
    let completed = rt
        .prompt(&id, "hello", CancellationToken::new())
        .await
        .unwrap();
    ordinary(&serde_json::to_value(completed).unwrap());
    ordinary(&serde_json::to_value(rt.recover(&id).unwrap()).unwrap());
    ordinary(&serde_json::to_value(rt.fork(&id, "fork", true).unwrap()).unwrap());
    for (method, params) in [
        ("session.get", json!({"session_id":id})),
        ("session.create", json!({"label":"rpc"})),
        ("session.recover", json!({"session_id":id})),
        (
            "session.prompt",
            json!({"session_id":id,"text":"hello over RPC"}),
        ),
        (
            "session.fork",
            json!({"session_id":id,"label":"rpc-fork","confirm":true,"mode":"chat"}),
        ),
    ] {
        ordinary(
            &rt.call(method, params, CancellationToken::new())
                .await
                .unwrap(),
        );
    }
    let sdk = serde_json::to_value(rt.get(&id).unwrap()).unwrap();
    assert_eq!(
        rt.call(
            "session.get",
            json!({"session_id":id}),
            CancellationToken::new()
        )
        .await
        .unwrap(),
        sdk
    );
    let list = rt
        .call("session.list", json!({}), CancellationToken::new())
        .await
        .unwrap();
    for session in list.as_array().unwrap() {
        ordinary(session);
    }
}

#[test]
fn obsolete_side_bindings_are_preserved_for_explicit_host_reconciliation() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("sessions.db");
    let rt = Runtime::open(
        &db_path,
        dir.path(),
        Arc::new(Chat),
        Model::new("fixture", "obsolete"),
        RuntimeConfig::default(),
    )
    .unwrap();
    let id = rt.create(None).unwrap().id;
    drop(rt);
    let db = rusqlite::Connection::open(&db_path).unwrap();
    db.execute_batch("CREATE TABLE runtime_bindings (session TEXT PRIMARY KEY, module TEXT NOT NULL, generation INTEGER NOT NULL, owner TEXT NOT NULL, frozen INTEGER NOT NULL DEFAULT 0);").unwrap();
    db.execute(
        "INSERT INTO runtime_bindings VALUES (?1,'strongflow',1,'old-owner',0)",
        [&id],
    )
    .unwrap();
    let reopened = Runtime::open(
        &db_path,
        dir.path(),
        Arc::new(Chat),
        Model::new("fixture", "obsolete"),
        RuntimeConfig::default(),
    );
    assert!(
        matches!(reopened, Err(cayw_runtime::RuntimeError::InvalidInput(reason)) if reason.contains("explicit host reconciliation"))
    );
    let unchanged: (String, bool) = db
        .query_row(
            "SELECT module,frozen FROM runtime_bindings WHERE session=?1",
            [&id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(unchanged, ("strongflow".into(), false));
}

#[cfg(feature = "strongflow")]
#[tokio::test]
async fn binding_checkpoint_and_unload_are_visible_in_the_canonical_event_cursor() {
    use cayw_core::CoreEvent;
    use cayw_modules::{
        ModuleContext,
        strongflow::{PlanStep, StrongFlow, WorkflowPlan},
    };
    use std::collections::BTreeSet;
    let dir = tempfile::tempdir().unwrap();
    let open = || {
        Runtime::open(
            dir.path().join("sessions.db"),
            dir.path(),
            Arc::new(Chat),
            Model::new("fixture", "module-view"),
            RuntimeConfig::default(),
        )
        .unwrap()
    };
    let rt = open();
    let flow = Arc::new(StrongFlow::new());
    flow.create_plan(WorkflowPlan {
        id: "obligations".into(),
        steps: vec![PlanStep {
            id: "real-check".into(),
            depends_on: BTreeSet::new(),
            acceptance_checks: BTreeSet::from(["actual-tests".into()]),
        }],
    })
    .unwrap();
    let description = rt.load_native(flow, ModuleContext::new([], [])).unwrap();
    let created = rt.create(None).unwrap();
    rt.bind_strongflow(&created.id).unwrap();
    let bound = serde_json::to_value(rt.get(&created.id).unwrap()).unwrap();
    assert!(bound["cursor"].as_u64().unwrap() > created.cursor);
    assert_eq!(bound["frozen"], false);
    assert_eq!(bound["binding"]["registry_id"], description.registry_id);
    assert_eq!(bound["binding"]["generation"], description.generation);
    assert_eq!(
        bound["binding"]["workflow_state"]["workflows"]["obligations"]["plan"]["steps"][0]["acceptance_checks"],
        json!(["actual-tests"])
    );
    let before = bound["cursor"].as_u64().unwrap();
    rt.unload_module("strongflow").unwrap();
    let changes = rt
        .wait_events(
            &created.id,
            before,
            100,
            std::time::Duration::ZERO,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(changes.iter().any(|record| matches!(&record.event, CoreEvent::Host {kind, data} if kind == "module_frozen" && data["actor"].is_string() && data["decision"]["binding"].is_object())));
    let frozen = serde_json::to_value(rt.get(&created.id).unwrap()).unwrap();
    assert!(frozen["cursor"].as_u64().unwrap() > before);
    assert_eq!(frozen["frozen"], true);
    assert!(
        frozen["binding"]["reason"]
            .as_str()
            .is_some_and(|reason| !reason.is_empty())
    );
    let fork = rt.fork(&created.id, "confirmed chat", true).unwrap();
    ordinary(&serde_json::to_value(&fork).unwrap());
    assert_eq!(
        serde_json::to_value(rt.get(&created.id).unwrap()).unwrap()["frozen"],
        true
    );
    drop(rt);
    let reopened = open();
    assert_eq!(
        serde_json::to_value(reopened.get(&created.id).unwrap()).unwrap(),
        frozen
    );
    ordinary(
        &serde_json::to_value(
            reopened
                .prompt(&fork.id, "ordinary", CancellationToken::new())
                .await
                .unwrap(),
        )
        .unwrap(),
    );
}
