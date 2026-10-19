#![cfg(unix)]

use cayw_workspace::{CancellationToken, Limits, Workspace};
use serde_json::json;
use std::fs;

#[tokio::test]
async fn whitelisting_a_directory_in_a_deeper_ignore_does_not_whitelist_its_files() {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir_all(temp.path().join("sub/kept")).unwrap();
    fs::write(temp.path().join(".gitignore"), "*.tmp\n").unwrap();
    fs::write(temp.path().join("sub/.gitignore"), "!kept/\n").unwrap();
    fs::write(temp.path().join("sub/kept/ignored.tmp"), "needle").unwrap();
    fs::write(temp.path().join("sub/kept/visible.rs"), "needle").unwrap();
    let ws = Workspace::open(temp.path().canonicalize().unwrap(), Limits::default()).unwrap();
    let result = ws
        .execute(
            "fs.glob",
            &json!({"path":".","pattern":"**/*.{tmp,rs}"}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(result["paths"], json!(["sub/kept/visible.rs"]));
    let result = ws
        .execute(
            "fs.grep",
            &json!({"path":"sub/kept/ignored.tmp","pattern":"needle"}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(result["matches"], json!([]));
}
