//! Foreground entrypoint for the unique local Client and its built-in UI plugins.
use cayw_ipc::Client;
use cayw_runtime::local_cli::LocalPaths;
use serde_json::{Value, json};
use std::{
    error::Error,
    net::SocketAddr,
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
    signal::unix::{Signal, SignalKind},
    time::{Instant, sleep_until},
};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

pub(super) struct ShutdownSignals {
    interrupt: Signal,
    terminate: Signal,
}

impl ShutdownSignals {
    pub(super) fn new() -> std::io::Result<Self> {
        Ok(Self {
            interrupt: tokio::signal::unix::signal(SignalKind::interrupt())?,
            terminate: tokio::signal::unix::signal(SignalKind::terminate())?,
        })
    }

    pub(super) async fn recv(&mut self) {
        tokio::select! {
            _ = self.interrupt.recv() => {}
            _ = self.terminate.recv() => {}
        }
    }
}

const HELP: &str = "cayw start [--workspace DIR] [--ui web|tui|none] [--offline] [--demo-authorized] [--config PATH] [--no-open] [--lan IP:PORT] [--account-file FILE]\n\nOne foreground command starts or reuses the unique local Client and opens the requested workspace.\nThe default UI is TUI. Select --ui web to ask the Client to start the optional local WebUI.\nTUI keeps the caller's foreground terminal, then hands its process identity to the Client for supervision.\n--no-open, SSH and headless web launches print the URL. --lan IP:PORT allows LAN access; an optional configured account enables password login.\nCtrl+C stops only a UI or Client created by this command. Existing services are kept.\nUse --workspace to choose another project. No model request is made by startup.";

pub async fn run(mut args: impl Iterator<Item = String>) -> Result<()> {
    let mut workspace = std::env::current_dir()?;
    let mut ui = "tui".to_string();
    let mut extra = Vec::new();
    let mut no_open = false;
    let mut lan = None;
    let mut account_file = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                println!("{HELP}");
                return Ok(());
            }
            "--workspace" => workspace = PathBuf::from(args.next().ok_or("missing workspace")?),
            "--ui" => ui = args.next().ok_or("missing UI")?,
            "--no-open" => no_open = true,
            "--lan" if lan.is_none() => lan = Some(args.next().ok_or("--lan requires IP:PORT")?),
            "--account-file" if account_file.is_none() => {
                account_file = Some(PathBuf::from(
                    args.next().ok_or("--account-file requires FILE")?,
                ))
            }
            "--offline" | "--demo-authorized" | "--pi-compat" => extra.push(arg),
            "--config" => {
                extra.push(arg);
                extra.push(args.next().ok_or("missing config path")?);
            }
            _ => return Err(format!("unexpected start argument {arg}").into()),
        }
    }
    if !matches!(ui.as_str(), "web" | "tui" | "none") {
        return Err("--ui must be web, tui or none".into());
    }
    if (lan.is_some() || account_file.is_some()) && ui != "web" {
        return Err("--lan and --account-file require explicit --ui web".into());
    }
    let mut signals = ShutdownSignals::new()?;
    let paths = LocalPaths::from_environment(workspace)?;
    paths.initialize()?;
    let account_file = match account_file {
        Some(path) if path.is_absolute() => path,
        Some(path) => std::env::current_dir()?.join(path),
        None => paths.data_dir.join("web-account.json"),
    };
    let options = UiOptions {
        ui,
        no_open,
        lan,
        account_file,
    };
    let Some(mut connection) = connect_or_start(&paths, &extra, &mut signals).await? else {
        return Ok(());
    };
    let result = start(
        &paths,
        &options,
        connection.address,
        &connection.token,
        connection.child.as_mut(),
        &mut signals,
    )
    .await;
    let cleanup = match connection.child.as_mut() {
        Some(child) => stop_child(child).await,
        None => Ok(()),
    };
    match (result, cleanup) {
        (Err(start), Err(cleanup)) => {
            Err(format!("{start}; Client shutdown also was not confirmed: {cleanup}").into())
        }
        (Err(start), _) => Err(start),
        (_, Err(cleanup)) => Err(cleanup),
        _ => Ok(()),
    }
}

