use std::sync::Arc;

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
fn html_artifact_preview_is_renderer_projection() {
    let ws = ArtifactWorkspace::new();
    let a = put_card(&ws, "card-a", "sess-1", "<div>A</div>", 10);
    assert_eq!(a.content_version, 1);
    let _ = put_card(&ws, "chart-b", "sess-1", "<svg></svg>", 20);
    let _ = put_card(&ws, "other", "sess-2", "<p>x</p>", 30);

    let preview = ws.store.preview("card-a").unwrap();
    assert_eq!(preview.html, "<div>A</div>");
    assert_eq!(preview.content_type, ArtifactPreview::HTML_CONTENT_TYPE);
    assert_eq!(preview.title, "Card card-a");
    assert_eq!(preview.kind, ArtifactKind::HtmlCard);
    assert_eq!(preview.result_handle.as_deref(), Some("result:demo"));
    // Preview is not the storage record type.
    let _: ArtifactPreview = preview;

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
fn disk_recovery_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let ws = ArtifactWorkspace::new();
    put_card(&ws, "card-a", "sess-1", "<p>disk</p>", 1);
    ws.actions
        .dispatch(ActionRequest {
            action_id: "act-disk".into(),
            artifact_id: "card-a".into(),
            session_id: "sess-1".into(),
            kind: ActionKind::Button,
            intent: ActionIntent::UpdateState {
                key: "tab".into(),
                value_json: "\"b\"".into(),
            },
            expected_content_version: Some(1),
            requested_at_ms: 2,
        })
        .unwrap();

    let path = ws.save_to_disk(dir.path()).unwrap();
    assert_eq!(path, recovery_path(dir.path()));
    assert!(path.is_file());

    let bridge = Arc::new(RecordingHostDispatch::new());
    let restored = ArtifactWorkspace::load_from_disk(dir.path(), bridge).unwrap();
    assert_eq!(restored.store.get("card-a").unwrap().body, "<p>disk</p>");
    assert_eq!(
        restored
            .actions
            .artifact_state("card-a")
            .unwrap()
            .get("tab"),
        Some(&"\"b\"".to_string())
    );
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
fn button_follow_up_hits_host_dispatch_bridge() {
    let bridge = Arc::new(RecordingHostDispatch::new());
    let ws = ArtifactWorkspace::with_dispatch(Arc::clone(&bridge) as SharedHostDispatch);
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
    assert_eq!(outcome.record.status, ActionStatus::Applied);
    let json = outcome.record.outcome_json.unwrap();
    assert!(json.contains("follow_up_task_dispatched"));
    assert!(json.contains("sess-1"));
    assert!(json.contains("summarize again"));
    assert!(!json.contains("pending_follow_up_task"));

    let calls = bridge.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0],
        RecordedHostCall::FollowUpTask {
            action_id: "act-fu".into(),
            artifact_id: "card-a".into(),
            session_id: "sess-1".into(),
            prompt: "summarize again".into(),
        }
    );
}

#[test]
fn invoke_tool_hits_host_dispatch_bridge() {
    let bridge = Arc::new(RecordingHostDispatch::new());
    let ws = ArtifactWorkspace::with_dispatch(Arc::clone(&bridge) as SharedHostDispatch);
    put_card(&ws, "card-a", "sess-1", "<form></form>", 1);
    let outcome = ws
        .actions
        .dispatch(ActionRequest {
            action_id: "act-tool".into(),
            artifact_id: "card-a".into(),
            session_id: "sess-1".into(),
            kind: ActionKind::FormSubmit,
            intent: ActionIntent::InvokeTool {
                tool: "search.query".into(),
                args_json: "{\"q\":\"x\"}".into(),
            },
            expected_content_version: Some(1),
            requested_at_ms: 12,
        })
        .unwrap();
    assert_eq!(outcome.record.status, ActionStatus::Applied);
    let json = outcome.record.outcome_json.unwrap();
    assert!(json.contains("tool_invoked"));
    assert!(json.contains("search.query"));
    assert!(!json.contains("pending_tool_invoke"));

    let calls = bridge.calls();
    assert_eq!(calls.len(), 1);
    match &calls[0] {
        RecordedHostCall::InvokeTool {
            tool, args_json, ..
        } => {
            assert_eq!(tool, "search.query");
            assert_eq!(args_json, "{\"q\":\"x\"}");
        }
        other => panic!("unexpected call: {other:?}"),
    }
}

