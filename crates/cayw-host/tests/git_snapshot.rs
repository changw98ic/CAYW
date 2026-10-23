//! Git read snapshots must match a real Git invocation for supported passive
//! repository settings and must reject formats they cannot represent.
#![cfg(unix)]

use async_trait::async_trait;
use cayw_ai::CancellationToken;
use cayw_codemode::{CapabilityError, CapabilityHost};
use cayw_host::{AuditSink, Host, HostConfig, PermissionGate};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

const GIT: &str = "/usr/bin/git";
static NEXT: AtomicU64 = AtomicU64::new(1);

struct Dir(PathBuf);
impl Dir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "cayw-git-snapshot-{}-{}",
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

fn real_status(root: &Path) -> String {
    git_ok(root, &["status", "--porcelain"])
}

fn init(root: &Path, extra: &[&str]) {
    let mut args = vec!["init", "-q", "-b", "main"];
    args.extend_from_slice(extra);
    git_ok(root, &args);
    git_ok(root, &["config", "user.name", "Fixture"]);
    git_ok(root, &["config", "user.email", "fixture@example.invalid"]);
}

fn commit_all(root: &Path) {
    git_ok(root, &["add", "-A"]);
    git_ok(root, &["commit", "-q", "-m", "fixture"]);
}

fn host(root: &Path) -> Arc<Host> {
    Arc::new(
        Host::new(
            root,
            Arc::new(Allow),
            Arc::new(Audit),
            HostConfig {
                git_program: Some(GIT.into()),
                ..Default::default()
            },
        )
        .unwrap(),
    )
}

async fn status(host: &Host) -> Result<Value, CapabilityError> {
    host.call("git.status", json!({}), CancellationToken::new())
        .await
}

