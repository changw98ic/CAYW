//! Device-authoritative text, fixed to the original Session/Task and Host call.
use cayw_ai::{AssistantEvent, AssistantStream, CancellationToken, Model, ModelRequest, Provider};
use cayw_content::Limits;
use cayw_ipc::{Client, ServerConfig, serve};
use cayw_runtime::{
    ContentConfig, ContentScope, Runtime, RuntimeConfig, ScopedPermission, fixture::script_response,
};
use serde_json::{Value, json};
use std::{path::Path, sync::Arc};

struct Script(String);
impl Provider for Script {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        Box::pin(futures::stream::iter([AssistantEvent::Done {
            message: script_response(
                &request,
                &self.0,
                &format!("content-{}", request.messages.len()),
            ),
        }]))
    }
}
fn config(root: &Path, permissions: Vec<ScopedPermission>) -> RuntimeConfig {
    RuntimeConfig {
        content: Some(ContentConfig {
            directory: root.join("content"),
            device_id: "local-device".into(),
            limits: Limits {
                max_content_bytes: 16384,
                ..Default::default()
            },
        }),
        permissions,
        ..Default::default()
    }
}
fn open(root: &Path, script: &str, config: RuntimeConfig) -> Runtime {
    Runtime::open(
        root.join("sessions.sqlite"),
        root.join("workspace"),
        Arc::new(Script(script.into())),
        Model::new("fixture", "content"),
        config,
    )
    .unwrap()
}
fn put_args(name: &str, text: &str) -> Value {
    json!({"name":name,"text":text,"expected_version":0})
}
fn completed_put(rt: &Runtime, session: &str) -> Value {
    rt.events(session, 0, 1000)
        .unwrap()
        .into_iter()
        .find_map(|record| {
            let value = serde_json::to_value(record).unwrap();
            let call = &value["event"]["call"];
            (value["event"]["type"] == "capability_settled"
                && call["name"] == "content.put"
                && call["outcome"]["type"] == "completed")
                .then(|| call["outcome"]["result"].clone())
        })
        .expect("real content.put has a paired completed receipt")
}

#[tokio::test]
async fn local_content_survives_reopen_and_updates_compare_full_revision() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("workspace")).unwrap();
    let args = put_args("notes", "first");
    let cfg = config(&root, vec![ScopedPermission::exact("content.put", args)]);
    let rt = open(
        &root,
        "return cayw.call('content.put', {name='notes',text='first',expected_version=0})",
        cfg.clone(),
    );
    let session = rt.create(None).unwrap().id;
    rt.prompt(&session, "create", CancellationToken::new())
        .await
        .unwrap();
    let first = rt
        .content_inspect(&session, None, "notes")
        .unwrap()
        .unwrap();
    assert_eq!(first.text, "first");
    assert_eq!(first.record.version, 1);
    assert_eq!(first.session_id, session);
    assert!(first.task_id.is_none());
    assert_eq!(first.scope, ContentScope::Session);
    assert_eq!(completed_put(&rt, &session)["record"]["version"], 1);
    drop(rt);
    let update = json!({"name":"notes","text":"second","expected_version":1,"expected_revision":first.revision_hex});
    let mut updated = cfg;
    updated
        .permissions
        .push(ScopedPermission::exact("content.put", update.clone()));
    let script = format!(
        "return cayw.call('content.put', {{name='notes',text='second',expected_version=1,expected_revision='{}'}})",
        first.revision_hex
    );
    let rt = open(&root, &script, updated.clone());
    assert_eq!(
        rt.content_inspect(&session, None, "notes")
            .unwrap()
            .unwrap(),
        first
    );
    rt.prompt(&session, "update", CancellationToken::new())
        .await
        .unwrap();
    let second = rt
        .content_inspect(&session, None, "notes")
        .unwrap()
        .unwrap();
    assert_eq!(second.text, "second");
    assert_eq!(second.record.version, 2);
    assert_eq!(second.record.parent_revision, Some(first.record.revision));
    assert_ne!(second.record.revision, first.record.revision);
    // A second identical request carries the stale observation and must not overwrite.
    rt.prompt(&session, "stale update", CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        rt.content_inspect(&session, None, "notes")
            .unwrap()
            .unwrap(),
        second
    );
    let other = rt.create(None).unwrap().id;
    assert!(rt.content_inspect(&other, None, "notes").unwrap().is_none());
    assert!(rt.content_inspect(&session, None, "../notes").is_err());
}

