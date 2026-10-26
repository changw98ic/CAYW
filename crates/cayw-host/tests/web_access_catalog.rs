//! F10: web.* tools appear in the host catalog and execute via Executor::WebAccess.

use async_trait::async_trait;
use cayw_ai::CancellationToken;
use cayw_codemode::{CapabilityError, CapabilityHost};
use cayw_host::{AuditSink, Host, HostConfig, PermissionGate};
use cayw_web_access::WebAccessOptions;
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
};

static NEXT: AtomicU64 = AtomicU64::new(1);

struct Workspace(PathBuf);
impl Workspace {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "cayw-host-web-{}-{}",
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

struct Gate;
#[async_trait]
impl PermissionGate for Gate {
    async fn authorize(
        &self,
        _: &str,
        _: &Value,
        cancel: CancellationToken,
    ) -> Result<(), CapabilityError> {
        if cancel.is_cancelled() {
            Err(CapabilityError::new("denied"))
        } else {
            Ok(())
        }
    }
}

#[derive(Default)]
struct Audit {
    records: Mutex<Vec<String>>,
}
#[async_trait]
impl AuditSink for Audit {
    async fn started(&self, capability: &str, _: &Value) -> Result<String, CapabilityError> {
        let mut records = self.records.lock().unwrap();
        let id = records.len().to_string();
        records.push(format!("start:{capability}"));
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

fn open(config: HostConfig) -> (Workspace, Host) {
    let ws = Workspace::new();
    let host = Host::new(
        ws.path(),
        Arc::new(Gate),
        Arc::new(Audit::default()),
        config,
    )
    .unwrap();
    (ws, host)
}

#[tokio::test]
async fn default_host_omits_web_catalog() {
    let (_ws, host) = open(HostConfig::default());
    assert!(
        !host
            .capabilities()
            .iter()
            .any(|d| d.name.starts_with("web.")),
        "ordinary HostConfig::default() must not advertise web.*"
    );
}

#[tokio::test]
async fn enabled_web_access_registers_web_tools() {
    let (_ws, host) = open(HostConfig {
        web_access: Some(WebAccessOptions::default()),
        ..HostConfig::default()
    });
    let names: Vec<_> = host.capabilities().into_iter().map(|d| d.name).collect();
    for need in ["web.search", "web.fetch", "web.extract"] {
        assert!(
            names.iter().any(|n| n == need),
            "missing {need} in {names:?}"
        );
    }
    assert!(!names.iter().any(|n| n == "search.query"));
}

#[tokio::test]
async fn fixture_web_search_via_host_call() {
    let config = HostConfig {
        web_access: Some(WebAccessOptions::fixture()),
        ..Default::default()
    };
    let (_ws, host) = open(config);
    let result = host
        .call(
            "web.search",
            json!({"query": "anything", "limit": 3}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(result["provider"], "fixture");
    assert_eq!(result["hits"], json!([]));
}

#[tokio::test]
async fn disabled_web_access_omits_catalog() {
    let config = HostConfig {
        web_access: None,
        ..Default::default()
    };
    let (_ws, host) = open(config);
    assert!(!host
        .capabilities()
        .iter()
        .any(|d| d.name.starts_with("web.")));
}