#[test]
fn host_dispatch_error_rejects_action() {
    struct FailingDispatch;
    impl HostActionDispatch for FailingDispatch {
        fn follow_up_task(
            &self,
            _req: FollowUpDispatch<'_>,
        ) -> Result<HostDispatchResult, HostDispatchError> {
            Err(HostDispatchError::rejected("runtime unavailable"))
        }
        fn invoke_tool(
            &self,
            _req: ToolDispatch<'_>,
        ) -> Result<HostDispatchResult, HostDispatchError> {
            Err(HostDispatchError::rejected("host denied"))
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    let ws = ArtifactWorkspace::with_dispatch(Arc::new(FailingDispatch));
    put_card(&ws, "card-a", "sess-1", "<button>x</button>", 1);
    let outcome = ws
        .actions
        .dispatch(ActionRequest {
            action_id: "act-fail".into(),
            artifact_id: "card-a".into(),
            session_id: "sess-1".into(),
            kind: ActionKind::Button,
            intent: ActionIntent::FollowUpTask {
                prompt: "go".into(),
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
            .contains("runtime unavailable")
    );
}

#[test]
fn transactional_snapshot_recovery_preserves_store_on_action_failure() {
    let ws = ArtifactWorkspace::new();
    put_card(&ws, "card-original", "sess-1", "<p>original</p>", 100);

    // Prepare a recovery bundle where artifacts are valid, but actions contain an invalid action_id
    let mut bundle = ws.export_recovery();
    bundle.artifacts = ArtifactSnapshot {
        artifacts: vec![ArtifactRecord {
            artifact_id: "card-new".into(),
            session_id: "sess-1".into(),
            kind: ArtifactKind::HtmlCard,
            title: "New".into(),
            body: "<p>new</p>".into(),
            content_version: 1,
            created_at_ms: 200,
            updated_at_ms: 200,
            result_handle: None,
        }],
    };
    bundle.actions.actions.push(ActionRecord {
        action_id: "invalid action id with spaces!".into(),
        artifact_id: "card-new".into(),
        session_id: "sess-1".into(),
        kind: ActionKind::Button,
        intent: ActionIntent::ReadOnly {
            note: "test".into(),
        },
        status: ActionStatus::Applied,
        rejection_reason: None,
        outcome_json: None,
        requested_at_ms: 200,
        recorded_at_ms: 200,
    });

    let res = ws.restore_recovery(bundle);
    assert!(res.is_err(), "recovery must fail due to invalid action ID");

    // The store must NOT have been updated with card-new; card-original must still be there!
    assert!(
        ws.store.get("card-original").is_ok(),
        "original card must remain untouched"
    );
    assert!(
        ws.store.get("card-new").is_err(),
        "uncommitted new card must not exist"
    );
}

#[test]
fn host_dispatch_unknown_records_outcome_unknown_and_blocks_session() {
    struct UnknownDispatch;
    impl HostActionDispatch for UnknownDispatch {
        fn follow_up_task(
            &self,
            _req: FollowUpDispatch<'_>,
        ) -> Result<HostDispatchResult, HostDispatchError> {
            Err(HostDispatchError::unknown(
                "backend timeout during follow-up",
            ))
        }
        fn invoke_tool(
            &self,
            _req: ToolDispatch<'_>,
        ) -> Result<HostDispatchResult, HostDispatchError> {
            Err(HostDispatchError::unknown("capability uncertain timeout"))
        }
        fn cancel_action(&self, _action_id: &str) -> Result<bool, HostDispatchError> {
            Err(HostDispatchError::unknown("cancel uncertain timeout"))
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    let ws = ArtifactWorkspace::with_dispatch(Arc::new(UnknownDispatch));
    put_card(&ws, "card-a", "sess-1", "<button>x</button>", 1);

    // 1. Dispatching with unknown error records OutcomeUnknown status
    let outcome = ws
        .actions
        .dispatch(ActionRequest {
            action_id: "act-unknown-1".into(),
            artifact_id: "card-a".into(),
            session_id: "sess-1".into(),
            kind: ActionKind::Button,
            intent: ActionIntent::FollowUpTask {
                prompt: "summarize".into(),
            },
            expected_content_version: Some(1),
            requested_at_ms: 10,
        })
        .unwrap();

    assert_eq!(outcome.record.status, ActionStatus::OutcomeUnknown);
    assert!(
        outcome
            .record
            .rejection_reason
            .as_deref()
            .unwrap()
            .contains("backend timeout during follow-up")
    );
    assert!(ws.actions.has_outcome_unknown("sess-1"));

    // 2. Subsequent action on the same session is blocked with NeedsReconciliation
    let blocked = ws
        .actions
        .dispatch(ActionRequest {
            action_id: "act-blocked-2".into(),
            artifact_id: "card-a".into(),
            session_id: "sess-1".into(),
            kind: ActionKind::Button,
            intent: ActionIntent::ReadOnly {
                note: "test".into(),
            },
            expected_content_version: Some(1),
            requested_at_ms: 20,
        })
        .unwrap_err();

    assert!(matches!(blocked, ArtifactError::NeedsReconciliation(_)));
}

#[test]
fn host_dispatch_unknown_cancel_preserves_needs_reconciliation() {
    struct CancelUnknownDispatch;
    impl HostActionDispatch for CancelUnknownDispatch {
        fn follow_up_task(
            &self,
            _req: FollowUpDispatch<'_>,
        ) -> Result<HostDispatchResult, HostDispatchError> {
            Ok(HostDispatchResult {
                outcome_json: "{}".into(),
            })
        }
        fn invoke_tool(
            &self,
            _req: ToolDispatch<'_>,
        ) -> Result<HostDispatchResult, HostDispatchError> {
            Ok(HostDispatchResult {
                outcome_json: "{}".into(),
            })
        }
        fn cancel_action(&self, _action_id: &str) -> Result<bool, HostDispatchError> {
            Err(HostDispatchError::unknown(
                "cancel failed with unknown outcome",
            ))
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    let ws = ArtifactWorkspace::with_dispatch(Arc::new(CancelUnknownDispatch));
    put_card(&ws, "card-a", "sess-1", "<button>x</button>", 1);

    let outcome = ws
        .actions
        .dispatch(ActionRequest {
            action_id: "act-cancel-me".into(),
            artifact_id: "card-a".into(),
            session_id: "sess-1".into(),
            kind: ActionKind::Button,
            intent: ActionIntent::FollowUpTask {
                prompt: "run".into(),
            },
            expected_content_version: Some(1),
            requested_at_ms: 10,
        })
        .unwrap();
    assert_eq!(outcome.record.status, ActionStatus::Applied);

    // Cancel returning unknown must return NeedsReconciliation rather than Invalid
    let cancel_err = ws.actions.cancel("act-cancel-me").unwrap_err();
    assert!(
        matches!(cancel_err, ArtifactError::NeedsReconciliation(msg) if msg.contains("cancel_action: cancel failed with unknown outcome"))
    );
}

#[test]
fn outcome_unknown_blocks_subsequent_actions() {
    struct UnknownDispatch;
    impl HostActionDispatch for UnknownDispatch {
        fn follow_up_task(
            &self,
            _req: FollowUpDispatch<'_>,
        ) -> Result<HostDispatchResult, HostDispatchError> {
            Err(HostDispatchError::unknown(
                "timeout after tool started effects",
            ))
        }
        fn invoke_tool(
            &self,
            _req: ToolDispatch<'_>,
        ) -> Result<HostDispatchResult, HostDispatchError> {
            Err(HostDispatchError::unknown("interrupted during execution"))
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    let ws = ArtifactWorkspace::with_dispatch(Arc::new(UnknownDispatch));
    put_card(&ws, "card-a", "sess-1", "<button>x</button>", 1);
    let outcome = ws
        .actions
        .dispatch(ActionRequest {
            action_id: "act-unknown".into(),
            artifact_id: "card-a".into(),
            session_id: "sess-1".into(),
            kind: ActionKind::Button,
            intent: ActionIntent::FollowUpTask {
                prompt: "run risky task".into(),
            },
            expected_content_version: Some(1),
            requested_at_ms: 1,
        })
        .unwrap();

    assert_eq!(outcome.record.status, ActionStatus::OutcomeUnknown);
    assert!(ws.actions.has_outcome_unknown("sess-1"));

    // Subsequent action on sess-1 must be blocked with NeedsReconciliation
    let err = ws
        .actions
        .dispatch(ActionRequest {
            action_id: "act-subsequent".into(),
            artifact_id: "card-a".into(),
            session_id: "sess-1".into(),
            kind: ActionKind::Button,
            intent: ActionIntent::ReadOnly {
                note: "safe read".into(),
            },
            expected_content_version: Some(1),
            requested_at_ms: 2,
        })
        .unwrap_err();

    assert!(
        matches!(err, ArtifactError::NeedsReconciliation(_)),
        "subsequent actions must require reconciliation, got {err:?}"
    );
}

#[test]
fn cross_session_action_is_rejected() {
    let bridge = Arc::new(RecordingHostDispatch::new());
    let ws = ArtifactWorkspace::with_dispatch(Arc::clone(&bridge) as SharedHostDispatch);
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
    // Bridge must not be called for rejected cross-session actions.
    assert!(bridge.calls().is_empty());
}

#[test]
fn version_mismatch_rejects_stale_action() {
    let bridge = Arc::new(RecordingHostDispatch::new());
    let ws = ArtifactWorkspace::with_dispatch(Arc::clone(&bridge) as SharedHostDispatch);
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
    assert!(bridge.calls().is_empty());
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
