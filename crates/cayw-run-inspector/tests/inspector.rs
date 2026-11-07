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
    evidence: Option<u64>,
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
        evidence_cursor: evidence,
        error_summary: None,
    }
}

fn snapshot(session: &str, fingerprint: Option<&str>, steps: Vec<RunStep>) -> SessionRunSnapshot {
    SessionRunSnapshot {
        session_id: session.into(),
        label: Some(session.into()),
        task_fingerprint: fingerprint.map(str::to_owned),
        steps,
    }
}

#[test]
fn two_failed_retry_paths_are_distinguishable() {
    // Path 1: bash dig=A fails → same params retry fails again (same_params_retry).
    // Path 2: read dig=B fails → write dig=C succeeds (changed_tool).
    let snap = snapshot(
        "sess-a",
        Some("task-fix"),
        vec![
            step(
                "s1",
                "sess-a",
                1,
                0,
                "bash",
                "dig-a",
                StepOutcome::Failed,
                1_000,
                50,
                Some(10),
            ),
            step(
                "s2",
                "sess-a",
                1,
                1,
                "bash",
                "dig-a",
                StepOutcome::Failed,
                1_100,
                40,
                Some(11),
            ),
            step(
                "s3",
                "sess-a",
                2,
                0,
                "read",
                "dig-b",
                StepOutcome::Failed,
                2_000,
                20,
                Some(20),
            ),
            step(
                "s4",
                "sess-a",
                2,
                1,
                "write",
                "dig-c",
                StepOutcome::Success,
                2_050,
                30,
                Some(21),
            ),
        ],
    );

    let paths = build_retry_paths(&snap);
    assert!(
        paths.len() >= 2,
        "expected at least two failure heads, got {paths:?}"
    );

    let path_s1 = paths
        .iter()
        .find(|path| path.failure_step_id == "s1")
        .expect("s1 path");
    let path_s3 = paths
        .iter()
        .find(|path| path.failure_step_id == "s3")
        .expect("s3 path");

    assert_ne!(path_s1.path_id, path_s3.path_id);
    assert_eq!(path_s1.recovery, RecoveryKind::SameParamsRetry);
    assert_eq!(path_s3.recovery, RecoveryKind::ChangedTool);
    assert_eq!(path_s1.follow_up_step_ids, vec!["s2".to_string()]);
    assert_eq!(path_s3.follow_up_step_ids, vec!["s4".to_string()]);
    assert_eq!(path_s1.evidence_cursors, vec![10, 11]);
    assert_eq!(path_s3.evidence_cursors, vec![20, 21]);

    let timeline = build_run_timeline(&snap);
    assert_eq!(timeline.failure_count, 3); // s1, s2, s3
    assert_eq!(timeline.retry_paths.len(), paths.len());
    assert_eq!(timeline.track.points.len(), 4);
}

#[test]
fn session_a_b_timeline_aligns_on_shared_task() {
    let sess_a = snapshot(
        "A",
        Some("task-shared"),
        vec![
            step(
                "a1",
                "A",
                1,
                0,
                "read",
                "p1",
                StepOutcome::Success,
                5_000,
                10,
                Some(1),
            ),
            step(
                "a2",
                "A",
                1,
                1,
                "bash",
                "p2",
                StepOutcome::Failed,
                5_020,
                100,
                Some(2),
            ),
            step(
                "a3",
                "A",
                1,
                2,
                "bash",
                "p2b",
                StepOutcome::Success,
                5_130,
                50,
                Some(3),
            ),
            step(
                "a4",
                "A",
                2,
                0,
                "write",
                "p3",
                StepOutcome::Success,
                6_000,
                20,
                Some(4),
            ),
        ],
    );
    let sess_b = snapshot(
        "B",
        Some("task-shared"),
        vec![
            step(
                "b1",
                "B",
                1,
                0,
                "read",
                "p1",
                StepOutcome::Success,
                5_010,
                15,
                Some(101),
            ),
            step(
                "b2",
                "B",
                1,
                1,
                "bash",
                "p2",
                StepOutcome::Success,
                5_040,
                80,
                Some(102),
            ),
            step(
                "b3",
                "B",
                2,
                0,
                "write",
                "p3",
                StepOutcome::Success,
                5_900,
                25,
                Some(103),
            ),
            step(
                "b4",
                "B",
                3,
                0,
                "bash",
                "p4",
                StepOutcome::Empty,
                7_000,
                5,
                Some(104),
            ),
        ],
    );

    let compare = compare_sessions(&[sess_a, sess_b]).expect("compare");
    assert!(compare.same_task);
    assert_eq!(compare.session_ids, vec!["A".to_string(), "B".to_string()]);
    assert_eq!(compare.tracks.len(), 2);
    // Shared wall epoch = min(5000, 5010) = 5000
    assert_eq!(compare.tracks[0].t0_ms, 5_000);
    assert_eq!(compare.tracks[1].t0_ms, 5_000);
    assert_eq!(compare.tracks[0].points[0].offset_ms, 0);
    assert_eq!(compare.tracks[1].points[0].offset_ms, 10);

    let turns: Vec<u64> = compare.aligned_turns.iter().map(|t| t.turn).collect();
    assert_eq!(turns, vec![1, 2, 3]);

    let turn1 = &compare.aligned_turns[0];
    assert_eq!(turn1.lanes.len(), 2);
    let lane_a = turn1.lanes.iter().find(|l| l.session_id == "A").unwrap();
    let lane_b = turn1.lanes.iter().find(|l| l.session_id == "B").unwrap();
    assert_eq!(lane_a.branch_count, 1);
    assert_eq!(lane_b.branch_count, 0);
    assert_eq!(lane_a.elapsed_ms, 10 + 100 + 50);
    assert_eq!(lane_b.elapsed_ms, 15 + 80);

    let turn3 = compare
        .aligned_turns
        .iter()
        .find(|t| t.turn == 3)
        .expect("turn 3");
    assert_eq!(turn3.lanes.len(), 1);
    assert_eq!(turn3.lanes[0].session_id, "B");

    let paths_a = compare.retry_paths_by_session.get("A").unwrap();
    assert_eq!(paths_a.len(), 1);
    assert_eq!(paths_a[0].recovery, RecoveryKind::ChangedParams);
}

