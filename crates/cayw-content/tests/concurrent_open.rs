use cayw_content::{ContentStore, Error, ExpectedRevision, Id, Limits};
use std::{
    io::{BufRead, BufReader, Read, Write},
    path::Path,
    process::{Child, Command, Stdio},
};
use tempfile::TempDir;

fn id(value: &str) -> Id {
    Id::new(value).unwrap()
}

fn start_worker(root: &Path) -> Child {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "first_open_worker", "--nocapture"])
        .env("CAYW_CONTENT_FIRST_OPEN_ROOT", root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut output = BufReader::new(child.stdout.as_mut().unwrap());
    loop {
        let mut line = String::new();
        assert_ne!(
            output.read_line(&mut line).unwrap(),
            0,
            "worker exited before ready"
        );
        if line.trim() == "CAYW_CONTENT_READY" {
            break;
        }
    }
    child
}

fn concurrent_first_open(precreate_root: bool) {
    let outer = TempDir::new_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    for round in 0..5 {
        let root = outer.path().join(format!("round-{round}/nested/store"));
        if precreate_root {
            std::fs::create_dir_all(&root).unwrap();
        }
        // Every worker is waiting on its private stdin before any of them can
        // open the store. No root or lock is pre-created by the absent case.
        let mut children: Vec<_> = (0..16).map(|_| start_worker(&root)).collect();
        for child in &mut children {
            child.stdin.take().unwrap().write_all(b"x").unwrap();
        }
        let outputs: Vec<_> = children
            .into_iter()
            .map(|c| c.wait_with_output().unwrap())
            .collect();
        for output in &outputs {
            assert!(
                matches!(output.status.code(), Some(0 | 3)),
                "round {round}, precreated={precreate_root}: {:?}\n{}\n{}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        assert_eq!(outputs.iter().filter(|o| o.status.success()).count(), 1);
        let db = ContentStore::open(&root, id("device"), Limits::default()).unwrap();
        let (record, bytes) = db.read(&id("counter")).unwrap().unwrap();
        assert_eq!(record.version, 1);
        assert_eq!(bytes, b"first concurrent value");
        let second = db.put(&id("counter"), record.expected(), b"next").unwrap();
        assert_eq!(second.version, 2);
    }
}

#[test]
fn concurrent_first_open_creates_all_root_components_and_preserves_cas() {
    concurrent_first_open(false);
}

#[test]
fn concurrent_first_open_creates_lock_in_existing_root_and_preserves_cas() {
    concurrent_first_open(true);
}

#[test]
fn first_open_worker() {
    let Some(root) = std::env::var_os("CAYW_CONTENT_FIRST_OPEN_ROOT") else {
        return;
    };
    println!("CAYW_CONTENT_READY");
    std::io::stdout().flush().unwrap();
    std::io::stdin().read_exact(&mut [0]).unwrap();
    let db = ContentStore::open(root, id("device"), Limits::default()).unwrap();
    match db.put(
        &id("counter"),
        ExpectedRevision::Absent,
        b"first concurrent value",
    ) {
        Ok(_) => std::process::exit(0),
        Err(Error::Conflict { .. }) => std::process::exit(3),
        Err(error) => panic!("unexpected initial CAS outcome: {error}"),
    }
}
