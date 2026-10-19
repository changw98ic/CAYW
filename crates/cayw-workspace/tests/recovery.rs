#![cfg(feature = "fault-injection")]
use cayw_workspace::{CancellationToken, FaultAction, FaultPoint, Limits, Workspace};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{fs, process::Command};

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn open(root: &std::path::Path) -> Workspace {
    Workspace::open(root.canonicalize().unwrap(), Limits::default()).unwrap()
}
async fn call(
    workspace: &Workspace,
    name: &str,
    args: Value,
) -> Result<Value, cayw_workspace::CapabilityError> {
    workspace
        .execute(name, &args, CancellationToken::new())
        .await
}
async fn prepare(workspace: &Workspace) -> Value {
    call(workspace,"fs.prepare_change_batch",json!({"operations":[
        {"op":"write","path":"a.json","expected":{"kind":"sha256","sha256":hash(b"1")},"content":"10","encoding":"utf8","language":"json"},
        {"op":"write","path":"b.json","expected":{"kind":"absent"},"content":"20","encoding":"utf8","language":"json"}
    ]})).await.unwrap()
}
fn submit(prepared: &Value) -> Value {
    json!({"batch_id":prepared["batch_id"],"expected_batch_digest":prepared["batch_digest"]})
}
fn recover(prepared: &Value, action: &str) -> Value {
    let mut args = submit(prepared);
    args["action"] = json!(action);
    args
}

