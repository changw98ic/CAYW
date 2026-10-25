use cayw_workspace::{CancellationToken, CapabilityError, Limits, Workspace};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{fs, path::Path, process::Command, time::Duration};
use tempfile::TempDir;

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn workspace(temp: &TempDir) -> Workspace {
    Workspace::open(temp.path().canonicalize().unwrap(), Limits::default()).unwrap()
}
async fn call(workspace: &Workspace, name: &str, args: Value) -> Result<Value, CapabilityError> {
    workspace
        .execute(name, &args, CancellationToken::new())
        .await
}
fn write(path: &str, content: &str) -> Value {
    json!({"op":"write","path":path,"expected":{"kind":"absent"},"encoding":"utf8","content":content,"language":"json"})
}
fn submit(prepared: &Value) -> Value {
    json!({"batch_id":prepared["batch_id"],"expected_batch_digest":prepared["batch_digest"]})
}

#[cfg(unix)]
#[tokio::test]
async fn an_existing_directory_handle_stays_pinned_after_the_path_is_replaced() {
    let temp = tempfile::tempdir().unwrap();
    let original = temp.path().join("root");
    let moved = temp.path().join("moved");
    fs::create_dir(&original).unwrap();
    let handle =
        cap_std::fs::Dir::open_ambient_dir(&original, cap_std::ambient_authority()).unwrap();
    fs::rename(&original, &moved).unwrap();
    fs::create_dir(&original).unwrap();
    let workspace = Workspace::open_dir(handle, Limits::default()).unwrap();
    let prepared = call(
        &workspace,
        "fs.prepare_change_batch",
        json!({"operations":[write("note.json", "{\"pinned\":true}")]}),
    )
    .await
    .unwrap();
    call(&workspace, "fs.submit_change_batch", submit(&prepared))
        .await
        .unwrap();
    assert_eq!(
        fs::read(moved.join("note.json")).unwrap(),
        b"{\"pinned\":true}"
    );
    assert!(!original.join("note.json").exists());
    assert!(!original.join(".cayw-workspace").exists());
    assert!(moved.join(".cayw-workspace").is_dir());
}

