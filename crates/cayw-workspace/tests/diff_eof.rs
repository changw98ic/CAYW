#![cfg(unix)]

use cayw_workspace::{CancellationToken, Limits, Workspace};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

async fn call(
    ws: &Workspace,
    diff: &str,
    max_hunks: usize,
) -> Result<Value, cayw_workspace::CapabilityError> {
    ws.execute(
        "code.diff_hunks",
        &json!({"diff":diff,"max_hunks":max_hunks}),
        CancellationToken::new(),
    )
    .await
}

// Prefixing a valid file puts every invalid case beyond the returned hunk limit.
async fn refuses_continuations(ws: &Workspace, cases: &[&str]) {
    for diff in cases {
        for prefix in ["", "--- a/first\n+++ b/first\n@@ -1 +1 @@\n-a\n+b\n"] {
            for max_hunks in [1, 1000] {
                let input = format!("{prefix}{diff}");
                let error = call(ws, &input, max_hunks)
                    .await
                    .expect_err("a file side cannot continue after its EOF marker");
                assert!(!error.outcome_unknown, "{input}");
            }
        }
    }
}

#[tokio::test]
async fn diff_eof_rejects_more_old_lines_in_same_hunk() {
    let temp = tempfile::tempdir().unwrap();
    let ws = Workspace::open(temp.path().canonicalize().unwrap(), Limits::default()).unwrap();
    refuses_continuations(
        &ws,
        &[
            "--- a/f\n+++ b/f\n@@ -1,2 +1,2 @@\n-a\n\\ No newline at end of file\n-b\n+x\n+y\n",
            "--- a/f\n+++ b/f\n@@ -1,2 +1 @@\n-a\n\\ No newline at end of file\n context\n",
            "--- a/f\n+++ b/f\n@@ -1,2 +1,2 @@\n-a\n\\ No newline at end of file\n+x\n+y\n-b\n",
        ],
    )
    .await;
}

#[tokio::test]
async fn diff_eof_rejects_more_new_lines_in_same_hunk() {
    let temp = tempfile::tempdir().unwrap();
    let ws = Workspace::open(temp.path().canonicalize().unwrap(), Limits::default()).unwrap();
    refuses_continuations(
        &ws,
        &[
            "--- a/f\n+++ b/f\n@@ -1 +1,2 @@\n-a\n+x\n\\ No newline at end of file\n+y\n",
            "--- a/f\n+++ b/f\n@@ -1 +1,2 @@\n+x\n\\ No newline at end of file\n context\n",
            "--- a/f\n+++ b/f\n@@ -1,2 +1,2 @@\n+x\n\\ No newline at end of file\n-a\n-b\n+y\n",
        ],
    )
    .await;
}

#[tokio::test]
async fn diff_eof_context_marker_ends_both_sides() {
    let temp = tempfile::tempdir().unwrap();
    let ws = Workspace::open(temp.path().canonicalize().unwrap(), Limits::default()).unwrap();
    refuses_continuations(
        &ws,
        &[
            "--- a/f\n+++ b/f\n@@ -1,2 +1,2 @@\n a\n\\ No newline at end of file\n-b\n+c\n",
            "--- a/f\n+++ b/f\n@@ -1,2 +1,2 @@\n a\n\\ No newline at end of file\n+c\n-b\n",
            "--- a/f\n+++ b/f\n@@ -1,2 +1,2 @@\n a\n\\ No newline at end of file\n context\n",
        ],
    )
    .await;
}

