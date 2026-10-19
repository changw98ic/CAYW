use async_trait::async_trait;
use cayw_ai::{CancellationToken, ToolDefinition};
use cayw_codemode::{CapabilityError, CapabilityHost};
use cayw_host::{AuditSink, ExternalHost, Host, HostConfig, PermissionGate, Profile, ProfileKind};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Root(PathBuf);
impl Root {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "cayw-external-scope-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&p).unwrap();
        Self(p)
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
#[derive(Default)]
struct Gate {
    calls: AtomicUsize,
    deny: bool,
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
        if self.deny {
            Err(CapabilityError::new("denied"))
        } else {
            Ok(())
        }
    }
}
#[derive(Default)]
struct Audit(Mutex<Vec<&'static str>>);
#[async_trait]
impl AuditSink for Audit {
    async fn started(&self, _: &str, _: &Value) -> Result<String, CapabilityError> {
        self.0.lock().unwrap().push("running");
        Ok("call".into())
    }
    async fn settled(
        &self,
        _: &str,
        r: &Result<Value, CapabilityError>,
    ) -> Result<(), CapabilityError> {
        self.0
            .lock()
            .unwrap()
            .push(if r.as_ref().is_err_and(|e| e.outcome_unknown) {
                "unknown"
            } else if r.is_err() {
                "failed"
            } else {
                "succeeded"
            });
        Ok(())
    }
    fn dropped(&self, _: &str) -> Result<(), CapabilityError> {
        self.0.lock().unwrap().push("dropped");
        Ok(())
    }
}
struct Service(&'static str);
#[async_trait]
impl ExternalHost for Service {
    async fn call(
        &self,
        name: &str,
        args: Value,
        _: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        Ok(json!({"service":self.0,"name":name,"args":args}))
    }
}
async fn call(h: &Host, name: &str, args: Value) -> Result<Value, CapabilityError> {
    h.call(name, args, CancellationToken::new()).await
}
fn parent(root: &Root, config: HostConfig) -> Host {
    Host::new(
        &root.0,
        Arc::new(Gate::default()),
        Arc::new(Audit::default()),
        config,
    )
    .unwrap()
}

#[test]
fn workspace_identity_comes_from_the_pinned_directory_across_independent_opens_and_scopes() {
    let container = Root::new();
    let root = container.0.join("root");
    let moved = container.0.join("moved");
    fs::create_dir(&root).unwrap();
    let config = HostConfig {
        external: Some(Arc::new(Service("parent"))),
        ..Default::default()
    };
    let open = |path: &std::path::Path| {
        Host::new(
            path,
            Arc::new(Gate::default()),
            Arc::new(Audit::default()),
            config.clone(),
        )
        .unwrap()
    };
    let original = open(&root);
    let identity = original.workspace_identity().unwrap();
    assert_eq!(identity, open(&root).workspace_identity().unwrap());
    fs::rename(&root, &moved).unwrap();
    fs::create_dir(&root).unwrap();
    assert_eq!(identity, original.workspace_identity().unwrap());
    assert_eq!(identity, open(&moved).workspace_identity().unwrap());
    assert_ne!(identity, open(&root).workspace_identity().unwrap());
    let scope = original
        .scoped(Arc::new(Gate::default()), Arc::new(Audit::default()))
        .unwrap();
    let external = original
        .scoped_external(
            Arc::new(Gate::default()),
            Arc::new(Audit::default()),
            Arc::new(Service("scope")),
        )
        .unwrap();
    assert_eq!(identity, scope.workspace_identity().unwrap());
    assert_eq!(identity, external.workspace_identity().unwrap());
}

#[tokio::test]
async fn external_scope_only_replaces_its_service_gate_and_audit_and_shares_call_budget() {
    let root = Root::new();
    let base = parent(
        &root,
        HostConfig {
            external: Some(Arc::new(Service("parent"))),
            max_calls: 4,
            ..Default::default()
        },
    );
    let gate = Arc::new(Gate::default());
    let audit = Arc::new(Audit::default());
    let one = base
        .scoped_external(gate.clone(), audit.clone(), Arc::new(Service("one")))
        .unwrap();
    let two = base
        .scoped_external(
            Arc::new(Gate::default()),
            Arc::new(Audit::default()),
            Arc::new(Service("two")),
        )
        .unwrap();
    assert_eq!(
        serde_json::to_value(base.capabilities()).unwrap(),
        serde_json::to_value(one.capabilities()).unwrap()
    );
    assert_eq!(
        call(&base, "mcp.discover", json!({})).await.unwrap()["service"],
        "parent"
    );
    assert_eq!(
        call(&one, "mcp.discover", json!({})).await.unwrap()["service"],
        "one"
    );
    assert_eq!(
        call(&two, "mcp.discover", json!({})).await.unwrap()["service"],
        "two"
    );
    let nested = one.scoped(gate.clone(), audit.clone()).unwrap();
    assert_eq!(
        call(&nested, "skills.list", json!({})).await.unwrap()["service"],
        "one"
    );
    assert!(call(&base, "skills.list", json!({})).await.is_err());
    assert_eq!(gate.calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        *audit.0.lock().unwrap(),
        vec!["running", "succeeded", "running", "succeeded"]
    );
}

#[tokio::test]
async fn denied_external_scope_does_not_use_parent_permission_or_service() {
    let root = Root::new();
    let base = parent(
        &root,
        HostConfig {
            external: Some(Arc::new(Service("parent"))),
            ..Default::default()
        },
    );
    let gate = Arc::new(Gate {
        deny: true,
        ..Default::default()
    });
    let audit = Arc::new(Audit::default());
    let denied = base
        .scoped_external(gate.clone(), audit.clone(), Arc::new(Service("denied")))
        .unwrap();
    assert_eq!(
        call(&denied, "mcp.discover", json!({}))
            .await
            .unwrap_err()
            .message,
        "denied"
    );
    assert_eq!(gate.calls.load(Ordering::SeqCst), 1);
    assert!(audit.0.lock().unwrap().is_empty());
    assert_eq!(
        call(&base, "mcp.discover", json!({})).await.unwrap()["service"],
        "parent"
    );
}

#[test]
fn external_scope_refuses_to_add_an_absent_external_catalog() {
    let root = Root::new();
    let base = parent(&root, HostConfig::default());
    assert!(
        base.scoped_external(
            Arc::new(Gate::default()),
            Arc::new(Audit::default()),
            Arc::new(Service("forbidden"))
        )
        .is_err()
    );
    assert!(!base.outcome_unknown());
}

#[tokio::test]
async fn external_scope_retains_spent_serialized_bytes() {
    let root = Root::new();
    let base = parent(
        &root,
        HostConfig {
            external: Some(Arc::new(Service("parent"))),
            max_total_bytes: 43,
            ..Default::default()
        },
    );
    call(&base, "data.parse", json!({"format":"json","text":"1"}))
        .await
        .unwrap();
    let gate = Arc::new(Gate::default());
    let scope = base
        .scoped_external(
            gate.clone(),
            Arc::new(Audit::default()),
            Arc::new(Service("scope")),
        )
        .unwrap();
    assert!(
        call(&scope, "data.parse", json!({"format":"json","text":"1"}))
            .await
            .is_err()
    );
    assert_eq!(gate.calls.load(Ordering::SeqCst), 0);
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
        Err(CapabilityError::unknown("uncertain service effect"))
    }
}
#[tokio::test]
async fn external_scope_shares_the_execution_lane_and_cannot_reset_unknown() {
    let root = Root::new();
    let base = parent(
        &root,
        HostConfig {
            external: Some(Arc::new(Service("parent"))),
            ..Default::default()
        },
    );
    let service = Arc::new(PausedUnknown {
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let one = Arc::new(
        base.scoped_external(
            Arc::new(Gate::default()),
            Arc::new(Audit::default()),
            service.clone(),
        )
        .unwrap(),
    );
    let gate = Arc::new(Gate::default());
    let two = base
        .scoped_external(
            gate.clone(),
            Arc::new(Audit::default()),
            Arc::new(Service("two")),
        )
        .unwrap();
    let cloned = one.clone();
    let first = tokio::spawn(async move { call(&cloned, "mcp.discover", json!({})).await });
    service.entered.notified().await;
    let mut second = Box::pin(call(
        &two,
        "fs.write",
        json!({"path":"never","content":"bad"}),
    ));
    assert!(futures::poll!(&mut second).is_pending());
    assert_eq!(gate.calls.load(Ordering::SeqCst), 0);
    service.release.notify_one();
    assert!(first.await.unwrap().unwrap_err().outcome_unknown);
    assert!(second.await.unwrap_err().outcome_unknown);
    assert!(!root.0.join("never").exists());
    let later = base
        .scoped_external(
            Arc::new(Gate::default()),
            Arc::new(Audit::default()),
            Arc::new(Service("later")),
        )
        .unwrap();
    assert!(
        call(&later, "skills.list", json!({}))
            .await
            .unwrap_err()
            .outcome_unknown
    );
    assert!(base.outcome_unknown());
}

fn service_definition() -> ToolDefinition {
    ToolDefinition {
        name: "capability.request".into(),
        description: "Trusted application request service".into(),
        parameters: json!({"type":"object","properties":{"id":{"type":"integer"}},"required":["id"],"additionalProperties":false}),
    }
}
struct CountingService(AtomicUsize);
#[async_trait]
impl ExternalHost for CountingService {
    async fn call(
        &self,
        _: &str,
        args: Value,
        _: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(args)
    }
}

#[tokio::test]
async fn injected_service_uses_original_budget_gate_audit_and_stays_out_of_catalog() {
    let root = Root::new();
    let gate = Arc::new(Gate::default());
    let audit = Arc::new(Audit::default());
    let base = Host::new(
        &root.0,
        gate.clone(),
        audit.clone(),
        HostConfig {
            max_calls: 2,
            ..Default::default()
        },
    )
    .unwrap();
    let definition = service_definition();
    let service = CountingService(AtomicUsize::new(0));
    assert_eq!(
        base.call_service(
            &definition,
            json!({"id":1}),
            CancellationToken::new(),
            &service
        )
        .await
        .unwrap(),
        json!({"id":1})
    );
    call(&base, "data.parse", json!({"format":"json","text":"1"}))
        .await
        .unwrap();
    let scope = base.scoped(gate.clone(), audit.clone()).unwrap();
    assert!(
        scope
            .call_service(
                &definition,
                json!({"id":2}),
                CancellationToken::new(),
                &service
            )
            .await
            .is_err()
    );
    assert_eq!(service.0.load(Ordering::SeqCst), 1);
    assert_eq!(gate.calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        *audit.0.lock().unwrap(),
        ["running", "succeeded", "running", "succeeded"]
    );
    assert!(
        !base
            .capabilities()
            .iter()
            .any(|d| d.name == definition.name)
    );
    assert!(
        call(&base, &definition.name, json!({"id":3}))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn injected_service_validates_schema_arguments_name_collision_and_permission_before_effects()
{
    let root = Root::new();
    let gate = Arc::new(Gate {
        deny: true,
        ..Default::default()
    });
    let audit = Arc::new(Audit::default());
    let base = Host::new(&root.0, gate.clone(), audit.clone(), HostConfig::default()).unwrap();
    let service = CountingService(AtomicUsize::new(0));
    let mut definition = service_definition();
    assert!(
        base.call_service(
            &definition,
            json!({"id":"1"}),
            CancellationToken::new(),
            &service
        )
        .await
        .is_err()
    );
    definition.parameters = json!({"type":"not-a-json-schema-type"});
    assert!(
        base.call_service(&definition, json!({}), CancellationToken::new(), &service)
            .await
            .is_err()
    );
    definition = service_definition();
    definition.name = "fs.read".into();
    assert!(
        base.call_service(
            &definition,
            json!({"id":1}),
            CancellationToken::new(),
            &service
        )
        .await
        .is_err()
    );
    definition.name = "invalid/name".into();
    assert!(
        base.call_service(
            &definition,
            json!({"id":1}),
            CancellationToken::new(),
            &service
        )
        .await
        .is_err()
    );
    assert!(
        base.call_service(
            &service_definition(),
            json!({"id":1}),
            CancellationToken::new(),
            &service
        )
        .await
        .is_err()
    );
    assert_eq!(gate.calls.load(Ordering::SeqCst), 1);
    assert_eq!(service.0.load(Ordering::SeqCst), 0);
    assert!(audit.0.lock().unwrap().is_empty());
}

#[tokio::test]
async fn injected_service_shares_the_lane_and_unknown_blocks_base_and_other_scopes() {
    let root = Root::new();
    let base = parent(&root, HostConfig::default());
    let service = PausedUnknown {
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    };
    let definition = service_definition();
    let mut first = Box::pin(base.call_service(
        &definition,
        json!({"id":1}),
        CancellationToken::new(),
        &service,
    ));
    assert!(futures::poll!(&mut first).is_pending());
    service.entered.notified().await;
    let mut second = Box::pin(call(
        &base,
        "fs.write",
        json!({"path":"never","content":"bad"}),
    ));
    assert!(futures::poll!(&mut second).is_pending());
    service.release.notify_one();
    assert!(first.await.unwrap_err().outcome_unknown);
    assert!(second.await.unwrap_err().outcome_unknown);
    assert!(!root.0.join("never").exists());
    let scope = base
        .scoped(Arc::new(Gate::default()), Arc::new(Audit::default()))
        .unwrap();
    assert!(
        scope
            .call_service(
                &definition,
                json!({"id":2}),
                CancellationToken::new(),
                &service
            )
            .await
            .unwrap_err()
            .outcome_unknown
    );
}

struct LargeService;
#[async_trait]
impl ExternalHost for LargeService {
    async fn call(
        &self,
        _: &str,
        _: Value,
        _: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        Ok(json!({"effect":"x".repeat(1024)}))
    }
}
#[tokio::test]
async fn injected_service_uses_bytes_already_spent_by_base_calls() {
    let root = Root::new();
    let gate = Arc::new(Gate::default());
    let base = Host::new(
        &root.0,
        gate.clone(),
        Arc::new(Audit::default()),
        HostConfig {
            max_total_bytes: 43,
            ..Default::default()
        },
    )
    .unwrap();
    call(&base, "data.parse", json!({"format":"json","text":"1"}))
        .await
        .unwrap();
    let service = CountingService(AtomicUsize::new(0));
    assert!(
        base.call_service(
            &service_definition(),
            json!({"id":1}),
            CancellationToken::new(),
            &service
        )
        .await
        .is_err()
    );
    assert_eq!(gate.calls.load(Ordering::SeqCst), 1);
    assert_eq!(service.0.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn injected_service_unretainable_response_is_unknown_and_charges_shared_bytes() {
    let root = Root::new();
    let gate = Arc::new(Gate::default());
    let audit = Arc::new(Audit::default());
    let base = Host::new(
        &root.0,
        gate.clone(),
        audit.clone(),
        HostConfig {
            max_result_bytes: 16,
            max_total_bytes: 32,
            ..Default::default()
        },
    )
    .unwrap();
    let error = base
        .call_service(
            &service_definition(),
            json!({"id":1}),
            CancellationToken::new(),
            &LargeService,
        )
        .await
        .unwrap_err();
    assert!(error.outcome_unknown);
    assert!(base.outcome_unknown());
    assert_eq!(*audit.0.lock().unwrap(), ["running", "unknown"]);
    assert!(
        call(&base, "data.parse", json!({"format":"json","text":"1"}))
            .await
            .is_err()
    );
    assert_eq!(gate.calls.load(Ordering::SeqCst), 1);
}

struct PendingService(Mutex<Option<CancellationToken>>);
#[async_trait]
impl ExternalHost for PendingService {
    async fn call(
        &self,
        _: &str,
        _: Value,
        cancel: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        *self.0.lock().unwrap() = Some(cancel);
        std::future::pending().await
    }
}
#[tokio::test]
async fn injected_service_deadline_uses_original_timeout_and_cancels_child() {
    let root = Root::new();
    let audit = Arc::new(Audit::default());
    let base = Host::new(
        &root.0,
        Arc::new(Gate::default()),
        audit.clone(),
        HostConfig {
            call_timeout: Duration::from_millis(500),
            ..Default::default()
        },
    )
    .unwrap();
    let service = PendingService(Mutex::new(None));
    let error = base
        .call_service(
            &service_definition(),
            json!({"id":1}),
            CancellationToken::new(),
            &service,
        )
        .await
        .unwrap_err();
    assert!(error.outcome_unknown);
    assert!(base.outcome_unknown());
    assert!(service.0.lock().unwrap().as_ref().unwrap().is_cancelled());
    assert_eq!(*audit.0.lock().unwrap(), ["running", "unknown"]);
}

#[tokio::test]
async fn injected_service_drop_and_cancellation_keep_unknown_audit_and_cancel_child() {
    for drop_call in [false, true] {
        let root = Root::new();
        let audit = Arc::new(Audit::default());
        let base = Host::new(
            &root.0,
            Arc::new(Gate::default()),
            audit.clone(),
            HostConfig::default(),
        )
        .unwrap();
        let definition = service_definition();
        let service = PendingService(Mutex::new(None));
        let cancel = CancellationToken::new();
        let mut run =
            Box::pin(base.call_service(&definition, json!({"id":1}), cancel.clone(), &service));
        assert!(futures::poll!(&mut run).is_pending());
        if drop_call {
            drop(run);
            assert_eq!(*audit.0.lock().unwrap(), ["running", "dropped"]);
        } else {
            cancel.cancel();
            assert!(run.await.unwrap_err().outcome_unknown);
            assert_eq!(*audit.0.lock().unwrap(), ["running", "unknown"]);
        }
        assert!(base.outcome_unknown());
        assert!(service.0.lock().unwrap().as_ref().unwrap().is_cancelled());
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parent_drain_joins_the_process_started_by_an_external_scope() {
    let root = Root::new();
    let base = parent(
        &root,
        HostConfig {
            external: Some(Arc::new(Service("parent"))),
            profiles: BTreeMap::from([(
                "wait".into(),
                Profile {
                    kind: ProfileKind::Check,
                    program: env!("CARGO_BIN_EXE_cayw-host-test-process").into(),
                    args: vec!["wait".into()],
                    env: BTreeMap::new(),
                    timeout: Duration::from_secs(20),
                    max_output_bytes: 4096,
                    allow_filter: false,
                },
            )]),
            ..Default::default()
        },
    );
    let scope = Arc::new(
        base.scoped_external(
            Arc::new(Gate::default()),
            Arc::new(Audit::default()),
            Arc::new(Service("scope")),
        )
        .unwrap(),
    );
    let cloned = scope.clone();
    let task =
        tokio::spawn(async move { call(&cloned, "check.run", json!({"profile":"wait"})).await });
    let pid = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Ok(text) = fs::read_to_string(root.0.join("effect.pid"))
                && let Ok(pid) = text.parse::<u32>()
            {
                break pid;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(base.active_processes(), 1);
    base.drain_processes().await.unwrap();
    assert_eq!(scope.active_processes(), 0);
    assert!(task.await.unwrap().unwrap_err().outcome_unknown);
    let observed = std::process::Command::new("/bin/kill")
        .args(["-0", &pid.to_string()])
        .output()
        .unwrap();
    assert!(
        !observed.status.success(),
        "parent drain left scope PID {pid} alive"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn external_scope_keeps_the_pinned_root_and_original_process_runner() {
    let container = Root::new();
    let root = container.0.join("root");
    let moved = container.0.join("moved");
    fs::create_dir(&root).unwrap();
    let base = Host::new(
        &root,
        Arc::new(Gate::default()),
        Arc::new(Audit::default()),
        HostConfig {
            external: Some(Arc::new(Service("parent"))),
            profiles: BTreeMap::from([(
                "probe".into(),
                Profile {
                    kind: ProfileKind::Check,
                    program: env!("CARGO_BIN_EXE_cayw-host-test-process").into(),
                    args: vec!["effect".into()],
                    env: BTreeMap::new(),
                    timeout: Duration::from_secs(10),
                    max_output_bytes: 4096,
                    allow_filter: false,
                },
            )]),
            ..Default::default()
        },
    )
    .unwrap();
    fs::rename(&root, &moved).unwrap();
    fs::create_dir(&root).unwrap();
    let scope = base
        .scoped_external(
            Arc::new(Gate::default()),
            Arc::new(Audit::default()),
            Arc::new(Service("scope")),
        )
        .unwrap();
    assert_eq!(
        call(&scope, "check.run", json!({"profile":"probe"}))
            .await
            .unwrap()["success"],
        true
    );
    assert_eq!(fs::read(moved.join("effect")).unwrap(), b"started");
    assert!(!root.join("effect").exists());
    base.close_processes();
    assert!(
        call(&scope, "check.run", json!({"profile":"probe"}))
            .await
            .is_err()
    );
    scope.drain_processes().await.unwrap();
    let later = base
        .scoped_external(
            Arc::new(Gate::default()),
            Arc::new(Audit::default()),
            Arc::new(Service("later")),
        )
        .unwrap();
    assert!(
        call(&later, "check.run", json!({"profile":"probe"}))
            .await
            .is_err()
    );
    assert_eq!(
        call(&base, "skills.list", json!({})).await.unwrap()["service"],
        "parent"
    );
}
