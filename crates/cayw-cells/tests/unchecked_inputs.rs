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
                assert!(
                    matches!(result, Err(CellError::Invalid(_))),
                    "{case}: {result:?}"
                );
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
                        matches!(result, Err(CellError::Invalid(_))),
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
