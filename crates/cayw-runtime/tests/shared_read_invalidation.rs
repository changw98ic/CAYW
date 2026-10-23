//! Real Runtime shared reads stay fresh across a cooperating write. Revoking the
//! exact grant stops the active task and a later read captures current bytes
//! instead of serving a value admitted under the revoked authority.
use cayw_ai::{
    AssistantEvent, AssistantStream, CancellationToken, Message, Model, ModelRequest, Provider,
};
use cayw_core::{CapabilityCallRecord, CapabilityOutcome, CoreEvent, SessionState};
use cayw_runtime::{ApprovalChoice, Runtime, RuntimeConfig, ScopedPermission, TaskState};
use futures::stream;
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

struct Scripts {
    scripts: Vec<(&'static str, String)>,
}
impl Provider for Scripts {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let prompt = last_user_text(&request);
        let script = self
            .scripts
            .iter()
            .find(|(needle, _)| prompt.contains(needle))
            .map(|(_, script)| script.as_str())
            .unwrap_or("return nil");
        let message = cayw_runtime::fixture::script_response(
            &request,
            script,
            &format!("shared-read-{}", request.messages.len()),
        );
        Box::pin(stream::iter(vec![AssistantEvent::Done { message }]))
    }
}
fn last_user_text(request: &ModelRequest) -> String {
    request
        .messages
        .iter()
        .rev()
        .find_map(|message| match message {
            Message::User { .. } => Some(message.text()),
            _ => None,
        })
        .unwrap_or_default()
}
fn runtime(
    temp: &tempfile::TempDir,
    scripts: Vec<(&'static str, String)>,
    permissions: Vec<ScopedPermission>,
) -> Runtime {
    let root = temp.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    Runtime::open(
        temp.path().join("session.db"),
        root,
        Arc::new(Scripts { scripts }),
        Model::new("fixture", "shared-read-invalidation"),
        RuntimeConfig {
            max_run_time: Duration::from_secs(10),
            permissions,
            host: cayw_host::HostConfig {
                frozen_reads: true,
                ..Default::default()
            },
            ..Default::default()
        },
    )
    .unwrap()
}
async fn pending_matching(runtime: &Runtime, id: &str, path: &str) -> cayw_core::ApprovalRequest {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(pending) = runtime
                .get(id)
                .unwrap()
                .pending_approvals
                .into_iter()
                .find(|pending| pending.input.scope.contains(path))
            {
                return pending;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap()
}
fn events(runtime: &Runtime, id: &str) -> Vec<cayw_core::EventRecord> {
    runtime.events(id, 0, 1000).unwrap()
}
fn settled_reads(runtime: &Runtime, id: &str) -> Vec<CapabilityCallRecord> {
    events(runtime, id)
        .into_iter()
        .filter_map(|event| match event.event {
            CoreEvent::CapabilitySettled { call }
                if call.name == "fs.read"
                    && matches!(
                        call.outcome.as_ref(),
                        Some(CapabilityOutcome::Completed { .. })
                    ) =>
            {
                Some(call)
            }
            _ => None,
        })
        .collect()
}
fn read_content(call: &CapabilityCallRecord) -> &str {
    match call.outcome.as_ref() {
        Some(CapabilityOutcome::Completed { result }) => {
            result["content"].as_str().expect("fs.read content")
        }
        outcome => panic!("expected completed fs.read, got {outcome:?}"),
    }
}
fn shared_reads(runtime: &Runtime, id: &str) -> Vec<Value> {
    events(runtime, id)
        .into_iter()
        .filter_map(|event| match event.event {
            CoreEvent::Host { kind, data } if kind == "runtime_shared_read" => Some(data),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn real_runtime_read_write_read_captures_the_new_bytes() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("a.txt"), "old bytes").unwrap();
    let expected_sha256 = cayw_bridge::content_sha256("old bytes");
    let script = format!(
        "local first=cayw.call('fs.read',{{path='a.txt'}}); cayw.call('fs.write',{{path='a.txt',content='new bytes',overwrite=true,expected_sha256='{expected_sha256}'}}); return cayw.call('fs.read',{{path='a.txt'}})"
    );
    let rt = Runtime::open(
        temp.path().join("session.db"),
        &root,
        Arc::new(Scripts {
            scripts: vec![("read write read", script)],
        }),
        Model::new("fixture", "shared-read-invalidation"),
        RuntimeConfig {
            max_run_time: Duration::from_secs(10),
            permissions: vec![
                ScopedPermission::exact("fs.read", json!({"path":"a.txt"})),
                ScopedPermission::exact(
                    "fs.write",
                    json!({
                        "path":"a.txt",
                        "content":"new bytes",
                        "overwrite":true,
                        "expected_sha256":expected_sha256,
                    }),
                ),
            ],
            host: cayw_host::HostConfig {
                frozen_reads: true,
                ..Default::default()
            },
            ..Default::default()
        },
    )
    .unwrap();
    let id = rt.create(None).unwrap().id;
    let result = rt
        .prompt(&id, "read write read", CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(result.state, SessionState::Idle);

    let reads = settled_reads(&rt, &id);
    assert_eq!(reads.len(), 2, "{reads:?}");
    assert_eq!(read_content(&reads[0]), "old bytes");
    assert_eq!(read_content(&reads[1]), "new bytes");
    let shared = shared_reads(&rt, &id);
    assert_eq!(shared.len(), 2, "{shared:?}");
    assert_eq!(shared[0]["role"], "leader");
    assert_eq!(shared[1]["role"], "leader");
    let before = shared[0]["stamp"]["generation"].as_u64().unwrap();
    let after = shared[1]["stamp"]["generation"].as_u64().unwrap();
    assert_eq!(after, before + 1, "{shared:?}");
    assert_ne!(shared[0]["result_sha256"], shared[1]["result_sha256"]);
    assert_eq!(
        std::fs::read_to_string(root.join("a.txt")).unwrap(),
        "new bytes"
    );
    assert!(
        result
            .messages
            .iter()
            .any(|message| message.text().contains("new bytes"))
    );
}

#[tokio::test]
async fn runtime_revoke_cancels_the_task_and_the_next_read_is_not_stale() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("a.txt"), "before revoke").unwrap();
    std::fs::write(root.join("b.txt"), "blocks the original task").unwrap();
    let rt = runtime(
        &temp,
        vec![
            (
                "seed and block",
                "cayw.call('fs.read',{path='a.txt'}); return cayw.call('fs.read',{path='b.txt'})"
                    .into(),
            ),
            (
                "read after revoke",
                "return cayw.call('fs.read',{path='a.txt'})".into(),
            ),
        ],
        Vec::new(),
    );
    let id = rt.create(None).unwrap().id;
    let worker_rt = rt.clone();
    let worker_id = id.clone();
    let worker = tokio::spawn(async move {
        worker_rt
            .prompt(&worker_id, "seed and block", CancellationToken::new())
            .await
    });
    let first = pending_matching(&rt, &id, "a.txt").await;
    assert_eq!(first.input.capability, "fs.read");
    rt.approve(&id, &first.id, ApprovalChoice::AllowTask, "fixture-user")
        .unwrap();
    let blocked = pending_matching(&rt, &id, "b.txt").await;
    assert_eq!(blocked.input.capability, "fs.read");
    let grant = rt
        .grants(&id)
        .unwrap()
        .into_iter()
        .find(|grant| grant.approval_id == first.id && grant.state == "active")
        .expect("the first read has an active task grant");

    rt.revoke(&id, &grant.id, "fixture-user").unwrap();
    let _ = worker.await.unwrap();
    rt.wait_idle(&id).await;
    assert_eq!(rt.get(&id).unwrap().state, SessionState::Idle);
    assert_eq!(rt.health().unwrap().running, 0);
    assert_eq!(
        rt.tasks(&id).unwrap().last().unwrap().state,
        TaskState::Cancelled
    );
    assert!(
        rt.audit(&id, 0, 1000)
            .unwrap()
            .iter()
            .any(|record| record.kind == "grant_revoked")
    );
    assert_eq!(
        rt.grants(&id)
            .unwrap()
            .into_iter()
            .find(|candidate| candidate.id == grant.id)
            .unwrap()
            .state,
        "revoked"
    );
    assert!(rt.get(&id).unwrap().pending_approvals.is_empty());

    // A stale cache would still return the bytes admitted before revocation.
    std::fs::write(root.join("a.txt"), "after revoke").unwrap();
    let reader_rt = rt.clone();
    let reader_id = id.clone();
    let reader = tokio::spawn(async move {
        reader_rt
            .prompt(&reader_id, "read after revoke", CancellationToken::new())
            .await
    });
    let approval = pending_matching(&rt, &id, "a.txt").await;
    rt.approve(&id, &approval.id, ApprovalChoice::AllowOnce, "fixture-user")
        .unwrap();
    let result = reader.await.unwrap().unwrap();
    assert_eq!(result.state, SessionState::Idle);

    let reads = settled_reads(&rt, &id);
    assert_eq!(reads.len(), 2, "{reads:?}");
    assert_eq!(read_content(&reads[0]), "before revoke");
    assert_eq!(read_content(&reads[1]), "after revoke");
    let shared = shared_reads(&rt, &id);
    assert_eq!(shared.len(), 2, "{shared:?}");
    assert_eq!(shared[0]["role"], "leader");
    assert_eq!(shared[1]["role"], "leader");
    assert_ne!(shared[0]["result_sha256"], shared[1]["result_sha256"]);
    assert!(
        result
            .messages
            .iter()
            .any(|message| message.text().contains("after revoke"))
    );
}
