use cayw_ipc::Client;
use cayw_runtime::local_cli::LocalPaths;
use serde_json::json;
use std::{error::Error, net::SocketAddr, path::PathBuf, process::Stdio};
use tokio::process::Command;

const USAGE: &str = "cayw tui [--workspace DIR]\n\n打开终端交互界面，连接唯一的本地 Client，并由 Client 路由到指定工作区。\n先运行 cayw start 或 cayw serve。工作区须先通过 cayw init 注册。\n使用 ~/.cayw 中的本机 Client 设置；CAYW_HOME 可指定其他数据根。\nCtrl+N 新建会话 | Enter 发送 | Ctrl+X 取消 | Ctrl+Q 退出";

pub async fn run(arguments: impl IntoIterator<Item = String>) -> Result<(), Box<dyn Error>> {
    let mut arguments = arguments.into_iter();
    let mut workspace = None;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--help" | "-h" => {
                println!("{USAGE}");
                return Ok(());
            }
            "--workspace" if workspace.is_none() => {
                workspace = Some(PathBuf::from(
                    arguments.next().ok_or("--workspace requires DIR")?,
                ));
            }
            _ => return Err("tui accepts only --workspace; use `cayw tui --help`".into()),
        }
    }
    let mut signals = super::cli_start::ShutdownSignals::new()?;
    let workspace = workspace.map_or_else(std::env::current_dir, Ok)?;
    let paths = LocalPaths::from_environment(workspace)?;
    let (address, token) = paths.connect().map_err(|error| {
        std::io::Error::new(
            error.kind(),
            format!("无法连接本地 Client：{error}。请先运行 `cayw start` 或 `cayw serve`。"),
        )
    })?;
    let client = tokio::select! {
        result = Client::connect(address, token.clone()) => result?,
        _ = signals.recv() => return Ok(()),
    };
    let catalog = tokio::select! {
        result = client.request("client.workspaces.list", json!({})) => result?,
        _ = signals.recv() => {
            client.close().await;
            return Ok(());
        }
    };
    let workspace_id = catalog["workspaces"]
        .as_array()
        .and_then(|workspaces| {
            workspaces.iter().find_map(|entry| {
                (entry["path"].as_str() == paths.workspace.to_str())
                    .then(|| entry["id"].as_str().map(str::to_owned))
                    .flatten()
            })
        })
        .ok_or("工作区尚未注册。请先运行 `cayw init --workspace DIR`。")?;
    let opened = tokio::select! {
        result = client.request("client.workspaces.open", json!({"id":workspace_id})) => Some(result),
        _ = signals.recv() => None,
    };
    client.close().await;
    let Some(opened) = opened else {
        return Ok(());
    };
    opened?;
    run_connected(address, token, workspace_id, &mut signals).await
}