struct ClientConnection {
    address: SocketAddr,
    token: String,
    child: Option<Child>,
}

fn existing_connection(
    address: SocketAddr,
    token: String,
    extra: &[String],
) -> Result<ClientConnection> {
    if !extra.is_empty() {
        return Err("Client already running: startup overrides cannot be applied to it; stop that Client first".into());
    }
    Ok(ClientConnection {
        address,
        token,
        child: None,
    })
}

async fn connect_or_start(
    paths: &LocalPaths,
    extra: &[String],
    signals: &mut ShutdownSignals,
) -> Result<Option<ClientConnection>> {
    let existing = tokio::select! {
        result = healthy(paths) => Some(result),
        _ = signals.recv() => None,
    };
    let Some(existing) = existing else {
        return Ok(None);
    };
    if let Ok((address, token)) = existing {
        return existing_connection(address, token, extra).map(Some);
    }

    let startup_deadline = Instant::now() + Duration::from_secs(30);
    let _start_lock = loop {
        if let Some(lock) = paths.try_acquire_client_start()? {
            break lock;
        }
        let existing = tokio::select! {
            result = healthy(paths) => Some(result),
            _ = signals.recv() => None,
        };
        let Some(existing) = existing else {
            return Ok(None);
        };
        if let Ok((address, token)) = existing {
            return existing_connection(address, token, extra).map(Some);
        }
        if Instant::now() >= startup_deadline {
            return Err("another command started the Client but it did not become healthy".into());
        }
        let retry = (Instant::now() + Duration::from_millis(50)).min(startup_deadline);
        tokio::select! {
            _ = sleep_until(retry) => {}
            _ = signals.recv() => return Ok(None),
        }
    };

    // Another command may have finished startup immediately before releasing the lock.
    let existing = tokio::select! {
        result = healthy(paths) => Some(result),
        _ = signals.recv() => None,
    };
    let Some(existing) = existing else {
        return Ok(None);
    };
    if let Ok((address, token)) = existing {
        return existing_connection(address, token, extra).map(Some);
    }

    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("serve")
        .arg("--workspace")
        .arg(&paths.workspace)
        .args(extra)
        .stdin(Stdio::null())
        .stdout(Stdio::piped());
    command.as_std_mut().process_group(0);
    let mut child = command.spawn()?;
    let mut lines = BufReader::new(child.stdout.take().ok_or("Client stdout unavailable")?).lines();
    let readiness = tokio::select! {
        result = tokio::time::timeout(Duration::from_secs(30), lines.next_line()) => Some(result),
        _ = signals.recv() => None,
    };
    let Some(readiness) = readiness else {
        stop_child(&mut child).await?;
        return Ok(None);
    };
    let line = match readiness {
        Ok(Ok(Some(line))) => line,
        Ok(Ok(None)) => {
            let _ = stop_child(&mut child).await;
            return Err("Client exited before readiness; see its error above".into());
        }
        Ok(Err(error)) => {
            let _ = stop_child(&mut child).await;
            return Err(error.into());
        }
        Err(_) => {
            let _ = stop_child(&mut child).await;
            return Err("Client did not report readiness".into());
        }
    };
    let ready: Value = match serde_json::from_str(&line) {
        Ok(ready) => ready,
        Err(error) => {
            let _ = stop_child(&mut child).await;
            return Err(format!("invalid Client readiness: {error}").into());
        }
    };
    let address = match ready["listening"]
        .as_str()
        .ok_or("Client readiness is missing its local address")
        .and_then(|address| {
            address
                .parse()
                .map_err(|_| "Client readiness has an invalid local address")
        }) {
        Ok(address) => address,
        Err(error) => {
            let _ = stop_child(&mut child).await;
            return Err(error.into());
        }
    };
    let token = match paths.token() {
        Ok(token) => token,
        Err(error) => {
            let _ = stop_child(&mut child).await;
            return Err(error.into());
        }
    };
    Ok(Some(ClientConnection {
        address,
        token,
        child: Some(child),
    }))
}

