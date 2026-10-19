#![cfg(unix)]

use cayw_workspace::{CancellationToken, Limits, Workspace};
use serde_json::json;
use std::fs;

#[tokio::test]
async fn explicit_subroots_charge_ancestor_entries_before_reading_files() {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir_all(temp.path().join("a/b")).unwrap();
    fs::write(temp.path().join("a/b/f"), "x").unwrap();
    let ws = Workspace::open(
        temp.path().canonicalize().unwrap(),
        Limits {
            max_tree_entries: 2,
            ..Limits::default()
        },
    )
    .unwrap();
    for (name, args) in [
        ("fs.grep", json!({"path":"a/b/f","pattern":"x"})),
        ("fs.glob", json!({"path":"a/b","pattern":"*"})),
    ] {
        let err = ws
            .execute(name, &args, CancellationToken::new())
            .await
            .unwrap_err();
        assert!(err.message.contains("entry limit"));
        assert!(!err.outcome_unknown);
    }
}

#[tokio::test]
async fn binary_ignore_input_is_refused_rather_than_hiding_unscanned_files() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join(".gitignore"), b"x\0\n").unwrap();
    fs::write(temp.path().join("a"), "x").unwrap();
    let ws = Workspace::open(temp.path().canonicalize().unwrap(), Limits::default()).unwrap();
    let result = ws
        .execute(
            "fs.glob",
            &json!({"path":".","pattern":"*"}),
            CancellationToken::new(),
        )
        .await;
    assert!(result.is_err());
    assert!(!result.unwrap_err().outcome_unknown);
}
