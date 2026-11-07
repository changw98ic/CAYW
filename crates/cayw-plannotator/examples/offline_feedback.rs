//! Offline smoke: annotate a plan → next attempt sees structured feedback;
//! version bump refuses silent apply.

use std::collections::BTreeMap;

use cayw_plannotator::*;

fn main() {
    let mut store = FeedbackStore::new();
    let target = AnnotationTarget {
        target_id: "plan-login".into(),
        kind: TargetKind::Plan,
        file: FileVersionBinding {
            path: "plans/login.md".into(),
            version: 2,
            content_sha256: "sha-v2".into(),
        },
        label: Some("login plan".into()),
    };

    let fb = store
        .submit(FeedbackDraft {
            session_id: "sess-demo".into(),
            target: target.clone(),
            decision: FeedbackDecision::DenyWithFeedback,
            comments: vec![AnnotationComment {
                comment_id: "c1".into(),
                body: "validation belongs in the form handler, not the route".into(),
                anchor: Some(AnnotationAnchor {
                    start_offset: 80,
                    end_offset: 120,
                    quote: Some("- [ ] Add validation to the login form".into()),
                    start_line: Some(4),
                    end_line: Some(4),
                }),
                created_at_ms: 1_700_000_000_000,
            }],
            summary: Some("revise before execute".into()),
            created_at_ms: 1_700_000_000_100,
            current_attempt: 1,
        })
        .expect("submit");

    println!(
        "recorded {} visible_from_attempt={} decision={:?}",
        fb.feedback_id, fb.visible_from_attempt, fb.decision
    );

    let mut live_ok = BTreeMap::new();
    live_ok.insert(
        "plans/login.md".into(),
        LiveFileVersion {
            version: 2,
            content_sha256: "sha-v2".into(),
        },
    );
    let applied = apply_feedback_for_attempt(
        &store,
        &ApplyContext {
            session_id: "sess-demo".into(),
            attempt: 2,
            live_files: live_ok,
        },
    )
    .expect("apply matching");
    for outcome in &applied {
        match outcome {
            ApplyOutcome::Applied(p) => {
                println!(
                    "attempt-2 applied: path={} v={} comments={}",
                    p.path,
                    p.file_version,
                    p.comments.len()
                );
            }
            other => println!("unexpected: {other:?}"),
        }
    }

    let mut live_stale = BTreeMap::new();
    live_stale.insert(
        "plans/login.md".into(),
        LiveFileVersion {
            version: 3,
            content_sha256: "sha-v3".into(),
        },
    );
    let rejected = apply_feedback_for_attempt(
        &store,
        &ApplyContext {
            session_id: "sess-demo".into(),
            attempt: 2,
            live_files: live_stale,
        },
    )
    .expect("apply mismatch");
    for outcome in &rejected {
        match outcome {
            ApplyOutcome::RejectedVersionMismatch { bound, live, .. } => {
                println!(
                    "version mismatch refuse silent apply: bound v={} sha={} live v={} sha={}",
                    bound.version, bound.content_sha256, live.version, live.content_sha256
                );
            }
            other => println!("unexpected: {other:?}"),
        }
    }

    println!("protocol_version={PLANNOTATOR_PROTOCOL_VERSION}");
}
