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
            max_tree_entries: Some(4),
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
            max_output_bytes: Some(24),
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

#[tokio::test]
async fn git_diff_paths_preserve_spaces_and_c_quoting() {
    let temp = tempfile::tempdir().unwrap();
    let ws = Workspace::open(temp.path().canonicalize().unwrap(), Limits::default()).unwrap();
    for (old, new, expected) in [
        ("a/hello world.txt", "b/hello world.txt", "hello world.txt"),
        ("a/foo b/bar.txt", "b/foo b/bar.txt", "foo b/bar.txt"),
        ("a/中文.txt", "b/中文.txt", "中文.txt"),
        (
            r#""a/\344\270\255.txt""#,
            r#""b/\344\270\255.txt""#,
            "中.txt",
        ),
        (
            r#""a/say\"hello.txt""#,
            r#""b/say\"hello.txt""#,
            "say\"hello.txt",
        ),
        (
            r#""a/back\\slash.txt""#,
            r#""b/back\\slash.txt""#,
            "back\\slash.txt",
        ),
    ] {
        let diff =
            format!("diff --git {old} {new}\n--- {old}\n+++ {new}\n@@ -1 +1 @@\n-old\n+new\n");
        let value = call(&ws, "code.diff_hunks", json!({"diff":diff}))
            .await
            .unwrap();
        assert_eq!(value["hunks"][0]["old_path"], expected, "{diff}");
        assert_eq!(value["hunks"][0]["new_path"], expected, "{diff}");
    }
    for (old, new, range, body) in [
        ("/dev/null", "b/new file", "-0,0 +1", "+new\n"),
        ("a/new file", "/dev/null", "-1 +0,0", "-old\n"),
    ] {
        let diff = format!(
            "diff --git a/new file b/new file\n--- {old}\n+++ {new}\n@@ {range} @@\n{body}"
        );
        assert!(
            call(&ws, "code.diff_hunks", json!({"diff":diff}))
                .await
                .is_ok()
        );
    }
    for wrapper in [
        "a/other b/hello world.txt",
        "a/hello world.txt b/other",
        "a/hello world.txt b/hello world.txt extra",
        r#""a/hello\qworld.txt" b/hello world.txt"#,
    ] {
        let diff = format!(
            "diff --git {wrapper}\n--- a/hello world.txt\n+++ b/hello world.txt\n@@ -1 +1 @@\n-a\n+b\n"
        );
        assert!(
            call(&ws, "code.diff_hunks", json!({"diff":diff}))
                .await
                .is_err()
        );
    }
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn actual_git_diff_paths_round_trip() {
    use std::process::Command;
    let temp = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        let output = Command::new("/usr/bin/git")
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", temp.path())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .current_dir(temp.path())
            .args([
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    };
    git(&["init"]);
    let paths = [
        "hello world.txt",
        "foo b/bar.txt",
        "中文.txt",
        "quote\".txt",
        "back\\slash.txt",
    ];
    for path in paths {
        let path = temp.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "old\n").unwrap();
    }
    git(&["add", "."]);
    git(&["commit", "-m", "fixture"]);
    for path in paths {
        fs::write(temp.path().join(path), "new\n").unwrap();
    }
    let ws = Workspace::open(temp.path().canonicalize().unwrap(), Limits::default()).unwrap();
    for quote in ["core.quotePath=true", "core.quotePath=false"] {
        let diff = git(&["-c", quote, "diff", "--no-ext-diff", "--no-color"]);
        let value = call(&ws, "code.diff_hunks", json!({"diff":diff}))
            .await
            .unwrap();
        let hunks = value["hunks"].as_array().unwrap();
        assert_eq!(hunks.len(), paths.len());
        for path in paths {
            assert!(
                hunks
                    .iter()
                    .any(|hunk| hunk["old_path"] == path && hunk["new_path"] == path)
            );
        }
    }
}

#[tokio::test]
async fn outline_flat_comparisons_do_not_consume_generic_depth() {
    let temp = tempfile::tempdir().unwrap();
    let ws = Workspace::open(temp.path().canonicalize().unwrap(), Limits::default()).unwrap();
    for operator in ["<", "<=", "<<", "<<="] {
        let statements = (0..160)
            .map(|_| format!("let _ = a {operator} b;\n"))
            .collect::<String>();
        let chain = if operator == "<<=" {
            "a <<= b;".into()
        } else {
            (0..160)
                .map(|_| format!("(a {operator} b)"))
                .collect::<Vec<_>>()
                .join(" && ")
                + ";"
        };
        let source = format!(
            "// 中文\nstruct Holder<T>(T);\nfn comparisons(a: u32, b: u32) {{\n{statements}{chain}\nlet _: Holder<Vec<u8>>;\n}}\nfn after() {{}}\n"
        );
        fs::write(temp.path().join("comparisons.rs"), &source).unwrap();
        let value = call(
            &ws,
            "code.outline",
            json!({"path":"comparisons.rs","language":"rust"}),
        )
        .await
        .unwrap();
        let symbols = value["symbols"].as_array().unwrap();
        assert_eq!(symbols.len(), 3, "{operator}");
        let function = &symbols[1];
        let start = function["byte_start"].as_u64().unwrap() as usize;
        let end = function["byte_end"].as_u64().unwrap() as usize;
        assert!(source[start..end].starts_with("fn comparisons"));
        assert!(source[start..end].ends_with('}'));
        assert_eq!(symbols[2]["name"], "after");
    }
    let chain = (0..160).map(|_| "a <= b").collect::<Vec<_>>().join(" && ");
    fs::write(
        temp.path().join("chain.rs"),
        format!("fn f(a: u32, b: u32) {{ let _ = {chain}; }}"),
    )
    .unwrap();
    call(
        &ws,
        "code.outline",
        json!({"path":"chain.rs","language":"rust"}),
    )
    .await
    .unwrap();
    for expression in ["a as u32 <= b", "(a as u32) < b", "a as u32 <<= b"] {
        let source = format!(
            "fn casts(a: u32, b: u32) {{ {} }}",
            format!("let _ = {expression};").repeat(160)
        );
        fs::write(temp.path().join("casts.rs"), source).unwrap();
        call(
            &ws,
            "code.outline",
            json!({"path":"casts.rs","language":"rust"}),
        )
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn outline_keeps_deep_generic_and_prefix_protection() {
    let temp = tempfile::tempdir().unwrap();
    let ws = Workspace::open(temp.path().canonicalize().unwrap(), Limits::default()).unwrap();
    // Just over the existing 64-level limit, not a stack-overflow stress input.
    let open = "Vec<".repeat(65);
    for source in [
        format!("type Deep = {open}u8{};", ">".repeat(65)),
        format!("type Deep = {open}u8;"),
        format!("type Deep = {open}?{};", ">".repeat(65)),
        format!("fn f() {{ let _ = value as {open}u8; }}"),
        format!("fn f() {{ become value as {open}u8; }}"),
        format!("type Ref = {}u8;", "&".repeat(513)),
        format!(
            "type Deep = {}u8{};",
            "Outer<{ let _ = 1; 1 }, ".repeat(65),
            ">".repeat(65)
        ),
        format!(
            "type Deep = {}u8{};",
            "Vec<[".repeat(33),
            "; 1]>".repeat(33)
        ),
    ] {
        fs::write(temp.path().join("deep.rs"), source).unwrap();
        let error = call(
            &ws,
            "code.outline",
            json!({"path":"deep.rs","language":"rust"}),
        )
        .await
        .unwrap_err();
        assert!(!error.outcome_unknown);
        assert!(error.message.contains("Rust syntax"), "{error}");
    }
    for source in [
        "type Shallow = Vec<Vec<u8>>;",
        "fn f<T: for<'a> Fn(&'a str)>() {}",
        "type Qualified<T> = <<T as Trait>::Item as Other>::Out;",
        "type Const = Array<{ 1 < 2 }, Vec<u8>>;",
    ] {
        fs::write(temp.path().join("shallow.rs"), source).unwrap();
        call(
            &ws,
            "code.outline",
            json!({"path":"shallow.rs","language":"rust"}),
        )
        .await
        .unwrap();
    }
}
