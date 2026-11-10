//! Offline acceptance for private per-workspace Git recovery points.
//!
//! Every test uses a new temporary workspace and a separate CAYW state
//! directory. None reads or writes a real user directory or model.

use cayw_ai::Model;
use cayw_runtime::RecoveryScope;
use cayw_runtime::recovery_points::{
    RecoveryConfig, RecoveryLimits, RecoveryPoints, RestoreRequest, SnapshotOutcome,
    detect_git_program,
};
use cayw_runtime::{Runtime, RuntimeConfig};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};
use tempfile::TempDir;

struct Fixture {
    _temporary: TempDir,
    workspace: PathBuf,
    state: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let workspace = temporary.path().join("workspace");
        let state = temporary
            .path()
            .join("state")
            .join("workspaces")
            .join("key");
        fs::create_dir_all(&workspace).expect("workspace");
        fs::create_dir_all(&state).expect("state");
        Self {
            _temporary: temporary,
            workspace,
            state,
        }
    }

    fn points(&self, git: &Path) -> RecoveryPoints {
        RecoveryPoints::open(
            &self.workspace,
            &self.state,
            RecoveryConfig {
                git_program: Some(git.to_path_buf()),
                ..Default::default()
            },
        )
    }

    fn points_with_limits(&self, git: &Path, limits: RecoveryLimits) -> RecoveryPoints {
        RecoveryPoints::open(
            &self.workspace,
            &self.state,
            RecoveryConfig {
                git_program: Some(git.to_path_buf()),
                limits,
                ..Default::default()
            },
        )
    }

    fn write(&self, path: &str, content: &str) {
        let path = self.workspace.join(path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("parent");
        }
        fs::write(path, content).expect("write");
    }

    fn read(&self, path: &str) -> String {
        fs::read_to_string(self.workspace.join(path)).expect("read")
    }
}

fn available_git() -> Option<PathBuf> {
    let git = detect_git_program();
    if git.is_none() {
        eprintln!("skipping: no usable Git on this machine");
    }
    git
}

fn point(outcome: SnapshotOutcome) -> (String, bool, bool) {
    match outcome {
        SnapshotOutcome::Ready {
            id,
            created,
            complete,
            ..
        } => (id, created, complete),
        other => panic!("expected a ready point, got {other:?}"),
    }
}

