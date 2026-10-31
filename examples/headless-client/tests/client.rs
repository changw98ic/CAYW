use serde_json::{Value, json};
use std::{
    path::Path,
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    process::Command,
    task::JoinHandle,
};

const TOKEN: &str = "public-client-test-token";
const WATCHDOG: Duration = Duration::from_secs(30);

struct TestServerHandler {
    runtime: Arc<cayw_runtime::Runtime>,
    request_timeout_ms: u64,
}

#[async_trait::async_trait]
impl cayw_ipc::RpcHandler for TestServerHandler {
    async fn call(
        &self,
        method: &str,
        params: Value,
        cancel: cayw_ai::CancellationToken,
    ) -> Result<Value, cayw_ipc::RpcError> {
        let mut response = self.runtime.call(method, params, cancel).await?;
        if method == "runtime.health"
            && let Value::Object(ref mut map) = response
        {
            map.insert("request_timeout_ms".into(), json!(self.request_timeout_ms));
        }
        Ok(response)
    }
}

fn serve_test_runtime(
    listener: TcpListener,
    runtime: Arc<cayw_runtime::Runtime>,
    token: &str,
    timeout: Duration,
    cancel: cayw_ai::CancellationToken,
) -> JoinHandle<std::io::Result<()>> {
    let mut config = cayw_ipc::ServerConfig::new(token);
    config.request_timeout = timeout;
    let timeout_ms = u64::try_from(timeout.as_millis()).unwrap();
    let handler = Arc::new(TestServerHandler {
        runtime,
        request_timeout_ms: timeout_ms,
    });
    tokio::spawn(cayw_ipc::serve(listener, handler, config, cancel))
}
fn command(root: &Path, mode: &str, address: Option<std::net::SocketAddr>) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cayw-client"));
    command.env_clear().args(["--mode", mode]);
    if let Some(address) = address {
        command.args([
            "--address",
            &address.to_string(),
            "--token-env",
            "CLIENT_TEST_TOKEN",
        ]);
        command.env("CLIENT_TEST_TOKEN", TOKEN);
    } else {
        command.arg("--db").arg(root.join("session.sqlite"));
        command.arg("--workspace").arg(root.join("workspace"));
        command.arg("--config").arg(root.join("runtime.json"));
    }
    command
}
async fn run(mut command: Command) -> Vec<Value> {
    let output = tokio::time::timeout(WATCHDOG, command.output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        output.status.success(),
        "status={} stderr={} stdout={}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}
#[tokio::test]
async fn help_and_invalid_input_never_need_model_credentials() {
    let output = Command::new(env!("CARGO_BIN_EXE_cayw-client"))
        .env_clear()
        .arg("--help")
        .output()
        .await
        .unwrap();
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    for expected in [
        "cayw-client [connection options] COMMAND",
        "Default: discover cayw serve for the current workspace via ~/.cayw (or CAYW_HOME).",
        "IPC: [--mode ipc]",
        "SDK: --mode sdk --db PATH --workspace DIR",
    ] {
        assert!(
            help.contains(expected),
            "missing independently usable client help {expected:?}: {help}"
        );
    }
    let output = Command::new(env!("CARGO_BIN_EXE_cayw-client"))
        .env_clear()
        .args([
            "--mode",
            "ipc",
            "--address",
            "127.0.0.1:1",
            "--token-env",
            "CLIENT_TEST_TOKEN",
            "prompt",
            "--session",
            "original-session",
            "--text",
            "test",
            "--extra",
            "secret-must-not-print",
        ])
        .env("CLIENT_TEST_TOKEN", "token-must-not-print")
        .output()
        .await
        .unwrap();
    assert!(!output.status.success());
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error["session_id"], "original-session");
    assert!(!String::from_utf8_lossy(&output.stderr).contains("secret-must-not-print"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("token-must-not-print"));
}

async fn request(
    mut socket: TcpStream,
    requests: Arc<Mutex<Vec<Value>>>,
    continue_stream: Arc<tokio::sync::Notify>,
) {
    let mut bytes = Vec::new();
    let header_end = loop {
        let mut piece = [0; 4096];
        let size = socket.read(&mut piece).await.unwrap();
        assert_ne!(size, 0);
        bytes.extend_from_slice(&piece[..size]);
        if let Some(offset) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break offset + 4;
        }
    };
    let headers = String::from_utf8_lossy(&bytes[..header_end]);
    let length: usize = headers
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(|n| n.trim().parse().unwrap())
        })
        .unwrap();
    while bytes.len() < header_end + length {
        let mut piece = [0; 4096];
        let size = socket.read(&mut piece).await.unwrap();
        assert_ne!(size, 0);
        bytes.extend_from_slice(&piece[..size]);
    }
    let body: Value = serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap();
    requests.lock().unwrap().push(body.clone());
    let messages = body["messages"].as_array().unwrap();
    let user_text = messages
        .iter()
        .rev()
        .find(|message| message["role"] == "user")
        .and_then(|message| message["content"].as_array())
        .and_then(|content| content.first())
        .and_then(|part| part["text"].as_str());
    if matches!(
        user_text,
        Some("cancel the held stream" | "close output after partial")
    ) {
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n").await.unwrap();
        let part = format!(
            "data: {}\n\n",
            json!({"id":"held-response","model":"fixture-response","choices":[{"index":0,"delta":{"content":"partial before cancellation"},"finish_reason":null}]})
        );
        socket
            .write_all(format!("{:x}\r\n{}\r\n", part.len(), part).as_bytes())
            .await
            .unwrap();
        if user_text == Some("close output after partial") {
            continue_stream.notified().await;
            let more = format!(
                "data: {}\n\n",
                json!({"id":"held-response","model":"fixture-response","choices":[{"index":0,"delta":{"content":"next after closed output"},"finish_reason":null}]})
            );
            socket
                .write_all(format!("{:x}\r\n{}\r\n", more.len(), more).as_bytes())
                .await
                .unwrap();
        }
        let mut discarded = [0; 1];
        let _ = tokio::time::timeout(WATCHDOG, socket.read(&mut discarded)).await;
        return;
    }
    if user_text == Some("fail the provider") {
        socket.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
        return;
    }
    let users = messages.iter().filter(|m| m["role"] == "user").count();
    let held_process = user_text == Some("cancel a held process");
    // Keep this fixture in its first tool wait while the test observes the
    // actual child process before cancelling the original task. This fixes
    // the fixture condition; it does not establish the cause of an earlier
    // intermittent extra model request.
    let yield_time_ms = if held_process { 30000 } else { 1000 };
    let script = if held_process {
        "return cayw.call('test.run',{profile='hold'})"
    } else {
        "return cayw.call('fs.write',{path='fact.txt',content='41'})"
    };
    let page = messages
        .last()
        .filter(|message| message["role"] == "tool")
        .and_then(|message| message["content"].as_array())
        .and_then(|parts| parts.first())
        .and_then(|part| part["text"].as_str())
        .and_then(|text| serde_json::from_str::<Value>(text).ok());
    let deltas = if let Some(page) = &page {
        if page["vm_alive"] == true || page["has_more"] == true {
            vec![
                json!({"tool_calls":[{"index":0,"id":format!("wait-fact-{}",messages.len()),"type":"function","function":{"name":"execute_code","arguments":json!({"action":"wait","cell_id":page["cell_id"],"cursor":page["next_cursor"],"yield_time_ms":30000,"max_output_tokens":4096}).to_string()}}]}),
            ]
        } else {
            vec![json!({"content":"Saved 41."})]
        }
    } else if users == 1 {
        vec![
            json!({"tool_calls":[{"index":0,"id":"write-fact","type":"function","function":{"name":"execute_code","arguments":json!({"action":"exec","script":script,"yield_time_ms":yield_time_ms,"max_output_tokens":4096}).to_string()}}]}),
        ]
    } else {
        vec![
            json!({"content":"The previous fact was 41; "}),
            json!({"content":"41 + 1 = 42."}),
        ]
    };
    let tool = deltas[0].get("tool_calls").is_some();
    if held_process {
        assert!(tool, "cancelled process requested another model answer");
    }
    let mut sse = String::new();
    for delta in deltas {
        sse.push_str(&format!("data: {}\n\n", json!({"id":format!("response-{users}-{}",messages.len()),"model":"fixture-response","choices":[{"index":0,"delta":delta,"finish_reason":null}]})));
    }
    sse.push_str(&format!("data: {}\n\n",json!({"choices":[{"index":0,"delta":{},"finish_reason":if tool {"tool_calls"} else {"stop"}}]})));
    sse.push_str("data: {\"choices\":[],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":7}}\n\ndata: [DONE]\n\n");
    socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",sse.len(),sse).as_bytes()).await.unwrap();
}
struct Fixture {
    base: String,
    requests: Arc<Mutex<Vec<Value>>>,
    continue_stream: Arc<tokio::sync::Notify>,
    task: JoinHandle<()>,
}
impl Fixture {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let continue_stream = Arc::new(tokio::sync::Notify::new());
        let continued = continue_stream.clone();
        let task = tokio::spawn(async move {
            loop {
                let (socket, _) = listener.accept().await.unwrap();
                tokio::spawn(request(socket, captured.clone(), continued.clone()));
            }
        });
        Self {
            base,
            requests,
            continue_stream,
            task,
        }
    }
    fn model_configuration(&self) -> Value {
        json!({"service":"openai", "api":"chat_completions", "id":"fixture-route",
            "base_url":self.base, "api_version":"v1", "max_output_tokens":4096,
            "api_key_env":"CAYW_CLIENT_TEST_KEY"})
    }
    fn configured_provider(&self) -> cayw_runtime::ConfiguredProvider {
        serde_json::from_value::<cayw_runtime::ProviderConfig>(self.model_configuration())
            .unwrap()
            .build_with_environment(|name| {
                assert_eq!(name, "CAYW_CLIENT_TEST_KEY");
                Some("public-http-fixture-key".into())
            })
            .unwrap()
    }
    fn configure(&self, command: &mut Command, root: &Path) {
        let configuration = root.join("runtime.json");
        if !configuration.exists() {
            std::fs::write(
                configuration,
                serde_json::to_vec(&json!({"model":self.model_configuration()})).unwrap(),
            )
            .unwrap();
        }
        command.env("CAYW_CLIENT_TEST_KEY", "public-http-fixture-key");
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}
fn last<'a>(lines: &'a [Value], kind: &str) -> &'a Value {
    lines
        .iter()
        .rev()
        .find(|line| line["type"] == kind)
        .unwrap_or_else(|| panic!("missing {kind}: {lines:?}"))
}
async fn line(reader: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>) -> Value {
    serde_json::from_str(
        &tokio::time::timeout(WATCHDOG, reader.next_line())
            .await
            .unwrap()
            .unwrap()
            .expect("client exited before lifecycle result"),
    )
    .unwrap()
}
async fn check_two_turns(mode: &str) {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("workspace")).unwrap();
    let fixture = Fixture::start().await;
    let mut runtime_server = None;
    let address = if mode == "ipc" {
        let configured = fixture.configured_provider();
        let runtime = cayw_runtime::Runtime::open(
            root.path().join("session.sqlite"),
            root.path().join("workspace"),
            configured.provider,
            configured.model,
            cayw_runtime::RuntimeConfig::default(),
        )
        .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let cancel = cayw_ai::CancellationToken::new();
        let task = serve_test_runtime(
            listener,
            Arc::new(runtime),
            TOKEN,
            Duration::from_secs(360),
            cancel.clone(),
        );
        runtime_server = Some((cancel, task));
        Some(address)
    } else {
        None
    };
    let make = || {
        let mut cmd = command(root.path(), mode, address);
        fixture.configure(&mut cmd, root.path());
        cmd
    };
    let mut create = make();
    create.args(["create", "--label", "two actual user turns"]);
    let result = run(create).await;
    let id = last(&result, "result")["result"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let cursor_path = root.path().join("processed-cursor.json");
    let mut prompt = make();
    prompt
        .args([
            "prompt",
            "--session",
            &id,
            "--text",
            "Save the fact 41 using a tool.",
            "--cursor-file",
        ])
        .arg(&cursor_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = prompt.spawn().unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut observed = Vec::new();
    let approval = loop {
        let event = line(&mut reader).await;
        let found =
            event["type"] == "event" && event["event"]["event"]["type"] == "approval_requested";
        observed.push(event.clone());
        if found {
            break event["event"]["event"]["approval"]["id"]
                .as_str()
                .unwrap()
                .to_owned();
        }
    };
    assert!(
        !root.path().join("workspace/fact.txt").exists(),
        "effect happened without trusted approval"
    );
    let mut approve = make();
    approve.args([
        "approve",
        "--session",
        &id,
        "--approval",
        &approval,
        "--decision",
        "allow_once",
    ]);
    run(approve).await;
    loop {
        let event = line(&mut reader).await;
        let done = event["type"] == "prompt_finished";
        observed.push(event);
        if done {
            break;
        }
    }
    assert!(
        tokio::time::timeout(WATCHDOG, child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    assert_eq!(
        std::fs::read(root.path().join("workspace/fact.txt")).unwrap(),
        b"41"
    );
    let first = last(&observed, "prompt_finished");
    assert_eq!(first["task"]["state"], "completed");
    let saved: Value = serde_json::from_slice(&std::fs::read(&cursor_path).unwrap()).unwrap();
    assert_eq!(saved["session_id"], id);
    let after = saved["after"].as_u64().unwrap();
    let mut second = make();
    second
        .args([
            "prompt",
            "--session",
            &id,
            "--text",
            "Use the previous saved fact and add one.",
            "--cursor-file",
        ])
        .arg(&cursor_path);
    let second = run(second).await;
    assert!(
        second
            .iter()
            .filter(|line| line["type"] == "event")
            .all(|line| line["event"]["cursor"].as_u64().unwrap() > after)
    );
    let finish = last(&second, "prompt_finished");
    assert_eq!(finish["task"]["state"], "completed");
    assert!(
        finish["session"]["messages"]
            .to_string()
            .contains("41 + 1 = 42")
    );
    let mut get = make();
    get.args(["get", "--session", &id]);
    let reopened = run(get).await;
    assert_eq!(last(&reopened, "result")["result"]["id"], id);
    assert_eq!(
        last(&reopened, "result")["result"]["messages"],
        finish["session"]["messages"]
    );
    let mut tasks = make();
    tasks.args(["tasks", "--session", &id]);
    let tasks = run(tasks).await;
    assert_eq!(
        last(&tasks, "result")["result"].as_array().unwrap().len(),
        2
    );
    let mut events = make();
    events.args([
        "events",
        "--session",
        &id,
        "--after",
        "0",
        "--limit",
        "1000",
    ]);
    let full = run(events).await;
    let all: Vec<_> = full.iter().filter(|line| line["type"] == "event").collect();
    assert!(!all.is_empty());
    for (index, event) in all.iter().enumerate() {
        assert_eq!(event["event"]["cursor"], index + 1);
    }
    let mut suffix = make();
    suffix
        .args(["events", "--session", &id, "--cursor-file"])
        .arg(&cursor_path);
    let suffix = run(suffix).await;
    assert!(suffix.iter().all(|line| line["type"] != "event"));
    let requests = fixture.requests.lock().unwrap().clone();
    assert!(
        requests.len() >= 3,
        "two user turns must include tool consumption; model request count differs from user turn count"
    );
    let resumed = &requests.last().unwrap()["messages"].as_array().unwrap();
    assert_eq!(
        resumed
            .iter()
            .filter(|message| message["role"] == "user")
            .count(),
        2
    );
    assert!(
        resumed.iter().any(|message| message["role"] == "tool"),
        "second turn lost the original tool result: {resumed:?}"
    );
    assert!(
        resumed
            .iter()
            .any(|message| message["role"] == "assistant" && message["content"] == "Saved 41."),
        "second turn lost the original fact: {resumed:?}"
    );
    drop(requests);
    if let Some((cancel, task)) = runtime_server {
        cancel.cancel();
        task.await.unwrap().unwrap();
    }
}
#[tokio::test]
async fn sdk_two_turns_require_approval_and_preserve_history_and_cursor_across_processes() {
    check_two_turns("sdk").await;
}
#[tokio::test]
async fn ipc_two_turns_require_approval_and_preserve_history_and_cursor_across_processes() {
    check_two_turns("ipc").await;
}

async fn check_unknown_rejection(mode: &str) {
    // A missing RPC unknown suffix or a dropped client flag must not turn an
    // unresolved original effect into a known failure that appears safe to retry.
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("workspace")).unwrap();
    let fixture = Fixture::start().await;
    let configuration = root.path().join("runtime.json");
    std::fs::write(
        &configuration,
        serde_json::to_vec(&json!({
            "model":fixture.model_configuration(),
            "profiles":{"hold":{"kind":"test","program":"/bin/sleep","args":["30"],
                "timeout_ms":35000,"max_output_bytes":4096,"io":{"kind":"pipes"}}},
            "permissions":[{"capability":"test.run","arguments":{"profile":"hold"}}]
        }))
        .unwrap(),
    )
    .unwrap();
    let configured = fixture.configured_provider();
    let runtime = cayw_runtime::Runtime::open(
        root.path().join("session.sqlite"),
        root.path().join("workspace"),
        configured.provider,
        configured.model,
        cayw_runtime::HeadlessConfig::load(&configuration)
            .unwrap()
            .into_runtime_config()
            .unwrap(),
    )
    .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let shutdown = cayw_ai::CancellationToken::new();
    let server = serve_test_runtime(
        listener,
        Arc::new(runtime),
        TOKEN,
        Duration::from_secs(360),
        shutdown.clone(),
    );
    let make = || {
        let mut cmd = command(root.path(), mode, (mode == "ipc").then_some(address));
        fixture.configure(&mut cmd, root.path());
        cmd
    };
    let mut create = make();
    create.arg("create");
    let created = run(create).await;
    let id = last(&created, "result")["result"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut prompt = make();
    prompt
        .args([
            "prompt",
            "--session",
            &id,
            "--text",
            "cancel a held process",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = prompt.spawn().unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap()).lines();
    let original = loop {
        let record = line(&mut reader).await;
        if record["type"] == "task" {
            break record["task"].clone();
        }
    };
    let terminal = tokio::spawn(async move {
        loop {
            let record = line(&mut reader).await;
            if record["type"] == "prompt_finished" {
                return record;
            }
        }
    });
    let parent = if mode == "sdk" {
        child.id().unwrap()
    } else {
        std::process::id()
    };
    let processes = tokio::time::timeout(WATCHDOG, async {
        loop {
            let output = Command::new("/bin/ps")
                .env_clear()
                .args(["-axo", "pid=,ppid=,command="])
                .output()
                .await
                .unwrap();
            assert!(output.status.success());
            let processes: Vec<_> = String::from_utf8(output.stdout)
                .unwrap()
                .lines()
                .filter_map(|line| {
                    let mut fields = line.split_whitespace();
                    let pid: u32 = fields.next()?.parse().ok()?;
                    let ppid: u32 = fields.next()?.parse().ok()?;
                    let program = fields.next()?;
                    (ppid == parent && program == "/bin/sleep" && fields.next() == Some("30"))
                        .then_some(pid)
                })
                .collect();
            if !processes.is_empty() {
                return processes;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(processes.len(), 1);
    let mut tasks = make();
    tasks.args(["tasks", "--session", &id]);
    let before = run(tasks).await;
    let before = last(&before, "result")["result"][0].clone();
    assert_eq!(before["id"], original["id"]);
    assert_eq!(before["state"], "running");
    let mut cancel = make();
    cancel.args([
        "cancel",
        "--session",
        &id,
        "--task",
        original["id"].as_str().unwrap(),
    ]);
    assert_eq!(
        last(&run(cancel).await, "result")["result"]["cancelled"],
        true
    );
    let terminal = tokio::time::timeout(WATCHDOG, terminal)
        .await
        .unwrap()
        .unwrap();
    assert!(
        !tokio::time::timeout(WATCHDOG, child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    assert_eq!(terminal["task"]["state"], "outcome_unknown");
    assert_eq!(terminal["session"]["state"], "needs_reconciliation");
    for field in ["id", "attempt_id", "worker_id", "fence"] {
        assert_eq!(terminal["task"][field], before[field]);
    }
    let mut tasks = make();
    tasks.args(["tasks", "--session", &id]);
    let baseline = run(tasks).await;
    let baseline = last(&baseline, "result")["result"].clone();
    let captured = fixture.requests.lock().unwrap().clone();
    let request_count = captured.len();
    assert_eq!(request_count, 1, "cancel fixture requests: {captured:?}");
    let mut repeat = make();
    repeat.args(["prompt", "--session", &id, "--text", "do not replay"]);
    let rejected = tokio::time::timeout(WATCHDOG, repeat.output())
        .await
        .unwrap()
        .unwrap();
    assert!(!rejected.status.success());
    let error: Value = serde_json::from_slice(&rejected.stderr).unwrap();
    let peer = cayw_ipc::Client::connect(address, TOKEN).await.unwrap();
    let rpc_error = peer
        .request(
            "task.enqueue",
            json!({"session_id":id,"text":"do not replay"}),
        )
        .await
        .unwrap_err();
    let mut tasks = make();
    tasks.args(["tasks", "--session", &id]);
    let after = run(tasks).await;
    assert_eq!(last(&after, "result")["result"], baseline);
    assert_eq!(fixture.requests.lock().unwrap().len(), request_count);
    peer.close().await;
    shutdown.cancel();
    server.await.unwrap().unwrap();
    assert_eq!(error["session_id"], id);
    assert_eq!(error["code"], rpc_error.code, "{mode}: {error}");
    assert_eq!(error["outcome_unknown"], true, "{mode}: {error}");
    assert_eq!(rpc_error.code, "needs_reconciliation_outcome_unknown");
    assert!(rpc_error.outcome_unknown);
}
#[tokio::test]
async fn sdk_unknown_rejection_preserves_identity_and_never_replays() {
    check_unknown_rejection("sdk").await;
}
#[tokio::test]
async fn ipc_unknown_rejection_preserves_identity_and_never_replays() {
    check_unknown_rejection("ipc").await;
}

async fn check_ctrl_c_cancel(mode: &str) {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("workspace")).unwrap();
    let fixture = Fixture::start().await;
    let mut runtime_server = None;
    let address = if mode == "ipc" {
        let configured = fixture.configured_provider();
        let runtime = cayw_runtime::Runtime::open(
            root.path().join("session.sqlite"),
            root.path().join("workspace"),
            configured.provider,
            configured.model,
            cayw_runtime::RuntimeConfig::default(),
        )
        .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let cancel = cayw_ai::CancellationToken::new();
        let task = serve_test_runtime(
            listener,
            Arc::new(runtime),
            TOKEN,
            Duration::from_secs(360),
            cancel.clone(),
        );
        runtime_server = Some((cancel, task));
        Some(address)
    } else {
        None
    };
    let make = || {
        let mut command = command(root.path(), mode, address);
        fixture.configure(&mut command, root.path());
        command
    };
    let mut create = make();
    create.arg("create");
    let created = run(create).await;
    let id = last(&created, "result")["result"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut prompt = make();
    prompt
        .args([
            "prompt",
            "--session",
            &id,
            "--text",
            "cancel the held stream",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = prompt.spawn().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut records = vec![];
    loop {
        let record = line(&mut lines).await;
        let partial = record.to_string().contains("partial before cancellation");
        records.push(record);
        if partial {
            break;
        }
    }
    assert!(
        Command::new("/bin/kill")
            .arg("-INT")
            .arg(child.id().unwrap().to_string())
            .status()
            .await
            .unwrap()
            .success()
    );
    loop {
        let text = tokio::time::timeout(WATCHDOG, lines.next_line())
            .await
            .unwrap()
            .unwrap();
        let Some(text) = text else {
            let mut stderr = String::new();
            child
                .stderr
                .take()
                .unwrap()
                .read_to_string(&mut stderr)
                .await
                .unwrap();
            panic!("client ended before terminal: stderr={stderr}, records={records:?}");
        };
        let record: Value = serde_json::from_str(&text).unwrap();
        let done = record["type"] == "prompt_finished";
        records.push(record);
        if done {
            break;
        }
    }
    assert!(
        !tokio::time::timeout(WATCHDOG, child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    let initial_task = records
        .iter()
        .find(|record| record["type"] == "task")
        .unwrap()["task"]
        .clone();
    let requested = records
        .iter()
        .position(|record| record["type"] == "cancel_requested")
        .unwrap();
    let ack = records
        .iter()
        .position(|record| record["type"] == "cancel_ack")
        .unwrap();
    let finish = records
        .iter()
        .position(|record| record["type"] == "prompt_finished")
        .unwrap();
    assert!(requested < ack && ack < finish);
    assert_eq!(records[ack]["cancelled"], true);
    let target = &records[ack]["target"];
    assert_eq!(target["task_id"], initial_task["id"]);
    assert_eq!(target["task_id"], records[finish]["task"]["id"]);
    for field in ["session_id", "worker_id", "attempt_id", "fence"] {
        assert_eq!(target[field], records[finish]["task"][field]);
    }
    assert!(target["fence"].as_u64().unwrap() > 0);
    assert_eq!(records[finish]["task"]["state"], "cancelled");
    assert_eq!(records[finish]["task"]["cancel_requested"], true);
    assert_eq!(
        fixture.requests.lock().unwrap().len(),
        1,
        "client retried a cancelled request"
    );
    let mut audit = make();
    audit.args(["audit", "--session", &id]);
    let audit = run(audit).await;
    assert!(
        last(&audit, "result")["result"]
            .as_array()
            .unwrap()
            .iter()
            .any(|record| record["kind"] == "run_cancelled")
    );
    if let Some((cancel, task)) = runtime_server {
        cancel.cancel();
        task.await.unwrap().unwrap();
    }
}

#[tokio::test]
async fn sdk_ctrl_c_reports_cancel_ack_then_original_task_terminal_without_retry() {
    check_ctrl_c_cancel("sdk").await;
}

#[tokio::test]
async fn ipc_ctrl_c_reports_cancel_ack_then_original_task_terminal_without_retry() {
    check_ctrl_c_cancel("ipc").await;
}

#[tokio::test]
async fn broken_stdout_cancels_original_worker_without_stranding_a_live_task() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("workspace")).unwrap();
    let fixture = Fixture::start().await;
    let make = || {
        let mut command = command(root.path(), "sdk", None);
        fixture.configure(&mut command, root.path());
        command
    };
    let mut create = make();
    create.arg("create");
    let created = run(create).await;
    let id = last(&created, "result")["result"]["id"].as_str().unwrap();
    let mut prompt = make();
    prompt
        .args([
            "prompt",
            "--session",
            id,
            "--text",
            "close output after partial",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = prompt.spawn().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    loop {
        let event = line(&mut lines).await;
        if event.to_string().contains("partial before cancellation") {
            break;
        }
    }
    drop(lines);
    fixture.continue_stream.notify_one();
    assert!(
        !tokio::time::timeout(WATCHDOG, child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    let mut tasks = make();
    tasks.args(["tasks", "--session", id]);
    let tasks = run(tasks).await;
    let task = &last(&tasks, "result")["result"][0];
    assert_eq!(
        task["state"], "cancelled",
        "output failure abandoned original worker: {task:?}"
    );
    assert_eq!(task["cancel_requested"], true);
}

#[tokio::test]
async fn failed_task_returns_nonzero_and_foreign_cursor_blocks_new_dispatch() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("workspace")).unwrap();
    let fixture = Fixture::start().await;
    let make = || {
        let mut command = command(root.path(), "sdk", None);
        fixture.configure(&mut command, root.path());
        command
    };
    let mut create = make();
    create.arg("create");
    let created = run(create).await;
    let id = last(&created, "result")["result"]["id"].as_str().unwrap();
    let cursor = root.path().join("foreign.json");
    std::fs::write(&cursor, b"{\"session_id\":\"another-session\",\"after\":0}").unwrap();
    let mut blocked = make();
    blocked
        .args([
            "prompt",
            "--session",
            id,
            "--text",
            "must not execute",
            "--cursor-file",
        ])
        .arg(&cursor);
    let blocked = blocked.output().await.unwrap();
    assert!(!blocked.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&blocked.stderr).unwrap()["session_id"],
        id
    );
    assert!(fixture.requests.lock().unwrap().is_empty());
    let mut failed = make();
    failed.args(["prompt", "--session", id, "--text", "fail the provider"]);
    let failed = tokio::time::timeout(WATCHDOG, failed.output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        !failed.status.success(),
        "Ok(SessionView) incorrectly reported a failed task as completed"
    );
    let output = String::from_utf8(failed.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(last(&output, "prompt_finished")["task"]["state"], "failed");
    let captured = fixture.requests.lock().unwrap().clone();
    assert_eq!(
        captured.len(),
        1,
        "client retried provider failure: {captured:?}"
    );
    let mut forged = make();
    forged.args(["events", "--session", id, "--after", "18446744073709551615"]);
    let forged = forged.output().await.unwrap();
    assert!(!forged.status.success());
    assert_eq!(fixture.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn unreachable_cursor_destination_rejects_before_a_task_is_admitted() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("workspace")).unwrap();
    let fixture = Fixture::start().await;
    let make = || {
        let mut command = command(root.path(), "sdk", None);
        fixture.configure(&mut command, root.path());
        command
    };
    let mut create = make();
    create.arg("create");
    let created = run(create).await;
    let id = last(&created, "result")["result"]["id"].as_str().unwrap();
    let path = root.path().join("missing-parent/cursor.json");
    let mut prompt = make();
    prompt
        .args([
            "prompt",
            "--session",
            id,
            "--text",
            "must not start",
            "--cursor-file",
        ])
        .arg(path);
    let output = prompt.output().await.unwrap();
    assert!(!output.status.success());
    let mut tasks = make();
    tasks.args(["tasks", "--session", id]);
    let tasks = run(tasks).await;
    assert_eq!(last(&tasks, "result")["result"], json!([]));
    assert!(fixture.requests.lock().unwrap().is_empty());
}
