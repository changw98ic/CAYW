#![cfg(unix)]

use cayw_workspace::{CancellationToken, CapabilityError, Limits, Workspace};
use serde_json::{Value, json};
use std::{
    fs,
    time::{Duration, Instant},
};

async fn call(ws: &Workspace, name: &str, args: Value) -> Result<Value, CapabilityError> {
    ws.execute(name, &args, CancellationToken::new()).await
}
fn open(temp: &tempfile::TempDir, limits: Limits) -> Workspace {
    // Same fork-inherited flock busy window Host::open_workspace retries.
    let root = temp.path().canonicalize().unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match Workspace::open(&root, limits.clone()) {
            Ok(workspace) => return workspace,
            Err(error)
                if !error.outcome_unknown
                    && error.message.starts_with("workspace is busy:")
                    && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("workspace open failed: {error:?}"),
        }
    }
}

#[tokio::test]
async fn exact_match_limit_is_complete_but_one_more_match_truncates() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("a"), "xxx").unwrap();
    let ws = open(&temp, Limits::default());
    for (max, truncated) in [(3, false), (2, true)] {
        let value = call(
            &ws,
            "fs.grep",
            json!({"path":"a","pattern":"x","max_matches":max}),
        )
        .await
        .unwrap();
        assert_eq!(value["matches"].as_array().unwrap().len(), max);
        assert_eq!(value["truncated"], truncated);
        assert_eq!(value["scan_complete"], !truncated);
    }
    assert_eq!(
        call(
            &ws,
            "fs.glob",
            json!({"path":".","pattern":"*","max_results":1})
        )
        .await
        .unwrap()["scan_complete"],
        true
    );
}

#[tokio::test]
async fn ignore_rules_do_not_read_global_config_and_escape_gitignore_punctuation() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(
        temp.path().join(".gitignore"),
        "\\#hash\n\\!bang\n/trailing/\n",
    )
    .unwrap();
    for file in ["#hash", "!bang", "visible"] {
        fs::write(temp.path().join(file), "ok").unwrap();
    }
    fs::create_dir(temp.path().join("trailing")).unwrap();
    fs::write(temp.path().join("trailing/hidden"), "ok").unwrap();
    let ws = open(&temp, Limits::default());
    let value = call(&ws, "fs.glob", json!({"path":".","pattern":"**/*"}))
        .await
        .unwrap();
    assert_eq!(value["paths"], json!([".gitignore", "visible"]));
}

#[tokio::test]
async fn all_four_capabilities_reject_unknown_fields_and_zero_result_limits() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("lib.rs"), "fn a() {}\n").unwrap();
    let ws = open(&temp, Limits::default());
    for (name, mut args, field) in [
        ("fs.glob", json!({"path":".","pattern":"*"}), "max_results"),
        (
            "fs.grep",
            json!({"path":"lib.rs","pattern":"fn"}),
            "max_matches",
        ),
        (
            "code.outline",
            json!({"path":"lib.rs","language":"rust"}),
            "max_symbols",
        ),
        ("code.diff_hunks", json!({"diff":""}), "max_hunks"),
    ] {
        args["unexpected"] = json!(true);
        assert!(
            !call(&ws, name, args.clone())
                .await
                .unwrap_err()
                .outcome_unknown
        );
        args.as_object_mut().unwrap().remove("unexpected");
        args[field] = json!(0);
        assert!(!call(&ws, name, args).await.unwrap_err().outcome_unknown);
    }
}

#[tokio::test]
async fn reads_honor_explicit_file_depth_output_limits_and_allow_large_ignore_files() {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir_all(temp.path().join("a/b")).unwrap();
    fs::write(temp.path().join("a/b/c"), "x".repeat(100)).unwrap();
    for limits in [
        Limits {
            max_depth: Some(1),
            ..Limits::default()
        },
        Limits {
            max_file_bytes: Some(4),
            ..Limits::default()
        },
        Limits {
            max_output_bytes: Some(80),
            ..Limits::default()
        },
    ] {
        let ws = open(&temp, limits);
        assert!(
            !call(&ws, "fs.grep", json!({"path":".","pattern":"x"}))
                .await
                .unwrap_err()
                .outcome_unknown
        );
    }
    fs::write(temp.path().join(".gitignore"), "#".repeat(65537)).unwrap();
    let ws = open(&temp, Limits::default());
    assert!(
        call(&ws, "fs.glob", json!({"path":".","pattern":"**/*"}))
            .await
            .is_ok()
    );
    assert!(
        call(
            &ws,
            "fs.glob",
            json!({"path":".","pattern":"**/*","respect_gitignore":false})
        )
        .await
        .is_ok()
    );
}

