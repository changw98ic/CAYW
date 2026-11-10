#![cfg(target_os = "macos")]

use cayw_ipc::Client;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    ffi::OsStr,
    fs::{self, OpenOptions},
    io::Write,
    os::unix::{
        ffi::OsStrExt,
        fs::{OpenOptionsExt, PermissionsExt},
    },
    path::Path,
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    process::{Child, Command},
};

const WATCHDOG: Duration = Duration::from_secs(30);
const SCRIPT: &str = r#"
local add = cayw.call('mcp.call', {server='shared-source', tool='add', arguments={a=19, b=23}})
local pi = cayw.call('mcp.call', {server='cayw-pi-fixture', tool='pi_fixture.echo', arguments={text='workspace-isolation'}})
return {
  sum = add.value.sum,
  mcp_pid = add.value.server_pid,
  echo = pi.value.echo,
  package = pi.value.package,
  pi_call_index = pi.value.call_index,
}
"#;

fn command(home: &Path, workspace: &Path, cayw_home: &OsStr) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cayw"));
    command
        .env_clear()
        .env("HOME", home)
        .env("CAYW_HOME", cayw_home)
        .env("CAYW_MAIN_SESSION", home.join("main-session"))
        .current_dir(workspace)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

async fn run(mut command: Command) -> std::process::Output {
    tokio::time::timeout(WATCHDOG, command.output())
        .await
        .expect("CLI must finish within the watchdog")
        .expect("CLI should start")
}

async fn init(home: &Path, workspace: &Path, cayw_home: &Path) {
    let mut command = command(home, workspace, cayw_home.as_os_str());
    command.arg("init");
    let output = run(command).await;
    assert!(
        output.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

async fn read_ready(child: &mut Child) -> Value {
    let mut lines = BufReader::new(child.stdout.take().expect("serve stdout")).lines();
    let line = tokio::time::timeout(WATCHDOG, lines.next_line())
        .await
        .expect("serve must report readiness")
        .expect("serve stdout is readable")
        .expect("serve prints a ready line");
    serde_json::from_str(&line).unwrap()
}

fn workspace_key(workspace: &Path) -> String {
    let canonical = workspace.canonicalize().unwrap();
    format!("{:x}", Sha256::digest(canonical.as_os_str().as_bytes()))
}

fn write_private_json(path: &Path, value: &Value) {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    serde_json::to_writer_pretty(&mut file, value).unwrap();
    file.write_all(b"\n").unwrap();
}

async fn read_request(socket: &mut TcpStream) -> Value {
    let mut bytes = Vec::new();
    let header_end = loop {
        let mut buffer = [0; 4096];
        let count = socket.read(&mut buffer).await.unwrap();
        assert_ne!(count, 0);
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(index) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            break index + 4;
        }
        assert!(bytes.len() < 128 * 1024);
    };
    let headers = std::str::from_utf8(&bytes[..header_end]).unwrap();
    assert!(headers.starts_with("POST /v1/chat/completions HTTP/1.1\r\n"));
    let length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().unwrap())
        })
        .unwrap();
    while bytes.len() < header_end + length {
        let mut buffer = [0; 4096];
        let count = socket.read(&mut buffer).await.unwrap();
        assert_ne!(count, 0);
        bytes.extend_from_slice(&buffer[..count]);
    }
    serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap()
}

