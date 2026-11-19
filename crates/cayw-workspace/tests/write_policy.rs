use cayw_workspace::{CancellationToken, DirectoryWritePolicy, Limits, Workspace};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

fn open(path: &std::path::Path) -> Workspace {
    Workspace::open(path.canonicalize().unwrap(), Limits::default()).unwrap()
}
fn restricted(path: &std::path::Path) -> Workspace {
    open(path).with_write_policy(DirectoryWritePolicy::read_only("memory").unwrap())
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
fn write(path: &str) -> Value {
    json!({"op":"write","path":path,"expected":{"kind":"absent"},"encoding":"utf8","content":"1","language":"json"})
}
fn submit(batch: &Value) -> Value {
    json!({"batch_id":batch["batch_id"],"expected_batch_digest":batch["batch_digest"]})
}

#[tokio::test]
async fn protected_paths_and_preexisting_batches_cannot_write() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("memory")).unwrap();
    std::fs::write(temp.path().join("memory/entry.json"), "1").unwrap();
    let trusted = open(temp.path());
    let prepared = call(
        &trusted,
        "fs.prepare_change_batch",
        json!({"operations":[write("memory/new.json")]}),
    )
    .await
    .unwrap();
    let scope = restricted(temp.path());
    assert!(
        call(&scope, "fs.submit_change_batch", submit(&prepared))
            .await
            .is_err()
    );
    assert!(!temp.path().join("memory/new.json").exists());
    let digest = format!("{:x}", Sha256::digest(b"1"));
    for (name, args) in [
        (
            "fs.mkdir",
            json!({"path":"memory/new","expected_absent":true}),
        ),
        (
            "fs.remove",
            json!({"path":"memory/entry.json","expected_sha256":digest}),
        ),
        (
            "fs.move",
            json!({"source":"memory/entry.json","destination":"elsewhere.json","expected_sha256":digest,"expected_destination_absent":true}),
        ),
        (
            "fs.move",
            json!({"source":"outside.json","destination":"memory/another.json","expected_sha256":digest,"expected_destination_absent":true}),
        ),
        (
            "fs.prepare_change_batch",
            json!({"operations":[write("memory/new.json")]}),
        ),
    ] {
        std::fs::write(temp.path().join("outside.json"), "1").unwrap();
        let error = call(&scope, name, args).await.unwrap_err();
        assert!(error.message.contains("read-only"), "{name}: {error}");
    }
    assert_eq!(
        std::fs::read_to_string(temp.path().join("memory/entry.json")).unwrap(),
        "1"
    );
    // The same pending batch is still usable by the trusted writer.
    call(&trusted, "fs.submit_change_batch", submit(&prepared))
        .await
        .unwrap();
}

#[tokio::test]
async fn aliases_follow_the_real_volume_rules() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("memory")).unwrap();
    let insensitive = temp.path().join("Memory").exists();
    if !insensitive {
        std::fs::create_dir(temp.path().join("Memory")).unwrap();
    }
    let scope = restricted(temp.path());
    for name in ["Memory", "MEMORY", "mEmOrY"] {
        if !insensitive && !temp.path().join(name).exists() {
            std::fs::create_dir(temp.path().join(name)).unwrap();
        }
        let result = call(
            &scope,
            "fs.mkdir",
            json!({"path":format!("{name}/new"),"expected_absent":true}),
        )
        .await;
        if insensitive {
            assert!(result.unwrap_err().message.contains("read-only"));
        } else {
            result.unwrap();
        }
    }
    assert!(!temp.path().join("memory/new").exists());
}

#[cfg(feature = "fault-injection")]
#[tokio::test]
async fn interrupted_batch_recovery_checks_persisted_paths() {
    use cayw_workspace::{FaultAction, FaultPoint};
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("memory")).unwrap();
    let trusted = open(temp.path());
    let prepared = call(
        &trusted,
        "fs.prepare_change_batch",
        json!({"operations":[write("memory/new.json")]}),
    )
    .await
    .unwrap();
    trusted.inject_fault(FaultPoint::AfterInstall(0), FaultAction::Error);
    assert!(
        call(&trusted, "fs.submit_change_batch", submit(&prepared))
            .await
            .unwrap_err()
            .outcome_unknown
    );
    let scope = restricted(temp.path());
    for action in ["rollback", "commit"] {
        let mut args = submit(&prepared);
        args["action"] = json!(action);
        assert!(
            call(&scope, "fs.recover_change_batch", args)
                .await
                .unwrap_err()
                .message
                .contains("read-only")
        );
    }
    assert_eq!(
        std::fs::read_to_string(temp.path().join("memory/new.json")).unwrap(),
        "1"
    );
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn first_alias_mkdir_is_checked_before_memory_exists() {
    use std::os::fd::AsRawFd;
    let temp = tempfile::tempdir().unwrap();
    let root = std::fs::File::open(temp.path()).unwrap();
    let sensitive = unsafe { libc::fpathconf(root.as_raw_fd(), libc::_PC_CASE_SENSITIVE) };
    assert!(sensitive >= 0);
    let scope = restricted(temp.path());
    let result = call(
        &scope,
        "fs.mkdir",
        json!({"path":"Memory","expected_absent":true}),
    )
    .await;
    if sensitive == 0 {
        assert!(result.unwrap_err().message.contains("read-only"));
    } else {
        result.unwrap();
    }
    assert!(!temp.path().join("memory").exists());
}
