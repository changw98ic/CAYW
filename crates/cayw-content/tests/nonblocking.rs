use cayw_content::{ContentHash, ContentStore, Error, ExpectedRevision, Id, Limits};
use std::{
    io::{BufRead, BufReader, Read, Write},
    process::{Child, Command, Stdio},
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

#[test]
fn contended_lock_refuses_without_changes_then_allows_exact_cas() {
    let root = TempDir::new_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let store = ContentStore::open(root.path(), id("device"), Limits::default()).unwrap();
    let first = store
        .put(&id("note"), ExpectedRevision::Absent, b"original")
        .unwrap();
    let authority = std::fs::read(root.path().join("state.json")).unwrap();
    let mut worker = Worker(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "exclusive_lock_worker", "--nocapture"])
            .env("CAYW_CONTENT_LOCK_ROOT", root.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let mut output = BufReader::new(worker.0.stdout.take().unwrap());
    loop {
        let mut line = String::new();
        assert_ne!(
            output.read_line(&mut line).unwrap(),
            0,
            "lock worker exited"
        );
        if line.trim() == "CAYW_CONTENT_LOCK_HELD" {
            break;
        }
    }
    assert!(matches!(store.try_read(&id("note")), Err(Error::Busy)));
    assert!(matches!(
        store.try_put(&id("note"), first.expected(), b"updated"),
        Err(Error::Busy)
    ));
    assert_eq!(
        std::fs::read(root.path().join("state.json")).unwrap(),
        authority
    );
    assert!(!root.path().join(".object-stage").exists());
    assert!(!root.path().join(".state-stage").exists());
    worker.0.stdin.take().unwrap().write_all(b"x").unwrap();
    assert!(worker.0.wait().unwrap().success());
    assert_eq!(
        store.try_read(&id("note")).unwrap().unwrap(),
        (first.clone(), b"original".to_vec())
    );
    let second = store
        .try_put(&id("note"), first.expected(), b"updated")
        .unwrap();
    assert_eq!(second.version, 2);
    assert_eq!(second.parent_revision, Some(first.revision));
    assert!(matches!(
        store.try_put(&id("note"), first.expected(), b"stale"),
        Err(Error::Conflict { actual, .. }) if actual == second.expected()
    ));
    drop(store);
    let reopened = ContentStore::open(root.path(), id("device"), Limits::default()).unwrap();
    assert_eq!(
        reopened.try_read(&id("note")).unwrap().unwrap(),
        (second, b"updated".to_vec())
    );
}

#[test]
fn exclusive_lock_worker() {
    let Some(root) = std::env::var_os("CAYW_CONTENT_LOCK_ROOT") else {
        return;
    };
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(std::path::PathBuf::from(root).join(".lock"))
        .unwrap();
    fs2::FileExt::lock_exclusive(&file).unwrap();
    println!("CAYW_CONTENT_LOCK_HELD");
    std::io::stdout().flush().unwrap();
    std::io::stdin().read_exact(&mut [0]).unwrap();
    drop(file);
}

#[test]
fn hexadecimal_hash_parser_accepts_only_canonical_sha256() {
    let hash = ContentHash::of(b"content");
    assert_eq!(ContentHash::from_hex(&hash.to_string()).unwrap(), hash);
    for invalid in [
        "".to_owned(),
        "0".repeat(63),
        "0".repeat(65),
        "g".repeat(64),
        "A".repeat(64),
        "é".repeat(32),
        format!(" {}", "0".repeat(64)),
    ] {
        assert!(matches!(
            ContentHash::from_hex(&invalid),
            Err(Error::InvalidHash)
        ));
    }
}

#[test]
fn nonblocking_reads_preserve_integrity_and_writes_preserve_quotas() {
    let root = TempDir::new_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let limits = Limits {
        max_content_bytes: 8,
        max_total_bytes: 8,
        max_records: 1,
        ..Limits::default()
    };
    let store = ContentStore::open(root.path(), id("device"), limits).unwrap();
    assert_eq!(store.try_read(&id("missing")).unwrap(), None);
    let first = store
        .try_put(&id("note"), ExpectedRevision::Absent, b"original")
        .unwrap();
    assert!(matches!(
        store.try_put(&id("other"), ExpectedRevision::Absent, b"x"),
        Err(Error::QuotaExceeded)
    ));
    assert!(matches!(
        store.try_put(&id("note"), first.expected(), b"a longer value"),
        Err(Error::ContentTooLarge)
    ));
    assert!(matches!(
        store.try_put(&id("note"), first.expected(), b"next"),
        Err(Error::QuotaExceeded)
    ));
    std::fs::write(
        root.path()
            .join("objects")
            .join(first.content_hash.to_string()),
        b"corrupt!",
    )
    .unwrap();
    assert!(matches!(store.try_read(&id("note")), Err(Error::Integrity)));
}
