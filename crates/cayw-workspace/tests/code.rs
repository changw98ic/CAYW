#![cfg(unix)]

use cayw_workspace::{CancellationToken, Limits, Workspace};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs;

async fn call(
    ws: &Workspace,
    name: &str,
    args: Value,
) -> Result<Value, cayw_workspace::CapabilityError> {
    ws.execute(name, &args, CancellationToken::new()).await
}

// A text approximation would accept the malformed Rust and mislocate the raw
// identifier after Unicode. A top-level-only parser would miss the methods.
#[tokio::test]
async fn outline_parses_nested_rust_symbols_and_exact_unicode_byte_ranges() {
    let temp = tempfile::tempdir().unwrap();
    let source = "// λ\nmod nested {\n    pub struct Café;\n    impl Café { pub fn r#type(&self) {} }\n    trait T { fn café(&self); }\n    mod external;\n}\n";
    fs::write(temp.path().join("lib.rs"), source).unwrap();
    let ws = Workspace::open(temp.path().canonicalize().unwrap(), Limits::default()).unwrap();
    let result = call(
        &ws,
        "code.outline",
        json!({"path":"lib.rs","language":"rust"}),
    )
    .await
    .unwrap();
    assert_eq!(result["language"], "rust");
    assert_eq!(
        result["sha256"],
        format!("{:x}", Sha256::digest(source.as_bytes()))
    );
    let symbols = result["symbols"].as_array().unwrap();
    for (name, kind, parent) in [
        ("nested", "module", None),
        ("Café", "struct", Some(0)),
        ("Café", "impl", Some(0)),
        ("r#type", "function", Some(2)),
        ("T", "trait", Some(0)),
        ("café", "function", Some(4)),
        ("external", "module", Some(0)),
    ] {
        let symbol = &symbols[if name == "nested" {
            0
        } else if name == "Café" && kind == "struct" {
            1
        } else if kind == "impl" {
            2
        } else if name == "r#type" {
            3
        } else if name == "T" {
            4
        } else if name == "café" {
            5
        } else {
            6
        }];
        assert_eq!(symbol["name"], name);
        assert_eq!(symbol["kind"], kind);
        assert_eq!(symbol["parent"], parent.map_or(Value::Null, |i| json!(i)));
        let start = symbol["byte_start"].as_u64().unwrap() as usize;
        let end = symbol["byte_end"].as_u64().unwrap() as usize;
        assert!(source[start..end].contains(name));
    }
    let raw = &symbols[3];
    assert_eq!(
        &source[raw["byte_start"].as_u64().unwrap() as usize
            ..raw["byte_end"].as_u64().unwrap() as usize],
        "pub fn r#type(&self) {}"
    );
    assert_eq!(raw["start_line"], 4);
    assert_eq!(raw["end_line"], 4);
    assert_eq!(result["truncated"], false);
    let short = call(
        &ws,
        "code.outline",
        json!({"path":"lib.rs","language":"rust","max_symbols":2}),
    )
    .await
    .unwrap();
    assert_eq!(short["symbols"].as_array().unwrap().len(), 2);
    assert_eq!(short["truncated"], true);
    fs::write(temp.path().join("bad.rs"), "fn broken( {").unwrap();
    let err = call(
        &ws,
        "code.outline",
        json!({"path":"bad.rs","language":"rust"}),
    )
    .await
    .unwrap_err();
    assert!(err.message.contains("Rust syntax"));
    assert!(!err.outcome_unknown);
    assert!(
        call(
            &ws,
            "code.outline",
            json!({"path":"lib.rs","language":"python"})
        )
        .await
        .is_err()
    );
}

