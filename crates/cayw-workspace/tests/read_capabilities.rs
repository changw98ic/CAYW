use cayw_workspace::{CancellationToken, CapabilityError, Limits, Workspace};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    time::{Duration, Instant},
};
use tempfile::TempDir;

fn workspace(temp: &TempDir, limits: Limits) -> Workspace {
    Workspace::open(temp.path().canonicalize().unwrap(), limits).unwrap()
}
async fn call(workspace: &Workspace, name: &str, args: Value) -> Result<Value, CapabilityError> {
    workspace
        .execute(name, &args, CancellationToken::new())
        .await
}
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn paths(result: &Value) -> Vec<&str> {
    result["paths"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item.as_str().unwrap())
        .collect()
}

#[tokio::test]
async fn glob_obeys_nested_gitignore_negation_anchors_and_parent_exclusions() {
    let temp = tempfile::tempdir().unwrap();
    for dir in ["src/deep", "blocked", "nested/cache", "nested/kept"] {
        fs::create_dir_all(temp.path().join(dir)).unwrap();
    }
    fs::write(
        temp.path().join(".gitignore"),
        "/root.rs\n*.tmp\n!keep.tmp\nblocked/\n!blocked/no.rs\ncache/\n",
    )
    .unwrap();
    fs::write(
        temp.path().join("src/.gitignore"),
        "!keep.tmp\n/deep/*.rs\n!/deep/yes.rs\n",
    )
    .unwrap();
    fs::write(temp.path().join("nested/.gitignore"), "!cache/\n").unwrap();
    for file in [
        "root.rs",
        "src/root.rs",
        "src/deep/no.rs",
        "src/deep/yes.rs",
        "blocked/no.rs",
        "drop.tmp",
        "keep.tmp",
        "src/keep.tmp",
        "nested/cache/a.rs",
        "nested/kept/z.rs",
    ] {
        fs::write(temp.path().join(file), "needle\n").unwrap();
    }
    let workspace = workspace(&temp, Limits::default());
    let result = call(
        &workspace,
        "fs.glob",
        json!({"path":".","pattern":"**/*.{rs,tmp}"}),
    )
    .await
    .unwrap();
    assert_eq!(
        paths(&result),
        [
            "keep.tmp",
            "nested/cache/a.rs",
            "nested/kept/z.rs",
            "src/deep/yes.rs",
            "src/keep.tmp",
            "src/root.rs"
        ]
    );
    assert_eq!(result["scan_complete"], true);
    assert_eq!(result["truncated"], false);
    let subtree = call(
        &workspace,
        "fs.glob",
        json!({"path":"src/deep","pattern":"*.rs"}),
    )
    .await
    .unwrap();
    assert_eq!(paths(&subtree), ["src/deep/yes.rs"]);
    let ignored = call(
        &workspace,
        "fs.glob",
        json!({"path":"blocked","pattern":"**/*.rs"}),
    )
    .await
    .unwrap();
    assert!(paths(&ignored).is_empty());
    let all = call(
        &workspace,
        "fs.glob",
        json!({"path":".","pattern":"**/*.rs","respect_gitignore":false}),
    )
    .await
    .unwrap();
    assert_eq!(paths(&all).len(), 7);
    let grep = call(
        &workspace,
        "fs.grep",
        json!({"path":".","pattern":"needle","max_matches":100}),
    )
    .await
    .unwrap();
    assert_eq!(grep["matches"].as_array().unwrap().len(), 6);
}

