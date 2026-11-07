use async_trait::async_trait;
use cayw_ai::{CancellationToken, ToolDefinition};
use cayw_cells::*;
use cayw_codemode::{CapabilityError, Limits};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
use tokio::sync::Semaphore;

struct Backend {
    fail_finished: bool,
    entered: Semaphore,
    calls: AtomicUsize,
    settled: AtomicUsize,
    hung_dropped: AtomicUsize,
}
impl Backend {
    fn new(fail_finished: bool) -> Arc<Self> {
        Arc::new(Self {
            fail_finished,
            entered: Semaphore::new(0),
            calls: AtomicUsize::new(0),
            settled: AtomicUsize::new(0),
            hung_dropped: AtomicUsize::new(0),
        })
    }
}
#[async_trait]
impl CellBackend for Backend {
    fn capabilities(&self) -> Vec<ToolDefinition> {
        ["unknown", "cleanup", "write", "oversized", "hung", "deep"]
            .into_iter()
            .map(|name| ToolDefinition {
                name: name.into(),
                description: String::new(),
                parameters: json!({"type":"object"}),
            })
            .collect()
    }
    fn created(&self, _: &CellExecution) -> Result<(), CellError> {
        Ok(())
    }
    async fn call(
        &self,
        _: &CellExecution,
        _: u64,
        name: &str,
        _: Value,
        cancel: CancellationToken,
    ) -> Result<BackendResponse, CapabilityError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let result = match name {
            "unknown" => Err(CapabilityError::unknown("receipt lost")),
            "oversized" => Ok(json!("x".repeat(CellLimits::default().item_bytes + 1))),
            "deep" => {
                let mut value = Value::Null;
                for _ in 0..10_000 {
                    value = Value::Array(vec![value]);
                }
                Ok(value)
            }
            "hung" => {
                struct Mark<'a>(&'a AtomicUsize);
                impl Drop for Mark<'_> {
                    fn drop(&mut self) {
                        self.0.fetch_add(1, Ordering::SeqCst);
                    }
                }
                let _mark = Mark(&self.hung_dropped);
                self.entered.add_permits(1);
                cancel.cancelled().await;
                std::future::pending().await
            }
            "cleanup" => {
                self.entered.add_permits(1);
                cancel.cancelled().await;
                tokio::task::yield_now().await;
                self.settled.fetch_add(1, Ordering::SeqCst);
                Ok(json!({"receipt":"actual"}))
            }
            _ => Ok(json!(true)),
        };
        Ok(BackendResponse::new(result))
    }
    fn finished(&self, _: &CellExecution, _: &CellTerminal) -> Result<(), CellError> {
        if self.fail_finished {
            Err(CellError::Journal("Core terminal reply lost".into()))
        } else {
            Ok(())
        }
    }
}
fn identity(task: &str) -> CellIdentity {
    CellIdentity {
        session_id: "session".into(),
        task_id: task.into(),
        attempt_id: "original-attempt".into(),
        permission_context: "host-policy".into(),
    }
}
fn request(source: &str) -> ExecRequest {
    ExecRequest {
        script: source.into(),
        yield_time_ms: 10_000,
        max_output_tokens: 4096,
    }
}
fn manager(
    id: CellIdentity,
    journal: Arc<SqliteJournal>,
    backend: Arc<Backend>,
    reads: Arc<ReadCoordinator>,
) -> Arc<CellManager> {
    CellManager::new(
        id,
        journal,
        backend,
        reads,
        CellLimits::default(),
        Limits {
            shutdown_grace: Duration::from_millis(100),
            ..Limits::default()
        },
    )
    .unwrap()
}
async fn terminal(cells: &CellManager, mut page: CellPage) -> CellPage {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !page.status.is_terminal() || page.vm_alive {
            page = cells
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
        page
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn host_settlement_preserves_the_original_known_stop() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(SqliteJournal::open(dir.path().join("cells.db")).unwrap());
    let backend = Backend::new(false);
    let cells = manager(
        identity("stop"),
        journal.clone(),
        backend,
        ReadCoordinator::new(vec![], CellLimits::default()).unwrap(),
    );
    let page = terminal(
        &cells,
        cells
            .exec(
                request("pcall(function() text(string.rep('x',300000)) end)"),
                "parent",
                CancellationToken::new(),
            )
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(page.status, CellStatus::Failed);
    assert_eq!(page.stop, Some(cayw_codemode::CapabilityStop::Budget));
    let record = cells.record(&page.cell_id).unwrap();
    cells.close().await;
    drop(cells);
    let scope = journal
        .stopped_scope(
            ExecutorStopped {
                identity: record.identity.clone(),
                evidence: "original VM joined".into(),
            },
            CellLimits::default(),
        )
        .unwrap();
    let mut settlement = HostSettlement {
        execution: record.execution(),
        calls: vec![],
        execution_terminal: Some(HostExecutionSettlement {
            expected_backend_error: None,
            terminal: CellTerminal {
                status: record.status,
                error: record.error.clone(),
                stop: None,
                outcome_unknown: false,
            },
            evidence: "original known terminal".into(),
        }),
        evidence: "original stop remains authoritative".into(),
    };
    assert!(matches!(
        scope.settle(settlement.clone()),
        Err(CellError::Conflict)
    ));
    settlement
        .execution_terminal
        .as_mut()
        .unwrap()
        .terminal
        .stop = record.stop;
    assert_eq!(scope.settle(settlement).unwrap().stop, record.stop);
    scope.finish().unwrap();
}

// Break caught: the capability is known in Core, but a lost lifecycle reply
// permanently freezes Cells even after the old VM and manager have stopped.
#[tokio::test]
async fn host_settlement_after_close_allows_a_new_task_without_replaying() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(SqliteJournal::open(dir.path().join("cells.db")).unwrap());
    let backend = Backend::new(true);
    let reads = ReadCoordinator::new(vec![], CellLimits::default()).unwrap();
    let cells = manager(
        identity("old"),
        journal.clone(),
        backend.clone(),
        reads.clone(),
    );
    let page = terminal(
        &cells,
        cells
            .exec(
                request("cayw.call('unknown',{key='original'})"),
                "parent",
                CancellationToken::new(),
            )
            .await
            .unwrap(),
    )
    .await;
    let record = cells.record(&page.cell_id).unwrap();
    assert!(record.backend_error.is_some());
    cells.close().await;
    drop(cells);
    let stopped = journal
        .stopped_scope(
            ExecutorStopped {
                identity: identity("old"),
                evidence: "old VM joined; Device execution stopped".into(),
            },
            CellLimits::default(),
        )
        .unwrap();
    let settlement = HostSettlement {
        execution: record.execution(),
        calls: vec![HostCallSettlement {
            sequence: 1,
            name: "unknown".into(),
            arguments: json!({"key":"original"}),
            dependencies: None,
            terminal: HostCallTerminal::Succeeded {
                result: HostResult::Value(json!({"receipt":"Core durable success"})),
            },
        }],
        execution_terminal: None,
        evidence: "Core capability terminal persisted before syncing Cells".into(),
    };
    let repaired_calls = stopped.settle(settlement.clone()).unwrap();
    assert_eq!(repaired_calls.calls[0].status, IntentStatus::Succeeded);
    assert_eq!(repaired_calls.backend_error, record.backend_error);
    assert!(matches!(stopped.finish(), Err(CellError::Frozen)));
    let mut repaired_execution = settlement;
    repaired_execution.evidence =
        "Core execution and capabilities all terminal, parent reply paired".into();
    repaired_execution.execution_terminal = Some(HostExecutionSettlement {
        expected_backend_error: record.backend_error,
        terminal: CellTerminal {
            status: CellStatus::Reconciled,
            error: None,
            stop: None,
            outcome_unknown: false,
        },
        evidence: "Core durable execution settlement after explicit host observation".into(),
    });
    for bad_kind in 0..4 {
        let mut bad = repaired_execution.clone();
        let terminal = bad.execution_terminal.as_mut().unwrap();
        match bad_kind {
            0 => terminal.expected_backend_error = Some("another failure".into()),
            1 => terminal.terminal.outcome_unknown = true,
            2 => terminal.terminal.status = CellStatus::Running,
            _ => terminal.evidence.clear(),
        }
        assert!(
            stopped.settle(bad).is_err(),
            "bad backend evidence {bad_kind}"
        );
    }
    let settled = stopped.settle(repaired_execution.clone()).unwrap();
    assert!(settled.backend_error.is_none());
    assert_eq!(settled.status, CellStatus::Reconciled);
    let retry = stopped.settle(repaired_execution.clone()).unwrap();
    assert_eq!(retry.revision, settled.revision);
    let mut changed_known_terminal = repaired_execution;
    changed_known_terminal
        .execution_terminal
        .as_mut()
        .unwrap()
        .terminal
        .status = CellStatus::Completed;
    assert!(matches!(
        stopped.settle(changed_known_terminal),
        Err(CellError::Conflict)
    ));
    stopped.finish().unwrap();
    reads.thaw(&stopped).unwrap();
    drop(stopped);
    let next = manager(identity("new"), journal, Backend::new(false), reads);
    let page = next
        .exec(
            request("return true"),
            "next-parent",
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(terminal(&next, page).await.status, CellStatus::Completed);
    assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
    next.close().await;
}

// Break caught: changing any origin/intent field, or an already-known terminal,
// silently overwrites history while attempting to settle a different execution.
#[tokio::test]
async fn settlement_rejects_changed_origins_intents_and_known_results() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(SqliteJournal::open(dir.path().join("cells.db")).unwrap());
    let cells = manager(
        identity("old"),
        journal.clone(),
        Backend::new(false),
        ReadCoordinator::new(vec![], CellLimits::default()).unwrap(),
    );
    let page = terminal(
        &cells,
        cells
            .exec(
                request("cayw.call('unknown',{a=1})"),
                "original-parent",
                CancellationToken::new(),
            )
            .await
            .unwrap(),
    )
    .await;
    let original = cells.record(&page.cell_id).unwrap();
    cells.close().await;
    drop(cells);
    let mut current = identity("old");
    current.attempt_id = "later-attempt".into();
    let scope = journal
        .stopped_scope(
            ExecutorStopped {
                identity: current,
                evidence: "old executor joined".into(),
            },
            CellLimits::default(),
        )
        .unwrap();
    assert_eq!(
        scope
            .record(&original.execution())
            .unwrap()
            .identity
            .attempt_id,
        "original-attempt"
    );
    let evidence = HostSettlement {
        execution: original.execution(),
        calls: vec![HostCallSettlement {
            sequence: 1,
            name: "unknown".into(),
            arguments: json!({"a":1}),
            dependencies: None,
            terminal: HostCallTerminal::Succeeded {
                result: HostResult::Value(json!(42)),
            },
        }],
        execution_terminal: None,
        evidence: "Core persisted original receipt".into(),
    };
    for field in 0..10 {
        let mut bad = evidence.clone();
        match field {
            0 => bad.execution.identity.session_id = "other".into(),
            1 => bad.execution.identity.task_id = "other".into(),
            2 => bad.execution.identity.attempt_id = "other".into(),
            3 => bad.execution.identity.permission_context = "other".into(),
            4 => bad.execution.cell_id = "other".into(),
            5 => bad.execution.source_hash = "sha256:wrong".into(),
            6 => bad.execution.parent_call_id = "other".into(),
            7 => bad.calls[0].sequence = 0,
            8 => bad.calls[0].name = "write".into(),
            _ => bad.calls[0].arguments = json!({"a":2}),
        }
        assert!(scope.settle(bad).is_err(), "field={field}");
        assert_eq!(
            scope.record(&original.execution()).unwrap().revision,
            original.revision
        );
    }
    let settled = scope.settle(evidence.clone()).unwrap();
    assert_eq!(settled.calls[0].result, Some(json!(42)));
    for terminal in [
        HostCallTerminal::Succeeded {
            result: HostResult::Value(json!(43)),
        },
        HostCallTerminal::Failed {
            error: "other".into(),
        },
        HostCallTerminal::NotExecuted {
            reason: "not dispatched".into(),
        },
    ] {
        let mut bad = evidence.clone();
        bad.calls[0].terminal = terminal;
        assert!(matches!(scope.settle(bad), Err(CellError::Conflict)));
    }
    assert_eq!(scope.settle(evidence).unwrap().revision, settled.revision);
    assert_eq!(
        scope
            .record(&original.execution())
            .unwrap()
            .host_settlements
            .len(),
        1
    );
    scope.finish().unwrap();
}

// Break caught: crash recovery collapses Pending-before-Core-plan, durable Core
// success, ordinary failure and omitted success into one permanently unknown state.
#[test]
fn stopped_crash_windows_accept_host_facts_and_preserve_known_terminals() {
    use sha2::{Digest, Sha256};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cells.db");
    let journal = Arc::new(SqliteJournal::open(&path).unwrap());
    let source = "cayw.call('write', {})";
    let mut record = CellRecord {
        cell_id: "crash-fixture".into(),
        identity: identity("old"),
        source: source.into(),
        source_hash: format!("sha256:{:x}", Sha256::digest(source)),
        parent_call_id: "lost-parent-reply".into(),
        status: CellStatus::Running,
        calls: (1..=5)
            .map(|sequence| CapabilityIntent {
                sequence,
                name: "write".into(),
                arguments: json!({"n":sequence}),
                dependencies: None,
                status: IntentStatus::Pending,
                result: None,
                result_omitted: false,
                result_hash: None,
                error: None,
            })
            .collect(),
        outputs: vec![],
        output_bytes: 0,
        error: None,
        stop: None,
        backend_error: None,
        reconciliation: vec![],
        host_settlements: vec![],
        revision: 1,
    };
    record.calls[0].status = IntentStatus::Succeeded;
    record.calls[0].result = Some(json!(7));
    record.calls[1].status = IntentStatus::Failed;
    record.calls[1].error = Some("known rejection".into());
    let huge_result = json!("x".repeat(CellLimits::default().item_bytes + 1));
    let huge_hash = format!(
        "sha256:{:x}",
        Sha256::digest(serde_json::to_vec(&huge_result).unwrap())
    );
    record.calls[4].status = IntentStatus::Succeeded;
    record.calls[4].result_omitted = true;
    record.calls[4].result_hash = Some(huge_hash.clone());
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute(
        "INSERT INTO cells_v1(id,session,task,revision,body) VALUES(?1,'session','old',1,?2)",
        rusqlite::params![record.cell_id, serde_json::to_string(&record).unwrap()],
    )
    .unwrap();
    let scope = journal
        .stopped_scope(
            ExecutorStopped {
                identity: identity("old"),
                evidence: "child crash observed; Device stopped".into(),
            },
            CellLimits::default(),
        )
        .unwrap();
    assert!(journal.is_frozen("session").unwrap());
    let terminals = [
        HostCallTerminal::Succeeded {
            result: HostResult::Value(json!(7)),
        },
        HostCallTerminal::Failed {
            error: "known rejection".into(),
        },
        HostCallTerminal::NotExecuted {
            reason: "no Core plan exists after execution fenced".into(),
        },
        HostCallTerminal::Succeeded {
            result: HostResult::Value(json!({"receipt":"Core committed before Cells crashed"})),
        },
        HostCallTerminal::Succeeded {
            result: HostResult::Omitted { sha256: huge_hash },
        },
    ];
    let settlement = HostSettlement {
        execution: record.execution(),
        calls: terminals
            .into_iter()
            .enumerate()
            .map(|(index, terminal)| HostCallSettlement {
                sequence: index as u64 + 1,
                name: "write".into(),
                arguments: json!({"n":index + 1}),
                dependencies: None,
                terminal,
            })
            .collect(),
        execution_terminal: Some(HostExecutionSettlement {
            expected_backend_error: None,
            terminal: CellTerminal {
                status: CellStatus::Reconciled,
                error: None,
                stop: None,
                outcome_unknown: false,
            },
            evidence: "Core stopped execution and parent pairing are durable".into(),
        }),
        evidence: "Core durable terminals read after recovery".into(),
    };
    let settled = scope.settle(settlement.clone()).unwrap();
    assert_eq!(settled.calls[2].status, IntentStatus::NotExecuted);
    assert_eq!(settled.calls[3].status, IntentStatus::Succeeded);
    for index in [0, 1, 4] {
        assert_eq!(
            serde_json::to_value(&settled.calls[index]).unwrap(),
            serde_json::to_value(&record.calls[index]).unwrap()
        );
    }
    assert!(!settled.unresolved());
    assert_eq!(scope.settle(settlement).unwrap().revision, settled.revision);
    scope.finish().unwrap();
    assert!(!journal.is_frozen("session").unwrap());
}

// Break caught: a partial Cells persistence failure thaws the session or changes
// known Core facts, leaving the caller unable to safely retry after a crash.
#[tokio::test]
async fn cells_write_and_thaw_failures_keep_freeze_and_retry_without_duplicate_audit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cells.db");
    let journal = Arc::new(SqliteJournal::open(&path).unwrap());
    let cells = manager(
        identity("old"),
        journal.clone(),
        Backend::new(false),
        ReadCoordinator::new(vec![], CellLimits::default()).unwrap(),
    );
    let page = terminal(
        &cells,
        cells
            .exec(
                request("cayw.call('unknown',{})"),
                "parent",
                CancellationToken::new(),
            )
            .await
            .unwrap(),
    )
    .await;
    let record = cells.record(&page.cell_id).unwrap();
    cells.close().await;
    drop(cells);
    let scope = journal
        .stopped_scope(
            ExecutorStopped {
                identity: identity("old"),
                evidence: "executor joined".into(),
            },
            CellLimits::default(),
        )
        .unwrap();
    let settlement = HostSettlement {
        execution: record.execution(),
        calls: vec![HostCallSettlement {
            sequence: 1,
            name: "unknown".into(),
            arguments: json!({}),
            dependencies: None,
            terminal: HostCallTerminal::Failed {
                error: "host proved known failure".into(),
            },
        }],
        execution_terminal: None,
        evidence: "Core durable failure is committed first".into(),
    };
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("CREATE TRIGGER block_update BEFORE UPDATE ON cells_v1 BEGIN SELECT RAISE(ABORT,'injected Cells write failure'); END;").unwrap();
    assert!(scope.settle(settlement.clone()).is_err());
    assert_eq!(
        scope.record(&record.execution()).unwrap().revision,
        record.revision
    );
    assert!(journal.is_frozen("session").unwrap());
    db.execute_batch("DROP TRIGGER block_update;").unwrap();
    let committed = scope.settle(settlement.clone()).unwrap();
    drop(scope); // Simulate a crash after Cells commits but before Runtime gets its reply.
    let scope = journal
        .stopped_scope(
            ExecutorStopped {
                identity: identity("old"),
                evidence: "retry after caller restart".into(),
            },
            CellLimits::default(),
        )
        .unwrap();
    assert_eq!(
        scope.settle(settlement).unwrap().revision,
        committed.revision
    );
    db.execute_batch("CREATE TRIGGER block_thaw BEFORE DELETE ON cells_frozen_v1 BEGIN SELECT RAISE(ABORT,'injected thaw failure'); END;").unwrap();
    assert!(scope.finish().is_err());
    assert!(journal.is_frozen("session").unwrap());
    db.execute_batch("DROP TRIGGER block_thaw;").unwrap();
    scope.finish().unwrap();
    assert!(!journal.is_frozen("session").unwrap());
}

