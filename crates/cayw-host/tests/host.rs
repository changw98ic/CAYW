use async_trait::async_trait;
use cayw_ai::CancellationToken;
use cayw_codemode::{CapabilityError, CapabilityHost, CodeMode, Limits};
use cayw_host::{AuditSink, ExternalHost, Host, HostConfig, PermissionGate, Profile, ProfileKind};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

static NEXT: AtomicU64 = AtomicU64::new(1);
struct Workspace(PathBuf);
impl Workspace {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "cayw-host-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&root).unwrap();
        Self(root)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Gate {
    allow: bool,
    calls: Mutex<Vec<String>>,
}
#[async_trait]
impl PermissionGate for Gate {
    async fn authorize(
        &self,
        capability: &str,
        _: &Value,
        cancel: CancellationToken,
    ) -> Result<(), CapabilityError> {
        self.calls.lock().unwrap().push(capability.into());
        if self.allow && !cancel.is_cancelled() {
            Ok(())
        } else {
            Err(CapabilityError::new("denied"))
        }
    }
}
#[derive(Default)]
struct Audit {
    records: Mutex<Vec<String>>,
    fail_start: bool,
    fail_settle: bool,
}
#[async_trait]
impl AuditSink for Audit {
    async fn started(&self, capability: &str, _: &Value) -> Result<String, CapabilityError> {
        if self.fail_start {
            return Err(CapabilityError::new("journal unavailable"));
        }
        let mut records = self.records.lock().unwrap();
        let id = records.len().to_string();
        records.push(format!("start:{capability}"));
        Ok(id)
    }
    async fn settled(
        &self,
        _: &str,
        result: &Result<Value, CapabilityError>,
    ) -> Result<(), CapabilityError> {
        if self.fail_settle {
            return Err(CapabilityError::new("journal unavailable"));
        }
        self.records.lock().unwrap().push(
            if result.is_ok() {
                "success"
            } else if result.as_ref().unwrap_err().outcome_unknown {
                "unknown"
            } else {
                "failure"
            }
            .into(),
        );
        Ok(())
    }
    fn dropped(&self, _: &str) -> Result<(), CapabilityError> {
        self.records.lock().unwrap().push("dropped".into());
        Ok(())
    }
}
fn host(root: &Path, config: HostConfig) -> (Arc<Host>, Arc<Gate>, Arc<Audit>) {
    let gate = Arc::new(Gate {
        allow: true,
        calls: Mutex::new(vec![]),
    });
    let audit = Arc::new(Audit::default());
    (
        Arc::new(Host::new(root, gate.clone(), audit.clone(), config).unwrap()),
        gate,
        audit,
    )
}
async fn call(host: &Host, name: &str, args: Value) -> Result<Value, CapabilityError> {
    host.call(name, args, CancellationToken::new()).await
}

#[tokio::test]
async fn schema_permission_and_audit_failures_prevent_file_changes() {
    let root = Workspace::new();
    let gate = Arc::new(Gate {
        allow: false,
        calls: Mutex::new(vec![]),
    });
    let audit = Arc::new(Audit::default());
    let denied = Host::new(
        root.path(),
        gate.clone(),
        audit.clone(),
        HostConfig::default(),
    )
    .unwrap();
    assert!(call(
        &denied,
        "fs.write",
        json!({"path":"new", "content":"hello", "arbitrary":true})
    )
    .await
    .is_err());
    assert!(gate.calls.lock().unwrap().is_empty());
    assert!(call(
        &denied,
        "fs.write",
        json!({"path":"new", "content":"hello"})
    )
    .await
    .is_err());
    assert!(!root.path().join("new").exists());
    assert!(audit.records.lock().unwrap().is_empty());
    let gate = Arc::new(Gate {
        allow: true,
        calls: Mutex::new(vec![]),
    });
    let audit = Arc::new(Audit {
        fail_start: true,
        ..Default::default()
    });
    let no_journal = Host::new(root.path(), gate, audit, HostConfig::default()).unwrap();
    assert!(call(
        &no_journal,
        "fs.write",
        json!({"path":"new", "content":"hello"})
    )
    .await
    .is_err());
    assert!(!root.path().join("new").exists());
}

#[tokio::test]
async fn real_file_round_trip_requires_compare_and_swap_and_exact_patch() {
    let root = Workspace::new();
    let (host, gate, audit) = host(root.path(), HostConfig::default());
    let first = call(
        &host,
        "fs.write",
        json!({"path":"note", "content":"hello world"}),
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(root.path().join("note")).unwrap(),
        "hello world"
    );
    let read = call(&host, "fs.read", json!({"path":"note"}))
        .await
        .unwrap();
    assert_eq!(read["content"], "hello world");
    assert_eq!(read["sha256"], first["sha256"]);
    assert!(
        call(&host, "fs.write", json!({"path":"note", "content":"lost"}))
            .await
            .is_err()
    );
    assert!(call(
        &host,
        "fs.write",
        json!({"path":"note", "content":"lost", "overwrite":true})
    )
    .await
    .is_err());
    let patched = call(
        &host,
        "fs.patch",
        json!({"path":"note", "expected_sha256":read["sha256"], "old":"world", "new":"Rust"}),
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(root.path().join("note")).unwrap(),
        "hello Rust"
    );
    assert!(call(
        &host,
        "fs.patch",
        json!({"path":"note", "expected_sha256":read["sha256"], "old":"hello", "new":"lost"})
    )
    .await
    .is_err());
    call(&host, "fs.write", json!({"path":"note", "content":"aa", "overwrite":true, "expected_sha256":patched["sha256"]})).await.unwrap();
    let duplicate = call(&host, "fs.read", json!({"path":"note"}))
        .await
        .unwrap();
    assert!(call(
        &host,
        "fs.patch",
        json!({"path":"note", "expected_sha256":duplicate["sha256"], "old":"a", "new":"b"})
    )
    .await
    .is_err());
    let stat = call(&host, "fs.stat", json!({"path":"note"}))
        .await
        .unwrap();
    assert_eq!(stat["size"], 2);
    let list = call(&host, "fs.list", json!({"path":"."})).await.unwrap();
    assert_eq!(list["entries"].as_array().unwrap().len(), 1);
    assert_eq!(list["entries"][0]["name"], "note");
    assert!(!gate.calls.lock().unwrap().is_empty());
    assert_eq!(audit.records.lock().unwrap().last().unwrap(), "success");
}

#[tokio::test]
async fn patch_rejects_overlapping_matches_and_changed_or_missing_targets() {
    let root = Workspace::new();
    std::fs::write(root.path().join("note"), "aaa").unwrap();
    let (host, _, _) = host(root.path(), HostConfig::default());
    let read = call(&host, "fs.read", json!({"path":"note"}))
        .await
        .unwrap();
    assert!(call(
        &host,
        "fs.patch",
        json!({"path":"note","expected_sha256":read["sha256"],"old":"aa","new":"b"})
    )
    .await
    .is_err());
    assert_eq!(
        std::fs::read_to_string(root.path().join("note")).unwrap(),
        "aaa"
    );
    assert!(call(
        &host,
        "fs.write",
        json!({"path":"missing","content":"bad","overwrite":true,"expected_sha256":read["sha256"]})
    )
    .await
    .is_err());
    let missing = call(
        &host,
        "fs.patch",
        json!({"path":"missing","expected_sha256":read["sha256"],"old":"aaa","new":"bad"}),
    )
    .await
    .unwrap_err();
    assert!(!missing.outcome_unknown);
    assert!(!root.path().join("missing").exists());
    let unmatched = call(
        &host,
        "fs.patch",
        json!({"path":"note","expected_sha256":read["sha256"],"old":"absent","new":"bad"}),
    )
    .await
    .unwrap_err();
    assert!(!unmatched.outcome_unknown);
    assert_eq!(std::fs::read(root.path().join("note")).unwrap(), b"aaa");
    assert!(!host.outcome_unknown());
}

#[cfg(unix)]
#[tokio::test]
async fn atomic_replacement_preserves_restrictive_existing_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let root = Workspace::new();
    std::fs::write(root.path().join("secret"), "private").unwrap();
    std::fs::set_permissions(
        root.path().join("secret"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let (host, _, _) = host(root.path(), HostConfig::default());
    let read = call(&host, "fs.read", json!({"path":"secret"}))
        .await
        .unwrap();
    call(&host, "fs.write", json!({"path":"secret","content":"still private","overwrite":true,"expected_sha256":read["sha256"]})).await.unwrap();
    assert_eq!(
        std::fs::metadata(root.path().join("secret"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn separate_hosts_cannot_both_commit_the_same_expected_hash() {
    let root = Workspace::new();
    std::fs::write(root.path().join("note"), "initial").unwrap();
    let (one, _, _) = host(root.path(), HostConfig::default());
    let (two, _, _) = host(root.path(), HostConfig::default());
    let hash = call(&one, "fs.read", json!({"path":"note"})).await.unwrap()["sha256"].clone();
    let args = json!({"path":"note", "content":"one", "overwrite":true, "expected_sha256":hash});
    let args_two =
        json!({"path":"note", "content":"two", "overwrite":true, "expected_sha256":hash});
    let a = tokio::spawn(async move { call(&one, "fs.write", args).await });
    let b = tokio::spawn(async move { call(&two, "fs.write", args_two).await });
    let (a, b) = (a.await.unwrap(), b.await.unwrap());
    assert_ne!(a.is_ok(), b.is_ok());
    assert!(matches!(
        std::fs::read_to_string(root.path().join("note"))
            .unwrap()
            .as_str(),
        "one" | "two"
    ));
}

#[tokio::test]
async fn paths_symlinks_and_byte_limits_are_enforced() {
    let root = Workspace::new();
    let outside = Workspace::new();
    std::fs::write(outside.path().join("secret"), "outside").unwrap();
    let config = HostConfig {
        max_file_bytes: 8,
        ..Default::default()
    };
    let (host, _, _) = host(root.path(), config);
    for path in [
        "../secret",
        "/etc/passwd",
        "x/../secret",
        ".cayw-host-lock",
        ".CAYW-HOST-LOCK",
        ".cayw-workspace",
        ".CAYW-WORKSPACE/state.json",
        "nested/.cayw-workspace/candidate-0",
        ".git/config",
        "nested/.GiT/hooks/run",
    ] {
        assert!(
            call(&host, "fs.read", json!({"path":path})).await.is_err(),
            "{path}"
        );
        assert!(
            call(&host, "fs.write", json!({"path":path,"content":"bad"}))
                .await
                .is_err(),
            "{path}"
        );
    }
    assert!(call(
        &host,
        "fs.write",
        json!({"path":"too-big","content":"你好世界"})
    )
    .await
    .is_err());
    assert!(!root.path().join("too-big").exists());
    std::fs::write(root.path().join("too-big"), "123456789").unwrap();
    assert!(call(&host, "fs.read", json!({"path":"too-big"}))
        .await
        .is_err());
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(outside.path(), root.path().join("escape")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret"), root.path().join("link"))
            .unwrap();
        for path in ["escape/secret", "link"] {
            assert!(call(&host, "fs.read", json!({"path":path})).await.is_err());
            assert!(
                call(
                    &host,
                    "fs.write",
                    json!({"path":path,"content":"bad","overwrite":true,"expected_sha256":"0000000000000000000000000000000000000000000000000000000000000000"})
                )
                .await
                .is_err()
            );
        }
        assert_eq!(
            std::fs::read_to_string(outside.path().join("secret")).unwrap(),
            "outside"
        );
    }
}

#[tokio::test]
async fn failed_settlement_retains_unknown_and_stops_further_calls() {
    let root = Workspace::new();
    let audit = Arc::new(Audit {
        fail_settle: true,
        ..Default::default()
    });
    let gate = Arc::new(Gate {
        allow: true,
        calls: Mutex::new(vec![]),
    });
    let host = Host::new(root.path(), gate, audit.clone(), HostConfig::default()).unwrap();
    let error = call(
        &host,
        "fs.write",
        json!({"path":"note","content":"committed"}),
    )
    .await
    .unwrap_err();
    assert!(error.outcome_unknown);
    assert_eq!(
        std::fs::read_to_string(root.path().join("note")).unwrap(),
        "committed"
    );
    assert!(
        call(&host, "fs.read", json!({"path":"note"}))
            .await
            .unwrap_err()
            .outcome_unknown
    );
    assert!(audit.records.lock().unwrap().contains(&"dropped".into()));
}

#[tokio::test]
async fn result_and_directory_budgets_prevent_unbounded_data() {
    let root = Workspace::new();
    std::fs::write(root.path().join("a"), "a").unwrap();
    std::fs::write(root.path().join("b"), "b").unwrap();
    let (limited, _, _) = host(
        root.path(),
        HostConfig {
            max_list_entries: 1,
            ..Default::default()
        },
    );
    assert!(call(&limited, "fs.list", json!({"path":"."}))
        .await
        .is_err());
    let (tiny, _, audit) = host(
        root.path(),
        HostConfig {
            max_result_bytes: 8,
            ..Default::default()
        },
    );
    let error = call(&tiny, "fs.write", json!({"path":"committed","content":"x"}))
        .await
        .unwrap_err();
    assert!(error.outcome_unknown);
    assert_eq!(
        std::fs::read_to_string(root.path().join("committed")).unwrap(),
        "x"
    );
    assert_eq!(audit.records.lock().unwrap().last().unwrap(), "unknown");
}

struct DelayedUnknownExternal {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}
#[async_trait]
impl ExternalHost for DelayedUnknownExternal {
    async fn call(
        &self,
        _: &str,
        _: Value,
        _: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        self.entered.notify_one();
        self.release.notified().await;
        Err(CapabilityError::unknown(
            "external effect needs reconciliation",
        ))
    }
}
#[tokio::test]
async fn queued_call_cannot_execute_while_an_admitted_call_is_unsettled() {
    let root = Workspace::new();
    let external = Arc::new(DelayedUnknownExternal {
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let (host, _, _) = host(
        root.path(),
        HostConfig {
            external: Some(external.clone()),
            ..Default::default()
        },
    );
    let clone = host.clone();
    let first = tokio::spawn(async move {
        call(
            &clone,
            "mcp.call",
            json!({"server":"fixture","tool":"wait","arguments":{}}),
        )
        .await
    });
    external.entered.notified().await;
    let mut second = Box::pin(call(
        &host,
        "fs.write",
        json!({"path":"never","content":"bad"}),
    ));
    assert!(futures::poll!(&mut second).is_pending());
    assert!(!root.path().join("never").exists());
    external.release.notify_one();
    assert!(first.await.unwrap().unwrap_err().outcome_unknown);
    assert!(second.await.unwrap_err().outcome_unknown);
    assert!(!root.path().join("never").exists());
}

struct DropFailureExternal;
#[async_trait]
impl AuditSink for DropFailureExternal {
    async fn started(&self, _: &str, _: &Value) -> Result<String, CapabilityError> {
        Ok("call".into())
    }
    async fn settled(
        &self,
        _: &str,
        _: &Result<Value, CapabilityError>,
    ) -> Result<(), CapabilityError> {
        Ok(())
    }
    fn dropped(&self, _: &str) -> Result<(), CapabilityError> {
        Err(CapabilityError::new("drop persistence unavailable"))
    }
}
#[tokio::test]
async fn dropped_audit_failure_is_observable_and_permanently_blocks_host() {
    let root = Workspace::new();
    let external = Arc::new(WaitingExternal {
        entered: tokio::sync::Notify::new(),
    });
    let gate = Arc::new(Gate {
        allow: true,
        calls: Mutex::new(vec![]),
    });
    let host = Arc::new(
        Host::new(
            root.path(),
            gate,
            Arc::new(DropFailureExternal),
            HostConfig {
                external: Some(external.clone()),
                ..Default::default()
            },
        )
        .unwrap(),
    );
    let cloned = host.clone();
    let task = tokio::spawn(async move {
        call(
            &cloned,
            "mcp.call",
            json!({"server":"fixture","tool":"wait","arguments":{}}),
        )
        .await
    });
    external.entered.notified().await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(
        host.audit_failure().as_deref(),
        Some("drop persistence unavailable")
    );
    assert!(
        call(&host, "fs.write", json!({"path":"never","content":"bad"}))
            .await
            .unwrap_err()
            .outcome_unknown
    );
    assert!(!root.path().join("never").exists());
}

struct UnknownGate;
#[async_trait]
impl PermissionGate for UnknownGate {
    async fn authorize(
        &self,
        _: &str,
        _: &Value,
        _: CancellationToken,
    ) -> Result<(), CapabilityError> {
        Err(CapabilityError::unknown(
            "permission storage outcome unknown",
        ))
    }
}
#[tokio::test]
async fn unknown_permission_result_stops_subsequent_calls() {
    let root = Workspace::new();
    let host = Host::new(
        root.path(),
        Arc::new(UnknownGate),
        Arc::new(Audit::default()),
        HostConfig::default(),
    )
    .unwrap();
    assert!(
        call(&host, "fs.write", json!({"path":"never","content":"bad"}))
            .await
            .unwrap_err()
            .outcome_unknown
    );
    assert!(host.outcome_unknown());
    assert!(!root.path().join("never").exists());
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
        Err(CapabilityError::new("x".repeat(2048)))
    }
}
#[tokio::test]
async fn external_error_payload_is_bounded_before_audit_and_return() {
    let root = Workspace::new();
    let (host, _, _) = host(
        root.path(),
        HostConfig {
            external: Some(Arc::new(ErrorExternal)),
            max_result_bytes: 64,
            ..Default::default()
        },
    );
    let error = call(
        &host,
        "mcp.call",
        json!({"server":"fixture","tool":"fail","arguments":{}}),
    )
    .await
    .unwrap_err();
    assert!(error.message.len() <= 64);
    assert!(!error.outcome_unknown);
}

struct WaitingGate {
    entered: tokio::sync::Notify,
}
#[async_trait]
impl PermissionGate for WaitingGate {
    async fn authorize(
        &self,
        _: &str,
        _: &Value,
        _: CancellationToken,
    ) -> Result<(), CapabilityError> {
        self.entered.notify_one();
        std::future::pending().await
    }
}
#[tokio::test]
async fn cancellation_while_awaiting_permission_never_starts_audit_or_writes() {
    let root = Workspace::new();
    let gate = Arc::new(WaitingGate {
        entered: tokio::sync::Notify::new(),
    });
    let audit = Arc::new(Audit::default());
    let host = Arc::new(
        Host::new(
            root.path(),
            gate.clone(),
            audit.clone(),
            HostConfig::default(),
        )
        .unwrap(),
    );
    let token = CancellationToken::new();
    let cloned = host.clone();
    let child = token.clone();
    let task = tokio::spawn(async move {
        cloned
            .call("fs.write", json!({"path":"never","content":"bad"}), child)
            .await
    });
    gate.entered.notified().await;
    token.cancel();
    let error = task.await.unwrap().unwrap_err();
    assert!(!error.outcome_unknown);
    assert!(!host.outcome_unknown());
    assert!(audit.records.lock().unwrap().is_empty());
    assert!(!root.path().join("never").exists());
}

#[tokio::test]
async fn external_call_deadline_retains_unknown_and_cancels_child_token() {
    let root = Workspace::new();
    let external = Arc::new(WaitingExternal {
        entered: tokio::sync::Notify::new(),
    });
    let (host, _, audit) = host(
        root.path(),
        HostConfig {
            external: Some(external.clone()),
            // Durable generation admission must finish before testing an actual
            // dispatched service timeout, including on a busy test filesystem.
            call_timeout: Duration::from_secs(2),
            ..Default::default()
        },
    );
    let error = call(
        &host,
        "mcp.call",
        json!({"server":"fixture","tool":"wait","arguments":{}}),
    )
    .await
    .unwrap_err();
    tokio::time::timeout(Duration::from_secs(1), external.entered.notified())
        .await
        .expect("service must actually dispatch before the deadline");
    assert!(error.outcome_unknown);
    assert_eq!(audit.records.lock().unwrap().last().unwrap(), "unknown");
}

struct ImmediateExternal;
#[async_trait]
impl ExternalHost for ImmediateExternal {
    async fn call(
        &self,
        name: &str,
        arguments: Value,
        _: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        Ok(json!({"capability":name,"arguments":arguments}))
    }
}

#[tokio::test]
async fn external_services_use_strict_schemas_permission_and_settlement() {
    let root = Workspace::new();
    let config = HostConfig {
        external: Some(Arc::new(ImmediateExternal)),
        ..Default::default()
    };
    let (host, gate, audit) = host(root.path(), config.clone());
    let cases = [
        (
            "search.query",
            json!({"query":"needle","path":".","limit":10}),
        ),
        ("mcp.discover", json!({})),
        ("mcp.describe", json!({"server":"fixture","tool":"echo"})),
        (
            "mcp.call",
            json!({"server":"fixture","tool":"echo","arguments":{"text":"hello"}}),
        ),
        ("skills.list", json!({})),
        ("skills.read", json!({"name":"fixture"})),
    ];
    for (name, args) in &cases {
        let result = call(&host, name, args.clone()).await.unwrap();
        assert_eq!(result["capability"], *name);
        assert_eq!(&result["arguments"], args);
    }
    assert_eq!(gate.calls.lock().unwrap().len(), 6);
    assert_eq!(audit.records.lock().unwrap().len(), 12);
    assert!(call(
        &host,
        "mcp.call",
        json!({"server":"fixture","tool":"echo","arguments":[],"extra":true})
    )
    .await
    .is_err());
    assert_eq!(gate.calls.lock().unwrap().len(), 6);
    let denied_gate = Arc::new(Gate {
        allow: false,
        calls: Mutex::new(vec![]),
    });
    let denied_audit = Arc::new(Audit::default());
    let denied = Host::new(root.path(), denied_gate, denied_audit.clone(), config).unwrap();
    for (name, args) in cases {
        assert!(call(&denied, name, args).await.is_err());
    }
    assert!(denied_audit.records.lock().unwrap().is_empty());
}

struct DelayedStart {
    entered: tokio::sync::Notify,
}
#[async_trait]
impl AuditSink for DelayedStart {
    async fn started(&self, _: &str, _: &Value) -> Result<String, CapabilityError> {
        self.entered.notify_one();
        std::future::pending().await
    }
    async fn settled(
        &self,
        _: &str,
        _: &Result<Value, CapabilityError>,
    ) -> Result<(), CapabilityError> {
        Ok(())
    }
    fn dropped(&self, _: &str) -> Result<(), CapabilityError> {
        Ok(())
    }
}
#[tokio::test]
async fn dropping_while_start_audit_is_pending_still_blocks_future_execution() {
    let root = Workspace::new();
    let audit = Arc::new(DelayedStart {
        entered: tokio::sync::Notify::new(),
    });
    let gate = Arc::new(Gate {
        allow: true,
        calls: Mutex::new(vec![]),
    });
    let host =
        Arc::new(Host::new(root.path(), gate, audit.clone(), HostConfig::default()).unwrap());
    let clone = host.clone();
    let task = tokio::spawn(async move {
        call(&clone, "fs.write", json!({"path":"never","content":"bad"})).await
    });
    audit.entered.notified().await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert!(host.outcome_unknown());
    assert!(!root.path().join("never").exists());
}

struct WaitingExternal {
    entered: tokio::sync::Notify,
}
#[async_trait]
impl ExternalHost for WaitingExternal {
    async fn call(
        &self,
        _: &str,
        _: Value,
        _: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        self.entered.notify_one();
        std::future::pending().await
    }
}
#[tokio::test]
async fn dropping_an_external_call_records_unknown_and_cancels_the_host() {
    let root = Workspace::new();
    let external = Arc::new(WaitingExternal {
        entered: tokio::sync::Notify::new(),
    });
    let (host, _, audit) = host(
        root.path(),
        HostConfig {
            external: Some(external.clone()),
            ..Default::default()
        },
    );
    let cloned = host.clone();
    let task = tokio::spawn(async move {
        call(
            &cloned,
            "mcp.call",
            json!({"server":"fixture", "tool":"wait", "arguments":{}}),
        )
        .await
    });
    external.entered.notified().await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert!(audit.records.lock().unwrap().contains(&"dropped".into()));
    assert!(
        call(&host, "skills.list", json!({}))
            .await
            .unwrap_err()
            .outcome_unknown
    );
}

#[tokio::test]
async fn cancelled_and_over_budget_calls_do_not_reach_permission_or_storage() {
    let root = Workspace::new();
    let (host, gate, audit) = host(
        root.path(),
        HostConfig {
            max_argument_bytes: 64,
            max_calls: 1,
            ..Default::default()
        },
    );
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(host
        .call("fs.write", json!({"path":"x","content":"a"}), cancel)
        .await
        .is_err());
    assert!(call(
        &host,
        "fs.write",
        json!({"path":"x","content":"a".repeat(65)})
    )
    .await
    .is_err());
    assert!(gate.calls.lock().unwrap().is_empty());
    call(&host, "fs.write", json!({"path":"x","content":"a"}))
        .await
        .unwrap();
    assert!(call(&host, "fs.write", json!({"path":"y","content":"b"}))
        .await
        .is_err());
    assert!(!root.path().join("y").exists());
    assert_eq!(audit.records.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn codemode_uses_the_same_permission_and_audit_boundary() {
    let root = Workspace::new();
    let (host, _, audit) = host(root.path(), HostConfig::default());
    let mode = CodeMode::new(host, Limits::default()).unwrap();
    let report = mode
        .execute(
            "return cayw.call('fs.write', {path='from-luau', content='hello'})",
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(!report.outcome_unknown);
    assert_eq!(
        std::fs::read_to_string(root.path().join("from-luau")).unwrap(),
        "hello"
    );
    assert_eq!(
        *audit.records.lock().unwrap(),
        vec!["start:fs.write", "success"]
    );
}

#[tokio::test]
async fn luau_data_and_change_batch_share_the_host_gate_and_durable_lane() {
    let root = Workspace::new();
    let (host, gate, audit) = host(root.path(), HostConfig::default());
    let mode = CodeMode::new(host, Limits::default()).unwrap();
    let report = mode.execute(r#"
        local parsed = cayw.call('data.parse', {format='json', text='{"answer":42}'})
        local encoded = cayw.call('data.serialize', {format='json', value=parsed.value})
        local hashed = cayw.call('hash.digest', {algorithm='sha256', input={kind='utf8',value='abc'}})
        local batch = cayw.call('fs.prepare_change_batch', {operations={{
            op='write', path='answer.json', expected={kind='absent'},
            content=encoded.text, encoding='utf8', language='json'
        }}})
        local committed = cayw.call('fs.submit_change_batch', {
            batch_id=batch.batch_id, expected_batch_digest=batch.batch_digest
        })
        local read = cayw.call('fs.read', {path='answer.json'})
        local status = cayw.call('fs.batch_status', {batch_id=batch.batch_id})
        return {answer=parsed.value.answer, hex=hashed.hex, content=read.content,
            prepared=batch.user_files_changed, committed=committed.status, status=status.status}
    "#, CancellationToken::new()).await.unwrap();
    assert_eq!(
        report.return_value,
        json!({
            "answer":42,
            "hex":"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            "content":"{\"answer\":42}", "prepared":false, "committed":"committed", "status":"committed"
        })
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("answer.json")).unwrap(),
        "{\"answer\":42}"
    );
    assert_eq!(
        *gate.calls.lock().unwrap(),
        vec![
            "data.parse",
            "data.serialize",
            "hash.digest",
            "fs.prepare_change_batch",
            "fs.submit_change_batch",
            "fs.read",
            "fs.batch_status"
        ]
    );
    assert_eq!(audit.records.lock().unwrap().len(), 14);
    assert!(audit
        .records
        .lock()
        .unwrap()
        .chunks_exact(2)
        .all(|records| records[1] == "success"));
}

#[tokio::test]
async fn listing_and_denied_builtins_never_create_workspace_files() {
    let root = Workspace::new();
    let gate = Arc::new(Gate {
        allow: false,
        calls: Mutex::new(vec![]),
    });
    let audit = Arc::new(Audit::default());
    let host = Host::new(
        root.path(),
        gate.clone(),
        audit.clone(),
        HostConfig::default(),
    )
    .unwrap();
    for name in [
        "data.parse",
        "hash.digest",
        "fs.mkdir",
        "fs.prepare_change_batch",
        "fs.batch_status",
    ] {
        assert!(
            host.capabilities()
                .iter()
                .any(|definition| definition.name == name),
            "{name}"
        );
    }
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    for (name, args) in [
        ("data.parse", json!({"format":"json","text":"{}"})),
        ("fs.mkdir", json!({"path":"denied","expected_absent":true})),
        (
            "fs.prepare_change_batch",
            json!({"operations":[{"op":"write","path":"denied.json","expected":{"kind":"absent"},"content":"{}","encoding":"utf8","language":"json"}]}),
        ),
        ("fs.batch_status", json!({})),
    ] {
        let error = call(&host, name, args).await.unwrap_err();
        assert_eq!(error.message, "denied");
        assert!(!error.outcome_unknown);
    }
    assert_eq!(gate.calls.lock().unwrap().len(), 4);
    assert!(audit.records.lock().unwrap().is_empty());
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn workspace_capabilities_do_not_fall_through_the_old_filesystem_dispatch() {
    let root = Workspace::new();
    let (host, _, _) = host(root.path(), HostConfig::default());
    call(
        &host,
        "fs.mkdir",
        json!({"path":"child","expected_absent":true}),
    )
    .await
    .unwrap();
    let write = call(
        &host,
        "fs.write",
        json!({"path":"child/source","content":"bounded"}),
    )
    .await
    .unwrap();
    call(&host, "fs.move", json!({"source":"child/source","destination":"child/target","expected_sha256":write["sha256"],"expected_destination_absent":true})).await.unwrap();
    let tree = call(&host, "fs.tree", json!({"path":".","max_entries":8}))
        .await
        .unwrap();
    assert_eq!(tree["entries"].as_array().unwrap().len(), 3);
    assert!(!root.path().join("child/source").exists());
    assert_eq!(
        std::fs::read_to_string(root.path().join("child/target")).unwrap(),
        "bounded"
    );
    call(
        &host,
        "fs.remove",
        json!({"path":"child/target","expected_sha256":write["sha256"]}),
    )
    .await
    .unwrap();
    call(&host, "fs.remove", json!({"path":"child"}))
        .await
        .unwrap();
    assert!(!root.path().join("child").exists());
}

#[test]
fn invalid_builtin_config_is_rejected_before_opening_a_store() {
    for config in [
        HostConfig {
            data: Some(cayw_data::Limits {
                max_depth: 0,
                ..Default::default()
            }),
            ..Default::default()
        },
        HostConfig {
            workspace: Some(cayw_workspace::Limits {
                max_batches: 0,
                ..Default::default()
            }),
            ..Default::default()
        },
    ] {
        let root = Workspace::new();
        let gate = Arc::new(Gate {
            allow: true,
            calls: Mutex::new(vec![]),
        });
        assert!(Host::new(root.path(), gate, Arc::new(Audit::default()), config).is_err());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }
}

#[tokio::test]
async fn arbitrary_json_depth_is_rejected_before_permission_or_audit() {
    let root = Workspace::new();
    let (host, gate, audit) = host(root.path(), Default::default());
    let mut value = json!(1);
    for _ in 0..16_000 {
        value = Value::Array(vec![value]);
    }
    let mut args = serde_json::Map::new();
    args.insert("format".into(), json!("json"));
    args.insert("value".into(), value);
    let error = call(&host, "data.serialize", Value::Object(args))
        .await
        .unwrap_err();
    assert!(error.message.contains("nesting"), "{error}");
    assert!(gate.calls.lock().unwrap().is_empty());
    assert!(audit.records.lock().unwrap().is_empty());
}

#[tokio::test]
async fn builtin_limits_and_invalid_schemas_reject_before_permission_or_storage() {
    let root = Workspace::new();
    let (host, gate, audit) = host(
        root.path(),
        HostConfig {
            data: Some(cayw_data::Limits {
                input_bytes: 64,
                ..Default::default()
            }),
            workspace: Some(cayw_workspace::Limits {
                max_argument_bytes: 64,
                ..Default::default()
            }),
            ..Default::default()
        },
    );
    for (name, args) in [
        (
            "data.parse",
            json!({"format":"json","text":"{}","extra":true}),
        ),
        ("data.parse", json!({"format":"json","text":"x".repeat(65)})),
        (
            "hash.digest",
            json!({"algorithm":"sha256","input":{"kind":"bytes","value":[256]}}),
        ),
        ("fs.mkdir", json!({"path":"denied","expected_absent":false})),
        (
            "fs.mkdir",
            json!({"path":"x".repeat(65),"expected_absent":true}),
        ),
        (
            "fs.prepare_change_batch",
            json!({"operations":[{"op":"write","path":"bad.json","expected":{"kind":"absent"},"content":"{}","encoding":"utf8","language":"json","extra":true}]}),
        ),
    ] {
        assert!(call(&host, name, args).await.is_err());
    }
    assert!(gate.calls.lock().unwrap().is_empty());
    assert!(audit.records.lock().unwrap().is_empty());
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn builtin_data_audit_failure_keeps_the_host_stopped() {
    let root = Workspace::new();
    let gate = Arc::new(Gate {
        allow: true,
        calls: Mutex::new(vec![]),
    });
    let audit = Arc::new(Audit {
        fail_settle: true,
        ..Default::default()
    });
    let host = Host::new(root.path(), gate, audit.clone(), Default::default()).unwrap();
    assert!(
        call(
            &host,
            "data.parse",
            json!({"format":"json","text":"{\"answer\":42}"})
        )
        .await
        .unwrap_err()
        .outcome_unknown
    );
    assert!(host.audit_failure().is_some());
    assert!(
        call(
            &host,
            "hash.digest",
            json!({"algorithm":"sha256","input":{"kind":"utf8","value":"abc"}})
        )
        .await
        .unwrap_err()
        .outcome_unknown
    );
    assert!(audit.records.lock().unwrap().contains(&"dropped".into()));
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

#[cfg(unix)]
#[tokio::test]
async fn archives_use_the_builtin_executor_and_cannot_reach_outside_the_root() {
    let root = Workspace::new();
    // A minimal hand-built uncompressed TAR, independent of the archive executor.
    let mut header = [0u8; 512];
    header[..8].copy_from_slice(b"note.txt");
    header[100..108].copy_from_slice(b"0000600\0");
    header[108..116].copy_from_slice(b"0000000\0");
    header[116..124].copy_from_slice(b"0000000\0");
    header[124..136].copy_from_slice(b"00000000003\0");
    header[136..148].copy_from_slice(b"00000000000\0");
    header[148..156].fill(b' ');
    header[156] = b'0';
    header[257..263].copy_from_slice(b"ustar\0");
    header[263..265].copy_from_slice(b"00");
    let checksum: u32 = header.iter().map(|byte| u32::from(*byte)).sum();
    header[148..156].copy_from_slice(format!("{checksum:06o}\0 ").as_bytes());
    let mut archive = header.to_vec();
    archive.extend_from_slice(b"abc");
    archive.resize(2048, 0);
    std::fs::write(root.path().join("input.tar"), archive).unwrap();
    let (host, gate, audit) = host(root.path(), Default::default());
    let listed = call(
        &host,
        "archive.list",
        json!({"path":"input.tar","format":"tar"}),
    )
    .await
    .unwrap();
    assert_eq!(listed["entries"][0]["path"], "note.txt");
    let read = call(
        &host,
        "archive.read",
        json!({"path":"input.tar","format":"tar","entry":"note.txt"}),
    )
    .await
    .unwrap();
    assert_eq!(read["data"]["content"], "abc");
    assert_eq!(read["data"]["encoding"], "utf8");
    assert!(call(
        &host,
        "archive.list",
        json!({"path":"../outside.tar","format":"tar"})
    )
    .await
    .is_err());
    assert_eq!(gate.calls.lock().unwrap().len(), 2);
    assert_eq!(audit.records.lock().unwrap().len(), 4);
}

#[tokio::test]
async fn builtin_disable_switches_remove_dispatch_without_creating_a_store() {
    let root = Workspace::new();
    let (host, gate, audit) = host(
        root.path(),
        HostConfig {
            data: None,
            workspace: None,
            ..Default::default()
        },
    );
    for (name, args) in [
        ("data.parse", json!({"format":"json","text":"{}"})),
        ("fs.tree", json!({"path":".","max_entries":8})),
    ] {
        assert_eq!(
            call(&host, name, args).await.unwrap_err().message,
            "capability is not configured"
        );
    }
    assert!(gate.calls.lock().unwrap().is_empty());
    assert!(audit.records.lock().unwrap().is_empty());
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn pure_builtin_failures_settle_without_stopping_the_host() {
    let root = Workspace::new();
    let (host, _, audit) = host(
        root.path(),
        HostConfig {
            max_result_bytes: 16,
            ..Default::default()
        },
    );
    let error = call(
        &host,
        "hash.digest",
        json!({"algorithm":"sha256","input":{"kind":"utf8","value":"abc"}}),
    )
    .await
    .unwrap_err();
    assert!(!error.outcome_unknown);
    assert!(!host.outcome_unknown());
    let result = call(&host, "data.parse", json!({"format":"json","text":"1"}))
        .await
        .unwrap();
    assert_eq!(result, json!({"value":1}));
    assert_eq!(
        *audit.records.lock().unwrap(),
        vec![
            "start:hash.digest",
            "failure",
            "start:data.parse",
            "success"
        ]
    );
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

#[cfg(unix)]
#[tokio::test]
async fn builtin_and_ordinary_filesystem_keep_the_same_root_after_rename() {
    let container = Workspace::new();
    let original = container.path().join("root");
    let moved = container.path().join("moved");
    std::fs::create_dir(&original).unwrap();
    let (host, _, _) = host(&original, Default::default());
    std::fs::rename(&original, &moved).unwrap();
    std::fs::create_dir(&original).unwrap();
    call(
        &host,
        "fs.mkdir",
        json!({"path":"child","expected_absent":true}),
    )
    .await
    .unwrap();
    call(
        &host,
        "fs.write",
        json!({"path":"child/note","content":"same directory"}),
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(moved.join("child/note")).unwrap(),
        "same directory"
    );
    assert_eq!(std::fs::read_dir(&original).unwrap().count(), 0);
}

#[cfg(unix)]
async fn interrupted_batch(root: &Path) -> Value {
    let workspace =
        cayw_workspace::Workspace::open(root.canonicalize().unwrap(), Default::default()).unwrap();
    let prepared = workspace.execute("fs.prepare_change_batch", &json!({"operations":[
        {"op":"write","path":"one.json","expected":{"kind":"absent"},"content":"{\"value\":1}","encoding":"utf8","language":"json"},
        {"op":"write","path":"two.json","expected":{"kind":"absent"},"content":"{\"value\":2}","encoding":"utf8","language":"json"}
    ]}), CancellationToken::new()).await.unwrap();
    workspace.inject_fault(
        cayw_workspace::FaultPoint::AfterInstall(0),
        cayw_workspace::FaultAction::Cancel,
    );
    assert!(workspace.execute("fs.submit_change_batch", &json!({"batch_id":prepared["batch_id"],"expected_batch_digest":prepared["batch_digest"]}), CancellationToken::new()).await.unwrap_err().outcome_unknown);
    assert!(root.join("one.json").is_file());
    assert!(!root.join("two.json").exists());
    prepared
}

#[cfg(unix)]
#[tokio::test]
async fn pending_workspace_batches_fence_ordinary_writes_across_fresh_hosts() {
    let root = Workspace::new();
    let prepared = interrupted_batch(root.path()).await;
    for config in [
        HostConfig::default(),
        HostConfig {
            workspace: None,
            ..Default::default()
        },
    ] {
        let (host, _, audit) = host(root.path(), config);
        for (name, args) in [
            ("fs.write", json!({"path":"never","content":"blocked"})),
            (
                "fs.patch",
                json!({"path":"one.json","expected_sha256":"0000000000000000000000000000000000000000000000000000000000000000","old":"1","new":"9"}),
            ),
        ] {
            let error = call(&host, name, args).await.unwrap_err();
            assert!(error.message.contains("explicit recovery"), "{error}");
            assert!(!root.path().join("never").exists());
        }
        assert!(!host.outcome_unknown());
        let read = call(&host, "fs.read", json!({"path":"one.json"}))
            .await
            .unwrap();
        assert_eq!(read["content"], "{\"value\":1}");
        assert!(audit
            .records
            .lock()
            .unwrap()
            .iter()
            .any(|record| record == "failure"));
    }
    let (host, _, _) = host(root.path(), HostConfig::default());
    for (name, args) in [
        ("fs.batch_status", json!({"batch_id":prepared["batch_id"]})),
        ("fs.tree", json!({"path":".","max_entries":8})),
    ] {
        call(&host, name, args).await.unwrap();
    }
    call(&host, "fs.recover_change_batch", json!({"batch_id":prepared["batch_id"],"expected_batch_digest":prepared["batch_digest"],"action":"rollback"})).await.unwrap();
    call(
        &host,
        "fs.write",
        json!({"path":"after","content":"allowed"}),
    )
    .await
    .unwrap();
    assert!(!root.path().join("one.json").exists());
    assert!(!root.path().join("two.json").exists());
    let (fresh, _, _) = self::host(root.path(), HostConfig::default());
    call(
        &fresh,
        "fs.write",
        json!({"path":"fresh","content":"allowed"}),
    )
    .await
    .unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn pending_batches_block_named_profiles_and_denied_recovery_changes_nothing() {
    let root = Workspace::new();
    let prepared = interrupted_batch(root.path()).await;
    let config = HostConfig {
        profiles: BTreeMap::from([(
            "fixture".into(),
            process_profile(ProfileKind::Build, "write"),
        )]),
        ..Default::default()
    };
    let (profile_host, _, _) = host(root.path(), config);
    assert!(
        call(&profile_host, "build.run", json!({"profile":"fixture"}))
            .await
            .unwrap_err()
            .message
            .contains("explicit recovery")
    );
    assert!(!root.path().join("process-output").exists());
    let gate = Arc::new(Gate {
        allow: false,
        calls: Mutex::new(vec![]),
    });
    let denied = Host::new(
        root.path(),
        gate,
        Arc::new(Audit::default()),
        Default::default(),
    )
    .unwrap();
    assert_eq!(call(&denied,"fs.recover_change_batch",json!({"batch_id":prepared["batch_id"],"expected_batch_digest":prepared["batch_digest"],"action":"commit"})).await.unwrap_err().message,"denied");
    let (fresh, _, _) = host(root.path(), HostConfig::default());
    let status = call(
        &fresh,
        "fs.batch_status",
        json!({"batch_id":prepared["batch_id"]}),
    )
    .await
    .unwrap();
    assert_eq!(status["outcome_unknown"], true);
    assert_eq!(
        std::fs::read_to_string(root.path().join("one.json")).unwrap(),
        "{\"value\":1}"
    );
    assert!(!root.path().join("two.json").exists());
    assert!(call(
        &fresh,
        "fs.write",
        json!({"path":"never","content":"blocked"})
    )
    .await
    .is_err());
    call(&fresh,"fs.recover_change_batch",json!({"batch_id":prepared["batch_id"],"expected_batch_digest":prepared["batch_digest"],"action":"commit"})).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(root.path().join("two.json")).unwrap(),
        "{\"value\":2}"
    );
    call(
        &fresh,
        "fs.write",
        json!({"path":"after","content":"allowed"}),
    )
    .await
    .unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn third_party_content_blocks_recovery_and_never_releases_the_write_fence() {
    let root = Workspace::new();
    let prepared = interrupted_batch(root.path()).await;
    std::fs::write(root.path().join("one.json"), "third party").unwrap();
    let (host, _, audit) = host(root.path(), Default::default());
    assert!(call(&host,"fs.recover_change_batch",json!({"batch_id":prepared["batch_id"],"expected_batch_digest":prepared["batch_digest"],"action":"rollback"})).await.unwrap_err().outcome_unknown);
    assert!(host.outcome_unknown());
    assert_eq!(audit.records.lock().unwrap().last().unwrap(), "unknown");
    assert_eq!(
        std::fs::read_to_string(root.path().join("one.json")).unwrap(),
        "third party"
    );
    let (fresh, _, _) = self::host(root.path(), Default::default());
    let status = call(
        &fresh,
        "fs.batch_status",
        json!({"batch_id":prepared["batch_id"]}),
    )
    .await
    .unwrap();
    assert_eq!(status["outcome_unknown"], true);
    assert_eq!(status["paths"][0]["matches"], "third_party");
    assert!(call(
        &fresh,
        "fs.write",
        json!({"path":"never","content":"blocked"})
    )
    .await
    .is_err());
    assert!(!root.path().join("never").exists());
}

#[tokio::test]
async fn builtin_result_or_audit_loss_preserves_unknown_and_blocks_recovery_bypass() {
    for fail_settle in [false, true] {
        let root = Workspace::new();
        let audit = Arc::new(Audit {
            fail_settle,
            ..Default::default()
        });
        let gate = Arc::new(Gate {
            allow: true,
            calls: Mutex::new(vec![]),
        });
        let host = Host::new(
            root.path(),
            gate,
            audit.clone(),
            HostConfig {
                max_result_bytes: if fail_settle { 1024 * 1024 } else { 8 },
                ..Default::default()
            },
        )
        .unwrap();
        let error = call(
            &host,
            "fs.mkdir",
            json!({"path":"committed","expected_absent":true}),
        )
        .await
        .unwrap_err();
        assert!(error.outcome_unknown);
        assert!(host.outcome_unknown());
        assert!(root.path().join("committed").is_dir());
        assert!(
            call(&host, "fs.batch_status", json!({}))
                .await
                .unwrap_err()
                .outcome_unknown
        );
        assert!(call(&host,"fs.recover_change_batch",json!({"batch_id":"b-0000","expected_batch_digest":"0000000000000000000000000000000000000000000000000000000000000000","action":"rollback"})).await.unwrap_err().outcome_unknown);
        assert!(audit.records.lock().unwrap().contains(&if fail_settle {
            "dropped".into()
        } else {
            "unknown".into()
        }));
    }
}

#[cfg(unix)]
#[tokio::test]
async fn check_lint_and_format_profiles_use_fixed_commands_and_reject_agent_argv() {
    for (kind, name) in [
        (ProfileKind::Check, "check.run"),
        (ProfileKind::Lint, "lint.run"),
        (ProfileKind::Format, "format.run"),
    ] {
        let root = Workspace::new();
        let (host, gate, _) = host(
            root.path(),
            HostConfig {
                profiles: BTreeMap::from([("fixture".into(), process_profile(kind, "write"))]),
                ..Default::default()
            },
        );
        for args in [
            json!({"profile":"fixture","args":["injected"]}),
            json!({"profile":"fixture","filter":"anything"}),
            json!({"profile":"fixture","env":{"HOME":"injected"}}),
        ] {
            assert!(call(&host, name, args).await.is_err());
        }
        assert!(gate.calls.lock().unwrap().is_empty());
        let result = call(&host, name, json!({"profile":"fixture"}))
            .await
            .unwrap();
        assert_eq!(result["success"], true);
        assert_eq!(
            std::fs::read_to_string(root.path().join("process-output")).unwrap(),
            "real-device"
        );
    }
}

fn process_profile(kind: ProfileKind, mode: &str) -> Profile {
    Profile {
        kind,
        program: std::env::current_exe().unwrap(),
        args: vec![
            "--exact".into(),
            "fixture_process".into(),
            "--nocapture".into(),
        ],
        env: BTreeMap::from([("CAYW_HOST_FIXTURE_MODE".into(), mode.into())]),
        timeout: Duration::from_secs(10),
        max_output_bytes: 8192,
        allow_filter: false,
        io: cayw_device::ProcessIo::Pipes,
    }
}
#[test]
fn fixture_process() {
    let Ok(mode) = std::env::var("CAYW_HOST_FIXTURE_MODE") else {
        return;
    };
    match mode.as_str() {
        "write" => {
            std::fs::write("process-output", b"real-device").unwrap();
            println!("fixture-complete");
        }
        "inspect" => {
            let arguments: Vec<_> = std::env::args().collect();
            std::fs::write("process-arguments", serde_json::to_vec(&arguments).unwrap()).unwrap();
            assert!(std::env::var_os("HOME").is_none());
            println!("explicit-environment");
        }
        "fail" => {
            std::fs::write("before-failure", b"effect").unwrap();
            panic!("known fixture failure");
        }
        "wait" => {
            std::fs::write("process-ready", b"ready").unwrap();
            loop {
                std::thread::sleep(Duration::from_secs(1));
            }
        }
        "flood" => {
            for _ in 0..10000 {
                println!("{}", "x".repeat(256));
            }
        }
        _ => panic!("unknown fixture mode"),
    }
}
#[cfg(unix)]
#[tokio::test]
async fn named_build_profile_executes_a_real_device_process_and_rejects_argv() {
    let root = Workspace::new();
    let config = HostConfig {
        profiles: BTreeMap::from([(
            "fixture".into(),
            process_profile(ProfileKind::Build, "write"),
        )]),
        ..Default::default()
    };
    let (host, _, audit) = host(root.path(), config);
    assert!(call(
        &host,
        "build.run",
        json!({"profile":"fixture", "args":["anything"]})
    )
    .await
    .is_err());
    assert!(call(&host, "test.run", json!({"profile":"fixture"}))
        .await
        .is_err());
    let result = call(&host, "build.run", json!({"profile":"fixture"}))
        .await
        .unwrap();
    assert_eq!(result["success"], true);
    assert!(result["stdout"]
        .as_str()
        .unwrap()
        .contains("fixture-complete"));
    assert_eq!(
        std::fs::read_to_string(root.path().join("process-output")).unwrap(),
        "real-device"
    );
    assert_eq!(audit.records.lock().unwrap().last().unwrap(), "success");
}

#[cfg(unix)]
#[tokio::test]
async fn test_filter_is_one_literal_argument_and_nonzero_exit_is_confirmed() {
    let root = Workspace::new();
    let mut profile = process_profile(ProfileKind::Test, "inspect");
    profile.allow_filter = true;
    let (host, gate, _) = host(
        root.path(),
        HostConfig {
            profiles: BTreeMap::from([
                ("fixture".into(), profile),
                ("failure".into(), process_profile(ProfileKind::Test, "fail")),
            ]),
            ..Default::default()
        },
    );
    // A filter is one literal argv item. Shell syntax, spaces, parentheses,
    // semicolons and relative-looking text are ordinary characters now; only a
    // leading dash would let the test program read it as an option.
    let error = call(
        &host,
        "test.run",
        json!({"profile":"fixture","filter":"--ignored"}),
    )
    .await
    .expect_err("a leading dash must not become an option");
    assert!(!error.outcome_unknown);
    assert!(gate.calls.lock().unwrap().is_empty());
    for filter in ["name;touch bad", "name space", "$(touch bad)", "../bad"] {
        let result = call(
            &host,
            "test.run",
            json!({"profile":"fixture","filter":filter}),
        )
        .await
        .unwrap_or_else(|error| panic!("{filter}: {}", error.message));
        assert_eq!(result["success"], true, "{filter}: {result}");
        let args: Vec<String> =
            serde_json::from_slice(&std::fs::read(root.path().join("process-arguments")).unwrap())
                .unwrap();
        assert_eq!(args.last().unwrap(), filter, "filter must stay one argv");
    }
    assert!(
        !root.path().join("bad").exists(),
        "a literal filter must never be interpreted by a shell"
    );
    let result = call(
        &host,
        "test.run",
        json!({"profile":"fixture","filter":"fixture_process"}),
    )
    .await
    .unwrap();
    assert_eq!(result["success"], true);
    let args: Vec<String> =
        serde_json::from_slice(&std::fs::read(root.path().join("process-arguments")).unwrap())
            .unwrap();
    assert_eq!(args.last().unwrap(), "fixture_process");
    let failed = call(&host, "test.run", json!({"profile":"failure"}))
        .await
        .unwrap();
    assert_eq!(failed["success"], false);
    assert_eq!(
        std::fs::read_to_string(root.path().join("before-failure")).unwrap(),
        "effect"
    );
    assert!(!host.outcome_unknown());
    call(
        &host,
        "fs.write",
        json!({"path":"after","content":"allowed"}),
    )
    .await
    .unwrap();
}

#[cfg(unix)]
fn git_object(git: &Path, root: &Path, kind: &str, bytes: &[u8]) -> String {
    use std::{
        io::Write,
        process::{Command, Stdio},
    };
    let mut child = Command::new(git)
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
#[tokio::test]
async fn git_fixed_commands_use_real_device_without_running_repo_helpers() {
    use std::os::unix::fs::PermissionsExt;
    let root = Workspace::new();
    let git = std::env::var_os("CAYW_HOST_TEST_GIT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/usr/bin/git"));
    assert!(
        git.is_file(),
        "set CAYW_HOST_TEST_GIT to an absolute git executable"
    );
    std::fs::create_dir_all(root.path().join(".git/objects")).unwrap();
    std::fs::create_dir_all(root.path().join(".git/refs/heads")).unwrap();
    std::fs::create_dir_all(root.path().join(".git/hooks")).unwrap();
    std::fs::write(root.path().join(".git/HEAD"), "ref: refs/heads/fixture\n").unwrap();
    std::fs::write(
        root.path().join(".git/config"),
        "[core]\n repositoryformatversion = 0\n bare = false\n",
    )
    .unwrap();
    let blob = git_object(&git, root.path(), "blob", b"original\n");
    let mut tree = b"100644 note\0".to_vec();
    tree.extend(
        (0..blob.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&blob[index..index + 2], 16).unwrap()),
    );
    let tree = git_object(&git, root.path(), "tree", &tree);
    let commit = git_object(&git, root.path(), "commit", format!("tree {tree}\nauthor Fixture <fixture@example.invalid> 1 +0000\ncommitter Fixture <fixture@example.invalid> 1 +0000\n\nfixture subject\n").as_bytes());
    std::fs::write(
        root.path().join(".git/refs/heads/fixture"),
        format!("{commit}\n"),
    )
    .unwrap();
    // Test data only: no repository commit or network operation is performed.
    let index = std::process::Command::new(&git)
        .args(["read-tree", &commit])
        .current_dir(root.path())
        .env_clear()
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(index.status.success());
    std::fs::write(root.path().join("note"), "changed\n").unwrap();
    let helper = root.path().join("helper");
    std::fs::write(&helper, "#!/bin/sh\nprintf unsafe > helper-ran\n").unwrap();
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(
        root.path().join(".gitattributes"),
        "note diff=danger filter=danger\n",
    )
    .unwrap();
    std::fs::write(root.path().join(".git/config"), format!("[core]\n repositoryformatversion = 0\n bare = false\n fsmonitor = {}\n pager = {}\n hooksPath = {}\n[diff]\n external = {}\n[diff \"danger\"]\n textconv = {}\n[log]\n showSignature = true\n[gpg]\n program = {}\n[filter \"danger\"]\n clean = {}\n required = true\n", helper.display(), helper.display(), helper.display(), helper.display(), helper.display(), helper.display(), helper.display())).unwrap();
    let (host, gate, audit) = host(
        root.path(),
        HostConfig {
            git_program: Some(git),
            ..Default::default()
        },
    );
    for (name, args, needle) in [
        ("git.status", json!({}), "note"),
        ("git.diff", json!({"path":"note"}), "+changed"),
        ("git.log", json!({"limit":1}), "fixture subject"),
        (
            "git.show",
            json!({"commit":commit,"path":"note"}),
            "original",
        ),
    ] {
        let result = call(&host, name, args).await.unwrap();
        assert_eq!(result["success"], true, "{name}: {result}");
        assert!(
            result["stdout"].as_str().unwrap().contains(needle),
            "{name}: {result}"
        );
    }
    assert!(!root.path().join("helper-ran").exists());
    assert_eq!(gate.calls.lock().unwrap().len(), 4);
    assert_eq!(audit.records.lock().unwrap().len(), 8);
    assert!(call(&host, "git.show", json!({"commit":"--output=escape"}))
        .await
        .is_err());
    assert!(call(&host, "git.diff", json!({"args":["--ext-diff"]}))
        .await
        .is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn real_device_cancellation_waits_for_cleanup_and_records_unknown() {
    let root = Workspace::new();
    let (host, _, audit) = host(
        root.path(),
        HostConfig {
            profiles: BTreeMap::from([(
                "fixture".into(),
                process_profile(ProfileKind::Test, "wait"),
            )]),
            ..Default::default()
        },
    );
    let token = CancellationToken::new();
    let cloned = host.clone();
    let child = token.clone();
    let task = tokio::spawn(async move {
        cloned
            .call("test.run", json!({"profile":"fixture"}), child)
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if root.path().join("process-ready").exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    token.cancel();
    let error = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(error.outcome_unknown);
    assert_eq!(audit.records.lock().unwrap().last().unwrap(), "unknown");
}

#[cfg(unix)]
#[tokio::test]
async fn real_device_timeout_and_output_overflow_remain_unknown() {
    for mode in ["wait", "flood"] {
        let root = Workspace::new();
        let mut profile = process_profile(ProfileKind::Test, mode);
        profile.timeout = Duration::from_millis(250);
        profile.max_output_bytes = 2048;
        let (host, _, audit) = host(
            root.path(),
            HostConfig {
                profiles: BTreeMap::from([("fixture".into(), profile)]),
                ..Default::default()
            },
        );
        let error = call(&host, "test.run", json!({"profile":"fixture"}))
            .await
            .unwrap_err();
        assert!(error.outcome_unknown, "{mode}: {}", error.message);
        assert_eq!(audit.records.lock().unwrap().last().unwrap(), "unknown");
    }
}

#[tokio::test]
async fn vision_tools_are_in_catalog_and_crop_works() {
    use base64::Engine;
    let root = Workspace::new();
    let (host, _, _) = host(
        root.path(),
        HostConfig {
            vision: Some(cayw_vision::Limits::default()),
            ..HostConfig::default()
        },
    );
    let names: Vec<_> = host.capabilities().into_iter().map(|d| d.name).collect();
    for expected in [
        "vision.crop",
        "vision.pixel_diff",
        "vision.colors",
        "vision.ocr",
        "vision.describe",
    ] {
        assert!(
            names.iter().any(|n| n == expected),
            "missing {expected} in {names:?}"
        );
    }
    let rgba = {
        let mut buf = vec![0u8; 8 * 8 * 4];
        for i in 0..16 {
            let idx = i * 4;
            buf[idx..idx + 4].copy_from_slice(&[255, 255, 255, 255]);
        }
        buf
    };
    let result = call(
        &host,
        "vision.crop",
        json!({
            "image": {
                "width": 8,
                "height": 8,
                "rgba_base64": base64::engine::general_purpose::STANDARD.encode(&rgba),
            },
            "x": 0,
            "y": 0,
            "width": 4,
            "height": 4
        }),
    )
    .await
    .expect("vision.crop");
    assert_eq!(result["width"], 4);
    assert_eq!(result["height"], 4);
}

#[tokio::test]
async fn default_host_omits_vision_catalog() {
    let root = Workspace::new();
    let (host, _, _) = host(root.path(), HostConfig::default());
    assert!(
        !host
            .capabilities()
            .iter()
            .any(|d| d.name.starts_with("vision.")),
        "ordinary HostConfig::default() must not advertise vision.*"
    );
}
