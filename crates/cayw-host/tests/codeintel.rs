//! F04: producer refresh + definitions share one bridge with a Trace-style clone.
#![cfg(unix)]

use async_trait::async_trait;
use cayw_ai::CancellationToken;
use cayw_codeintel::SymbolVersionBridge;
use cayw_codemode::{CapabilityError, CapabilityHost};
use cayw_host::{AuditSink, Host, HostConfig, PermissionGate};
use serde_json::{Value, json};
use std::{
    fs,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

static NEXT: AtomicU64 = AtomicU64::new(0);
const SOURCE: &str = "pub fn visible() {\n    let value = 42;\n}\nstruct Helper;\n";

struct Root(PathBuf);
impl Root {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "cayw-host-codeintel-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        fs::create_dir(path.join("src")).unwrap();
        fs::write(path.join("src/lib.rs"), SOURCE).unwrap();
        Self(path)
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Default)]
struct Gate;
#[async_trait]
impl PermissionGate for Gate {
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
struct Audit {
    starts: Mutex<Vec<String>>,
}
#[async_trait]
impl AuditSink for Audit {
    async fn started(&self, name: &str, _: &Value) -> Result<String, CapabilityError> {
        let mut starts = self.starts.lock().unwrap();
        let id = starts.len().to_string();
        starts.push(name.into());
        Ok(id)
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

async fn call(host: &Host, name: &str, args: Value) -> Value {
    host.call(name, args, CancellationToken::new())
        .await
        .unwrap_or_else(|e| panic!("{name}: {e}"))
}

#[tokio::test]
async fn refresh_publishes_outline_and_definitions_share_bridge_with_trace_clone() {
    let root = Root::new();
    let bridge = SymbolVersionBridge::new();
    let trace = bridge.clone(); // Trace/editor hold the same instance
    let host = Host::new(
        &root.0,
        Arc::new(Gate),
        Arc::new(Audit::default()),
        HostConfig {
            codeintel: Some(bridge.clone()),
            ..HostConfig::default()
        },
    )
    .unwrap();

    let refreshed = call(
        &host,
        "codeintel.refresh",
        json!({"path": "src/lib.rs", "language": "rust"}),
    )
    .await;
    assert_eq!(refreshed["protocol_version"], 1);
    assert_eq!(refreshed["file"]["path"], "src/lib.rs");
    assert_eq!(refreshed["file"]["version"], 1);
    assert!(refreshed["symbol_count"].as_u64().unwrap() >= 1);
    assert_eq!(refreshed["source"], "code.outline");

    let defs = call(
        &host,
        "codeintel.definitions",
        json!({"name": "visible", "kind": "function"}),
    )
    .await;
    assert_eq!(defs["definitions"].as_array().unwrap().len(), 1);
    assert_eq!(defs["definitions"][0]["id"]["name"], "visible");

    // Trace clone sees the same indexed version without a second publish.
    let from_trace = trace.file_version("src/lib.rs").unwrap().unwrap();
    assert_eq!(from_trace.version, refreshed["file"]["version"]);
    assert_eq!(
        from_trace.content_sha256,
        refreshed["file"]["content_sha256"]
    );
    let trace_defs = trace
        .definitions(cayw_codeintel::DefinitionQuery {
            name: "visible".into(),
            path: None,
            kind: Some("function".into()),
            file_version: Some(from_trace.version),
        })
        .unwrap();
    assert_eq!(trace_defs.len(), 1);
    assert_eq!(
        trace_defs[0].id.wire(),
        defs["definitions"][0]["id"]
            .as_object()
            .map(|_| {
                // Compare via bridge resolve fields
                format!(
                    "sv1:{}|{}|{}|{}|{}|{}",
                    defs["definitions"][0]["id"]["path"].as_str().unwrap(),
                    defs["definitions"][0]["id"]["kind"].as_str().unwrap(),
                    defs["definitions"][0]["id"]["name"].as_str().unwrap(),
                    defs["definitions"][0]["id"]["byte_start"],
                    defs["definitions"][0]["id"]["file_version"],
                    defs["definitions"][0]["id"]["content_sha256"]
                        .as_str()
                        .unwrap()
                )
            })
            .unwrap()
    );

    let version = call(
        &host,
        "codeintel.file_version",
        json!({"path": "src/lib.rs"}),
    )
    .await;
    assert_eq!(version["file"]["version"], 1);

    // Content change → version bump via producer path.
    fs::write(
        root.0.join("src/lib.rs"),
        "pub fn visible() {}\npub fn extra() {}\n",
    )
    .unwrap();
    let again = call(&host, "codeintel.refresh", json!({"path": "src/lib.rs"})).await;
    assert_eq!(again["file"]["version"], 2);
    assert_eq!(
        trace.file_version("src/lib.rs").unwrap().unwrap().version,
        2
    );
}

#[tokio::test]
async fn codeintel_omitted_when_bridge_disabled() {
    let root = Root::new();
    let host = Host::new(
        &root.0,
        Arc::new(Gate),
        Arc::new(Audit::default()),
        HostConfig {
            codeintel: None,
            ..HostConfig::default()
        },
    )
    .unwrap();
    let err = host
        .call(
            "codeintel.refresh",
            json!({"path": "src/lib.rs"}),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(
        err.message.contains("not configured") || err.message.contains("capability"),
        "{}",
        err.message
    );
}

#[tokio::test]
async fn default_host_omits_codeintel_catalog() {
    let root = Root::new();
    let host = Host::new(
        &root.0,
        Arc::new(Gate),
        Arc::new(Audit::default()),
        HostConfig::default(),
    )
    .unwrap();
    assert!(
        !host
            .capabilities()
            .iter()
            .any(|d| d.name.starts_with("codeintel.")),
        "ordinary HostConfig::default() must not advertise codeintel.*"
    );
}
