//! Regressions for removed Git/profile restrictions (C07-C11).
#![cfg(unix)]

use async_trait::async_trait;
use cayw_ai::CancellationToken;
use cayw_codemode::{CapabilityError, CapabilityHost};
use cayw_host::{
    AuditSink, GitWritePolicy, Host, HostConfig, PermissionGate, Profile, ProfileKind,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

const GIT: &str = "/usr/bin/git";
static NEXT: AtomicU64 = AtomicU64::new(1);

struct Dir(PathBuf);
impl Dir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "cayw-git-restrictions-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Allow;
#[async_trait]
impl PermissionGate for Allow {
    async fn authorize(
        &self,
        _: &str,
        _: &Value,
        _: CancellationToken,
    ) -> Result<(), CapabilityError> {
        Ok(())
    }
}

#[derive(Default)]
struct Audit;
#[async_trait]
impl AuditSink for Audit {
    async fn started(&self, _: &str, _: &Value) -> Result<String, CapabilityError> {
        Ok("1".into())
    }
    async fn settled(
        &self,
        _: &str,
        _: &Result<Value, CapabilityError>,
    ) -> Result<(), CapabilityError> {
        Ok(())
    }
    fn dropped(&self, _: &str) -> Result<(), CapabilityError> {
        Ok(())
    }
}

fn git(root: &Path, args: &[&str]) -> std::process::Output {
    Command::new(GIT)
        .args(args)
        .current_dir(root)
        .env_clear()
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .output()
        .unwrap()
}

fn git_ok(root: &Path, args: &[&str]) -> String {
    let output = git(root, args);
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn init(root: &Path) {
    git_ok(root, &["init", "-q", "-b", "main"]);
    git_ok(root, &["config", "user.name", "Fixture"]);
    git_ok(root, &["config", "user.email", "fixture@example.invalid"]);
}

fn commit_all(root: &Path) {
    git_ok(root, &["add", "-A"]);
    git_ok(root, &["commit", "-q", "-m", "fixture"]);
}

fn policy() -> GitWritePolicy {
    GitWritePolicy {
        identity_name: "Fixture".into(),
        identity_email: "fixture@example.invalid".into(),
        allow_stage: true,
        allow_commit: true,
        allow_branch: true,
        allow_worktree: true,
    }
}

fn host(root: &Path, profiles: BTreeMap<String, Profile>) -> Arc<Host> {
    Arc::new(
        Host::new(
            root,
            Arc::new(Allow),
            Arc::new(Audit),
            HostConfig {
                git_program: Some(GIT.into()),
                git_writes: Some(policy()),
                profiles,
                ..Default::default()
            },
        )
        .unwrap(),
    )
}

fn stdout(result: &Value) -> String {
    result["stdout"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn history_and_status_do_not_depend_on_ignored_build_data_or_worktree_links() {
    let root = Dir::new();
    let outside = Dir::new();
    init(root.path());
    std::fs::write(root.path().join(".gitignore"), "target/\n").unwrap();
    std::fs::write(root.path().join("a.txt"), "a\n").unwrap();
    std::fs::write(outside.path().join("secret"), "must not be read\n").unwrap();
    std::os::unix::fs::symlink(outside.path().join("secret"), root.path().join("link")).unwrap();
    commit_all(root.path());
    // 70 MiB of ignored build data: the old whole-repository copy exceeded even
    // its 64 MiB configured ceiling and refused every read before launch.
    std::fs::create_dir(root.path().join("target")).unwrap();
    let large = std::fs::File::create(root.path().join("target/blob")).unwrap();
    large.set_len(70 * 1024 * 1024).unwrap();
    drop(large);
    assert_eq!(
        git_ok(root.path(), &["status", "--porcelain"]),
        "",
        "fixture must be clean for real Git"
    );

    let host = host(root.path(), BTreeMap::new());
    let status = host
        .call("git.status", json!({}), CancellationToken::new())
        .await
        .expect("ignored build data must not block status");
    assert_eq!(status["success"], true, "{status}");
    assert_eq!(stdout(&status), "", "status must agree with real Git");

    let log = host
        .call("git.log", json!({"limit": 5}), CancellationToken::new())
        .await
        .expect("repository size must not block history");
    assert_eq!(log["success"], true, "{log}");
    assert!(stdout(&log).contains("fixture"), "{log}");
}

#[tokio::test]
async fn allowed_filter_accepts_underscore_unicode_spaces_and_parentheses_as_one_argv() {
    let root = Dir::new();
    let profile = Profile {
        kind: ProfileKind::Test,
        program: "/bin/echo".into(),
        args: vec!["filter:".into()],
        env: BTreeMap::new(),
        timeout: Duration::from_secs(10),
        max_output_bytes: 4096,
        allow_filter: true,
        io: cayw_host::ProcessIo::Pipes,
    };
    let host = host(
        root.path(),
        BTreeMap::from([("tests".to_string(), profile)]),
    );
    for filter in ["_private_case", "中文 测试 (unit)"] {
        let result = host
            .call(
                "test.run",
                json!({"profile":"tests", "filter":filter}),
                CancellationToken::new(),
            )
            .await
            .unwrap_or_else(|error| panic!("{filter}: {}", error.message));
        assert_eq!(result["success"], true, "{filter}: {result}");
        assert_eq!(stdout(&result), format!("filter: {filter}\n"), "{filter}");
    }
    let error = host
        .call(
            "test.run",
            json!({"profile":"tests", "filter":"--definitely-an-option"}),
            CancellationToken::new(),
        )
        .await
        .expect_err("a leading dash must not become an option");
    assert!(!error.outcome_unknown);
    assert!(
        error.message.to_ascii_lowercase().contains("filter"),
        "{}",
        error.message
    );
}

#[tokio::test]
async fn unicode_git_branch_and_worktree_names_are_accepted_without_ascii_whitelist() {
    let root = Dir::new();
    init(root.path());
    std::fs::write(root.path().join("a.txt"), "a\n").unwrap();
    commit_all(root.path());
    let commit = git_ok(root.path(), &["rev-parse", "HEAD"])
        .trim()
        .to_string();
    let host = host(root.path(), BTreeMap::new());

    let branch = host
        .call(
            "git.branch",
            json!({"action":"create", "name":"功能/新增", "commit":commit, "expected_head":commit}),
            CancellationToken::new(),
        )
        .await
        .expect("a valid Unicode branch name must be accepted");
    assert_eq!(branch["action"], "create");
    assert_eq!(
        git_ok(root.path(), &["rev-parse", "refs/heads/功能/新增"]).trim(),
        commit
    );

    let worktree = host
        .call(
            "git.worktree",
            json!({"action":"create", "name":"工作树", "branch":"功能/新增", "commit":commit, "expected_head":commit}),
            CancellationToken::new(),
        )
        .await
        .expect("a valid Unicode worktree name must be accepted");
    assert_eq!(worktree["action"], "create");
    assert!(root.path().join(".cayw-worktrees/工作树").is_dir());

    for name in ["bad:name", "-option", "HEAD", "a..b"] {
        let error = host
            .call(
                "git.branch",
                json!({"action":"create", "name":name, "commit":commit, "expected_head":commit}),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(
            !error.outcome_unknown,
            "{name} must be a known refusal: {}",
            error.message
        );
    }
}

#[tokio::test]
async fn ordinary_worktree_directory_names_follow_path_rules_instead_of_ref_rules() {
    let root = Dir::new();
    init(root.path());
    std::fs::write(root.path().join("a.txt"), "a\n").unwrap();
    commit_all(root.path());
    let commit = git_ok(root.path(), &["rev-parse", "HEAD"])
        .trim()
        .to_string();
    git_ok(root.path(), &["branch", "worktree-topic", &commit]);
    let host = host(root.path(), BTreeMap::new());
    for name in [
        ".hidden",
        "-leading",
        "a..b",
        "trailing.",
        "ordinary.lock",
        "中文 工作树 (one)",
    ] {
        host.call(
            "git.worktree",
            json!({"action":"create","name":name,"branch":"worktree-topic","commit":commit,"expected_head":commit}),
            CancellationToken::new(),
        ).await.unwrap_or_else(|error| panic!("create {name:?}: {error}"));
        assert!(
            root.path()
                .join(".cayw-worktrees")
                .join(name)
                .join("a.txt")
                .is_file()
        );
        host.call(
            "git.worktree",
            json!({"action":"remove","name":name,"expected_head":commit}),
            CancellationToken::new(),
        )
        .await
        .unwrap_or_else(|error| panic!("remove {name:?}: {error}"));
        assert!(!root.path().join(".cayw-worktrees").join(name).exists());
    }
    for name in [".", "..", "../escape", "inside/escape"] {
        assert!(host.call(
            "git.worktree",
            json!({"action":"create","name":name,"branch":"worktree-topic","commit":commit,"expected_head":commit}),
            CancellationToken::new(),
        ).await.is_err(), "{name:?}");
    }
}

#[tokio::test]
async fn locally_scoped_branch_names_are_not_rejected_by_additional_name_rules() {
    let root = Dir::new();
    init(root.path());
    std::fs::write(root.path().join("a.txt"), "a\n").unwrap();
    commit_all(root.path());
    let commit = git_ok(root.path(), &["rev-parse", "HEAD"])
        .trim()
        .to_string();
    let host = host(root.path(), BTreeMap::new());
    for name in [
        "head",
        "Head-variant",
        "@",
        "refs/heads/ordinary",
        "标点!()=,;",
    ] {
        assert!(
            git(root.path(), &["check-ref-format", "--branch", name])
                .status
                .success()
        );
        host.call(
            "git.branch",
            json!({"action":"create","name":name,"commit":commit,"expected_head":commit}),
            CancellationToken::new(),
        )
        .await
        .unwrap_or_else(|error| panic!("create {name:?}: {error}"));
        assert_eq!(
            git_ok(root.path(), &["rev-parse", &format!("refs/heads/{name}")]).trim(),
            commit
        );
        host.call(
            "git.branch",
            json!({"action":"delete","name":name,"commit":commit,"expected_head":commit}),
            CancellationToken::new(),
        )
        .await
        .unwrap_or_else(|error| panic!("delete {name:?}: {error}"));
    }
}

#[tokio::test]
async fn boolean_words_do_not_disable_filter_commands_or_include_paths() {
    for (key, value) in [
        ("filter.evil.clean", "false"),
        ("filter.evil.process", "off"),
        ("include.path", "false"),
    ] {
        let root = Dir::new();
        init(root.path());
        std::fs::write(root.path().join("a.txt"), "a\n").unwrap();
        commit_all(root.path());
        let head = git_ok(root.path(), &["rev-parse", "HEAD"])
            .trim()
            .to_string();
        let tree = git_ok(root.path(), &["write-tree"]).trim().to_string();
        git_ok(root.path(), &["config", key, value]);
        let host = host(root.path(), BTreeMap::new());
        std::fs::write(root.path().join("b.txt"), "b\n").unwrap();
        let error = host
            .call(
                "git.stage",
                json!({"paths":["b.txt"],"expected_head":head,"expected_index_tree":tree}),
                CancellationToken::new(),
            )
            .await
            .expect_err("command/path strings must not use boolean false semantics");
        assert!(!error.outcome_unknown, "{key}: {error}");
        assert!(error.message.contains(key), "{key}: {error}");
        assert_eq!(git_ok(root.path(), &["write-tree"]).trim(), tree);
    }
}

#[tokio::test]
async fn explicitly_disabled_git_config_values_do_not_block_writes() {
    let root = Dir::new();
    init(root.path());
    std::fs::write(root.path().join("a.txt"), "a\n").unwrap();
    commit_all(root.path());
    git_ok(root.path(), &["config", "core.fsmonitor", "false"]);
    git_ok(root.path(), &["config", "protocol.allow", "never"]);
    git_ok(root.path(), &["config", "core.hooksPath", "/dev/null"]);
    let host = host(root.path(), BTreeMap::new());
    let head = git_ok(root.path(), &["rev-parse", "HEAD"])
        .trim()
        .to_string();
    let tree = git_ok(root.path(), &["write-tree"]).trim().to_string();
    std::fs::write(root.path().join("b.txt"), "b\n").unwrap();

    let staged = host
        .call(
            "git.stage",
            json!({"paths":["b.txt"], "expected_head":head, "expected_index_tree":tree}),
            CancellationToken::new(),
        )
        .await
        .expect("explicitly disabled config values must not block a write");
    assert_ne!(staged["index_tree"], tree, "{staged}");

    // A disabled write does not hide genuinely executable configuration.
    git_ok(root.path(), &["config", "filter.evil.clean", "/bin/true"]);
    let head = git_ok(root.path(), &["rev-parse", "HEAD"])
        .trim()
        .to_string();
    let tree = git_ok(root.path(), &["write-tree"]).trim().to_string();
    std::fs::write(root.path().join("c.txt"), "c\n").unwrap();
    let error = host
        .call(
            "git.stage",
            json!({"paths":["c.txt"], "expected_head":head, "expected_index_tree":tree}),
            CancellationToken::new(),
        )
        .await
        .expect_err("an executable filter definition must still be refused");
    assert!(!error.outcome_unknown);
    assert!(
        error.message.contains("filter.evil.clean"),
        "rejection must name the refused key: {}",
        error.message
    );
}