#[tokio::test]
async fn source_parse_and_diff_limits_still_validate_hidden_suffixes() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("bad.rs"), "fn okay() {} fn broken(").unwrap();
    let ws = open(&temp, Limits::default());
    assert!(
        call(
            &ws,
            "code.outline",
            json!({"path":"bad.rs","language":"rust","max_symbols":1})
        )
        .await
        .is_err()
    );
    let diff = "--- a/f\n+++ b/f\n@@ -1 +1 @@\n-a\n+b\n@@ -2,2 +2 @@\n-x\n+y\n";
    assert!(
        call(&ws, "code.diff_hunks", json!({"diff":diff,"max_hunks":1}))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn diff_rejects_traversal_metadata_nul_and_empty_hunks() {
    let temp = tempfile::tempdir().unwrap();
    let ws = open(&temp, Limits::default());
    for diff in [
        "diff --git a/../secret b/f\n--- a/f\n+++ b/f\n@@ -1 +1 @@\n-a\n+b\n",
        "--- a/f\n+++ b/f\n@@ -1,0 +1,0 @@\n",
        "--- a/f\n+++ b/f\n@@ -1 +1 @@\n-a\n+b\0\n",
        "--- /dev/null\n+++ b/f\n@@ -5,0 +1 @@\n+x\n",
        "--- a/f\n+++ b/f\n@@ -1 +1 @@\n-a\n+b\n\\ No newline at end of file\n\\ No newline at end of file\n",
    ] {
        assert!(
            call(&ws, "code.diff_hunks", json!({"diff":diff}))
                .await
                .is_err(),
            "{diff:?}"
        );
    }
}

#[tokio::test]
async fn outline_has_depth_bound_and_correct_shebang_and_unicode_spans() {
    let temp = tempfile::tempdir().unwrap();
    let source = "#!/usr/bin/env rust-script\n// λ\nfn café() {}\n";
    fs::write(temp.path().join("a.rs"), source).unwrap();
    let ws = open(&temp, Limits::default());
    let value = call(
        &ws,
        "code.outline",
        json!({"path":"a.rs","language":"rust"}),
    )
    .await
    .unwrap();
    let symbol = &value["symbols"][0];
    assert_eq!(symbol["start_line"], 3);
    let start = symbol["byte_start"].as_u64().unwrap() as usize;
    let end = symbol["byte_end"].as_u64().unwrap() as usize;
    assert_eq!(&source[start..end], "fn café() {}");
    fs::write(
        temp.path().join("deep.rs"),
        format!(
            "fn a() {{ let _ = {}1{}; }}",
            "(".repeat(1000),
            ")".repeat(1000)
        ),
    )
    .unwrap();
    assert!(
        call(
            &ws,
            "code.outline",
            json!({"path":"deep.rs","language":"rust"})
        )
        .await
        .is_err()
    );
    let ws = open(
        &temp,
        Limits {
            max_depth: Some(1),
            ..Limits::default()
        },
    );
    assert!(
        call(
            &ws,
            "code.outline",
            json!({"path":"a/b/a.rs","language":"rust"})
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn repeated_search_completes_and_expired_deadlines_remain_errors() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("a"), "x\n".repeat(100_000)).unwrap();
    let ws = open(&temp, Limits::default());
    let result = call(
        &ws,
        "fs.grep",
        json!({"path":"a","pattern":"x","max_matches":100_000}),
    )
    .await
    .unwrap();
    assert_eq!(result["matches"].as_array().unwrap().len(), 100_000);
    assert_eq!(result["truncated"], false);
    let error = ws
        .execute_until(
            "fs.grep",
            &json!({"path":"a","pattern":"x"}),
            CancellationToken::new(),
            Instant::now() - Duration::from_secs(1),
        )
        .await
        .unwrap_err();
    assert!(error.message.contains("deadline"));
}

#[tokio::test]
async fn outline_accepts_many_shallow_declarations_without_semicolons() {
    let temp = tempfile::tempdir().unwrap();
    let source = (0..1100)
        .map(|n| format!("fn f{n}() {{}}\n"))
        .collect::<String>();
    fs::write(temp.path().join("a.rs"), source).unwrap();
    let ws = open(&temp, Limits::default());
    let result = call(
        &ws,
        "code.outline",
        json!({"path":"a.rs","language":"rust","max_symbols":1100}),
    )
    .await
    .unwrap();
    assert_eq!(result["symbols"].as_array().unwrap().len(), 1100);
    assert_eq!(result["truncated"], false);
}

#[tokio::test]
async fn long_recursive_expression_is_rejected_before_recursive_parsing() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(
        temp.path().join("recursive.rs"),
        format!("fn f() {{ let _ = {}true; }}", "!".repeat(10_000)),
    )
    .unwrap();
    let ws = open(&temp, Limits::default());
    let error = call(
        &ws,
        "code.outline",
        json!({"path":"recursive.rs","language":"rust"}),
    )
    .await
    .unwrap_err();
    assert!(error.message.contains("complexity"));
    assert!(!error.outcome_unknown);
}

#[tokio::test]
async fn outline_accepts_long_iterative_binary_and_postfix_expressions() {
    let temp = tempfile::tempdir().unwrap();
    let ws = open(&temp, Limits::default());
    for (name, expression) in [
        ("binary", format!("{}1", "1+".repeat(10_000))),
        ("postfix", format!("x{}", ".f()".repeat(10_000))),
    ] {
        fs::write(
            temp.path().join("large.rs"),
            format!("fn {name}() {{ let _ = {expression}; }}"),
        )
        .unwrap();
        let result = call(
            &ws,
            "code.outline",
            json!({"path":"large.rs","language":"rust"}),
        )
        .await
        .unwrap();
        assert_eq!(result["symbols"][0]["name"], name);
        assert_eq!(result["truncated"], false);
    }
}
