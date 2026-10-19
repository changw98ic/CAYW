//! Offline example with an exact permission and a durable JSON-lines audit.
//! Run with `cargo run -p cayw-host --example local --locked`.
use async_trait::async_trait;
use cayw_ai::CancellationToken;
use cayw_codemode::{CapabilityError, CodeMode, Limits};
use cayw_host::{AuditSink, Host, HostConfig, PermissionGate};
use serde_json::{Value, json};
use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

struct ExactPermission;
#[async_trait]
impl PermissionGate for ExactPermission {
    async fn authorize(
        &self,
        name: &str,
        arguments: &Value,
        cancel: CancellationToken,
    ) -> Result<(), CapabilityError> {
        if !cancel.is_cancelled()
            && name == "fs.write"
            && *arguments == json!({"path":"hello.txt","content":"Hello from CodeMode"})
        {
            Ok(())
        } else {
            Err(CapabilityError::new(
                "this example permits only its exact file write",
            ))
        }
    }
}

struct FileAudit {
    file: Mutex<File>,
    next: AtomicU64,
}
impl FileAudit {
    fn open(path: &Path) -> std::io::Result<Self> {
        Ok(Self {
            file: Mutex::new(OpenOptions::new().create_new(true).write(true).open(path)?),
            next: AtomicU64::new(1),
        })
    }
    fn record(&self, value: Value) -> Result<(), CapabilityError> {
        let mut file = self
            .file
            .lock()
            .map_err(|_| CapabilityError::unknown("audit lock poisoned"))?;
        let mut bytes =
            serde_json::to_vec(&value).map_err(|error| CapabilityError::new(error.to_string()))?;
        bytes.push(b'\n');
        file.write_all(&bytes)
            .and_then(|_| file.sync_all())
            .map_err(|error| CapabilityError::unknown(error.to_string()))
    }
}
#[async_trait]
impl AuditSink for FileAudit {
    async fn started(&self, name: &str, arguments: &Value) -> Result<String, CapabilityError> {
        let id = self.next.fetch_add(1, Ordering::Relaxed).to_string();
        self.record(json!({"id":id,"state":"running","capability":name,"arguments":arguments}))?;
        Ok(id)
    }
    async fn settled(
        &self,
        id: &str,
        result: &Result<Value, CapabilityError>,
    ) -> Result<(), CapabilityError> {
        let value = match result {
            Ok(value) => json!({"id":id,"state":"succeeded","result":value}),
            Err(error) => {
                json!({"id":id,"state":if error.outcome_unknown {"outcome_unknown"} else {"failed"},"error":error.message})
            }
        };
        self.record(value)
    }
    fn dropped(&self, id: &str) -> Result<(), CapabilityError> {
        self.record(json!({"id":id,"state":"outcome_unknown","reason":"future dropped"}))
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = std::env::temp_dir().join(format!(
        "cayw-host-example-{}-{}",
        std::process::id(),
        cayw_ai::timestamp_ms()
    ));
    std::fs::create_dir(&root)?;
    let audit_path = root.with_extension("audit.jsonl");
    let audit = Arc::new(FileAudit::open(&audit_path)?);
    // The audit entry itself must survive a directory-entry loss as well.
    #[cfg(unix)]
    File::open(audit_path.parent().ok_or("audit has no parent")?)?.sync_all()?;
    let host = Arc::new(Host::new(
        &root,
        Arc::new(ExactPermission),
        audit,
        HostConfig::default(),
    )?);
    let mode = CodeMode::new(host, Limits::default())?;
    let report = mode
        .execute(
            "return cayw.call('fs.write', {path='hello.txt', content='Hello from CodeMode'})",
            CancellationToken::new(),
        )
        .await?;
    if std::fs::read_to_string(root.join("hello.txt"))? != "Hello from CodeMode" {
        return Err("file verification failed".into());
    }
    println!(
        "{}",
        serde_json::to_string_pretty(
            &json!({"workspace":root,"audit":audit_path,"report":report})
        )?
    );
    Ok(())
}
