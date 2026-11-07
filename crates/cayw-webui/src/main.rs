use cayw_webui::{
    AccountSettings, DeviceRouter, GatewayLimits, IpcEndpoint, LanAccount, OwnershipStore,
    WebAccess,
};
use std::{
    collections::HashMap,
    env,
    io::{self, IsTerminal},
    net::SocketAddr,
    path::PathBuf,
    process::ExitCode,
    sync::Arc,
};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

const HELP: &str = "CAYW 可选网页界面\n\n推荐：cayw --ui web [--workspace DIR] [--no-open]\n本地默认仅监听回环地址，无登录。局域网须显式 --lan IP:PORT；账户可选，未配置时允许访问，配置后使用账号密码登录。\n配置账户：网页设置 → 局域网账号；本机入口保存后立即生效。备用 CLI：cayw account --username NAME [--workspace DIR]\n独立网关：cayw-webui [--ipc-addr 127.0.0.1:PORT] [--web-addr 127.0.0.1:PORT] [--ownership PATH]\n  cayw-webui --lan IP:PORT --account-file FILE\n  cayw-webui account --username NAME --file FILE\n必需环境变量 CAYW_IPC_ADDR / CAYW_IPC_TOKEN 仅用于本机 Runtime 连接，不需要在浏览器中输入。\nCAYW_WEB_ADDR 默认为 127.0.0.1:0，不能通过此变量开放局域网。\n局域网地址须为本机明确的私有或回环 IP；不接受通配地址。账户配置交互式输入密码两次，不回显。\n";
struct Config {
    ipc_addr: SocketAddr,
    web_addr: SocketAddr,
    ipc_token: String,
    ownership_path: PathBuf,
    account_path: PathBuf,
    access: WebAccess,
}
impl Config {
    fn parse(
        args: impl IntoIterator<Item = String>,
        mut values: HashMap<String, String>,
    ) -> Result<Self, String> {
        let mut args = args.into_iter();
        let mut lan = None;
        let mut account = None;
        let mut web_override = false;
        while let Some(arg) = args.next() {
            let value = args.next().ok_or("参数缺少值")?;
            match arg.as_str() {
                "--ipc-addr" => {
                    values.insert("CAYW_IPC_ADDR".into(), value);
                }
                "--web-addr" => {
                    values.insert("CAYW_WEB_ADDR".into(), value);
                    web_override = true;
                }
                "--ownership" | "--ownership-path" => {
                    values.insert("CAYW_OWNERSHIP_PATH".into(), value);
                }
                "--lan" if lan.is_none() => lan = Some(value),
                "--account-file" if account.is_none() => account = Some(PathBuf::from(value)),
                _ => return Err("不支持或重复的参数；使用 --help 查看运行方式".into()),
            }
        }
        let parse = |value: &str| {
            value
                .parse::<SocketAddr>()
                .map_err(|_| "地址必须是 IP:端口".to_string())
        };
        let ipc_addr = parse(values.get("CAYW_IPC_ADDR").ok_or("缺少 CAYW_IPC_ADDR")?)?;
        if !ipc_addr.ip().is_loopback() {
            return Err("Runtime IPC 只接受 loopback 地址".into());
        }
        let ownership_path = PathBuf::from(
            values
                .get("CAYW_OWNERSHIP_PATH")
                .map(String::as_str)
                .unwrap_or("ownership.json"),
        );
        let account_path = account.unwrap_or_else(|| {
            ownership_path
                .parent()
                .unwrap_or(std::path::Path::new("."))
                .join("web-account.json")
        });
        let (web_addr, access) = if let Some(lan) = lan {
            if web_override {
                return Err("--lan 与 --web-addr 不能同时指定".into());
            }
            let addr = parse(&lan)?;
            let private = match addr.ip() {
                std::net::IpAddr::V4(ip) => ip.is_private() || ip.is_loopback(),
                std::net::IpAddr::V6(ip) => ip.is_unique_local() || ip.is_loopback(),
            };
            if !private {
                return Err("--lan 必须指定本机私有或回环 IP，不能使用通配或公网地址".into());
            }
            let account = match LanAccount::read(&account_path) {
                Ok(account) => Some(account),
                Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                Err(error) => return Err(format!("账户文件无效或无法读取：{error}")),
            };
            (addr, WebAccess::Lan(account))
        } else {
            let addr = parse(
                values
                    .get("CAYW_WEB_ADDR")
                    .map(String::as_str)
                    .unwrap_or("127.0.0.1:0"),
            )?;
            if !addr.ip().is_loopback() {
                return Err("本地模式仅监听回环地址；局域网访问必须显式 --lan".into());
            }
            (addr, WebAccess::Local)
        };
        let ipc_token = values
            .remove("CAYW_IPC_TOKEN")
            .ok_or("缺少 CAYW_IPC_TOKEN")?;
        if !cayw_webui::valid_token(&ipc_token) {
            return Err("CAYW_IPC_TOKEN 格式无效".into());
        }
        Ok(Self {
            ipc_addr,
            web_addr,
            ipc_token,
            ownership_path,
            account_path,
            access,
        })
    }
}
fn account(args: impl IntoIterator<Item = String>) -> Result<(), String> {
    let mut args = args.into_iter();
    let mut username = None;
    let mut path = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--username" if username.is_none() => username = Some(args.next().ok_or("缺少用户名")?),
            "--file" if path.is_none() => {
                path = Some(PathBuf::from(args.next().ok_or("缺少账户文件路径")?))
            }
            _ => return Err("account --username NAME --file FILE".into()),
        }
    }
    let path = path.ok_or("需要 --file FILE")?;
    let username = username.ok_or("需要 --username NAME")?;
    if path.exists() {
        return Err("账户文件已存在，未覆盖；请选择新的 --file 路径".into());
    }
    if !io::stdin().is_terminal() {
        return Err("请在交互终端配置账户；密码不接受命令行参数".into());
    }
    let password = rpassword::prompt_password("Password (hidden): ").map_err(|e| e.to_string())?;
    let confirm =
        rpassword::prompt_password("Confirm password (hidden): ").map_err(|e| e.to_string())?;
    if password != confirm {
        return Err("两次密码不一致，未保存".into());
    }
    let account = LanAccount::from_password(&username, &password).map_err(|e| e.to_string())?;
    account
        .create(&path)
        .map_err(|e| format!("账户未能保存：{e}"))?;
    println!("账户已保存。局域网模式使用该账户文件；未启动网络服务。");
    Ok(())
}
#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.as_slice() == ["--help"] || args.as_slice() == ["-h"] {
        print!("{HELP}");
        return ExitCode::SUCCESS;
    }
    let result = if args.first().is_some_and(|arg| arg == "account") {
        account(args.into_iter().skip(1))
    } else {
        run(args).await
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("网页界面未能运行：{error}");
            ExitCode::FAILURE
        }
    }
}
async fn run(args: Vec<String>) -> Result<(), String> {
    let config = Config::parse(
        args,
        env::vars()
            .filter(|(name, _)| {
                matches!(
                    name.as_str(),
                    "CAYW_IPC_ADDR" | "CAYW_IPC_TOKEN" | "CAYW_WEB_ADDR" | "CAYW_OWNERSHIP_PATH"
                )
            })
            .collect(),
    )?;
    let client = IpcEndpoint::new(config.ipc_addr, config.ipc_token)
        .map_err(|_| "本机 IPC 地址或令牌无效")?;
    let devices = Arc::new(DeviceRouter::new(client));
    let ownership = OwnershipStore::open(&config.ownership_path)
        .map_err(|e| format!("无法打开持久所有权存储：{e}"))?;
    let listener = TcpListener::bind(config.web_addr)
        .await
        .map_err(|e| format!("无法监听网页地址：{e}"))?;
    let address = listener.local_addr().map_err(|e| e.to_string())?;
    let local_listener = if matches!(config.access, WebAccess::Lan(_)) {
        Some(
            TcpListener::bind("127.0.0.1:0")
                .await
                .map_err(|e| format!("无法监听本机管理入口：{e}"))?,
        )
    } else {
        None
    };
    let local_address = local_listener
        .as_ref()
        .map(TcpListener::local_addr)
        .transpose()
        .map_err(|e| e.to_string())?
        .unwrap_or(address);
    let login_required = matches!(config.access, WebAccess::Lan(Some(_)));
    if matches!(config.access, WebAccess::Lan(None)) {
        eprintln!(
            "局域网访问已开放，未配置账号，网络内可直接访问。请在本机管理页面的设置中配置可选账号，保存后立即启用局域网登录。"
        );
    }
    let mode = if matches!(config.access, WebAccess::Lan(_)) {
        "lan"
    } else {
        "local"
    };
    if env::var_os("CAYW_LAUNCHER").is_some() {
        println!(
            "{}",
            serde_json::json!({"url":format!("http://{address}/"),"access":mode,"login_required":login_required,"local_url":format!("http://{local_address}/")})
        );
    } else {
        println!("网页界面：http://{address}/");
        if local_listener.is_some() {
            println!("本机管理：http://{local_address}/（设置 → 局域网账号）");
        }
    }
    let cancel = CancellationToken::new();
    let stop = cancel.clone();
    let signal = tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        stop.cancel();
    });
    let result = cayw_webui::serve_with_account_settings(
        listener,
        devices,
        config.access,
        GatewayLimits::default(),
        Some(Arc::new(ownership)),
        AccountSettings {
            path: config.account_path,
            local_listener,
        },
        cancel,
    )
    .await
    .map_err(|e| format!("HTTP 服务错误：{e}"));
    signal.abort();
    result
}
#[cfg(test)]
mod tests {
    use super::*;
    fn environment() -> HashMap<String, String> {
        [
            ("CAYW_IPC_ADDR", "127.0.0.1:4100"),
            ("CAYW_IPC_TOKEN", "fedcba9876543210fedcba9876543210"),
        ]
        .into_iter()
        .map(|(k, v)| (k.into(), v.into()))
        .collect()
    }
    #[test]
    fn local_default_is_loopback_and_explicit_lan_can_have_no_account() {
        let config = Config::parse([], environment()).unwrap();
        assert_eq!(config.web_addr, "127.0.0.1:0".parse().unwrap());
        assert!(matches!(config.access, WebAccess::Local));
        let lan = Config::parse(
            ["--lan", "192.168.1.10:1234"].into_iter().map(String::from),
            environment(),
        )
        .unwrap();
        assert!(matches!(lan.access, WebAccess::Lan(None)));
        let missing = Config::parse(
            [
                "--lan",
                "127.0.0.1:0",
                "--account-file",
                "/definitely/missing/cayw-account.json",
            ]
            .into_iter()
            .map(String::from),
            environment(),
        )
        .unwrap();
        assert!(matches!(missing.access, WebAccess::Lan(None)));
        for args in [
            vec!["--web-addr", "0.0.0.0:1234"],
            vec!["--lan", "0.0.0.0:1234"],
        ] {
            assert!(Config::parse(args.into_iter().map(String::from), environment()).is_err());
        }
    }
}