// Break caught: a Core fence or a second SQLite connection steals ownership
// from a live VM or from a manager which has only loaded old records.
#[tokio::test]
async fn stopped_scope_is_exclusive_across_connections_and_session_vms() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cells.db");
    let a = Arc::new(SqliteJournal::open(&path).unwrap());
    let b = Arc::new(SqliteJournal::open(&path).unwrap());
    let cells = manager(
        identity("old"),
        a.clone(),
        Backend::new(false),
        ReadCoordinator::new(vec![], CellLimits::default()).unwrap(),
    );
    let stopped = ExecutorStopped {
        identity: identity("old"),
        evidence: "Core fence cannot steal a live Cells lease".into(),
    };
    assert!(
        b.stopped_scope(stopped.clone(), CellLimits::default())
            .is_err()
    );
    let page = cells
        .exec(
            request("yield_control()"),
            "parent",
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(page.status, CellStatus::Yielded);
    assert!(
        b.stopped_scope(stopped.clone(), CellLimits::default())
            .is_err()
    );
    let mut other = stopped.clone();
    other.identity.task_id = "other-task".into();
    let other_scope = b.stopped_scope(other, CellLimits::default()).unwrap();
    assert!(matches!(other_scope.finish(), Err(CellError::Frozen)));
    cells.close().await;
    assert!(
        b.stopped_scope(stopped.clone(), CellLimits::default())
            .is_err()
    );
    drop(cells);
    other_scope.finish().unwrap();
    drop(other_scope);
    let scope = b
        .stopped_scope(stopped.clone(), CellLimits::default())
        .unwrap();
    assert!(
        a.stopped_scope(stopped.clone(), CellLimits::default())
            .is_err()
    );
    drop(scope);
    let loaded = manager(
        identity("old"),
        a,
        Backend::new(false),
        ReadCoordinator::new(vec![], CellLimits::default()).unwrap(),
    );
    assert!(b.stopped_scope(stopped, CellLimits::default()).is_err());
    loaded.close().await;
}

