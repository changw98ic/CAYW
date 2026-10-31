//! Foreground local application supervisor; optional UIs stay separate programs.
use cayw_ai::CancellationToken;
use cayw_ipc::Client;
use cayw_runtime::local_cli::{LocalPaths, read_private, save_config};
use serde_json::{Value, json};
use std::{error::Error, io, path::PathBuf, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
};

type Result<T> = std::result::Result<T, Box<dyn Error>>;
const HELP: &str = "cayw start [--workspace DIR] [--ui web|tui|none] [--offline] [--demo-authorized] [--config PATH] [--no-open]\n\nOne foreground command starts or connects to this workspace's runtime and starts the chosen UI.\nWeb opens your default browser with a one-use login link; refresh keeps the session.\n--no-open, SSH and headless launches print a short-lived clickable link instead.\nCtrl+C stops only processes started by this command. Existing services are kept.\nUse --workspace to choose another project. No model request is made by startup.";

pub async fn run(mut args: impl Iterator<Item = String>) -> Result<()> {
    let mut workspace = std::env::current_dir()?;
    let mut ui = "web".to_string();
    let mut extra = Vec::new();
    let mut no_open = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                println!("{HELP}");
                return Ok(());
            }
            "--workspace" => workspace = PathBuf::from(args.next().ok_or("missing workspace")?),
            "--ui" => ui = args.next().ok_or("missing UI")?,
            "--no-open" => no_open = true,
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
    let paths = LocalPaths::from_environment(workspace)?;
    paths.initialize()?;
    let executable = std::env::current_exe()?;
    let ui_binary = executable.with_file_name(if ui == "web" {
        "cayw-webui"
    } else {
        "cayw-tui"
    });
    if ui != "none" && !ui_binary.is_file() {
        return Err(format!(
            "{} is missing; install a complete package or select --ui none",
            ui_binary.display()
        )
        .into());
    }
    // A single launcher owns the UI/token file per workspace. A second start
    // reports the existing launch without spawning or stopping anything.
    let lock_path = paths.state_file("launcher.lock");
    save_if_missing(&lock_path, b"")?;
    let lock = std::fs::File::open(&lock_path)?;
    if lock.try_lock().is_err() {
        // A concurrent first start may still be waiting on Runtime/UI readiness.
        // Wait for its private descriptor so the second command can hand off too.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            match read_private(&paths.state_file("launcher.json"), 16384) {
                Ok(ready) => {
                    present(&paths, serde_json::from_slice(&ready)?, no_open).await?;
                    return Ok(());
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    if tokio::time::Instant::now() >= deadline {
                        return Err("another launcher has not confirmed readiness; inspect its terminal and retry".into());
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Err(error) => return Err(error.into()),
            }
        }
    }
    match std::fs::remove_file(paths.state_file("launcher.json")) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let stop = CancellationToken::new();
    let interrupt = stop.clone();
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let signal = tokio::spawn(async move {
        tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
        interrupt.cancel();
    });
    let mut children = Vec::<Child>::new();
    let result = start(
        &paths,
        &ui_binary,
        &ui,
        no_open,
        &extra,
        &stop,
        &mut children,
    )
    .await;
    // Stop UI first, then the owned runtime; signal, allow durable cancellation
    // and cleanup, then report an unconfirmed shutdown rather than kill/replay.
    let mut cleanup_error = None;
    for child in children.iter_mut().rev() {
        if let Err(error) = stop_child(child).await {
            eprintln!("{error}");
            cleanup_error = Some(error);
        }
    }

    signal.abort();
    let _ = std::fs::remove_file(paths.state_file("launcher.json"));
    match cleanup_error {
        Some(error) => Err(error),
        None => result,
    }
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
        Ok(status) => { status?; Ok(()) },
        Err(_) => Err("owned process did not confirm shutdown; inspect persistent task state before restarting".into()),
    }
}
async fn healthy(paths: &LocalPaths) -> Result<(std::net::SocketAddr, String)> {
    let (address, token) = paths.connect()?;
    let client = tokio::time::timeout(
        Duration::from_secs(3),
        Client::connect(address, token.clone()),
    )
    .await??;
    let health = client.request("runtime.health", json!({})).await?;
    if health["status"] != "ok" {
        return Err("existing runtime is unhealthy".into());
    }
    Ok((address, token))
}
async fn start(
    paths: &LocalPaths,
    ui_binary: &std::path::Path,
    ui: &str,
    no_open: bool,
    extra: &[String],
    stop: &CancellationToken,
    children: &mut Vec<Child>,
) -> Result<()> {
    let (address, token, owned) = if let Ok((address, token)) = healthy(paths).await {
        if !extra.is_empty() {
            return Err("runtime already running: startup overrides cannot be applied to it; stop that runtime first".into());
        }
        (address, token, false)
    } else {
        let mut command = Command::new(std::env::current_exe()?);
        command
            .arg("serve")
            .arg("--workspace")
            .arg(&paths.workspace)
            .args(extra)
            .stdin(Stdio::null())
            .stdout(Stdio::piped());
        children.push(command.spawn()?);
        let child = children.last_mut().expect("owned runtime");
        let mut lines =
            BufReader::new(child.stdout.take().ok_or("runtime stdout unavailable")?).lines();
        let line = tokio::select! {
            _ = stop.cancelled() => return Ok(()),
            line = tokio::time::timeout(Duration::from_secs(30), lines.next_line()) => line??.ok_or("runtime exited before readiness; see its error above")?,
        };
        let ready: Value = serde_json::from_str(&line)?;
        let address = ready["listening"]
            .as_str()
            .ok_or("missing runtime address")?
            .parse()?;
        (address, paths.token()?, true)
    };
    let mut ready =
        json!({"workspace":paths.workspace,"runtime":address,"owned_runtime":owned,"ui":ui});
    if ui != "none" {
        let mut command = Command::new(ui_binary);
        command
            .env("CAYW_IPC_ADDR", address.to_string())
            .env("CAYW_IPC_TOKEN", token);
        if ui == "web" {
            let web_token = format!(
                "{}{}",
                uuid::Uuid::new_v4().simple(),
                uuid::Uuid::new_v4().simple()
            );
            let path = paths.state_file("web.token");
            let previous = match read_private(&path, 4096) {
                Ok(bytes) => Some(bytes),
                Err(e) if e.kind() == io::ErrorKind::NotFound => None,
                Err(e) => return Err(e.into()),
            };
            save_config(&path, previous.as_deref(), web_token.as_bytes())?;
            command
                .env("CAYW_LAUNCHER", "1")
                .env("CAYW_WEB_TOKEN", web_token)
                .env("CAYW_OWNERSHIP_PATH", paths.state_file("ownership.json"))
                .env("CAYW_WEB_ADDR", "127.0.0.1:0")
                .stdout(Stdio::piped())
                .stdin(Stdio::null());
            children.push(command.spawn()?);
            let child = children.last_mut().expect("owned web UI");
            let mut lines =
                BufReader::new(child.stdout.take().ok_or("Web stdout unavailable")?).lines();
            let line = tokio::select! {
                _ = stop.cancelled() => return Ok(()),
                line = tokio::time::timeout(Duration::from_secs(30), lines.next_line()) => line??.ok_or("WebUI exited before readiness; see its error above")?,
            };
            let web: Value = serde_json::from_str(&line)?;
            ready["url"] = web["url"].clone();
            ready["web_token_file"] = json!(path);
        } else {
            children.push(command.spawn()?);
        }
    }
    let path = paths.state_file("launcher.json");
    let previous = match read_private(&path, 16384) {
        Ok(bytes) => Some(bytes),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    save_config(&path, previous.as_deref(), &serde_json::to_vec(&ready)?)?;
    present(paths, ready, no_open).await?;
    loop {
        tokio::select! {
            _ = stop.cancelled() => return Ok(()),
            _ = tokio::time::sleep(Duration::from_millis(100)) => {
                for child in children.iter_mut() {
                    if let Some(status) = child.try_wait()? {
                        if status.success() { return Ok(()); }
                        return Err(format!("owned process exited with {status}; no request was replayed").into());
                    }
                }
            }
        }
    }
}
fn save_if_missing(path: &std::path::Path, bytes: &[u8]) -> io::Result<()> {
    match read_private(path, 1024) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => save_config(path, None, bytes),
        Err(e) => Err(e),
    }
}

