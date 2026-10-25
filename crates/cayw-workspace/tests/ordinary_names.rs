//! Regression tests for macOS path names and `.cayw-*` ownership (C05/C06/C07).

use cayw_workspace::{CancellationToken, CapabilityError, Limits, Workspace};
use serde_json::{Value, json};
use std::fs;

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
fn paths(value: &Value) -> Vec<&str> {
    value["paths"]
        .as_array()
        .unwrap()
        .iter()
        .map(|path| path.as_str().unwrap())
        .collect()
}

#[tokio::test]
async fn ignored_utf8_names_do_not_fail_scans_and_utf8_paths_work() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join(".gitignore"), "说明.txt\nignored/\n").unwrap();
    fs::write(temp.path().join("说明.txt"), "needle").unwrap();
    fs::write(temp.path().join("ok.txt"), "needle").unwrap();
    fs::create_dir(temp.path().join("ignored")).unwrap();
    fs::write(temp.path().join("ignored/说明2.txt"), "needle").unwrap();
    let workspace =
        Workspace::open(temp.path().canonicalize().unwrap(), Limits::default()).unwrap();

    // The ignored UTF-8 entry is skipped by the ignore rule, not by a failing
    // name validation that aborts the whole scan.
    let glob = call(
        &workspace,
        "fs.glob",
        json!({"path":".","pattern":"**/*.txt"}),
    )
    .await
    .unwrap();
    assert_eq!(paths(&glob), ["ok.txt"]);
    let grep = call(
        &workspace,
        "fs.grep",
        json!({"path":".","pattern":"needle"}),
    )
    .await
    .unwrap();
    assert_eq!(grep["matches"].as_array().unwrap().len(), 1);

    // Ordinary macOS names, including those Windows forbids, are allowed.
    for name in ["CON", "a:b", "a ", "a.", "a\\b", "名字"] {
        call(
            &workspace,
            "fs.mkdir",
            json!({"path":name,"expected_absent":true}),
        )
        .await
        .unwrap_or_else(|error| panic!("mkdir {name:?}: {error}"));
    }
    let prepared = call(
        &workspace,
        "fs.prepare_change_batch",
        json!({"operations":[write("名字/说明.json","\"moved-in\"")]}),
    )
    .await
    .unwrap();
    call(&workspace, "fs.submit_change_batch", submit(&prepared))
        .await
        .unwrap();
    assert_eq!(
        fs::read_to_string(temp.path().join("名字/说明.json")).unwrap(),
        "\"moved-in\""
    );
    let tree = call(
        &workspace,
        "fs.tree",
        json!({"path":"名字","max_entries":10}),
    )
    .await
    .unwrap();
    assert_eq!(tree["entries"][1]["path"], "名字/说明.json");
}

#[tokio::test]
async fn glob_accepts_macos_colon_names_but_still_rejects_escapes() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("a:b.txt"), "needle").unwrap();
    fs::write(temp.path().join("plain.txt"), "needle").unwrap();
    let workspace =
        Workspace::open(temp.path().canonicalize().unwrap(), Limits::default()).unwrap();
    for name in ["a:b.txt", "*.txt", "a[:]b.txt"] {
        let glob = call(&workspace, "fs.glob", json!({"path":".","pattern":name}))
            .await
            .unwrap_or_else(|error| panic!("glob {name:?}: {error}"));
        assert!(
            paths(&glob).contains(&"a:b.txt"),
            "{name:?} did not match: {glob}"
        );
    }
    for pattern in ["/etc/*", "../*", "a/../b"] {
        assert!(
            call(&workspace, "fs.glob", json!({"path":".","pattern":pattern}))
                .await
                .is_err(),
            "glob accepted {pattern:?}"
        );
    }
}

#[tokio::test]
async fn batch_rejects_unicode_aliases_as_resolved_by_the_target_volume() {
    for (first, second) in [
        ("café.json", "cafe\u{301}.json"),
        ("a.json", "A.json"),
        ("İ.json", "i\u{307}.json"),
        ("Σ.json", "ς.json"),
        ("ß.json", "SS.json"),
    ] {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join(first), "null").unwrap();
        let aliases = temp.path().join(second).exists();
        fs::remove_file(temp.path().join(first)).unwrap();
        eprintln!("volume names {first:?}/{second:?}: aliases={aliases}");
        let workspace =
            Workspace::open(temp.path().canonicalize().unwrap(), Limits::default()).unwrap();
        let prepared = call(
            &workspace,
            "fs.prepare_change_batch",
            json!({"operations":[write(first,"1"),write(second,"2")]}),
        )
        .await;
        if aliases {
            let error =
                prepared.expect_err("two spellings resolved to one file on the target volume");
            assert!(!error.outcome_unknown, "{error}");
            assert!(!temp.path().join(first).exists());
            assert_eq!(
                fs::read_dir(temp.path().join(".cayw-workspace"))
                    .unwrap()
                    .count(),
                0
            );
        } else {
            let prepared = prepared.expect("distinct names on this volume must remain usable");
            call(&workspace, "fs.submit_change_batch", submit(&prepared))
                .await
                .unwrap();
            assert_eq!(fs::read_to_string(temp.path().join(first)).unwrap(), "1");
            assert_eq!(fs::read_to_string(temp.path().join(second)).unwrap(), "2");
        }
        assert!(fs::read_dir(temp.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".cayw-host-tmp-")
        }));
    }
}

