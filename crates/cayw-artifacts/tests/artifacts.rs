use cayw_artifacts::*;

fn put_card(
    ws: &ArtifactWorkspace,
    id: &str,
    session: &str,
    body: &str,
    at: u64,
) -> ArtifactRecord {
    ws.store
        .put(PutArtifactRequest {
            artifact_id: id.into(),
            session_id: session.into(),
            kind: ArtifactKind::HtmlCard,
            title: format!("Card {id}"),
            body: body.into(),
            at_ms: at,
            result_handle: Some("result:demo".into()),
        })
        .unwrap()
}

#[test]
fn html_artifact_preview_and_session_list() {
    let ws = ArtifactWorkspace::new();
    let a = put_card(&ws, "card-a", "sess-1", "<div>A</div>", 10);
    assert_eq!(a.content_version, 1);
    let _ = put_card(&ws, "chart-b", "sess-1", "<svg></svg>", 20);
    let _ = put_card(&ws, "other", "sess-2", "<p>x</p>", 30);

    let preview = ws.store.preview("card-a").unwrap();
    assert_eq!(preview.body, "<div>A</div>");
    assert_eq!(preview.result_handle.as_deref(), Some("result:demo"));

    let listed = ws.store.list_by_session("sess-1").unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].artifact_id, "card-a");
    assert_eq!(listed[1].artifact_id, "chart-b");
}

#[test]
fn restart_recovery_restores_artifacts_by_session() {
    let ws = ArtifactWorkspace::new();
    put_card(&ws, "card-a", "sess-1", "<p>persist</p>", 1);
    ws.store
        .update(UpdateArtifactRequest {
            artifact_id: "card-a".into(),
            session_id: "sess-1".into(),
            title: Some("Updated".into()),
            body: None,
            at_ms: 2,
            expected_content_version: Some(1),
        })
        .unwrap();

    let bundle = ws.export_recovery();
    let restored = ArtifactWorkspace::new();
    restored.restore_recovery(bundle).unwrap();

    let got = restored.store.get("card-a").unwrap();
    assert_eq!(got.title, "Updated");
    assert_eq!(got.content_version, 2);
    assert_eq!(got.session_id, "sess-1");
    assert_eq!(restored.store.list_by_session("sess-2").unwrap().len(), 0);
}

#[test]
fn read_only_action_is_applied_without_follow_up() {
    let ws = ArtifactWorkspace::new();
    put_card(&ws, "card-a", "sess-1", "<button>ok</button>", 1);
    let outcome = ws
        .actions
        .dispatch(ActionRequest {
            action_id: "act-ro".into(),
            artifact_id: "card-a".into(),
            session_id: "sess-1".into(),
            kind: ActionKind::Button,
            intent: ActionIntent::ReadOnly {
                note: "preview only".into(),
            },
            expected_content_version: Some(1),
            requested_at_ms: 5,
        })
        .unwrap();
    assert_eq!(outcome.record.status, ActionStatus::Applied);
    assert!(
        outcome
            .record
            .outcome_json
            .as_deref()
            .unwrap()
            .contains("read_only")
    );
}

#[test]
fn button_follow_up_accepts_same_session_task_intent() {
    let ws = ArtifactWorkspace::new();
    put_card(&ws, "card-a", "sess-1", "<button>run</button>", 1);
    let outcome = ws
        .actions
        .dispatch(ActionRequest {
            action_id: "act-fu".into(),
            artifact_id: "card-a".into(),
            session_id: "sess-1".into(),
            kind: ActionKind::Button,
            intent: ActionIntent::FollowUpTask {
                prompt: "summarize again".into(),
            },
            expected_content_version: Some(1),
            requested_at_ms: 10,
        })
        .unwrap();
    assert_eq!(outcome.record.status, ActionStatus::Accepted);
    let json = outcome.record.outcome_json.unwrap();
    assert!(json.contains("pending_follow_up_task"));
    assert!(json.contains("sess-1"));
    assert!(json.contains("summarize again"));
}