fn response(index: usize, continuation: bool) -> String {
    let chunk = |delta: Value, reason: Value| {
        json!({
            "id":format!("workspace-response-{index}"),
            "model":"workspace-fixture",
            "choices":[{"index":0,"delta":delta,"finish_reason":reason}]
        })
    };
    let mut values = if continuation {
        vec![chunk(
            json!({"role":"assistant","content":"workspace optional services completed"}),
            Value::Null,
        )]
    } else {
        vec![chunk(
            json!({
                "role":"assistant",
                "tool_calls":[{
                    "index":0,
                    "id":format!("workspace-call-{index}"),
                    "type":"function",
                    "function":{
                        "name":"execute_code",
                        "arguments":json!({
                            "action":"exec",
                            "script":SCRIPT,
                            "yield_time_ms":30_000,
                            "max_output_tokens":8192
                        }).to_string()
                    }
                }]
            }),
            Value::Null,
        )]
    };
    values.push(chunk(
        json!({}),
        json!(if continuation { "stop" } else { "tool_calls" }),
    ));
    values.push(json!({
        "choices":[],
        "usage":{"prompt_tokens":11,"completion_tokens":7,"total_tokens":18}
    }));
    values
        .into_iter()
        .map(|value| format!("data: {value}\r\n\r\n"))
        .collect::<String>()
        + "data: [DONE]\r\n\r\n"
}

async fn model_server() -> (String, tokio::task::JoinHandle<Vec<Value>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let mut requests = Vec::new();
        for index in 0..4 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_request(&mut socket).await;
            let continuation = request["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|message| message["role"] == "tool");
            let body = response(index, continuation);
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            socket.write_all(headers.as_bytes()).await.unwrap();
            socket.write_all(body.as_bytes()).await.unwrap();
            socket.shutdown().await.unwrap();
            requests.push(request);
        }
        requests
    });
    (base_url, task)
}

async fn workspace_call(client: &Client, workspace_id: &str, method: &str, params: Value) -> Value {
    client
        .request(
            "client.workspace.call",
            json!({
                "workspace_id":workspace_id,
                "rpc":{"method":method,"params":params}
            }),
        )
        .await
        .unwrap()
}

fn execution_value(request: &Value) -> Value {
    let result = request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|message| message["role"] == "tool")
        .expect("continuation request contains the execute_code result");
    let structured: Value = serde_json::from_str(
        result["content"][1]["text"]
            .as_str()
            .expect("OpenAI tool result carries structured content"),
    )
    .unwrap();
    structured["outputs"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|entry| {
            (entry["output"]["type"] == "json").then(|| entry["output"]["value"].clone())
        })
        .expect("execute_code returned a JSON value")
}

fn process_exists(pid: u32) -> bool {
    let pid = pid.to_string();
    std::process::Command::new("/bin/ps")
        .args(["-p", &pid, "-o", "pid="])
        .output()
        .is_ok_and(|output| {
            output.status.success()
                && String::from_utf8_lossy(&output.stdout)
                    .split_whitespace()
                    .any(|value| value == pid)
        })
}

async fn assert_process_stopped(pid: u32) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while process_exists(pid) && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        !process_exists(pid),
        "MCP fixture {pid} survived Client shutdown"
    );
}

