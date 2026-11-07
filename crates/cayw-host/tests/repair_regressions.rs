use async_trait::async_trait;
use cayw_ai::CancellationToken;
use cayw_codemode::{CapabilityError, CapabilityHost, CapabilityStop};
use cayw_host::{AuditSink, ExternalHost, Host, HostConfig, PermissionGate, Profile, ProfileKind};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

static NEXT: AtomicU64 = AtomicU64::new(1);
struct Root(PathBuf);
impl Root {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "cayw-host-repair-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Default)]
struct Gate {
    error: Option<CapabilityError>,
    calls: AtomicUsize,
}
#[async_trait]
impl PermissionGate for Gate {
    async fn authorize(
        &self,
        _: &str,
        _: &Value,
        _: CancellationToken,
    ) -> Result<(), CapabilityError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match &self.error {
            Some(error) => Err(CapabilityError {
                message: error.message.clone(),
                outcome_unknown: error.outcome_unknown,
                stop: error.stop,
            }),
            None => Ok(()),
        }
    }
}
#[derive(Default)]
struct Audit {
    records: Mutex<Vec<Value>>,
    fail_start: bool,
    fail_settle: bool,
}
fn diagnostic() -> String {
    "\"\\\n\u{1}你好".repeat(1024)
}
#[async_trait]
impl AuditSink for Audit {
    async fn started(&self, name: &str, args: &Value) -> Result<String, CapabilityError> {
        if self.fail_start {
            return Err(CapabilityError::new(diagnostic()));
        }
        let mut records = self.records.lock().unwrap();
        let id = records.len().to_string();
        records.push(json!({"phase":"running", "id":id, "name":name, "arguments":args}));
        Ok(id)
    }
    async fn settled(
        &self,
        id: &str,
        result: &Result<Value, CapabilityError>,
    ) -> Result<(), CapabilityError> {
        self.records.lock().unwrap().push(match result {
            Ok(value) => json!({"phase":"succeeded", "id":id, "value":value}),
            Err(error) => json!({"phase":if error.outcome_unknown {"unknown"} else {"failed"}, "id":id, "message":error.message}),
        });
        if self.fail_settle {
            Err(CapabilityError::new(diagnostic()))
        } else {
            Ok(())
        }
    }
    fn dropped(&self, id: &str) -> Result<(), CapabilityError> {
        self.records
            .lock()
            .unwrap()
            .push(json!({"phase":"unknown", "id":id}));
        Ok(())
    }
}
fn host(root: &Path, config: HostConfig, gate: Arc<Gate>, audit: Arc<Audit>) -> Host {
    Host::new(root, gate, audit, config).unwrap()
}
async fn call(host: &Host, name: &str, args: Value) -> Result<Value, CapabilityError> {
    host.call(name, args, CancellationToken::new()).await
}
fn operations() -> Value {
    json!({"operations":[
        {"op":"write","path":"one.json","expected":{"kind":"absent"},"content":"1","encoding":"utf8","language":"json"},
        {"op":"write","path":"two.json","expected":{"kind":"absent"},"content":"2","encoding":"utf8","language":"json"}
    ]})
}
async fn pending(root: &Path) -> Value {
    let workspace =
        cayw_workspace::Workspace::open(root.canonicalize().unwrap(), Default::default()).unwrap();
    let prepared = workspace
        .execute(
            "fs.prepare_change_batch",
            &operations(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    workspace.inject_fault(
        cayw_workspace::FaultPoint::AfterInstall(0),
        cayw_workspace::FaultAction::Cancel,
    );
    assert!(workspace.execute("fs.submit_change_batch", &json!({"batch_id":prepared["batch_id"],"expected_batch_digest":prepared["batch_digest"]}), CancellationToken::new()).await.unwrap_err().outcome_unknown);
    prepared
}
fn recovery(prepared: &Value) -> Value {
    json!({"batch_id":prepared["batch_id"],"expected_batch_digest":prepared["batch_digest"],"action":"rollback"})
}

#[cfg(unix)]
#[tokio::test]
async fn pending_refusal_does_not_poison_new_mutations_or_explicit_recovery() {
    for name in [
        "fs.mkdir",
        "fs.move",
        "fs.remove",
        "fs.prepare_change_batch",
        "fs.submit_change_batch",
    ] {
        let root = Root::new();
        let prepared = pending(root.path()).await;
        let audit = Arc::new(Audit::default());
        let host = host(
            root.path(),
            Default::default(),
            Arc::new(Gate::default()),
            audit.clone(),
        );
        let read = call(&host, "fs.read", json!({"path":"one.json"}))
            .await
            .unwrap();
        let args = match name {
            "fs.mkdir" => json!({"path":"never","expected_absent":true}),
            "fs.move" => {
                json!({"source":"one.json","destination":"never","expected_sha256":read["sha256"],"expected_destination_absent":true})
            }
            "fs.remove" => json!({"path":"one.json","expected_sha256":read["sha256"]}),
            "fs.prepare_change_batch" => operations(),
            _ => {
                json!({"batch_id":prepared["batch_id"],"expected_batch_digest":prepared["batch_digest"]})
            }
        };
        let error = call(&host, name, args).await.unwrap_err();
        assert!(!error.outcome_unknown, "{name}: {error}");
        assert!(!host.outcome_unknown());
        assert!(!root.path().join("never").exists());
        assert_eq!(
            call(
                &host,
                "fs.batch_status",
                json!({"batch_id":prepared["batch_id"]})
            )
            .await
            .unwrap()["outcome_unknown"],
            true
        );
        assert_eq!(
            call(&host, "fs.read", json!({"path":"one.json"}))
                .await
                .unwrap()["content"],
            "1"
        );
        call(&host, "fs.recover_change_batch", recovery(&prepared))
            .await
            .unwrap();
        assert!(!root.path().join("one.json").exists());
        call(
            &host,
            "fs.mkdir",
            json!({"path":"after","expected_absent":true}),
        )
        .await
        .unwrap();
        assert!(root.path().join("after").is_dir());
        assert!(
            audit
                .records
                .lock()
                .unwrap()
                .iter()
                .any(|r| r["phase"] == "failed")
        );
    }
}

// Opening consumes part of the allowance; recovery's first validation waits
// until the original allowance has definitely expired. No elapsed-time tolerance
// or upper bound is asserted, only the next checkpoint and unchanged user files.
struct ExpiringValidator {
    calls: AtomicUsize,
    started: Mutex<Option<Instant>>,
    allowance: Duration,
}
impl cayw_workspace::SyntaxValidator for ExpiringValidator {
    fn validate(&self, language: &str, candidate: &[u8]) -> Result<(), CapabilityError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let until = if call == 0 {
            let start = Instant::now();
            *self.started.lock().unwrap() = Some(start);
            Some(start + self.allowance / 2)
        } else if call == 2 {
            Some(self.started.lock().unwrap().unwrap() + self.allowance)
        } else {
            None
        };
        if let Some(until) = until {
            while Instant::now() < until {
                std::thread::park_timeout(until.saturating_duration_since(Instant::now()));
            }
        }
        cayw_workspace::JsonTomlValidator.validate(language, candidate)
    }
}
#[cfg(unix)]
#[tokio::test]
async fn opening_journals_cannot_restart_the_host_deadline() {
    let root = Root::new();
    let prepared = pending(root.path()).await;
    let validator = Arc::new(ExpiringValidator {
        calls: AtomicUsize::new(0),
        started: Mutex::new(None),
        allowance: Duration::from_secs(1),
    });
    let audit = Arc::new(Audit::default());
    let host = host(
        root.path(),
        HostConfig {
            call_timeout: Duration::from_secs(1),
            workspace: Some(cayw_workspace::Limits {
                syntax_validator: validator.clone(),
                ..Default::default()
            }),
            ..Default::default()
        },
        Arc::new(Gate::default()),
        audit.clone(),
    );
    let error = call(&host, "fs.recover_change_batch", recovery(&prepared))
        .await
        .unwrap_err();
    assert!(error.message.contains("deadline"), "{error}");
    assert!(validator.calls.load(Ordering::SeqCst) <= 3);
    assert_eq!(
        std::fs::read_to_string(root.path().join("one.json")).unwrap(),
        "1"
    );
    assert!(!root.path().join("two.json").exists());
    assert!(audit.records.lock().unwrap().last().unwrap()["phase"] != "succeeded");
}
struct CancellingValidator {
    calls: AtomicUsize,
    cancel: CancellationToken,
}
impl cayw_workspace::SyntaxValidator for CancellingValidator {
    fn validate(&self, language: &str, candidate: &[u8]) -> Result<(), CapabilityError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.cancel.cancel();
        cayw_workspace::JsonTomlValidator.validate(language, candidate)
    }
}
#[cfg(unix)]
#[tokio::test]
async fn journal_opening_observes_the_callers_cancellation_at_the_next_checkpoint() {
    let root = Root::new();
    let prepared = pending(root.path()).await;
    let cancel = CancellationToken::new();
    let validator = Arc::new(CancellingValidator {
        calls: AtomicUsize::new(0),
        cancel: cancel.clone(),
    });
    let host = host(
        root.path(),
        HostConfig {
            workspace: Some(cayw_workspace::Limits {
                syntax_validator: validator.clone(),
                ..Default::default()
            }),
            ..Default::default()
        },
        Arc::new(Gate::default()),
        Arc::new(Audit::default()),
    );
    assert!(
        host.call("fs.recover_change_batch", recovery(&prepared), cancel)
            .await
            .is_err()
    );
    assert_eq!(validator.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        std::fs::read_to_string(root.path().join("one.json")).unwrap(),
        "1"
    );
}

