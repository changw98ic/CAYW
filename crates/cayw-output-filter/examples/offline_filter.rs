//! Offline demo: filter on vs off for the same shell-like capture.

use cayw_output_filter::{
    FilterOptions, FilterProfile, TaskRunCost, apply_filter, compare_filter_toggle, dual_entry,
};

fn main() {
    let raw = concat!(
        "\u{1b}[32mCompiling\u{1b}[0m demo v0.1.0\n",
        "noise: building dependency graph...\n",
        "noise: checking 120 crates\n",
        "error[E0308]: mismatched types\n",
        "  --> src/main.rs:10:5\n",
        "warning: unused variable `x`\n",
        "noise: link step\n",
        "Finished `test` profile [unoptimized] target(s) in 1.23s\n",
        "test result: FAILED. 1 passed; 1 failed\n",
    );

    let on = apply_filter(
        raw,
        FilterOptions {
            enabled: true,
            profile: FilterProfile::CargoLike,
            head_lines: 2,
            tail_lines: 4,
            max_lines: 32,
        },
    )
    .expect("filter on");
    let off = apply_filter(
        raw,
        FilterOptions {
            enabled: false,
            profile: FilterProfile::CargoLike,
            ..FilterOptions::default()
        },
    )
    .expect("filter off");

    let dual = dual_entry(&on);
    println!("digest={}", dual.original_digest);
    println!("--- summary ---");
    print!("{}", dual.summary);
    println!("--- original (bytes={}) ---", dual.original.len());
    println!("reduction_bps={:?}", on.reduction.reduction_bps());

    let report = compare_filter_toggle(
        "demo-shell-task",
        &on,
        TaskRunCost {
            wall_ms: 1400,
            cost_micros: None,
            failure_reread_count: 1,
        },
        &off,
        TaskRunCost {
            wall_ms: 1500,
            cost_micros: None,
            failure_reread_count: 0,
        },
    )
    .expect("compare");
    println!("disclaimer={}", report.disclaimer);
    println!(
        "on_bytes={} off_bytes={}",
        report.with_filter.reduction.filtered_bytes, report.without_filter.reduction.filtered_bytes
    );
}