#[tokio::test]
async fn grep_full_file_unicode_offsets_lines_hash_and_context_are_exact() {
    let temp = tempfile::tempdir().unwrap();
    let text = "前\nαx\nβy\n后\n";
    fs::write(temp.path().join("a.txt"), text).unwrap();
    let workspace = workspace(&temp, Limits::default());
    let result = call(&workspace, "fs.grep", json!({"path":".","pattern":"^αx.*βy$","multiline":true,"dot_matches_new_line":true,"context_lines":1})).await.unwrap();
    let found = &result["matches"][0];
    assert_eq!(result["matches"].as_array().unwrap().len(), 1);
    assert_eq!(found["sha256"], hash(text.as_bytes()));
    assert_eq!(found["byte_start"], 4);
    assert_eq!(found["byte_end"], 11);
    assert_eq!(found["start_line"], 2);
    assert_eq!(found["end_line"], 3);
    assert_eq!(found["text"], "αx\nβy");
    assert_eq!(found["context"]["text"], text);
    assert_eq!(found["context"]["start_line"], 1);
    assert_eq!(found["context"]["end_line"], 4);
    assert_eq!(result["scan_complete"], true);
}

#[tokio::test]
async fn grep_literal_case_flags_empty_matches_and_binary_skips_are_explicit() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("text"), "A.B\nαβ").unwrap();
    fs::write(temp.path().join("binary"), b"A.B\0").unwrap();
    fs::write(temp.path().join("nonutf8"), b"A.B\xff").unwrap();
    let workspace = workspace(&temp, Limits::default());
    let result = call(
        &workspace,
        "fs.grep",
        json!({"path":".","pattern":"a.b","literal":true,"case_insensitive":true}),
    )
    .await
    .unwrap();
    assert_eq!(result["matches"].as_array().unwrap().len(), 1);
    assert_eq!(result["matches"][0]["text"], "A.B");
    assert_eq!(result["skipped"][0]["reason"], "binary");
    assert_eq!(result["skipped"][1]["reason"], "non_utf8");
    let zero = call(
        &workspace,
        "fs.grep",
        json!({"path":".","pattern":"","max_matches":100}),
    )
    .await
    .unwrap();
    let positions: Vec<u64> = zero["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["byte_start"].as_u64().unwrap())
        .collect();
    assert_eq!(positions, [0, 1, 2, 3, 4, 6, 8]);
}

#[tokio::test]
async fn result_limits_are_deterministic_and_distinct_from_scan_failures() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("b.txt"), "aa").unwrap();
    fs::write(temp.path().join("a.txt"), "aa").unwrap();
    let workspace = workspace(&temp, Limits::default());
    let args = json!({"path":".","pattern":"**/[ab].t?t","max_results":1});
    let first = call(&workspace, "fs.glob", args.clone()).await.unwrap();
    assert_eq!(paths(&first), ["a.txt"]);
    assert_eq!(first["truncated"], true);
    assert_eq!(first["scan_complete"], false);
    assert_eq!(first, call(&workspace, "fs.glob", args).await.unwrap());
    let found = call(
        &workspace,
        "fs.grep",
        json!({"path":".","pattern":"a","max_matches":1}),
    )
    .await
    .unwrap();
    assert_eq!(found["matches"].as_array().unwrap().len(), 1);
    assert_eq!(found["truncated"], true);
    assert_eq!(found["scan_complete"], false);
    let bounded = self::workspace(
        &temp,
        Limits {
            max_tree_entries: 1,
            ..Limits::default()
        },
    );
    let error = call(&bounded, "fs.glob", json!({"path":".","pattern":"**/*"}))
        .await
        .unwrap_err();
    assert!(!error.outcome_unknown);
    assert!(error.message.contains("entry"));
}