#[tokio::test]
async fn one_client_keeps_mcp_and_pi_instances_isolated_per_workspace() {
    let cayw_home = tempfile::tempdir().unwrap();
    fs::set_permissions(cayw_home.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let home = tempfile::tempdir().unwrap();
    let first_workspace = tempfile::tempdir().unwrap();
    let second_workspace = tempfile::tempdir().unwrap();
    init(home.path(), first_workspace.path(), cayw_home.path()).await;
    init(home.path(), second_workspace.path(), cayw_home.path()).await;

    let (base_url, model_task) = model_server().await;
    let mcp_file = cayw_home.path().join("mcp.json");
    fs::write(
        &mcp_file,
        serde_json::to_vec(&json!({
            "mcpServers": {
                "shared-source": {
                    "command": env!("CARGO_BIN_EXE_cayw-runtime-mcp-fixture"),
                    "lifecycle": "eager"
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();
    fs::write(
        cayw_home.path().join("config.json"),
        serde_json::to_vec_pretty(&json!({
            "mcp_servers_file":mcp_file,
            "pi_compat":{"enabled":true,"fixture":true},
            "permissions":[
                {
                    "capability":"mcp.call",
                    "arguments":{"server":"shared-source","tool":"add","arguments":{"a":19,"b":23}}
                },
                {
                    "capability":"mcp.call",
                    "arguments":{"server":"cayw-pi-fixture","tool":"pi_fixture.echo","arguments":{"text":"workspace-isolation"}}
                }
            ]
        }))
        .unwrap(),
    )
    .unwrap();
    let model = json!({
        "selected":"fixture",
        "models":{
            "fixture":{
                "service":"openai",
                "api":"chat_completions",
                "id":"workspace-fixture",
                "base_url":base_url,
                "api_version":"v1",
                "max_output_tokens":256
            }
        }
    });
    // Per-workspace model selection is still the path the current Client reads;
    // the device-wide migration is tracked separately.
    for workspace in [&first_workspace, &second_workspace] {
        let state = cayw_home
            .path()
            .join("workspaces")
            .join(workspace_key(workspace.path()));
        write_private_json(&state.join("models.json"), &model);
    }

    let mut serve = command(
        home.path(),
        first_workspace.path(),
        cayw_home.path().as_os_str(),
    );
    serve.arg("serve");
    let mut server = serve.spawn().unwrap();
    let ready = read_ready(&mut server).await;
    let token = fs::read_to_string(ready["token_file"].as_str().unwrap()).unwrap();
    let client = Client::connect(
        ready["listening"].as_str().unwrap().parse().unwrap(),
        token.trim(),
    )
    .await
    .unwrap();
    let catalog = client
        .request("client.workspaces.list", json!({}))
        .await
        .unwrap();
    let second_path = second_workspace.path().canonicalize().unwrap();
    let second_id = catalog["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["path"].as_str() == second_path.to_str())
        .and_then(|entry| entry["id"].as_str())
        .unwrap()
        .to_owned();
    client
        .request("client.workspaces.open", json!({"id":second_id}))
        .await
        .unwrap();

    let first_session = client
        .request("session.create", json!({"label":"first workspace"}))
        .await
        .unwrap();
    let first = client
        .request(
            "session.prompt",
            json!({"session_id":first_session["id"],"text":"run workspace services"}),
        )
        .await
        .unwrap();
    assert_eq!(first["state"], "idle");

    let second_session = workspace_call(
        &client,
        &second_id,
        "session.create",
        json!({"label":"second workspace"}),
    )
    .await;
    let second = workspace_call(
        &client,
        &second_id,
        "session.prompt",
        json!({"session_id":second_session["id"],"text":"run workspace services"}),
    )
    .await;
    assert_eq!(second["state"], "idle");

    let requests = tokio::time::timeout(WATCHDOG, model_task)
        .await
        .expect("model fixture received both two-turn prompts")
        .unwrap();
    let values = requests
        .iter()
        .filter(|request| {
            request["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|message| message["role"] == "tool")
        })
        .map(execution_value)
        .collect::<Vec<_>>();
    assert_eq!(values.len(), 2);
    for value in &values {
        assert_eq!(value["sum"], 42);
        assert_eq!(value["echo"], "workspace-isolation");
        assert_eq!(value["package"], "cayw-pi-fixture");
        assert_eq!(value["pi_call_index"], 1);
    }
    let first_pid = values[0]["mcp_pid"].as_u64().unwrap() as u32;
    let second_pid = values[1]["mcp_pid"].as_u64().unwrap() as u32;
    assert_ne!(first_pid, second_pid);
    assert!(process_exists(first_pid));
    assert!(process_exists(second_pid));

    client.close().await;
    std::process::Command::new("/bin/kill")
        .arg("-INT")
        .arg(server.id().unwrap().to_string())
        .status()
        .unwrap();
    let status = tokio::time::timeout(WATCHDOG, server.wait())
        .await
        .expect("Client shutdown must finish")
        .unwrap();
    let mut stderr = String::new();
    server
        .stderr
        .take()
        .expect("serve stderr")
        .read_to_string(&mut stderr)
        .await
        .unwrap();
    assert!(
        status.success(),
        "Client shutdown failed with {status}: {stderr}"
    );
    assert_process_stopped(first_pid).await;
    assert_process_stopped(second_pid).await;
}