pub(super) async fn stop_child(child: &mut Child) -> Result<()> {
    if child.try_wait()?.is_some() {
        return Ok(());
    }
    let status = Command::new("/bin/kill")
        .arg("-INT")
        .arg(child.id().ok_or("child identity unavailable")?.to_string())
        .status()
        .await?;
    if !status.success() && child.try_wait()?.is_none() {
        return Err("could not signal owned child; shutdown is unconfirmed".into());
    }
    match tokio::time::timeout(Duration::from_secs(90), child.wait()).await {
        Ok(status) => {
            status?;
            Ok(())
        }
        Err(_) => Err("owned process did not confirm shutdown; inspect persistent task state before restarting".into()),
    }
}

async fn healthy(paths: &LocalPaths) -> Result<(SocketAddr, String)> {
    let (address, token) = paths.connect()?;
    let client = tokio::time::timeout(
        Duration::from_secs(3),
        Client::connect(address, token.clone()),
    )
    .await??;
    let health = client.request("client.health", json!({})).await?;
    client.close().await;
    if health["status"] != "ok" || health["client"] != true {
        return Err("existing local Client is unhealthy".into());
    }
    Ok((address, token))
}

async fn open_workspace(
    address: SocketAddr,
    token: &str,
    workspace: &Path,
    signals: &mut ShutdownSignals,
) -> Result<Option<String>> {
    let client = tokio::select! {
        result = Client::connect(address, token) => result?,
        _ = signals.recv() => return Ok(None),
    };
    let result = {
        let operation = async {
            let catalog = client.request("client.workspaces.list", json!({})).await?;
            let id = catalog["workspaces"]
                .as_array()
                .and_then(|workspaces| {
                    workspaces.iter().find_map(|entry| {
                        (entry["path"].as_str() == workspace.to_str())
                            .then(|| entry["id"].as_str().map(str::to_owned))
                            .flatten()
                    })
                })
                .ok_or("workspace is not registered; run `cayw init --workspace DIR`")?;
            client
                .request("client.workspaces.open", json!({"id":id}))
                .await?;
            Ok::<_, Box<dyn Error>>(id)
        };
        tokio::pin!(operation);
        tokio::select! {
            result = &mut operation => Some(result),
            _ = signals.recv() => None,
        }
    };
    client.close().await;
    match result {
        Some(result) => result.map(Some),
        None => Ok(None),
    }
}

async fn start(
    paths: &LocalPaths,
    options: &UiOptions,
    address: SocketAddr,
    token: &str,
    owned_client: Option<&mut Child>,
    signals: &mut ShutdownSignals,
) -> Result<()> {
    let Some(workspace_id) = open_workspace(address, token, &paths.workspace, signals).await?
    else {
        return Ok(());
    };
    match options.ui.as_str() {
        "tui" => {
            super::cli_tui::run_connected(address, token.to_owned(), workspace_id, signals).await
        }
        "web" => {
            start_web(
                paths,
                options,
                address,
                token,
                workspace_id,
                owned_client.is_some(),
                signals,
            )
            .await
        }
        "none" => {
            println!(
                "{}",
                json!({
                    "workspace":paths.workspace,
                    "workspace_id":workspace_id,
                    "client":address,
                    "owned_client":owned_client.is_some(),
                    "ui":"none"
                })
            );
            let Some(child) = owned_client else {
                return Ok(());
            };
            tokio::select! {
                _ = signals.recv() => Ok(()),
                status = child.wait() => process_exit(status?),
            }
        }
        _ => unreachable!("validated UI"),
    }
}

