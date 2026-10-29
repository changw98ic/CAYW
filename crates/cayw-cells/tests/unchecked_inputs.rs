//! Public input rejection must return an error without aborting the host process.
use async_trait::async_trait;
use cayw_ai::{CancellationToken, ToolDefinition};
use cayw_cells::*;
use cayw_codemode::{CapabilityError, Limits};
use serde_json::{Value, json};
use std::{
    process::{Child, Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};

const CASE: &str = "CAYW_CELLS_UNCHECKED_INPUT_CASE";
const DEEP: usize = 10_000;

fn nested(depth: usize, objects: bool) -> Value {
    let mut value = Value::Null;
    for _ in 0..depth {
        value = if objects {
            Value::Object(serde_json::Map::from_iter([("child".into(), value)]))
        } else {
            Value::Array(vec![value])
        };
    }
    value
}

// Each case is a new process: SIGABRT must fail the parent test, and must not
// prevent the remaining cases or shallow controls from running.
fn isolated_cases(test: &str, cases: &[&str]) {
    struct ChildGuard(Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut failures = Vec::new();
    for case in cases {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("child.log");
        let log = std::fs::File::create(&log_path).unwrap();
        let mut child = ChildGuard(
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", test, "--nocapture"])
                .env(CASE, case)
                .stdout(Stdio::from(log.try_clone().unwrap()))
                .stderr(Stdio::from(log))
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(30);
        let outcome = loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                break status.to_string();
            }
            if Instant::now() >= deadline {
                child.0.kill().unwrap();
                child.0.wait().unwrap();
                break "timeout".into();
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let output = std::fs::read_to_string(log_path).unwrap();
        let success = child.0.wait().unwrap().success() && outcome != "timeout";
        eprintln!("{case}: {outcome}");
        if !success {
            failures.push(format!("{case}: {outcome}\n{output}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

// Break caught: key/limit rejection or JSON validation recursively drops the
// owned, unchecked Value. The worker deliberately uses the reproduced 2 MiB
// stack; making the test stack larger would hide the bug.
#[test]
fn store_rejects_unchecked_values_without_process_abort() {
    let Ok(case) = std::env::var(CASE) else {
        isolated_cases(
            "store_rejects_unchecked_values_without_process_abort",
            &[
                "shallow",
                "shallow-empty-key",
                "deep-array",
                "deep-object",
                "deep-empty-key",
                "deep-long-key",
                "deep-control-key",
                "deep-invalid-limits",
                "deep-byte-limit",
                "deep-unvisited-sibling",
            ],
        );
        return;
    };
    std::thread::Builder::new()
        .name("public-store-2mib".into())
        .stack_size(2 * 1024 * 1024)
        .spawn(move || {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("cells.db");
            let journal = SqliteJournal::open(&path).unwrap();
            let mut limits = CellLimits::default();
            let baseline = journal
                .store("s", "value", 0, json!("original"), &limits)
                .unwrap();
            let key = match case.as_str() {
                "shallow-empty-key" | "deep-empty-key" => String::new(),
                "deep-long-key" => "k".repeat(129),
                "deep-control-key" => "\n".into(),
                _ => "value".into(),
            };
            if case == "deep-invalid-limits" {
                limits.json_depth = usize::MAX;
            }
            if case == "deep-byte-limit" {
                limits.store_bytes = 1;
            }
            let shallow = case.starts_with("shallow");
            let mut value = nested(if shallow { 10 } else { DEEP }, case == "deep-object");
            if case == "deep-unvisited-sibling" {
                value = Value::Array(vec![json!("x".repeat(limits.store_bytes + 1)), value]);
            }
            let result = journal.store("s", &key, 1, value, &limits);
            if case == "shallow" {
                let stored = result.unwrap();
                assert_eq!(stored.version, 2);
                assert_eq!(stored.value, nested(10, false));
                assert_eq!(journal.load("s", "value").unwrap(), Some(stored));
            } else {
                if matches!(
                    case.as_str(),
                    "deep-array" | "deep-object" | "deep-byte-limit" | "deep-unvisited-sibling"
                ) {
                    assert!(
                        matches!(result, Err(CellError::Capacity(_))),
                        "{case}: {result:?}"
                    );
                } else {
                    assert!(
                        matches!(result, Err(CellError::Invalid(_))),
                        "{case}: {result:?}"
                    );
                }
                assert_eq!(journal.load("s", "value").unwrap(), Some(baseline));
                assert!(!journal.is_frozen("s").unwrap());
            }
            drop(journal);
            let reopened = SqliteJournal::open(path).unwrap();
            assert_eq!(
                reopened.load("s", "value").unwrap().unwrap().version,
                if case == "shallow" { 2 } else { 1 }
            );
        })
        .unwrap()
        .join()
        .unwrap();
}

struct Unknown;
#[async_trait]
impl CellBackend for Unknown {
    fn capabilities(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "unknown".into(),
            description: String::new(),
            parameters: json!({"type":"object"}),
        }]
    }
    fn created(&self, _: &CellExecution) -> Result<(), CellError> {
        Ok(())
    }
    async fn call(
        &self,
        _: &CellExecution,
        _: u64,
        _: &str,
        _: Value,
        _: CancellationToken,
    ) -> Result<BackendResponse, CapabilityError> {
        Err(CapabilityError::unknown("receipt unavailable"))
    }
    fn finished(&self, _: &CellExecution, _: &CellTerminal) -> Result<(), CellError> {
        Ok(())
    }
}

// Break caught: reconciliation serializes/compares/clones unchecked evidence
// before checking both Values, or drains only the first rejected Value.
#[test]
fn reconcile_rejects_unchecked_values_without_process_abort() {
    let Ok(case) = std::env::var(CASE) else {
        isolated_cases(
            "reconcile_rejects_unchecked_values_without_process_abort",
            &[
                "shallow",
                "shallow-failed",
                "observed-array",
                "observed-object",
                "arguments-array",
                "both-deep",
                "oversized-arguments-deep-result",
                "empty-reference",
                "missing-cell",
                "bad-source",
                "failed-deep-result",
            ],
        );
        return;
    };
    std::thread::Builder::new()
        .name("public-reconcile-2mib".into())
        .stack_size(2 * 1024 * 1024)
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .unwrap();
            runtime.block_on(async {
                let dir = tempfile::tempdir().unwrap();
                let journal = Arc::new(SqliteJournal::open(dir.path().join("cells.db")).unwrap());
                let limits = CellLimits::default();
                let identity = CellIdentity {
                    session_id: "s".into(),
                    task_id: "t".into(),
                    attempt_id: "a".into(),
                    permission_context: "host-policy".into(),
                };
                let cells = CellManager::new(
                    identity.clone(),
                    journal.clone(),
                    Arc::new(Unknown),
                    ReadCoordinator::new(vec![], limits.clone()).unwrap(),
                    limits.clone(),
                    Limits::default(),
                )
                .unwrap();
                let page = cells
                    .exec(
                        ExecRequest {
                            script: "cayw.call('unknown',{})".into(),
                            yield_time_ms: 1000,
                            max_output_tokens: 4096,
                        },
                        "original-parent",
                        CancellationToken::new(),
                    )
                    .await
                    .unwrap();
                cells.close().await;
                let original = cells.record(&page.cell_id).unwrap();
                assert_eq!(original.status, CellStatus::OutcomeUnknown);
                assert_eq!(original.calls[0].status, IntentStatus::OutcomeUnknown);
                assert!(journal.is_frozen("s").unwrap());
                let shallow = case.starts_with("shallow");
                let mut evidence = ReconciliationEvidence {
                    source_hash: original.source_hash.clone(),
                    call_sequence: 1,
                    expected_arguments: if case == "arguments-array" || case == "both-deep" {
                        nested(DEEP, false)
                    } else {
                        json!({})
                    },
                    observed_result: nested(
                        if shallow {
                            10
                        } else if case == "arguments-array" {
                            0
                        } else {
                            DEEP
                        },
                        case == "observed-object",
                    ),
                    evidence: "durable host observation".into(),
                    resolution: Resolution::Succeeded,
                };
                if case == "oversized-arguments-deep-result" {
                    evidence.expected_arguments = json!("x".repeat(limits.item_bytes + 1));
                }
                if case == "empty-reference" {
                    evidence.evidence.clear();
                }
                if case == "bad-source" {
                    evidence.source_hash = "sha256:wrong".into();
                }
                if case == "failed-deep-result" || case == "shallow-failed" {
                    evidence.resolution = Resolution::Failed;
                }
                let id = if case == "missing-cell" {
                    "missing"
                } else {
                    &page.cell_id
                };
                let retry = shallow.then(|| evidence.clone());
                let result = cells.reconcile(id, evidence);
                if shallow {
                    let record = result.unwrap();
                    assert_eq!(record.status, CellStatus::Reconciled);
                    assert_eq!(record.execution(), original.execution());
                    assert_eq!(
                        record.calls[0].status,
                        if case == "shallow-failed" {
                            IntentStatus::Failed
                        } else {
                            IntentStatus::Succeeded
                        }
                    );
                    if case == "shallow" {
                        assert_eq!(record.calls[0].result, Some(nested(10, false)));
                    }
                    assert_eq!(record.reconciliation.len(), 1);
                    assert!(!journal.is_frozen("s").unwrap());
                    let retried = cells.reconcile(id, retry.unwrap()).unwrap();
                    assert_eq!(retried.revision, record.revision);
                    assert_eq!(retried.reconciliation.len(), 1);
                } else {
                    assert!(
                        matches!(result, Err(CellError::Capacity(_))),
                        "{case}: {result:?}"
                    );
                    assert_eq!(
                        serde_json::to_value(cells.record(&page.cell_id).unwrap()).unwrap(),
                        serde_json::to_value(&original).unwrap()
                    );
                    assert!(journal.is_frozen("s").unwrap());
                    let persisted = journal.records(&identity, &limits).unwrap();
                    assert_eq!(
                        serde_json::to_value(&persisted[0]).unwrap(),
                        serde_json::to_value(original).unwrap()
                    );
                }
            });
        })
        .unwrap()
        .join()
        .unwrap();
}

struct EmptyBackend;
#[async_trait]
impl CellBackend for EmptyBackend {
    fn capabilities(&self) -> Vec<ToolDefinition> {
        Vec::new()
    }
    fn created(&self, _: &CellExecution) -> Result<(), CellError> {
        Ok(())
    }
    async fn call(
        &self,
        _: &CellExecution,
        _: u64,
        _: &str,
        _: Value,
        _: CancellationToken,
    ) -> Result<BackendResponse, CapabilityError> {
        Err(CapabilityError::new("no fixture host capability"))
    }
    fn finished(&self, _: &CellExecution, _: &CellTerminal) -> Result<(), CellError> {
        Ok(())
    }
}

struct AllowAllValues;
#[async_trait]
impl CellValueAuthorizer for AllowAllValues {
    async fn authorize(
        &self,
        _: &CellExecution,
        _: CellValueOperation,
        _: &Value,
        _: CancellationToken,
    ) -> Result<(), CapabilityError> {
        Ok(())
    }
}

#[tokio::test]
async fn cross_exec_store_load_conflict_and_reopened_readback() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("cells.db");
    let journal = Arc::new(SqliteJournal::open(&db_path).unwrap());
    let limits = CellLimits::default();
    let identity = CellIdentity {
        session_id: "test-session".into(),
        task_id: "test-task".into(),
        attempt_id: "attempt-1".into(),
        permission_context: "authorized".into(),
    };
    let backend = authorized_cell_backend(
        Arc::new(EmptyBackend),
        CellValueExecutor::new(journal.clone(), limits.clone()),
        Arc::new(AllowAllValues),
    );
    let manager = CellManager::new(
        identity.clone(),
        journal.clone(),
        backend.clone(),
        ReadCoordinator::new(vec![], limits.clone()).unwrap(),
        limits.clone(),
        Limits::default(),
    )
    .unwrap();
    // 1. Cross-exec: initial store creation with expected_version = 0.
    let create_page = manager
        .exec(
            ExecRequest {
                script: "return store('search_result', { query = 'rust', count = 1 }, 0)".into(),
                yield_time_ms: 1000,
                max_output_tokens: 4096,
            },
            "call-1",
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(create_page.status, CellStatus::Completed);
    assert!(!create_page.vm_alive);
    assert!(!create_page.has_more);
    assert_eq!(
        create_page.outputs[0].output,
        CellOutput::Json {
            value: json!({"version": 1})
        }
    );

    // 2. Cross-exec: conflict when updating with wrong expected_version = 0.
    let conflict_page = manager
        .exec(
            ExecRequest {
                script: "return store('search_result', { query = 'rust', count = 2 }, 0)".into(),
                yield_time_ms: 1000,
                max_output_tokens: 4096,
            },
            "call-2",
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(conflict_page.status, CellStatus::Failed);
    assert!(
        conflict_page
            .error
            .as_deref()
            .unwrap_or("")
            .contains("session value version does not match")
    );

    // 3. Cross-exec: load prior version and update successfully with expected_version.
    let update_page = manager
        .exec(
            ExecRequest {
                script: r#"
                    local prev = load('search_result')
                    assert(prev ~= nil, 'key should exist')
                    assert(prev.version == 1, 'expected version 1')
                    assert(prev.value.count == 1, 'expected count 1')
                    return store('search_result', { query = 'rust', count = 2 }, prev.version)
                "#
                .into(),
                yield_time_ms: 1000,
                max_output_tokens: 4096,
            },
            "call-3",
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(update_page.status, CellStatus::Completed);
    assert_eq!(
        update_page.outputs[0].output,
        CellOutput::Json {
            value: json!({"version": 2})
        }
    );

    manager.close().await;
    drop(manager);
    drop(backend);
    drop(journal);

    // 4. Reopen real SQLite file and verify direct journal readback.
    let reopened_journal = Arc::new(SqliteJournal::open(&db_path).unwrap());
    let loaded = reopened_journal
        .load("test-session", "search_result")
        .unwrap()
        .expect("persisted value in sqlite");
    assert_eq!(loaded.version, 2);
    assert_eq!(loaded.value, json!({"query": "rust", "count": 2}));

    // 5. Reopen real SQLite file under a fresh CellManager/CellTool and verify exec readback.
    let reopened_backend = authorized_cell_backend(
        Arc::new(EmptyBackend),
        CellValueExecutor::new(reopened_journal.clone(), limits.clone()),
        Arc::new(AllowAllValues),
    );
    let reopened_manager = CellManager::new(
        identity,
        reopened_journal,
        reopened_backend,
        ReadCoordinator::new(vec![], limits.clone()).unwrap(),
        limits.clone(),
        Limits::default(),
    )
    .unwrap();
    let readback_page = reopened_manager
        .exec(
            ExecRequest {
                script: "return load('search_result')".into(),
                yield_time_ms: 1000,
                max_output_tokens: 4096,
            },
            "call-4",
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(readback_page.status, CellStatus::Completed);
    assert_eq!(
        readback_page.outputs[0].output,
        CellOutput::Json {
            value: json!({
                "version": 2,
                "value": { "query": "rust", "count": 2 }
            })
        }
    );
    reopened_manager.close().await;
}
