use cayw_webui::{GatewayLimits, IpcEndpoint, ProjectEndpoint};
use std::{collections::HashMap, env, net::SocketAddr, process::ExitCode};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

const HELP: &str = "CAYW 本机网页界面\n\n运行：cayw-webui [--ipc-addr 127.0.0.1:PORT] [--project ID=名称@127.0.0.1:PORT] [--web-addr 127.0.0.1:PORT]\n\n必需环境变量：\n  CAYW_IPC_ADDR   本机默认 Runtime IPC 地址；--ipc-addr 可覆盖\n  CAYW_IPC_TOKEN  宿主配置的 IPC 令牌，仅供网关使用\n  CAYW_WEB_TOKEN  独立的网页访问令牌，打开网页后手动输入\n可选环境变量：\n  CAYW_WEB_ADDR   本机 HTTP 地址，默认 127.0.0.1:0（系统分配端口）\n  CAYW_PROJECTS   额外本机项目列表（格式：id=名称@127.0.0.1:PORT，用分号分隔）\n\n只接受 loopback 地址；令牌不支持 URL 或命令行参数。两枚令牌必须独立随机生成，至少包含 128 bit 随机数据（例如 16 个随机字节编码为 32 位十六进制）；统一接受 32..4096 个不含空白或控制字符的 ASCII 字节，长度校验不代替随机生成。Ctrl+C 停止网关。\n";

struct Config {
    ipc_addr: SocketAddr,
    web_addr: SocketAddr,
    ipc_token: String,
    web_token: String,
    extra_projects: Vec<(String, String, SocketAddr)>,
}