// Break caught: concurrent subprocess startup inherits the flock handle until
// exec, extending a dropped manager's task lease past close/drop in the parent.
#[cfg(unix)]
#[test]
fn inherited_file_handles_do_not_extend_a_dropped_executor_lease() {
    use std::io::{Read, Write};
    use std::os::fd::FromRawFd;
    unsafe extern "C" {
        fn pipe(fds: *mut i32) -> i32;
        fn fork() -> i32;
        fn read(fd: i32, buffer: *mut u8, count: usize) -> isize;
        fn write(fd: i32, buffer: *const u8, count: usize) -> isize;
        fn _exit(code: i32) -> !;
        fn waitpid(pid: i32, status: *mut i32, options: i32) -> i32;
    }
    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(SqliteJournal::open(dir.path().join("cells.db")).unwrap());
    let cells = manager(
        identity("old"),
        journal.clone(),
        Backend::new(false),
        ReadCoordinator::new(vec![], CellLimits::default()).unwrap(),
    );
    let mut ready = [0; 2];
    let mut release = [0; 2];
    // Only raw async-signal-safe syscalls run in the fork child; it never calls
    // SQLite, Tokio, Rust destructors or an allocator before _exit.
    assert_eq!(unsafe { pipe(ready.as_mut_ptr()) }, 0);
    assert_eq!(unsafe { pipe(release.as_mut_ptr()) }, 0);
    let ready_read = unsafe { std::fs::File::from_raw_fd(ready[0]) };
    let _ready_write = unsafe { std::fs::File::from_raw_fd(ready[1]) };
    let _release_read = unsafe { std::fs::File::from_raw_fd(release[0]) };
    let release_write = unsafe { std::fs::File::from_raw_fd(release[1]) };
    let pid = unsafe { fork() };
    assert!(pid >= 0);
    if pid == 0 {
        let mut byte = 1_u8;
        unsafe {
            write(ready[1], &byte, 1);
            read(release[0], &mut byte, 1);
            _exit(0);
        }
    }
    struct Child {
        pid: i32,
        release: std::fs::File,
    }
    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.release.write_all(&[1]);
            let mut status = 0;
            unsafe {
                waitpid(self.pid, &mut status, 0);
            }
        }
    }
    let _child = Child {
        pid,
        release: release_write,
    };
    let mut byte = [0];
    (&ready_read).read_exact(&mut byte).unwrap();
    drop(cells);
    let recovered = journal.stopped_scope(
        ExecutorStopped {
            identity: identity("old"),
            evidence: "parent executor stopped while fork child has inherited fds".into(),
        },
        CellLimits::default(),
    );
    assert!(
        recovered.is_ok(),
        "inherited unrelated subprocess handle must not extend the stopped executor lease"
    );
}