async fn start_web(
    paths: &LocalPaths,
    options: &UiOptions,
    address: SocketAddr,
    token: &str,
    workspace_id: String,
    owned_client: bool,
    signals: &mut ShutdownSignals,
) -> Result<()> {
    let client = tokio::select! {
        result = Client::connect(address, token) => result?,
        _ = signals.recv() => return Ok(()),
    };
    let mut params = json!({
        "plugin":"webui",
        "workspace_id":workspace_id,
        "account_file":options.account_file,
    });
    if let Some(lan) = &options.lan {
        params["lan"] = json!(lan);
    }
    let (mut ready, interrupted) = {
        let request = client.request("client.plugins.start", params);
        tokio::pin!(request);
        tokio::select! {
            result = &mut request => (result?, false),
            _ = signals.recv() => (request.await?, true),
        }
    };
    let reused = ready["reused"]
        .as_bool()
        .ok_or("Client did not report whether the WebUI was reused")?;
    let launch_id = ready["launch_id"]
        .as_str()
        .ok_or("Client did not return a WebUI launch identity")?
        .to_owned();
    if interrupted {
        let result = if reused {
            Ok(())
        } else {
            client
                .request("client.plugins.stop", json!({"launch_id":launch_id}))
                .await
                .map(|_| ())
                .map_err(|error| error.into())
        };
        client.close().await;
        return result;
    }

    ready["workspace"] = json!(paths.workspace);
    ready["client"] = json!(address);
    ready["owned_client"] = json!(owned_client);
    ready["ui"] = json!("web");
    let (presented, presentation_interrupted) = {
        let presentation = present(ready.clone(), options.no_open);
        tokio::pin!(presentation);
        tokio::select! {
            result = &mut presentation => (result, false),
            _ = signals.recv() => (presentation.await, true),
        }
    };
    presented?;
    if presentation_interrupted {
        let result = if reused {
            Ok(())
        } else {
            client
                .request("client.plugins.stop", json!({"launch_id":launch_id}))
                .await
                .map(|_| ())
                .map_err(|error| error.into())
        };
        client.close().await;
        return result;
    }
    if reused {
        client.close().await;
        return Ok(());
    }
    let result = wait_for_web(&client, &launch_id, signals).await;
    client.close().await;
    result
}

async fn wait_for_web(
    client: &Client,
    launch_id: &str,
    signals: &mut ShutdownSignals,
) -> Result<()> {
    let mut interval = tokio::time::interval(Duration::from_millis(250));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = signals.recv() => {
                client.request("client.plugins.stop", json!({"launch_id":launch_id})).await?;
                return Ok(());
            }
            _ = interval.tick() => {
                let listing = client.request("client.plugins.list", json!({})).await?;
                let running = listing["entries"].as_array().is_some_and(|entries| {
                    entries.iter().any(|entry| entry["launch_id"] == launch_id)
                });
                if !running {
                    return Err("Client-owned WebUI exited; no request was replayed".into());
                }
            }
        }
    }
}

fn process_exit(status: std::process::ExitStatus) -> Result<()> {
    if status.success() {
        Ok(())
    } else {
        Err(format!("owned Client exited with {status}; no request was replayed").into())
    }
}

struct UiOptions {
    ui: String,
    no_open: bool,
    lan: Option<String>,
    account_file: PathBuf,
}

