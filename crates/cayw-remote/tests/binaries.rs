//! Real process smoke test. Every listener uses an ephemeral loopback address.
use cayw_remote::{OperatorClient, Secret, ServerUrl};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn command(binary: &Path, token: &Secret, url: &ServerUrl) -> Command {
    let mut cmd = Command::new(binary);
    cmd.env("CAYW_REMOTE_OPERATOR_TOKEN", token.expose())
        .env("CAYW_REMOTE_SERVER_URL", url.as_str());
    cmd
}
async fn output(mut cmd: Command) -> Value {
    let result = tokio::task::spawn_blocking(move || cmd.output().unwrap())
        .await
        .unwrap();
    assert!(
        result.status.success(),
        "process failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    serde_json::from_slice(&result.stdout).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "build the device example, then run --test binaries -- --ignored"]
async fn server_binary_device_example_and_operator_commands_work_on_loopback() {
    // This integration target is run after cargo build --example device.
    let server_binary = PathBuf::from(env!("CARGO_BIN_EXE_cayw-remote-server"));
    let device_binary = server_binary.parent().unwrap().join("examples/device");
    assert!(
        device_binary.is_file(),
        "build the device example before this process test"
    );
    let dir = tempfile::tempdir().unwrap();
    let reserved = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = reserved.local_addr().unwrap();
    drop(reserved);
    let url = ServerUrl::new(&format!("ws://{address}"), false).unwrap();
    let token = Secret::generate().unwrap();
    let mut server_cmd = command(&server_binary, &token, &url);
    server_cmd
        .arg("serve")
        .env("CAYW_REMOTE_LISTEN", address.to_string())
        .env("CAYW_REMOTE_PUBLIC_URL", url.as_str())
        .env("CAYW_REMOTE_DATABASE", dir.path().join("server.sqlite"))
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let _server = Process(server_cmd.spawn().unwrap());
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if tokio::net::TcpStream::connect(address).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let pair_file = dir.path().join("pair.secret");
    let mut pair = command(&server_binary, &token, &url);
    pair.arg("pair").env("CAYW_REMOTE_PAIR_FILE", &pair_file);
    assert_eq!(output(pair).await["pairing_code_saved"], true);
    let pending_path = dir.path().join("device-pending.json");
    let start_device = || {
        let mut cmd = Command::new(&device_binary);
        cmd.env_remove("CAYW_REMOTE_OPERATOR_TOKEN")
            .env("CAYW_REMOTE_SERVER_URL", url.as_str())
            .env(
                "CAYW_REMOTE_PAIR_CODE",
                std::fs::read_to_string(&pair_file).unwrap(),
            )
            .env("CAYW_REMOTE_PENDING_FILE", &pending_path)
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        Process(cmd.spawn().unwrap())
    };
    let mut device = start_device();
    let id_path = pending_path.with_extension("id");
    let operator = OperatorClient::new(url.clone(), token.clone());
    let id = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(id) = std::fs::read_to_string(&id_path)
                && operator.device_status(&id).await.is_ok_and(|s| s.connected)
            {
                break id;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let payload = dir.path().join("payload.json");
    std::fs::write(&payload, b"{\"n\":9}").unwrap();
    let mut invoke = command(&server_binary, &token, &url);
    invoke.args(["invoke", &id, "binary-echo"]).arg(&payload);
    assert_eq!(output(invoke).await["state"], "running");
    let result = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let mut status = command(&server_binary, &token, &url);
            status.args(["status", &id, "binary-echo"]);
            let result = output(status).await;
            if result["state"] == "completed" {
                break result;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(result["value"], json!({"device_reply":{"n":9}}));
    let old_generation = operator.device_status(&id).await.unwrap().generation;
    device.0.kill().unwrap();
    device.0.wait().unwrap();
    drop(device);
    let _restarted = start_device();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !operator
            .device_status(&id)
            .await
            .is_ok_and(|s| s.connected && s.generation > old_generation)
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        operator.status(&id, "binary-echo").await.unwrap().value,
        Some(json!({"device_reply":{"n":9}}))
    );
    std::fs::write(&payload, b"{\"wait_for_cancel\":true}").unwrap();
    let mut invoke = command(&server_binary, &token, &url);
    invoke.args(["invoke", &id, "binary-cancel"]).arg(&payload);
    output(invoke).await;
    let mut cancel = command(&server_binary, &token, &url);
    cancel.args(["cancel", &id, "binary-cancel"]);
    output(cancel).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while operator.status(&id, "binary-cancel").await.unwrap().state
            != cayw_remote::RequestState::Cancelled
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let mut revoke = command(&server_binary, &token, &url);
    revoke.args(["revoke", &id]);
    assert_eq!(output(revoke).await["revoked"], true);
    assert!(operator.device_status(&id).await.unwrap().revoked);
}

#[tokio::test]
async fn server_cli_invoke_rejects_oversized_payload_file_before_invoking() {
    let server_binary = PathBuf::from(env!("CARGO_BIN_EXE_cayw-remote-server"));
    let dir = tempfile::tempdir().unwrap();
    let oversized = dir.path().join("oversized.json");
    std::fs::write(&oversized, vec![b'x'; 65 * 1024]).unwrap();

    let token = Secret::generate().unwrap();
    let url = ServerUrl::new("ws://127.0.0.1:8787", false).unwrap();
    let mut invoke = command(&server_binary, &token, &url);
    invoke.args(["invoke", "dev-1", "req-1"]).arg(&oversized);

    let output = invoke.output().unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("message exceeds configured bound"),
        "stderr: {stderr}"
    );
}

#[tokio::test]
async fn server_cli_invoke_rejects_non_regular_payload_file_before_invoking() {
    let server_binary = PathBuf::from(env!("CARGO_BIN_EXE_cayw-remote-server"));
    let dir = tempfile::tempdir().unwrap();

    let token = Secret::generate().unwrap();
    let url = ServerUrl::new("ws://127.0.0.1:8787", false).unwrap();
    let mut invoke = command(&server_binary, &token, &url);
    invoke.args(["invoke", "dev-1", "req-1"]).arg(dir.path());

    let output = invoke.output().unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("configuration invalid"), "stderr: {stderr}");
}
