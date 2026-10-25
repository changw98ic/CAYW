#![cfg(unix)]

use cayw_workspace::{CancellationToken, CapabilityError, Limits, Workspace};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{fs, time::Instant};
use tempfile::TempDir;

fn workspace(temp: &TempDir, limits: Limits) -> Workspace {
    // Host retries this same busy window: a concurrent fork can briefly inherit
    // unrelated lock fds until exec (see Host::open_workspace). Tests must too.
    let root = temp.path().canonicalize().unwrap();
    let deadline = Instant::now() + std::time::Duration::from_secs(2);
    loop {
        match Workspace::open(&root, limits.clone()) {
            Ok(workspace) => return workspace,
            Err(error)
                if !error.outcome_unknown
                    && error.message.starts_with("workspace is busy:")
                    && Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(error) => panic!("workspace open failed: {error:?}"),
        }
    }
}
async fn call(workspace: &Workspace, name: &str, args: Value) -> Result<Value, CapabilityError> {
    workspace
        .execute(name, &args, CancellationToken::new())
        .await
}
fn paths(result: &Value) -> Vec<&str> {
    result["paths"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect()
}

// Missing ancestor ignore loading, incorrect negation precedence, or an ambient
// walk would expose one of the hand-listed excluded paths.
#[tokio::test]
async fn glob_honors_nested_gitignore_negation_parent_and_root_rules() {
    let temp = tempfile::tempdir().unwrap();
    for dir in [
        "src/deep",
        "blocked",
        "hidden",
        ".git",
        ".cayw-host-tmp-private",
    ] {
        fs::create_dir_all(temp.path().join(dir)).unwrap();
    }
    fs::write(
        temp.path().join(".gitignore"),
        "*.tmp\n/root.rs\nblocked/\nhidden/*\n!hidden/keep.rs\n",
    )
    .unwrap();
    fs::write(
        temp.path().join("src/.gitignore"),
        "!keep.tmp\n/deep/drop.rs\n",
    )
    .unwrap();
    for file in [
        "root.rs",
        "ok.rs",
        "drop.tmp",
        "src/root.rs",
        "src/keep.tmp",
        "src/drop.tmp",
        "src/deep/drop.rs",
        "src/deep/yes.rs",
        "blocked/keep.rs",
        "hidden/drop.rs",
        "hidden/keep.rs",
        ".git/secret.rs",
        ".cayw-host-tmp-private/secret.rs",
    ] {
        fs::write(temp.path().join(file), "match").unwrap();
    }
    fs::write(temp.path().join("blocked/.gitignore"), "!keep.rs\n").unwrap();
    let ws = workspace(&temp, Limits::default());
    let result = call(
        &ws,
        "fs.glob",
        json!({"path":".","pattern":"**/*.{rs,tmp}"}),
    )
    .await
    .unwrap();
    assert_eq!(
        paths(&result),
        [
            "hidden/keep.rs",
            "ok.rs",
            "src/deep/yes.rs",
            "src/keep.tmp",
            "src/root.rs"
        ]
    );
    assert_eq!(result["scan_complete"], true);
    assert_eq!(result["truncated"], false);
    let sub = call(&ws, "fs.glob", json!({"path":"src","pattern":"**/*"}))
        .await
        .unwrap();
    assert_eq!(
        paths(&sub),
        [
            "src/.gitignore",
            "src/deep/yes.rs",
            "src/keep.tmp",
            "src/root.rs"
        ]
    );
    let all = call(
        &ws,
        "fs.glob",
        json!({"path":".","pattern":"**/[kr]*.rs","respect_gitignore":false}),
    )
    .await
    .unwrap();
    assert_eq!(
        paths(&all),
        [
            "blocked/keep.rs",
            "hidden/keep.rs",
            "root.rs",
            "src/root.rs"
        ]
    );
}

// Line-oriented matching or character offsets in byte fields would produce
// wrong hand-checked positions for the multibyte input.
#[tokio::test]
async fn grep_matches_full_utf8_files_with_exact_offsets_hash_and_context() {
    let temp = tempfile::tempdir().unwrap();
    let source = "before\nα start\nβ end\nafter\n";
    fs::write(temp.path().join("a.txt"), source).unwrap();
    fs::write(temp.path().join("binary"), [0, 1, 2]).unwrap();
    fs::write(temp.path().join("nonutf8"), [0xff, 0xfe]).unwrap();
    let ws = workspace(&temp, Limits::default());
    let result = call(&ws, "fs.grep", json!({"path":".","pattern":"^α.*β end$","multiline":true,"dot_matches_new_line":true,"context_lines":1})).await.unwrap();
    let m = &result["matches"][0];
    assert_eq!(m["path"], "a.txt");
    assert_eq!(m["text"], "α start\nβ end");
    assert_eq!(m["byte_start"], 7);
    assert_eq!(m["byte_end"], 22);
    assert_eq!(m["start_line"], 2);
    assert_eq!(m["end_line"], 3);
    assert_eq!(
        m["sha256"],
        format!("{:x}", Sha256::digest(source.as_bytes()))
    );
    assert_eq!(
        m["context"],
        json!({"start_line":1,"end_line":4,"text":source})
    );
    assert_eq!(result["skipped"].as_array().unwrap().len(), 2);
    assert_eq!(result["scan_complete"], true);
    let literal = call(
        &ws,
        "fs.grep",
        json!({"path":"a.txt","pattern":"START\nβ","literal":true,"case_insensitive":true}),
    )
    .await
    .unwrap();
    assert_eq!(literal["matches"][0]["byte_start"], 10);
}

#[tokio::test]
async fn grep_and_glob_share_ignore_defaults_and_report_matching_truncation() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join(".gitignore"), "ignored.txt\n").unwrap();
    fs::write(temp.path().join("ignored.txt"), "x x x").unwrap();
    fs::write(temp.path().join("a.txt"), "x x x").unwrap();
    fs::write(temp.path().join("b.txt"), "x").unwrap();
    let ws = workspace(&temp, Limits::default());
    let args = json!({"path":".","pattern":"\\bx\\b","max_matches":2});
    let a = call(&ws, "fs.grep", args.clone()).await.unwrap();
    let b = call(&ws, "fs.grep", args).await.unwrap();
    assert_eq!(a, b);
    assert_eq!(a["matches"].as_array().unwrap().len(), 2);
    assert_eq!(a["truncated"], true);
    assert_eq!(a["scan_complete"], false);
    assert!(
        a["matches"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["path"] == "a.txt")
    );
    let ignored = call(&ws, "fs.grep", json!({"path":"ignored.txt","pattern":"x"}))
        .await
        .unwrap();
    assert_eq!(ignored["matches"], json!([]));
    let explicit = call(
        &ws,
        "fs.grep",
        json!({"path":"ignored.txt","pattern":"x","respect_gitignore":false}),
    )
    .await
    .unwrap();
    assert_eq!(explicit["matches"].as_array().unwrap().len(), 3);
    let glob = call(
        &ws,
        "fs.glob",
        json!({"path":".","pattern":"**/*.txt","max_results":1}),
    )
    .await
    .unwrap();
    assert_eq!(paths(&glob), ["a.txt"]);
    assert_eq!(glob["truncated"], true);
}