pub(super) async fn run_connected(
    address: SocketAddr,
    token: String,
    workspace_id: String,
    signals: &mut super::cli_start::ShutdownSignals,
) -> Result<(), Box<dyn Error>> {
    let client = tokio::select! {
        result = Client::connect(address, token.clone()) => result?,
        _ = signals.recv() => return Ok(()),
    };
    let (prepared, prepare_interrupted) = {
        let request = client.request(
            "client.plugins.start",
            json!({"plugin":"tui","workspace_id":workspace_id}),
        );
        tokio::pin!(request);
        tokio::select! {
            result = &mut request => (result?, false),
            _ = signals.recv() => (request.await?, true),
        }
    };
    let launch_id = prepared["launch_id"]
        .as_str()
        .ok_or("Client did not return a TUI launch identity")?
        .to_owned();
    if prepare_interrupted {
        let cleanup = cleanup_unattached(&client, &launch_id, None).await;
        client.close().await;
        cleanup?;
        return Ok(());
    }

    let binary = std::env::current_exe()?.with_file_name("cayw-tui");
    if !binary.is_file() {
        let cleanup = cleanup_unattached(&client, &launch_id, None).await;
        client.close().await;
        cleanup?;
        return Err(format!(
            "无法启动终端界面 {}：文件不存在。请使用发行包的 install.sh 安装完整程序。",
            binary.display()
        )
        .into());
    }
    let spawned = Command::new(&binary)
        .env("CAYW_IPC_ADDR", address.to_string())
        .env("CAYW_IPC_TOKEN", token)
        .env("CAYW_WORKSPACE_ID", &workspace_id)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(error) => {
            let cleanup = cleanup_unattached(&client, &launch_id, None).await;
            client.close().await;
            cleanup?;
            return Err(error.into());
        }
    };
    let pid = match child.id() {
        Some(pid) => pid,
        None => {
            let cleanup = cleanup_unattached(&client, &launch_id, Some(&mut child)).await;
            client.close().await;
            cleanup?;
            return Err("终端界面进程标识不可用".into());
        }
    };
    let (attached, attach_interrupted) = {
        let request = client.request(
            "client.plugins.attach",
            json!({"launch_id":launch_id,"pid":pid}),
        );
        tokio::pin!(request);
        tokio::select! {
            result = &mut request => (result, false),
            _ = signals.recv() => (request.await, true),
        }
    };
    let attached = match attached {
        Ok(attached) => attached,
        Err(error) => {
            let cleanup = cleanup_unattached(&client, &launch_id, Some(&mut child)).await;
            client.close().await;
            return match cleanup {
                Ok(()) => Err(format!("Client 未能接管终端界面：{error}").into()),
                Err(cleanup) => Err(format!(
                    "Client 未能接管终端界面：{error}；本地清理也未确认：{cleanup}"
                )
                .into()),
            };
        }
    };
    if attached["exited_before_attach"] == true && attached["supervised"] == false {
        // Client did not adopt a live process. Only our own child's wait result
        // can establish a clean early quit; never turn a crash into success.
        let status = child.wait().await?;
        let cleanup = cleanup_unattached(&client, &launch_id, None).await;
        client.close().await;
        cleanup?;
        return tui_exit(status, attach_interrupted);
    }

    let mut interrupted = attach_interrupted;
    let status = if interrupted {
        stop_attached(&client, &launch_id, &mut child).await?
    } else {
        tokio::select! {
            status = child.wait() => status?,
            _ = signals.recv() => {
                interrupted = true;
                stop_attached(&client, &launch_id, &mut child).await?
            }
        }
    };
    let detach = client
        .request(
            "client.plugins.detach",
            json!({"launch_id":launch_id,"pid":pid}),
        )
        .await;
    client.close().await;
    detach?;
    tui_exit(status, interrupted)
}

fn tui_exit(status: std::process::ExitStatus, interrupted: bool) -> Result<(), Box<dyn Error>> {
    if status.success() || interrupted {
        Ok(())
    } else {
        Err(format!("终端界面退出：{status}").into())
    }
}

async fn cleanup_unattached(
    client: &Client,
    launch_id: &str,
    child: Option<&mut tokio::process::Child>,
) -> Result<(), Box<dyn Error>> {
    let mut failures = Vec::new();
    if let Some(child) = child
        && let Err(error) = super::cli_start::stop_child(child).await
    {
        failures.push(format!("TUI process: {error}"));
    }
    if let Err(error) = client
        .request("client.plugins.stop", json!({"launch_id":launch_id}))
        .await
    {
        failures.push(format!("Client launch record: {error}"));
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; ").into())
    }
}

async fn stop_attached(
    client: &Client,
    launch_id: &str,
    child: &mut tokio::process::Child,
) -> Result<std::process::ExitStatus, Box<dyn Error>> {
    let (stop, status) = tokio::join!(
        client.request("client.plugins.stop", json!({"launch_id":launch_id})),
        child.wait(),
    );
    stop?;
    Ok(status?)
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    #[test]
    fn early_tui_exit_uses_actual_child_status() {
        assert!(tui_exit(std::process::ExitStatus::from_raw(0), false).is_ok());
        assert!(tui_exit(std::process::ExitStatus::from_raw(7 << 8), false).is_err());
        assert!(tui_exit(std::process::ExitStatus::from_raw(2), false).is_err());
    }
}
