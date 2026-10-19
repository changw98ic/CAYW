#![cfg(unix)]

use async_trait::async_trait;
use cayw_ai::CancellationToken;
use cayw_codemode::{CallStatus, CapabilityError, CapabilityHost, CodeMode, Limits};
use cayw_host::{AuditSink, Host, HostConfig, PermissionGate};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

static NEXT: AtomicU64 = AtomicU64::new(0);
const SOURCE: &str = "pub fn visible() {\n    let value = 42;\n}\n";
const DIFF: &str = "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,3 +1,3 @@\n pub fn visible() {\n-    let value = 41;\n+    let value = 42;\n }\n";

struct Root(PathBuf);
impl Root {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "cayw-host-read-contracts-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        fs::create_dir(path.join("src")).unwrap();
        fs::write(path.join("src/lib.rs"), SOURCE).unwrap();
        fs::write(path.join(".gitignore"), "ignored.rs\n").unwrap();
        fs::write(path.join("ignored.rs"), "pub fn hidden() {}\n").unwrap();
        Self(path)
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Gate {
    allow: bool,
    calls: Mutex<Vec<(String, Value)>>,
}
#[async_trait]
impl PermissionGate for Gate {
    async fn authorize(
        &self,
        name: &str,
        arguments: &Value,
        _: CancellationToken,
    ) -> Result<(), CapabilityError> {
        self.calls
            .lock()
            .unwrap()
            .push((name.into(), arguments.clone()));
        if self.allow {
            Ok(())
        } else {
            Err(CapabilityError::new("fixture permission denied"))
        }
    }
}

#[derive(Default)]
struct Audit {
    starts: Mutex<Vec<(String, String, Value)>>,
    terminals: Mutex<BTreeMap<String, Result<Value, CapabilityError>>>,
    drops: Mutex<Vec<String>>,
    cancel_at_start: Option<CancellationToken>,
}
#[async_trait]
impl AuditSink for Audit {
    async fn started(&self, name: &str, arguments: &Value) -> Result<String, CapabilityError> {
        let mut starts = self.starts.lock().unwrap();
        let id = starts.len().to_string();
        starts.push((id.clone(), name.into(), arguments.clone()));
        if let Some(cancel) = &self.cancel_at_start {
            cancel.cancel();
        }
        Ok(id)
    }
    async fn settled(
        &self,
        id: &str,
        result: &Result<Value, CapabilityError>,
    ) -> Result<(), CapabilityError> {
        assert!(
            self.terminals
                .lock()
                .unwrap()
                .insert(id.into(), result.clone())
                .is_none()
        );
        Ok(())
    }
    fn dropped(&self, id: &str) -> Result<(), CapabilityError> {
        self.drops.lock().unwrap().push(id.into());
        Ok(())
    }
}

fn fixture(root: &Root, allow: bool, config: HostConfig) -> (Arc<Host>, Arc<Gate>, Arc<Audit>) {
    let gate = Arc::new(Gate {
        allow,
        calls: Mutex::new(Vec::new()),
    });
    let audit = Arc::new(Audit::default());
    let host = Arc::new(Host::new(&root.0, gate.clone(), audit.clone(), config).unwrap());
    (host, gate, audit)
}

fn requests() -> Vec<(&'static str, Value)> {
    vec![
        ("fs.glob", json!({"path":".","pattern":"**/*.rs"})),
        (
            "fs.grep",
            json!({"path":".","pattern":"^pub fn","multiline":true}),
        ),
        (
            "code.outline",
            json!({"path":"src/lib.rs","language":"rust"}),
        ),
        ("code.diff_hunks", json!({"diff":DIFF})),
    ]
}

#[tokio::test]
async fn discovery_and_denied_reads_do_not_open_the_workspace_store() {
    let root = Root::new();
    let (host, gate, audit) = fixture(&root, false, HostConfig::default());
    let catalog = host.capabilities();
    for (name, arguments) in requests() {
        assert_eq!(catalog.iter().filter(|tool| tool.name == name).count(), 1);
        let error = host
            .call(name, arguments, CancellationToken::new())
            .await
            .unwrap_err();
        assert!(!error.outcome_unknown);
        assert!(error.message.contains("permission denied"));
    }
    assert_eq!(gate.calls.lock().unwrap().len(), 4);
    assert!(audit.starts.lock().unwrap().is_empty());
    assert!(audit.terminals.lock().unwrap().is_empty());
    assert!(audit.drops.lock().unwrap().is_empty());
    assert!(!root.0.join(".cayw-workspace").exists());
    assert!(!root.0.join(".cayw-host-lock").exists());
    assert_eq!(
        fs::read_to_string(root.0.join("src/lib.rs")).unwrap(),
        SOURCE
    );
}

#[tokio::test]
async fn strict_schema_and_confined_paths_fail_before_permission() {
    let root = Root::new();
    let (host, gate, audit) = fixture(&root, true, HostConfig::default());
    for (name, arguments) in requests() {
        let mut extra = arguments.clone();
        extra["unregistered"] = json!(true);
        assert!(
            !host
                .call(name, extra, CancellationToken::new())
                .await
                .unwrap_err()
                .outcome_unknown
        );
        if arguments.get("path").is_some() {
            for path in [
                "../escape",
                "/outside",
                ".git/config",
                ".cayw-workspace/state.json",
            ] {
                let mut escaped = arguments.clone();
                escaped["path"] = json!(path);
                assert!(
                    host.call(name, escaped, CancellationToken::new())
                        .await
                        .is_err()
                );
            }
        }
    }
    assert!(gate.calls.lock().unwrap().is_empty());
    assert!(audit.starts.lock().unwrap().is_empty());
    assert!(!root.0.join(".cayw-workspace").exists());
}

#[tokio::test]
async fn luau_routes_all_four_reads_through_original_host_permissions_and_audit() {
    let root = Root::new();
    let (host, gate, audit) = fixture(&root, true, HostConfig::default());
    let mode = CodeMode::new(host.clone(), Limits::default()).unwrap();
    let script = format!(
        "local files = cayw.call('fs.glob', {{path='.', pattern='**/*.rs'}})\n\
         local matches = cayw.call('fs.grep', {{path='.', pattern='^pub fn', multiline=true}})\n\
         local outline = cayw.call('code.outline', {{path='src/lib.rs', language='rust'}})\n\
         local hunks = cayw.call('code.diff_hunks', {{diff=[=[{DIFF}]=]}})\n\
         return {{files=files, matches=matches, outline=outline, hunks=hunks}}"
    );
    let report = mode
        .execute(&script, CancellationToken::new())
        .await
        .unwrap();
    assert!(!report.outcome_unknown);
    assert_eq!(report.calls.len(), 4);
    let expected = requests();
    let starts = audit.starts.lock().unwrap();
    let terminals = audit.terminals.lock().unwrap();
    let permissions = gate.calls.lock().unwrap();
    for (index, (name, arguments)) in expected.iter().enumerate() {
        let call = &report.calls[index];
        assert_eq!(call.name, *name);
        assert_eq!(call.status, CallStatus::Succeeded);
        assert_eq!(call.arguments.as_ref().unwrap(), arguments);
        assert!(!call.result_omitted);
        assert_eq!(&permissions[index], &(name.to_string(), arguments.clone()));
        let (id, audited_name, audited_arguments) = &starts[index];
        assert_eq!(audited_name, name);
        assert_eq!(audited_arguments, arguments);
        assert_eq!(
            terminals[id].as_ref().unwrap(),
            call.result.as_ref().unwrap()
        );
    }
    assert_eq!(terminals.len(), 4);
    assert!(audit.drops.lock().unwrap().is_empty());
    for (key, call) in ["files", "matches", "outline", "hunks"]
        .into_iter()
        .zip(&report.calls)
    {
        assert_eq!(&report.return_value[key], call.result.as_ref().unwrap());
    }
    let files = &report.return_value["files"];
    assert_eq!(files["paths"], json!(["src/lib.rs"]));
    assert_eq!(files["scan_complete"], true);
    assert_eq!(files["truncated"], false);
    let matches = &report.return_value["matches"];
    assert_eq!(matches["matches"].as_array().unwrap().len(), 1);
    assert_eq!(matches["matches"][0]["path"], "src/lib.rs");
    assert_eq!(matches["matches"][0]["start_line"], 1);
    assert_eq!(matches["matches"][0]["byte_start"], 0);
    assert_eq!(matches["matches"][0]["text"], "pub fn");
    let digest = format!("{:x}", Sha256::digest(SOURCE.as_bytes()));
    assert_eq!(matches["matches"][0]["sha256"], digest);
    assert_eq!(matches["scan_complete"], true);
    let outline = &report.return_value["outline"];
    assert_eq!(outline["sha256"], digest);
    assert_eq!(outline["syntax_only"], true);
    assert_eq!(outline["symbols"][0]["name"], "visible");
    let hunks = &report.return_value["hunks"];
    assert_eq!(hunks["validated_complete"], true);
    assert_eq!(hunks["total_hunks"], 1);
    assert_eq!(hunks["hunks"][0]["old_path"], "src/lib.rs");
    assert_eq!(hunks["hunks"][0]["new_path"], "src/lib.rs");
    assert_eq!(hunks["hunks"][0]["new"]["start_line"], 1);
    assert_eq!(hunks["hunks"][0]["new"]["line_count"], 3);
    assert_eq!(
        fs::read_to_string(root.0.join("src/lib.rs")).unwrap(),
        SOURCE
    );
    assert_eq!(
        fs::read_to_string(root.0.join("ignored.rs")).unwrap(),
        "pub fn hidden() {}\n"
    );
}

#[tokio::test]
async fn scoped_reads_share_host_call_budget_and_cannot_expand_the_catalog() {
    let root = Root::new();
    let (host, gate, audit) = fixture(
        &root,
        true,
        HostConfig {
            max_calls: 1,
            ..Default::default()
        },
    );
    let scoped = host.scoped(gate.clone(), audit.clone()).unwrap();
    host.call(
        "code.diff_hunks",
        json!({"diff":DIFF}),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let error = scoped
        .call(
            "fs.glob",
            json!({"path":".","pattern":"**/*.rs"}),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(!error.outcome_unknown);
    assert!(error.message.contains("budget"));
    assert_eq!(gate.calls.lock().unwrap().len(), 1);
    assert_eq!(audit.starts.lock().unwrap().len(), 1);
    assert_eq!(audit.terminals.lock().unwrap().len(), 1);

    let disabled_root = Root::new();
    let (disabled, disabled_gate, disabled_audit) = fixture(
        &disabled_root,
        true,
        HostConfig {
            workspace: None,
            ..Default::default()
        },
    );
    let scope = disabled
        .scoped(disabled_gate.clone(), disabled_audit.clone())
        .unwrap();
    for (name, args) in requests() {
        assert!(
            !scope
                .capabilities()
                .iter()
                .any(|definition| definition.name == name)
        );
        assert!(
            scope
                .call(name, args, CancellationToken::new())
                .await
                .is_err()
        );
    }
    assert!(disabled_gate.calls.lock().unwrap().is_empty());
    assert!(disabled_audit.starts.lock().unwrap().is_empty());
    assert!(!disabled_root.0.join(".cayw-workspace").exists());
}

#[tokio::test]
async fn read_result_limits_settle_known_failure_and_preserve_next_call() {
    let root = Root::new();
    let (host, gate, audit) = fixture(
        &root,
        true,
        HostConfig {
            max_result_bytes: 128,
            ..Default::default()
        },
    );
    let error = host
        .call(
            "fs.grep",
            json!({"path":"src/lib.rs","pattern":"value"}),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(!error.outcome_unknown);
    assert!(!host.outcome_unknown());
    assert!(
        !audit.terminals.lock().unwrap()["0"]
            .as_ref()
            .unwrap_err()
            .outcome_unknown
    );
    host.call(
        "fs.stat",
        json!({"path":"src/lib.rs"}),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(gate.calls.lock().unwrap().len(), 2);
    assert_eq!(audit.terminals.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn cancellation_after_audit_start_does_not_open_workspace_or_lose_terminal() {
    for (name, args) in requests() {
        let root = Root::new();
        let cancel = CancellationToken::new();
        let gate = Arc::new(Gate {
            allow: true,
            calls: Mutex::new(Vec::new()),
        });
        let audit = Arc::new(Audit {
            cancel_at_start: Some(cancel.clone()),
            ..Default::default()
        });
        let host = Host::new(&root.0, gate.clone(), audit.clone(), HostConfig::default()).unwrap();
        let error = host.call(name, args, cancel).await.unwrap_err();
        assert!(!error.outcome_unknown);
        assert_eq!(gate.calls.lock().unwrap().len(), 1);
        assert_eq!(audit.starts.lock().unwrap().len(), 1);
        assert_eq!(audit.terminals.lock().unwrap().len(), 1);
        assert!(
            !audit.terminals.lock().unwrap()["0"]
                .as_ref()
                .unwrap_err()
                .outcome_unknown
        );
        assert!(audit.drops.lock().unwrap().is_empty());
        assert!(!root.0.join(".cayw-workspace").exists());
        assert!(!host.outcome_unknown());
    }
}
