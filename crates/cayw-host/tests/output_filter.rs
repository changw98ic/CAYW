//! F02: Host process results present filtered summary + store handle.
#![cfg(unix)]

use async_trait::async_trait;
use cayw_ai::CancellationToken;
use cayw_codemode::{CapabilityError, CapabilityHost, CapabilityStop};
use cayw_host::{
    AuditSink, FilterOptions, FilterProfile, FilteredOriginalStore, Host, HostConfig,
    PermissionGate, Profile, ProfileKind,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Root(PathBuf);
impl Root {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "cayw-output-filter-host-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Allow;
#[async_trait]
impl PermissionGate for Allow {
    async fn authorize(
        &self,
        _: &str,
        _: &Value,
        _: CancellationToken,
    ) -> Result<(), CapabilityError> {
        Ok(())
    }
}

#[derive(Default)]
struct Audit(Mutex<Vec<Value>>);
#[async_trait]
impl AuditSink for Audit {
    async fn started(&self, name: &str, _: &Value) -> Result<String, CapabilityError> {
        let mut records = self.0.lock().unwrap();
        let id = records.len().to_string();
        records.push(json!({"id":id,"phase":"running","name":name}));
        Ok(id)
    }
    async fn settled(
        &self,
        id: &str,
        result: &Result<Value, CapabilityError>,
    ) -> Result<(), CapabilityError> {
        self.0.lock().unwrap().push(match result {
            Ok(value) => json!({"id":id,"phase":"succeeded","value":value}),
            Err(error) => json!({"id":id,"phase":if error.outcome_unknown {"unknown"} else {"failed"},"message":error.message}),
        });
        Ok(())
    }
    fn dropped(&self, id: &str) -> Result<(), CapabilityError> {
        self.0
            .lock()
            .unwrap()
            .push(json!({"id":id,"phase":"unknown","dropped":true}));
        Ok(())
    }
}

#[derive(Default)]
struct MemoryStore(Mutex<BTreeMap<String, Vec<u8>>>);
impl FilteredOriginalStore for MemoryStore {
    fn put_original(&self, digest: &str, bytes: &[u8]) -> Result<Value, CapabilityError> {
        let mut map = self.0.lock().unwrap();
        if let Some(existing) = map.get(digest) {
            if existing.as_slice() != bytes {
                return Err(CapabilityError::new(
                    "memory store digest conflict with different bytes",
                ));
            }
        } else {
            map.insert(digest.to_string(), bytes.to_vec());
        }
        Ok(json!({
            "handle": digest,
            "scope": "session",
            "session_id": "test-session",
            "task_id": None::<String>,
            "content_hash": digest,
            "bytes": bytes.len() as u64,
            "encoding": "utf8",
            "released": false,
            "stored": true,
        }))
    }
    fn get_original(&self, digest: &str) -> Result<Option<Vec<u8>>, CapabilityError> {
        Ok(self.0.lock().unwrap().get(digest).cloned())
    }
}

fn noisy_script(root: &Path) -> PathBuf {
    let path = root.join("noisy.sh");
    let body = "#!/bin/sh\ncat <<'EOF'\nCompiling demo v0.1.0\n   Compiling unused v0.2.0\n    Finished `test` profile [unoptimized + debuginfo] target(s) in 0.01s\n     Running unittests src/lib.rs\n\nrunning 3 tests\ntest a::ok ... ok\ntest b::ok ... ok\ntest c::ok ... ok\n\ntest result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\nEOF\n";
    fs::write(&path, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms).unwrap();
    }
    path
}

fn host(root: &Path, config: HostConfig) -> Arc<Host> {
    Arc::new(Host::new(root, Arc::new(Allow), Arc::new(Audit::default()), config).unwrap())
}

fn noisy_profile(script: PathBuf) -> Profile {
    Profile {
        kind: ProfileKind::Test,
        program: script,
        args: vec![],
        env: BTreeMap::new(),
        timeout: Duration::from_secs(10),
        max_output_bytes: 64 * 1024,
        allow_filter: false,
        io: cayw_device::ProcessIo::Pipes,
    }
}

#[tokio::test]
async fn process_profile_result_stores_original_and_returns_handle() {
    let root = Root::new();
    let script = noisy_script(&root.0);
    let store = Arc::new(MemoryStore::default());
    let host = host(
        &root.0,
        HostConfig {
            profiles: BTreeMap::from([("noisy".into(), noisy_profile(script))]),
            output_filter: Some(FilterOptions {
                enabled: true,
                profile: FilterProfile::CargoLike,
                head_lines: 2,
                tail_lines: 2,
                max_lines: 12,
            }),
            content_store: Some(store.clone()),
            ..Default::default()
        },
    );
    let result = host
        .call(
            "test.run",
            json!({"profile": "noisy"}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    host.drain_processes().await.unwrap();

    let stdout = result["stdout"].as_str().unwrap();
    assert!(result.get("stdout_original").is_none());
    assert!(result.get("stderr_original").is_none());
    assert!(
        stdout.contains("test result: ok.") || stdout.contains("Finished "),
        "summary must keep cargo-like signal lines: {stdout}"
    );
    assert!(
        !stdout.contains("test a::ok ... ok"),
        "summary should drop per-test ok noise: {stdout}"
    );
    assert_eq!(result["output_filter"]["protocol_version"], 1);
    assert_eq!(
        result["output_filter"]["stdout"]["applied_profile"],
        "cargo_like"
    );
    assert_eq!(result["output_filter"]["stdout"]["original_inline"], false);
    assert_eq!(
        result["output_filter"]["stdout"]["reduction"]["changed"],
        true
    );
    let original_bytes = result["output_filter"]["stdout"]["reduction"]["original_bytes"]
        .as_u64()
        .unwrap();
    let filtered_bytes = result["output_filter"]["stdout"]["reduction"]["filtered_bytes"]
        .as_u64()
        .unwrap();
    assert!(filtered_bytes < original_bytes);
    assert_eq!(filtered_bytes as usize, stdout.len());
    let digest = result["output_filter"]["stdout"]["original_digest"]
        .as_str()
        .unwrap();
    assert_eq!(digest.len(), 64);
    let handle = &result["output_filter"]["stdout"]["original_handle"];
    assert_eq!(handle["handle"], digest);
    assert_eq!(handle["content_hash"], digest);
    assert_eq!(handle["bytes"], original_bytes);
    assert_eq!(handle["encoding"], "utf8");
    assert_eq!(handle["scope"], "session");
    assert_eq!(handle["released"], false);
    assert_eq!(handle["stored"], true);

    let restored = host.read_output_original(digest).unwrap().unwrap();
    assert!(
        restored.contains("test a::ok ... ok"),
        "stored original must retain dropped noise: {restored}"
    );
    assert_eq!(restored.len() as u64, original_bytes);
    let encoded = serde_json::to_string(&result).unwrap();
    assert!(
        !encoded.contains("test a::ok ... ok"),
        "agent payload must stay small and not embed the full original"
    );
    // Meta + summary stay far below a dual-entry payload that re-embeds original.
    let mut with_original = result.clone();
    with_original["stdout_original"] = json!(restored);
    let dual = serde_json::to_string(&with_original).unwrap();
    assert!(
        dual.len() > encoded.len() + restored.len() / 2,
        "inlining original is what inflates the agent payload ({} vs {})",
        encoded.len(),
        dual.len()
    );
}

#[tokio::test]
async fn filtered_reduction_without_content_store_fails_closed() {
    let root = Root::new();
    let script = noisy_script(&root.0);
    let host = host(
        &root.0,
        HostConfig {
            profiles: BTreeMap::from([("noisy".into(), noisy_profile(script))]),
            output_filter: Some(FilterOptions {
                enabled: true,
                profile: FilterProfile::CargoLike,
                head_lines: 2,
                tail_lines: 2,
                max_lines: 12,
            }),
            content_store: None,
            ..Default::default()
        },
    );
    let error = host
        .call(
            "test.run",
            json!({"profile": "noisy"}),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    host.drain_processes().await.unwrap();
    assert!(
        error.message.contains("ContentStore is not configured"),
        "must fail closed instead of discarding original: {}",
        error.message
    );
    assert!(!error.outcome_unknown);
}

#[tokio::test]
async fn original_storage_failure_keeps_unknown_and_stop_state() {
    struct UncertainStore;
    impl FilteredOriginalStore for UncertainStore {
        fn put_original(&self, _: &str, _: &[u8]) -> Result<Value, CapabilityError> {
            Err(CapabilityError {
                message: "original commit outcome is unknown".into(),
                outcome_unknown: true,
                stop: Some(CapabilityStop::Budget),
            })
        }
        fn get_original(&self, _: &str) -> Result<Option<Vec<u8>>, CapabilityError> {
            Ok(None)
        }
    }

    let root = Root::new();
    let script = noisy_script(&root.0);
    let audit = Arc::new(Audit::default());
    let host = Host::new(
        &root.0,
        Arc::new(Allow),
        audit.clone(),
        HostConfig {
            profiles: BTreeMap::from([("noisy".into(), noisy_profile(script))]),
            output_filter: Some(FilterOptions {
                enabled: true,
                profile: FilterProfile::CargoLike,
                head_lines: 2,
                tail_lines: 2,
                max_lines: 12,
            }),
            content_store: Some(Arc::new(UncertainStore)),
            ..Default::default()
        },
    )
    .unwrap();
    let error = host
        .call(
            "test.run",
            json!({"profile": "noisy"}),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    host.drain_processes().await.unwrap();

    assert!(error.outcome_unknown);
    assert_eq!(error.stop, Some(CapabilityStop::Budget));
    assert!(error.message.contains("failed to externalize original"));
    assert!(error.message.contains("original commit outcome is unknown"));
    assert_eq!(audit.0.lock().unwrap().last().unwrap()["phase"], "unknown");
}

#[tokio::test]
async fn output_filter_none_keeps_stdout_only_shape() {
    let root = Root::new();
    let profile = Profile {
        kind: ProfileKind::Check,
        program: PathBuf::from("/bin/echo"),
        args: vec!["legacy-shape".into()],
        env: BTreeMap::new(),
        timeout: Duration::from_secs(10),
        max_output_bytes: 4096,
        allow_filter: false,
        io: cayw_device::ProcessIo::Pipes,
    };
    let host = host(
        &root.0,
        HostConfig {
            profiles: BTreeMap::from([("echo".into(), profile)]),
            output_filter: None,
            ..Default::default()
        },
    );
    let result = host
        .call(
            "check.run",
            json!({"profile": "echo"}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    host.drain_processes().await.unwrap();
    assert_eq!(result["stdout"], "legacy-shape\n");
    assert!(result.get("stdout_original").is_none());
    assert!(result.get("output_filter").is_none());
}

#[tokio::test]
async fn unchanged_short_capture_needs_no_store_handle() {
    // When filtering does not drop bytes, Host must not require ContentStore.
    // (git.blame/log/show force Passthrough in process::output_filter_options;
    // short cargo_like captures similarly leave reduction.changed=false.)
    let root = Root::new();
    let profile = Profile {
        kind: ProfileKind::Check,
        program: PathBuf::from("/bin/echo"),
        args: vec!["short-ok".into()],
        env: BTreeMap::new(),
        timeout: Duration::from_secs(10),
        max_output_bytes: 4096,
        allow_filter: false,
        io: cayw_device::ProcessIo::Pipes,
    };
    let host = host(
        &root.0,
        HostConfig {
            profiles: BTreeMap::from([("echo".into(), profile)]),
            output_filter: Some(FilterOptions {
                enabled: true,
                profile: FilterProfile::CargoLike,
                head_lines: 8,
                tail_lines: 8,
                max_lines: 200,
            }),
            content_store: None,
            ..Default::default()
        },
    );
    let result = host
        .call(
            "check.run",
            json!({"profile": "echo"}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    host.drain_processes().await.unwrap();
    assert_eq!(result["stdout"], "short-ok\n");
    assert_eq!(
        result["output_filter"]["stdout"]["reduction"]["changed"],
        false
    );
    assert!(
        result["output_filter"]["stdout"]
            .get("original_handle")
            .is_none()
    );
}