// Read the private control credential only inside this local launcher. A
// browser receives a fresh, authority-bound, one-use ticket, never this token.
async fn present(paths: &LocalPaths, mut ready: Value, no_open: bool) -> Result<()> {
    if ready["ui"] == "web" {
        let base = ready["url"].as_str().ok_or("missing browser URL")?;
        let url = reqwest::Url::parse(base)?;
        if url.scheme() != "http"
            || url.username() != ""
            || url.password().is_some()
            || url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
            || !url
                .host_str()
                .and_then(|host| host.parse::<std::net::IpAddr>().ok())
                .is_some_and(|ip| ip.is_loopback())
        {
            return Err("invalid local browser control address".into());
        }
        let token = String::from_utf8(read_private(&paths.state_file("web.token"), 4096)?)?;
        let response = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(5))
            .build()?
            .post(url.join("api/browser/launch")?)
            .bearer_auth(token)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(format!(
                "browser handoff failed (HTTP {}); existing service was kept",
                response.status()
            )
            .into());
        }
        let handoff: Value = response.json().await?;
        let link = handoff["launch_url"]
            .as_str()
            .ok_or("missing browser handoff link")?;
        let ticket = link
            .strip_prefix(&format!("{base}?launch="))
            .ok_or("invalid browser handoff link")?;
        if ticket.len() != 64 || !ticket.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err("invalid browser handoff ticket".into());
        }
        let skip = open_skip_reason(no_open);
        let opened = match skip {
            Some(reason) => {
                ready["browser_open"] = json!(reason);
                false
            }
            None => match open_browser(link).await {
                Ok(()) => {
                    ready["browser_open"] = json!("opened");
                    true
                }
                Err(error) => {
                    eprintln!(
                        "Browser could not be opened: {error}. Open the launch_url below within 60 seconds; rerun cayw start for a new link."
                    );
                    ready["browser_open"] = json!("failed");
                    false
                }
            },
        };
        if !opened {
            ready["launch_url"] = json!(link);
            ready["launch_expires_in"] = json!(60);
        }
        ready
            .as_object_mut()
            .expect("ready object")
            .remove("web_token_file");
    }
    println!("{ready}");
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
        // Some system openers wait for the browser. Do not terminate the user's
        // browser; keep serving and offer the clickable fallback.
        Err(_) => Err("system browser opener has not confirmed opening".into()),
    }
}
