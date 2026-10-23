#![cfg(target_os = "macos")]
use cayw_host::ProcessIdentity;
use cayw_ipc::Client;
use cayw_runtime::{HeadlessConfig, RuntimeConfig, ScopedPermission};
use serde_json::{Value, json};
use std::{path::Path, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
};

const TOKEN: &str = "headless-config-local-test-token";
const WATCHDOG: Duration = Duration::from_secs(20);

fn configuration(root: &Path, permissions: bool) -> Value {
    json!({
        "budgets": {"max_total_tokens": 2048,"max_turns": 8,
            "max_run_time_ms": 90000,"max_capability_calls": 8,
            "max_capability_bytes": 1048576,"host_wait_ms": 60000,
            "host_call_timeout_ms": 60000},
        "profiles": {"tree": {"kind":"test",
            "program":env!("CARGO_BIN_EXE_cayw-runtime-process-fixture"),
            "args":["tree",root],"env":{"PRIVATE_PROFILE_VALUE":"must-not-appear-in-startup"},
            "timeout_ms":60000,"max_output_bytes":4096,"allow_filter":false,
            "io":{"kind":"pipes"}}},
        "permissions": if permissions {json!([{"capability":"test.run","arguments":{"profile":"tree"}}])} else {json!([])}
    })
}

#[test]
fn public_configuration_maps_fixed_profiles_budgets_and_exact_permissions_without_execution() {
    let root = tempfile::tempdir().unwrap();
    let mut input = configuration(root.path(), true);
    input["budgets"]["host_wait_ms"] = json!(45000);
    input["budgets"]["host_call_timeout_ms"] = json!(50000);
    input["permissions"].as_array_mut().unwrap().push(json!({
        "capability":"fs.write", "arguments":{"path":"fixed.txt","content":"fixed content"}
    }));
    let path = root.path().join("config.json");
    std::fs::write(&path, serde_json::to_vec(&input).unwrap()).unwrap();
    let config = HeadlessConfig::load(path)
        .unwrap()
        .into_runtime_config()
        .unwrap();
    assert_eq!(config.max_total_tokens, 2048);
    assert_eq!(config.max_turns, 8);
    assert_eq!(config.max_run_time, Duration::from_millis(90000));
    assert_eq!(config.max_capability_calls, 8);
    assert_eq!(config.max_capability_bytes, 1048576);
    assert_eq!(config.codemode.host_wait, Duration::from_millis(45000));
    assert_eq!(config.host.call_timeout, Duration::from_millis(50000));
    let profile = &config.host.profiles["tree"];
    assert_eq!(profile.kind, cayw_host::ProfileKind::Test);
    assert_eq!(
        profile.program,
        Path::new(env!("CARGO_BIN_EXE_cayw-runtime-process-fixture"))
    );
    assert_eq!(profile.args, ["tree", root.path().to_str().unwrap()]);
    assert_eq!(
        profile.env["PRIVATE_PROFILE_VALUE"],
        "must-not-appear-in-startup"
    );
    assert_eq!(profile.timeout, Duration::from_millis(60000));
    assert_eq!(profile.max_output_bytes, 4096);
    assert!(!profile.allow_filter);
    assert!(matches!(profile.io, cayw_host::ProcessIo::Pipes));
    assert!(config.host.terminal_handoffs.is_none());
    for (actual, expected) in config.permissions.iter().zip([
        ScopedPermission::exact("test.run", json!({"profile":"tree"})),
        ScopedPermission::exact(
            "fs.write",
            json!({"content":"fixed content","path":"fixed.txt"}),
        ),
    ]) {
        assert_eq!(actual.capability, expected.capability);
        assert_eq!(actual.scope, expected.scope);
    }
    assert_eq!(config.permissions.len(), 2);
    assert!(!root.path().join("leader.pid").exists());
    assert!(!root.path().join("leaf.pid").exists());
    assert!(!root.path().join("session.db").exists());
}

