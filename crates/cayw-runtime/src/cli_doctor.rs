//! Read-only local product diagnostics. Provider endpoints are never contacted.
use crate::cli_settings;
use cayw_ipc::Client;
use cayw_runtime::{HeadlessConfig, local_cli::LocalPaths};
use serde_json::{Value, json};
use std::{
    error::Error,
    fs,
    path::{Path, PathBuf},
};

pub async fn run(workspace: &Path, explicit_config: Option<&Path>) -> Result<(), Box<dyn Error>> {
    let paths = LocalPaths::from_environment(workspace)?;
    let mut checks = Vec::new();

    match paths.validate_existing() {
        Ok(()) => checks.push(ok("local_state", "本地状态目录和私有文件可读取")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => checks.push(warning(
            "local_state",
            "本地状态尚未初始化；运行 `cayw start` 或 `cayw init` 后会创建",
        )),
        Err(error) => checks.push(failed("local_state", error.to_string())),
    }

    let config_file = explicit_config.unwrap_or(&paths.config_file);
    let mut selected = None;
    match config_file.try_exists() {
        Ok(true) => match HeadlessConfig::load(config_file) {
            Ok(config) => {
                selected = config.model().cloned();
                checks.push(ok("configuration", "配置文件可以解析"));
            }
            Err(error) => checks.push(failed("configuration", error.to_string())),
        },
        Ok(false) => checks.push(warning("configuration", "配置文件尚不存在")),
        Err(error) => checks.push(failed("configuration", error.to_string())),
    }

    if explicit_config.is_none() {
        match cli_settings::selected_model(&paths) {
            Ok(model) if model.is_some() => selected = model,
            Ok(_) => {}
            Err(error) => checks.push(failed("model_profiles", error.to_string())),
        }
    }
    match selected {
        Some(model) => checks.push(json!({
            "name":"model",
            "status":"ok",
            "message":"已选择模型；doctor 没有发送模型请求",
            "model":model.public_view(),
            "credential":if model.has_api_key() { "plain_file" } else { "none" },
        })),
        None => checks.push(warning(
            "model",
            "尚未选择模型；启动仍可继续，首次对话会提示运行 `cayw conf`",
        )),
    }

    let cleanup_path = paths.data_dir.join("client-plugin-cleanup.json");
    match crate::cli_client_plugins::inspect_cleanup(&cleanup_path) {
        Ok(details) if details["exists"] == true => {
            let live = details["live_processes"].as_u64().unwrap_or(0);
            checks.push(json!({
                "name":"client_plugin_cleanup",
                "status":"warning",
                "message":if live == 0 {
                    "存在 Client 插件清理记录；记录中的进程已退出，doctor 未删除该记录"
                } else {
                    "存在仍在运行的 Client 插件清理记录；请核对唯一 Client 和对应进程"
                },
                "details":details,
            }));
        }
        Ok(details) => checks.push(json!({
            "name":"client_plugin_cleanup",
            "status":"ok",
            "message":"没有待核对的 Client 插件清理记录",
            "details":details,
        })),
        Err(error) => checks.push(warning(
            "client_plugin_cleanup",
            format!(
                "Client 插件清理记录无法读取或校验；UI 插件保持不可用，Client 仍可启动；请修复或删除该记录：{error}"
            ),
        )),
    }

    match legacy_client_state(&paths) {
        Ok(check) => checks.push(check),
        Err(error) => checks.push(failed("legacy_client_state", error.to_string())),
    }

    match paths.connect() {
        Ok((address, token)) => {
            let health = tokio::time::timeout(std::time::Duration::from_secs(3), async {
                let client = Client::connect(address, token).await?;
                let health = client.request("client.health", json!({})).await;
                client.close().await;
                health
            })
            .await;
            match health {
                Ok(Ok(health)) => {
                    let plugin_summary = health["client_plugins"].clone();
                    checks.push(json!({
                        "name":"client",
                        "status":"ok",
                        "message":"唯一 Client 入口可连接",
                        "unique":true,
                        "client_file":paths.client_file,
                        "open_workspaces":health["open_workspaces"],
                        "health":health,
                    }));
                    checks.push(client_plugins_check(plugin_summary));
                    checks.push(optional_services_check(health["disabled_services"].clone()));
                }
                Ok(Err(error)) => {
                    checks.push(failed("client", error.to_string()));
                    checks.push(warning(
                        "client_plugins",
                        "Client 不可连接，无法读取当前 UI 插件状态",
                    ));
                    checks.push(warning(
                        "optional_services",
                        "Client 不可连接，无法读取可选服务状态",
                    ));
                }
                Err(_) => {
                    checks.push(failed("client", "Client 健康检查超时"));
                    checks.push(warning(
                        "client_plugins",
                        "Client 健康检查超时，无法读取当前 UI 插件状态",
                    ));
                    checks.push(warning(
                        "optional_services",
                        "Client 健康检查超时，无法读取可选服务状态",
                    ));
                }
            }
        }
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::InvalidInput | std::io::ErrorKind::NotFound
            ) =>
        {
            checks.push(json!({
                "name":"client",
                "status":"warning",
                "message":error.to_string(),
                "unique":null,
                "client_file":paths.client_file,
            }));
            checks.push(warning(
                "client_plugins",
                "没有可连接的 Client，当前没有可读取的 UI 插件状态",
            ));
            checks.push(warning(
                "optional_services",
                "没有可连接的 Client，当前没有可选服务状态",
            ));
        }
        Err(error) => {
            checks.push(failed("client", error.to_string()));
            checks.push(warning(
                "client_plugins",
                "Client 状态不可读取，无法读取当前 UI 插件状态",
            ));
            checks.push(warning(
                "optional_services",
                "Client 状态不可读取，无法读取可选服务状态",
            ));
        }
    }

    #[cfg(feature = "plugins")]
    checks.push(crate::cli_plugins::doctor(&paths));
    #[cfg(not(feature = "plugins"))]
    checks.push(warning("plugins", "当前二进制没有启用 plugins feature"));

    let failed = checks.iter().any(|check| check["status"] == "failed");
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "schema":"cayw.doctor.v1",
            "status":if failed { "attention" } else { "ok" },
            "workspace":paths.workspace,
            "config_file":config_file,
            "network_model_request":false,
            "checks":checks,
        }))?
    );
    Ok(())
}

