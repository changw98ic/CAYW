use cayw_ai::CancellationToken;
use cayw_codemode::{CapabilityError, CapabilityHost};
use cayw_host::{AuditSink, HostConfig, PermissionGate};
use cayw_requests::*;
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

struct HighClock {
    armed: AtomicBool,
    ready: Arc<AtomicBool>,
    contender_started: Arc<AtomicBool>,
}
impl Clock for HighClock {
    fn now_ms(&self) -> Result<u64> {
        if !self.armed.load(Ordering::SeqCst) {
            return Ok(1000);
        }
        self.ready.store(true, Ordering::SeqCst);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self.contender_started.load(Ordering::SeqCst) {
            assert!(Instant::now() < deadline, "contender did not start");
            std::thread::yield_now();
        }
        Ok(1500)
    }
}
struct LowClock(Arc<AtomicBool>);
impl Clock for LowClock {
    fn now_ms(&self) -> Result<u64> {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self.0.load(Ordering::SeqCst) {
            assert!(
                Instant::now() < deadline,
                "high-clock operation did not finish"
            );
            std::thread::yield_now();
        }
        Ok(1000)
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
        Err(CapabilityError::unknown("interrupted"))
    }
}

// Dropping the outer write lock on Budget/Database errors lets the low-clock
// connection enter before last_now_ms is persisted, reviving the expired grant.
#[test]
fn rejected_mutation_keeps_clock_highwater_atomic_with_real_host_admission() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    for trial in 0..32 {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("ordinary.txt"), "real content\n").unwrap();
        let config = RequestConfig {
            repeated_valid_threshold: 1,
            max_requests_per_task: 1,
            sqlite_busy_timeout_ms: 0,
            ..RequestConfig::default()
        };
        let ready = Arc::new(AtomicBool::new(false));
        let contender_started = Arc::new(AtomicBool::new(false));
        let high_done = Arc::new(AtomicBool::new(false));
        let clock = Arc::new(HighClock {
            armed: AtomicBool::new(false),
            ready: ready.clone(),
            contender_started: contender_started.clone(),
        });
        let store = Arc::new(
            RequestStore::open_with_clock(
                temp.path().join("requests.sqlite"),
                config.clone(),
                clock.clone(),
            )
            .unwrap(),
        );
        let second = Arc::new(
            RequestStore::open_with_clock(
                store.path(),
                config.clone(),
                Arc::new(LowClock(high_done.clone())),
            )
            .unwrap(),
        );
        let audit = Arc::new(Audit::default());
        let host_config = RequestHostConfig {
            root: temp.path().into(),
            host: HostConfig::default(),
            permission_policy_id: "policy".into(),
            operations: vec![OperationScope::exact(
                "file",
                "fs.read",
                json!({"path":"ordinary.txt"}),
                "policy",
            )],
            gate: Arc::new(Allow),
            audit: audit.clone(),
        };
        let catalog = host_config.catalog().unwrap();
        let identity = TaskIdentity::new("session", "task", "attempt").unwrap();
        let actor = HostActor::new("host").unwrap();
        let input = RequestInput {
            purpose: "read file".into(),
            gap: "need exact file".into(),
            operation_id: "file".into(),
            capability: "fs.read".into(),
            arguments: json!({"path":"ordinary.txt"}),
        };
        store
            .start_task(&identity, TaskIntent::General, &catalog, &actor)
            .unwrap();
        let request = store.request(&identity, &catalog, input.clone()).unwrap();
        let ticket = store.evaluate(&identity, &request.id, &actor).unwrap();
        store.approve(&ticket, &catalog, 100, "verified").unwrap();
        let host = RequestHost::new(store.clone(), identity.clone(), host_config).unwrap();
        assert_eq!(
            runtime
                .block_on(host.call("fs.read", input.arguments.clone(), CancellationToken::new()))
                .unwrap()["content"],
            "real content\n"
        );
        let id_copy = identity.clone();
        let cat_copy = catalog.clone();
        let contender = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !ready.load(Ordering::SeqCst) {
                assert!(Instant::now() < deadline);
                std::thread::yield_now();
            }
            contender_started.store(true, Ordering::SeqCst);
            loop {
                match second.admit(
                    &id_copy,
                    &cat_copy,
                    "fs.read",
                    &json!({"path":"ordinary.txt"}),
                ) {
                    Err(RequestError::Database(_)) if Instant::now() < deadline => {
                        std::thread::yield_now();
                    }
                    result => return result,
                }
            }
        });
        clock.armed.store(true, Ordering::SeqCst);
        let error = store
            .request(&identity, &catalog, input.clone())
            .unwrap_err();
        high_done.store(true, Ordering::SeqCst);
        let observed = contender.join().unwrap();
        assert!(observed.is_err(), "expired grant revived on trial {trial}");
        assert!(matches!(error, RequestError::Budget), "{error:?}");
        clock.armed.store(false, Ordering::SeqCst);
        assert!(
            runtime
                .block_on(host.call("fs.read", input.arguments.clone(), CancellationToken::new()))
                .is_err(),
            "same RequestHost reused expired grant on trial {trial}"
        );
        assert_eq!(audit.0.load(Ordering::SeqCst), 1);
        assert!(!host.outcome_unknown());
        let reopened =
            RequestStore::open_with_clock(store.path(), config, Arc::new(LowClock(high_done)))
                .unwrap();
        assert!(
            reopened
                .admit(&identity, &catalog, "fs.read", &input.arguments)
                .is_err()
        );
        assert_eq!(
            reopened.status(&identity, &request.id).unwrap().state,
            RequestState::Expired
        );
        let database = rusqlite::Connection::open(store.path()).unwrap();
        let highwater: i64 = database
            .query_row("SELECT last_now_ms FROM request_meta", [], |row| row.get(0))
            .unwrap();
        assert_eq!(highwater, 1500);
    }
}