#[tokio::test]
async fn batch_parent_directory_aliases_use_the_resolved_parent() {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join("café")).unwrap();
    let aliases = temp.path().join("cafe\u{301}").exists();
    if !aliases {
        fs::create_dir(temp.path().join("cafe\u{301}")).unwrap();
    }
    let workspace =
        Workspace::open(temp.path().canonicalize().unwrap(), Limits::default()).unwrap();
    let prepared = call(
        &workspace,
        "fs.prepare_change_batch",
        json!({"operations":[write("café/a.json","1"),write("cafe\u{301}/a.json","2")]}),
    )
    .await;
    if aliases {
        let error = prepared.expect_err("different parent spellings still name one target");
        assert!(!error.outcome_unknown, "{error}");
        assert!(!temp.path().join("café/a.json").exists());
    } else {
        call(
            &workspace,
            "fs.submit_change_batch",
            submit(&prepared.unwrap()),
        )
        .await
        .unwrap();
        assert_eq!(
            fs::read_to_string(temp.path().join("café/a.json")).unwrap(),
            "1"
        );
        assert_eq!(
            fs::read_to_string(temp.path().join("cafe\u{301}/a.json")).unwrap(),
            "2"
        );
    }
    // Different leaves in one resolved directory are still independent.
    let prepared = call(
        &workspace,
        "fs.prepare_change_batch",
        json!({"operations":[write("café/one.json","1"),write("cafe\u{301}/two.json","2")]}),
    )
    .await
    .unwrap();
    call(&workspace, "fs.submit_change_batch", submit(&prepared))
        .await
        .unwrap();
}

#[tokio::test]
async fn persisted_batches_recheck_volume_aliases_before_submit_and_recovery() {
    use sha2::{Digest, Sha256};
    for recovery in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let first = "café.json";
        let second = "cafe\u{301}.json";
        fs::write(temp.path().join(first), "null").unwrap();
        let aliases = temp.path().join(second).exists();
        fs::remove_file(temp.path().join(first)).unwrap();
        let workspace =
            Workspace::open(temp.path().canonicalize().unwrap(), Limits::default()).unwrap();
        let mut prepared = call(
            &workspace,
            "fs.prepare_change_batch",
            json!({"operations":[write(first,"1"),write("other.json","2")]}),
        )
        .await
        .unwrap();
        let journal = temp
            .path()
            .join(".cayw-workspace")
            .join(prepared["batch_id"].as_str().unwrap());
        // A persisted record may have come from a volume with different name
        // rules. Bind the changed manifest into its state and caller digest.
        let manifest = fs::read_to_string(journal.join("manifest.json"))
            .unwrap()
            .replace(
                "\"path\":\"other.json\"",
                &format!("\"path\":{}", serde_json::to_string(second).unwrap()),
            );
        let digest = format!("{:x}", Sha256::digest(manifest.as_bytes()));
        fs::write(journal.join("manifest.json"), manifest).unwrap();
        let mut state: Value =
            serde_json::from_slice(&fs::read(journal.join("state.json")).unwrap()).unwrap();
        state["batch_digest"] = json!(digest);
        state["phase"] = json!(if recovery {
            "outcome_unknown"
        } else {
            "prepared"
        });
        fs::write(
            journal.join("state.json"),
            serde_json::to_vec(&state).unwrap(),
        )
        .unwrap();
        prepared["batch_digest"] = json!(digest);
        let workspace =
            Workspace::open(temp.path().canonicalize().unwrap(), Limits::default()).unwrap();
        let before = call(
            &workspace,
            "fs.batch_status",
            json!({"batch_id":prepared["batch_id"]}),
        )
        .await
        .unwrap();
        let result = if recovery {
            let mut arguments = submit(&prepared);
            arguments["action"] = json!("commit");
            call(&workspace, "fs.recover_change_batch", arguments).await
        } else {
            call(&workspace, "fs.submit_change_batch", submit(&prepared)).await
        };
        if aliases {
            let error =
                result.expect_err("persisted aliases must be rejected before user-file writes");
            assert_eq!(error.outcome_unknown, recovery, "{error}");
            assert!(error.message.contains("alias"), "{error}");
            assert!(!temp.path().join(first).exists());
            let after = call(
                &workspace,
                "fs.batch_status",
                json!({"batch_id":prepared["batch_id"]}),
            )
            .await
            .unwrap();
            assert_eq!(after["status"], before["status"]);
        } else {
            result.unwrap();
            assert_eq!(fs::read_to_string(temp.path().join(first)).unwrap(), "1");
            assert_eq!(fs::read_to_string(temp.path().join(second)).unwrap(), "2");
        }
    }
}

