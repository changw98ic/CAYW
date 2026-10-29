//! WB-01 regression tests: durable ownership store with bare, dot-slash, and absolute paths.
//!
//! Verifies that bare relative path ("ownership.json") and dot-slash relative path ("./ownership.json")
//! persist and reopen cleanly when run inside a dedicated working directory in a child process,
//! without modifying process-global working directory in the test runner.
//! Also verifies absolute path handling and ensures that real write failures propagate errors.

use cayw_webui::OwnershipStore;
use std::{env, fs, process::Command};
use tempfile::tempdir;

/// Child worker entrypoint for testing relative paths with isolated process working directory.
/// When executed as part of the normal cargo test suite, it exits immediately without action.
#[test]
fn child_worker_runner() {
    let mode = match env::var("CAYW_REVIEW_OWNERSHIP_CHILD_MODE") {
        Ok(m) => m,
        Err(_) => return,
    };
    match mode.as_str() {
        "bare_relative" => {
            let store = OwnershipStore::open("ownership.json")
                .expect("open bare ownership.json must succeed");
            store
                .bind_session("sess-bare-child", "dev-bare-1")
                .expect("bind_session with bare ownership.json must succeed");
            store
                .bind_task("task-bare-child", "dev-bare-1")
                .expect("bind_task with bare ownership.json must succeed");
            assert_eq!(
                store.session_device("sess-bare-child").as_deref(),
                Some("dev-bare-1")
            );
            assert_eq!(
                store.task_device("task-bare-child").as_deref(),
                Some("dev-bare-1")
            );

            let reopened = OwnershipStore::open("ownership.json")
                .expect("reopen bare ownership.json must succeed");
            assert_eq!(
                reopened.session_device("sess-bare-child").as_deref(),
                Some("dev-bare-1"),
                "reopened store must retain session affinity"
            );
            assert_eq!(
                reopened.task_device("task-bare-child").as_deref(),
                Some("dev-bare-1"),
                "reopened store must retain task affinity"
            );
            assert_eq!(reopened.session_count(), 1);
            assert_eq!(reopened.task_count(), 1);
        }
        "dot_slash_relative" => {
            let store = OwnershipStore::open("./ownership.json")
                .expect("open ./ownership.json must succeed");
            store
                .bind_session("sess-dot-child", "dev-dot-1")
                .expect("bind_session with ./ownership.json must succeed");
            store
                .bind_task("task-dot-child", "dev-dot-1")
                .expect("bind_task with ./ownership.json must succeed");
            assert_eq!(
                store.session_device("sess-dot-child").as_deref(),
                Some("dev-dot-1")
            );
            assert_eq!(
                store.task_device("task-dot-child").as_deref(),
                Some("dev-dot-1")
            );

            let reopened = OwnershipStore::open("./ownership.json")
                .expect("reopen ./ownership.json must succeed");
            assert_eq!(
                reopened.session_device("sess-dot-child").as_deref(),
                Some("dev-dot-1"),
                "reopened store must retain session affinity"
            );
            assert_eq!(
                reopened.task_device("task-dot-child").as_deref(),
                Some("dev-dot-1"),
                "reopened store must retain task affinity"
            );
            assert_eq!(reopened.session_count(), 1);
            assert_eq!(reopened.task_count(), 1);
        }
        other => panic!("unknown child mode: {other}"),
    }
}