async fn present(mut ready: Value, no_open: bool) -> Result<()> {
    if ready["ui"] == "tui" {
        return Ok(());
    }
    if ready["ui"] == "web" {
        // Account settings can change the shared LAN policy while WebUI runs.
        // Query the local management listener instead of trusting startup state.
        let status_url = format!(
            "{}api/auth/status",
            ready["local_url"]
                .as_str()
                .or_else(|| ready["url"].as_str())
                .ok_or("missing browser URL")?
        );
        let current: std::result::Result<Value, Box<dyn Error>> = async {
            let response = reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(5))
                .build()?
                .get(status_url)
                .send()
                .await?
                .error_for_status()?;
            Ok(response.json().await?)
        }
        .await;
        match current {
            Ok(status) => {
                ready["login_required"] = status["login_required"].clone();
                ready["lan_login_enabled"] = status["lan_login_enabled"].clone();
            }
            Err(error) => {
                ready["login_required"] = Value::Null;
                eprintln!(
                    "Browser login state could not be confirmed: {error}; inspect the page status."
                );
            }
        }
        let url = ready["open_url"]
            .as_str()
            .or_else(|| ready["local_url"].as_str())
            .or_else(|| ready["url"].as_str())
            .ok_or("missing browser URL")?
            .to_owned();
        if let Some(reason) = open_skip_reason(no_open) {
            ready["browser_open"] = json!(reason);
        } else {
            match open_browser(&url).await {
                Ok(()) => ready["browser_open"] = json!("opened"),
                Err(error) => {
                    eprintln!("Browser could not be opened: {error}. Open the URL below.");
                    ready["browser_open"] = json!("failed");
                }
            }
        }
    }
    println!("{ready}");
    Ok(())
}

/// Explicit account setup delegates to the optional WebUI binary, keeping all
/// password handling and hashing outside Runtime/Core.
pub async fn account(mut args: impl Iterator<Item = String>) -> Result<()> {
    let mut workspace = std::env::current_dir()?;
    let mut username = None;
    let mut file = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--workspace" => workspace = PathBuf::from(args.next().ok_or("missing workspace")?),
            "--username" if username.is_none() => {
                username = Some(args.next().ok_or("missing username")?)
            }
            "--file" if file.is_none() => {
                file = Some(PathBuf::from(args.next().ok_or("missing file")?))
            }
            "--help" | "-h" => {
                println!(
                    "cayw account --username NAME [--workspace DIR] [--file FILE]\nInteractively creates a Client-wide LAN account with a salted password hash; no network service is started."
                );
                return Ok(());
            }
            _ => {
                return Err(
                    "account accepts --username NAME, --workspace DIR and --file FILE".into(),
                );
            }
        }
    }
    let username = username.ok_or("account requires --username NAME")?;
    let paths = LocalPaths::from_environment(workspace)?;
    paths.initialize()?;
    let status = Command::new(std::env::current_exe()?.with_file_name("cayw-webui"))
        .arg("account")
        .arg("--username")
        .arg(username)
        .arg("--file")
        .arg(file.unwrap_or_else(|| paths.data_dir.join("web-account.json")))
        .status()
        .await?;
    if !status.success() {
        return Err("LAN account setup failed; no service was started".into());
    }
    Ok(())
}

fn open_skip_reason(no_open: bool) -> Option<&'static str> {
    if no_open {
        return Some("disabled");
    }
    if ["SSH_CONNECTION", "SSH_TTY"]
        .iter()
        .any(|key| std::env::var_os(key).is_some_and(|value| !value.is_empty()))
    {
        return Some("ssh");
    }
    if !cfg!(target_os = "macos")
        && !["DISPLAY", "WAYLAND_DISPLAY"]
            .iter()
            .any(|key| std::env::var_os(key).is_some_and(|value| !value.is_empty()))
    {
        return Some("no-desktop");
    }
    None
}

async fn open_browser(link: &str) -> Result<()> {
    let program = if cfg!(target_os = "macos") {
        "/usr/bin/open"
    } else {
        "xdg-open"
    };
    let mut child = Command::new(program)
        .arg(link)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    match tokio::time::timeout(Duration::from_secs(5), child.wait()).await {
        Ok(Ok(status)) if status.success() => Ok(()),
        Ok(Ok(status)) => Err(format!("system browser opener exited with {status}").into()),
        Ok(Err(error)) => Err(error.into()),
        // Some system openers wait for the browser. Stop and reap only the
        // disposable opener process; the browser is a separate application.
        Err(_) => {
            child.start_kill()?;
            child.wait().await?;
            Err("system browser opener has not confirmed opening".into())
        }
    }
}