#[tokio::test]
async fn settled_batch_status_survives_missing_target_parents_without_probes() {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join("parent")).unwrap();
    let workspace =
        Workspace::open(temp.path().canonicalize().unwrap(), Limits::default()).unwrap();
    let prepared = call(
        &workspace,
        "fs.prepare_change_batch",
        json!({"operations":[write("parent/one.json","1"),write("parent/two.json","2")]}),
    )
    .await
    .unwrap();
    call(&workspace, "fs.submit_change_batch", submit(&prepared))
        .await
        .unwrap();
    fs::remove_dir_all(temp.path().join("parent")).unwrap();
    let workspace =
        Workspace::open(temp.path().canonicalize().unwrap(), Limits::default()).unwrap();
    let report = call(
        &workspace,
        "fs.batch_status",
        json!({"batch_id":prepared["batch_id"]}),
    )
    .await
    .unwrap();
    assert_eq!(report["status"], "committed");
    assert!(
        report["paths"]
            .as_array()
            .unwrap()
            .iter()
            .all(|path| path["matches"] == "third_party")
    );
    let mut names: Vec<_> = fs::read_dir(temp.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    names.sort();
    assert_eq!(names, [".cayw-host-lock", ".cayw-workspace"]);
}

#[tokio::test]
async fn separate_hardlink_entries_are_independent_batch_targets() {
    use sha2::{Digest, Sha256};
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("first.json"), "0").unwrap();
    fs::hard_link(
        temp.path().join("first.json"),
        temp.path().join("second.json"),
    )
    .unwrap();
    let workspace =
        Workspace::open(temp.path().canonicalize().unwrap(), Limits::default()).unwrap();
    let expected = format!("{:x}", Sha256::digest(b"0"));
    let mut first = write("first.json", "1");
    first["expected"] = json!({"kind":"sha256","sha256":expected});
    let mut second = write("second.json", "2");
    second["expected"] = first["expected"].clone();
    let prepared = call(
        &workspace,
        "fs.prepare_change_batch",
        json!({"operations":[first,second]}),
    )
    .await
    .unwrap();
    call(&workspace, "fs.submit_change_batch", submit(&prepared))
        .await
        .unwrap();
    assert_eq!(
        fs::read_to_string(temp.path().join("first.json")).unwrap(),
        "1"
    );
    assert_eq!(
        fs::read_to_string(temp.path().join("second.json")).unwrap(),
        "2"
    );
}

#[tokio::test]
async fn user_cayw_names_are_ordinary_but_actual_internal_objects_stay_protected() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join(".cayw-note.md"), "note").unwrap();
    fs::create_dir(temp.path().join(".cayw-other")).unwrap();
    fs::write(temp.path().join(".cayw-other/file.txt"), "other").unwrap();
    let workspace =
        Workspace::open(temp.path().canonicalize().unwrap(), Limits::default()).unwrap();

    let glob = call(
        &workspace,
        "fs.glob",
        json!({"path":".","pattern":"**/*","respect_gitignore":false}),
    )
    .await
    .unwrap();
    assert_eq!(paths(&glob), [".cayw-note.md", ".cayw-other/file.txt"]);
    let tree = call(&workspace, "fs.tree", json!({"path":".","max_entries":100}))
        .await
        .unwrap();
    let entries: Vec<&str> = tree["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["path"].as_str().unwrap())
        .filter(|path| !path.is_empty() && *path != ".")
        .collect();
    assert!(entries.contains(&".cayw-note.md"));
    assert!(
        !entries
            .iter()
            .any(|path| path.starts_with(".cayw-workspace"))
    );

    // A user `.cayw-*` name is writable through the normal batch path.
    let prepared = call(
        &workspace,
        "fs.prepare_change_batch",
        json!({"operations":[write(".cayw-added.md","\"added\"")]}),
    )
    .await
    .unwrap();
    call(&workspace, "fs.submit_change_batch", submit(&prepared))
        .await
        .unwrap();
    assert_eq!(
        fs::read_to_string(temp.path().join(".cayw-added.md")).unwrap(),
        "\"added\""
    );

    for path in [".cayw-workspace", ".cayw-host-lock", ".cayw-host-tmp-1-1"] {
        assert!(
            call(
                &workspace,
                "fs.mkdir",
                json!({"path":path,"expected_absent":true})
            )
            .await
            .is_err(),
            "{path}"
        );
        assert!(
            call(
                &workspace,
                "fs.prepare_change_batch",
                json!({"operations":[write(path,"null")]})
            )
            .await
            .is_err(),
            "{path}"
        );
    }
}
