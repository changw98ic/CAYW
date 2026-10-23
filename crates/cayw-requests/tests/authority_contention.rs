use cayw_requests::{Clock, RequestConfig, RequestError, RequestStore, Result, TaskIdentity};
use serde_json::Value;
use std::{
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
    sync::{
        Arc, Barrier,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

#[derive(Default)]
struct CountingClock(AtomicUsize);
impl Clock for CountingClock {
    fn now_ms(&self) -> Result<u64> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(1000)
    }
}

fn authority_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".authority");
    name.into()
}

fn competing_opens(path: &Path, config: &RequestConfig, clock: &Arc<CountingClock>) {
    std::thread::scope(|scope| {
        let start = Arc::new(Barrier::new(4));
        let handles = (0..4)
            .map(|_| {
                let start = start.clone();
                let clock = clock.clone();
                let config = config.clone();
                scope.spawn(move || {
                    start.wait();
                    RequestStore::open_with_clock(path, config, clock)
                })
            })
            .collect::<Vec<_>>();
        let results = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>();
        for result in &results {
            assert!(
                result.is_ok(),
                "competing open failed: {:?}",
                result.as_ref().err()
            );
        }
        assert_eq!(results.len(), 4);
    });
    assert_eq!(
        clock.0.load(Ordering::SeqCst),
        0,
        "opening observed task time"
    );
    let record: Value =
        serde_json::from_slice(&std::fs::read(authority_path(path)).unwrap()).unwrap();
    assert_eq!(record["phase"], "ready");
    let connection = rusqlite::Connection::open(path).unwrap();
    let (id, stopped, events): (String, i64, i64) = connection
        .query_row(
            "SELECT authority_id,authority_stopped,(SELECT count(*) FROM request_events) FROM request_meta",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(record["id"], id);
    assert_eq!((stopped, events), (0, 0));
}

#[test]
fn concurrent_first_open_serializes_initialization_without_stopping_authority() {
    // Break caught: try_lock-only admission makes another legitimate initializer fail.
    for _ in 0..8 {
        let temp = tempfile::tempdir().unwrap();
        let config = RequestConfig {
            sqlite_busy_timeout_ms: 1000,
            ..RequestConfig::default()
        };
        competing_opens(
            &temp.path().join("requests.sqlite"),
            &config,
            &Arc::new(CountingClock::default()),
        );
    }
}

#[test]
fn concurrent_reopen_keeps_the_existing_authority_identity() {
    // Break caught: an observer cannot open during another connection's initialization.
    for _ in 0..8 {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("requests.sqlite");
        let config = RequestConfig {
            sqlite_busy_timeout_ms: 1000,
            ..RequestConfig::default()
        };
        let clock = Arc::new(CountingClock::default());
        let original = RequestStore::open_with_clock(&path, config.clone(), clock.clone()).unwrap();
        let before: Value =
            serde_json::from_slice(&std::fs::read(authority_path(&path)).unwrap()).unwrap();
        competing_opens(&path, &config, &clock);
        let after: Value =
            serde_json::from_slice(&std::fs::read(authority_path(&path)).unwrap()).unwrap();
        assert_eq!(before, after);
        drop(original);
    }
}

fn locked_authority(path: &Path) -> File {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(authority_path(path))
        .unwrap();
    file.lock().unwrap();
    file
}

fn assert_busy(error: RequestError) {
    match error {
        RequestError::Database(rusqlite::Error::SqliteFailure(error, _)) => {
            assert_eq!(error.code, rusqlite::ErrorCode::DatabaseBusy);
        }
        error => panic!("expected contention without authority stop, got {error:?}"),
    }
}

#[test]
fn occupied_authority_times_out_without_reading_time_or_changing_persistence() {
    // Break caught: waiting without a bound or rewriting a busy sidecar steals its owner.
    for wait_ms in [0, 50] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("requests.sqlite");
        let config = RequestConfig {
            sqlite_busy_timeout_ms: wait_ms,
            ..RequestConfig::default()
        };
        let clock = Arc::new(CountingClock::default());
        let store = RequestStore::open_with_clock(&path, config.clone(), clock.clone()).unwrap();
        let before = std::fs::read(authority_path(&path)).unwrap();
        let held = locked_authority(&path);
        let (done, completed) = mpsc::channel();
        let contender_path = path.clone();
        let contender_config = config.clone();
        let contender_clock = clock.clone();
        let contender = std::thread::spawn(move || {
            let result =
                RequestStore::open_with_clock(contender_path, contender_config, contender_clock);
            done.send(result.err()).unwrap();
        });
        // A watchdog detects an unbounded File::lock; no millisecond duration is asserted.
        let result = completed.recv_timeout(Duration::from_secs(10));
        drop(held);
        contender.join().unwrap();
        assert_busy(
            result
                .expect("open waited indefinitely for a held authority")
                .expect("open ignored its owner"),
        );
        assert_eq!(std::fs::read(authority_path(&path)).unwrap(), before);
        assert_eq!(clock.0.load(Ordering::SeqCst), 0);
        RequestStore::open_with_clock(&path, config, clock).unwrap();
        drop(store);
    }
}

