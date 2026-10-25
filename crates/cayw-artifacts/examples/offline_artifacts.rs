//! Offline smoke demo for F09 HTML artifacts + F19 action protocol.
//! No network, no Provider, no Runtime task launch.

use cayw_artifacts::*;

fn main() {
    let ws = ArtifactWorkspace::new();
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
    println!("preview bytes={}", preview.body.len());

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

    let bundle = ws.export_recovery();
    let restored = ArtifactWorkspace::new();
    restored.restore_recovery(bundle).expect("restore");
    let listed = restored
        .store
        .list_by_session("session-demo")
        .expect("list");
    println!(
        "after restore: {} artifact(s), {} action(s)",
        listed.len(),
        restored
            .actions
            .list_actions_for_session("session-demo")
            .expect("actions")
            .len()
    );
}
