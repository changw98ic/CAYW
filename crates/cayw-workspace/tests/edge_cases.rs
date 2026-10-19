#![cfg(unix)]

use cayw_workspace::{CancellationToken, Limits, Workspace};
use serde_json::{Value, json};
use std::{fs, time::Duration};

async fn call(
    ws: &Workspace,
    name: &str,
    args: Value,
) -> Result<Value, cayw_workspace::CapabilityError> {
    ws.execute(name, &args, CancellationToken::new()).await
}

#[tokio::test]
async fn outlined_ranges_preserve_bom_and_shebang_offsets() {
    let temp = tempfile::tempdir().unwrap();
    let ws = Workspace::open(temp.path().canonicalize().unwrap(), Limits::default()).unwrap();
    for source in [
        "\u{feff}fn α() {}",
        "#!/usr/bin/env rust-script\nfn α() {}",
        "\u{feff}#!/usr/bin/env rust-script\nfn α() {}",
    ] {
        fs::write(temp.path().join("a.rs"), source).unwrap();
        let result = call(
            &ws,
            "code.outline",
            json!({"path":"a.rs","language":"rust"}),
        )
        .await
        .unwrap();
        let symbol = &result["symbols"][0];
        let begin = symbol["byte_start"].as_u64().unwrap() as usize;
        let end = symbol["byte_end"].as_u64().unwrap() as usize;
        assert_eq!(&source[begin..end], "fn α() {}");
    }
}

#[tokio::test]
async fn regex_empty_matches_are_utf8_aligned_and_not_adjacent_to_nonempty_match() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("a"), "α").unwrap();
    let ws = Workspace::open(temp.path().canonicalize().unwrap(), Limits::default()).unwrap();
    let result = call(&ws, "fs.grep", json!({"path":"a","pattern":""}))
        .await
        .unwrap();
    assert_eq!(
        result["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["byte_start"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        [0, 2]
    );
    let result = call(&ws, "fs.grep", json!({"path":"a","pattern":"α|$"}))
        .await
        .unwrap();
    assert_eq!(result["matches"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn gitignore_links_or_oversized_rules_fail_without_ambient_reads() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("rules"), "ok\n").unwrap();
    fs::write(temp.path().join("ok"), "hi").unwrap();
    symlink(outside.path().join("rules"), temp.path().join(".gitignore")).unwrap();
    let ws = Workspace::open(temp.path().canonicalize().unwrap(), Limits::default()).unwrap();
    assert!(
        !call(&ws, "fs.glob", json!({"path":".","pattern":"**/*"}))
            .await
            .unwrap_err()
            .outcome_unknown
    );
    let explicit = call(
        &ws,
        "fs.glob",
        json!({"path":".","pattern":"**/*","respect_gitignore":false}),
    )
    .await
    .unwrap();
    assert_eq!(explicit["paths"], json!(["ok"]));
    fs::remove_file(temp.path().join(".gitignore")).unwrap();
    fs::write(temp.path().join(".gitignore"), "x".repeat(5000)).unwrap();
    assert!(
        !call(&ws, "fs.glob", json!({"path":".","pattern":"**/*"}))
            .await
            .unwrap_err()
            .outcome_unknown
    );
}

#[tokio::test]
async fn cancellation_during_multifile_scan_is_observed_at_a_checkpoint() {
    let temp = tempfile::tempdir().unwrap();
    for i in 0..50 {
        fs::write(temp.path().join(format!("f{i:02}")), vec![b'x'; 128 * 1024]).unwrap();
    }
    let ws = Workspace::open(temp.path().canonicalize().unwrap(), Limits::default()).unwrap();
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    let thread = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(2));
        trigger.cancel();
    });
    let err = ws
        .execute("fs.grep", &json!({"path":".","pattern":"z"}), cancel)
        .await
        .unwrap_err();
    thread.join().unwrap();
    assert!(err.message.contains("cancel"));
    assert!(!err.outcome_unknown);
}

#[tokio::test]
async fn diff_crlf_positions_are_measured_in_original_bytes() {
    let temp = tempfile::tempdir().unwrap();
    let ws = Workspace::open(temp.path().canonicalize().unwrap(), Limits::default()).unwrap();
    let diff = "--- a/f\r\n+++ b/f\r\n@@ -1 +1 @@\r\n-a\r\n+b\r\n";
    let result = call(&ws, "code.diff_hunks", json!({"diff":diff}))
        .await
        .unwrap();
    let hunk = &result["hunks"][0];
    assert_eq!(hunk["header_byte_start"], 18);
    assert_eq!(hunk["byte_end"], diff.len());
}