struct BlockingClock {
    observed: mpsc::SyncSender<()>,
    release: std::sync::Mutex<mpsc::Receiver<()>>,
}
impl Clock for BlockingClock {
    fn now_ms(&self) -> Result<u64> {
        self.observed.send(()).unwrap();
        self.release
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(10))
            .expect("test did not release the original transaction");
        Ok(1000)
    }
}

#[test]
fn occupied_transaction_never_replays_or_stops_the_original_operation() {
    // Break caught: contention recovery reads Pending without its lock or clears the owner.
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("requests.sqlite");
    let config = RequestConfig::default();
    let (observed, observation) = mpsc::sync_channel(1);
    let (release, released) = mpsc::sync_channel(1);
    let clock = Arc::new(BlockingClock {
        observed,
        release: std::sync::Mutex::new(released),
    });
    let store = Arc::new(RequestStore::open_with_clock(&path, config.clone(), clock).unwrap());
    let identity = TaskIdentity::new("session", "task", "attempt").unwrap();
    let owner = std::thread::spawn(move || store.task_state(&identity));
    observation.recv_timeout(Duration::from_secs(10)).unwrap();
    let pending = std::fs::read(authority_path(&path)).unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&pending).unwrap()["phase"],
        "pending"
    );
    let contender = RequestStore::open(&path, config.clone());
    let after_contention = std::fs::read(authority_path(&path)).unwrap();
    release.send(()).unwrap();
    let owner_result = owner.join().unwrap();
    assert_busy(contender.err().expect("open ignored a live transaction"));
    assert_eq!(after_contention, pending);
    assert!(matches!(owner_result, Err(RequestError::NotFound)));
    RequestStore::open(&path, config).unwrap();
    let connection = rusqlite::Connection::open(path).unwrap();
    let (highwater, stopped, events): (i64, i64, i64) = connection
        .query_row(
            "SELECT last_now_ms,authority_stopped,(SELECT count(*) FROM request_events) FROM request_meta",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!((highwater, stopped, events), (1000, 0, 0));
}

struct ChildClock;
impl Clock for ChildClock {
    fn now_ms(&self) -> Result<u64> {
        panic!("store initialization must not observe task time")
    }
}

#[test]
fn authority_open_process_child() {
    let Some(path) = std::env::var_os("CAYW_CONTENTION_AUTHORITY_DB") else {
        return;
    };
    let path = PathBuf::from(path);
    let ordinal = std::env::var("CAYW_CONTENTION_CHILD").unwrap();
    let parent = path.parent().unwrap();
    std::fs::write(parent.join(format!("ready-{ordinal}")), b"ready").unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !parent.join("start").exists() {
        assert!(Instant::now() < deadline, "parent did not start contenders");
        std::thread::yield_now();
    }
    let store = RequestStore::open_with_clock(
        path,
        RequestConfig {
            sqlite_busy_timeout_ms: 1000,
            ..RequestConfig::default()
        },
        Arc::new(ChildClock),
    );
    assert!(
        store.is_ok(),
        "concurrent process open failed: {:?}",
        store.err()
    );
}

