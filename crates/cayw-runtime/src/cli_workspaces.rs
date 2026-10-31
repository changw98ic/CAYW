//! Host-only workspace catalog and owned local Runtime processes. Selection
//! never changes an existing Runtime root or stops another workspace.
use crate::{cli_settings::uncertain, cli_start::stop_child};
use cayw_ai::CancellationToken;
use cayw_ipc::Client;
use cayw_runtime::local_cli::LocalPaths;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap, error::Error, os::unix::ffi::OsStrExt, path::Path, process::Stdio,
    time::Duration,
};
use tokio::process::{Child, Command};
type Result<T> = std::result::Result<T, Box<dyn Error>>;

pub struct Workspaces {
    paths: LocalPaths,
    offline: bool,
    discoverable: bool,
    children: BTreeMap<String, Child>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Open {
    id: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}
impl Workspaces {
    pub fn new(paths: LocalPaths, offline: bool, discoverable: bool) -> Self {
        Self {
            paths,
            offline,
            discoverable,
            children: BTreeMap::new(),
        }
    }
    fn id(&self, path: &Path) -> String {
        if path == self.paths.workspace {
            "local".into()
        } else {
            format!(
                "workspace-{:x}",
                Sha256::digest(path.as_os_str().as_bytes())
            )
        }
    }
    pub async fn call(
        &mut self,
        method: &str,
        params: Value,
        cancel: CancellationToken,
    ) -> Result<Value> {
        if !self.discoverable {
            return Err("workspace switching requires the default local database and IPC discovery; use cayw start for registered local workspaces".into());
        }
        let mut exited = Vec::new();
        for (id, child) in &mut self.children {
            if child.try_wait()?.is_some() {
                exited.push(id.clone());
            }
        }
        for id in exited {
            self.children.remove(&id);
        }
        match method {
            "settings.workspaces.list" => {
                let _: Empty = serde_json::from_value(params)?;
                let mut rows = Vec::new();
                for path in self.paths.known_workspaces()? {
                    let id = self.id(&path);
                    let state = if let Some(child) = self.children.get_mut(&id) {
                        if child.try_wait()?.is_none() {
                            "managed"
                        } else {
                            "stopped"
                        }
                    } else if LocalPaths::from_environment(&path)
                        .and_then(|paths| paths.connect())
                        .is_ok()
                    {
                        "discoverable"
                    } else if path.is_dir() {
                        "not_started"
                    } else {
                        "unavailable"
                    };
                    rows.push(json!({"id":id,"path":path,"state":state}));
                }
                Ok(
                    json!({"current":"local","workspaces":rows,"registration":"cayw init --workspace DIR"}),
                )
            }
            "settings.workspaces.open" => {
                let open: Open = serde_json::from_value(params)?;
                // The browser supplies an opaque catalog ID, never a directory,
                // command, environment or permission override.
                let path = self.paths.known_workspaces()?.into_iter().find(|path| self.id(path) == open.id).ok_or("workspace is not registered; register it locally with cayw init --workspace DIR")?;
                let paths = LocalPaths::from_environment(&path)?;
                if paths.workspace != path {
                    return Err("registered workspace path changed; explicitly register its new canonical directory".into());
                }
                let existing_error = match healthy(&paths).await {
                    Ok(address) => {
                        return Ok(
                            json!({"id":open.id,"path":path,"ipc_addr":address,"reused":true,"managed":self.children.contains_key(&open.id)}),
                        );
                    }
                    Err(error) => error.to_string(),
                };
                if open.id == "local" {
                    return Err(format!(
                        "primary workspace service is unavailable: {existing_error}"
                    )
                    .into());
                }
                if !self.children.contains_key(&open.id) {
                    match paths.acquire_server() {
                        Ok(available) => drop(available),
                        Err(ownership) => return Err(format!("workspace service is not ready: {}; cannot claim its runtime: {ownership}", existing_error).into()),
                    }

                    if self.children.len() >= 16 {
                        return Err("workspace host already owns 16 runtimes; close this launcher after finishing its tasks before opening more".into());
                    }
                    let mut command = Command::new(std::env::current_exe()?);
                    command
                        .arg("serve")
                        .arg("--workspace")
                        .arg(&path)
                        .env("CAYW_HOME", &self.paths.data_dir)
                        .current_dir(&path)
                        .stdin(Stdio::null())
                        .stdout(Stdio::null());
                    // Never copy primary-workspace command-line grants or custom
                    // policy paths to another project. Offline mode has no grant.
                    if self.offline {
                        command.arg("--offline");
                    }
                    self.children.insert(open.id.clone(), command.spawn()?);
                }
                let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
                loop {
                    if let Some(status) = self
                        .children
                        .get_mut(&open.id)
                        .expect("owned child")
                        .try_wait()?
                    {
                        return Err(format!("workspace runtime exited with {status}; see the launch terminal for its original error; no task was replayed").into());
                    }
                    tokio::select! {
                        _ = cancel.cancelled() => return Err(uncertain(std::io::Error::other("workspace startup was interrupted; inspect workspace status before retrying"))),
                        result = healthy(&paths) => if let Ok(address) = result {
                            return Ok(json!({"id":open.id,"path":path,"ipc_addr":address,"reused":false,"managed":true}));
                        },
                    }
                    if tokio::time::Instant::now() >= deadline {
                        return Err(uncertain(std::io::Error::other(
                            "workspace readiness was not confirmed; its owned process is retained for inspection",
                        )));
                    }
                    tokio::select! { _ = cancel.cancelled() => return Err(uncertain(std::io::Error::other("workspace startup interrupted; inspect status"))), _ = tokio::time::sleep(Duration::from_millis(50)) => {} }
                }
            }
            _ => Err("unsupported workspace method".into()),
        }
    }
    pub async fn shutdown(&mut self) -> Result<()> {
        let mut errors = Vec::new();
        for (id, child) in &mut self.children {
            if let Err(error) = stop_child(child).await {
                errors.push(format!("{id}: {error}"));
            }
        }
        if !errors.is_empty() {
            return Err(errors.join("; ").into());
        }
        Ok(())
    }
}
async fn healthy(paths: &LocalPaths) -> Result<std::net::SocketAddr> {
    let (address, token) = paths.connect()?;
    tokio::time::timeout(Duration::from_secs(3), async {
        let client = Client::connect(address, token).await?;
        let health = client.request("runtime.health", json!({})).await?;
        if health["status"] != "ok" || health["protocol_version"] != cayw_core::PROTOCOL_VERSION {
            return Err("workspace runtime protocol is incompatible".into());
        }
        Ok::<_, Box<dyn Error>>(address)
    })
    .await?
}

pub fn optional_tools(pi_enabled: bool) -> Result<Value> {
    use std::os::unix::fs::PermissionsExt;
    let executable = std::env::current_exe()?;
    let installed = |name: &str| {
        std::fs::metadata(executable.with_file_name(name))
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    let prefix = executable
        .parent()
        .filter(|path| path.file_name().is_some_and(|name| name == "bin"))
        .and_then(Path::parent);
    let install = |package: &str, program: &str, features: &str| match prefix {
        Some(prefix) => format!(
            "cargo install --path crates/{package} --bin {program} {features} --locked --root '{}'",
            prefix.to_string_lossy().replace('\'', "'\"'\"'")
        ),
        None => format!("cargo build -p {package} --bin {program} {features} --locked"),
    };
    Ok(json!({
        "note":"在 CAYW 源码根目录运行；安装构建需将可选程序放在当前 cayw 的同一目录。外部服务需单独启动并保持独立认证。",
        "editor":{"installed":installed("cayw-visual-editor"),"status":"external_service_required","install":install("cayw-visual-editor","cayw-visual-editor",""),"start":"cayw-visual-editor --help"},
        "trace":{"installed":installed("cayw-trace-debug"),"status":"external_service_required","install":install("cayw-trace-debug","cayw-trace-debug",""),"start":"cayw-trace-debug --help"},
        "pi":{"installed":cfg!(feature="pi-compat"),"enabled":pi_enabled,"install":install("cayw-runtime","cayw","--features pi-compat"),"start":"cayw serve --pi-compat"}
    }))
}
