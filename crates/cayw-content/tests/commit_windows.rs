#![cfg(feature = "fault-injection")]

use cayw_content::{ContentHash, ContentStore, Error, ExpectedRevision, Id, Limits, Record};
use std::{
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Child, Command, Stdio},
    sync::Arc,
};
use tempfile::TempDir;

fn id(value: &str) -> Id {
    Id::new(value).unwrap()
}

struct Worker(Child);
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn killed_at(root: &Path, point: &str, expected: ExpectedRevision) -> serde_json::Value {
    let mut worker = Worker(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "commit_window_worker", "--nocapture"])
            .env("CAYW_CONTENT_COMMIT_ROOT", root)
            .env("CAYW_CONTENT_COMMIT_POINT", point)
            .env(
                "CAYW_CONTENT_COMMIT_EXPECTED",
                serde_json::to_string(&expected).unwrap(),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let mut output = BufReader::new(worker.0.stdout.take().unwrap());
    let evidence = loop {
        let mut line = String::new();
        assert_ne!(
            output.read_line(&mut line).unwrap(),
            0,
            "worker exited before {point}"
        );
        if let Some(value) = line.trim().strip_prefix("CAYW_CONTENT_WINDOW ") {
            break serde_json::from_str::<serde_json::Value>(value).unwrap();
        }
    };
    assert_eq!(evidence["point"], point);
    worker.0.kill().unwrap();
    let status = worker.0.wait().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(status.signal(), Some(9));
    }
    evidence
}

#[test]
fn killed_before_authority_replace_preserves_previous_record() {
    let root = TempDir::new_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let db = ContentStore::open(root.path(), id("device"), Limits::default()).unwrap();
    let first = db
        .put(&id("note"), ExpectedRevision::Absent, b"original")
        .unwrap();
    let authority = std::fs::read(root.path().join("state.json")).unwrap();
    drop(db);
    let evidence = killed_at(root.path(), "content_state_staged", first.expected());
    assert_eq!(evidence["data"]["name"], "note");
    assert_eq!(evidence["data"]["version"], 2);
    assert_eq!(
        std::fs::read(root.path().join("state.json")).unwrap(),
        authority
    );
    let reopened = ContentStore::open(root.path(), id("device"), Limits::default()).unwrap();
    assert_eq!(
        reopened.try_read(&id("note")).unwrap().unwrap(),
        (first.clone(), b"original".to_vec())
    );
    let second = reopened
        .try_put(&id("note"), first.expected(), b"replacement")
        .unwrap();
    assert_eq!(second.version, 2);
}

fn killed_after_authority_replace(point: &str) {
    let root = TempDir::new_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let db = ContentStore::open(root.path(), id("device"), Limits::default()).unwrap();
    let first = db
        .put(&id("note"), ExpectedRevision::Absent, b"original")
        .unwrap();
    drop(db);
    let evidence = killed_at(root.path(), point, first.expected());
    let reopened = ContentStore::open(root.path(), id("device"), Limits::default()).unwrap();
    let (committed, bytes) = reopened.try_read(&id("note")).unwrap().unwrap();
    assert_eq!(bytes, b"replacement");
    assert_eq!(committed.version, 2);
    assert_eq!(committed.parent_revision, Some(first.revision));
    assert_eq!(evidence["data"]["revision"], committed.revision.to_string());
    assert_eq!(
        evidence["data"]["content_hash"],
        ContentHash::of(&bytes).to_string()
    );
    assert!(matches!(
        reopened.try_put(&id("note"), first.expected(), b"replacement"),
        Err(Error::Conflict { actual, .. }) if actual == committed.expected()
    ));
    assert_eq!(
        reopened.try_read(&id("note")).unwrap().unwrap().0.version,
        2
    );
}

#[test]
fn killed_after_replace_exposes_actual_record_without_replaying_unknown_call() {
    killed_after_authority_replace("content_state_renamed");
}

#[test]
fn killed_after_directory_sync_retains_exact_committed_revision() {
    killed_after_authority_replace("content_state_synced");
}

#[test]
fn commit_window_worker() {
    let Some(root) = std::env::var_os("CAYW_CONTENT_COMMIT_ROOT") else {
        return;
    };
    let point = std::env::var("CAYW_CONTENT_COMMIT_POINT").unwrap();
    let expected: ExpectedRevision =
        serde_json::from_str(&std::env::var("CAYW_CONTENT_COMMIT_EXPECTED").unwrap()).unwrap();
    cayw_ai::faults::install(Arc::new(move |observed, data| {
        if observed == point {
            assert_eq!(data.as_object().unwrap().len(), 4);
            println!(
                "CAYW_CONTENT_WINDOW {}",
                serde_json::json!({"point":observed, "data":data})
            );
            std::io::stdout().flush().unwrap();
            loop {
                std::thread::park();
            }
        }
    }))
    .unwrap();
    let db = ContentStore::open(root, id("device"), Limits::default()).unwrap();
    let _: Record = db.try_put(&id("note"), expected, b"replacement").unwrap();
    panic!("content put missed selected checkpoint");
}
