use std::collections::BTreeMap;

use cayw_plannotator::*;

fn target(id: &str, path: &str, version: u64, sha: &str) -> AnnotationTarget {
    AnnotationTarget {
        target_id: id.into(),
        kind: TargetKind::Plan,
        file: FileVersionBinding {
            path: path.into(),
            version,
            content_sha256: sha.into(),
        },
        label: Some("plan".into()),
    }
}

fn comment(id: &str, body: &str) -> AnnotationComment {
    AnnotationComment {
        comment_id: id.into(),
        body: body.into(),
        anchor: Some(AnnotationAnchor {
            start_offset: 10,
            end_offset: 40,
            quote: Some("- [ ] Add validation".into()),
            start_line: Some(3),
            end_line: Some(3),
        }),
        created_at_ms: 1_000,
    }
}

#[test]
fn annotate_one_visible_on_subsequent_attempt() {
    let mut store = FeedbackStore::new();
    let fb = store
        .submit(FeedbackDraft {
            session_id: "sess-1".into(),
            target: target("t-plan", "plans/login.md", 2, "sha-v2"),
            decision: FeedbackDecision::DenyWithFeedback,
            comments: vec![comment("c1", "validation belongs in the form handler")],
            summary: Some("revise plan before execute".into()),
            created_at_ms: 2_000,
            current_attempt: 1,
        })
        .expect("submit");

    assert_eq!(fb.visible_from_attempt, 2);
    assert!(store.for_attempt("sess-1", 1).is_empty());
    let visible = store.for_attempt("sess-1", 2);
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].feedback_id, fb.feedback_id);
    assert_eq!(
        visible[0].comments[0].body,
        "validation belongs in the form handler"
    );
}

#[test]
fn subsequent_attempt_applies_when_versions_match() {
    let mut store = FeedbackStore::new();
    let fb = store
        .submit(FeedbackDraft {
            session_id: "sess-1".into(),
            target: target("t-plan", "plans/login.md", 2, "sha-v2"),
            decision: FeedbackDecision::ApproveWithNotes,
            comments: vec![comment("c1", "keep checklist order")],
            summary: None,
            created_at_ms: 3_000,
            current_attempt: 3,
        })
        .expect("submit");

    let mut live = BTreeMap::new();
    live.insert(
        "plans/login.md".into(),
        LiveFileVersion {
            version: 2,
            content_sha256: "sha-v2".into(),
        },
    );
    let outcomes = apply_feedback_for_attempt(
        &store,
        &ApplyContext {
            session_id: "sess-1".into(),
            attempt: 4,
            live_files: live,
        },
    )
    .expect("apply");

    assert_eq!(outcomes.len(), 1);
    match &outcomes[0] {
        ApplyOutcome::Applied(payload) => {
            assert_eq!(payload.feedback_id, fb.feedback_id);
            assert_eq!(payload.file_version, 2);
            assert_eq!(payload.decision, FeedbackDecision::ApproveWithNotes);
            assert_eq!(payload.comments.len(), 1);
        }
        other => panic!("expected Applied, got {other:?}"),
    }
    assert_eq!(applied_payloads(&outcomes).len(), 1);
}

#[test]
fn version_mismatch_rejects_silent_apply() {
    let mut store = FeedbackStore::new();
    store
        .submit(FeedbackDraft {
            session_id: "sess-1".into(),
            target: target("t-plan", "plans/login.md", 2, "sha-v2"),
            decision: FeedbackDecision::DenyWithFeedback,
            comments: vec![comment("c1", "old note on v2")],
            summary: None,
            created_at_ms: 4_000,
            current_attempt: 1,
        })
        .expect("submit");

    let mut live = BTreeMap::new();
    live.insert(
        "plans/login.md".into(),
        LiveFileVersion {
            version: 3,
            content_sha256: "sha-v3".into(),
        },
    );
    let outcomes = apply_feedback_for_attempt(
        &store,
        &ApplyContext {
            session_id: "sess-1".into(),
            attempt: 2,
            live_files: live,
        },
    )
    .expect("apply");

    assert_eq!(outcomes.len(), 1);
    match &outcomes[0] {
        ApplyOutcome::RejectedVersionMismatch {
            bound,
            live,
            feedback_id,
        } => {
            assert_eq!(bound.version, 2);
            assert_eq!(bound.content_sha256, "sha-v2");
            assert_eq!(live.version, 3);
            assert_eq!(live.content_sha256, "sha-v3");
            assert!(!feedback_id.is_empty());
        }
        other => panic!("expected RejectedVersionMismatch, got {other:?}"),
    }
    assert!(applied_payloads(&outcomes).is_empty());
}

#[test]
fn missing_live_file_rejects() {
    let mut store = FeedbackStore::new();
    store
        .submit(FeedbackDraft {
            session_id: "sess-1".into(),
            target: target("t-md", "docs/spec.md", 1, "sha-a"),
            decision: FeedbackDecision::DenyWithFeedback,
            comments: vec![comment("c1", "clarify API")],
            summary: None,
            created_at_ms: 5_000,
            current_attempt: 1,
        })
        .expect("submit");

    let outcomes = apply_feedback_for_attempt(
        &store,
        &ApplyContext {
            session_id: "sess-1".into(),
            attempt: 2,
            live_files: BTreeMap::new(),
        },
    )
    .expect("apply");

    assert!(matches!(
        &outcomes[0],
        ApplyOutcome::RejectedMissingLive { .. }
    ));
}