// Break caught: legacy capability reconciliation removes the durable freeze
// while a different owned VM remains paused and could later dispatch effects.
#[tokio::test]
async fn capability_reconciliation_cannot_thaw_while_any_session_vm_is_alive() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(SqliteJournal::open(dir.path().join("cells.db")).unwrap());
    let cells = manager(
        identity("old"),
        journal.clone(),
        Backend::new(false),
        ReadCoordinator::new(vec![], CellLimits::default()).unwrap(),
    );
    let paused = cells
        .exec(
            request("yield_control(); cayw.call('write',{})"),
            "paused",
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(paused.status, CellStatus::Yielded);
    let unknown = terminal(
        &cells,
        cells
            .exec(
                request("cayw.call('unknown',{})"),
                "unknown",
                CancellationToken::new(),
            )
            .await
            .unwrap(),
    )
    .await;
    let record = cells.record(&unknown.cell_id).unwrap();
    cells
        .reconcile(
            &record.cell_id,
            ReconciliationEvidence {
                source_hash: record.source_hash,
                call_sequence: 1,
                expected_arguments: json!({}),
                observed_result: json!(true),
                evidence: "host observed actual success".into(),
                resolution: Resolution::Succeeded,
            },
        )
        .unwrap();
    assert!(
        journal.is_frozen("session").unwrap(),
        "another live VM must block durable thaw"
    );
    cells.close().await;
}

