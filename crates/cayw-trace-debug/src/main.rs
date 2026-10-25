use cayw_trace_debug::{Error, TraceStore};
use std::{env, net::SocketAddr, path::PathBuf, process::ExitCode, sync::Arc, time::Duration};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

const HELP: &str = "Trace Debug 本机插件\n\n运行：cayw-trace-debug --store PATH --session SESSION_ID [--ipc-addr 127.0.0.1:PORT] [--web-addr 127.0.0.1:PORT]\n\n环境变量：CAYW_IPC_ADDR、CAYW_IPC_TOKEN、CAYW_TRACE_WEB_TOKEN。\n网页令牌须与 IPC 令牌不同，均至少32位不含空白的ASCII字符。令牌仅从环境读取，打开网页后手动输入网页令牌。\n--session 可重复，分别保存连续游标。只观察事件；节点保存只在本地重放，不创建Runtime任务，不重复业务写入。\n追踪文件含运行输入、源码与结果，仅保存本机。Ctrl+C 停止插件。\n";

struct Config {
    ipc: SocketAddr,
    web: SocketAddr,
    ipc_token: String,
    web_token: String,
    store: PathBuf,
    sessions: Vec<String>,
}
impl Config {
    fn parse() -> Result<Self, Error> {
        let mut args = env::args().skip(1);
        let mut ipc = env::var("CAYW_IPC_ADDR").ok();
        let mut web = "127.0.0.1:0".to_owned();
        let mut store = None;
        let mut sessions = vec![];
        while let Some(name) = args.next() {
            let value = args
                .next()
                .ok_or_else(|| Error::Invalid("参数缺少值，使用 --help 查看用法".into()))?;
            match name.as_str() {
                "--store" => store = Some(PathBuf::from(value)),
                "--session" if !value.is_empty() && value.len() <= 128 => sessions.push(value),
                "--ipc-addr" => ipc = Some(value),
                "--web-addr" => web = value,
                _ => return Err(Error::Invalid("参数无效，使用 --help 查看用法".into())),
            }
        }
        fn address(value: &str) -> Result<SocketAddr, Error> {
            let addr: SocketAddr = value
                .parse()
                .map_err(|_| Error::Invalid("地址必须是本机 IP:端口".into()))?;
            if !addr.ip().is_loopback() {
                return Err(Error::Invalid("只接受本机 loopback 地址".into()));
            }
            Ok(addr)
        }
        fn token(name: &str) -> Result<String, Error> {
            let value = env::var(name).map_err(|_| Error::Invalid(format!("缺少 {name}")))?;
            if !(32..=4096).contains(&value.len()) || !value.bytes().all(|b| b.is_ascii_graphic()) {
                return Err(Error::Invalid(format!(
                    "{name} 需要32..4096位不含空白的ASCII字符"
                )));
            }
            Ok(value)
        }
        sessions.sort();
        sessions.dedup();
        if sessions.is_empty() || sessions.len() > 16 {
            return Err(Error::Invalid("需要1..16个 --session".into()));
        }
        let ipc_token = token("CAYW_IPC_TOKEN")?;
        let web_token = token("CAYW_TRACE_WEB_TOKEN")?;
        if ipc_token == web_token {
            return Err(Error::Invalid("网页令牌必须与IPC令牌不同".into()));
        }
        Ok(Self {
            ipc: address(
                &ipc.ok_or_else(|| Error::Invalid("缺少 CAYW_IPC_ADDR 或 --ipc-addr".into()))?,
            )?,
            web: address(&web)?,
            ipc_token,
            web_token,
            store: store.ok_or_else(|| Error::Invalid("缺少 --store".into()))?,
            sessions,
        })
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    if env::args()
        .skip(1)
        .any(|arg| arg == "--help" || arg == "-h")
    {
        print!("{HELP}");
        return ExitCode::SUCCESS;
    }
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Trace 插件停止：{error}");
            ExitCode::FAILURE
        }
    }
}