fn git_output(workspace: &Path, arguments: &[&str]) -> String {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(workspace)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("git");
    assert!(
        output.status.success(),
        "git {arguments:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("utf8")
        .trim()
        .to_owned()
}

/// Use the same safe Git program that the recovery implementation detected,
/// never the macOS `/usr/bin/git` shim.
fn git_program_output(program: &Path, workspace: &Path, arguments: &[&str]) {
    let output = Command::new(program)
        .args(arguments)
        .current_dir(workspace)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("git");
    assert!(
        output.status.success(),
        "git {arguments:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn point_tree(git: &Path, state: &Path, commit: &str) -> String {
    let repository = state.join("recovery").join("repository.git");
    let output = Command::new(git)
        .args([
            "--git-dir",
            repository.to_str().expect("utf8 repository"),
            "ls-tree",
            "-r",
            "--name-only",
            commit,
        ])
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("git ls-tree");
    assert!(output.status.success());
    String::from_utf8(output.stdout).expect("utf8")
}

#[test]
fn snapshot_restores_a_mistakenly_deleted_included_file() {
    let Some(git) = available_git() else {
        return;
    };
    let fixture = Fixture::new();
    fixture.write("src/lib.rs", "pub fn value() -> u8 { 1 }\n");
    fixture.write("README", "hello\n");
    fixture.write(".gitignore", "ignored.log\n");
    fixture.write("ignored.log", "not snapshotted\n");
    let points = fixture.points(&git);
    let (first, created, complete) = point(points.ensure_point("initial").expect("point"));
    assert!(created);
    assert!(
        !complete,
        "an ignored file is outside the recovery scope and cannot count as complete"
    );
    fs::remove_file(fixture.workspace.join("src/lib.rs")).expect("delete");
    let report = points
        .restore(RestoreRequest {
            snapshot: first.clone(),
            paths: vec!["src/lib.rs".into()],
            confirm_overwrite: vec![],
        })
        .expect("restore");
    assert_eq!(report.restored, vec!["src/lib.rs".to_owned()]);
    assert!(report.complete);
    assert_eq!(fixture.read("src/lib.rs"), "pub fn value() -> u8 { 1 }\n");
    let tree = point_tree(&git, &fixture.state, &first);
    assert!(tree.contains("src/lib.rs"));
    assert!(tree.contains("README"));
    assert!(!tree.contains("ignored.log"));
    assert!(
        !tree
            .lines()
            .any(|line| line == ".git" || line.starts_with(".git/"))
    );
    assert!(!fixture.workspace.join(".git").exists());
}

#[test]
fn user_repository_index_branch_and_history_are_unchanged() {
    let Some(git) = available_git() else {
        return;
    };
    let fixture = Fixture::new();
    fixture.write("tracked.txt", "first\n");
    git_output(&fixture.workspace, &["init", "--quiet"]);
    git_output(&fixture.workspace, &["add", "tracked.txt"]);
    git_output(
        &fixture.workspace,
        &[
            "-c",
            "user.name=User",
            "-c",
            "user.email=user@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "initial",
        ],
    );
    fixture.write("tracked.txt", "staged change\n");
    git_output(&fixture.workspace, &["add", "tracked.txt"]);
    let before_head = git_output(&fixture.workspace, &["rev-parse", "HEAD"]);
    let before_branch = git_output(&fixture.workspace, &["symbolic-ref", "HEAD"]);
    let before_index = git_output(&fixture.workspace, &["write-tree"]);
    let before_status = git_output(&fixture.workspace, &["status", "--porcelain=v1"]);
    let points = fixture.points(&git);
    let (first, _, _) = point(points.ensure_point("task start").expect("point"));
    fs::remove_file(fixture.workspace.join("tracked.txt")).expect("delete");
    points
        .restore(RestoreRequest {
            snapshot: first,
            paths: vec!["tracked.txt".into()],
            confirm_overwrite: vec![],
        })
        .expect("restore");
    assert_eq!(
        git_output(&fixture.workspace, &["rev-parse", "HEAD"]),
        before_head
    );
    assert_eq!(
        git_output(&fixture.workspace, &["symbolic-ref", "HEAD"]),
        before_branch
    );
    assert_eq!(
        git_output(&fixture.workspace, &["write-tree"]),
        before_index
    );
    let after_status = git_output(&fixture.workspace, &["status", "--porcelain=v1"]);
    let without_internal = |status: &str| {
        status
            .lines()
            .filter(|line| !line.trim_end().ends_with(".cayw-host-lock"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert_eq!(
        without_internal(&after_status),
        without_internal(&before_status)
    );
    assert_eq!(fixture.read("tracked.txt"), "staged change\n");
}

#[test]
fn unavailable_git_warns_and_ordinary_work_continues() {
    let fixture = Fixture::new();
    let missing = fixture._temporary.path().join("missing-git");
    let points = fixture.points(&missing);
    match points.ensure_point("task start").expect("warning result") {
        SnapshotOutcome::Unavailable { warning } => assert!(warning.contains("Git")),
        other => panic!("expected an unavailable warning, got {other:?}"),
    }
    let status = points.status();
    assert_eq!(status["enabled"], true);
    assert!(status["warning"].as_str().expect("warning").contains("Git"));
    // The caller keeps working; no recovery state was fabricated.
    fixture.write("still-works.txt", "ok\n");
    assert_eq!(fixture.read("still-works.txt"), "ok\n");
    assert!(!fixture.state.join("recovery").exists());
}

#[test]
fn established_points_are_never_deleted_by_later_program_operations() {
    let Some(git) = available_git() else {
        return;
    };
    let fixture = Fixture::new();
    fixture.write("file.txt", "one\n");
    let points = fixture.points(&git);
    let (first, created, _) = point(points.ensure_point("first").expect("first"));
    assert!(created);
    fixture.write("file.txt", "two\n");
    let (second, created, _) = point(points.ensure_point("second").expect("second"));
    assert!(created);
    assert_ne!(first, second);
    // Identical content does not create another point.
    let (same, created, _) = point(points.ensure_point("third").expect("third"));
    assert_eq!(same, second);
    assert!(!created);
    // Restore establishes its own undo point but never removes earlier ones.
    fs::remove_file(fixture.workspace.join("file.txt")).expect("delete");
    points
        .restore(RestoreRequest {
            snapshot: first.clone(),
            paths: vec!["file.txt".into()],
            confirm_overwrite: vec![],
        })
        .expect("restore");
    let ids = points.point_ids().expect("ids");
    assert!(ids.contains(&first));
    assert!(ids.contains(&second));
    assert!(
        fixture
            .state
            .join("recovery")
            .join("repository.git")
            .is_dir()
    );
}

#[test]
fn changes_after_the_point_need_confirmation_before_overwrite() {
    let Some(git) = available_git() else {
        return;
    };
    let fixture = Fixture::new();
    fixture.write("file.txt", "snapshot\n");
    let points = fixture.points(&git);
    let (snapshot, _, _) = point(points.ensure_point("initial").expect("point"));
    fixture.write("file.txt", "third party\n");
    let report = points
        .restore(RestoreRequest {
            snapshot: snapshot.clone(),
            paths: vec!["file.txt".into()],
            confirm_overwrite: vec![],
        })
        .expect("report");
    assert!(!report.complete);
    assert_eq!(report.needs_confirmation, vec!["file.txt".to_owned()]);
    assert!(report.restored.is_empty());
    assert_eq!(fixture.read("file.txt"), "third party\n");
    let confirmed = points
        .restore(RestoreRequest {
            snapshot,
            paths: vec!["file.txt".into()],
            confirm_overwrite: vec!["file.txt".into()],
        })
        .expect("confirmed restore");
    assert!(confirmed.complete);
    assert_eq!(fixture.read("file.txt"), "snapshot\n");
}

#[test]
fn protected_names_never_enter_a_snapshot_or_a_restore() {
    let Some(git) = available_git() else {
        return;
    };
    let fixture = Fixture::new();
    fixture.write("visible.txt", "visible\n");
    fixture.write(".cayw-workspace/hidden.txt", "hidden\n");
    fixture.write(".cayw-host-read-state", "hidden\n");
    fixture.write(".git-adjacent", "ordinary\n");
    let points = fixture.points(&git);
    let (first, _, _) = point(points.ensure_point("initial").expect("point"));
    let tree = point_tree(&git, &fixture.state, &first);
    assert!(tree.contains("visible.txt"));
    assert!(!tree.contains(".cayw-workspace"));
    assert!(!tree.contains(".cayw-host-read-state"));
    assert!(tree.contains(".git-adjacent"));
    let error = points
        .restore(RestoreRequest {
            snapshot: first,
            paths: vec![".cayw-workspace/hidden.txt".into()],
            confirm_overwrite: vec![],
        })
        .expect_err("protected restore must fail");
    assert!(!error.outcome_unknown);
}

#[test]
fn out_of_scope_content_is_named_and_never_claimed_as_complete() {
    let Some(git) = available_git() else {
        return;
    };
    let fixture = Fixture::new();
    fixture.write("small.txt", "small\n");
    fixture.write("large.bin", &"x".repeat(1024));
    let points = fixture.points_with_limits(
        &git,
        RecoveryLimits {
            max_file_bytes: 16,
            ..Default::default()
        },
    );
    match points.ensure_point("initial").expect("point") {
        SnapshotOutcome::Ready {
            id,
            complete,
            out_of_scope,
            out_of_scope_sample,
            warning,
            ..
        } => {
            assert!(!complete);
            assert_eq!(out_of_scope, 1);
            assert_eq!(out_of_scope_sample, vec!["large.bin".to_owned()]);
            assert!(warning.expect("warning").contains("不在恢复范围"));
            let tree = point_tree(&git, &fixture.state, &id);
            assert!(tree.contains("small.txt"));
            assert!(!tree.contains("large.bin"));
        }
        other => panic!("expected a ready point, got {other:?}"),
    }
    let status = points.status();
    assert_eq!(status["last_complete"], false);
    assert_eq!(status["out_of_scope"], 1);
    assert_eq!(status["out_of_scope_sample"][0], "large.bin");
}

#[test]
fn first_task_change_creates_one_point_and_ignored_content_is_backed_up() {
    let Some(git) = available_git() else {
        return;
    };
    let fixture = Fixture::new();
    fixture.write(".gitignore", "ignored.log\n");
    fixture.write("ignored.log", "ignored bytes\n");
    fixture.write("kept.txt", "kept\n");
    let points = fixture.points(&git);
    let warnings = points.before_capability(
        "task-1",
        "fs.remove",
        &serde_json::json!({"path":"ignored.log"}),
    );
    assert_eq!(points.point_ids().expect("points").len(), 1);
    assert!(warnings.iter().any(|warning| warning.contains("本地备份")));
    let backups = fixture.state.join("recovery").join("backups");
    assert!(backups.is_dir());
    let mut found = false;
    for entry in fs::read_dir(&backups).expect("backups") {
        let entry = entry.expect("entry");
        let bytes = fs::read(entry.path().join("root").join("link"))
            .or_else(|_| fs::read(entry.path().join("root").join("ignored.log")));
        if let Ok(bytes) = bytes
            && bytes == b"ignored bytes\n"
        {
            found = true;
        }
    }
    assert!(found, "ignored file backup must contain its bytes");
    // The first change already established the task point. Another single-path
    // change does not duplicate it; a multi-file operation does establish one.
    points.before_capability(
        "task-1",
        "fs.mkdir",
        &serde_json::json!({"path":"new","expected_absent":true}),
    );
    assert_eq!(points.point_ids().expect("points").len(), 1);
    fixture.write("kept.txt", "changed\n");
    points.before_capability(
        "task-1",
        "fs.submit_change_batch",
        &serde_json::json!({"batch_id":"b-123456","expected_batch_digest":"0".repeat(64)}),
    );
    assert_eq!(points.point_ids().expect("points").len(), 2);
}

#[test]
fn ignored_over_limit_and_nested_repository_paths_leave_the_recovery_scope() {
    let Some(git) = available_git() else {
        return;
    };
    let fixture = Fixture::new();
    fixture.write(".gitignore", "ignored/\nlarge.bin\nnested/\n");
    fixture.write("kept.txt", "kept\n");
    fixture.write("large.bin", &"x".repeat(1024));
    fixture.write("ignored/secret.txt", "ignored\n");
    fixture.write("nested/inside.txt", "nested\n");
    let points = fixture.points_with_limits(
        &git,
        RecoveryLimits {
            max_file_bytes: 16,
            ..Default::default()
        },
    );
    match points.ensure_point("initial").expect("point") {
        SnapshotOutcome::Ready {
            complete,
            out_of_scope,
            out_of_scope_sample,
            ..
        } => {
            assert!(!complete);
            assert_eq!(out_of_scope, out_of_scope_sample.len());
            for path in ["large.bin", "ignored", "nested"] {
                assert!(
                    out_of_scope_sample.iter().any(|entry| entry == path),
                    "sample must name {path}: {out_of_scope_sample:?}"
                );
            }
        }
        other => panic!("expected a ready point, got {other:?}"),
    }
    let scope = points
        .approval_scope(&RecoveryScope::new([fixture.workspace.clone()]))
        .expect("enabled recovery has a scope");
    assert!(scope.allows("kept.txt", &fixture.workspace));
    for path in [
        "large.bin",
        "ignored/secret.txt",
        "ignored/new.txt",
        "nested/inside.txt",
    ] {
        assert!(
            !scope.allows(path, &fixture.workspace),
            "{path} must be outside the recovery scope"
        );
    }
}

#[test]
fn nested_repository_paths_leave_the_recovery_scope() {
    let Some(git) = available_git() else {
        return;
    };
    let fixture = Fixture::new();
    fixture.write("kept.txt", "kept\n");
    let nested = fixture.workspace.join("nested");
    fs::create_dir(&nested).expect("nested directory");
    git_program_output(&git, &nested, &["init", "--quiet"]);
    fixture.write("nested/inside.txt", "nested\n");
    git_program_output(&git, &nested, &["add", "inside.txt"]);
    git_program_output(
        &git,
        &nested,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "nested",
        ],
    );
    let points = fixture.points(&git);
    match points.ensure_point("initial").expect("point") {
        SnapshotOutcome::Ready {
            out_of_scope_sample,
            ..
        } => assert!(
            out_of_scope_sample.iter().any(|entry| entry == "nested"),
            "a nested repository must be named out of scope: {out_of_scope_sample:?}"
        ),
        other => panic!("expected a ready point, got {other:?}"),
    }
    let scope = points
        .approval_scope(&RecoveryScope::new([fixture.workspace.clone()]))
        .expect("enabled recovery has a scope");
    assert!(scope.allows("kept.txt", &fixture.workspace));
    assert!(!scope.allows("nested/inside.txt", &fixture.workspace));
    assert!(!scope.allows("nested/new.txt", &fixture.workspace));
}

#[test]
fn workspace_disable_stops_new_points_and_backups_without_deleting_old_points() {
    let Some(git) = available_git() else {
        return;
    };
    let fixture = Fixture::new();
    fixture.write("kept.txt", "one\n");
    let points = fixture.points(&git);
    let (first, _, _) = point(points.ensure_point("initial").expect("point"));
    points.disable().expect("disable recovery");
    assert!(!points.enabled());
    match points
        .ensure_point("after disable")
        .expect("warning result")
    {
        SnapshotOutcome::Disabled => {}
        other => panic!("expected Disabled, got {other:?}"),
    }
    fixture.write(".gitignore", "ignored.log\n");
    fixture.write("ignored.log", "ignored\n");
    let warnings = points.before_capability(
        "task-after-disable",
        "fs.remove",
        &serde_json::json!({"path":"ignored.log"}),
    );
    assert!(
        warnings.is_empty(),
        "disabled recovery must not create local backups: {warnings:?}"
    );
    assert!(!fixture.state.join("recovery").join("backups").exists());
    assert_eq!(points.point_ids().expect("points"), vec![first]);
    assert!(
        points
            .approval_scope(&RecoveryScope::new([fixture.workspace.clone()]))
            .is_none(),
        "disabled recovery must ask for existing file changes"
    );
    let config: serde_json::Value = serde_json::from_slice(
        &fs::read(fixture.state.join("recovery").join("config.json")).expect("config"),
    )
    .expect("config json");
    assert_eq!(config["enabled"], false);
}

#[test]
fn interrupted_restore_keeps_outcome_unknown_and_inspection_is_read_only() {
    let Some(git) = available_git() else {
        return;
    };
    let fixture = Fixture::new();
    fixture.write("x.txt", "snapshot\n");
    let points = fixture.points(&git);
    let (snapshot, _, _) = point(points.ensure_point("initial").expect("point"));
    // Replace the file with a directory so the install step fails after the
    // restore intent is durable but before the path is installed.
    fs::remove_file(fixture.workspace.join("x.txt")).expect("remove file");
    fs::create_dir(fixture.workspace.join("x.txt")).expect("directory");
    fixture.write("x.txt/child.txt", "third party\n");
    let error = points
        .restore(RestoreRequest {
            snapshot,
            paths: vec!["x.txt".into()],
            confirm_overwrite: vec!["x.txt".into()],
        })
        .expect_err("directory target interrupts the restore");
    assert!(error.outcome_unknown);
    let intent_path = fixture.state.join("recovery").join("restore-intent.json");
    let before = fs::read(&intent_path).expect("restore intent");
    let first = points.reconciliation();
    assert_eq!(first["outcome_unknown"], true);
    assert_eq!(first["unfinished"], true);
    assert_eq!(first["status"], "outcome_unknown");
    assert_eq!(first["pending"], serde_json::json!(["x.txt"]));
    assert_eq!(first["failed"], "x.txt");
    assert_eq!(first["read_only"], true);
    let second = points.reconciliation();
    assert_eq!(first, second, "inspection is deterministic and read-only");
    assert_eq!(
        fs::read(&intent_path).expect("restore intent"),
        before,
        "inspection never rewrites the unfinished intent"
    );
    assert_eq!(
        fs::read(fixture.workspace.join("x.txt/child.txt")).expect("child"),
        b"third party\n",
        "inspection never replays a restore write"
    );
    let status = points.status();
    assert_eq!(status["restore_outcome_unknown"], true);
    assert_eq!(status["restore_reconciliation"]["unfinished"], true);
}

#[test]
fn runtime_program_entry_disables_recovery_without_deleting_points() {
    let Some(git) = available_git() else {
        return;
    };
    let fixture = Fixture::new();
    fixture.write("kept.txt", "one\n");
    let runtime = Runtime::open(
        fixture._temporary.path().join("session.db"),
        &fixture.workspace,
        std::sync::Arc::new(cayw_runtime::fixture::OfflineProvider::default()),
        Model::new("fixture", "offline-runtime"),
        RuntimeConfig {
            recovery: Some(RecoveryConfig {
                git_program: Some(git),
                data_dir: Some(fixture.state.clone()),
                ..Default::default()
            }),
            recovery_scope: Some(RecoveryScope::new([fixture.workspace.clone()])),
            ..Default::default()
        },
    )
    .expect("runtime opens");
    let created = runtime.create_recovery_point().expect("initial point");
    assert_eq!(created["status"], "ready");
    let points_before = runtime.recovery_points().expect("points");
    assert_eq!(points_before.len(), 1);

    let disabled = runtime.disable_git_recovery().expect("disable");
    assert_eq!(disabled["enabled"], false);
    assert_eq!(disabled["points_retained"], 1);
    assert_eq!(runtime.recovery_status().expect("status")["enabled"], false);
    assert_eq!(
        runtime.recovery_points().expect("retained points"),
        points_before,
        "disabling must not delete existing points"
    );
    let after = runtime.create_recovery_point().expect("disabled result");
    assert_eq!(after["status"], "disabled");
    assert_eq!(
        runtime.recovery_reconciliation().expect("inspection")["outcome_unknown"],
        false
    );
}