#[test]
fn default_bare_relative_ownership_persists_and_restores_in_child_process() {
    let dir = tempdir().expect("create tempdir");
    let current_exe = env::current_exe().expect("must determine current test executable");
    let output = Command::new(&current_exe)
        .current_dir(dir.path())
        .env("CAYW_REVIEW_OWNERSHIP_CHILD_MODE", "bare_relative")
        .args(["--exact", "child_worker_runner", "--nocapture"])
        .output()
        .expect("must execute child test process");

    assert!(
        output.status.success(),
        "child process failed for bare relative ownership.json: stdout: {}, stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let json_path = dir.path().join("ownership.json");
    assert!(
        json_path.exists(),
        "ownership.json file must exist in child process working directory"
    );

    let content = fs::read_to_string(&json_path).expect("read persisted ownership.json");
    let parsed: serde_json::Value =
        serde_json::from_str(&content).expect("persisted content must be valid json");
    assert_eq!(
        parsed["session_devices"]["sess-bare-child"], "dev-bare-1",
        "persisted session_devices must match"
    );
    assert_eq!(
        parsed["task_devices"]["task-bare-child"], "dev-bare-1",
        "persisted task_devices must match"
    );
}

#[test]
fn dot_slash_relative_ownership_persists_and_restores_in_child_process() {
    let dir = tempdir().expect("create tempdir");
    let current_exe = env::current_exe().expect("must determine current test executable");
    let output = Command::new(&current_exe)
        .current_dir(dir.path())
        .env("CAYW_REVIEW_OWNERSHIP_CHILD_MODE", "dot_slash_relative")
        .args(["--exact", "child_worker_runner", "--nocapture"])
        .output()
        .expect("must execute child test process");

    assert!(
        output.status.success(),
        "child process failed for ./ownership.json: stdout: {}, stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let json_path = dir.path().join("ownership.json");
    assert!(
        json_path.exists(),
        "ownership.json file must exist in child process working directory"
    );

    let content = fs::read_to_string(&json_path).expect("read persisted ownership.json");
    let parsed: serde_json::Value =
        serde_json::from_str(&content).expect("persisted content must be valid json");
    assert_eq!(
        parsed["session_devices"]["sess-dot-child"], "dev-dot-1",
        "persisted session_devices must match"
    );
    assert_eq!(
        parsed["task_devices"]["task-dot-child"], "dev-dot-1",
        "persisted task_devices must match"
    );
}

#[test]
fn absolute_path_ownership_bind_and_reopen() {
    let dir = tempdir().expect("create tempdir");
    let path = dir.path().join("nested").join("custom_ownership.json");

    {
        let store = OwnershipStore::open(&path).expect("open nested absolute path must succeed");
        store
            .bind_session("sess-abs-1", "dev-abs-primary")
            .expect("bind_session must succeed");
        store
            .bind_task("task-abs-1", "dev-abs-primary")
            .expect("bind_task must succeed");

        assert_eq!(
            store.session_device("sess-abs-1").as_deref(),
            Some("dev-abs-primary")
        );
        assert_eq!(
            store.task_device("task-abs-1").as_deref(),
            Some("dev-abs-primary")
        );
        assert_eq!(store.session_count(), 1);
        assert_eq!(store.task_count(), 1);

        store
            .clear_session("sess-abs-1")
            .expect("clear_session must succeed");
        assert_eq!(store.session_device("sess-abs-1"), None);
        assert_eq!(store.session_count(), 0);

        store
            .bind_session("sess-abs-2", "dev-abs-secondary")
            .expect("bind_session after clear must succeed");
    }

    let restored = OwnershipStore::open(&path).expect("reopen nested absolute path must succeed");
    assert_eq!(restored.session_device("sess-abs-1"), None);
    assert_eq!(
        restored.session_device("sess-abs-2").as_deref(),
        Some("dev-abs-secondary")
    );
    assert_eq!(
        restored.task_device("task-abs-1").as_deref(),
        Some("dev-abs-primary")
    );
    assert_eq!(restored.session_count(), 1);
    assert_eq!(restored.task_count(), 1);
}

#[test]
#[cfg(unix)]
fn real_write_failure_propagates_error_without_swallowing() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempdir().expect("create tempdir");
    let unwriteable_dir = dir.path().join("unwriteable_dir");
    fs::create_dir(&unwriteable_dir).expect("create unwriteable directory");
    let path = unwriteable_dir.join("ownership.json");

    // Make directory read-only so creating temp files inside it fails
    fs::set_permissions(&unwriteable_dir, fs::Permissions::from_mode(0o500))
        .expect("set read-only permissions");

    let store_result = OwnershipStore::open(&path);
    if let Ok(store) = store_result {
        let bind_result = store.bind_session("sess-fail", "dev-fail");
        assert!(
            bind_result.is_err(),
            "bind_session must fail when directory is read-only"
        );
        let error = bind_result.unwrap_err();
        assert!(
            matches!(
                error.kind(),
                std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::Other
            ),
            "error kind must be PermissionDenied or related I/O error, got: {:?}",
            error.kind()
        );
    }

    // Restore permissions so tempfile directory cleanup succeeds
    let _ = fs::set_permissions(&unwriteable_dir, fs::Permissions::from_mode(0o700));
}

#[test]
fn empty_path_rejected_at_open() {
    let err = OwnershipStore::open("")
        .err()
        .expect("empty path must be rejected");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
}