fn stdout(result: &Value) -> String {
    result["stdout"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn filemode_false_clean_repository_matches_real_git() {
    let root = Dir::new();
    init(root.path(), &[]);
    std::fs::write(root.path().join("a.sh"), "#!/bin/sh\necho hi\n").unwrap();
    let mut permissions = std::fs::metadata(root.path().join("a.sh"))
        .unwrap()
        .permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
    std::fs::set_permissions(root.path().join("a.sh"), permissions).unwrap();
    commit_all(root.path());
    git_ok(root.path(), &["config", "core.filemode", "false"]);
    let mut permissions = std::fs::metadata(root.path().join("a.sh"))
        .unwrap()
        .permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o644);
    std::fs::set_permissions(root.path().join("a.sh"), permissions).unwrap();
    assert_eq!(
        real_status(root.path()),
        "",
        "fixture must be clean for real Git"
    );

    let result = status(&host(root.path()))
        .await
        .expect("supported passive config must be readable");
    assert_eq!(result["success"], true, "{result}");
    assert_eq!(stdout(&result), "", "snapshot must agree with real Git");
}

#[tokio::test]
async fn autocrlf_true_clean_repository_matches_real_git() {
    let root = Dir::new();
    init(root.path(), &[]);
    git_ok(root.path(), &["config", "core.autocrlf", "true"]);
    std::fs::write(root.path().join("a.txt"), "line1\r\nline2\r\n").unwrap();
    commit_all(root.path());
    assert_eq!(
        real_status(root.path()),
        "",
        "fixture must be clean for real Git"
    );

    let result = status(&host(root.path()))
        .await
        .expect("supported passive config must be readable");
    assert_eq!(result["success"], true, "{result}");
    assert_eq!(stdout(&result), "", "snapshot must agree with real Git");
}

#[tokio::test]
async fn sha256_repository_is_rejected_instead_of_misread_as_sha1() {
    let root = Dir::new();
    let output = git(
        root.path(),
        &["init", "-q", "-b", "main", "--object-format=sha256"],
    );
    assert!(
        output.status.success(),
        "SHA-256 repository fixture must initialize: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    git_ok(root.path(), &["config", "user.name", "Fixture"]);
    git_ok(
        root.path(),
        &["config", "user.email", "fixture@example.invalid"],
    );
    std::fs::write(root.path().join("a.txt"), "a\n").unwrap();
    commit_all(root.path());
    assert_eq!(real_status(root.path()), "");

    let error = status(&host(root.path()))
        .await
        .expect_err("SHA-256 cannot be represented by a SHA-1 snapshot");
    assert!(!error.outcome_unknown);
    let message = error.message.to_ascii_lowercase();
    assert!(
        message.contains("not supported") || message.contains("unsupported"),
        "rejection must be explicit: {}",
        error.message
    );
}

#[tokio::test]
async fn tracked_reserved_cayw_path_is_rejected() {
    let root = Dir::new();
    init(root.path(), &[]);
    std::fs::write(root.path().join("a.txt"), "a\n").unwrap();
    std::fs::write(root.path().join(".cayw-note.md"), "notes\n").unwrap();
    commit_all(root.path());
    assert_eq!(
        real_status(root.path()),
        "",
        "fixture must be clean for real Git"
    );

    let error = status(&host(root.path()))
        .await
        .expect_err("tracked reserved paths cannot be represented");
    assert!(!error.outcome_unknown);
    assert!(
        error.message.contains(".cayw-") || error.message.contains("reserved"),
        "rejection must name the reserved path: {}",
        error.message
    );
}

#[tokio::test]
async fn untracked_internal_cayw_lock_is_skipped_without_false_deletion() {
    let root = Dir::new();
    init(root.path(), &[]);
    std::fs::write(root.path().join("a.txt"), "a\n").unwrap();
    commit_all(root.path());
    std::fs::write(root.path().join(".cayw-host-lock-123"), "lock\n").unwrap();
    assert_eq!(real_status(root.path()), "?? .cayw-host-lock-123\n");

    let result = status(&host(root.path()))
        .await
        .expect("an untracked internal lock file must not break reads");
    assert_eq!(result["success"], true, "{result}");
    let output = stdout(&result);
    assert!(
        !output.contains(".cayw"),
        "reserved internal files stay unaddressable: {output}"
    );
}

#[tokio::test]
async fn sparse_checkout_repository_is_rejected() {
    let root = Dir::new();
    init(root.path(), &[]);
    std::fs::write(root.path().join("a.txt"), "a\n").unwrap();
    commit_all(root.path());
    git_ok(root.path(), &["config", "core.sparseCheckout", "true"]);
    std::fs::write(root.path().join(".git/info/sparse-checkout"), "/a.txt\n").unwrap();

    let error = status(&host(root.path()))
        .await
        .expect_err("sparse semantics cannot be represented by a full worktree copy");
    assert!(!error.outcome_unknown);
    let message = error.message.to_ascii_lowercase();
    assert!(
        message.contains("sparse")
            || message.contains("not supported")
            || message.contains("unsupported"),
        "rejection must be explicit: {}",
        error.message
    );
}

#[tokio::test]
async fn local_filters_includes_fsmonitor_and_hooks_never_execute() {
    let root = Dir::new();
    let outside = Dir::new();
    let marker = outside.path().join("executed");
    let script = outside.path().join("evil.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf executed >> '{}'\ncat\n",
            marker.display()
        ),
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&script).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
    std::fs::set_permissions(&script, permissions).unwrap();
    let include = outside.path().join("outside.inc");
    std::fs::write(
        &include,
        format!(
            "[filter \"included\"]\n\tclean = {}\n[core]\n\tfsmonitor = {}\n",
            script.display(),
            script.display()
        ),
    )
    .unwrap();

    init(root.path(), &[]);
    std::fs::write(root.path().join("a.txt"), "one\n").unwrap();
    commit_all(root.path());
    std::fs::write(root.path().join(".gitattributes"), "*.txt filter=evil\n").unwrap();
    git_ok(
        root.path(),
        &["config", "filter.evil.clean", script.to_str().unwrap()],
    );
    git_ok(
        root.path(),
        &["config", "filter.evil.smudge", script.to_str().unwrap()],
    );
    git_ok(
        root.path(),
        &["config", "core.fsmonitor", script.to_str().unwrap()],
    );
    git_ok(
        root.path(),
        &["config", "core.hooksPath", outside.path().to_str().unwrap()],
    );
    git_ok(
        root.path(),
        &["config", "include.path", include.to_str().unwrap()],
    );
    std::fs::write(root.path().join("a.txt"), "two\n").unwrap();

    let result = status(&host(root.path()))
        .await
        .expect("a local filter definition must not be loaded or fail the snapshot");
    assert_eq!(result["success"], true, "{result}");
    assert!(
        !marker.exists(),
        "snapshot read executed a configured filter, fsmonitor, hook or include"
    );
}

#[tokio::test]
async fn explicit_empty_filemode_is_false_and_matches_real_git() {
    let root = Dir::new();
    init(root.path(), &[]);
    std::fs::write(root.path().join("a.sh"), "#!/bin/sh\necho hi\n").unwrap();
    let mut permissions = std::fs::metadata(root.path().join("a.sh"))
        .unwrap()
        .permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
    std::fs::set_permissions(root.path().join("a.sh"), permissions).unwrap();
    commit_all(root.path());
    // `core.filemode =` (explicit empty) is false; an implicit key would be true.
    git_ok(root.path(), &["config", "core.filemode", ""]);
    let mut permissions = std::fs::metadata(root.path().join("a.sh"))
        .unwrap()
        .permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o644);
    std::fs::set_permissions(root.path().join("a.sh"), permissions).unwrap();
    assert_eq!(
        real_status(root.path()),
        "",
        "fixture must be clean for real Git"
    );

    let result = status(&host(root.path()))
        .await
        .expect("an explicitly empty passive boolean must be readable");
    assert_eq!(result["success"], true, "{result}");
    assert_eq!(stdout(&result), "", "snapshot must agree with real Git");
}

#[tokio::test]
async fn explicit_empty_sparse_checkout_is_false_and_allowed() {
    let root = Dir::new();
    init(root.path(), &[]);
    std::fs::write(root.path().join("a.txt"), "a\n").unwrap();
    commit_all(root.path());
    git_ok(root.path(), &["config", "core.sparseCheckout", ""]);
    assert_eq!(
        real_status(root.path()),
        "",
        "fixture must be clean for real Git"
    );

    let result = status(&host(root.path()))
        .await
        .expect("an explicitly empty sparse flag is false and representable");
    assert_eq!(result["success"], true, "{result}");
    assert_eq!(stdout(&result), "");
}

#[tokio::test]
async fn implicit_sparse_checkout_key_is_true_and_rejected() {
    let root = Dir::new();
    init(root.path(), &[]);
    std::fs::write(root.path().join("a.txt"), "a\n").unwrap();
    commit_all(root.path());
    // A key without `=` is an implicit boolean true in Git's own config parser.
    let mut config = std::fs::read(root.path().join(".git/config")).unwrap();
    config.extend_from_slice(b"\n[core]\n\tsparseCheckout\n");
    std::fs::write(root.path().join(".git/config"), config).unwrap();

    let error = status(&host(root.path()))
        .await
        .expect_err("an implicit sparse flag is true and must be rejected");
    assert!(!error.outcome_unknown);
    assert!(
        error.message.to_ascii_lowercase().contains("sparse"),
        "rejection must be explicit: {}",
        error.message
    );
}