#[test]
fn other_session_feedback_not_visible() {
    let mut store = FeedbackStore::new();
    store
        .submit(FeedbackDraft {
            session_id: "sess-a".into(),
            target: target("t1", "a.md", 1, "sha"),
            decision: FeedbackDecision::Approve,
            comments: vec![],
            summary: None,
            created_at_ms: 1,
            current_attempt: 1,
        })
        .expect("submit");

    assert!(store.for_attempt("sess-b", 99).is_empty());
}

#[test]
fn deny_without_comments_rejected() {
    let mut store = FeedbackStore::new();
    let err = store
        .submit(FeedbackDraft {
            session_id: "sess-1".into(),
            target: target("t1", "a.md", 1, "sha"),
            decision: FeedbackDecision::DenyWithFeedback,
            comments: vec![],
            summary: None,
            created_at_ms: 1,
            current_attempt: 1,
        })
        .expect_err("must reject empty deny");
    assert!(matches!(err, PlannotatorError::Invalid(_)));
}

#[test]
fn protocol_version_is_one() {
    assert_eq!(PLANNOTATOR_PROTOCOL_VERSION, 1);
}

#[test]
fn markdown_diff_local_page_kinds_roundtrip() {
    for kind in [
        TargetKind::Markdown,
        TargetKind::Diff,
        TargetKind::LocalPage,
    ] {
        let mut store = FeedbackStore::new();
        let mut t = target("t-x", "x", 1, "sha");
        t.kind = kind;
        let fb = store
            .submit(FeedbackDraft {
                session_id: "s".into(),
                target: t,
                decision: FeedbackDecision::Approve,
                comments: vec![],
                summary: None,
                created_at_ms: 1,
                current_attempt: 0,
            })
            .expect("submit");
        assert_eq!(fb.target.kind, kind);
        assert_eq!(fb.visible_from_attempt, 1);
    }
}

#[test]
fn applied_outcomes_produce_injectable_system_section() {
    let mut store = FeedbackStore::new();
    let fb = store
        .submit(FeedbackDraft {
            session_id: "sess-1".into(),
            target: target("t-plan", "plans/login.md", 2, "sha-v2"),
            decision: FeedbackDecision::DenyWithFeedback,
            comments: vec![comment("c1", "fix the handler")],
            summary: Some("revise".into()),
            created_at_ms: 2_000,
            current_attempt: 1,
        })
        .expect("submit");
    let mut live = BTreeMap::new();
    live.insert(
        "plans/login.md".into(),
        LiveFileVersion {
            version: 2,
            content_sha256: "sha-v2".into(),
        },
    );
    let outcomes = apply_feedback_for_attempt(
        &store,
        &ApplyContext {
            session_id: "sess-1".into(),
            attempt: 2,
            live_files: live,
        },
    )
    .expect("apply");
    let (key, body) = applied_feedback_section(&outcomes)
        .expect("section")
        .expect("must inject Applied payload");
    assert_eq!(key, FEEDBACK_SECTION_KEY);
    let parsed: Vec<AgentFeedbackPayload> = serde_json::from_str(&body).expect("json");
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].feedback_id, fb.feedback_id);
    assert_eq!(parsed[0].decision, FeedbackDecision::DenyWithFeedback);
}

#[test]
fn rejected_outcomes_do_not_inject() {
    let mut store = FeedbackStore::new();
    store
        .submit(FeedbackDraft {
            session_id: "sess-1".into(),
            target: target("t-plan", "plans/login.md", 2, "sha-v2"),
            decision: FeedbackDecision::DenyWithFeedback,
            comments: vec![comment("c1", "stale")],
            summary: None,
            created_at_ms: 2_000,
            current_attempt: 1,
        })
        .expect("submit");
    let mut live = BTreeMap::new();
    live.insert(
        "plans/login.md".into(),
        LiveFileVersion {
            version: 3,
            content_sha256: "sha-v3".into(),
        },
    );
    let outcomes = apply_feedback_for_attempt(
        &store,
        &ApplyContext {
            session_id: "sess-1".into(),
            attempt: 2,
            live_files: live,
        },
    )
    .expect("apply");
    assert!(applied_feedback_section(&outcomes).expect("ok").is_none());
}

#[test]
fn restore_is_idempotent_and_advances_ids() {
    let mut store = FeedbackStore::new();
    let fb = store
        .submit(FeedbackDraft {
            session_id: "sess-1".into(),
            target: target("t-plan", "plans/login.md", 1, "sha"),
            decision: FeedbackDecision::Approve,
            comments: vec![],
            summary: None,
            created_at_ms: 1,
            current_attempt: 0,
        })
        .expect("submit");
    store.restore(fb.clone());
    assert_eq!(store.len(), 1);
    let again = store
        .submit(FeedbackDraft {
            session_id: "sess-1".into(),
            target: target("t-plan-2", "plans/b.md", 1, "sha2"),
            decision: FeedbackDecision::Approve,
            comments: vec![],
            summary: None,
            created_at_ms: 2,
            current_attempt: 0,
        })
        .expect("submit");
    assert_ne!(again.feedback_id, fb.feedback_id);
}