#[tokio::test]
async fn scan_bytes_regex_complexity_and_output_are_bounded() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("a"), "123456").unwrap();
    fs::write(temp.path().join("b"), "123456").unwrap();
    let workspace = workspace(
        &temp,
        Limits {
            max_batch_bytes: 10,
            ..Limits::default()
        },
    );
    let error = call(
        &workspace,
        "fs.grep",
        json!({"path":".","pattern":"absent"}),
    )
    .await
    .unwrap_err();
    assert!(!error.outcome_unknown);
    assert!(error.message.contains("byte"));
    for pattern in ["[", "a{1000000000}", &"(".repeat(100)] {
        let error = call(&workspace, "fs.grep", json!({"path":".","pattern":pattern}))
            .await
            .unwrap_err();
        assert!(!error.outcome_unknown);
        assert!(error.message.contains("regex"));
    }
    let small = self::workspace(
        &temp,
        Limits {
            max_output_bytes: 200,
            ..Limits::default()
        },
    );
    assert!(
        call(&small, "fs.grep", json!({"path":".","pattern":".*"}))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn outline_parses_nested_rust_items_raw_unicode_and_exact_source_ranges() {
    let temp = tempfile::tempdir().unwrap();
    let text = "// 汉字\nmod 外 { pub struct É; impl É { pub fn r#type(&self) {} } fn α() {} }\n";
    fs::write(temp.path().join("lib.rs"), text).unwrap();
    let workspace = workspace(&temp, Limits::default());
    let result = call(
        &workspace,
        "code.outline",
        json!({"path":"lib.rs","language":"rust"}),
    )
    .await
    .unwrap();
    assert_eq!(result["sha256"], hash(text.as_bytes()));
    assert_eq!(result["language"], "rust");
    let symbols = result["symbols"].as_array().unwrap();
    let names: Vec<&str> = symbols
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["外", "É", "É", "r#type", "α"]);
    let method = &symbols[3];
    let start = method["byte_start"].as_u64().unwrap() as usize;
    let end = method["byte_end"].as_u64().unwrap() as usize;
    assert_eq!(&text[start..end], "pub fn r#type(&self) {}");
    assert_eq!(method["start_line"], 2);
    assert_eq!(method["end_line"], 2);
    assert_eq!(method["kind"], "function");
    assert_eq!(method["parent"], 2);
    assert_eq!(result["truncated"], false);
}

#[tokio::test]
async fn diff_hunks_validates_counts_paths_and_exact_old_new_spans() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = workspace(&temp, Limits::default());
    let text = "diff --git a/src/a.rs b/src/a.rs\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -2,2 +2,3 @@ fn α\n keep\n-old\n+new\n+more\n@@ -9,0 +11 @@\n+last\n\\ No newline at end of file\n";
    let result = call(&workspace, "code.diff_hunks", json!({"diff":text}))
        .await
        .unwrap();
    assert_eq!(result["sha256"], hash(text.as_bytes()));
    assert_eq!(result["hunks"][0]["old_path"], "src/a.rs");
    assert_eq!(result["hunks"][0]["new_path"], "src/a.rs");
    let first = &result["hunks"][0];
    assert_eq!(
        first["old"],
        json!({"start_line":2,"line_count":2,"end_line_exclusive":4})
    );
    assert_eq!(
        first["new"],
        json!({"start_line":2,"line_count":3,"end_line_exclusive":5})
    );
    assert_eq!(first["header_line"], 4);
    assert_eq!(first["body_start_line"], 5);
    assert_eq!(first["body_end_line_exclusive"], 9);
    assert_eq!(result["hunks"][1]["old"]["line_count"], 0);
    assert_eq!(result["validated_complete"], true);
    let created = call(
        &workspace,
        "code.diff_hunks",
        json!({"diff":"--- /dev/null\n+++ b/new.rs\n@@ -0,0 +1 @@\n+new\n"}),
    )
    .await
    .unwrap();
    assert!(created["hunks"][0]["old_path"].is_null());
}

#[tokio::test]
async fn malformed_or_unsupported_code_and_diff_are_known_errors() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("bad.rs"), "fn broken(").unwrap();
    let workspace = workspace(&temp, Limits::default());
    for args in [
        json!({"path":"bad.rs","language":"rust"}),
        json!({"path":"bad.rs","language":"python"}),
    ] {
        assert!(
            !call(&workspace, "code.outline", args)
                .await
                .unwrap_err()
                .outcome_unknown
        );
    }
    for text in [
        "@@ -1 +1 @@\n-a\n+b\n",
        "--- a/a\n+++ b/a\n@@ -1,2 +1 @@\n-a\n+b\n",
        "--- a/a\n+++ b/a\n@@ -18446744073709551615,2 +1 @@\n",
        "--- a/../bad\n+++ b/ok\n@@ -1 +1 @@\n-a\n+b\n",
        "--- a/a\n+++ b/a\n@@ -1 +1 @@\n-a\n+b\n+extra\n",
    ] {
        assert!(
            !call(&workspace, "code.diff_hunks", json!({"diff":text}))
                .await
                .unwrap_err()
                .outcome_unknown
        );
    }
}