#[tokio::test]
async fn links_special_entries_and_protected_roots_are_never_followed() {
    use std::os::unix::{fs::symlink, net::UnixListener};
    let temp = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("secret"), "secret").unwrap();
    symlink(outside.path(), temp.path().join("link-dir")).unwrap();
    symlink(outside.path().join("secret"), temp.path().join("link-file")).unwrap();
    let _socket = UnixListener::bind(temp.path().join("socket")).unwrap();
    assert!(
        std::process::Command::new("mkfifo")
            .arg(temp.path().join("fifo"))
            .status()
            .unwrap()
            .success()
    );
    fs::create_dir(temp.path().join(".git")).unwrap();
    fs::write(temp.path().join(".git/secret"), "secret").unwrap();
    fs::write(temp.path().join("ok"), "ok").unwrap();
    let ws = workspace(&temp, Limits::default());
    for name in ["fs.glob", "fs.grep"] {
        let pattern = if name == "fs.glob" { "*" } else { ".*" };
        let result = call(
            &ws,
            name,
            json!({"path":".","pattern":pattern,"respect_gitignore":false}),
        )
        .await
        .unwrap();
        assert_eq!(result["skipped"].as_array().unwrap().len(), 4);
        assert_eq!(result["protected_entries_omitted"], true);
        if name == "fs.glob" {
            assert_eq!(paths(&result), ["ok"]);
        }
        for bad in ["link-dir", "link-file", "fifo", ".git", "../secret"] {
            assert!(
                !call(&ws, name, json!({"path":bad,"pattern":pattern}))
                    .await
                    .unwrap_err()
                    .outcome_unknown
            );
        }
    }
}