#[tokio::test]
async fn diff_eof_state_survives_later_hunks() {
    let temp = tempfile::tempdir().unwrap();
    let ws = Workspace::open(temp.path().canonicalize().unwrap(), Limits::default()).unwrap();
    refuses_continuations(
        &ws,
        &[
            "--- a/f\n+++ b/f\n@@ -1 +1 @@\n-a\n\\ No newline at end of file\n+x\n@@ -3 +3 @@\n-b\n+y\n",
            "--- a/f\n+++ b/f\n@@ -1 +1 @@\n-a\n+x\n\\ No newline at end of file\n@@ -3 +3 @@\n-b\n+y\n",
            "--- a/f\n+++ b/f\n@@ -1 +1 @@\n a\n\\ No newline at end of file\n@@ -3 +3 @@\n context\n",
            // A valid other-side-only hunk must not clear the old-side EOF state.
            "--- a/f\n+++ b/f\n@@ -1 +1 @@\n-a\n\\ No newline at end of file\n+x\n@@ -1,0 +2 @@\n+y\n@@ -2 +3,0 @@\n-b\n",
            // Empty ranges must still be anchored at the side's known EOF.
            "--- a/f\n+++ b/f\n@@ -1 +1 @@\n-a\n\\ No newline at end of file\n+x\n@@ -3,0 +4 @@\n+y\n",
            "--- a/f\n+++ b/f\n@@ -1 +1 @@\n-a\n+x\n\\ No newline at end of file\n@@ -4 +3,0 @@\n-b\n",
        ],
    )
    .await;
}

#[tokio::test]
async fn diff_eof_accepts_continuing_only_the_other_side() {
    let temp = tempfile::tempdir().unwrap();
    let ws = Workspace::open(temp.path().canonicalize().unwrap(), Limits::default()).unwrap();
    for (diff, total_hunks) in [
        (
            "--- a/f\n+++ b/f\n@@ -1 +1 @@\n-a\n\\ No newline at end of file\n+x\n\\ No newline at end of file\n",
            1,
        ),
        (
            "--- a/f\n+++ b/f\n@@ -1 +1,2 @@\n-a\n\\ No newline at end of file\n+x\n+y\n",
            1,
        ),
        (
            "--- a/f\n+++ b/f\n@@ -1,2 +1,2 @@\n-a\n-b\n\\ No newline at end of file\n+x\n+y\n",
            1,
        ),
        (
            "--- a/f\n+++ b/f\n@@ -1,2 +1 @@\n+x\n\\ No newline at end of file\n-a\n-b\n",
            1,
        ),
        (
            "--- a/f\n+++ b/f\n@@ -1 +1 @@\n a\n\\ No newline at end of file\n",
            1,
        ),
        (
            "--- a/f\n+++ b/f\n@@ -1 +1 @@\n-a\n\\ No newline at end of file\n+x\n@@ -1,0 +2 @@\n+y\n@@ -1,0 +3 @@\n+z\n\\ No newline at end of file\n",
            3,
        ),
        (
            "--- a/f\n+++ b/f\n@@ -1 +1 @@\n-a\n+x\n\\ No newline at end of file\n@@ -2 +1,0 @@\n-b\n@@ -3 +1,0 @@\n-c\n\\ No newline at end of file\n",
            3,
        ),
    ] {
        for max_hunks in [1, 1000] {
            let value = call(&ws, diff, max_hunks).await.unwrap();
            assert_eq!(value["total_hunks"], total_hunks, "{diff}");
            assert_eq!(value["validated_complete"], true);
            assert_eq!(value["truncated"], total_hunks > max_hunks);
            assert_eq!(
                value["hunks"].as_array().unwrap().len(),
                total_hunks.min(max_hunks)
            );
            assert_eq!(
                value["sha256"],
                format!("{:x}", Sha256::digest(diff.as_bytes()))
            );
        }
    }
}