#[test]
fn cross_session_action_is_rejected() {
    let ws = ArtifactWorkspace::new();
    put_card(&ws, "card-a", "sess-1", "<button>x</button>", 1);
    let outcome = ws
        .actions
        .dispatch(ActionRequest {
            action_id: "act-x".into(),
            artifact_id: "card-a".into(),
            session_id: "sess-other".into(),
            kind: ActionKind::Button,
            intent: ActionIntent::FollowUpTask {
                prompt: "should fail".into(),
            },
            expected_content_version: None,
            requested_at_ms: 11,
        })
        .unwrap();
    assert_eq!(outcome.record.status, ActionStatus::Rejected);
    assert!(
        outcome
            .record
            .rejection_reason
            .as_deref()
            .unwrap()
            .contains("cross_session")
    );
}

#[test]
fn version_mismatch_rejects_stale_action() {
    let ws = ArtifactWorkspace::new();
    put_card(&ws, "card-a", "sess-1", "<p>v1</p>", 1);
    ws.store
        .update(UpdateArtifactRequest {
            artifact_id: "card-a".into(),
            session_id: "sess-1".into(),
            title: None,
            body: Some("<p>v2</p>".into()),
            at_ms: 2,
            expected_content_version: Some(1),
        })
        .unwrap();
    let outcome = ws
        .actions
        .dispatch(ActionRequest {
            action_id: "act-stale".into(),
            artifact_id: "card-a".into(),
            session_id: "sess-1".into(),
            kind: ActionKind::FormSubmit,
            intent: ActionIntent::InvokeTool {
                tool: "search.query".into(),
                args_json: "{}".into(),
            },
            expected_content_version: Some(1),
            requested_at_ms: 3,
        })
        .unwrap();
    assert_eq!(outcome.record.status, ActionStatus::Rejected);
    assert!(
        outcome
            .record
            .rejection_reason
            .as_deref()
            .unwrap()
            .contains("version_mismatch")
    );
}

#[test]
fn action_state_survives_refresh_snapshot() {
    let ws = ArtifactWorkspace::new();
    put_card(&ws, "card-a", "sess-1", "<form></form>", 1);
    ws.actions
        .dispatch(ActionRequest {
            action_id: "act-state".into(),
            artifact_id: "card-a".into(),
            session_id: "sess-1".into(),
            kind: ActionKind::FormSubmit,
            intent: ActionIntent::UpdateState {
                key: "selected_row".into(),
                value_json: "\"r2\"".into(),
            },
            expected_content_version: Some(1),
            requested_at_ms: 4,
        })
        .unwrap();
    assert_eq!(
        ws.actions
            .artifact_state("card-a")
            .unwrap()
            .get("selected_row"),
        Some(&"\"r2\"".to_string())
    );

    let bundle = ws.export_recovery();
    let restored = ArtifactWorkspace::new();
    restored.restore_recovery(bundle).unwrap();
    assert_eq!(
        restored
            .actions
            .artifact_state("card-a")
            .unwrap()
            .get("selected_row"),
        Some(&"\"r2\"".to_string())
    );
    let actions = restored.actions.list_actions_for_session("sess-1").unwrap();
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0].status, ActionStatus::Applied);
}

#[test]
fn foreign_session_cannot_update_artifact() {
    let ws = ArtifactWorkspace::new();
    put_card(&ws, "card-a", "sess-1", "<p>a</p>", 1);
    let err = ws
        .store
        .update(UpdateArtifactRequest {
            artifact_id: "card-a".into(),
            session_id: "sess-2".into(),
            title: Some("Nope".into()),
            body: None,
            at_ms: 9,
            expected_content_version: None,
        })
        .unwrap_err();
    assert!(matches!(err, ArtifactError::SessionMismatch { .. }));
}
