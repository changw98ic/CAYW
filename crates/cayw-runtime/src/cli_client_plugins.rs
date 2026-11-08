//! Built-in Client-role plugin lifecycle. WebUI is a Client-owned background
//! process. TUI uses a foreground terminal handoff, then the Client supervises it.
use cayw_ai::CancellationToken;
use cayw_ipc::RpcError;
use cayw_runtime::local_cli::{read_private, save_config};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    error::Error,
    net::SocketAddr,
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command as StdCommand, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
    sync::Mutex,
};

const START_TIMEOUT: Duration = Duration::from_secs(30);
const STOP_TIMEOUT: Duration = Duration::from_secs(90);
const PREPARE_TIMEOUT: Duration = Duration::from_secs(30);
const CLEANUP_SCHEMA: &str = "cayw.client-plugin-cleanup.v1";
const CLEANUP_FILE: &str = "client-plugin-cleanup.json";
const RECENT_LIMIT: usize = 16;

type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

#[derive(Clone)]
pub struct ClientPlugins {
    inner: Arc<Inner>,
}

struct Inner {
    address: SocketAddr,
    token: String,
    data_dir: PathBuf,
    cleanup_path: PathBuf,
    webui_binary: PathBuf,
    tui_binary: PathBuf,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    webui: Option<WebUi>,
    failed_web: BTreeMap<String, FailedWeb>,
    pending_tui: BTreeMap<String, PendingTui>,
    tui: BTreeMap<String, Tui>,
    recent: VecDeque<Value>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProcessIdentity {
    pid: u32,
    started: String,
    command: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CleanupProcess {
    launch_id: String,
    plugin: String,
    workspace_id: String,
    process: ProcessIdentity,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CleanupLedger {
    schema: String,
    processes: Vec<CleanupProcess>,
}

struct WebUi {
    child: Child,
    identity: ProcessIdentity,
    launch_id: String,
    workspace_id: String,
    lan: Option<String>,
    account_file: PathBuf,
    ready: Value,
}

struct FailedWeb {
    child: Child,
    cleanup: CleanupProcess,
    error: String,
}

struct PendingTui {
    workspace_id: String,
    created: Instant,
}

struct Tui {
    workspace_id: String,
    identity: ProcessIdentity,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Start {
    plugin: String,
    workspace_id: String,
    #[serde(default)]
    lan: Option<String>,
    #[serde(default)]
    account_file: Option<PathBuf>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Attach {
    launch_id: String,
    pid: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Detach {
    launch_id: String,
    pid: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Stop {
    launch_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}

impl ClientPlugins {
    pub fn new(address: SocketAddr, token: String, data_dir: PathBuf) -> Result<Self> {
        let executable = std::env::current_exe()?;
        let parent = executable.parent().unwrap_or_else(|| Path::new("."));
        Self::with_binaries(
            address,
            token,
            data_dir,
            parent.join("cayw-webui"),
            parent.join("cayw-tui"),
        )
    }

    fn with_binaries(
        address: SocketAddr,
        token: String,
        data_dir: PathBuf,
        webui_binary: PathBuf,
        tui_binary: PathBuf,
    ) -> Result<Self> {
        let cleanup_path = data_dir.join(CLEANUP_FILE);
        reconcile_cleanup(&cleanup_path)?;
        Ok(Self {
            inner: Arc::new(Inner {
                address,
                token,
                data_dir,
                cleanup_path,
                webui_binary,
                tui_binary,
                state: Mutex::new(State::default()),
            }),
        })
    }

    pub async fn call(
        &self,
        method: &str,
        params: Value,
        cancel: CancellationToken,
    ) -> std::result::Result<Value, RpcError> {
        let result = match method {
            "client.plugins.list" => {
                let _: Empty = parse(params)?;
                self.list().await
            }
            "client.plugins.start" => self.start(parse(params)?, cancel).await,
            "client.plugins.attach" => self.attach(parse(params)?).await,
            "client.plugins.detach" => self.detach(parse(params)?).await,
            "client.plugins.stop" => self.stop(parse(params)?).await,
            _ => Err("unsupported Client plugin method".into()),
        };
        result.map_err(client_plugin_error)
    }

    pub fn start_workspace(&self, params: &Value) -> std::result::Result<String, RpcError> {
        let request: Start = parse(params.clone())?;
        self.validate_start(&request).map_err(client_plugin_error)?;
        Ok(request.workspace_id)
    }

    pub async fn read_only_summary(&self) -> Value {
        let state = self.inner.state.lock().await;
        state_summary(&state)
    }

    fn persist_cleanup(&self, processes: &[CleanupProcess]) -> Result<()> {
        persist_cleanup(&self.inner.cleanup_path, processes)
    }

    pub async fn shutdown(&self) -> Result<()> {
        let mut state = self.inner.state.lock().await;
        reap(&mut state)?;
        state.pending_tui.clear();
        let intended = cleanup_snapshot(&state);
        if intended.is_empty() {
            self.persist_cleanup(&[])?;
            return Ok(());
        }
        self.persist_cleanup(&intended)?;

        let mut failures = Vec::new();
        if let Some(webui) = state.webui.as_mut() {
            match stop_child(&mut webui.child).await {
                Ok(()) => {
                    let webui = state.webui.take().expect("stopped WebUI");
                    record_terminal(
                        &mut state,
                        "webui",
                        &webui.launch_id,
                        &webui.workspace_id,
                        webui.identity.pid,
                        "stopped",
                        None,
                    );
                }
                Err(error) => failures.push(format!("WebUI: {error}")),
            }
        }
        let failed_launches = state.failed_web.keys().cloned().collect::<Vec<_>>();
        for launch_id in failed_launches {
            let result = stop_child(
                &mut state
                    .failed_web
                    .get_mut(&launch_id)
                    .expect("listed failed WebUI")
                    .child,
            )
            .await;
            match result {
                Ok(()) => {
                    let failed = state
                        .failed_web
                        .remove(&launch_id)
                        .expect("stopped failed WebUI");
                    record_terminal(
                        &mut state,
                        "webui",
                        &launch_id,
                        &failed.cleanup.workspace_id,
                        failed.cleanup.process.pid,
                        "cleanup_completed",
                        Some(failed.error),
                    );
                }
                Err(error) => failures.push(format!("WebUI {}: {error}", launch_id)),
            }
        }
        let tui_launches = state.tui.keys().cloned().collect::<Vec<_>>();
        for launch_id in tui_launches {
            let (workspace_id, identity) = {
                let tui = state.tui.get(&launch_id).expect("listed TUI");
                (tui.workspace_id.clone(), tui.identity.clone())
            };
            match stop_pid(&identity).await {
                Ok(()) => {
                    state.tui.remove(&launch_id);
                    record_terminal(
                        &mut state,
                        "tui",
                        &launch_id,
                        &workspace_id,
                        identity.pid,
                        "stopped",
                        None,
                    );
                }
                Err(error) => failures.push(format!("TUI {}: {error}", identity.pid)),
            }
        }
        if let Err(error) = self.persist_cleanup(&cleanup_snapshot(&state)) {
            failures.push(format!("cleanup state: {error}"));
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(failures.join("; ").into())
        }
    }

    fn validate_start(&self, request: &Start) -> Result<()> {
        validate_workspace_id(&request.workspace_id)?;
        match request.plugin.as_str() {
            "webui" => {
                if request
                    .account_file
                    .as_ref()
                    .is_some_and(|path| !path.is_absolute())
                {
                    return Err("WebUI account file must be an absolute path".into());
                }
            }
            "tui" => {
                if request.lan.is_some() || request.account_file.is_some() {
                    return Err("TUI does not accept WebUI network or account settings".into());
                }
            }
            _ => return Err("plugin must be webui or tui".into()),
        }
        Ok(())
    }

    async fn start(&self, request: Start, cancel: CancellationToken) -> Result<Value> {
        self.validate_start(&request)?;
        if cancel.is_cancelled() {
            return Err("Client plugin start was cancelled before launch".into());
        }
        match request.plugin.as_str() {
            "webui" => {
                if !self.inner.webui_binary.is_file() {
                    return Err(format!(
                        "{} is missing; install a complete package",
                        self.inner.webui_binary.display()
                    )
                    .into());
                }
                self.start_webui(request, cancel).await
            }
            "tui" => {
                if !self.inner.tui_binary.is_file() {
                    return Err(format!(
                        "{} is missing; install a complete package",
                        self.inner.tui_binary.display()
                    )
                    .into());
                }
                let mut state = self.inner.state.lock().await;
                reap(&mut state)?;
                self.persist_cleanup(&cleanup_snapshot(&state))?;
                let launch_id = uuid::Uuid::new_v4().to_string();
                state.pending_tui.insert(
                    launch_id.clone(),
                    PendingTui {
                        workspace_id: request.workspace_id.clone(),
                        created: Instant::now(),
                    },
                );
                Ok(json!({
                    "plugin":"tui",
                    "role":"client",
                    "kind":"foreground",
                    "launch_id":launch_id,
                    "workspace_id":request.workspace_id,
                    "attach_required":true,
                    "reused":false,
                }))
            }
            _ => Err("plugin must be webui or tui".into()),
        }
    }

    async fn abort_web_start(
        &self,
        state: &mut State,
        mut child: Child,
        cleanup: CleanupProcess,
        reason: impl Into<String>,
    ) -> Result<Value> {
        let reason = reason.into();
        let mut intended = cleanup_snapshot(state);
        intended.push(cleanup.clone());
        self.persist_cleanup(&intended)?;
        match stop_child(&mut child).await {
            Ok(()) => {
                self.persist_cleanup(&cleanup_snapshot(state))?;
                record_terminal(
                    state,
                    "webui",
                    &cleanup.launch_id,
                    &cleanup.workspace_id,
                    cleanup.process.pid,
                    "start_failed",
                    Some(reason.clone()),
                );
                Err(reason.into())
            }
            Err(stop) => {
                let launch_id = cleanup.launch_id.clone();
                state.failed_web.insert(
                    launch_id,
                    FailedWeb {
                        child,
                        cleanup,
                        error: reason.clone(),
                    },
                );
                self.persist_cleanup(&cleanup_snapshot(state))?;
                Err(format!(
                    "{reason}; WebUI cleanup is unconfirmed and the Client retained ownership: {stop}"
                )
                .into())
            }
        }
    }

    async fn start_webui(&self, request: Start, cancel: CancellationToken) -> Result<Value> {
        let account_file = request
            .account_file
            .unwrap_or_else(|| self.inner.data_dir.join("web-account.json"));
        let mut state = self.inner.state.lock().await;
        reap(&mut state)?;
        self.persist_cleanup(&cleanup_snapshot(&state))?;
        if !state.failed_web.is_empty() {
            return Err(
                "a previous WebUI cleanup is unconfirmed; stop or inspect it before starting another"
                    .into(),
            );
        }
        if let Some(webui) = state.webui.as_ref() {
            if request.lan.is_some() && request.lan != webui.lan {
                return Err(
                    "existing WebUI uses different LAN settings; stop it before changing listeners"
                        .into(),
                );
            }
            if account_file != webui.account_file {
                return Err(
                    "existing WebUI uses a different account file; stop it before changing policy"
                        .into(),
                );
            }
            let mut ready = webui.ready.clone();
            if request.lan.is_none() && webui.lan.is_some() {
                ready["lan_url"] = ready["url"].clone();
                ready["url"] = ready["local_url"].clone();
                ready["access"] = json!("local");
                ready["login_required"] = json!(false);
            }
            ready["plugin"] = json!("webui");
            ready["role"] = json!("client");
            ready["kind"] = json!("background");
            ready["launch_id"] = json!(webui.launch_id);
            ready["workspace_id"] = json!(request.workspace_id);
            ready["startup_workspace_id"] = json!(webui.workspace_id);
            ready["open_url"] = scoped_open_url(&ready, &request.workspace_id)?;
            ready["reused"] = json!(true);
            return Ok(ready);
        }

        let launch_id = uuid::Uuid::new_v4().to_string();
        let mut command = Command::new(&self.inner.webui_binary);
        command
            .env_clear()
            .env("CAYW_IPC_ADDR", self.inner.address.to_string())
            .env("CAYW_IPC_TOKEN", &self.inner.token)
            .env("CAYW_WORKSPACE_ID", &request.workspace_id)
            .env("CAYW_CLIENT_SUPERVISED", "1")
            .env(
                "CAYW_OWNERSHIP_PATH",
                self.inner.data_dir.join("ownership.json"),
            )
            .env("CAYW_WEB_ADDR", "127.0.0.1:0")
            .arg("--account-file")
            .arg(&account_file)
            .current_dir(&self.inner.data_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped());
        if let Some(lan) = &request.lan {
            command.arg("--lan").arg(lan);
        }
        command.as_std_mut().process_group(0);
        let mut child = command.spawn()?;
        let pid = child.id().ok_or("WebUI process identity unavailable")?;
        let identity = match process_identity(pid).await? {
            Some(identity) => identity,
            None => {
                let _ = child.wait().await;
                return Err("WebUI exited before process identity was recorded".into());
            }
        };
        let cleanup = CleanupProcess {
            launch_id: launch_id.clone(),
            plugin: "webui".into(),
            workspace_id: request.workspace_id.clone(),
            process: identity.clone(),
        };
        let mut intended = cleanup_snapshot(&state);
        intended.push(cleanup.clone());
        if let Err(persist) = self.persist_cleanup(&intended) {
            return match stop_child(&mut child).await {
                Ok(()) => Err(format!(
                    "WebUI ownership record could not be saved; the process was stopped: {persist}"
                )
                .into()),
                Err(stop) => {
                    state.failed_web.insert(
                        launch_id.clone(),
                        FailedWeb {
                            child,
                            cleanup,
                            error: format!("ownership record failed: {persist}"),
                        },
                    );
                    Err(format!(
                        "WebUI ownership record and process cleanup both failed; the Client retained PID {pid}: {persist}; {stop}"
                    )
                    .into())
                }
            };
        }
        let mut lines =
            BufReader::new(child.stdout.take().ok_or("WebUI stdout unavailable")?).lines();
        let line = tokio::select! {
            _ = cancel.cancelled() => Err("Client plugin start was cancelled".to_owned()),
            line = tokio::time::timeout(START_TIMEOUT, lines.next_line()) => {
                match line {
                    Ok(Ok(Some(line))) => Ok(line),
                    Ok(Ok(None)) => Err("WebUI exited before readiness".to_owned()),
                    Ok(Err(error)) => Err(error.to_string()),
                    Err(_) => Err("WebUI did not report readiness".to_owned()),
                }
            }
        };
        let line = match line {
            Ok(line) => line,
            Err(reason) => {
                return self
                    .abort_web_start(&mut state, child, cleanup, reason)
                    .await;
            }
        };
        let ready: Value = match serde_json::from_str(&line) {
            Ok(ready) => ready,
            Err(error) => {
                return self
                    .abort_web_start(
                        &mut state,
                        child,
                        cleanup,
                        format!("invalid WebUI readiness: {error}"),
                    )
                    .await;
            }
        };
        for field in ["url", "access", "login_required", "local_url"] {
            if ready.get(field).is_none() {
                return self
                    .abort_web_start(
                        &mut state,
                        child,
                        cleanup,
                        format!("WebUI readiness is missing {field}"),
                    )
                    .await;
            }
        }
        state.webui = Some(WebUi {
            child,
            identity,
            launch_id: launch_id.clone(),
            workspace_id: request.workspace_id.clone(),
            lan: request.lan,
            account_file,
            ready: ready.clone(),
        });
        self.persist_cleanup(&cleanup_snapshot(&state))?;
        let mut response = ready;
        response["plugin"] = json!("webui");
        response["role"] = json!("client");
        response["kind"] = json!("background");
        response["launch_id"] = json!(launch_id);
        response["workspace_id"] = json!(request.workspace_id.clone());
        response["startup_workspace_id"] = json!(request.workspace_id.clone());
        response["open_url"] = scoped_open_url(&response, &request.workspace_id)?;
        response["reused"] = json!(false);
        Ok(response)
    }

    async fn attach(&self, request: Attach) -> Result<Value> {
        if request.pid <= 1 || request.pid > i32::MAX as u32 {
            return Err("invalid foreground plugin pid".into());
        }
        let mut state = self.inner.state.lock().await;
        reap(&mut state)?;
        let pending = state
            .pending_tui
            .remove(&request.launch_id)
            .ok_or("unknown or expired TUI launch")?;
        if pending.created.elapsed() > PREPARE_TIMEOUT {
            return Err("TUI launch record expired before attach".into());
        }
        let Some(identity) = validate_tui_process(request.pid, &self.inner.tui_binary).await? else {
            // The foreground UI can connect and quit before this RPC finishes
            // inspecting it. An exited child is not an impersonating live process.
            // No supervision identity is granted; its launcher must reap the
            // original child and decide success from that child's exit status.
            record_terminal(
                &mut state,
                "tui",
                &request.launch_id,
                &pending.workspace_id,
                request.pid,
                "exited_before_attach",
                None,
            );
            self.persist_cleanup(&cleanup_snapshot(&state))?;
            return Ok(json!({
                "plugin":"tui",
                "launch_id":request.launch_id,
                "pid":request.pid,
                "supervised":false,
                "exited_before_attach":true,
            }));
        };
        state.tui.insert(
            request.launch_id.clone(),
            Tui {
                workspace_id: pending.workspace_id.clone(),
                identity,
            },
        );
        self.persist_cleanup(&cleanup_snapshot(&state))?;
        Ok(json!({
            "plugin":"tui",
            "role":"client",
            "kind":"foreground",
            "launch_id":request.launch_id,
            "workspace_id":pending.workspace_id,
            "pid":request.pid,
            "supervised":true,
        }))
    }

    async fn detach(&self, request: Detach) -> Result<Value> {
        let mut state = self.inner.state.lock().await;
        reap(&mut state)?;
        let detached = match state.tui.get(&request.launch_id) {
            Some(tui) if tui.identity.pid == request.pid => {
                let tui = state.tui.remove(&request.launch_id).expect("matched TUI");
                record_terminal(
                    &mut state,
                    "tui",
                    &request.launch_id,
                    &tui.workspace_id,
                    request.pid,
                    "detached",
                    None,
                );
                true
            }
            Some(_) => return Err("TUI launch identity does not match pid".into()),
            None => false,
        };
        self.persist_cleanup(&cleanup_snapshot(&state))?;
        Ok(json!({"detached":detached,"launch_id":request.launch_id}))
    }

    async fn stop(&self, request: Stop) -> Result<Value> {
        let mut state = self.inner.state.lock().await;
        reap(&mut state)?;
        if state.pending_tui.remove(&request.launch_id).is_some() {
            return Ok(json!({"stopped":true,"launch_id":request.launch_id,"pending":true}));
        }
        self.persist_cleanup(&cleanup_snapshot(&state))?;
        if state
            .webui
            .as_ref()
            .is_some_and(|webui| webui.launch_id == request.launch_id)
        {
            let result = stop_child(&mut state.webui.as_mut().expect("matched WebUI").child).await;
            if let Err(error) = result {
                return Err(format!(
                    "WebUI cleanup is unconfirmed; Client ownership was retained: {error}"
                )
                .into());
            }
            let webui = state.webui.take().expect("stopped WebUI");
            record_terminal(
                &mut state,
                "webui",
                &webui.launch_id,
                &webui.workspace_id,
                webui.identity.pid,
                "stopped",
                None,
            );
            self.persist_cleanup(&cleanup_snapshot(&state))?;
            return Ok(json!({"stopped":true,"launch_id":request.launch_id}));
        }
        if let Some(failed) = state.failed_web.get_mut(&request.launch_id) {
            if let Err(error) = stop_child(&mut failed.child).await {
                return Err(format!(
                    "failed WebUI cleanup remains unconfirmed; Client ownership was retained: {error}"
                )
                .into());
            }
            let failed = state
                .failed_web
                .remove(&request.launch_id)
                .expect("stopped failed WebUI");
            record_terminal(
                &mut state,
                "webui",
                &request.launch_id,
                &failed.cleanup.workspace_id,
                failed.cleanup.process.pid,
                "stopped",
                Some(failed.error),
            );
            self.persist_cleanup(&cleanup_snapshot(&state))?;
            return Ok(json!({"stopped":true,"launch_id":request.launch_id}));
        }
        if let Some(tui) = state.tui.get(&request.launch_id) {
            let workspace_id = tui.workspace_id.clone();
            let identity = tui.identity.clone();
            if let Err(error) = stop_pid(&identity).await {
                return Err(format!(
                    "TUI cleanup is unconfirmed; Client ownership was retained: {error}"
                )
                .into());
            }
            state.tui.remove(&request.launch_id);
            record_terminal(
                &mut state,
                "tui",
                &request.launch_id,
                &workspace_id,
                identity.pid,
                "stopped",
                None,
            );
            self.persist_cleanup(&cleanup_snapshot(&state))?;
            return Ok(json!({"stopped":true,"launch_id":request.launch_id}));
        }
        Ok(json!({"stopped":false,"launch_id":request.launch_id}))
    }

    async fn list(&self) -> Result<Value> {
        let mut state = self.inner.state.lock().await;
        reap(&mut state)?;
        self.persist_cleanup(&cleanup_snapshot(&state))?;
        Ok(state_summary(&state))
    }
}

fn state_summary(state: &State) -> Value {
    let mut entries = Vec::new();
    if let Some(webui) = &state.webui {
        entries.push(json!({
            "plugin":"webui",
            "role":"client",
            "kind":"background",
            "launch_id":webui.launch_id,
            "workspace_id":webui.workspace_id,
            "pid":webui.identity.pid,
            "access":webui.ready["access"],
            "url":webui.ready["url"],
            "local_url":webui.ready["local_url"],
            "status":"running",
        }));
    }
    entries.extend(state.failed_web.values().map(|failed| {
        json!({
            "plugin":"webui",
            "role":"client",
            "kind":"background",
            "launch_id":failed.cleanup.launch_id,
            "workspace_id":failed.cleanup.workspace_id,
            "pid":failed.cleanup.process.pid,
            "status":"cleanup_unknown",
            "last_error":failed.error,
        })
    }));
    entries.extend(state.tui.iter().map(|(launch_id, tui)| {
        json!({
            "plugin":"tui",
            "role":"client",
            "kind":"foreground",
            "launch_id":launch_id,
            "workspace_id":tui.workspace_id,
            "pid":tui.identity.pid,
            "status":"running",
        })
    }));
    json!({
        "schema":"cayw.client-plugins.v1",
        "entries":entries,
        "recent":state.recent,
    })
}

fn parse<T: serde::de::DeserializeOwned>(params: Value) -> std::result::Result<T, RpcError> {
    serde_json::from_value(params).map_err(|error| {
        RpcError::new(
            "invalid_params",
            format!("invalid Client plugin request: {error}"),
        )
    })
}

fn client_plugin_error(error: Box<dyn Error + Send + Sync>) -> RpcError {
    RpcError::new("client_plugin_error", error.to_string())
}

fn validate_workspace_id(id: &str) -> Result<()> {
    if id == "local" {
        return Ok(());
    }
    let digest = id.strip_prefix("workspace-");
    if digest.is_some_and(|digest| {
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    }) {
        Ok(())
    } else {
        Err("invalid local workspace ID".into())
    }
}

fn scoped_open_url(ready: &Value, workspace_id: &str) -> Result<Value> {
    let base = ready["local_url"]
        .as_str()
        .or_else(|| ready["url"].as_str())
        .ok_or("WebUI readiness is missing a browser URL")?
        .split('#')
        .next()
        .expect("split always has one part");
    Ok(json!(format!("{base}#workspace={workspace_id}")))
}

fn cleanup_snapshot(state: &State) -> Vec<CleanupProcess> {
    let mut processes = Vec::new();
    if let Some(webui) = &state.webui {
        processes.push(CleanupProcess {
            launch_id: webui.launch_id.clone(),
            plugin: "webui".into(),
            workspace_id: webui.workspace_id.clone(),
            process: webui.identity.clone(),
        });
    }
    processes.extend(
        state
            .failed_web
            .values()
            .map(|failed| failed.cleanup.clone()),
    );
    processes.extend(state.tui.iter().map(|(launch_id, tui)| CleanupProcess {
        launch_id: launch_id.clone(),
        plugin: "tui".into(),
        workspace_id: tui.workspace_id.clone(),
        process: tui.identity.clone(),
    }));
    processes
}

fn persist_cleanup(path: &Path, processes: &[CleanupProcess]) -> Result<()> {
    let previous = match read_private(path, 1024 * 1024) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    if processes.is_empty() {
        if previous.is_some() {
            std::fs::remove_file(path)?;
            if let Some(parent) = path.parent() {
                std::fs::File::open(parent)?.sync_all()?;
            }
        }
        return Ok(());
    }
    let ledger = CleanupLedger {
        schema: CLEANUP_SCHEMA.into(),
        processes: processes.to_vec(),
    };
    let mut bytes = serde_json::to_vec_pretty(&ledger)?;
    bytes.push(b'\n');
    save_config(path, previous.as_deref(), &bytes)?;
    Ok(())
}

pub(crate) fn inspect_cleanup(path: &Path) -> Result<Value> {
    let bytes = match read_private(path, 1024 * 1024) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(json!({"exists":false,"path":path}));
        }
        Err(error) => return Err(error.into()),
    };
    let ledger: CleanupLedger = serde_json::from_slice(&bytes)?;
    if ledger.schema != CLEANUP_SCHEMA {
        return Err("Client plugin cleanup record has an unsupported schema".into());
    }
    let mut live_processes = 0_usize;
    let mut processes = Vec::with_capacity(ledger.processes.len());
    for entry in ledger.processes {
        validate_workspace_id(&entry.workspace_id)?;
        if !matches!(entry.plugin.as_str(), "webui" | "tui") || entry.launch_id.trim().is_empty() {
            return Err("Client plugin cleanup record is invalid".into());
        }
        let live = identity_matches_sync(&entry.process)?;
        live_processes += usize::from(live);
        processes.push(json!({
            "plugin":entry.plugin,
            "launch_id":entry.launch_id,
            "workspace_id":entry.workspace_id,
            "pid":entry.process.pid,
            "live":live,
        }));
    }
    Ok(json!({
        "exists":true,
        "path":path,
        "schema":CLEANUP_SCHEMA,
        "processes":processes,
        "live_processes":live_processes,
    }))
}

fn reconcile_cleanup(path: &Path) -> Result<()> {
    let bytes = match read_private(path, 1024 * 1024) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let ledger: CleanupLedger = serde_json::from_slice(&bytes)?;
    if ledger.schema != CLEANUP_SCHEMA {
        return Err("Client plugin cleanup record has an unsupported schema".into());
    }
    let mut live = Vec::new();
    for entry in &ledger.processes {
        validate_workspace_id(&entry.workspace_id)?;
        if !matches!(entry.plugin.as_str(), "webui" | "tui") || entry.launch_id.trim().is_empty() {
            return Err("Client plugin cleanup record is invalid".into());
        }
        if identity_matches_sync(&entry.process)? {
            live.push(format!("{} PID {}", entry.plugin, entry.process.pid));
        }
    }
    if !live.is_empty() {
        return Err(format!(
            "previous Client plugin cleanup is unconfirmed ({}); stop or inspect those exact processes, then retry",
            live.join(", ")
        )
        .into());
    }
    std::fs::remove_file(path)?;
    if let Some(parent) = path.parent() {
        std::fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

fn record_terminal(
    state: &mut State,
    plugin: &str,
    launch_id: &str,
    workspace_id: &str,
    pid: u32,
    status: &str,
    error: Option<String>,
) {
    state.recent.push_back(json!({
        "plugin":plugin,
        "role":"client",
        "launch_id":launch_id,
        "workspace_id":workspace_id,
        "pid":pid,
        "status":status,
        "last_error":error,
    }));
    while state.recent.len() > RECENT_LIMIT {
        state.recent.pop_front();
    }
}

fn reap(state: &mut State) -> Result<()> {
    let expired = state
        .pending_tui
        .iter()
        .filter(|(_, pending)| pending.created.elapsed() > PREPARE_TIMEOUT)
        .map(|(launch_id, pending)| (launch_id.clone(), pending.workspace_id.clone()))
        .collect::<Vec<_>>();
    for (launch_id, workspace_id) in expired {
        state.pending_tui.remove(&launch_id);
        record_terminal(
            state,
            "tui",
            &launch_id,
            &workspace_id,
            0,
            "prepare_expired",
            None,
        );
    }

    let exited_tui = state
        .tui
        .iter()
        .map(|(launch_id, tui)| {
            Ok((
                launch_id.clone(),
                tui.workspace_id.clone(),
                tui.identity.clone(),
                identity_matches_sync(&tui.identity)?,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    for (launch_id, workspace_id, identity, live) in exited_tui {
        if !live {
            state.tui.remove(&launch_id);
            record_terminal(
                state,
                "tui",
                &launch_id,
                &workspace_id,
                identity.pid,
                "exited",
                None,
            );
        }
    }

    let webui_exit = match state.webui.as_mut() {
        Some(webui) => webui.child.try_wait()?,
        None => None,
    };
    if let Some(status) = webui_exit {
        let webui = state.webui.take().expect("exited WebUI");
        record_terminal(
            state,
            "webui",
            &webui.launch_id,
            &webui.workspace_id,
            webui.identity.pid,
            "exited",
            (!status.success()).then(|| format!("process exited with {status}")),
        );
    }

    let failed_exits = state
        .failed_web
        .iter_mut()
        .map(|(launch_id, failed)| {
            Ok((
                launch_id.clone(),
                failed.child.try_wait()?,
                failed.cleanup.clone(),
                failed.error.clone(),
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    for (launch_id, status, cleanup, error) in failed_exits {
        if status.is_some() {
            state.failed_web.remove(&launch_id);
            record_terminal(
                state,
                "webui",
                &launch_id,
                &cleanup.workspace_id,
                cleanup.process.pid,
                "cleanup_completed",
                Some(error),
            );
        }
    }
    Ok(())
}

fn process_field_sync(pid: u32, field: &str) -> Result<Option<String>> {
    if pid <= 1 || pid > i32::MAX as u32 {
        return Ok(None);
    }
    let output = StdCommand::new("/bin/ps")
        .arg("-p")
        .arg(pid.to_string())
        .args(["-o", field])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()?;
    if !output.status.success() {
        return Ok(None);
    }
    let value = String::from_utf8(output.stdout)?.trim().to_owned();
    Ok((!value.is_empty()).then_some(value))
}

async fn process_field(pid: u32, field: &str) -> Result<Option<String>> {
    if pid <= 1 || pid > i32::MAX as u32 {
        return Ok(None);
    }
    let output = Command::new("/bin/ps")
        .arg("-p")
        .arg(pid.to_string())
        .args(["-o", field])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .await?;
    if !output.status.success() {
        return Ok(None);
    }
    let value = String::from_utf8(output.stdout)?.trim().to_owned();
    Ok((!value.is_empty()).then_some(value))
}

fn process_identity_sync(pid: u32) -> Result<Option<ProcessIdentity>> {
    let Some(started) = process_field_sync(pid, "lstart=")? else {
        return Ok(None);
    };
    let Some(command) = process_field_sync(pid, "comm=")? else {
        return Ok(None);
    };
    if process_field_sync(pid, "lstart=")?.as_deref() != Some(started.as_str()) {
        return Ok(None);
    }
    Ok(Some(ProcessIdentity {
        pid,
        started,
        command,
    }))
}

async fn process_identity(pid: u32) -> Result<Option<ProcessIdentity>> {
    let Some(started) = process_field(pid, "lstart=").await? else {
        return Ok(None);
    };
    let Some(command) = process_field(pid, "comm=").await? else {
        return Ok(None);
    };
    if process_field(pid, "lstart=").await?.as_deref() != Some(started.as_str()) {
        return Ok(None);
    }
    Ok(Some(ProcessIdentity {
        pid,
        started,
        command,
    }))
}

fn identity_matches_sync(identity: &ProcessIdentity) -> Result<bool> {
    Ok(process_identity_sync(identity.pid)?.as_ref() == Some(identity))
}

async fn identity_matches(identity: &ProcessIdentity) -> Result<bool> {
    Ok(process_identity(identity.pid).await?.as_ref() == Some(identity))
}

async fn tui_process_has_exited(pid: u32) -> Result<bool> {
    // macOS may report a zombie's comm as `(cayw-tui)` rather than its
    // executable path. It is already dead, even while its launcher has not
    // reaped it. A live wrong executable must still fail identity validation.
    let output = Command::new("/bin/ps")
        .arg("-p")
        .arg(pid.to_string())
        .args(["-o", "stat="])
        .stdin(Stdio::null())
        .output()
        .await?;
    tui_exit_state(output)
}

fn tui_exit_state(output: std::process::Output) -> Result<bool> {
    let state = String::from_utf8(output.stdout)?;
    if output.status.success() && output.stderr.is_empty() && !state.trim().is_empty() {
        return Ok(state.trim().starts_with('Z'));
    }
    // ps returns 1 and no output when the selected PID no longer exists. Do
    // not confuse an inspection error, malformed output or signal with death.
    if output.status.code() == Some(1) && state.trim().is_empty() && output.stderr.is_empty() {
        return Ok(true);
    }
    Err(format!(
        "could not confirm TUI process state ({}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    )
    .into())
}

async fn validate_tui_process(pid: u32, expected: &Path) -> Result<Option<ProcessIdentity>> {
    let identity = process_identity(pid).await?;
    if tui_process_has_exited(pid).await? {
        return Ok(None);
    }
    let identity = identity.ok_or("TUI process identity changed before Client supervision")?;
    let expected_name = expected
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("invalid TUI binary path")?;
    if identity.command != expected.to_string_lossy()
        && !identity.command.ends_with(&format!("/{expected_name}"))
    {
        return Err("foreground process is not the packaged cayw-tui binary".into());
    }
    Ok(Some(identity))
}

async fn signal(pid: u32) -> Result<()> {
    let status = Command::new("/bin/kill")
        .arg("-INT")
        .arg(pid.to_string())
        .status()
        .await?;
    if !status.success() && process_identity(pid).await?.is_some() {
        return Err("could not signal Client-owned plugin; shutdown is unconfirmed".into());
    }
    Ok(())
}

async fn stop_pid(identity: &ProcessIdentity) -> Result<()> {
    if !identity_matches(identity).await? {
        return Ok(());
    }
    signal(identity.pid).await?;
    let deadline = tokio::time::Instant::now() + STOP_TIMEOUT;
    while identity_matches(identity).await? {
        if tokio::time::Instant::now() >= deadline {
            return Err(
                "Client-owned plugin did not confirm shutdown; inspect it before restarting".into(),
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Ok(())
}

async fn stop_child(child: &mut Child) -> Result<()> {
    if child.try_wait()?.is_some() {
        return Ok(());
    }
    let pid = child
        .id()
        .ok_or("Client-owned child identity unavailable")?;
    signal(pid).await?;
    match tokio::time::timeout(STOP_TIMEOUT, child.wait()).await {
        Ok(status) => {
            status?;
            Ok(())
        }
        Err(_) => {
            Err("Client-owned plugin did not confirm shutdown; inspect it before restarting".into())
        }
    }
}

// These exercise macOS local-product supervision, including the executable
// paths reported by macOS `ps`; Linux CI covers the Device identity contract.
#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn fixture_plugins(data_dir: &Path, webui: PathBuf, tui: PathBuf) -> ClientPlugins {
        ClientPlugins::with_binaries(
            "127.0.0.1:43210".parse().unwrap(),
            "synthetic-client-token-never-log".into(),
            data_dir.to_owned(),
            webui,
            tui,
        )
        .unwrap()
    }

    fn webui_fixture(path: &Path) {
        std::fs::write(
            path,
            "#!/bin/sh\ntrap 'exit 0' INT TERM\nprintf '%s\\n' '{\"url\":\"http://127.0.0.1:44100/\",\"access\":\"local\",\"login_required\":false,\"local_url\":\"http://127.0.0.1:44100/\"}'\nwhile :; do /bin/sleep 1; done\n",
        )
        .unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[tokio::test]
    async fn webui_is_one_client_owned_process_without_launcher_files() {
        let temp = tempfile::tempdir().unwrap();
        let webui = temp.path().join("cayw-webui");
        webui_fixture(&webui);
        let plugins = fixture_plugins(temp.path(), webui.clone(), temp.path().join("missing-tui"));
        let first = plugins
            .call(
                "client.plugins.start",
                json!({"plugin":"webui","workspace_id":"local"}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(first["role"], "client");
        assert_eq!(first["kind"], "background");
        assert_eq!(first["reused"], false);
        let launch_id = first["launch_id"].as_str().unwrap().to_owned();

        let second_workspace = format!("workspace-{}", "0".repeat(64));
        let reused = plugins
            .call(
                "client.plugins.start",
                json!({"plugin":"webui","workspace_id":second_workspace}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(reused["launch_id"], launch_id);
        assert_eq!(reused["reused"], true);
        assert_eq!(reused["startup_workspace_id"], "local");
        assert_eq!(reused["workspace_id"], second_workspace);

        let listed = plugins
            .call("client.plugins.list", json!({}), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(listed["schema"], "cayw.client-plugins.v1");
        assert_eq!(listed["entries"].as_array().unwrap().len(), 1);
        assert_eq!(listed["entries"][0]["launch_id"], launch_id);
        let cleanup_path = temp.path().join(CLEANUP_FILE);
        let cleanup_bytes = std::fs::read(&cleanup_path).unwrap();
        assert!(!String::from_utf8_lossy(&cleanup_bytes).contains("synthetic-client-token"));
        assert_eq!(
            serde_json::from_slice::<Value>(&cleanup_bytes).unwrap()["schema"],
            CLEANUP_SCHEMA
        );
        assert_eq!(
            std::fs::metadata(&cleanup_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let refused = ClientPlugins::with_binaries(
            "127.0.0.1:43211".parse().unwrap(),
            "other-synthetic-token-never-log".into(),
            temp.path().to_owned(),
            webui,
            temp.path().join("missing-tui"),
        )
        .err()
        .expect("a new Client must not take over a live process");
        assert!(refused.to_string().contains("cleanup is unconfirmed"));
        assert!(std::fs::read_dir(temp.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("launcher-")
        }));

        let stopped = plugins
            .call(
                "client.plugins.stop",
                json!({"launch_id":launch_id}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(stopped["stopped"], true);
        let summary = plugins.read_only_summary().await;
        assert_eq!(summary["entries"], json!([]));
        assert_eq!(summary["recent"][0]["status"], "stopped");
        assert!(!cleanup_path.exists());
        plugins.shutdown().await.unwrap();
    }

    #[test]
    fn supervised_tui_fixture_process() {
        if std::env::var_os("CAYW_TEST_TUI_CHILD").is_some() {
            loop {
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    }

    #[test]
    fn tui_exit_state_rejects_inspection_errors() {
        use std::os::unix::process::ExitStatusExt;
        let output = |code, stdout: &[u8], stderr: &[u8]| std::process::Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: stdout.to_vec(),
            stderr: stderr.to_vec(),
        };
        assert!(tui_exit_state(output(1, b"", b"")).unwrap());
        assert!(tui_exit_state(output(0, b"Z+\n", b"")).unwrap());
        assert!(!tui_exit_state(output(0, b"S+\n", b"")).unwrap());
        assert!(tui_exit_state(output(1, b"", b"inspection denied")).is_err());
        assert!(tui_exit_state(output(0, b"Z+\n", b"inspection warning")).is_err());
        assert!(tui_exit_state(output(2, b"", b"")).is_err());
        assert!(tui_exit_state(output(0, b"", b"")).is_err());
        assert!(tui_exit_state(output(0, b"\xff", b"")).is_err());
    }

    async fn assert_early_exit_is_not_supervised(reap_first: bool) {
        let temp = tempfile::tempdir().unwrap();
        let plugins = fixture_plugins(
            temp.path(),
            temp.path().join("missing-webui"),
            temp.path().join("cayw-tui"),
        );
        let prepared = plugins
            .call(
                "client.plugins.start",
                json!({"plugin":"tui","workspace_id":"local"}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let launch_id = prepared["launch_id"].as_str().unwrap();
        // std::process retains an unreaped child for the zombie case. Synchronize
        // on its actual OS state, not a fixed sleep or an assumed exit delay.
        let mut child = StdCommand::new("/bin/sh")
            .args(["-c", "exit 7"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        if reap_first {
            assert_eq!(child.wait().unwrap().code(), Some(7));
        } else {
            tokio::time::timeout(Duration::from_secs(30), async {
                loop {
                    if process_field(pid, "stat=")
                        .await
                        .unwrap()
                        .is_some_and(|state| state.starts_with('Z'))
                    {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("fixture must reach a known zombie state");
        }
        let attached = plugins
            .call(
                "client.plugins.attach",
                json!({"launch_id":launch_id,"pid":pid}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(attached["supervised"], false);
        assert_eq!(attached["exited_before_attach"], true);
        assert_eq!(attached["pid"], pid);
        assert_eq!(child.wait().unwrap().code(), Some(7));
        let summary = plugins.read_only_summary().await;
        assert_eq!(summary["entries"], json!([]));
        assert_eq!(summary["recent"][0]["status"], "exited_before_attach");
        assert!(plugins.inner.state.lock().await.pending_tui.is_empty());
        assert!(!temp.path().join(CLEANUP_FILE).exists());
    }

    #[tokio::test]
    async fn reaped_tui_before_attach_does_not_claim_supervision() {
        assert_early_exit_is_not_supervised(true).await;
    }

    #[tokio::test]
    async fn zombie_tui_before_attach_does_not_claim_supervision() {
        assert_early_exit_is_not_supervised(false).await;
    }

    #[tokio::test]
    async fn live_wrong_tui_executable_is_still_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let plugins = fixture_plugins(
            temp.path(),
            temp.path().join("missing-webui"),
            temp.path().join("cayw-tui"),
        );
        let prepared = plugins
            .call(
                "client.plugins.start",
                json!({"plugin":"tui","workspace_id":"local"}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        // This current test process is alive and is not the packaged cayw-tui.
        let error = plugins
            .call(
                "client.plugins.attach",
                json!({"launch_id":prepared["launch_id"],"pid":std::process::id()}),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(error.message.contains("not the packaged cayw-tui binary"));
        assert_eq!(plugins.read_only_summary().await["entries"], json!([]));
        assert!(!temp.path().join(CLEANUP_FILE).exists());
    }

    #[tokio::test]
    async fn foreground_tui_handoff_is_registered_and_stoppable() {
        let temp = tempfile::tempdir().unwrap();
        let executable = std::env::current_exe().unwrap();
        let plugins = fixture_plugins(
            temp.path(),
            temp.path().join("missing-webui"),
            executable.clone(),
        );
        let prepared = plugins
            .call(
                "client.plugins.start",
                json!({"plugin":"tui","workspace_id":"local"}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let launch_id = prepared["launch_id"].as_str().unwrap().to_owned();
        assert_eq!(prepared["attach_required"], true);

        let mut child = Command::new(&executable)
            .args([
                "--exact",
                "cli_client_plugins::tests::supervised_tui_fixture_process",
                "--nocapture",
            ])
            .env("CAYW_TEST_TUI_CHILD", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();
        let attached = plugins
            .call(
                "client.plugins.attach",
                json!({"launch_id":launch_id,"pid":pid}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(attached["supervised"], true);
        assert_eq!(attached["pid"], pid);

        let stop = plugins.call(
            "client.plugins.stop",
            json!({"launch_id":launch_id}),
            CancellationToken::new(),
        );
        let (stopped, status) = tokio::join!(stop, child.wait());
        assert_eq!(stopped.unwrap()["stopped"], true);
        assert!(!status.unwrap().success());
        assert_eq!(plugins.read_only_summary().await["entries"], json!([]));
    }
}