#[tokio::test]
async fn cancellation_and_expired_absolute_deadlines_refuse_all_new_reads() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("lib.rs"), "fn a() {}\n").unwrap();
    let workspace = workspace(&temp, Limits::default());
    for (name, args) in [
        ("fs.glob", json!({"path":".","pattern":"**/*"})),
        ("fs.grep", json!({"path":".","pattern":"fn"})),
        ("code.outline", json!({"path":"lib.rs","language":"rust"})),
        ("code.diff_hunks", json!({"diff":""})),
    ] {
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(
            !workspace
                .execute(name, &args, cancel)
                .await
                .unwrap_err()
                .outcome_unknown
        );
        assert!(
            !workspace
                .execute_until(
                    name,
                    &args,
                    CancellationToken::new(),
                    Instant::now() - Duration::from_secs(1)
                )
                .await
                .unwrap_err()
                .outcome_unknown
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn scanner_never_follows_links_opens_fifos_or_reads_protected_entries() {
    use std::{os::unix::fs::symlink, process::Command};
    let temp = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("secret"), "needle").unwrap();
    symlink(outside.path(), temp.path().join("link")).unwrap();
    symlink(outside.path().join("secret"), temp.path().join("file-link")).unwrap();
    assert!(
        Command::new("mkfifo")
            .arg(temp.path().join("pipe"))
            .status()
            .unwrap()
            .success()
    );
    for dir in [".git", ".cayw-private"] {
        fs::create_dir(temp.path().join(dir)).unwrap();
        fs::write(temp.path().join(dir).join("secret"), "needle").unwrap();
    }
    fs::write(temp.path().join("ok"), "needle").unwrap();
    let workspace = workspace(&temp, Limits::default());
    let glob = call(
        &workspace,
        "fs.glob",
        json!({"path":".","pattern":"**/*","respect_gitignore":false}),
    )
    .await
    .unwrap();
    assert_eq!(paths(&glob), ["ok"]);
    assert_eq!(glob["skipped"].as_array().unwrap().len(), 3);
    let grep = call(
        &workspace,
        "fs.grep",
        json!({"path":".","pattern":"needle","respect_gitignore":false}),
    )
    .await
    .unwrap();
    assert_eq!(grep["matches"].as_array().unwrap().len(), 1);
    assert_eq!(grep["matches"][0]["path"], "ok");
    for path in ["link", ".git", ".cayw-private"] {
        assert!(
            call(&workspace, "fs.glob", json!({"path":path,"pattern":"**/*"}))
                .await
                .is_err()
        );
    }
    symlink(
        outside.path().join("secret"),
        temp.path().join(".gitignore"),
    )
    .unwrap();
    assert!(
        !call(&workspace, "fs.glob", json!({"path":".","pattern":"**/*"}))
            .await
            .unwrap_err()
            .outcome_unknown
    );
}

#[cfg(unix)]
#[tokio::test]
async fn read_scanner_uses_the_held_root_after_rename_and_replacement() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("root");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("a.txt"), "original").unwrap();
    let workspace = Workspace::open(root.canonicalize().unwrap(), Limits::default()).unwrap();
    fs::rename(&root, temp.path().join("moved")).unwrap();
    fs::create_dir(&root).unwrap();
    fs::write(root.join("a.txt"), "replacement").unwrap();
    let result = call(
        &workspace,
        "fs.grep",
        json!({"path":".","pattern":"original"}),
    )
    .await
    .unwrap();
    assert_eq!(result["matches"].as_array().unwrap().len(), 1);
    assert_eq!(result["matches"][0]["text"], "original");
}