// Break caught: an oversized actual success has no verifiable representation
// after its payload is omitted, so backend recovery either erases it or blocks.
#[tokio::test]
async fn omitted_actual_success_keeps_digest_and_rejects_changed_receipts() {
    use sha2::{Digest, Sha256};
    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(SqliteJournal::open(dir.path().join("cells.db")).unwrap());
    let cells = manager(
        identity("old"),
        journal.clone(),
        Backend::new(false),
        ReadCoordinator::new(vec![], CellLimits::default()).unwrap(),
    );
    let page = terminal(
        &cells,
        cells
            .exec(
                request("cayw.call('oversized',{})"),
                "parent",
                CancellationToken::new(),
            )
            .await
            .unwrap(),
    )
    .await;
    let record = cells.record(&page.cell_id).unwrap();
    assert_eq!(record.calls[0].status, IntentStatus::Succeeded);
    assert!(record.calls[0].result_omitted);
    let hash = format!(
        "sha256:{:x}",
        Sha256::digest(
            serde_json::to_vec(&json!("x".repeat(CellLimits::default().item_bytes + 1))).unwrap()
        )
    );
    assert_eq!(record.calls[0].result_hash, Some(hash.clone()));
    cells.close().await;
    drop(cells);
    let scope = journal
        .stopped_scope(
            ExecutorStopped {
                identity: identity("old"),
                evidence: "VM joined".into(),
            },
            CellLimits::default(),
        )
        .unwrap();
    let mut settlement = HostSettlement {
        execution: record.execution(),
        calls: vec![HostCallSettlement {
            sequence: 1,
            name: "oversized".into(),
            arguments: json!({}),
            dependencies: None,
            terminal: HostCallTerminal::Succeeded {
                result: HostResult::Omitted { sha256: hash },
            },
        }],
        execution_terminal: None,
        evidence: "Core durable omitted success hash".into(),
    };
    let settled = scope.settle(settlement.clone()).unwrap();
    assert_eq!(
        serde_json::to_value(&settled.calls[0]).unwrap(),
        serde_json::to_value(&record.calls[0]).unwrap()
    );
    settlement.calls[0].terminal = HostCallTerminal::Succeeded {
        result: HostResult::Omitted {
            sha256: format!("sha256:{}", "0".repeat(64)),
        },
    };
    assert!(matches!(scope.settle(settlement), Err(CellError::Conflict)));
    scope.finish().unwrap();
}

