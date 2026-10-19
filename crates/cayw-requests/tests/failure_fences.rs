use cayw_ai::CancellationToken;
use cayw_codemode::{CapabilityError, CapabilityHost};
use cayw_host::{AuditSink, HostConfig, PermissionGate};
use cayw_requests::*;
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};

struct TestClock(AtomicU64);
impl Clock for TestClock {
    fn now_ms(&self) -> Result<u64> {
        Ok(self.0.load(Ordering::SeqCst))
    }
}
struct Allow;
#[async_trait::async_trait]
impl PermissionGate for Allow {
    async fn authorize(
        &self,
        _: &str,
        _: &Value,
        _: CancellationToken,
    ) -> std::result::Result<(), CapabilityError> {
        Ok(())
    }
}
#[derive(Default)]
struct Audit(AtomicUsize);
#[async_trait::async_trait]
impl AuditSink for Audit {
    async fn started(&self, _: &str, _: &Value) -> std::result::Result<String, CapabilityError> {
        Ok(self.0.fetch_add(1, Ordering::SeqCst).to_string())
    }
    async fn settled(
        &self,
        _: &str,
        _: &std::result::Result<Value, CapabilityError>,
    ) -> std::result::Result<(), CapabilityError> {
        Ok(())
    }
    fn dropped(&self, _: &str) -> std::result::Result<(), CapabilityError> {
        Err(CapabilityError::unknown("interrupted fixture"))
    }
}
fn identity() -> TaskIdentity {
    TaskIdentity::new("failure-session", "failure-task", "failure-attempt").unwrap()
}
fn actor() -> HostActor {
    HostActor::new("trusted-host").unwrap()
}
fn arguments() -> Value {
    json!({"path":"marker.txt","content":"actual write\n"})
}
fn host_config(root: &std::path::Path, audit: Arc<dyn AuditSink>) -> RequestHostConfig {
    RequestHostConfig {
        root: root.into(),
        host: HostConfig::default(),
        permission_policy_id: "failure-policy".into(),
        operations: vec![OperationScope::exact(
            "write",
            "fs.write",
            arguments(),
            "failure-policy",
        )],
        gate: Arc::new(Allow),
        audit,
    }
}
fn grant(store: &RequestStore, catalog: &OperationCatalog) -> AuthorizationReceipt {
    let input = RequestInput {
        purpose: "write this selected project result".into(),
        gap: "this task needs the selected result file".into(),
        operation_id: "write".into(),
        capability: "fs.write".into(),
        arguments: arguments(),
    };
    store.request(&identity(), catalog, input.clone()).unwrap();
    let record = store.request(&identity(), catalog, input).unwrap();
    let ticket = store.evaluate(&identity(), &record.id, &actor()).unwrap();
    store
        .approve(&ticket, catalog, 100, "verified exact fixture write")
        .unwrap();
    store
        .admit(&identity(), catalog, "fs.write", &arguments())
        .unwrap()
}

// Missing the durable authority stop after any one of these failures restores
// the old 1000 clock, making the receipt usable again after rollback to 900.
#[tokio::test]
async fn failed_clock_update_rollback_and_commit_stop_authority_across_connections_and_reopen() {
    for fault in [
        "CREATE TRIGGER fail_clock BEFORE UPDATE OF last_now_ms ON request_meta BEGIN SELECT RAISE(ABORT,'clock update aborted'); END;",
        "CREATE TRIGGER fail_clock BEFORE UPDATE OF last_now_ms ON request_meta BEGIN SELECT RAISE(ROLLBACK,'clock transaction rolled back'); END;",
        "CREATE TABLE commit_failure (id INTEGER REFERENCES request_meta(singleton) DEFERRABLE INITIALLY DEFERRED); CREATE TRIGGER fail_clock AFTER UPDATE OF last_now_ms ON request_meta BEGIN INSERT INTO commit_failure VALUES(2); END;",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let clock = Arc::new(TestClock(AtomicU64::new(1000)));
        let path = temp.path().join("requests.sqlite");
        let config = RequestConfig::default();
        let store =
            Arc::new(RequestStore::open_with_clock(&path, config.clone(), clock.clone()).unwrap());
        let second =
            Arc::new(RequestStore::open_with_clock(&path, config.clone(), clock.clone()).unwrap());
        let audit = Arc::new(Audit::default());
        let host_config = host_config(temp.path(), audit.clone());
        let catalog = host_config.catalog().unwrap();
        store
            .start_task(&identity(), TaskIntent::General, &catalog, &actor())
            .unwrap();
        let receipt = grant(&store, &catalog);
        let mut fence_path = path.as_os_str().to_os_string();
        fence_path.push(".authority");
        let fence_path = std::path::PathBuf::from(fence_path);
        let ready_record = std::fs::read(&fence_path).unwrap();
        let host = RequestHost::new(second.clone(), identity(), host_config).unwrap();
        let database = rusqlite::Connection::open(&path).unwrap();
        database.execute_batch(fault).unwrap();
        clock.0.store(1100, Ordering::SeqCst);
        assert!(store.check_receipt(&receipt, &catalog).is_err());
        database.execute_batch("DROP TRIGGER fail_clock").unwrap();
        clock.0.store(900, Ordering::SeqCst);
        assert!(
            store.check_receipt(&receipt, &catalog).is_err(),
            "same connection revived: {fault}"
        );
        assert!(
            second.check_receipt(&receipt, &catalog).is_err(),
            "second connection revived: {fault}"
        );
        assert!(
            host.call("fs.write", arguments(), CancellationToken::new())
                .await
                .is_err()
        );
        assert!(!temp.path().join("marker.txt").exists());
        assert_eq!(audit.0.load(Ordering::SeqCst), 0);
        assert!(!host.outcome_unknown());
        drop(host);
        drop(second);
        drop(store);
        assert!(
            RequestStore::open_with_clock(&path, config, clock.clone()).is_err(),
            "reopen silently recovered stopped authority"
        );
        assert_fresh_process_stopped(&path);
        // Even a stale Ready sidecar cannot override SQLite's permanent stop.
        std::fs::write(&fence_path, ready_record).unwrap();
        assert_fresh_process_stopped(&path);
        std::fs::write(&fence_path, b"torn record").unwrap();
        assert_fresh_process_stopped(&path);
    }
}