// Wrong default counts, inclusion of +/- headers in body counters, or trusting
// the hunk header without checking its body changes these literal spans.
#[tokio::test]
async fn diff_hunks_validates_headers_body_and_insertion_ranges() {
    let temp = tempfile::tempdir().unwrap();
    let ws = Workspace::open(temp.path().canonicalize().unwrap(), Limits::default()).unwrap();
    let diff = "diff --git a/src/a.rs b/src/a.rs\nindex 123..456 100644\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1,2 +1,3 @@ fn main\n same\n-old\n+new\n+extra\n@@ -5 +6 @@\n-x\n+y\n--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1 @@\n+new\n\\ No newline at end of file\n";
    let result = call(&ws, "code.diff_hunks", json!({"diff":diff}))
        .await
        .unwrap();
    let hunks = result["hunks"].as_array().unwrap();
    assert_eq!(hunks.len(), 3);
    assert_eq!(hunks[0]["old_path"], "src/a.rs");
    assert_eq!(hunks[0]["new_path"], "src/a.rs");
    assert_eq!(
        hunks[0]["old"],
        json!({"start_line":1,"line_count":2,"end_line_exclusive":3})
    );
    assert_eq!(
        hunks[0]["new"],
        json!({"start_line":1,"line_count":3,"end_line_exclusive":4})
    );
    assert_eq!(hunks[0]["header_line"], 5);
    assert_eq!(hunks[0]["body_start_line"], 6);
    assert_eq!(hunks[0]["body_end_line_exclusive"], 10);
    assert_eq!(hunks[1]["old"]["line_count"], 1);
    assert_eq!(hunks[2]["old_path"], Value::Null);
    assert_eq!(hunks[2]["old"]["start_line"], 0);
    assert_eq!(hunks[2]["old"]["line_count"], 0);
    assert_eq!(result["truncated"], false);
    assert_eq!(
        result["sha256"],
        format!("{:x}", Sha256::digest(diff.as_bytes()))
    );
    let limited = call(&ws, "code.diff_hunks", json!({"diff":diff,"max_hunks":1}))
        .await
        .unwrap();
    assert_eq!(limited["hunks"].as_array().unwrap().len(), 1);
    assert_eq!(limited["truncated"], true);
    assert_eq!(limited["total_hunks"], 3);
}

#[tokio::test]
async fn diff_hunks_enforces_line_and_output_bounds_and_pre_cancel() {
    let temp = tempfile::tempdir().unwrap();
    let diff = "--- a/f\n+++ b/f\n@@ -1 +1 @@\n-a\n+b\n";
    let line_limited = Workspace::open(
        temp.path().canonicalize().unwrap(),
        Limits {
            max_tree_entries: 4,
            ..Limits::default()
        },
    )
    .unwrap();
    let error = call(&line_limited, "code.diff_hunks", json!({"diff":diff}))
        .await
        .unwrap_err();
    assert!(!error.outcome_unknown);
    assert!(
        error.message.contains("diff input line limit exceeded"),
        "{error}"
    );

    let output_limited = Workspace::open(
        temp.path().canonicalize().unwrap(),
        Limits {
            max_output_bytes: 24,
            ..Limits::default()
        },
    )
    .unwrap();
    let error = call(&output_limited, "code.diff_hunks", json!({"diff":diff}))
        .await
        .unwrap_err();
    assert!(!error.outcome_unknown);
    assert!(error.message.contains("JSON byte budget"), "{error}");

    let cancel = CancellationToken::new();
    cancel.cancel();
    let error = line_limited
        .execute("code.diff_hunks", &json!({"diff":diff}), cancel)
        .await
        .unwrap_err();
    assert!(!error.outcome_unknown);
    assert!(error.message.contains("cancelled"), "{error}");
}

#[tokio::test]
async fn diff_hunks_rejects_malformed_counts_order_paths_and_trailing_body() {
    let temp = tempfile::tempdir().unwrap();
    let ws = Workspace::open(temp.path().canonicalize().unwrap(), Limits::default()).unwrap();
    for diff in [
        "--- a/f\n+++ b/f\n@@ -1,2 +1 @@\n-a\n+b\n",
        "--- a/f\n+++ b/f\n@@ -18446744073709551615 +1 @@\n-a\n+b\n",
        "--- a/f\n+++ b/f\n@@ -0 +1 @@\n-a\n+b\n",
        "--- a/f\n+++ b/f\n@@ -1 +1 @@\n-a\n+b\n+extra\n",
        "--- a/../secret\n+++ b/f\n@@ -1 +1 @@\n-a\n+b\n",
        "--- a/f\n+++ b/f\n@@ -1 +1 @@\n-a\n+b\n@@ -1 +1 @@\n-a\n+b\n",
        "--- a/f\n+++ b/f\n@@ -1 +1 @@\n-a\n+b\n@@ broken\n",
    ] {
        let err = call(&ws, "code.diff_hunks", json!({"diff":diff,"max_hunks":1}))
            .await
            .unwrap_err();
        assert!(!err.outcome_unknown, "{diff}");
    }
}
