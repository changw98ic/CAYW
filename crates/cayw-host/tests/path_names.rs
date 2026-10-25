//! Regression tests for macOS path names (C05/C06/C07).
//!
//! Only actual `.git`, Host lock/temporary and workspace-store objects are
//! reserved. UTF-8 names, Windows-only-forbidden characters and user
//! `.cayw-*` names are ordinary files.

use async_trait::async_trait;
use cayw_ai::CancellationToken;
use cayw_codemode::{CapabilityError, CapabilityHost};
use cayw_host::{AuditSink, Host, HostConfig, PermissionGate};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

static NEXT: AtomicU64 = AtomicU64::new(1);

struct Root(PathBuf);
impl Root {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "cayw-host-path-names-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

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
struct Audit;
#[async_trait]
impl AuditSink for Audit {
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
        Ok(())
    }
}

fn host(root: &Root) -> Host {
    Host::new(
        root.path(),
        Arc::new(Gate),
        Arc::new(Audit),
        HostConfig::default(),
    )
    .unwrap()
}

async fn call(host: &Host, name: &str, args: Value) -> Result<Value, CapabilityError> {
    host.call(name, args, CancellationToken::new()).await
}

#[tokio::test]
async fn utf8_windows_forbidden_and_user_cayw_names_round_trip() {
    let root = Root::new();
    let host = host(&root);
    fs::create_dir_all(root.path().join("子目录")).unwrap();
    for path in [
        "说明.txt",
        "CON",
        "NUL.txt",
        "a:b",
        "a ",
        "a.",
        "a\\b",
        ".cayw-note.md",
        "子目录/文件.json",
    ] {
        let content = format!("content for {path}");
        let written = call(&host, "fs.write", json!({"path":path,"content":content}))
            .await
            .unwrap_or_else(|error| panic!("write {path:?}: {error}"));
        assert_eq!(written["bytes"], content.len());
        let read = call(&host, "fs.read", json!({"path":path}))
            .await
            .unwrap_or_else(|error| panic!("read {path:?}: {error}"));
        assert_eq!(read["content"], content);
        assert_eq!(
            call(&host, "fs.stat", json!({"path":path})).await.unwrap()["kind"],
            "file"
        );
    }
    let list = call(&host, "fs.list", json!({"path":"."})).await.unwrap();
    let names: Vec<&str> = list["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["name"].as_str().unwrap())
        .collect();
    for name in ["说明.txt", "CON", "a:b", "a\\b", ".cayw-note.md", "子目录"] {
        assert!(names.contains(&name), "{name:?} missing from {names:?}");
    }
}

#[tokio::test]
async fn actual_internal_objects_and_escapes_stay_rejected() {
    let root = Root::new();
    fs::create_dir(root.path().join(".cayw-workspace")).unwrap();
    fs::create_dir(root.path().join(".git")).unwrap();
    fs::write(root.path().join(".git/config"), "secret").unwrap();
    fs::write(root.path().join(".cayw-host-lock"), "").unwrap();
    let host = host(&root);
    for path in [
        ".git/config",
        ".GiT/config",
        ".cayw-workspace/state.json",
        ".CAYW-WORKSPACE/state.json",
        "nested/.cayw-workspace/candidate-0",
        ".cayw-host-lock",
        ".CAYW-HOST-LOCK",
        ".cayw-host-tmp-123-1",
        "../escape",
        "/etc/passwd",
        "a/../b",
        "a\u{0}b",
    ] {
        assert!(
            call(&host, "fs.read", json!({"path":path})).await.is_err(),
            "read accepted {path:?}"
        );
        assert!(
            call(&host, "fs.write", json!({"path":path,"content":"bad"}))
                .await
                .is_err(),
            "write accepted {path:?}"
        );
    }
    assert_eq!(
        fs::read_to_string(root.path().join(".git/config")).unwrap(),
        "secret"
    );
    let list = call(&host, "fs.list", json!({"path":"."})).await.unwrap();
    assert!(
        !list["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["name"] == ".cayw-workspace"
                || entry["name"] == ".cayw-host-lock"
                || entry["name"] == ".git")
    );
}