fn assert_fresh_process_stopped(path: &std::path::Path) {
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "failed_authority_reopen_child", "--nocapture"])
        .env("CAYW_FAILURE_AUTHORITY_DB", path)
        .output()
        .unwrap();
    assert!(
        child.status.success(),
        "fresh process revived authority: {}{}",
        String::from_utf8_lossy(&child.stdout),
        String::from_utf8_lossy(&child.stderr)
    );
}

#[test]
fn failed_authority_reopen_child() {
    let Some(path) = std::env::var_os("CAYW_FAILURE_AUTHORITY_DB") else {
        return;
    };
    assert!(
        RequestStore::open_with_clock(
            std::path::PathBuf::from(path),
            RequestConfig::default(),
            Arc::new(TestClock(AtomicU64::new(900)))
        )
        .is_err(),
        "fresh process accepted failed authority"
    );
}

struct CrashClock;
impl Clock for CrashClock {
    fn now_ms(&self) -> Result<u64> {
        std::process::exit(73)
    }
}
#[test]
fn interrupted_clock_observation_child() {
    let Some(path) = std::env::var_os("CAYW_INTERRUPTED_AUTHORITY_DB") else {
        return;
    };
    let store = RequestStore::open_with_clock(
        std::path::PathBuf::from(path),
        RequestConfig::default(),
        Arc::new(CrashClock),
    )
    .unwrap();
    let _ = store.task_state(&identity());
    panic!("clock did not terminate the fixture");
}

#[test]
fn process_loss_during_clock_observation_leaves_a_durable_admission_stop() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("requests.sqlite");
    let store = RequestStore::open_with_clock(
        &path,
        RequestConfig::default(),
        Arc::new(TestClock(AtomicU64::new(1000))),
    )
    .unwrap();
    let config = host_config(temp.path(), Arc::new(Audit::default()));
    let catalog = config.catalog().unwrap();
    store
        .start_task(&identity(), TaskIntent::General, &catalog, &actor())
        .unwrap();
    let receipt = grant(&store, &catalog);
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "interrupted_clock_observation_child",
            "--nocapture",
        ])
        .env("CAYW_INTERRUPTED_AUTHORITY_DB", &path)
        .output()
        .unwrap();
    assert_eq!(child.status.code(), Some(73));
    assert_fresh_process_stopped(&path);
    assert!(store.check_receipt(&receipt, &catalog).is_err());
}

#[tokio::test]
async fn replacement_workspace_cannot_inherit_grants_but_open_original_host_keeps_its_directory() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    let store = Arc::new(
        RequestStore::open_with_clock(
            temp.path().join("requests.sqlite"),
            RequestConfig::default(),
            Arc::new(TestClock(AtomicU64::new(1000))),
        )
        .unwrap(),
    );
    let config = host_config(&root, Arc::new(Audit::default()));
    let catalog = config.catalog().unwrap();
    store
        .start_task(&identity(), TaskIntent::General, &catalog, &actor())
        .unwrap();
    grant(&store, &catalog);
    let host = RequestHost::new(store.clone(), identity(), config).unwrap();
    assert_eq!(host.active_processes(), 0);
    host.close_processes();
    host.drain_processes().await.unwrap();
    let original = temp.path().join("original-workspace");
    std::fs::rename(&root, &original).unwrap();
    std::fs::create_dir(&root).unwrap();
    let replacement = host_config(&root, Arc::new(Audit::default()));
    assert_ne!(
        catalog.fingerprint(),
        replacement.catalog().unwrap().fingerprint(),
        "path replacement inherited old directory identity"
    );
    assert!(RequestHost::new(store, identity(), replacement).is_err());
    host.call("fs.write", arguments(), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(original.join("marker.txt")).unwrap(),
        "actual write\n"
    );
    assert!(!root.join("marker.txt").exists());
}
