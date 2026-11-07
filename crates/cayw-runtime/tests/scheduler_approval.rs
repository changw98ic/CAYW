use cayw_ai::{AssistantEvent, AssistantStream, CancellationToken, Model, ModelRequest, Provider};
use cayw_runtime::{ApprovalChoice, Runtime, RuntimeConfig, ScopedPermission};
use serde_json::json;
use std::{sync::Arc, time::Duration};

struct Files;
impl Provider for Files {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let text = request
            .messages
            .iter()
            .rev()
            .find_map(|message| match message {
                cayw_ai::Message::User { .. } => Some(message.text()),
                _ => None,
            })
            .unwrap();
        let path = if text == "approval" {
            "approval.txt"
        } else {
            "ready.txt"
        };
        let script = format!("return cayw.call('fs.write',{{path='{path}',content='{text}'}})");
        let message = cayw_runtime::fixture::script_response(
            &request,
            &script,
            &format!("file-{}", request.messages.len()),
        );
        Box::pin(futures::stream::iter([AssistantEvent::Done { message }]))
    }
}

async fn pending(rt: &Runtime, id: &str) -> cayw_core::ApprovalRequest {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(approval) = rt.get(id).unwrap().pending_approvals.first() {
                return approval.clone();
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("approval must become visible")
}

#[tokio::test]
async fn waiting_approval_yields_the_only_slot_to_an_authorized_file_task() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let rt = Runtime::open(
        dir.path().join("sessions.sqlite"),
        &workspace,
        Arc::new(Files),
        Model::new("fixture", "scheduler-files"),
        RuntimeConfig {
            max_concurrent_runs: 1,
            max_queued_runs: 3,
            max_queue_time: Duration::from_secs(10),
            max_run_time: Duration::from_secs(20),
            permissions: vec![ScopedPermission::exact(
                "fs.write",
                json!({
                    "path":"ready.txt", "content":"ready"
                }),
            )],
            ..Default::default()
        },
    )
    .unwrap();
    let waiting_session = rt.create(None).unwrap().id;
    let ready_session = rt.create(None).unwrap().id;
    let waiting_rt = rt.clone();
    let waiting_id = waiting_session.clone();
    let waiting = tokio::spawn(async move {
        waiting_rt
            .prompt(&waiting_id, "approval", CancellationToken::new())
            .await
    });
    let approval = pending(&rt, &waiting_session).await;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let task = rt.task(&approval.input.task_id).unwrap();
            if task.state == cayw_runtime::TaskState::WaitingApproval {
                assert!(!task.holds_slot);
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("approval wait must return its slot before other work is submitted");
    assert!(!workspace.join("approval.txt").exists());
    let ready_result = rt
        .prompt(&ready_session, "ready", CancellationToken::new())
        .await;
    if ready_result.is_err() {
        let original = rt.task(&approval.input.task_id).unwrap();
        rt.cancel(&original.cancel_target()).unwrap();
        let _ = waiting.await;
        panic!(
            "authorized work must finish while another task waits for approval: {ready_result:?}"
        );
    }
    assert_eq!(
        std::fs::read_to_string(workspace.join("ready.txt")).unwrap(),
        "ready"
    );
    assert!(!workspace.join("approval.txt").exists());
    let original = rt.task(&approval.input.task_id).unwrap();
    rt.approve(
        &waiting_session,
        &approval.id,
        ApprovalChoice::AllowOnce,
        "host-user",
    )
    .unwrap();
    tokio::time::timeout(Duration::from_secs(10), waiting)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let terminal = rt.task(&original.id).unwrap();
    assert_eq!(terminal.attempt_id, original.attempt_id);
    assert_eq!(terminal.worker_id, original.worker_id);
    assert_eq!(terminal.fence, original.fence);
    assert_eq!(
        std::fs::read_to_string(workspace.join("approval.txt")).unwrap(),
        "approval"
    );
    assert_eq!(rt.health().unwrap().running, 0);
}
