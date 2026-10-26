//! F09/F19 audit remediation: artifact actions reach Runtime/Host (not Recording-only).

use cayw_ai::{AssistantEvent, AssistantStream, CancellationToken, Model, ModelRequest, Provider};
use cayw_runtime::{
    ActionIntent, ActionKind, ActionRequest, ActionStatus, ArtifactKind, PutArtifactRequest,
    Runtime, RuntimeConfig, RuntimeHostDispatch, ScopedPermission, TaskState,
};
use futures::stream;
use serde_json::json;
use std::sync::Arc;

struct IdleProvider;
impl Provider for IdleProvider {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let message = cayw_runtime::fixture::script_response(&request, "return 1", "idle-1");
        Box::pin(stream::iter(vec![AssistantEvent::Done { message }]))
    }
}

fn open(permissions: Vec<ScopedPermission>) -> (tempfile::TempDir, Runtime) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("workspace")).unwrap();
    let rt = Runtime::open(
        dir.path().join("session.db"),
        dir.path().join("workspace"),
        Arc::new(IdleProvider),
        Model::new("fixture", "artifact-dispatch"),
        RuntimeConfig {
            permissions,
            ..Default::default()
        },
    )
    .unwrap();
    (dir, rt)
}

fn put_card(rt: &Runtime, session: &str, id: &str, body: &str) {
    rt.put_artifact(PutArtifactRequest {
        artifact_id: id.into(),
        session_id: session.into(),
        kind: ArtifactKind::HtmlCard,
        title: format!("Card {id}"),
        body: body.into(),
        at_ms: 1,
        result_handle: None,
    })
    .unwrap();
}

#[test]
fn product_default_dispatch_is_runtime_host_dispatch() {
    let (_dir, rt) = open(vec![]);
    let dispatch = rt.artifact_host_dispatch();
    assert!(
        dispatch
            .as_ref()
            .as_any()
            .downcast_ref::<RuntimeHostDispatch>()
            .is_some(),
        "Runtime product path must default to RuntimeHostDispatch"
    );
}

#[test]
fn follow_up_action_enqueues_runtime_task() {
    let (_dir, rt) = open(vec![]);
    let session = rt.create(Some("artifacts".into())).unwrap().id;
    put_card(&rt, &session, "card-fu", "<button>retry</button>");

    let outcome = rt
        .dispatch_artifact_action(ActionRequest {
            action_id: "act-fu".into(),
            artifact_id: "card-fu".into(),
            session_id: session.clone(),
            kind: ActionKind::Button,
            intent: ActionIntent::FollowUpTask {
                prompt: "retry the summary from the card".into(),
            },
            expected_content_version: Some(1),
            requested_at_ms: 10,
        })
        .unwrap();

    assert_eq!(outcome.record.status, ActionStatus::Applied);
    let json = outcome.record.outcome_json.as_deref().unwrap();
    assert!(json.contains("follow_up_task_dispatched"));
    assert!(json.contains("task_id"));
    assert!(!json.contains("pending_follow_up_task"));

    let tasks = rt.tasks(&session).unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].state, TaskState::Queued);
    assert_eq!(tasks[0].input.text, "retry the summary from the card");
    assert!(json.contains(&tasks[0].id));
}

#[test]
fn invoke_tool_action_calls_real_host() {
    let (dir, rt) = open(vec![ScopedPermission::exact(
        "fs.read",
        json!({"path": "note.txt"}),
    )]);
    std::fs::write(dir.path().join("workspace/note.txt"), "host-evidence").unwrap();
    let session = rt.create(Some("tools".into())).unwrap().id;
    put_card(&rt, &session, "card-tool", "<form></form>");

    let outcome = rt
        .dispatch_artifact_action(ActionRequest {
            action_id: "act-tool".into(),
            artifact_id: "card-tool".into(),
            session_id: session,
            kind: ActionKind::FormSubmit,
            intent: ActionIntent::InvokeTool {
                tool: "fs.read".into(),
                args_json: "{\"path\":\"note.txt\"}".into(),
            },
            expected_content_version: Some(1),
            requested_at_ms: 12,
        })
        .unwrap();

    assert_eq!(outcome.record.status, ActionStatus::Applied);
    let json = outcome.record.outcome_json.unwrap();
    assert!(json.contains("tool_invoked"));
    assert!(json.contains("fs.read"));
    assert!(
        json.contains("host-evidence"),
        "Host fs.read result must appear in outcome: {json}"
    );
    assert!(!json.contains("pending_tool_invoke"));
}