// Break caught: repairing one task clears the session freeze while another
// task still owns unresolved durable calls or a failed backend lifecycle hook.
#[tokio::test]
async fn session_stays_frozen_until_every_task_record_has_host_settlement() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(SqliteJournal::open(dir.path().join("cells.db")).unwrap());
    let mut records = Vec::new();
    for task in ["first", "second"] {
        let cells = manager(
            identity(task),
            journal.clone(),
            Backend::new(true),
            ReadCoordinator::new(vec![], CellLimits::default()).unwrap(),
        );
        // Both paused tasks exist before the first failed hook freezes the session.
        let page = cells
            .exec(
                request("yield_control(); cayw.call('unknown',{})"),
                task,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        records.push((cells, page));
    }
    let mut stored = Vec::new();
    for (cells, page) in records {
        cells.close().await;
        stored.push(cells.record(&page.cell_id).unwrap());
    }
    // Every paused VM was joined. The failing terminal hooks remain uncertain.
    for (index, record) in stored.iter().enumerate() {
        let scope = journal
            .stopped_scope(
                ExecutorStopped {
                    identity: record.identity.clone(),
                    evidence: "all VMs joined".into(),
                },
                CellLimits::default(),
            )
            .unwrap();
        scope
            .settle(HostSettlement {
                execution: record.execution(),
                calls: vec![],
                execution_terminal: Some(HostExecutionSettlement {
                    expected_backend_error: record.backend_error.clone(),
                    terminal: CellTerminal {
                        status: CellStatus::Cancelled,
                        error: None,
                        stop: None,
                        outcome_unknown: false,
                    },
                    evidence: "Core execution durably cancelled with no dispatched capabilities"
                        .into(),
                }),
                evidence: "Core and Device stopped evidence".into(),
            })
            .unwrap();
        if index == 0 {
            assert!(matches!(scope.finish(), Err(CellError::Frozen)));
        } else {
            scope.finish().unwrap();
        }
    }
    assert!(!journal.is_frozen("session").unwrap());
}

// Break caught: Cells records an unchecked host success, then recursive Drop
// overflows before CodeMode can validate/drain it and retain known success.
#[test]
fn unchecked_deep_cells_success_does_not_abort_the_process() {
    const CHILD: &str = "CAYW_CELLS_DEEP_SETTLEMENT_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        runtime.block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let journal = Arc::new(SqliteJournal::open(dir.path().join("cells.db")).unwrap());
            let cells = manager(
                identity("old"),
                journal,
                Backend::new(false),
                ReadCoordinator::new(vec![], CellLimits::default()).unwrap(),
            );
            let page = terminal(
                &cells,
                cells
                    .exec(
                        request("cayw.call('deep',{})"),
                        "parent",
                        CancellationToken::new(),
                    )
                    .await
                    .unwrap(),
            )
            .await;
            assert_eq!(page.status, CellStatus::Failed);
            assert!(!page.outcome_unknown);
            let record = cells.record(&page.cell_id).unwrap();
            assert_eq!(record.calls[0].status, IntentStatus::Succeeded);
            assert!(record.calls[0].result_omitted);
            assert!(record.calls[0].result_hash.is_some());
            cells.close().await;
        });
        return;
    }
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "unchecked_deep_cells_success_does_not_abort_the_process",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .output()
        .unwrap();
    assert!(
        child.status.success(),
        "{}: {}",
        child.status,
        String::from_utf8_lossy(&child.stderr)
    );
}

