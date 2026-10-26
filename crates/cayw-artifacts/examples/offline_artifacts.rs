//! Offline smoke demo for F09 HTML artifacts + F19 action protocol.
//! No network, no Provider. Follow-up / tool actions hit RecordingHostDispatch
//! (stand-in for Runtime/Host); recovery is written under a temp product root.

use std::sync::Arc;

use cayw_artifacts::*;

fn main() {
    let bridge = Arc::new(RecordingHostDispatch::new());
    let ws = ArtifactWorkspace::with_dispatch(Arc::clone(&bridge) as SharedHostDispatch);
    println!("ARTIFACT_PROTOCOL_VERSION={}", ARTIFACT_PROTOCOL_VERSION);

    let card = ws
        .store
        .put(PutArtifactRequest {
            artifact_id: "demo-card".into(),
            session_id: "session-demo".into(),
            kind: ArtifactKind::HtmlCard,
            title: "Demo card".into(),
            body: "<button data-action=\"follow_up\">Retry summary</button>".into(),
            at_ms: 1_000,
            result_handle: Some("result:demo-html".into()),
        })
        .expect("put");
    println!(
        "put kind={} version={} handle={:?}",
        card.kind.as_str(),
        card.content_version,
        card.result_handle
    );

    let preview = ws.store.preview("demo-card").expect("preview");
    println!(
        "preview content_type={} html_bytes={}",
        preview.content_type,
        preview.html.len()
    );

    let follow = ws
        .actions
        .dispatch(ActionRequest {
            action_id: "demo-follow".into(),
            artifact_id: "demo-card".into(),
            session_id: "session-demo".into(),
            kind: ActionKind::Button,
            intent: ActionIntent::FollowUpTask {
                prompt: "Retry the summary with tighter bullets".into(),
            },
            expected_content_version: Some(1),
            requested_at_ms: 1_100,
        })
        .expect("dispatch");
    println!(
        "follow-up status={:?} outcome={}",
        follow.record.status,
        follow.record.outcome_json.as_deref().unwrap_or("")
    );
    println!("bridge calls after follow-up: {}", bridge.calls().len());

    let tool = ws
        .actions
        .dispatch(ActionRequest {
            action_id: "demo-tool".into(),
            artifact_id: "demo-card".into(),
            session_id: "session-demo".into(),
            kind: ActionKind::FormSubmit,
            intent: ActionIntent::InvokeTool {
                tool: "search.query".into(),
                args_json: "{\"q\":\"demo\"}".into(),
            },
            expected_content_version: Some(1),
            requested_at_ms: 1_150,
        })
        .expect("dispatch tool");
    println!(
        "invoke-tool status={:?} outcome={}",
        tool.record.status,
        tool.record.outcome_json.as_deref().unwrap_or("")
    );
    println!("bridge calls after tool: {}", bridge.calls().len());

    let cross = ws
        .actions
        .dispatch(ActionRequest {
            action_id: "demo-cross".into(),
            artifact_id: "demo-card".into(),
            session_id: "other-session".into(),
            kind: ActionKind::Button,
            intent: ActionIntent::FollowUpTask {
                prompt: "should reject".into(),
            },
            expected_content_version: None,
            requested_at_ms: 1_200,
        })
        .expect("dispatch cross");
    println!(
        "cross-session status={:?} reason={}",
        cross.record.status,
        cross.record.rejection_reason.as_deref().unwrap_or("")
    );

    let root = tempfile::tempdir().expect("temp root");
    let path = ws.save_to_disk(root.path()).expect("save");
    println!("disk recovery path={}", path.display());
    let restored = ArtifactWorkspace::load_from_disk(
        root.path(),
        Arc::new(RecordingHostDispatch::new()) as SharedHostDispatch,
    )
    .expect("load");
    let listed = restored
        .store
        .list_by_session("session-demo")
        .expect("list");
    println!(
        "after disk restore: {} artifact(s), {} action(s)",
        listed.len(),
        restored
            .actions
            .list_actions_for_session("session-demo")
            .expect("actions")
            .len()
    );
}
