use cayw_output_filter::{
    FilterOptions, FilterProfile, OUTPUT_FILTER_PROTOCOL_VERSION, TaskRunCost, apply_filter,
    builtin_profile, compare_filter_toggle, dual_entry,
};

#[test]
fn protocol_version_stable() {
    assert_eq!(OUTPUT_FILTER_PROTOCOL_VERSION, 1);
}

#[test]
fn same_shell_task_on_off_dual_entry() {
    let raw = {
        let mut s = String::new();
        for i in 0..80 {
            s.push_str(&format!("build step {i}\n"));
        }
        s.push_str("error: link failed\n");
        s.push_str("note: see log above\n");
        for i in 80..120 {
            s.push_str(&format!("cleanup {i}\n"));
        }
        s
    };

    let on = apply_filter(
        &raw,
        FilterOptions {
            enabled: true,
            profile: FilterProfile::Generic,
            head_lines: 3,
            tail_lines: 3,
            max_lines: 24,
        },
    )
    .unwrap();
    let off = apply_filter(
        &raw,
        FilterOptions {
            enabled: false,
            profile: FilterProfile::Generic,
            head_lines: 3,
            tail_lines: 3,
            max_lines: 24,
        },
    )
    .unwrap();

    assert_eq!(on.original_digest, off.original_digest);
    assert_eq!(on.original, raw);
    assert_eq!(off.filtered, raw);
    assert!(on.filtered.contains("error: link failed"));
    assert!(on.reduction.filtered_bytes < on.reduction.original_bytes);

    let dual = dual_entry(&on);
    assert_eq!(dual.summary, on.filtered);
    assert_eq!(dual.original, on.original);

    let report = compare_filter_toggle(
        "shell-fingerprint",
        &on,
        TaskRunCost {
            wall_ms: 200,
            cost_micros: Some(1000),
            failure_reread_count: 2,
        },
        &off,
        TaskRunCost {
            wall_ms: 180,
            cost_micros: Some(900),
            failure_reread_count: 0,
        },
    )
    .unwrap();
    assert!(report.disclaimer.contains("not_task_benefit"));
    assert_ne!(
        report.with_filter.reduction.filtered_bytes,
        report.without_filter.reduction.filtered_bytes
    );
    // Smaller output must not be treated as automatic task win: costs are independent fields.
    assert!(report.with_filter.cost.wall_ms > report.without_filter.cost.wall_ms);
}

#[test]
fn git_status_like_keeps_porcelain() {
    let raw = "On branch main\nChanges not staged for commit:\n M src/a.rs\n?? new.txt\nspurious chatter\nmore chatter\n";
    let r = apply_filter(
        raw,
        FilterOptions {
            enabled: true,
            profile: builtin_profile("git"),
            head_lines: 1,
            tail_lines: 0,
            max_lines: 20,
        },
    )
    .unwrap();
    assert!(r.filtered.contains("On branch main"));
    assert!(r.filtered.contains(" M src/a.rs") || r.filtered.contains("?? new.txt"));
}

#[test]
fn cargo_like_keeps_test_result() {
    let raw = "Compiling foo v0.1.0\nlots of noise\nerror: test failed\ntest result: FAILED. 0 passed; 1 failed\n";
    let r = apply_filter(
        raw,
        FilterOptions {
            enabled: true,
            profile: FilterProfile::CargoLike,
            head_lines: 1,
            tail_lines: 2,
            max_lines: 16,
        },
    )
    .unwrap();
    assert!(r.filtered.contains("error: test failed"));
    assert!(r.filtered.contains("test result:"));
}

#[test]
fn compare_rejects_digest_mismatch() {
    let a = apply_filter(
        "a",
        FilterOptions {
            enabled: true,
            ..FilterOptions::default()
        },
    )
    .unwrap();
    let b = apply_filter(
        "b",
        FilterOptions {
            enabled: false,
            ..FilterOptions::default()
        },
    )
    .unwrap();
    let err = compare_filter_toggle(
        "x",
        &a,
        TaskRunCost {
            wall_ms: 1,
            cost_micros: None,
            failure_reread_count: 0,
        },
        &b,
        TaskRunCost {
            wall_ms: 1,
            cost_micros: None,
            failure_reread_count: 0,
        },
    )
    .unwrap_err();
    assert!(err.to_string().contains("original_digest"));
}

#[test]
fn passthrough_preserves_git_blame_porcelain_that_generic_splits() {
    // Two-line line-porcelain (same shape Runtime git fixture produces).
    let raw = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 1 1 2
author Fixture
author-mail <fixture@example.invalid>
author-time 1700000000
author-tz +0000
committer Fixture
committer-mail <fixture@example.invalid>
committer-time 1700000000
committer-tz +0000
summary fixture
boundary
filename note
	alpha
aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 2 2
author Fixture
author-mail <fixture@example.invalid>
author-time 1700000000
author-tz +0000
committer Fixture
committer-mail <fixture@example.invalid>
committer-time 1700000000
committer-tz +0000
summary fixture
boundary
filename note
	beta
";
    let generic = apply_filter(raw, FilterOptions::default()).unwrap();
    assert!(
        !(generic.filtered.contains("alpha") && generic.filtered.contains("beta")),
        "default Generic head/tail must demonstrably drop a blamed content line: {}",
        generic.filtered
    );

    let passthrough = apply_filter(
        raw,
        FilterOptions {
            enabled: true,
            profile: FilterProfile::Passthrough,
            head_lines: 8,
            tail_lines: 12,
            max_lines: 0,
        },
    )
    .unwrap();
    assert!(passthrough.filtered.contains("alpha"));
    assert!(passthrough.filtered.contains("beta"));
    assert!(passthrough.filtered.contains("author Fixture"));
    assert!(passthrough.filtered.contains("filename note"));
    // dual_entry still retains original for offline compare; Host must not
    // re-embed it in the capability JSON.
    let dual = dual_entry(&passthrough);
    assert_eq!(dual.original, raw);
    assert_eq!(dual.summary, passthrough.filtered);
}