impl Config {
    fn parse(
        args: impl IntoIterator<Item = String>,
        mut values: HashMap<String, String>,
    ) -> Result<Self, String> {
        let mut args = args.into_iter();
        let mut project_specs = Vec::new();
        if let Some(raw) = values.remove("CAYW_PROJECTS") {
            for part in raw.split(';').map(str::trim).filter(|s| !s.is_empty()) {
                project_specs.push(part.to_owned());
            }
        }
        while let Some(argument) = args.next() {
            if argument == "--project" {
                project_specs.push(args.next().ok_or("项目参数缺少值")?);
                continue;
            }
            let name = match argument.as_str() {
                "--ipc-addr" => "CAYW_IPC_ADDR",
                "--web-addr" => "CAYW_WEB_ADDR",
                _ => return Err("不支持的参数；使用 --help 查看运行方式".to_owned()),
            };
            values.insert(name.to_owned(), args.next().ok_or("地址参数缺少值")?);
        }
        let address = |name: &str, default: Option<&str>| -> Result<SocketAddr, String> {
            let value = values
                .get(name)
                .map(String::as_str)
                .or(default)
                .ok_or_else(|| format!("缺少 {name}"))?;
            let address: SocketAddr = value
                .parse()
                .map_err(|_| format!("{name} 必须是 IP:端口"))?;
            if !address.ip().is_loopback() {
                return Err(format!("{name} 只接受 loopback 地址"));
            }
            Ok(address)
        };
        let ipc_addr = address("CAYW_IPC_ADDR", None)?;
        let web_addr = address("CAYW_WEB_ADDR", Some("127.0.0.1:0"))?;
        let mut token = |name: &str| -> Result<String, String> {
            let value = values.remove(name).ok_or_else(|| format!("缺少 {name}"))?;
            if !cayw_webui::valid_token(&value) {
                return Err(format!(
                    "{name} 必须是 32..4096 字节且不含空格的 ASCII 字符串"
                ));
            }
            Ok(value)
        };
        let ipc_token = token("CAYW_IPC_TOKEN")?;
        let web_token = token("CAYW_WEB_TOKEN")?;
        if ipc_token == web_token {
            return Err("CAYW_WEB_TOKEN 必须与 CAYW_IPC_TOKEN 不同".to_owned());
        }
        let mut extra_projects = Vec::new();
        for spec in project_specs {
            let (left, addr_str) = spec
                .rsplit_once('@')
                .ok_or_else(|| "项目配置格式必须是 ID=名称@IP:端口".to_owned())?;
            let (id, label) = left
                .split_once('=')
                .ok_or_else(|| "项目配置格式必须是 ID=名称@IP:端口".to_owned())?;
            let addr: SocketAddr = addr_str
                .parse()
                .map_err(|_| "项目 IPC 地址必须是 IP:端口".to_owned())?;
            if !addr.ip().is_loopback() {
                return Err("项目 IPC 地址只接受 loopback 地址".to_owned());
            }
            if id == "default" || extra_projects.iter().any(|(existing, _, _)| existing == id) {
                return Err("项目 ID 不能重复".to_owned());
            }
            extra_projects.push((id.to_owned(), label.to_owned(), addr));
        }
        Ok(Self {
            ipc_addr,
            web_addr,
            ipc_token,
            web_token,
            extra_projects,
        })
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.as_slice() == ["--help"] || args.as_slice() == ["-h"] {
        print!("{HELP}");
        return ExitCode::SUCCESS;
    }
    match run(args).await {
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
            .filter(|(name, _)| name.starts_with("CAYW_"))
            .collect(),
    )?;
    let default_client = IpcEndpoint::new(config.ipc_addr, config.ipc_token.clone())
        .map_err(|_| "本机 IPC 地址或令牌无效".to_owned())?;
    let mut projects = vec![
        ProjectEndpoint::new("default", "默认项目", default_client)
            .map_err(|_| "默认项目配置无效".to_owned())?,
    ];
    for (id, label, addr) in config.extra_projects {
        let endpoint = IpcEndpoint::new(addr, config.ipc_token.clone())
            .map_err(|_| "项目 IPC 地址或令牌无效".to_owned())?;
        projects.push(
            ProjectEndpoint::new(id, label, endpoint)
                .map_err(|_| "项目名称或 ID 无效".to_owned())?,
        );
    }
    let listener = TcpListener::bind(config.web_addr)
        .await
        .map_err(|_| "无法监听本机网页地址".to_owned())?;
    let address = listener
        .local_addr()
        .map_err(|_| "无法读取网页监听地址".to_owned())?;
    println!("网页界面：http://{address}/");
    println!("在网页中输入 CAYW_WEB_TOKEN 的值；令牌不会写入网页地址或浏览器存储。");
    let cancel = CancellationToken::new();
    let signal_cancel = cancel.clone();
    let signal = tokio::spawn(async move {
        let result = tokio::signal::ctrl_c().await;
        signal_cancel.cancel();
        result
    });
    let result = cayw_webui::serve_projects_with_limits(
        listener,
        projects,
        config.web_token,
        GatewayLimits::default(),
        cancel,
    )
    .await
    .map_err(|_| "本机 HTTP 服务停止并返回错误".to_owned());
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
            ("CAYW_WEB_TOKEN", "0123456789abcdef0123456789abcdef"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
    }

    #[test]
    fn defaults_and_explicit_loopback_overrides() {
        let config = Config::parse([], environment()).unwrap();
        assert_eq!(config.web_addr, "127.0.0.1:0".parse().unwrap());
        let config = Config::parse(
            ["--web-addr".to_owned(), "[::1]:7000".to_owned()],
            environment(),
        )
        .unwrap();
        assert_eq!(config.web_addr, "[::1]:7000".parse().unwrap());
    }

    #[test]
    fn bad_configuration_errors_never_echo_values() {
        for (key, value) in [
            ("CAYW_WEB_ADDR", "0.0.0.0:9999"),
            ("CAYW_IPC_ADDR", "10.0.0.1:10"),
            ("CAYW_WEB_TOKEN", "secret token"),
            ("CAYW_WEB_TOKEN", "ok"),
            ("CAYW_IPC_TOKEN", "id"),
            ("CAYW_WEB_TOKEN", "fedcba9876543210fedcba9876543210"),
        ] {
            let mut values = environment();
            values.insert(key.to_owned(), value.to_owned());
            let error = Config::parse([], values).err().unwrap();
            assert!(!error.contains(value));
        }
        assert!(
            Config::parse(
                ["--web-token".to_owned(), "private".to_owned()],
                environment()
            )
            .is_err()
        );
    }
}