#[tokio::test]
async fn replacement_diagnostic_consumes_remaining_bytes_and_blocks_another_admission() {
    let root = Root::new();
    let gate = Arc::new(Gate::default());
    let audit = Arc::new(Audit::default());
    let host = host(
        root.path(),
        HostConfig {
            max_result_bytes: 64,
            max_total_bytes: 64,
            ..Default::default()
        },
        gate.clone(),
        audit.clone(),
    );
    let args = json!({"format":"json","text":"x"});
    let error = call(&host, "data.parse", args.clone()).await.unwrap_err();
    let emitted = serde_json::to_vec(&error.message).unwrap().len();
    assert!(
        serde_json::to_vec(&args).unwrap().len() + emitted <= 64,
        "returned message: {:?}, encoded bytes: {emitted}",
        error.message
    );
    assert_eq!(
        audit.records.lock().unwrap().last().unwrap()["message"],
        error.message
    );
    assert!(call(&host, "data.parse", args).await.is_err());
    assert_eq!(gate.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn scopes_keep_independent_permissions_audits_and_one_cumulative_budget() {
    let root = Root::new();
    let parent_gate = Arc::new(Gate::default());
    let parent_audit = Arc::new(Audit::default());
    let parent = host(
        root.path(),
        HostConfig {
            max_calls: 3,
            ..Default::default()
        },
        parent_gate.clone(),
        parent_audit.clone(),
    );
    let deny_gate = Arc::new(Gate {
        error: Some(CapabilityError::new("scope denied")),
        ..Default::default()
    });
    let deny_audit = Arc::new(Audit::default());
    let denied = parent
        .scoped(deny_gate.clone(), deny_audit.clone())
        .unwrap();
    let allow_gate = Arc::new(Gate::default());
    let allow_audit = Arc::new(Audit::default());
    let allowed = parent
        .scoped(allow_gate.clone(), allow_audit.clone())
        .unwrap();
    assert!(
        !call(
            &denied,
            "fs.mkdir",
            json!({"path":"never","expected_absent":true})
        )
        .await
        .unwrap_err()
        .outcome_unknown
    );
    assert!(!root.path().join("never").exists());
    call(
        &allowed,
        "fs.mkdir",
        json!({"path":"allowed","expected_absent":true}),
    )
    .await
    .unwrap();
    call(&parent, "data.parse", json!({"format":"json","text":"1"}))
        .await
        .unwrap();
    assert!(
        call(
            &allowed,
            "fs.mkdir",
            json!({"path":"over-budget","expected_absent":true})
        )
        .await
        .is_err()
    );
    assert!(!root.path().join("over-budget").exists());
    assert_eq!(deny_gate.calls.load(Ordering::SeqCst), 1);
    assert_eq!(allow_gate.calls.load(Ordering::SeqCst), 1);
    assert_eq!(parent_gate.calls.load(Ordering::SeqCst), 1);
    assert!(deny_audit.records.lock().unwrap().is_empty());
    assert_eq!(allow_audit.records.lock().unwrap().len(), 2);
    assert_eq!(parent_audit.records.lock().unwrap().len(), 2);
}

#[cfg(unix)]
#[tokio::test]
async fn scope_construction_keeps_the_pinned_directory_and_shared_unknown_audit_failure() {
    let container = Root::new();
    let root = container.path().join("root");
    let moved = container.path().join("moved");
    std::fs::create_dir(&root).unwrap();
    let parent = host(
        &root,
        Default::default(),
        Arc::new(Gate::default()),
        Arc::new(Audit::default()),
    );
    std::fs::rename(&root, &moved).unwrap();
    std::fs::create_dir(&root).unwrap();
    let audit = Arc::new(Audit {
        fail_settle: true,
        ..Default::default()
    });
    let scoped = parent
        .scoped(Arc::new(Gate::default()), audit.clone())
        .unwrap();
    assert!(
        call(
            &scoped,
            "fs.mkdir",
            json!({"path":"effect","expected_absent":true})
        )
        .await
        .unwrap_err()
        .outcome_unknown
    );
    assert!(moved.join("effect").is_dir());
    assert!(!root.join("effect").exists());
    assert!(parent.outcome_unknown());
    assert_eq!(parent.audit_failure(), scoped.audit_failure());
    assert!(parent.audit_failure().is_some());
    let later = parent
        .scoped(Arc::new(Gate::default()), Arc::new(Audit::default()))
        .unwrap();
    assert!(
        call(
            &later,
            "fs.mkdir",
            json!({"path":"blocked","expected_absent":true})
        )
        .await
        .unwrap_err()
        .outcome_unknown
    );
    assert!(!moved.join("blocked").exists());
}

struct PausedUnknown {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}
#[async_trait]
impl ExternalHost for PausedUnknown {
    async fn call(
        &self,
        _: &str,
        _: Value,
        _: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        self.entered.notify_one();
        self.release.notified().await;
        Err(CapabilityError::unknown("uncertain effect"))
    }
}
#[tokio::test]
async fn scopes_share_execution_order_and_permanent_unknown_stop() {
    let root = Root::new();
    let external = Arc::new(PausedUnknown {
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let parent = host(
        root.path(),
        HostConfig {
            external: Some(external.clone()),
            ..Default::default()
        },
        Arc::new(Gate::default()),
        Arc::new(Audit::default()),
    );
    let one = Arc::new(
        parent
            .scoped(Arc::new(Gate::default()), Arc::new(Audit::default()))
            .unwrap(),
    );
    let second_gate = Arc::new(Gate::default());
    let second_audit = Arc::new(Audit::default());
    let two = parent
        .scoped(second_gate.clone(), second_audit.clone())
        .unwrap();
    let first = tokio::spawn(async move { call(&one, "mcp.discover", json!({})).await });
    external.entered.notified().await;
    let mut second = Box::pin(call(
        &two,
        "fs.mkdir",
        json!({"path":"never","expected_absent":true}),
    ));
    assert!(futures::poll!(&mut second).is_pending());
    external.release.notify_one();
    assert!(first.await.unwrap().unwrap_err().outcome_unknown);
    assert!(second.await.unwrap_err().outcome_unknown);
    assert!(parent.outcome_unknown());
    assert_eq!(second_gate.calls.load(Ordering::SeqCst), 0);
    assert!(second_audit.records.lock().unwrap().is_empty());
    assert!(!root.path().join("never").exists());
}

struct PausedGate {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}
#[async_trait]
impl PermissionGate for PausedGate {
    async fn authorize(
        &self,
        _: &str,
        _: &Value,
        _: CancellationToken,
    ) -> Result<(), CapabilityError> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(())
    }
}

#[tokio::test]
async fn host_wait_stops_keep_their_type_before_execution() {
    for queued in [false, true] {
        for cancelled in [false, true] {
            tokio::time::timeout(Duration::from_secs(10), async {
                let root = Root::new();
                let gate = Arc::new(PausedGate {
                    entered: tokio::sync::Notify::new(),
                    release: tokio::sync::Notify::new(),
                });
                let audit = Arc::new(Audit::default());
                let host = Host::new(
                    root.path(),
                    gate.clone(),
                    audit.clone(),
                    HostConfig {
                        call_timeout: if cancelled {
                            Duration::from_secs(5)
                        } else {
                            Duration::from_millis(100)
                        },
                        ..Default::default()
                    },
                )
                .unwrap();
                let blocker_cancel = CancellationToken::new();
                let mut blocker =
                    Box::pin(host.call("fs.stat", json!({"path":"."}), blocker_cancel.clone()));
                tokio::select! {
                    result = &mut blocker => panic!("permission wait returned early: {result:?}"),
                    () = gate.entered.notified() => {},
                }
                let cancel = CancellationToken::new();
                let error = if queued {
                    let mut waiting =
                        Box::pin(host.call("fs.stat", json!({"path":"."}), cancel.clone()));
                    assert!(futures::poll!(&mut waiting).is_pending());
                    if cancelled {
                        cancel.cancel();
                    }
                    let error = waiting.await.unwrap_err();
                    blocker_cancel.cancel();
                    assert!(blocker.await.is_err());
                    error
                } else {
                    if cancelled {
                        blocker_cancel.cancel();
                    }
                    blocker.await.unwrap_err()
                };
                assert_eq!(
                    error.stop,
                    Some(if cancelled {
                        CapabilityStop::Cancelled
                    } else {
                        CapabilityStop::Timeout
                    }),
                    "queued={queued}, cancelled={cancelled}: {error:?}"
                );
                assert!(!error.outcome_unknown);
                assert!(!host.outcome_unknown());
                assert!(audit.records.lock().unwrap().is_empty());
            })
            .await
            .expect("Host permission and queue waits must finish");
        }
    }
}

#[tokio::test]
async fn host_byte_budget_stops_leave_ordinary_errors_recoverable() {
    let root = Root::new();
    let audit = Arc::new(Audit::default());
    let host = host(
        root.path(),
        HostConfig {
            max_argument_bytes: 128,
            max_result_bytes: 128,
            ..Default::default()
        },
        Arc::new(Gate::default()),
        audit.clone(),
    );
    let invalid = call(&host, "data.parse", json!({"format":"json","text":"{"}))
        .await
        .unwrap_err();
    assert_eq!(invalid.stop, None);
    assert!(!invalid.outcome_unknown);
    assert_eq!(
        call(&host, "data.parse", json!({"format":"json","text":"7"}))
            .await
            .unwrap(),
        json!({"value":7})
    );
    let oversized = call(
        &host,
        "data.parse",
        json!({"format":"json","text":"x".repeat(200)}),
    )
    .await
    .unwrap_err();
    assert_eq!(oversized.stop, Some(CapabilityStop::Budget));
    assert!(!oversized.outcome_unknown);
    let large = json!({"padding":"x".repeat(200)}).to_string();
    std::fs::write(root.path().join("large.json"), large).unwrap();
    let result = call(&host, "fs.read", json!({"path":"large.json"}))
        .await
        .unwrap_err();
    assert_eq!(result.stop, Some(CapabilityStop::Budget));
    assert!(!result.outcome_unknown);
    assert!(!host.outcome_unknown());
}

struct ErrorExternal;
#[async_trait]
impl ExternalHost for ErrorExternal {
    async fn call(
        &self,
        _: &str,
        _: Value,
        _: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        Err(CapabilityError::new(diagnostic()))
    }
}
#[tokio::test]
async fn diagnostics_from_gate_start_execution_and_settlement_are_json_bounded() {
    for stage in 0..4 {
        let root = Root::new();
        let gate = Arc::new(Gate {
            error: if stage == 0 {
                Some(CapabilityError::new(diagnostic()))
            } else {
                None
            },
            ..Default::default()
        });
        let audit = Arc::new(Audit {
            fail_start: stage == 1,
            fail_settle: stage == 3,
            ..Default::default()
        });
        let host = host(
            root.path(),
            HostConfig {
                max_result_bytes: 16,
                max_total_bytes: 128,
                external: Some(Arc::new(ErrorExternal)),
                ..Default::default()
            },
            gate,
            audit.clone(),
        );
        let error = call(&host, "mcp.discover", json!({})).await.unwrap_err();
        assert!(
            serde_json::to_vec(&error.message).unwrap().len() <= 16,
            "stage {stage}: {:?}",
            error.message
        );
        for record in audit.records.lock().unwrap().iter() {
            if let Some(message) = record.get("message") {
                assert!(
                    serde_json::to_vec(message).unwrap().len() <= 16,
                    "stage {stage}: {record}"
                );
            }
        }
        assert_eq!(error.outcome_unknown, stage == 3);
        assert_eq!(host.outcome_unknown(), stage == 3);
    }
}
#[tokio::test]
async fn settlement_failure_uses_reserved_bytes_and_keeps_bounded_audit_pairs() {
    let root = Root::new();
    let gate = Arc::new(Gate::default());
    let audit = Arc::new(Audit {
        fail_settle: true,
        ..Default::default()
    });
    let host = host(
        root.path(),
        HostConfig {
            max_result_bytes: 16,
            max_total_bytes: 24,
            external: Some(Arc::new(ErrorExternal)),
            ..Default::default()
        },
        gate.clone(),
        audit.clone(),
    );
    let error = call(&host, "mcp.discover", json!({})).await.unwrap_err();
    {
        let records = audit.records.lock().unwrap();
        assert_eq!(records[0]["phase"], "running");
        assert_eq!(records[1]["phase"], "failed");
        assert_eq!(records.last().unwrap()["phase"], "unknown");
        assert_eq!(records[0]["id"], records[1]["id"]);
        let audited = serde_json::to_vec(&records[1]["message"]).unwrap().len();
        let returned = serde_json::to_vec(&error.message).unwrap().len();
        assert!(
            2 + audited + returned <= 24,
            "audit {audited}, returned {returned}"
        );
        assert!(audited <= 16 && returned <= 16);
        assert!(error.outcome_unknown && host.outcome_unknown());
    }
    assert!(
        call(&host, "mcp.discover", json!({}))
            .await
            .unwrap_err()
            .outcome_unknown
    );
    assert_eq!(gate.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn budgets_must_encode_an_empty_json_diagnostic() {
    let root = Root::new();
    for config in [
        HostConfig {
            max_result_bytes: 1,
            ..Default::default()
        },
        HostConfig {
            max_total_bytes: 1,
            ..Default::default()
        },
    ] {
        assert!(
            Host::new(
                root.path(),
                Arc::new(Gate::default()),
                Arc::new(Audit::default()),
                config
            )
            .is_err()
        );
    }
}

#[tokio::test]
async fn scopes_share_remaining_serialized_bytes_without_resetting_them() {
    let root = Root::new();
    let gate = Arc::new(Gate::default());
    let parent = host(
        root.path(),
        HostConfig {
            max_total_bytes: 43,
            ..Default::default()
        },
        gate.clone(),
        Arc::new(Audit::default()),
    );
    call(&parent, "data.parse", json!({"format":"json","text":"1"}))
        .await
        .unwrap();
    let scoped = parent
        .scoped(gate.clone(), Arc::new(Audit::default()))
        .unwrap();
    assert!(
        call(&scoped, "data.parse", json!({"format":"json","text":"1"}))
            .await
            .is_err()
    );
    assert_eq!(gate.calls.load(Ordering::SeqCst), 1);
}

#[cfg(unix)]
#[tokio::test]
async fn failed_recovery_preserves_fence_after_a_known_refusal() {
    let root = Root::new();
    let prepared = pending(root.path()).await;
    let host = host(
        root.path(),
        Default::default(),
        Arc::new(Gate::default()),
        Arc::new(Audit::default()),
    );
    assert!(
        !call(
            &host,
            "fs.mkdir",
            json!({"path":"never","expected_absent":true})
        )
        .await
        .unwrap_err()
        .outcome_unknown
    );
    let mut bad = recovery(&prepared);
    bad["expected_batch_digest"] = json!("0".repeat(64));
    assert!(
        !call(&host, "fs.recover_change_batch", bad)
            .await
            .unwrap_err()
            .outcome_unknown
    );
    assert!(
        !call(
            &host,
            "fs.mkdir",
            json!({"path":"never","expected_absent":true})
        )
        .await
        .unwrap_err()
        .outcome_unknown
    );
    assert_eq!(
        call(
            &host,
            "fs.batch_status",
            json!({"batch_id":prepared["batch_id"]})
        )
        .await
        .unwrap()["outcome_unknown"],
        true
    );
    assert!(!root.path().join("never").exists());
    call(&host, "fs.recover_change_batch", recovery(&prepared))
        .await
        .unwrap();
}

#[tokio::test]
async fn gate_and_start_failures_charge_the_cumulative_budget() {
    for stage in [0, 1] {
        let root = Root::new();
        let gate = Arc::new(Gate {
            error: if stage == 0 {
                Some(CapabilityError::new(diagnostic()))
            } else {
                None
            },
            ..Default::default()
        });
        let audit = Arc::new(Audit {
            fail_start: stage == 1,
            ..Default::default()
        });
        let host = host(
            root.path(),
            HostConfig {
                max_result_bytes: 16,
                max_total_bytes: 20,
                ..Default::default()
            },
            gate.clone(),
            audit,
        );
        let args = json!({"path":"."});
        let error = call(&host, "fs.stat", args.clone()).await.unwrap_err();
        assert!(
            serde_json::to_vec(&args).unwrap().len()
                + serde_json::to_vec(&error.message).unwrap().len()
                <= 20
        );
        assert!(call(&host, "fs.stat", args).await.is_err());
        assert_eq!(gate.calls.load(Ordering::SeqCst), 1);
    }
}

#[cfg(unix)]
#[tokio::test]
async fn exec_permission_denied_is_a_known_failure_and_leaves_host_usable() {
    use std::os::unix::fs::PermissionsExt;
    let root = Root::new();
    let program = root.path().join("not-executable");
    std::fs::write(&program, b"printf forbidden > shell-effect\n").unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o600)).unwrap();
    let audit = Arc::new(Audit::default());
    let host = host(
        root.path(),
        HostConfig {
            profiles: BTreeMap::from([(
                "denied".into(),
                Profile {
                    kind: ProfileKind::Check,
                    program,
                    args: vec![],
                    env: BTreeMap::new(),
                    timeout: Duration::from_secs(10),
                    max_output_bytes: 1024,
                    allow_filter: false,
                    io: cayw_device::ProcessIo::Pipes,
                },
            )]),
            ..Default::default()
        },
        Arc::new(Gate::default()),
        audit.clone(),
    );
    let error = call(&host, "check.run", json!({"profile":"denied"}))
        .await
        .unwrap_err();
    assert!(!root.path().join("shell-effect").exists());
    assert_eq!(host.active_processes(), 0);
    assert!(!error.outcome_unknown, "{error}");
    assert!(!host.outcome_unknown());
    assert_eq!(audit.records.lock().unwrap()[1]["phase"], "failed");
    call(
        &host,
        "fs.mkdir",
        json!({"path":"after-denial","expected_absent":true}),
    )
    .await
    .unwrap();
    assert!(root.path().join("after-denial").is_dir());
    host.drain_processes().await.unwrap();
}

#[cfg(unix)]
#[test]
fn fixture_process() {
    if std::env::var_os("CAYW_HOST_REPAIR_PROCESS").is_some() {
        std::fs::write("process-effect", "effect").unwrap();
    }
    if std::env::var_os("CAYW_HOST_REPAIR_WAIT").is_some() {
        std::fs::write("process.pid", std::process::id().to_string()).unwrap();
        loop {
            std::thread::park_timeout(Duration::from_secs(1));
        }
    }
}
#[cfg(unix)]
#[tokio::test]
async fn replaced_root_keeps_profiles_and_git_in_the_opened_directory() {
    let container = Root::new();
    let original = container.path().join("root");
    let moved = container.path().join("moved");
    std::fs::create_dir(&original).unwrap();
    let host = host(
        &original,
        HostConfig {
            git_program: Some("/usr/bin/git".into()),
            profiles: BTreeMap::from([(
                "fixture".into(),
                Profile {
                    kind: ProfileKind::Check,
                    program: std::env::current_exe().unwrap(),
                    args: vec![
                        "--exact".into(),
                        "fixture_process".into(),
                        "--nocapture".into(),
                    ],
                    env: BTreeMap::from([("CAYW_HOST_REPAIR_PROCESS".into(), "1".into())]),
                    timeout: Duration::from_secs(5),
                    max_output_bytes: 4096,
                    allow_filter: false,
                    io: cayw_device::ProcessIo::Pipes,
                },
            )]),
            ..Default::default()
        },
        Arc::new(Gate::default()),
        Arc::new(Audit::default()),
    );
    repository(&original);
    std::fs::rename(&original, &moved).unwrap();
    let outside = container.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("wrong-tree"), "replacement").unwrap();
    std::os::unix::fs::symlink(&outside, &original).unwrap();
    for (name, args) in [
        ("check.run", json!({"profile":"fixture"})),
        ("git.status", json!({})),
    ] {
        let result = call(&host, name, args).await.unwrap();
        assert_eq!(result["success"], true, "{name}: {result}");
        assert!(!result["stdout"].as_str().unwrap().contains("wrong-tree"));
    }
    assert!(!original.join("process-effect").exists());
    assert!(moved.join("process-effect").exists());
    call(
        &host,
        "fs.mkdir",
        json!({"path":"still-pinned","expected_absent":true}),
    )
    .await
    .unwrap();
    assert!(moved.join("still-pinned").is_dir());
}

#[cfg(unix)]
#[tokio::test]
async fn host_scopes_drain_dropped_processes_and_keep_other_tasks_independent() {
    let root = Root::new();
    let config = HostConfig {
        profiles: BTreeMap::from([(
            "wait".into(),
            Profile {
                kind: ProfileKind::Check,
                program: std::env::current_exe().unwrap(),
                args: vec![
                    "--exact".into(),
                    "fixture_process".into(),
                    "--nocapture".into(),
                ],
                env: BTreeMap::from([("CAYW_HOST_REPAIR_WAIT".into(), "1".into())]),
                timeout: Duration::from_secs(30),
                max_output_bytes: 4096,
                allow_filter: false,
                io: cayw_device::ProcessIo::Pipes,
            },
        )]),
        ..Default::default()
    };
    let parent = host(
        root.path(),
        config.clone(),
        Arc::new(Gate::default()),
        Arc::new(Audit::default()),
    );
    let scope = Arc::new(
        parent
            .scoped(Arc::new(Gate::default()), Arc::new(Audit::default()))
            .unwrap(),
    );
    let runner_scope = scope.clone();
    let call =
        tokio::spawn(
            async move { call(&runner_scope, "check.run", json!({"profile":"wait"})).await },
        );
    let pid = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(text) = std::fs::read_to_string(root.path().join("process.pid"))
                && let Ok(pid) = text.parse::<i32>()
            {
                break pid;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(parent.active_processes(), 1);
    call.abort();
    assert!(call.await.unwrap_err().is_cancelled());
    parent.drain_processes().await.unwrap();
    assert_eq!(scope.active_processes(), 0);
    // A small native process checks that the recorded PID no longer exists. This
    // test package need not expose libc as part of the Host's dependency boundary.
    let observed = std::process::Command::new("/bin/kill")
        .args(["-0", &pid.to_string()])
        .output()
        .unwrap();
    assert!(
        !observed.status.success(),
        "drain returned while PID {pid} was live"
    );
    let fresh_root = Root::new();
    let mut fresh_config = config;
    fresh_config.profiles.get_mut("wait").unwrap().env =
        BTreeMap::from([("CAYW_HOST_REPAIR_PROCESS".into(), "1".into())]);
    let independent = host(
        fresh_root.path(),
        fresh_config,
        Arc::new(Gate::default()),
        Arc::new(Audit::default()),
    );
    assert_eq!(
        self::call(&independent, "check.run", json!({"profile":"wait"}))
            .await
            .unwrap()["success"],
        true
    );
}

#[cfg(unix)]
fn git(root: &Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new("/usr/bin/git")
        .args(args)
        .current_dir(root)
        .env_clear()
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap()
}
#[cfg(unix)]
fn git_object(root: &Path, kind: &str, bytes: &[u8]) -> String {
    use std::{
        io::Write,
        process::{Command, Stdio},
    };
    let mut child = Command::new("/usr/bin/git")
        .args(["hash-object", "-w", "-t", kind, "--stdin"])
        .current_dir(root)
        .env_clear()
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(bytes).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}
#[cfg(unix)]
fn tree_entry(mode: &str, name: &str, id: &str) -> Vec<u8> {
    let mut bytes = format!("{mode} {name}\0").into_bytes();
    bytes.extend(
        (0..id.len())
            .step_by(2)
            .map(|at| u8::from_str_radix(&id[at..at + 2], 16).unwrap()),
    );
    bytes
}
#[cfg(unix)]
fn repository(root: &Path) -> String {
    std::fs::create_dir_all(root.join(".git/objects")).unwrap();
    std::fs::create_dir_all(root.join(".git/refs/heads")).unwrap();
    std::fs::create_dir_all(root.join(".git/info")).unwrap();
    std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/fixture\n").unwrap();
    std::fs::write(
        root.join(".git/config"),
        "[core]\n repositoryformatversion = 0\n bare = false\n",
    )
    .unwrap();
    let blob = git_object(root, "blob", b"original\n");
    let sub = git_object(root, "tree", &tree_entry("100644", "note", &blob));
    let tree = git_object(root, "tree", &tree_entry("40000", "sub", &sub));
    let commit=git_object(root,"commit",format!("tree {tree}\nauthor Fixture <fixture@example.invalid> 1 +0000\ncommitter Fixture <fixture@example.invalid> 1 +0000\n\nfixture\n").as_bytes());
    std::fs::write(root.join(".git/refs/heads/fixture"), format!("{commit}\n")).unwrap();
    assert!(git(root, &["read-tree", &commit]).status.success());
    commit
}
#[cfg(unix)]
#[tokio::test]
async fn git_historical_and_deleted_parent_paths_work_without_following_links() {
    let root = Root::new();
    let commit = repository(root.path());
    let host = host(
        root.path(),
        HostConfig {
            git_program: Some("/usr/bin/git".into()),
            ..Default::default()
        },
        Arc::new(Gate::default()),
        Arc::new(Audit::default()),
    );
    let shown = call(
        &host,
        "git.show",
        json!({"commit":commit,"path":"sub/note"}),
    )
    .await
    .unwrap();
    assert_eq!(shown["success"], true);
    assert_eq!(shown["stdout"], "original\n");
    let diff = call(&host, "git.diff", json!({"path":"sub/note"}))
        .await
        .unwrap();
    assert_eq!(diff["success"], true);
    assert!(diff["stdout"].as_str().unwrap().contains("deleted file"));
    let outside = Root::new();
    std::os::unix::fs::symlink(outside.path(), root.path().join("sub")).unwrap();
    assert!(
        call(&host, "git.diff", json!({"path":"sub/note"}))
            .await
            .is_err()
    );
}
#[cfg(unix)]
#[tokio::test]
async fn git_snapshot_accepts_links_without_following_and_still_bounds_metadata_budgets() {
    for case in [
        "worktree-link",
        "object-link",
        "byte-budget",
        "entry-budget",
    ] {
        let root = Root::new();
        repository(root.path());
        let outside = Root::new();
        std::fs::write(outside.path().join("secret"), "outside").unwrap();
        let mut config = HostConfig {
            git_program: Some("/usr/bin/git".into()),
            ..Default::default()
        };
        match case {
            "worktree-link" => {
                std::os::unix::fs::symlink(outside.path(), root.path().join("linked")).unwrap()
            }
            "object-link" => std::os::unix::fs::symlink(
                outside.path().join("secret"),
                root.path().join(".git/objects/linked"),
            )
            .unwrap(),
            "byte-budget" => config.max_file_bytes = 1,
            "entry-budget" => config.max_list_entries = 1,
            _ => unreachable!(),
        }
        let host = host(
            root.path(),
            config,
            Arc::new(Gate::default()),
            Arc::new(Audit::default()),
        );
        match case {
            // A safe worktree link and a stray object entry are not opened or
            // followed. Object data and the worktree are read in place, so a
            // link is ordinary content for Git's own status output.
            "worktree-link" | "object-link" => {
                let result = call(&host, "git.status", json!({}))
                    .await
                    .unwrap_or_else(|error| panic!("{case}: {}", error.message));
                assert_eq!(result["success"], true, "{case}: {result}");
                assert!(
                    !result["stdout"].as_str().unwrap().contains("outside"),
                    "{case}: a read must not follow the link: {result}"
                );
            }
            // Metadata copies keep their own money bounds even though object and
            // worktree data are no longer copied or size-gated.
            _ => {
                let error = call(&host, "git.status", json!({})).await.unwrap_err();
                assert!(!error.outcome_unknown, "{case}: {error}");
            }
        }
        assert_eq!(host.active_processes(), 0);
        assert_eq!(
            std::fs::read_to_string(outside.path().join("secret")).unwrap(),
            "outside"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn git_passive_metadata_retains_local_excludes_and_split_indexes() {
    let root = Root::new();
    repository(root.path());
    std::fs::create_dir(root.path().join("sub")).unwrap();
    std::fs::write(root.path().join("sub/note"), "original\n").unwrap();
    std::fs::write(root.path().join(".git/info/exclude"), "local-secret\n").unwrap();
    std::fs::write(root.path().join("local-secret"), "secret").unwrap();
    let host = host(
        root.path(),
        HostConfig {
            git_program: Some("/usr/bin/git".into()),
            ..Default::default()
        },
        Arc::new(Gate::default()),
        Arc::new(Audit::default()),
    );
    let status = call(&host, "git.status", json!({})).await.unwrap();
    assert_eq!(status["success"], true);
    assert!(
        !status["stdout"].as_str().unwrap().contains("local-secret"),
        "{status}"
    );
    assert!(
        git(root.path(), &["update-index", "--split-index"])
            .status
            .success()
    );
    let baseline = git(root.path(), &["status", "--porcelain=v1"]);
    assert!(baseline.status.success());
    let status = call(&host, "git.status", json!({})).await.unwrap();
    assert_eq!(status["success"], true, "{status}");
    assert_eq!(
        status["stdout"].as_str().unwrap(),
        String::from_utf8(baseline.stdout).unwrap()
    );
    assert!(
        !host
            .capabilities()
            .iter()
            .any(|cap| ["git.add", "git.commit", "git.push"].contains(&cap.name.as_str()))
    );
}