pub(crate) async fn observe(
    store: Arc<TraceStore>,
    address: SocketAddr,
    token: String,
    session: String,
    cancel: CancellationToken,
) -> Result<(), Error> {
    let mut backoff = Duration::from_millis(200);
    while !cancel.is_cancelled() {
        let client = match cayw_ipc::Client::connect(address, token.clone()).await {
            Ok(client) => client,
            Err(error) => {
                store.connection_error(
                    &session,
                    Some(format!(
                        "运行事件连接中断（{}）；正在按已保存游标重连",
                        error.code
                    )),
                )?;
                tokio::select! { _ = cancel.cancelled() => break, _ = tokio::time::sleep(backoff) => {} }
                backoff = (backoff * 2).min(Duration::from_secs(5));
                continue;
            }
        };
        let health = client
            .request("runtime.health", serde_json::json!({}))
            .await;
        if !health
            .as_ref()
            .is_ok_and(|health| health["protocol_version"] == 3)
        {
            client.close().await;
            return Err(Error::Invalid(
                "Runtime 事件协议不受支持，未开始采集".into(),
            ));
        }
        backoff = Duration::from_millis(200);
        let mut limit = 32_u64;
        loop {
            let after = store
                .snapshot()
                .sessions
                .get(&session)
                .map_or(0, |capture| capture.after);
            let response = client
                .request_with_cancel(
                    "session.events",
                    serde_json::json!({"session_id":session,"after":after,"limit":limit,"wait_ms":1000}),
                    cancel.clone(),
                )
                .await;
            if cancel.is_cancelled() {
                break;
            }
            match response {
                Ok(value) => {
                    let events: Vec<cayw_core::EventRecord> = serde_json::from_value(value)?;
                    store.ingest(&events)?;
                    store.connection_error(&session, None)?;
                    limit = 32;
                }
                Err(error) if error.code == "response_too_large_outcome_unknown" && limit > 1 => {
                    // This read produced no event. Ask for fewer events at the same cursor.
                    limit /= 2;
                }
                Err(error) if error.code == "response_too_large_outcome_unknown" => {
                    store.connection_error(
                        &session,
                        Some(format!(
                            "单条事件超过 IPC 响应上限（{}）；游标保留为 {after}",
                            error.code
                        )),
                    )?;
                    tokio::select! {
                        _ = cancel.cancelled() => break,
                        _ = tokio::time::sleep(Duration::from_secs(5)) => {}
                    }
                }
                Err(error) => {
                    store.connection_error(
                        &session,
                        Some(format!(
                            "事件采集暂停（{}）；游标保留为 {after}",
                            error.code
                        )),
                    )?;
                    break;
                }
            }
        }
        client.close().await;
        tokio::select! { _ = cancel.cancelled() => break, _ = tokio::time::sleep(backoff) => {} }
    }
    Ok(())
}

async fn run() -> Result<(), Error> {
    let config = Config::parse()?;
    let store = TraceStore::open(config.store)?;
    let listener = TcpListener::bind(config.web).await?;
    let address = listener.local_addr()?;
    let cancel = CancellationToken::new();
    let signal_cancel = cancel.clone();
    let signal = tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        signal_cancel.cancel();
    });
    let mut collectors = tokio::task::JoinSet::new();
    for session in config.sessions {
        let store = store.clone();
        let token = config.ipc_token.clone();
        let cancel = cancel.clone();
        collectors.spawn(observe(store, config.ipc, token, session, cancel));
    }
    println!(
        "{}",
        serde_json::json!({"web_url":format!("http://{address}/"),"mode":"trace_debug","schema":"cayw.trace.v1"})
    );
    let service =
        cayw_trace_debug::server::serve(listener, store, config.web_token, cancel.clone());
    tokio::pin!(service);
    let result = tokio::select! {
        result = &mut service => result,
        result = collectors.join_next() => {
            cancel.cancel();
            let _ = service.await;
            match result {
                Some(Ok(result)) => result,
                Some(Err(_)) => Err(Error::Invalid("事件采集任务停止".into())),
                None => Ok(()),
            }
        }
    };
    cancel.cancel();
    while let Some(joined) = collectors.join_next().await {
        if let Ok(Err(error)) = joined {
            eprintln!("事件采集已停止：{error}");
        }
    }
    signal.abort();
    result
}