#[test]
fn independent_processes_can_open_and_reopen_one_authority() {
    // Break caught: process-shared initialization contention escapes as a store failure.
    for preexisting in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("requests.sqlite");
        let config = RequestConfig {
            sqlite_busy_timeout_ms: 1000,
            ..RequestConfig::default()
        };
        let original = preexisting.then(|| RequestStore::open(&path, config).unwrap());
        let previous = preexisting.then(|| std::fs::read(authority_path(&path)).unwrap());
        let executable = std::env::current_exe().unwrap();
        let mut children = (0..2)
            .map(|ordinal| {
                std::process::Command::new(&executable)
                    .args(["--exact", "authority_open_process_child", "--nocapture"])
                    .env("CAYW_CONTENTION_AUTHORITY_DB", &path)
                    .env("CAYW_CONTENTION_CHILD", ordinal.to_string())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
                    .unwrap()
            })
            .collect::<Vec<_>>();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !(0..2).all(|ordinal| temp.path().join(format!("ready-{ordinal}")).exists()) {
            if Instant::now() >= deadline {
                for child in &mut children {
                    let _ = child.kill();
                    let _ = child.wait();
                }
                panic!("child processes did not become ready");
            }
            std::thread::yield_now();
        }
        std::fs::write(temp.path().join("start"), b"start").unwrap();
        let outcomes = children
            .into_iter()
            .map(|child| child.wait_with_output().unwrap())
            .collect::<Vec<_>>();
        for outcome in outcomes {
            assert!(
                outcome.status.success(),
                "child failed: {}{}",
                String::from_utf8_lossy(&outcome.stdout),
                String::from_utf8_lossy(&outcome.stderr),
            );
        }
        let record = std::fs::read(authority_path(&path)).unwrap();
        if let Some(previous) = previous {
            assert_eq!(record, previous);
        }
        let record: Value = serde_json::from_slice(&record).unwrap();
        assert_eq!(record["phase"], "ready");
        let connection = rusqlite::Connection::open(path).unwrap();
        let (id, stopped, events): (String, i64, i64) = connection
            .query_row(
                "SELECT authority_id,authority_stopped,(SELECT count(*) FROM request_events) FROM request_meta",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(record["id"], id);
        assert_eq!((stopped, events), (0, 0));
        drop(original);
    }
}

#[test]
fn released_pending_or_stopped_records_cannot_be_reopened_as_ready() {
    // Break caught: lock availability is mistaken for permission to reset an interrupted record.
    for phase in ["pending", "stopped"] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("requests.sqlite");
        let config = RequestConfig::default();
        let clock = Arc::new(CountingClock::default());
        let store = RequestStore::open_with_clock(&path, config.clone(), clock.clone()).unwrap();
        let mut record: Value =
            serde_json::from_slice(&std::fs::read(authority_path(&path)).unwrap()).unwrap();
        record["phase"] = phase.into();
        std::fs::write(authority_path(&path), serde_json::to_vec(&record).unwrap()).unwrap();
        let error = RequestStore::open_with_clock(&path, config, clock.clone())
            .err()
            .expect("interrupted authority was reset");
        assert!(matches!(error, RequestError::AuthorityStopped));
        assert_eq!(clock.0.load(Ordering::SeqCst), 0);
        let stopped: Value =
            serde_json::from_slice(&std::fs::read(authority_path(&path)).unwrap()).unwrap();
        assert_eq!(stopped["phase"], "stopped");
        assert_eq!(stopped["id"], record["id"]);
        drop(store);
    }
}