#[tokio::test]
async fn authenticated_inspection_checks_task_ownership_and_model_cannot_supply_it() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("workspace")).unwrap();
    let args = json!({"name":"private","scope":"task","text":"owned","expected_version":0});
    let cfg = config(&root, vec![ScopedPermission::exact("content.put", args)]);
    let rt = open(
        &root,
        "return cayw.call('content.put', {name='private',scope='task',text='owned',expected_version=0})",
        cfg,
    );
    let a = rt.create(None).unwrap().id;
    let b = rt.create(None).unwrap().id;
    rt.prompt(&a, "task content", CancellationToken::new())
        .await
        .unwrap();
    let task = rt.tasks(&a).unwrap().pop().unwrap();
    let original = rt
        .content_inspect(&a, Some(&task.id), "private")
        .unwrap()
        .unwrap();
    assert_eq!(original.task_id.as_deref(), Some(task.id.as_str()));
    assert!(rt.content_inspect(&a, None, "private").unwrap().is_none());
    assert!(rt.content_inspect(&b, Some(&task.id), "private").is_err());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let stop = CancellationToken::new();
    let server = tokio::spawn(serve(
        listener,
        Arc::new(rt.clone()),
        ServerConfig::new("content-test-token"),
        stop.clone(),
    ));
    assert_eq!(
        Client::connect(address, "wrong").await.unwrap_err().code,
        "unauthorized"
    );
    let client = Client::connect(address, "content-test-token")
        .await
        .unwrap();
    let read = client
        .request(
            "content.inspect",
            json!({"session_id":a,"task_id":task.id,"name":"private"}),
        )
        .await
        .unwrap();
    assert_eq!(read, serde_json::to_value(original).unwrap());
    assert_eq!(
        client
            .request(
                "content.inspect",
                json!({"session_id":b,"task_id":task.id,"name":"private"})
            )
            .await
            .unwrap_err()
            .code,
        "invalid_params"
    );
    client.close().await;
    stop.cancel();
    server.await.unwrap().unwrap();
}

#[tokio::test]
async fn denied_put_has_no_local_record_and_no_started_call() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("workspace")).unwrap();
    let rt = open(
        &root,
        "return cayw.call('content.put', {name='denied',text='forbidden',expected_version=0})",
        config(&root, vec![]),
    );
    let session = rt.create(None).unwrap().id;
    let runtime = rt.clone();
    let target = session.clone();
    let worker = tokio::spawn(async move {
        runtime
            .prompt(&target, "needs approval", CancellationToken::new())
            .await
    });
    let pending = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            if let Some(pending) = rt.get(&session).unwrap().pending_approvals.first() {
                break pending.clone();
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        rt.content_inspect(&session, None, "denied")
            .unwrap()
            .is_none()
    );
    rt.approve(
        &session,
        &pending.id,
        cayw_runtime::ApprovalChoice::Deny,
        "content-test",
    )
    .unwrap();
    worker.await.unwrap().unwrap();
    assert!(
        rt.content_inspect(&session, None, "denied")
            .unwrap()
            .is_none()
    );
    let events = serde_json::to_value(rt.events(&session, 0, 1000).unwrap()).unwrap();
    assert!(
        !events
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["event"]["type"] == "capability_started"
                && e["event"]["call"]["name"] == "content.put")
    );
}