#[tokio::test]
async fn held_root_search_is_not_redirected_after_rename() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("root");
    let moved = temp.path().join("moved");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("old.txt"), "old").unwrap();
    let ws = Workspace::open(root.canonicalize().unwrap(), Limits::default()).unwrap();
    fs::rename(&root, &moved).unwrap();
    fs::create_dir(&root).unwrap();
    fs::write(root.join("new.txt"), "new").unwrap();
    let result = call(&ws, "fs.glob", json!({"path":".","pattern":"**/*"}))
        .await
        .unwrap();
    assert_eq!(paths(&result), ["old.txt"]);
    let result = call(&ws, "fs.grep", json!({"path":".","pattern":"old"}))
        .await
        .unwrap();
    assert_eq!(result["matches"][0]["path"], "old.txt");
}

#[tokio::test]
async fn search_limits_invalid_patterns_and_cancellation_are_known_failures() {
    for limits in [
        Limits {
            max_tree_entries: 1,
            ..Limits::default()
        },
        Limits {
            max_file_bytes: 4,
            ..Limits::default()
        },
        Limits {
            max_batch_bytes: 6,
            ..Limits::default()
        },
    ] {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("a"), "12345").unwrap();
        fs::write(temp.path().join("b"), "12345").unwrap();
        let ws = workspace(&temp, limits);
        assert!(
            !call(&ws, "fs.grep", json!({"path":".","pattern":"1"}))
                .await
                .unwrap_err()
                .outcome_unknown
        );
    }
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("a"), "12345").unwrap();
    fs::write(temp.path().join("b"), "12345").unwrap();
    let ws = workspace(&temp, Limits::default());
    for pattern in [
        "[".to_string(),
        format!("{}a{}", "(".repeat(80), ")".repeat(80)),
        "a{1000000000}".to_string(),
    ] {
        let err = call(&ws, "fs.grep", json!({"path":".","pattern":pattern}))
            .await
            .unwrap_err();
        assert!(!err.outcome_unknown);
        assert!(err.message.contains("pattern") || err.message.contains("regex"));
    }
    assert!(
        !call(&ws, "fs.glob", json!({"path":".","pattern":"["}))
            .await
            .unwrap_err()
            .outcome_unknown
    );
    let token = CancellationToken::new();
    token.cancel();
    let err = ws
        .execute("fs.glob", &json!({"path":".","pattern":"**/*"}), token)
        .await
        .unwrap_err();
    assert!(err.message.contains("cancel"));
    let err = ws
        .execute_until(
            "fs.grep",
            &json!({"path":".","pattern":"a"}),
            CancellationToken::new(),
            Instant::now(),
        )
        .await
        .unwrap_err();
    assert!(err.message.contains("deadline"));
}
