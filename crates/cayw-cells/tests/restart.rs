mod support;
use async_trait::async_trait;
use cayw_ai::{CancellationToken, ToolDefinition};
use cayw_cells::*;
use cayw_codemode::{CapabilityError, CapabilityHost, Limits};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader};
use std::{
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use support::{TestCellExec, backend};
use tokio::sync::Semaphore;

struct CrashHost {
    entered: Semaphore,
    calls: AtomicUsize,
}
#[async_trait]
impl CapabilityHost for CrashHost {
    fn capabilities(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "write".into(),
            description: "crash fixture effect".into(),
            parameters: json!({"type":"object","additionalProperties":false}),
        }]
    }
    async fn call(
        &self,
        _: &str,
        _: Value,
        _: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.entered.add_permits(1);
        std::future::pending().await
    }
}
fn identity() -> CellIdentity {
    CellIdentity {
        session_id: "crash-session".into(),
        task_id: "crash-task".into(),
        attempt_id: "before-crash".into(),
        permission_context: "crash-gate".into(),
    }
}

/// Runs only when explicitly selected by the parent test. exit skips every Rust
/// destructor: the next process sees a real committed Pending intent, not a
/// fixture that merely claims to represent a crash.
#[test]
#[ignore = "subprocess fixture selected by abrupt_process_exit_recovers_unknown_without_replay"]
fn crash_child() {
    let path =
        std::env::var_os("CAYW_CELLS_TEST_CRASH_DB").expect("parent supplies temporary database");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    runtime.block_on(async {
        let journal = Arc::new(SqliteJournal::open(&path).unwrap());
        let host = Arc::new(CrashHost {
            entered: Semaphore::new(0),
            calls: AtomicUsize::new(0),
        });
        let limits = CellLimits::default();
        let manager = CellManager::new(
            identity(),
            journal,
            backend(host.clone()),
            ReadCoordinator::new(vec![], limits.clone()).unwrap(),
            limits,
            Limits {
                timeout: Duration::from_secs(60),
                ..Limits::default()
            },
        )
        .unwrap();
        let page = manager
            .exec_fixture(
                ExecRequest {
                    script: "cayw.call('write', {})".into(),
                    yield_time_ms: 1,
                    max_output_tokens: 4096,
                },
                CancellationToken::new(),
            )
            .await
            .unwrap();
        host.entered.acquire().await.unwrap().forget();
        assert_eq!(
            manager.record(&page.cell_id).unwrap().calls[0].status,
            IntentStatus::Pending
        );
        // Save only the generated id for the parent; all actual state is SQLite.
        std::fs::write(
            std::path::Path::new(&path).with_extension("id"),
            page.cell_id,
        )
        .unwrap();
        std::process::exit(73);
    });
}
#[test]
#[ignore = "subprocess fixture selected by live_process_scope_lock_is_not_stolen"]
fn live_scope_child() {
    let path = std::env::var_os("CAYW_CELLS_LIVE_SCOPE_DB").unwrap();
    let limits = CellLimits::default();
    let host = Arc::new(CrashHost {
        entered: Semaphore::new(0),
        calls: AtomicUsize::new(0),
    });
    let manager = CellManager::new(
        identity(),
        Arc::new(SqliteJournal::open(path).unwrap()),
        backend(host),
        ReadCoordinator::new(vec![], limits.clone()).unwrap(),
        limits,
        Limits::default(),
    )
    .unwrap();
    println!("CAYW_SCOPE_READY");
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).unwrap();
    drop(manager);
}