#[test]
fn differing_tasks_skip_turn_alignment() {
    let a = snapshot(
        "A",
        Some("task-1"),
        vec![step(
            "a1",
            "A",
            1,
            0,
            "read",
            "p",
            StepOutcome::Success,
            100,
            5,
            None,
        )],
    );
    let b = snapshot(
        "B",
        Some("task-2"),
        vec![step(
            "b1",
            "B",
            1,
            0,
            "read",
            "p",
            StepOutcome::Success,
            110,
            5,
            None,
        )],
    );
    let compare = compare_sessions(&[a, b]).unwrap();
    assert!(!compare.same_task);
    assert!(compare.aligned_turns.is_empty());
    assert_eq!(compare.tracks[0].t0_ms, 100);
    assert_eq!(compare.tracks[1].points[0].offset_ms, 10);
}

#[test]
fn unrecovered_failure_path() {
    let snap = snapshot(
        "solo",
        None,
        vec![step(
            "f1",
            "solo",
            1,
            0,
            "bash",
            "x",
            StepOutcome::Failed,
            0,
            10,
            Some(7),
        )],
    );
    let paths = build_retry_paths(&snap);
    assert_eq!(paths.len(), 1);
    assert_eq!(paths[0].recovery, RecoveryKind::Unrecovered);
    assert!(paths[0].follow_up_step_ids.is_empty());
    assert_eq!(paths[0].evidence_cursors, vec![7]);
    assert!(paths[0].recovery_elapsed_ms.is_none());
}

#[test]
fn compare_requires_two_sessions() {
    let snap = snapshot("only", None, vec![]);
    let err = compare_sessions(&[snap]).unwrap_err();
    assert!(matches!(err, InspectorError::Invalid(_)));
}

#[test]
fn session_comparison_never_mixes_cursors_and_preserves_origin_linkage() {
    let snap_a = snapshot(
        "sess-a",
        Some("shared-task"),
        vec![
            step(
                "a1",
                "sess-a",
                1,
                0,
                "read",
                "p1",
                StepOutcome::Failed,
                1_000,
                20,
                Some(10),
            ),
            step(
                "a2",
                "sess-a",
                1,
                1,
                "read",
                "p2",
                StepOutcome::Success,
                1_030,
                25,
                Some(11),
            ),
        ],
    );
    let snap_b = snapshot(
        "sess-b",
        Some("shared-task"),
        vec![
            step(
                "b1",
                "sess-b",
                1,
                0,
                "read",
                "p1",
                StepOutcome::Failed,
                1_005,
                30,
                Some(500),
            ),
            step(
                "b2",
                "sess-b",
                1,
                1,
                "write",
                "p3",
                StepOutcome::Success,
                1_040,
                35,
                Some(501),
            ),
        ],
    );

    let compare = compare_sessions(&[snap_a, snap_b]).expect("comparison");
    assert!(compare.same_task);

    // Verify session A retry paths only contain session A cursors and identity
    let paths_a = compare.retry_paths_by_session.get("sess-a").unwrap();
    assert_eq!(paths_a.len(), 1);
    assert_eq!(paths_a[0].session_id, "sess-a");
    assert_eq!(paths_a[0].failure_step_id, "a1");
    assert_eq!(paths_a[0].evidence_cursors, vec![10, 11]);

    // Verify session B retry paths only contain session B cursors and identity
    let paths_b = compare.retry_paths_by_session.get("sess-b").unwrap();
    assert_eq!(paths_b.len(), 1);
    assert_eq!(paths_b[0].session_id, "sess-b");
    assert_eq!(paths_b[0].failure_step_id, "b1");
    assert_eq!(paths_b[0].evidence_cursors, vec![500, 501]);

    // Ensure no cursors leaked across sessions
    for cursor in &paths_a[0].evidence_cursors {
        assert!(!paths_b[0].evidence_cursors.contains(cursor));
    }

    // Lanes in turn 1 clearly separate steps by session
    let turn1 = &compare.aligned_turns[0];
    let lane_a = turn1
        .lanes
        .iter()
        .find(|l| l.session_id == "sess-a")
        .unwrap();
    let lane_b = turn1
        .lanes
        .iter()
        .find(|l| l.session_id == "sess-b")
        .unwrap();
    assert_eq!(lane_a.step_ids, vec!["a1", "a2"]);
    assert_eq!(lane_b.step_ids, vec!["b1", "b2"]);
}
