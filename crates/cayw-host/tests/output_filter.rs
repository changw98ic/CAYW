//! F02: Host process results present filtered summary without inlining original.
#![cfg(unix)]

use async_trait::async_trait;
use cayw_ai::CancellationToken;
use cayw_codemode::{CapabilityError, CapabilityHost};
use cayw_host::{
    AuditSink, FilterOptions, FilterProfile, Host, HostConfig, PermissionGate, Profile, ProfileKind,
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

fn noisy_script(root: &Path) -> PathBuf {
    let path = root.join("noisy.sh");
    // Keep a cargo-like noise stream so CargoLike filtering demonstrably shrinks.
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

#[tokio::test]
async fn process_profile_result_exposes_filtered_summary_without_inline_original() {
    let root = Root::new();
    let script = noisy_script(&root.0);
    let profile = Profile {
        kind: ProfileKind::Test,
        program: script,
        args: vec![],
        env: BTreeMap::new(),
        timeout: Duration::from_secs(10),
        max_output_bytes: 64 * 1024,
        allow_filter: false,
        io: cayw_device::ProcessIo::Pipes,
    };
    let host = host(
        &root.0,
        HostConfig {
            profiles: BTreeMap::from([("noisy".into(), profile)]),
            output_filter: Some(FilterOptions {
                enabled: true,
                profile: FilterProfile::CargoLike,
                head_lines: 2,
                tail_lines: 2,
                max_lines: 12,
            }),
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
    assert!(
        result.get("stdout_original").is_none(),
        "full original must not be inlined alongside the summary"
    );
    assert!(
        result.get("stderr_original").is_none(),
        "full stderr original must not be inlined alongside the summary"
    );
    assert!(
        stdout.contains("test result: ok.") || stdout.contains("Finished "),
        "summary must keep cargo-like signal lines: {stdout}"
    );
    // Per-test "ok" noise from the raw script should be reduced out of the summary.
    assert!(
        !stdout.contains("test a::ok ... ok"),
        "summary should drop per-test ok noise: {stdout}"
    );
    assert_eq!(result["output_filter"]["protocol_version"], 1);
    assert_eq!(
        result["output_filter"]["stdout"]["applied_profile"],
        "cargo_like"
    );
    assert_eq!(result["output_filter"]["stdout"]["filter_enabled"], true);
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
    assert!(
        filtered_bytes < original_bytes,
        "filtered summary must be smaller than original ({filtered_bytes} vs {original_bytes})"
    );
    assert_eq!(filtered_bytes as usize, stdout.len());
    let digest = result["output_filter"]["stdout"]["original_digest"]
        .as_str()
        .unwrap();
    assert_eq!(digest.len(), 64);
    // Re-embedding the full capture would grow the agent payload by ~original_bytes.
    let encoded = serde_json::to_string(&result).unwrap();
    let mut with_original = result.clone();
    with_original["stdout_original"] = json!("X".repeat(original_bytes as usize));
    let dual_encoded = serde_json::to_string(&with_original).unwrap();
    assert!(
        dual_encoded.len() > encoded.len() + (original_bytes as usize / 2),
        "inlining original must be what inflates the payload ({} vs {})",
        encoded.len(),
        dual_encoded.len()
    );
}

#[tokio::test]
async fn output_filter_none_keeps_legacy_stdout_only_shape() {
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