fn client_plugins_check(summary: Value) -> Value {
    let valid =
        summary["schema"] == "cayw.client-plugins.v1" && summary["entries"].as_array().is_some();
    if !valid {
        return failed(
            "client_plugins",
            "Client 返回了无法识别的 UI 插件状态；doctor 未修改任何插件记录",
        );
    }
    let entries = summary["entries"].as_array().expect("validated entries");
    if let Some(error) = summary["cleanup_error"].as_str() {
        return json!({
            "name":"client_plugins",
            "status":"warning",
            "message":format!("Client UI 插件清理记录不可用；UI 插件保持不可用，Client 仍可启动；请修复或删除该记录：{error}"),
            "summary":summary,
        });
    }
    let unknown = entries
        .iter()
        .filter(|entry| entry["status"] == "cleanup_unknown")
        .count();
    json!({
        "name":"client_plugins",
        "status":if unknown == 0 { "ok" } else { "warning" },
        "message":if unknown == 0 {
            "Client UI 插件状态可读取"
        } else {
            "存在清理结果未知的 Client UI 插件；请核对对应进程"
        },
        "summary":summary,
    })
}

fn optional_services_check(disabled: Value) -> Value {
    let Some(entries) = disabled.as_array() else {
        return warning(
            "optional_services",
            "Client 健康信息没有可选服务状态；doctor 无法判断是否降级",
        );
    };
    if entries.is_empty() {
        return ok("optional_services", "没有可选服务被禁用");
    }
    let details = entries
        .iter()
        .map(|entry| {
            let name = entry["name"].as_str().unwrap_or("unknown");
            let reason = entry["reason"].as_str().unwrap_or("未提供原因");
            format!("{name}: {reason}")
        })
        .collect::<Vec<_>>()
        .join("; ");
    json!({
        "name":"optional_services",
        "status":"warning",
        "message":format!("可选服务已禁用：{details}"),
        "disabled_services":disabled,
    })
}

fn legacy_client_state(paths: &LocalPaths) -> Result<Value, Box<dyn Error>> {
    let state_dirs = match paths.workspace_state_dirs() {
        Ok(paths) => paths,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error.into()),
    };
    let mut entries = Vec::new();
    for state_dir in state_dirs {
        let mut count = 0_usize;
        for entry in fs::read_dir(&state_dir)? {
            let entry = entry?;
            count += 1;
            if count > 512 {
                return Err(format!(
                    "工作区状态目录项目过多，未继续扫描：{}",
                    state_dir.display()
                )
                .into());
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let legacy = name == "connection.json"
                || name == "server.lock"
                || (name.starts_with("launcher-")
                    && (name.ends_with(".lock") || name.ends_with(".json")));
            if !legacy {
                continue;
            }
            let path: PathBuf = entry.path();
            let metadata = fs::symlink_metadata(&path)?;
            let kind = if metadata.file_type().is_symlink() {
                "symlink"
            } else if metadata.is_file() {
                "file"
            } else if metadata.is_dir() {
                "directory"
            } else {
                "other"
            };
            entries.push(json!({"path":path,"kind":kind}));
        }
    }
    if entries.is_empty() {
        Ok(json!({
            "name":"legacy_client_state",
            "status":"ok",
            "message":"没有旧的按工作区连接或 launcher 状态",
            "entries":entries,
        }))
    } else {
        Ok(json!({
            "name":"legacy_client_state",
            "status":"warning",
            "message":"发现旧的按工作区连接或 launcher 状态；doctor 只报告，不删除",
            "entries":entries,
        }))
    }
}

fn ok(name: &str, message: impl Into<String>) -> Value {
    json!({"name":name,"status":"ok","message":message.into()})
}

fn warning(name: &str, message: impl Into<String>) -> Value {
    json!({"name":name,"status":"warning","message":message.into()})
}

fn failed(name: &str, message: impl Into<String>) -> Value {
    json!({"name":name,"status":"failed","message":message.into()})
}

#[cfg(test)]
mod tests {
    use super::optional_services_check;
    use serde_json::json;

    #[test]
    fn doctor_warns_when_optional_service_disabled() {
        let check = optional_services_check(json!([
            {"name":"web_access","reason":"invalid web-access limits: max_search_limit must be nonzero"}
        ]));
        assert_eq!(check["status"], "warning");
        let message = check["message"].as_str().unwrap();
        assert!(message.contains("web_access"), "{message}");
        assert!(message.contains("max_search_limit"), "{message}");
    }

    #[test]
    fn doctor_reports_ok_when_no_optional_service_is_disabled() {
        let check = optional_services_check(json!([]));
        assert_eq!(check["status"], "ok");
    }
}