#[tokio::test]
async fn failure_before_first_effect_is_known_and_journal_is_aborted() {
    for fault in [FaultPoint::AfterIntent, FaultPoint::BeforeInstall(0)] {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("a.json"), b"1").unwrap();
        let workspace = open(temp.path());
        let prepared = prepare(&workspace).await;
        workspace.inject_fault(fault, FaultAction::Error);
        let error = call(&workspace, "fs.submit_change_batch", submit(&prepared))
            .await
            .unwrap_err();
        assert!(!error.outcome_unknown);
        assert_eq!(fs::read(temp.path().join("a.json")).unwrap(), b"1");
        assert!(!temp.path().join("b.json").exists());
        let reopened = open(temp.path());
        assert_eq!(
            call(
                &reopened,
                "fs.batch_status",
                json!({"batch_id":prepared["batch_id"]})
            )
            .await
            .unwrap()["status"],
            "aborted"
        );
        call(
            &reopened,
            "fs.mkdir",
            json!({"path":"allowed","expected_absent":true}),
        )
        .await
        .unwrap();
    }
}
#[tokio::test]
async fn failure_windows_reopen_and_explicitly_commit_or_rollback() {
    for fault in [
        FaultPoint::AfterInstall(0),
        FaultPoint::BeforeInstall(1),
        FaultPoint::AfterInstall(1),
        FaultPoint::BeforeCommitRecord,
    ] {
        for action in ["commit", "rollback"] {
            let temp = tempfile::tempdir().unwrap();
            fs::write(temp.path().join("a.json"), b"1").unwrap();
            let workspace = open(temp.path());
            let prepared = prepare(&workspace).await;
            workspace.inject_fault(fault.clone(), FaultAction::Error);
            assert!(
                call(&workspace, "fs.submit_change_batch", submit(&prepared))
                    .await
                    .unwrap_err()
                    .outcome_unknown
            );
            let reopened = open(temp.path());
            let status = call(
                &reopened,
                "fs.batch_status",
                json!({"batch_id":prepared["batch_id"]}),
            )
            .await
            .unwrap();
            assert_eq!(status["outcome_unknown"], true);
            assert_eq!(status["paths"][0]["matches"], "new");
            assert!(
                !call(
                    &reopened,
                    "fs.mkdir",
                    json!({"path":"blocked","expected_absent":true})
                )
                .await
                .unwrap_err()
                .outcome_unknown
            );
            let recovered = call(
                &reopened,
                "fs.recover_change_batch",
                recover(&prepared, action),
            )
            .await
            .unwrap();
            assert_eq!(recovered["outcome_unknown"], false);
            if action == "commit" {
                assert_eq!(fs::read(temp.path().join("a.json")).unwrap(), b"10");
                assert_eq!(fs::read(temp.path().join("b.json")).unwrap(), b"20");
            } else {
                assert_eq!(fs::read(temp.path().join("a.json")).unwrap(), b"1");
                assert!(!temp.path().join("b.json").exists());
            }
            let stable = open(temp.path());
            assert_eq!(
                call(
                    &stable,
                    "fs.batch_status",
                    json!({"batch_id":prepared["batch_id"]})
                )
                .await
                .unwrap()["outcome_unknown"],
                false
            );
        }
    }
}
#[tokio::test]
async fn third_party_change_blocks_recovery_before_any_write() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("a.json"), b"1").unwrap();
    let workspace = open(temp.path());
    let prepared = prepare(&workspace).await;
    workspace.inject_fault(FaultPoint::AfterInstall(0), FaultAction::Error);
    assert!(
        call(&workspace, "fs.submit_change_batch", submit(&prepared))
            .await
            .unwrap_err()
            .outcome_unknown
    );
    fs::write(temp.path().join("b.json"), b"third party").unwrap();
    let reopened = open(temp.path());
    let status = call(
        &reopened,
        "fs.batch_status",
        json!({"batch_id":prepared["batch_id"]}),
    )
    .await
    .unwrap();
    assert_eq!(status["paths"][1]["matches"], "third_party");
    for action in ["commit", "rollback"] {
        let error = call(
            &reopened,
            "fs.recover_change_batch",
            recover(&prepared, action),
        )
        .await
        .unwrap_err();
        assert!(error.outcome_unknown);
        assert_eq!(fs::read(temp.path().join("a.json")).unwrap(), b"10");
        assert_eq!(
            fs::read(temp.path().join("b.json")).unwrap(),
            b"third party"
        );
    }
}
#[tokio::test]
async fn interrupted_recovery_can_be_checked_and_resumed() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("a.json"), b"1").unwrap();
    let workspace = open(temp.path());
    let prepared = prepare(&workspace).await;
    workspace.inject_fault(FaultPoint::AfterInstall(0), FaultAction::Error);
    call(&workspace, "fs.submit_change_batch", submit(&prepared))
        .await
        .unwrap_err();
    workspace.inject_fault(FaultPoint::AfterRecoveryInstall(0), FaultAction::Error);
    assert!(
        call(
            &workspace,
            "fs.recover_change_batch",
            recover(&prepared, "rollback")
        )
        .await
        .unwrap_err()
        .outcome_unknown
    );
    let reopened = open(temp.path());
    let status = call(
        &reopened,
        "fs.batch_status",
        json!({"batch_id":prepared["batch_id"]}),
    )
    .await
    .unwrap();
    assert_eq!(status["status"], "recovering_rollback");
    assert_eq!(status["paths"][0]["matches"], "old");
    call(
        &reopened,
        "fs.recover_change_batch",
        recover(&prepared, "rollback"),
    )
    .await
    .unwrap();
    assert_eq!(fs::read(temp.path().join("a.json")).unwrap(), b"1");
}
#[tokio::test]
async fn cancellation_before_and_after_first_effect_keeps_correct_certainty() {
    for (point, uncertain) in [
        (FaultPoint::AfterIntent, false),
        (FaultPoint::AfterInstall(0), true),
    ] {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("a.json"), b"1").unwrap();
        let workspace = open(temp.path());
        let prepared = prepare(&workspace).await;
        workspace.inject_fault(point, FaultAction::Cancel);
        let cancel = CancellationToken::new();
        let error = workspace
            .execute("fs.submit_change_batch", &submit(&prepared), cancel.clone())
            .await
            .unwrap_err();
        assert!(cancel.is_cancelled());
        assert_eq!(error.outcome_unknown, uncertain);
        assert!(!temp.path().join("b.json").exists());
        let reopened = open(temp.path());
        let status = call(
            &reopened,
            "fs.batch_status",
            json!({"batch_id":prepared["batch_id"]}),
        )
        .await
        .unwrap();
        // Cancellation before the first user-path effect is a known call
        // failure, but the already durable intent remains pending until explicit
        // recovery; cleanup cannot start a fresh deadline or ignore cancellation.
        assert_eq!(status["outcome_unknown"], true);
        if uncertain {
            assert_eq!(fs::read(temp.path().join("a.json")).unwrap(), b"10");
        }
        call(
            &reopened,
            "fs.recover_change_batch",
            recover(&prepared, "rollback"),
        )
        .await
        .unwrap();
        assert_eq!(fs::read(temp.path().join("a.json")).unwrap(), b"1");
        assert_eq!(
            call(
                &reopened,
                "fs.batch_status",
                json!({"batch_id":prepared["batch_id"]})
            )
            .await
            .unwrap()["outcome_unknown"],
            false
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn swapped_parent_symlink_blocks_recovery_without_touching_outside() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join("nested")).unwrap();
    fs::write(temp.path().join("nested/a.json"), b"1").unwrap();
    fs::write(outside.path().join("a.json"), b"outside").unwrap();
    let workspace = open(temp.path());
    let prepared = call(&workspace, "fs.prepare_change_batch", json!({"operations":[
        {"op":"write","path":"nested/a.json","expected":{"kind":"sha256","sha256":hash(b"1")},"content":"10","encoding":"utf8","language":"json"},
        {"op":"write","path":"b.json","expected":{"kind":"absent"},"content":"20","encoding":"utf8","language":"json"}
    ]})).await.unwrap();
    workspace.inject_fault(FaultPoint::AfterInstall(0), FaultAction::Error);
    assert!(
        call(&workspace, "fs.submit_change_batch", submit(&prepared))
            .await
            .unwrap_err()
            .outcome_unknown
    );
    fs::rename(temp.path().join("nested"), temp.path().join("saved")).unwrap();
    symlink(outside.path(), temp.path().join("nested")).unwrap();
    let reopened = open(temp.path());
    let status = call(
        &reopened,
        "fs.batch_status",
        json!({"batch_id":prepared["batch_id"]}),
    )
    .await
    .unwrap();
    assert_eq!(status["paths"][0]["matches"], "third_party");
    assert!(
        call(
            &reopened,
            "fs.recover_change_batch",
            recover(&prepared, "rollback")
        )
        .await
        .unwrap_err()
        .outcome_unknown
    );
    assert_eq!(fs::read(outside.path().join("a.json")).unwrap(), b"outside");
    assert_eq!(fs::read(temp.path().join("saved/a.json")).unwrap(), b"10");
    assert!(!temp.path().join("b.json").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn partial_authorized_staging_after_crash_is_recovered_but_unrelated_content_is_preserved() {
    use std::os::unix::fs::PermissionsExt;
    for malicious in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("a.json"), b"1").unwrap();
        let workspace = open(temp.path());
        let prepared = prepare(&workspace).await;
        workspace.inject_fault(FaultPoint::AfterInstall(0), FaultAction::Error);
        assert!(
            call(&workspace, "fs.submit_change_batch", submit(&prepared))
                .await
                .unwrap_err()
                .outcome_unknown
        );
        let stage = temp.path().join(format!(
            ".cayw-workspace-{}-1-commit",
            prepared["batch_id"].as_str().unwrap()
        ));
        fs::write(&stage, if malicious { b"x" } else { b"2" }).unwrap();
        fs::set_permissions(&stage, fs::Permissions::from_mode(0o600)).unwrap();
        let reopened = open(temp.path());
        let result = call(
            &reopened,
            "fs.recover_change_batch",
            recover(&prepared, "commit"),
        )
        .await;
        if malicious {
            assert!(result.unwrap_err().outcome_unknown);
            assert_eq!(fs::read(&stage).unwrap(), b"x");
            assert!(!temp.path().join("b.json").exists());
        } else {
            result.unwrap();
            assert_eq!(fs::read(temp.path().join("b.json")).unwrap(), b"20");
            assert!(!stage.exists());
        }
    }
}