#[tokio::test]
async fn prepare_is_inert_commit_preserves_modes_and_exact_candidates() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("a.json"), b"{\"old\":true}").unwrap();
    fs::write(temp.path().join("gone.json"), b"null").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            temp.path().join("a.json"),
            fs::Permissions::from_mode(0o640),
        )
        .unwrap();
    }
    let workspace = workspace(&temp);
    let prepared = call(&workspace, "fs.prepare_change_batch", json!({"operations":[
        {"op":"patch","path":"a.json","expected_sha256":hash(b"{\"old\":true}"),"old":"true","new":"false","language":"json"},
        write("new.json", "[1,2,3]"),
        {"op":"remove","path":"gone.json","expected_sha256":hash(b"null")}
    ]})).await.unwrap();
    assert_eq!(prepared["user_files_changed"], false);
    assert_eq!(
        prepared["operations"][0]["candidate"]["sha256"],
        hash(b"{\"old\":false}")
    );
    assert_eq!(
        fs::read(temp.path().join("a.json")).unwrap(),
        b"{\"old\":true}"
    );
    assert!(!temp.path().join("new.json").exists());
    assert!(temp.path().join("gone.json").exists());
    let reopened = self::workspace(&temp);
    let result = call(&reopened, "fs.submit_change_batch", submit(&prepared))
        .await
        .unwrap();
    assert_eq!(result["status"], "committed");
    assert_eq!(
        fs::read(temp.path().join("a.json")).unwrap(),
        b"{\"old\":false}"
    );
    assert_eq!(fs::read(temp.path().join("new.json")).unwrap(), b"[1,2,3]");
    assert!(!temp.path().join("gone.json").exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(temp.path().join("a.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o640
        );
        assert_eq!(
            fs::metadata(temp.path().join("new.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o600
        );
    }
    assert!(
        result["paths"]
            .as_array()
            .unwrap()
            .iter()
            .all(|path| path["matches"] == "new")
    );
    assert!(
        !call(&reopened, "fs.submit_change_batch", submit(&prepared))
            .await
            .unwrap_err()
            .outcome_unknown
    );
}

#[tokio::test]
async fn syntax_validation_and_strict_arguments_are_real() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = workspace(&temp);
    for input in [
        write("bad.json", "{"),
        json!({"op":"write","path":"bad.toml","expected":{"kind":"absent"},"encoding":"utf8","content":"x = [","language":"toml"}),
        json!({"op":"write","path":"a.rs","expected":{"kind":"absent"},"encoding":"utf8","content":"fn main() {}","language":"rust"}),
    ] {
        assert!(
            !call(
                &workspace,
                "fs.prepare_change_batch",
                json!({"operations":[input]})
            )
            .await
            .unwrap_err()
            .outcome_unknown
        );
    }
    let valid = call(&workspace, "fs.prepare_change_batch", json!({"operations":[{"op":"write","path":"a.toml","expected":{"kind":"absent"},"encoding":"utf8","content":"answer = 42","language":"toml"}]})).await.unwrap();
    call(&workspace, "fs.submit_change_batch", submit(&valid))
        .await
        .unwrap();
    assert!(
        call(
            &workspace,
            "fs.mkdir",
            json!({"path":"ok","expected_absent":true,"extra":true})
        )
        .await
        .is_err()
    );
    assert!(
        call(
            &workspace,
            "fs.prepare_change_batch",
            json!({"operations":[write("dup.json","null"),write("dup.json","null")]})
        )
        .await
        .is_err()
    );
    // Case variants are distinct names on a case-sensitive volume and the
    // same target on a case-insensitive one.
    let case = call(
        &workspace,
        "fs.prepare_change_batch",
        json!({"operations":[write("a.json","1"),write("A.json","2")]}),
    )
    .await;
    if temp.path().join(".CAYW-WORKSPACE").exists() {
        assert!(case.is_err());
    } else {
        call(&workspace, "fs.submit_change_batch", submit(&case.unwrap()))
            .await
            .unwrap();
        assert_eq!(fs::read(temp.path().join("a.json")).unwrap(), b"1");
        assert_eq!(fs::read(temp.path().join("A.json")).unwrap(), b"2");
    }
    assert!(
        call(
            &workspace,
            "fs.prepare_change_batch",
            json!({"operations":[write("a","null"),write("a/b","null")]})
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn overlapping_patch_matches_are_rejected_for_ascii_and_multibyte_text() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = workspace(&temp);
    for (before, old) in [("\"aaa\"", "aa"), ("\"ééé\"", "éé")] {
        fs::write(temp.path().join("overlap.json"), before).unwrap();
        let error = call(
            &workspace,
            "fs.prepare_change_batch",
            json!({"operations":[{"op":"patch","path":"overlap.json","expected_sha256":hash(before.as_bytes()),"old":old,"new":"b","language":"json"}]}),
        )
        .await
        .expect_err("overlapping matches must not be treated as unique");
        assert!(!error.outcome_unknown);
        assert!(error.message.contains("exactly once"));
        assert_eq!(
            fs::read(temp.path().join("overlap.json")).unwrap(),
            before.as_bytes()
        );
        assert_eq!(
            fs::read_dir(temp.path().join(".cayw-workspace"))
                .unwrap()
                .count(),
            0
        );
    }
    fs::write(temp.path().join("overlap.json"), "\"éé\"").unwrap();
    let prepared = call(
        &workspace,
        "fs.prepare_change_batch",
        json!({"operations":[{"op":"patch","path":"overlap.json","expected_sha256":hash("\"éé\"".as_bytes()),"old":"éé","new":"b","language":"json"}]}),
    )
    .await
    .unwrap();
    call(&workspace, "fs.submit_change_batch", submit(&prepared))
        .await
        .unwrap();
    assert_eq!(
        fs::read_to_string(temp.path().join("overlap.json")).unwrap(),
        "\"b\""
    );
}

#[tokio::test]
async fn errors_obey_serialized_output_budget_and_preserve_unknown_outcomes() {
    struct UnknownValidator;
    impl cayw_workspace::SyntaxValidator for UnknownValidator {
        fn validate(&self, _: &str, _: &[u8]) -> Result<(), CapabilityError> {
            Err(CapabilityError::unknown("\"\\\n中".repeat(8192)))
        }
    }
    let temp = tempfile::tempdir().unwrap();
    let bounded = Workspace::open(
        temp.path().canonicalize().unwrap(),
        Limits {
            max_output_bytes: 128,
            ..Limits::default()
        },
    )
    .unwrap();
    let error = call(
        &bounded,
        "fs.prepare_change_batch",
        json!({"operations":[{"op":"write","path":"bad.toml","expected":{"kind":"absent"},"encoding":"utf8","content":format!("a = {}", "[".repeat(8192)),"language":"toml"}]}),
    ).await.unwrap_err();
    assert!(!error.outcome_unknown);
    assert!(serde_json::to_vec(&error.message).unwrap().len() <= 128);
    assert!(!temp.path().join("bad.toml").exists());
    for limit in [2, 3, 8, 64, 128] {
        let bounded = Workspace::open(
            temp.path().canonicalize().unwrap(),
            Limits {
                max_output_bytes: limit,
                syntax_validator: std::sync::Arc::new(UnknownValidator),
                ..Limits::default()
            },
        )
        .unwrap();
        let error = call(
            &bounded,
            "fs.prepare_change_batch",
            json!({"operations":[write("unknown.json", "null")]}),
        )
        .await
        .unwrap_err();
        assert!(error.outcome_unknown);
        assert!(serde_json::to_vec(&error.message).unwrap().len() <= limit);
        assert!(!temp.path().join("unknown.json").exists());
    }
}

#[tokio::test]
async fn digest_conflict_and_late_multifile_conflict_change_nothing() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = workspace(&temp);
    let prepared = call(
        &workspace,
        "fs.prepare_change_batch",
        json!({"operations":[write("first.json","1"),write("second.json","2")]}),
    )
    .await
    .unwrap();
    let mut bad = submit(&prepared);
    bad["expected_batch_digest"] = json!("0".repeat(64));
    assert!(
        !call(&workspace, "fs.submit_change_batch", bad)
            .await
            .unwrap_err()
            .outcome_unknown
    );
    fs::write(temp.path().join("second.json"), b"third party").unwrap();
    assert!(
        !call(&workspace, "fs.submit_change_batch", submit(&prepared))
            .await
            .unwrap_err()
            .outcome_unknown
    );
    assert!(!temp.path().join("first.json").exists());
    assert_eq!(
        fs::read(temp.path().join("second.json")).unwrap(),
        b"third party"
    );
    let status = call(
        &workspace,
        "fs.batch_status",
        json!({"batch_id":prepared["batch_id"]}),
    )
    .await
    .unwrap();
    assert_eq!(status["status"], "prepared");
    assert_eq!(status["paths"][1]["matches"], "third_party");
}

#[tokio::test]
async fn tree_move_exact_delete_and_recursive_hash() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = workspace(&temp);
    call(
        &workspace,
        "fs.mkdir",
        json!({"path":"dir","expected_absent":true}),
    )
    .await
    .unwrap();
    fs::write(temp.path().join("dir/a.json"), b"1").unwrap();
    assert!(
        call(&workspace, "fs.remove", json!({"path":"dir"}))
            .await
            .is_err()
    );
    assert!(call(&workspace,"fs.move",json!({"source":"dir/a.json","destination":"dir/b.json","expected_sha256":hash(b"2"),"expected_destination_absent":true})).await.is_err());
    call(&workspace,"fs.move",json!({"source":"dir/a.json","destination":"dir/b.json","expected_sha256":hash(b"1"),"expected_destination_absent":true})).await.unwrap();
    assert!(!temp.path().join("dir/a.json").exists());
    fs::write(temp.path().join("dir/existing"), b"keep").unwrap();
    assert!(call(&workspace,"fs.move",json!({"source":"dir/b.json","destination":"dir/existing","expected_sha256":hash(b"1"),"expected_destination_absent":true})).await.is_err());
    let tree = call(
        &workspace,
        "fs.tree",
        json!({"path":"dir","max_entries":10}),
    )
    .await
    .unwrap();
    assert_eq!(tree["entries"].as_array().unwrap().len(), 3);
    assert!(
        call(&workspace, "fs.tree", json!({"path":"dir","max_entries":2}))
            .await
            .is_err()
    );
    fs::write(temp.path().join("dir/b.json"), b"2").unwrap();
    assert!(call(&workspace,"fs.remove",json!({"path":"dir","recursive":true,"expected_root_sha256":tree["root_sha256"],"max_entries":10})).await.is_err());
    let tree = call(
        &workspace,
        "fs.tree",
        json!({"path":"dir","max_entries":10}),
    )
    .await
    .unwrap();
    assert!(
        call(
            &workspace,
            "fs.remove",
            json!({"path":"dir","recursive":true,"expected_root_sha256":tree["root_sha256"]})
        )
        .await
        .is_err()
    );
    call(&workspace,"fs.remove",json!({"path":"dir","recursive":true,"expected_root_sha256":tree["root_sha256"],"max_entries":10})).await.unwrap();
    assert!(!temp.path().join("dir").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn nofollow_reserved_names_special_files_and_root_links() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("secret"), b"do not touch").unwrap();
    let workspace = workspace(&temp);
    for path in [
        "../bad",
        "/tmp/bad",
        "a//b",
        "a/./b",
        ".git/config",
        ".cayw-workspace/x",
        ".cayw-host-lock",
        "a\u{0}b",
    ] {
        assert!(
            call(
                &workspace,
                "fs.mkdir",
                json!({"path":path,"expected_absent":true})
            )
            .await
            .is_err(),
            "{path:?}"
        );
    }
    // macOS accepts UTF-8 and names that Windows forbids, and only the actual
    // internal objects above are reserved. User `.cayw-*` names stay ordinary.
    for path in [
        "CON",
        "a.",
        "a ",
        "a:b",
        "a\\b",
        ".cayw-note.md",
        "说明.txt",
    ] {
        call(
            &workspace,
            "fs.mkdir",
            json!({"path":path,"expected_absent":true}),
        )
        .await
        .unwrap_or_else(|error| panic!("{path:?}: {error}"));
    }
    assert!(temp.path().join("说明.txt").is_dir());
    assert!(temp.path().join(".cayw-note.md").is_dir());
    symlink(outside.path(), temp.path().join("escape")).unwrap();
    symlink(outside.path().join("secret"), temp.path().join("link")).unwrap();
    for path in ["escape/secret", "link"] {
        assert!(
            call(
                &workspace,
                "fs.remove",
                json!({"path":path,"expected_sha256":hash(b"do not touch")})
            )
            .await
            .is_err()
        );
        assert!(
            call(
                &workspace,
                "fs.prepare_change_batch",
                json!({"operations":[write(path,"null")]})
            )
            .await
            .is_err()
        );
    }
    assert!(
        call(&workspace, "fs.tree", json!({"path":".","max_entries":100}))
            .await
            .is_err()
    );
    assert_eq!(
        fs::read(outside.path().join("secret")).unwrap(),
        b"do not touch"
    );
    fs::remove_file(temp.path().join("escape")).unwrap();
    fs::remove_file(temp.path().join("link")).unwrap();
    fs::create_dir(temp.path().join("protected")).unwrap();
    fs::create_dir(temp.path().join("protected/.git")).unwrap();
    let tree = call(
        &workspace,
        "fs.tree",
        json!({"path":"protected","max_entries":10}),
    )
    .await
    .unwrap();
    assert!(call(&workspace,"fs.remove",json!({"path":"protected","recursive":true,"expected_root_sha256":tree["root_sha256"],"max_entries":10})).await.is_err());
    let root_link = outside.path().join("root-link");
    symlink(temp.path(), &root_link).unwrap();
    assert!(Workspace::open(root_link, Limits::default()).is_err());
    let fifo = temp.path().join("fifo");
    assert!(
        Command::new("/usr/bin/mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    assert!(
        call(
            &workspace,
            "fs.remove",
            json!({"path":"fifo","expected_sha256":hash(b"")})
        )
        .await
        .is_err()
    );
    assert!(
        call(
            &workspace,
            "archive.list",
            json!({"path":"fifo","format":"zip"})
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn cancellation_waits_on_the_exact_host_lock_and_never_detaches() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = workspace(&temp);
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(temp.path().join(".cayw-host-lock"))
        .unwrap();
    file.lock().unwrap();
    let cancel = CancellationToken::new();
    let arguments = json!({"path":"never","expected_absent":true});
    let future = workspace.execute("fs.mkdir", &arguments, cancel.clone());
    tokio::pin!(future);
    tokio::select! { _ = tokio::time::sleep(Duration::from_millis(20)) => {}, result = &mut future => panic!("ignored host lock: {result:?}") }
    cancel.cancel();
    assert!(!future.await.unwrap_err().outcome_unknown);
    file.unlock().unwrap();
    assert!(!temp.path().join("never").exists());
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert!(
        workspace
            .execute("fs.tree", &json!({"path":".","max_entries":10}), cancelled)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn hardlink_aliases_do_not_make_writes_follow_another_path() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("source.json"), b"1").unwrap();
    fs::hard_link(
        temp.path().join("source.json"),
        temp.path().join("alias.json"),
    )
    .unwrap();
    let workspace = workspace(&temp);
    let prepared = call(&workspace, "fs.prepare_change_batch", json!({"operations":[
        {"op":"write","path":"source.json","expected":{"kind":"sha256","sha256":hash(b"1")},"content":"2","encoding":"utf8","language":"json"}
    ]})).await.unwrap();
    call(&workspace, "fs.submit_change_batch", submit(&prepared))
        .await
        .unwrap();
    assert_eq!(fs::read(temp.path().join("source.json")).unwrap(), b"2");
    assert_eq!(fs::read(temp.path().join("alias.json")).unwrap(), b"1");
    call(
        &workspace,
        "fs.remove",
        json!({"path":"alias.json","expected_sha256":hash(b"1")}),
    )
    .await
    .unwrap();
    assert_eq!(fs::read(temp.path().join("source.json")).unwrap(), b"2");
}

#[tokio::test]
async fn limits_fail_before_user_changes_and_invalid_shapes_are_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let small = Workspace::open(
        temp.path().canonicalize().unwrap(),
        Limits {
            max_file_bytes: 2,
            max_batch_bytes: 3,
            max_batches: 1,
            ..Limits::default()
        },
    )
    .unwrap();
    assert!(
        call(
            &small,
            "fs.prepare_change_batch",
            json!({"operations":[write("too-big.json","null")]})
        )
        .await
        .is_err()
    );
    let prepared = call(
        &small,
        "fs.prepare_change_batch",
        json!({"operations":[write("one.json","1")]}),
    )
    .await
    .unwrap();
    assert!(
        call(
            &small,
            "fs.prepare_change_batch",
            json!({"operations":[write("two.json","2")]})
        )
        .await
        .is_err()
    );
    assert!(!temp.path().join("one.json").exists());
    call(&small, "fs.submit_change_batch", submit(&prepared))
        .await
        .unwrap();
    assert!(
        call(&small, "fs.batch_status", json!({"batch_id":null}))
            .await
            .is_err()
    );
    call(
        &small,
        "fs.mkdir",
        json!({"path":"é","expected_absent":true}),
    )
    .await
    .unwrap();
    assert!(temp.path().join("é").is_dir());
    assert!(
        Workspace::open(
            temp.path().canonicalize().unwrap(),
            Limits {
                max_output_bytes: 1,
                ..Limits::default()
            },
        )
        .is_err()
    );
    let tiny_output = Workspace::open(
        temp.path().canonicalize().unwrap(),
        Limits {
            max_output_bytes: 2,
            ..Limits::default()
        },
    )
    .unwrap();
    assert!(
        call(
            &tiny_output,
            "fs.prepare_change_batch",
            json!({"operations":[write("not-written.json","1")]})
        )
        .await
        .is_err()
    );
    assert!(!temp.path().join("not-written.json").exists());
    assert!(
        Workspace::open(
            temp.path().canonicalize().unwrap(),
            Limits {
                timeout: Duration::ZERO,
                ..Limits::default()
            }
        )
        .is_err()
    );
    assert!(
        Workspace::open(
            temp.path().canonicalize().unwrap(),
            Limits {
                max_depth: 0,
                ..Limits::default()
            }
        )
        .is_err()
    );
}

#[tokio::test]
async fn patch_preconditions_reject_missing_files_and_absent_old_text() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = workspace(&temp);
    let missing = call(
        &workspace,
        "fs.prepare_change_batch",
        json!({"operations":[{"op":"patch","path":"missing.json","expected_sha256":hash(b"null"),"old":"null","new":"true","language":"json"}]}),
    )
    .await
    .unwrap_err();
    assert!(!missing.outcome_unknown);
    assert!(
        missing
            .message
            .contains("compare-and-swap absence conflict"),
        "{missing}"
    );
    assert!(!temp.path().join("missing.json").exists());
    assert_eq!(
        fs::read_dir(temp.path().join(".cayw-workspace"))
            .unwrap()
            .count(),
        0
    );

    fs::write(temp.path().join("existing.json"), b"{\"old\":true}").unwrap();
    let before = fs::read(temp.path().join("existing.json")).unwrap();
    let unmatched = call(
        &workspace,
        "fs.prepare_change_batch",
        json!({"operations":[{"op":"patch","path":"existing.json","expected_sha256":hash(&before),"old":"missing","new":"false","language":"json"}]}),
    )
    .await
    .unwrap_err();
    assert!(!unmatched.outcome_unknown);
    assert!(
        unmatched.message.contains("patch text not found"),
        "{unmatched}"
    );
    assert_eq!(fs::read(temp.path().join("existing.json")).unwrap(), before);
    assert_eq!(
        fs::read_dir(temp.path().join(".cayw-workspace"))
            .unwrap()
            .count(),
        0
    );
}

#[tokio::test]
async fn mkdir_requires_an_existing_parent_and_an_absent_target() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = workspace(&temp);
    let missing_parent = call(
        &workspace,
        "fs.mkdir",
        json!({"path":"missing/child","expected_absent":true}),
    )
    .await
    .unwrap_err();
    assert!(!missing_parent.outcome_unknown);
    assert!(!temp.path().join("missing").exists());

    call(
        &workspace,
        "fs.mkdir",
        json!({"path":"existing","expected_absent":true}),
    )
    .await
    .unwrap();
    let existing = call(
        &workspace,
        "fs.mkdir",
        json!({"path":"existing","expected_absent":true}),
    )
    .await
    .unwrap_err();
    assert!(!existing.outcome_unknown);
    assert!(temp.path().join("existing").is_dir());
    assert_eq!(
        fs::read_dir(temp.path().join("existing")).unwrap().count(),
        0
    );
}

#[tokio::test]
async fn batch_status_without_an_id_lists_every_published_journal() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = workspace(&temp);
    let first = call(
        &workspace,
        "fs.prepare_change_batch",
        json!({"operations":[write("one.json","1")]}),
    )
    .await
    .unwrap();
    let second = call(
        &workspace,
        "fs.prepare_change_batch",
        json!({"operations":[write("two.json","2")]}),
    )
    .await
    .unwrap();
    let listed = call(&workspace, "fs.batch_status", json!({}))
        .await
        .unwrap();
    let batches = listed["batches"].as_array().unwrap();
    assert_eq!(batches.len(), 2);
    let ids: Vec<&str> = batches
        .iter()
        .map(|batch| batch["batch_id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&first["batch_id"].as_str().unwrap()));
    assert!(ids.contains(&second["batch_id"].as_str().unwrap()));
    assert!(batches.iter().all(|batch| batch["status"] == "prepared"));
    assert!(
        batches
            .iter()
            .all(|batch| batch["outcome_unknown"] == false)
    );
}

#[test]
fn catalog_schemas_are_valid_and_strict() {
    for definition in cayw_workspace::definitions() {
        cayw_ai::ArgumentValidator::new(&definition.parameters).unwrap();
        assert_eq!(definition.parameters["additionalProperties"], false);
    }
}

#[tokio::test]
async fn cross_process_worker() {
    let Ok(root) = std::env::var("CAYW_WORKSPACE_CAS_ROOT") else {
        return;
    };
    let prepared: Value =
        serde_json::from_str(&std::env::var("CAYW_WORKSPACE_CAS_PREPARED").unwrap()).unwrap();
    let marker = std::env::var("CAYW_WORKSPACE_CAS_MARKER").unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let workspace = loop {
        match Workspace::open(&root, Limits::default()) {
            Ok(workspace) => break workspace,
            Err(error)
                if error.message.starts_with("workspace is busy:")
                    && std::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(Duration::from_millis(5)).await
            }
            Err(error) => panic!("open child workspace: {error}"),
        }
    };
    let result = call(&workspace, "fs.submit_change_batch", submit(&prepared)).await;
    let outcome = match result {
        Ok(_) => "committed",
        Err(error) if !error.outcome_unknown => "conflict",
        Err(error) => panic!("unexpected uncertainty: {error}"),
    };
    fs::write(marker, outcome).unwrap();
}
fn spawn_worker(root: &Path, prepared: &Value, marker: &Path) -> std::process::Child {
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "cross_process_worker", "--nocapture"])
        .env("CAYW_WORKSPACE_CAS_ROOT", root)
        .env("CAYW_WORKSPACE_CAS_PREPARED", prepared.to_string())
        .env("CAYW_WORKSPACE_CAS_MARKER", marker)
        .spawn()
        .unwrap()
}
#[tokio::test]
async fn separate_processes_compare_and_swap_one_winner() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = workspace(&temp);
    let first = call(
        &workspace,
        "fs.prepare_change_batch",
        json!({"operations":[write("winner.json","1")]}),
    )
    .await
    .unwrap();
    let second = call(
        &workspace,
        "fs.prepare_change_batch",
        json!({"operations":[write("winner.json","2")]}),
    )
    .await
    .unwrap();
    let root = temp.path().canonicalize().unwrap();
    let marker_dir = tempfile::tempdir().unwrap();
    let mut first_child = spawn_worker(&root, &first, &marker_dir.path().join("first"));
    let mut second_child = spawn_worker(&root, &second, &marker_dir.path().join("second"));
    assert!(first_child.wait().unwrap().success());
    assert!(second_child.wait().unwrap().success());
    let mut outcomes = [
        fs::read_to_string(marker_dir.path().join("first")).unwrap(),
        fs::read_to_string(marker_dir.path().join("second")).unwrap(),
    ];
    outcomes.sort();
    assert_eq!(outcomes, ["committed", "conflict"]);
    assert!(
        [b"1".as_slice(), b"2".as_slice()]
            .contains(&fs::read(root.join("winner.json")).unwrap().as_slice())
    );
}