#[test]
fn invoke_tool_without_permission_is_rejected() {
    let (_dir, rt) = open(vec![]);
    let session = rt.create(None).unwrap().id;
    put_card(&rt, &session, "card-deny", "<button>x</button>");
    let outcome = rt
        .dispatch_artifact_action(ActionRequest {
            action_id: "act-deny".into(),
            artifact_id: "card-deny".into(),
            session_id: session,
            kind: ActionKind::Button,
            intent: ActionIntent::InvokeTool {
                tool: "fs.read".into(),
                args_json: "{\"path\":\"secret.txt\"}".into(),
            },
            expected_content_version: Some(1),
            requested_at_ms: 1,
        })
        .unwrap();
    assert_eq!(outcome.record.status, ActionStatus::Rejected);
    assert!(
        outcome
            .record
            .rejection_reason
            .as_deref()
            .unwrap()
            .contains("not preauthorized")
    );
}

#[test]
fn recovery_round_trip_keeps_runtime_dispatch() {
    let (dir, rt) = open(vec![]);
    let session = rt.create(None).unwrap().id;
    put_card(&rt, &session, "card-disk", "<p>disk</p>");
    rt.dispatch_artifact_action(ActionRequest {
        action_id: "act-state".into(),
        artifact_id: "card-disk".into(),
        session_id: session,
        kind: ActionKind::Button,
        intent: ActionIntent::UpdateState {
            key: "tab".into(),
            value_json: "\"b\"".into(),
        },
        expected_content_version: Some(1),
        requested_at_ms: 2,
    })
    .unwrap();

    let path = rt.save_artifact_recovery().unwrap();
    assert_eq!(path, dir.path().join("workspace/artifacts/recovery.json"));

    // Peer Runtime on the same workspace loads recovery with RuntimeHostDispatch.
    let peer = Runtime::open(
        dir.path().join("peer.db"),
        dir.path().join("workspace"),
        Arc::new(IdleProvider),
        Model::new("fixture", "artifact-dispatch-peer"),
        RuntimeConfig::default(),
    )
    .unwrap();
    peer.load_artifact_recovery().unwrap();
    assert_eq!(
        peer.preview_artifact("card-disk").unwrap().html,
        "<p>disk</p>"
    );
    assert_eq!(
        peer.artifact_actions()
            .artifact_state("card-disk")
            .unwrap()
            .get("tab"),
        Some(&"\"b\"".to_string())
    );

    let peer_session = peer.create(Some("peer".into())).unwrap().id;
    peer.put_artifact(PutArtifactRequest {
        artifact_id: "card-peer".into(),
        session_id: peer_session.clone(),
        kind: ArtifactKind::HtmlCard,
        title: "Peer".into(),
        body: "<button>go</button>".into(),
        at_ms: 50,
        result_handle: None,
    })
    .unwrap();
    let outcome = peer
        .dispatch_artifact_action(ActionRequest {
            action_id: "act-after".into(),
            artifact_id: "card-peer".into(),
            session_id: peer_session.clone(),
            kind: ActionKind::Button,
            intent: ActionIntent::FollowUpTask {
                prompt: "after recovery".into(),
            },
            expected_content_version: Some(1),
            requested_at_ms: 99,
        })
        .unwrap();
    assert_eq!(outcome.record.status, ActionStatus::Applied);
    let tasks = peer.tasks(&peer_session).unwrap();
    assert!(
        tasks.iter().any(|t| t.input.text == "after recovery"),
        "loaded recovery must keep RuntimeHostDispatch as product path"
    );
}