// Break caught: invalid host settlement values return an error, then overflow
// the stack in recursive Value drop instead of safely rejecting the evidence.
#[test]
fn unchecked_host_settlement_values_are_rejected_without_process_abort() {
    const CHILD: &str = "CAYW_CELLS_DEEP_EVIDENCE_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        runtime.block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let journal = Arc::new(SqliteJournal::open(dir.path().join("cells.db")).unwrap());
            let cells = manager(
                identity("old"),
                journal.clone(),
                Backend::new(false),
                ReadCoordinator::new(vec![], CellLimits::default()).unwrap(),
            );
            let page = terminal(
                &cells,
                cells
                    .exec(
                        request("cayw.call('unknown',{})"),
                        "parent",
                        CancellationToken::new(),
                    )
                    .await
                    .unwrap(),
            )
            .await;
            let record = cells.record(&page.cell_id).unwrap();
            cells.close().await;
            drop(cells);

            let multi_identity = CellIdentity {
                session_id: "session-multi".into(),
                task_id: "multi".into(),
                attempt_id: "original-attempt".into(),
                permission_context: "authorized".into(),
            };
            let multi_cells = manager(
                multi_identity.clone(),
                journal.clone(),
                Backend::new(false),
                ReadCoordinator::new(vec![], CellLimits::default()).unwrap(),
            );
            let multi_page = terminal(
                &multi_cells,
                multi_cells
                    .exec(
                        request("cayw.call('write',{k='v'}); cayw.call('unknown',{})"),
                        "parent",
                        CancellationToken::new(),
                    )
                    .await
                    .unwrap(),
            )
            .await;
            let multi_record = multi_cells.record(&multi_page.cell_id).unwrap();
            multi_cells.close().await;
            drop(multi_cells);

            assert!(journal.is_frozen("session").unwrap());
            assert!(journal.is_frozen("session-multi").unwrap());

            let scope = journal
                .stopped_scope(
                    ExecutorStopped {
                        identity: identity("old"),
                        evidence: "executor stopped".into(),
                    },
                    CellLimits::default(),
                )
                .unwrap();
            for shape in ["arguments", "result", "bad-origin"] {
                let mut deep = Value::Null;
                for _ in 0..10_000 {
                    deep = Value::Array(vec![deep]);
                }
                let mut settlement = HostSettlement {
                    execution: record.execution(),
                    calls: vec![HostCallSettlement {
                        sequence: 1,
                        name: "unknown".into(),
                        arguments: json!({}),
                        dependencies: None,
                        terminal: HostCallTerminal::Succeeded {
                            result: HostResult::Value(json!(true)),
                        },
                    }],
                    execution_terminal: None,
                    evidence: "Core record".into(),
                };
                if shape == "arguments" {
                    settlement.calls[0].arguments = deep;
                } else {
                    settlement.calls[0].terminal = HostCallTerminal::Succeeded {
                        result: HostResult::Value(deep),
                    };
                }
                if shape == "bad-origin" {
                    settlement.execution.parent_call_id = "bad".into();
                }
                assert!(scope.settle(settlement).is_err());
                assert_eq!(
                    scope.record(&record.execution()).unwrap().revision,
                    record.revision
                );
            }
            let multi_scope = journal
                .stopped_scope(
                    ExecutorStopped {
                        identity: multi_identity,
                        evidence: "executor stopped multi".into(),
                    },
                    CellLimits::default(),
                )
                .unwrap();
            for shape in [
                "multi-arguments",
                "multi-result",
                "multi-first-arguments",
                "multi-first-result",
            ] {
                let mut deep = Value::Null;
                for _ in 0..10_000 {
                    deep = Value::Array(vec![deep]);
                }
                let mut settlement = HostSettlement {
                    execution: multi_record.execution(),
                    calls: vec![
                        HostCallSettlement {
                            sequence: 1,
                            name: "write".into(),
                            arguments: json!({"k": "v"}),
                            dependencies: None,
                            terminal: HostCallTerminal::Succeeded {
                                result: HostResult::Value(json!(true)),
                            },
                        },
                        HostCallSettlement {
                            sequence: 2,
                            name: "unknown".into(),
                            arguments: json!({}),
                            dependencies: None,
                            terminal: HostCallTerminal::Succeeded {
                                result: HostResult::Value(json!(true)),
                            },
                        },
                    ],
                    execution_terminal: None,
                    evidence: "Core record multi".into(),
                };
                match shape {
                    "multi-arguments" => {
                        settlement.calls[1].arguments = deep;
                    }
                    "multi-result" => {
                        settlement.calls[1].terminal = HostCallTerminal::Succeeded {
                            result: HostResult::Value(deep),
                        };
                    }
                    "multi-first-arguments" => {
                        settlement.calls[0].arguments = deep;
                    }
                    "multi-first-result" => {
                        settlement.calls[0].terminal = HostCallTerminal::Succeeded {
                            result: HostResult::Value(deep),
                        };
                    }
                    _ => unreachable!(),
                }
                assert!(multi_scope.settle(settlement).is_err());
                assert_eq!(
                    multi_scope
                        .record(&multi_record.execution())
                        .unwrap()
                        .revision,
                    multi_record.revision
                );
            }
        });
        return;
    }
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "unchecked_host_settlement_values_are_rejected_without_process_abort",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .output()
        .unwrap();
    assert!(
        child.status.success(),
        "{}: {}",
        child.status,
        String::from_utf8_lossy(&child.stderr)
    );
}

// Break caught: success was already durable with an omitted payload before its
// digest was available; host repair must preserve it and audit the first digest.
#[test]
fn existing_omitted_success_can_add_evidence_without_replacing_known_success() {
    use sha2::{Digest, Sha256};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cells.db");
    let journal = Arc::new(SqliteJournal::open(&path).unwrap());
    let source = "return true";
    let record = CellRecord {
        cell_id: "omitted-before-digest".into(),
        identity: identity("old"),
        source: source.into(),
        source_hash: format!("sha256:{:x}", Sha256::digest(source)),
        parent_call_id: "parent".into(),
        status: CellStatus::OutcomeUnknown,
        calls: vec![CapabilityIntent {
            sequence: 1,
            name: "oversized".into(),
            arguments: json!({}),
            dependencies: None,
            status: IntentStatus::Succeeded,
            result: None,
            result_omitted: true,
            result_hash: None,
            error: None,
        }],
        outputs: vec![],
        output_bytes: 0,
        error: None,
        stop: None,
        backend_error: Some("old lifecycle reply lost".into()),
        reconciliation: vec![],
        host_settlements: vec![],
        revision: 1,
    };
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute(
        "INSERT INTO cells_v1(id,session,task,revision,body) VALUES(?1,'session','old',1,?2)",
        rusqlite::params![record.cell_id, serde_json::to_string(&record).unwrap()],
    )
    .unwrap();
    let scope = journal
        .stopped_scope(
            ExecutorStopped {
                identity: identity("old"),
                evidence: "VM and Device stopped".into(),
            },
            CellLimits::default(),
        )
        .unwrap();
    let hash = HostResult::value_hash(&json!("actual oversized value"));
    let mut evidence = HostSettlement {
        execution: record.execution(),
        calls: vec![HostCallSettlement {
            sequence: 1,
            name: "oversized".into(),
            arguments: json!({}),
            dependencies: None,
            terminal: HostCallTerminal::Succeeded {
                result: HostResult::Omitted {
                    sha256: hash.clone(),
                },
            },
        }],
        execution_terminal: Some(HostExecutionSettlement {
            expected_backend_error: record.backend_error.clone(),
            terminal: CellTerminal {
                status: CellStatus::Reconciled,
                error: None,
                stop: None,
                outcome_unknown: false,
            },
            evidence: "Core known success and execution durably verified".into(),
        }),
        evidence: "actual Core result digest read from durable result".into(),
    };
    let settled = scope.settle(evidence.clone()).unwrap();
    assert_eq!(settled.calls[0].status, IntentStatus::Succeeded);
    assert!(settled.calls[0].result.is_none());
    assert!(settled.calls[0].result_omitted);
    assert_eq!(settled.calls[0].result_hash, Some(hash));
    evidence.calls[0].terminal = HostCallTerminal::Succeeded {
        result: HostResult::Omitted {
            sha256: HostResult::value_hash(&json!("different")),
        },
    };
    assert!(matches!(scope.settle(evidence), Err(CellError::Conflict)));
    scope.finish().unwrap();
}