#[tokio::test]
async fn corrupt_candidate_does_not_change_user_files() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("a.json"), b"1").unwrap();
    let workspace = open(temp.path());
    let prepared = prepare(&workspace).await;
    let id = prepared["batch_id"].as_str().unwrap();
    fs::write(
        temp.path()
            .join(".cayw-workspace")
            .join(id)
            .join("candidate-1"),
        b"evil",
    )
    .unwrap();
    assert!(
        !call(&workspace, "fs.submit_change_batch", submit(&prepared))
            .await
            .unwrap_err()
            .outcome_unknown
    );
    assert_eq!(fs::read(temp.path().join("a.json")).unwrap(), b"1");
    assert!(!temp.path().join("b.json").exists());
}
#[tokio::test]
async fn crash_worker() {
    let Ok(root) = std::env::var("CAYW_WORKSPACE_CRASH_ROOT") else {
        return;
    };
    let prepared: Value =
        serde_json::from_str(&std::env::var("CAYW_WORKSPACE_CRASH_PREPARED").unwrap()).unwrap();
    let workspace = open(std::path::Path::new(&root));
    workspace.inject_fault(FaultPoint::AfterInstall(0), FaultAction::Crash);
    call(&workspace, "fs.submit_change_batch", submit(&prepared))
        .await
        .unwrap();
    panic!("fault did not crash");
}
#[tokio::test]
async fn actual_process_exit_leaves_durable_intent_and_path_evidence() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("a.json"), b"1").unwrap();
    let workspace = open(temp.path());
    let prepared = prepare(&workspace).await;
    let status = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "crash_worker", "--nocapture"])
        .env(
            "CAYW_WORKSPACE_CRASH_ROOT",
            temp.path().canonicalize().unwrap(),
        )
        .env("CAYW_WORKSPACE_CRASH_PREPARED", prepared.to_string())
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(86));
    let reopened = open(temp.path());
    let evidence = call(
        &reopened,
        "fs.batch_status",
        json!({"batch_id":prepared["batch_id"]}),
    )
    .await
    .unwrap();
    assert_eq!(evidence["status"], "committing");
    assert_eq!(evidence["recorded_installed"], 0);
    assert_eq!(evidence["paths"][0]["matches"], "new");
    assert_eq!(evidence["paths"][1]["matches"], "old");
    call(
        &reopened,
        "fs.recover_change_batch",
        recover(&prepared, "rollback"),
    )
    .await
    .unwrap();
    assert_eq!(fs::read(temp.path().join("a.json")).unwrap(), b"1");
    assert!(!temp.path().join("b.json").exists());
}