#[test]
fn live_process_scope_lock_is_not_stolen_and_unrelated_task_runs() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("live.db");
    let mut child = ChildGuard(
        Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "live_scope_child", "--nocapture"])
            .env("CAYW_CELLS_LIVE_SCOPE_DB", &path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut output = BufReader::new(child.0.stdout.take().unwrap());
    let mut line = String::new();
    loop {
        line.clear();
        assert!(output.read_line(&mut line).unwrap() > 0);
        if line.contains("CAYW_SCOPE_READY") {
            break;
        }
    }
    let journal = Arc::new(SqliteJournal::open(&path).unwrap());
    let limits = CellLimits::default();
    let host = Arc::new(CrashHost {
        entered: Semaphore::new(0),
        calls: AtomicUsize::new(0),
    });
    assert!(
        CellManager::new(
            identity(),
            journal.clone(),
            backend(host.clone()),
            ReadCoordinator::new(vec![], limits.clone()).unwrap(),
            limits.clone(),
            Limits::default()
        )
        .is_err()
    );
    assert!(
        journal
            .recover_stopped(
                &ExecutorStopped {
                    identity: identity(),
                    evidence: "must not steal live process".into()
                },
                &limits
            )
            .is_err()
    );
    let mut other = identity();
    other.task_id = "unrelated".into();
    let manager = CellManager::new(
        other,
        journal,
        backend(host),
        ReadCoordinator::new(vec![], limits.clone()).unwrap(),
        limits,
        Limits::default(),
    )
    .unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    runtime.block_on(async {
        let mut page = manager
            .exec(
                ExecRequest {
                    script: "return true".into(),
                    yield_time_ms: 1000,
                    max_output_tokens: 4096,
                },
                "parent",
                CancellationToken::new(),
            )
            .await
            .unwrap();
        while !page.status.is_terminal() {
            page = manager
                .wait(
                    WaitRequest {
                        cell_id: page.cell_id,
                        cursor: page.next_cursor,
                        yield_time_ms: 1000,
                        max_output_tokens: 4096,
                        continue_execution: false,
                    },
                    CancellationToken::new(),
                )
                .await
                .unwrap();
        }
        assert_eq!(page.status, CellStatus::Completed);
        manager.close().await;
    });
    drop(child.0.stdin.take());
    assert!(child.0.wait().unwrap().success());
}

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
#[test]
fn abrupt_process_exit_recovers_unknown_without_replay() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("crash.db");
    let mut child = ChildGuard(
        Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "crash_child", "--nocapture"])
            .env("CAYW_CELLS_TEST_CRASH_DB", &path)
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            assert_eq!(status.code(), Some(73));
            break;
        }
        assert!(Instant::now() < deadline, "crash child timed out");
        std::thread::sleep(Duration::from_millis(5));
    }
    let cell_id = std::fs::read_to_string(path.with_extension("id")).unwrap();
    let journal = Arc::new(SqliteJournal::open(&path).unwrap());
    assert!(!journal.is_frozen("crash-session").unwrap());
    assert_eq!(
        journal
            .records(&identity(), &CellLimits::default())
            .unwrap()[0]
            .calls[0]
            .status,
        IntentStatus::Pending
    );
    journal
        .recover_stopped(
            &ExecutorStopped {
                identity: identity(),
                evidence: "parent observed child exit status 73".into(),
            },
            &CellLimits::default(),
        )
        .unwrap();
    assert!(journal.is_frozen("crash-session").unwrap());
    let host = Arc::new(CrashHost {
        entered: Semaphore::new(0),
        calls: AtomicUsize::new(0),
    });
    let mut identity = identity();
    identity.attempt_id = "after-crash".into();
    let limits = CellLimits::default();
    let manager = CellManager::new(
        identity,
        journal,
        backend(host.clone()),
        ReadCoordinator::new(vec![], limits.clone()).unwrap(),
        limits,
        Limits::default(),
    )
    .unwrap();
    let record = manager.record(&cell_id).unwrap();
    assert_eq!(record.status, CellStatus::OutcomeUnknown);
    assert_eq!(record.calls[0].status, IntentStatus::OutcomeUnknown);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    runtime.block_on(async {
        let page = manager
            .wait(
                WaitRequest {
                    cell_id,
                    cursor: 0,
                    yield_time_ms: 0,
                    max_output_tokens: 4096,
                    continue_execution: true,
                },
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(page.outcome_unknown);
        assert!(!page.vm_alive);
        assert_eq!(host.calls.load(Ordering::SeqCst), 0);
        manager.close().await;
    });
}