#[tokio::test]
async fn diff_eof_resets_for_the_next_file() {
    let temp = tempfile::tempdir().unwrap();
    let ws = Workspace::open(temp.path().canonicalize().unwrap(), Limits::default()).unwrap();
    for separator in ["", "diff --git a/g b/g\nindex 123..456 100644\n"] {
        let diff = format!(
            "--- a/f\n+++ b/f\n@@ -1 +1 @@\n a\n\\ No newline at end of file\n{separator}--- a/g\n+++ b/g\n@@ -1 +1 @@\n-b\n+y\n"
        );
        for max_hunks in [1, 1000] {
            let value = call(&ws, &diff, max_hunks).await.unwrap();
            assert_eq!(value["total_hunks"], 2);
            assert_eq!(value["validated_complete"], true);
            assert_eq!(value["truncated"], max_hunks == 1);
            if max_hunks > 1 {
                assert_eq!(value["hunks"][1]["old_path"], "g");
                assert_eq!(value["hunks"][1]["new_path"], "g");
            }
        }
    }
}

#[tokio::test]
async fn diff_eof_preserves_crlf_unicode_bytes_lines_and_hash() {
    let temp = tempfile::tempdir().unwrap();
    let ws = Workspace::open(temp.path().canonicalize().unwrap(), Limits::default()).unwrap();
    let first = "@@ -1 +1 @@ λ\r\n-旧\r\n\\ No newline at end of file\r\n+新\r\n";
    let second = "@@ -1,0 +2 @@\r\n+尾\r\n\\ No newline at end of file";
    let diff = format!("--- a/f\r\n+++ b/f\r\n{first}{second}");
    let value = call(&ws, &diff, 1000).await.unwrap();
    assert_eq!(
        value["sha256"],
        format!("{:x}", Sha256::digest(diff.as_bytes()))
    );
    assert_eq!(value["validated_complete"], true);
    assert_eq!(value["total_hunks"], 2);
    assert_eq!(value["truncated"], false);
    let hunks = value["hunks"].as_array().unwrap();
    assert_eq!(
        hunks[0],
        json!({
            "old_path":"f","new_path":"f",
            "old":{"start_line":1,"line_count":1,"end_line_exclusive":2},
            "new":{"start_line":1,"line_count":1,"end_line_exclusive":2},
            "header_line":3,"header_byte_start":18,"header_byte_end":34,
            "body_start_line":4,"body_end_line_exclusive":7,"byte_start":18,"byte_end":75
        })
    );
    assert_eq!(
        hunks[1],
        json!({
            "old_path":"f","new_path":"f",
            "old":{"start_line":1,"line_count":0,"end_line_exclusive":1},
            "new":{"start_line":2,"line_count":1,"end_line_exclusive":3},
            "header_line":7,"header_byte_start":75,"header_byte_end":90,
            "body_start_line":8,"body_end_line_exclusive":10,"byte_start":75,"byte_end":123
        })
    );
    assert_eq!(&diff[18..75], first);
    assert_eq!(&diff[75..123], second);
    let limited = call(&ws, &diff, 1).await.unwrap();
    assert_eq!(limited["hunks"], json!([hunks[0]]));
    assert_eq!(limited["sha256"], value["sha256"]);
    assert_eq!(limited["total_hunks"], 2);
    assert_eq!(limited["validated_complete"], true);
    assert_eq!(limited["truncated"], true);
}

#[tokio::test]
async fn diff_eof_keeps_marker_placement_strict() {
    let temp = tempfile::tempdir().unwrap();
    let ws = Workspace::open(temp.path().canonicalize().unwrap(), Limits::default()).unwrap();
    refuses_continuations(
        &ws,
        &[
            "--- a/f\n+++ b/f\n@@ -1 +1 @@\n\\ No newline at end of file\n-a\n+b\n",
            "--- a/f\n+++ b/f\n@@ -1 +1 @@\n-a\n\\ No newline at end of file\n\\ No newline at end of file\n+b\n",
            "--- a/f\n+++ b/f\n@@ -1 +1 @@\n-a\n+b\n\\ No newline at end of file\n\\ No newline at end of file\n",
            "--- a/f\n+++ b/f\n@@ -1 +1 @@\n-a\n+b\n\\ No newline at end of file \n",
        ],
    )
    .await;
}