// Break caught: digesting results uses a different representation from the
// durable host result, so valid object/array/scalar receipts cannot reconcile.
#[test]
fn result_digest_matches_json_for_nested_values() {
    use sha2::{Digest, Sha256};
    for value in [
        json!(null),
        json!(true),
        json!({"z":[1,"\\\"雪",{"empty":[]}],"a":{}}),
    ] {
        let expected = format!(
            "sha256:{:x}",
            Sha256::digest(serde_json::to_vec(&value).unwrap())
        );
        assert_eq!(HostResult::value_hash(&value), expected);
    }
}

// Break caught: drop neither settles cooperatively nor expires shutdown grace,
// leaving a permanently suspended host future and its task lease alive.
#[tokio::test]
async fn dropped_invocation_bounds_uncooperative_cleanup_without_close() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(SqliteJournal::open(dir.path().join("cells.db")).unwrap());
    let backend = Backend::new(false);
    let cells = manager(
        identity("old"),
        journal.clone(),
        backend.clone(),
        ReadCoordinator::new(vec![], CellLimits::default()).unwrap(),
    );
    let mut execution = Box::pin(cells.exec(
        request("cayw.call('hung',{}); cayw.call('write',{})"),
        "parent",
        CancellationToken::new(),
    ));
    tokio::select! { result = &mut execution => panic!("early {result:?}"), permit = backend.entered.acquire() => permit.unwrap().forget() }
    drop(execution);
    tokio::time::timeout(Duration::from_secs(2), async {
        while backend.hung_dropped.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(journal.is_frozen("session").unwrap());
    assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
    cells.close().await;
}

// Break caught: dropping exec/wait aborts the owned VM before its original
// capability future can observe cancellation and return its actual receipt.
#[tokio::test]
async fn dropped_exec_and_wait_poll_the_same_capability_through_cleanup() {
    for drop_wait in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(SqliteJournal::open(dir.path().join("cells.db")).unwrap());
        let backend = Backend::new(false);
        let cells = manager(
            identity("old"),
            journal.clone(),
            backend.clone(),
            ReadCoordinator::new(vec![], CellLimits::default()).unwrap(),
        );
        let cell_id = if drop_wait {
            let mut req = request("cayw.call('cleanup',{}); cayw.call('write',{})");
            req.yield_time_ms = 0;
            let page = cells
                .exec(req, "parent", CancellationToken::new())
                .await
                .unwrap();
            backend.entered.acquire().await.unwrap().forget();
            let mut waiting = Box::pin(cells.wait(
                WaitRequest {
                    cell_id: page.cell_id.clone(),
                    cursor: 0,
                    yield_time_ms: 10_000,
                    max_output_tokens: 4096,
                    continue_execution: false,
                },
                CancellationToken::new(),
            ));
            assert!(futures::poll!(&mut waiting).is_pending());
            drop(waiting);
            page.cell_id
        } else {
            let mut execution = Box::pin(cells.exec(
                request("cayw.call('cleanup',{}); cayw.call('write',{})"),
                "parent",
                CancellationToken::new(),
            ));
            tokio::select! {
                result = &mut execution => panic!("returned before capability dispatch: {result:?}"),
                permit = backend.entered.acquire() => permit.unwrap().forget(),
            }
            drop(execution);
            journal
                .records(&identity("old"), &CellLimits::default())
                .unwrap()[0]
                .cell_id
                .clone()
        };
        tokio::time::timeout(Duration::from_secs(2), async {
            while cells.record(&cell_id).unwrap().status == CellStatus::Running {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("dropping the invocation itself must cancel and settle the owned VM");
        let record = cells.record(&cell_id).unwrap();
        assert_eq!(
            record.calls[0].status,
            IntentStatus::Succeeded,
            "drop_wait={drop_wait}"
        );
        assert_eq!(record.calls[0].result, Some(json!({"receipt":"actual"})));
        assert_eq!(backend.settled.load(Ordering::SeqCst), 1);
        assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
        assert!(!journal.is_frozen("session").unwrap());
        cells.close().await;
    }
}
