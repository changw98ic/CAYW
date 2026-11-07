//! Offline demo: two retry paths + A/B wall-aligned compare.

use cayw_run_inspector::*;

#[allow(clippy::too_many_arguments)]
fn step(
    id: &str,
    session: &str,
    turn: u64,
    ordinal: u64,
    tool: &str,
    params: &str,
    outcome: StepOutcome,
    started: u64,
    elapsed: u64,
) -> RunStep {
    RunStep {
        step_id: id.into(),
        session_id: session.into(),
        turn,
        ordinal,
        tool: tool.into(),
        params_digest: params.into(),
        outcome,
        started_at_ms: started,
        elapsed_ms: elapsed,
        evidence_cursor: None,
        error_summary: None,
    }
}

fn main() {
    let session_a = SessionRunSnapshot {
        session_id: "A".into(),
        label: Some("model-a".into()),
        task_fingerprint: Some("fix-login".into()),
        steps: vec![
            step(
                "a1",
                "A",
                1,
                0,
                "bash",
                "curl-v1",
                StepOutcome::Failed,
                1_000,
                80,
            ),
            step(
                "a2",
                "A",
                1,
                1,
                "bash",
                "curl-v1",
                StepOutcome::BlindRetry,
                1_100,
                80,
            ),
            step(
                "a3",
                "A",
                1,
                2,
                "bash",
                "curl-v2",
                StepOutcome::Success,
                1_200,
                60,
            ),
            step(
                "a4",
                "A",
                2,
                0,
                "write",
                "fix",
                StepOutcome::Failed,
                2_000,
                40,
            ),
            step(
                "a5",
                "A",
                2,
                1,
                "apply_patch",
                "fix",
                StepOutcome::Success,
                2_100,
                50,
            ),
        ],
    };
    let session_b = SessionRunSnapshot {
        session_id: "B".into(),
        label: Some("model-b".into()),
        task_fingerprint: Some("fix-login".into()),
        steps: vec![
            step(
                "b1",
                "B",
                1,
                0,
                "bash",
                "curl-v2",
                StepOutcome::Success,
                1_050,
                70,
            ),
            step(
                "b2",
                "B",
                2,
                0,
                "apply_patch",
                "fix",
                StepOutcome::Success,
                1_900,
                45,
            ),
        ],
    };

    let timeline = build_run_timeline(&session_a);
    println!("protocol={}", RUN_INSPECTOR_PROTOCOL_VERSION);
    println!(
        "timeline A steps={} failures={} paths={}",
        timeline.step_count,
        timeline.failure_count,
        timeline.retry_paths.len()
    );
    for path in &timeline.retry_paths {
        println!(
            "  path {} recovery={:?} follow={:?}",
            path.path_id, path.recovery, path.follow_up_step_ids
        );
    }

    let compare = compare_sessions(&[session_a, session_b]).expect("compare");
    println!(
        "compare same_task={} sessions={:?} aligned_turns={}",
        compare.same_task,
        compare.session_ids,
        compare.aligned_turns.len()
    );
    for turn in &compare.aligned_turns {
        let summary: Vec<_> = turn
            .lanes
            .iter()
            .map(|lane| {
                format!(
                    "{}:elapsed={}ms:branches={}",
                    lane.session_id, lane.elapsed_ms, lane.branch_count
                )
            })
            .collect();
        println!("  turn {} => {}", turn.turn, summary.join(" | "));
    }
    for track in &compare.tracks {
        println!(
            "  track {} t0={} wall_ms={} points={}",
            track.session_id,
            track.t0_ms,
            track.wall_ms,
            track.points.len()
        );
    }
}