#[test]
fn omitted_settings_keep_runtime_defaults_and_grant_no_permissions() {
    let defaults = RuntimeConfig::default();
    for (input, tokens) in [
        (json!({}), defaults.max_total_tokens),
        (json!({"budgets":{"max_total_tokens":73}}), 73),
    ] {
        let config = serde_json::from_value::<HeadlessConfig>(input)
            .unwrap()
            .into_runtime_config()
            .unwrap();
        assert_eq!(config.max_total_tokens, tokens);
        assert_eq!(config.max_turns, defaults.max_turns);
        assert_eq!(config.max_run_time, defaults.max_run_time);
        assert_eq!(config.max_capability_calls, defaults.max_capability_calls);
        assert_eq!(config.max_capability_bytes, defaults.max_capability_bytes);
        assert_eq!(config.codemode.host_wait, defaults.codemode.host_wait);
        assert_eq!(config.host.call_timeout, defaults.host.call_timeout);
        assert!(config.host.profiles.is_empty());
        assert!(config.permissions.is_empty());
    }
}

struct Server {
    child: tokio::process::Child,
    client: Client,
    ready: Value,
}
impl Server {
    async fn start(root: &Path, config: Value, base: &str) -> Self {
        Self::start_with_options(root, config, base, &[]).await
    }
    async fn start_with_options(
        root: &Path,
        mut config: Value,
        base: &str,
        options: &[&str],
    ) -> Self {
        if !options.contains(&"--offline") {
            config["model"] = json!({
                "service":"openai", "api":"chat_completions", "id":"fixture-config-model",
                "base_url":base, "api_version":"v1", "max_output_tokens":1024,
                "api_key_env":"CAYW_CONFIG_TEST_PROVIDER_KEY"
            });
        }
        std::fs::create_dir(root.join("workspace")).unwrap();
        std::fs::write(
            root.join("config.json"),
            serde_json::to_vec(&config).unwrap(),
        )
        .unwrap();
        let mut child = command(root, base).args(options).spawn().unwrap();
        let ready = tokio::time::timeout(
            WATCHDOG,
            BufReader::new(child.stdout.take().unwrap())
                .lines()
                .next_line(),
        )
        .await
        .expect("server readiness deadline")
        .unwrap()
        .expect("server readiness line");
        let ready: Value = serde_json::from_str(&ready).unwrap();
        let client = Client::connect(ready["listening"].as_str().unwrap().parse().unwrap(), TOKEN)
            .await
            .unwrap();
        Self {
            child,
            client,
            ready,
        }
    }
    async fn session(&self) -> String {
        self.client
            .request("session.create", json!({}))
            .await
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned()
    }
    async fn stop(mut self) {
        self.client.close().await;
        self.child.kill().await.unwrap();
        self.child.wait().await.unwrap();
    }
}
fn command(root: &Path, _base: &str) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_cayw"));
    command
        .args(["serve", "--db"])
        .arg(root.join("session.db"))
        .arg("--workspace")
        .arg(root.join("workspace"))
        .args([
            "--listen",
            "127.0.0.1:0",
            "--token-env",
            "CAYW_CONFIG_TEST_TOKEN",
            "--config",
        ])
        .arg(root.join("config.json"))
        .env_clear()
        .env("CAYW_CONFIG_TEST_TOKEN", TOKEN)
        .env(
            "CAYW_CONFIG_TEST_PROVIDER_KEY",
            "synthetic-provider-key-never-print",
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}
async fn read_request(socket: &mut TcpStream) -> Value {
    let mut bytes = Vec::new();
    let end = loop {
        let mut buffer = [0; 4096];
        let read = socket.read(&mut buffer).await.unwrap();
        assert_ne!(read, 0);
        bytes.extend_from_slice(&buffer[..read]);
        if let Some(index) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let headers = std::str::from_utf8(&bytes[..end]).unwrap();
    assert!(headers.starts_with("POST /v1/chat/completions HTTP/1.1"));
    let length: usize = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().unwrap())
        })
        .unwrap();
    while bytes.len() < end + length {
        let mut buffer = [0; 4096];
        let read = socket.read(&mut buffer).await.unwrap();
        assert_ne!(read, 0);
        bytes.extend_from_slice(&buffer[..read]);
    }
    serde_json::from_slice(&bytes[end..end + length]).unwrap()
}
async fn response(socket: &mut TcpStream, delta: Value, reason: &str, reported: bool) {
    let chunk = |delta, reason| {
        json!({"id":"config-response","model":"served-config-model",
        "choices":[{"index":0,"delta":delta,"finish_reason":reason}]})
    };
    let mut body = format!(
        "data: {}\n\ndata: {}\n\n",
        chunk(delta, Value::Null),
        chunk(json!({}), json!(reason)),
    );
    // Successful/denied profile-result consumption uses known synthetic usage.
    // The cancellation case retains its original missing usage and reservation.
    if reported {
        body.push_str(&format!(
            "data: {}\n\n",
            json!({"choices":[],"usage":{"prompt_tokens":12,"completion_tokens":7,"total_tokens":19}})
        ));
    }
    body.push_str("data: [DONE]\n\n");
    socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
}
async fn provider(reported: bool) -> (String, tokio::task::JoinHandle<Vec<Value>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let mut requests = vec![];
        let (mut socket, _) = listener.accept().await.unwrap();
        requests.push(read_request(&mut socket).await);
        response(&mut socket, json!({"tool_calls":[{"index":0,"id":"config-call","type":"function","function":{"name":"execute_code","arguments":json!({"action":"exec","script":"return cayw.call('test.run',{profile='tree'})","yield_time_ms":30000,"max_output_tokens":32768}).to_string()}}]}), "tool_calls", reported).await;
        let (mut socket, _) = listener.accept().await.unwrap();
        requests.push(read_request(&mut socket).await);
        response(
            &mut socket,
            json!({"content":"The profile result was consumed."}),
            "stop",
            reported,
        )
        .await;
        requests
    });
    (base, task)
}
async fn prompt(
    server: &Server,
    id: &str,
) -> tokio::task::JoinHandle<Result<Value, cayw_ipc::ClientError>> {
    let client = Client::connect(
        server.ready["listening"].as_str().unwrap().parse().unwrap(),
        TOKEN,
    )
    .await
    .unwrap();
    let id = id.to_owned();
    tokio::spawn(async move {
        client
            .request(
                "session.prompt",
                json!({"session_id":id,"text":"run the configured profile"}),
            )
            .await
    })
}
async fn pending(server: &Server, id: &str) -> Value {
    tokio::time::timeout(WATCHDOG, async {
        loop {
            let view = server
                .client
                .request("session.get", json!({"session_id":id}))
                .await
                .unwrap();
            if let Some(approval) = view["pending_approvals"].as_array().unwrap().first() {
                return approval.clone();
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("approval deadline")
}
async fn pid(root: &Path, name: &str) -> u32 {
    tokio::time::timeout(WATCHDOG, async {
        loop {
            if let Ok(text) = std::fs::read_to_string(root.join(name))
                && let Ok(pid) = text.parse()
            {
                return pid;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("process marker deadline")
}
fn same_process_running(original: &ProcessIdentity) -> bool {
    match ProcessIdentity::observe(original.pid) {
        Err(error) if error.raw_os_error() == Some(3) => false,
        Err(error) => panic!("cannot verify original process identity: {error}"),
        Ok(current) => {
            if current.started_seconds != original.started_seconds
                || current.started_microseconds != original.started_microseconds
            {
                return false;
            }
            let state = std::process::Command::new("/bin/ps")
                .args(["-o", "stat=", "-p", &original.pid.to_string()])
                .output()
                .unwrap();
            state.status.success()
                && !String::from_utf8_lossy(&state.stdout)
                    .trim()
                    .starts_with('Z')
        }
    }
}

#[tokio::test]
async fn configured_profile_waits_for_exact_approval_and_runs_through_luau() {
    let root = tempfile::tempdir().unwrap();
    let (base, provider) = provider(true).await;
    let server = Server::start(root.path(), configuration(root.path(), false), &base).await;
    let id = server.session().await;
    let task = prompt(&server, &id).await;
    let approval = pending(&server, &id).await;
    assert_eq!(approval["input"]["capability"], "test.run");
    assert!(!root.path().join("leader.pid").exists());
    assert!(!root.path().join("leaf.pid").exists());
    server
        .client
        .request(
            "approval.decide",
            json!({"session_id":id,"approval_id":approval["id"],"decision":"allow_once"}),
        )
        .await
        .unwrap();
    pid(root.path(), "leader.pid").await;
    pid(root.path(), "leaf.pid").await;
    std::fs::write(root.path().join("release"), "release").unwrap();
    let final_view = tokio::time::timeout(WATCHDOG, task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(final_view["state"], "idle");
    assert!(
        final_view["unresolved_capabilities"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let tasks = server
        .client
        .request("session.tasks", json!({"session_id":id}))
        .await
        .unwrap();
    assert_eq!(
        tasks[0]["state"], "completed",
        "task={tasks}; session={final_view}"
    );
    let requests = tokio::time::timeout(WATCHDOG, provider)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[1]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| message["role"] == "tool")
    );
    let ready = server.ready.to_string();
    for secret in [
        TOKEN,
        "synthetic-provider-key-never-print",
        "must-not-appear-in-startup",
    ] {
        assert!(!ready.contains(secret));
    }
    assert_eq!(server.ready["provider"], "openai");
    assert_eq!(server.ready["model"], "fixture-config-model");
    assert_eq!(server.ready["model_configuration"]["base_url"], base);
    assert_eq!(server.ready["configuration"]["max_total_tokens"], 2048);
    assert_eq!(server.ready["configuration"]["exact_permission_count"], 0);
    server.stop().await;
}

#[tokio::test]
async fn offline_startup_counts_configured_and_demo_permissions_without_connecting() {
    let root = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = Server::start_with_options(
        root.path(),
        configuration(root.path(), true),
        &base,
        &["--offline", "--demo-authorized"],
    )
    .await;
    assert_eq!(server.ready["configuration"]["exact_permission_count"], 3);
    assert!(server.ready["model_configuration"].is_null());
    assert_eq!(server.ready["provider"], "fixture");
    assert!(!root.path().join("leader.pid").exists());
    assert!(!root.path().join("leaf.pid").exists());
    assert!(
        tokio::time::timeout(Duration::from_millis(50), listener.accept())
            .await
            .is_err()
    );
    server.stop().await;
}

#[tokio::test]
async fn installed_runtime_loads_request_policy_without_grant_or_model_dispatch() {
    let root = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let mut input = configuration(root.path(), false);
    input["requests"] = json!({
        "permission_policy_id":"configured-project-test",
        "operations":[{"id":"project-test","capability":"test.run",
            "arguments":{"profile":"tree"},"permission_binding":"configured-tree"}],
        "limits":{"repeated_valid_threshold":3}
    });
    let server = Server::start_with_options(root.path(), input, &base, &["--offline"]).await;
    let operations = server
        .client
        .request("request.operations", json!({}))
        .await
        .unwrap();
    assert_eq!(operations[0]["arguments"], json!({"profile":"tree"}));
    let definitions = server
        .client
        .request("capabilities.list", json!({}))
        .await
        .unwrap();
    let definitions = definitions.as_array().unwrap();
    assert!(
        definitions
            .iter()
            .any(|item| item["name"] == "capability.request")
    );
    assert!(
        definitions
            .iter()
            .all(|item| !item["name"].as_str().unwrap().starts_with("request."))
    );
    assert_eq!(server.ready["configuration"]["exact_permission_count"], 0);
    let session = server.session().await;
    let tasks = server
        .client
        .request("session.tasks", json!({"session_id":session}))
        .await
        .unwrap();
    assert!(tasks.as_array().unwrap().is_empty());
    assert!(!root.path().join("leader.pid").exists());
    assert!(!root.path().join("leaf.pid").exists());
    assert!(
        tokio::time::timeout(Duration::from_millis(50), listener.accept())
            .await
            .is_err()
    );
    server.stop().await;
}

#[tokio::test]
async fn denying_a_configured_profile_never_starts_a_process() {
    let root = tempfile::tempdir().unwrap();
    let (base, provider) = provider(true).await;
    let server = Server::start(root.path(), configuration(root.path(), false), &base).await;
    let id = server.session().await;
    let task = prompt(&server, &id).await;
    let approval = pending(&server, &id).await;
    server
        .client
        .request(
            "approval.decide",
            json!({"session_id":id,"approval_id":approval["id"],"decision":"deny"}),
        )
        .await
        .unwrap();
    let view = tokio::time::timeout(WATCHDOG, task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(view["state"], "idle", "session={view}");
    let tasks = server
        .client
        .request("session.tasks", json!({"session_id":id}))
        .await
        .unwrap();
    assert_eq!(
        tasks[0]["state"], "completed",
        "task={tasks}; session={view}"
    );
    assert!(!root.path().join("leader.pid").exists());
    assert!(!root.path().join("leaf.pid").exists());
    let requests = tokio::time::timeout(WATCHDOG, provider)
        .await
        .unwrap()
        .unwrap();
    assert!(
        requests[1]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| message["role"] == "tool" && message.to_string().contains("permission"))
    );
    server.stop().await;
}

#[tokio::test]
async fn cancellation_drains_the_configured_original_processes_before_task_terminal() {
    let root = tempfile::tempdir().unwrap();
    let (base, provider) = provider(false).await;
    let server = Server::start(root.path(), configuration(root.path(), true), &base).await;
    let id = server.session().await;
    let task = prompt(&server, &id).await;
    let leader = ProcessIdentity::observe(pid(root.path(), "leader.pid").await).unwrap();
    let leaf = ProcessIdentity::observe(pid(root.path(), "leaf.pid").await).unwrap();
    let original_tasks = server
        .client
        .request("session.tasks", json!({"session_id":id}))
        .await
        .unwrap();
    let original_task: cayw_runtime::TaskRecord =
        serde_json::from_value(original_tasks[0].clone()).unwrap();
    let cancelled = server
        .client
        .request(
            "session.cancel",
            serde_json::to_value(original_task.cancel_target()).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(cancelled["cancelled"], true);
    let _ = tokio::time::timeout(WATCHDOG, task).await.unwrap().unwrap();
    let tasks = server
        .client
        .request("session.tasks", json!({"session_id":id}))
        .await
        .unwrap();
    assert_eq!(tasks[0]["state"], "outcome_unknown");
    assert!(!same_process_running(&leader));
    assert!(!same_process_running(&leaf));
    let audit = server
        .client
        .request(
            "session.audit",
            json!({"session_id":id,"after":0,"limit":1000}),
        )
        .await
        .unwrap();
    assert!(audit.as_array().unwrap().iter().any(
        |record| record["kind"] == "processes_stopped" && record["data"]["state"] == "drained"
    ));
    provider.abort();
    server.stop().await;
}

#[tokio::test]
async fn bad_or_oversized_configuration_is_rejected_before_database_or_network() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let cases = [
        json!({"unknown":"private-unknown-value"}).to_string(),
        json!({"profiles":{"bad":{"kind":"private-unknown-value","program":"/usr/bin/true"}}}).to_string(),
        json!({"profiles":{"bad":{"kind":"test","program":"relative"}}}).to_string(),
        json!({"profiles":{"bad":{"kind":"test","program":"/usr/bin/true","env":{"SENSITIVE":"private-unknown-value\0"}}}}).to_string(),
        json!({"budgets":{"max_total_tokens":0}}).to_string(),
        json!({"budgets":{"host_wait_ms":0}}).to_string(),
        json!({"budgets":{"host_wait_ms":3600001}}).to_string(),
        json!({"budgets":{"host_call_timeout_ms":0}}).to_string(),
        json!({"profiles":{"bad":{"kind":"test","program":"/private/missing-config-profile"}}}).to_string(),
        json!({"profiles":{"bad":{"kind":"test","program":"/usr/bin/true","timeout_ms":0}}}).to_string(),
        json!({"profiles":{"bad":{"kind":"test","program":"/usr/bin/true","max_output_bytes":0}}}).to_string(),
        json!({"profiles":{"bad":{"kind":"build","program":"/usr/bin/true","allow_filter":true}}}).to_string(),
        json!({"profiles":{"bad":{"kind":"test","program":"/usr/bin/true","io":{"kind":"pty"}}}}).to_string(),
        json!({"permissions":[{"capability":"test.run","arguments":{"profile":"missing"}}]}).to_string(),
        json!({"permissions":[{"capability":"fs.read","arguments":{"path":"ok","program":"private-unknown-value"}}]}).to_string(),
        json!({"permissions":[{"capability":"test.run","arguments":"private-unknown-value"}]}).to_string(),
        " ".repeat(1024 * 1024 + 1),
    ];
    for config in cases {
        // Reach the invalid Host field instead of failing early on the new
        // mandatory model selection. Preserve the oversized non-JSON case.
        let config = match serde_json::from_str::<Value>(&config) {
            Ok(mut value) => {
                value["model"] = json!({
                    "service":"openai", "api":"chat_completions", "id":"fixture-config-model",
                    "base_url":base, "api_version":"v1", "max_output_tokens":1024,
                    "api_key_env":"CAYW_CONFIG_TEST_PROVIDER_KEY"
                });
                serde_json::to_string(&value).unwrap()
            }
            Err(_) => config,
        };
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("workspace")).unwrap();
        std::fs::write(root.path().join("config.json"), config).unwrap();
        let output = tokio::time::timeout(WATCHDOG, command(root.path(), &base).output())
            .await
            .unwrap()
            .unwrap();
        assert!(!output.status.success());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("configuration"), "{error}");
        assert!(
            !error.contains("private-unknown-value"),
            "configuration leaked a value: {error}"
        );
        assert!(!root.path().join("session.db").exists());
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(50), listener.accept())
            .await
            .is_err()
    );
}
