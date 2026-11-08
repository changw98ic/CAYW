//! First-run model setup for the local CLI.
//!
//! Every test spawns the real `cayw` binary with a cleared environment and a
//! temporary HOME/CAYW_HOME. Only the public CLI, files on disk and the public
//! IPC client are used. No test reads a real credential or calls a model.
#![cfg(target_os = "macos")]

use cayw_ipc::Client;
use serde_json::{Value, json};
use std::{
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
};

/// Generous guard for a local process. It is not a timing assertion: a healthy
/// run finishes long before it, and a process that blocks on stdin fails.
const WATCHDOG: Duration = Duration::from_secs(30);
const SECRET: &str = "sk-sentinel-credential-must-not-be-saved";

fn cli(home: &Path, workspace: &Path, cayw_home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cayw"));
    command
        .env_clear()
        .env("HOME", home)
        .env("CAYW_HOME", cayw_home)
        .current_dir(workspace)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

fn init_command(home: &Path, workspace: &Path, cayw_home: &Path) -> Command {
    let mut command = cli(home, workspace, cayw_home);
    command.args(["init", "--workspace"]).arg(workspace);
    command
}

fn conf_command(home: &Path, workspace: &Path, cayw_home: &Path) -> Command {
    let mut command = cli(home, workspace, cayw_home);
    command.args(["conf", "--workspace"]).arg(workspace);
    command
}

fn serve_command(home: &Path, workspace: &Path, cayw_home: &Path) -> Command {
    let mut command = cli(home, workspace, cayw_home);
    command.args(["serve", "--workspace"]).arg(workspace);
    command
}

async fn run_with_input(mut command: Command, input: &[u8]) -> std::process::Output {
    let mut child = command.spawn().expect("CLI should start");
    {
        let mut stdin = child.stdin.take().expect("CLI stdin is piped");
        stdin.write_all(input).await.expect("write wizard input");
    }
    tokio::time::timeout(WATCHDOG, child.wait_with_output())
        .await
        .expect("CLI must finish within the watchdog")
        .expect("CLI should wait")
}

async fn run_holding_stdin(mut command: Command) -> std::process::Output {
    let mut child = command.spawn().expect("CLI should start");
    let _stdin = child.stdin.take().expect("CLI stdin is piped");
    tokio::time::timeout(WATCHDOG, child.wait_with_output())
        .await
        .expect("CLI must not wait for input on an ordinary pipe")
        .expect("CLI should wait")
}

fn first_json(output: &std::process::Output) -> Option<Value> {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find(|line| line.trim_start().starts_with('{'))
        .and_then(|line| serde_json::from_str(line).ok())
}

fn private_dir(path: &Path) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

fn write_private(path: &Path, bytes: &[u8]) {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    file.write_all(bytes).unwrap();
    file.sync_all().unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

fn only_state_dir(cayw_home: &Path) -> PathBuf {
    let mut entries = std::fs::read_dir(cayw_home.join("workspaces"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    assert_eq!(entries.len(), 1, "one workspace state directory expected");
    entries.pop().unwrap()
}

fn model_config(id: &str, max_output_tokens: u64) -> Value {
    json!({
        "service": "openai",
        "api": "chat_completions",
        "id": id,
        "base_url": "http://127.0.0.1:9/v1",
        "api_version": "v1",
        "max_output_tokens": max_output_tokens
    })
}

async fn read_ready(child: &mut Child) -> Value {
    let mut lines = BufReader::new(child.stdout.take().expect("serve stdout")).lines();
    let line = tokio::time::timeout(WATCHDOG, lines.next_line())
        .await
        .expect("serve must report readiness within the watchdog")
        .expect("serve stdout is readable")
        .expect("serve prints its ready line before accepting clients");
    serde_json::from_str(&line).expect("the first serve line is JSON")
}

async fn connect(ready: &Value) -> Client {
    let address = ready["listening"]
        .as_str()
        .expect("ready line has listening")
        .parse()
        .expect("listening is a socket address");
    let token_file = ready["token_file"]
        .as_str()
        .expect("a bare serve advertises the token file");
    let token = std::fs::read_to_string(token_file).expect("read the private IPC token");
    Client::connect(address, token.trim())
        .await
        .expect("authenticated IPC connection")
}

#[tokio::test]
async fn pipe_conf_saves_workspace_profile_and_preserves_host_settings() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    private_dir(root.path());
    let config_file = root.path().join("config.json");
    let existing = json!({
        "budgets": {"max_total_tokens": 123456, "max_turns": 7},
        "scheduler": {"max_concurrent_runs": 2},
        "permissions": [{"capability": "fs.read", "arguments": {"path": "notes.txt"}}]
    });
    write_private(&config_file, &serde_json::to_vec(&existing).unwrap());

    let mut command = conf_command(home.path(), workspace.path(), root.path());
    command.env("OPENAI_API_KEY", SECRET);
    let output = run_with_input(
        command,
        b"openai\n\nhttp://127.0.0.1:9/v1\nfixture-model\n\n\n\n",
    )
    .await;
    assert!(
        output.status.success(),
        "conf failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let report = first_json(&output).expect("conf prints one JSON line");
    assert_eq!(report["configured"], true);
    assert_eq!(report["model_configured"], true);
    let models_file = PathBuf::from(report["models_file"].as_str().unwrap());
    assert_eq!(models_file, only_state_dir(root.path()).join("models.json"));
    let saved: Value = serde_json::from_slice(&std::fs::read(&models_file).unwrap()).unwrap();
    assert_eq!(saved["selected"], "configured");
    assert_eq!(
        saved["models"]["configured"],
        model_config("fixture-model", 4096)
    );
    assert!(saved["models"]["configured"].get("api_key").is_none());
    assert_eq!(
        serde_json::from_slice::<Value>(&std::fs::read(&config_file).unwrap()).unwrap(),
        existing
    );

    let stored = String::from_utf8(std::fs::read(&models_file).unwrap()).unwrap();
    assert!(!stored.contains(SECRET));
    assert!(!String::from_utf8_lossy(&output.stdout).contains(SECRET));
    assert!(!String::from_utf8_lossy(&output.stderr).contains(SECRET));
}

#[tokio::test]
async fn cancel_or_eof_does_not_create_or_replace_a_workspace_profile() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    private_dir(root.path());
    let config_file = root.path().join("config.json");
    let existing = json!({
        "budgets": {"max_turns": 5},
        "permissions": [{"capability": "fs.read", "arguments": {"path": "notes.txt"}}]
    });
    write_private(&config_file, &serde_json::to_vec(&existing).unwrap());

    let cancel = run_with_input(
        conf_command(home.path(), workspace.path(), root.path()),
        b"openai\n\nhttp://127.0.0.1:9/v1\ncancelled-model\n\n\nn\n",
    )
    .await;
    assert!(cancel.status.success());
    let cancel_report = first_json(&cancel).unwrap();
    assert_eq!(cancel_report["configured"], false);
    assert_eq!(cancel_report["model_configured"], false);
    let models_file = PathBuf::from(cancel_report["models_file"].as_str().unwrap());
    assert!(!models_file.exists());
    assert_eq!(
        serde_json::from_slice::<Value>(&std::fs::read(&config_file).unwrap()).unwrap(),
        existing
    );

    let eof = run_with_input(
        conf_command(home.path(), workspace.path(), root.path()),
        b"openai\n\nhttp://127.0.0.1:9/v1\n",
    )
    .await;
    assert!(eof.status.success());
    let eof_report = first_json(&eof).unwrap();
    assert_eq!(eof_report["configured"], false);
    assert_eq!(eof_report["model_configured"], false);
    assert!(!models_file.exists());
    assert_eq!(
        serde_json::from_slice::<Value>(&std::fs::read(&config_file).unwrap()).unwrap(),
        existing
    );
}

#[tokio::test]
async fn init_after_conf_does_not_require_a_model_key() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    private_dir(root.path());

    let configured = run_with_input(
        conf_command(home.path(), workspace.path(), root.path()),
        b"openai\n\nhttp://127.0.0.1:9/v1\nfixture-model\n\n\n\n",
    )
    .await;
    assert!(
        configured.status.success(),
        "conf failed: {}",
        String::from_utf8_lossy(&configured.stderr)
    );

    let output = run_with_input(
        init_command(home.path(), workspace.path(), root.path()),
        b"",
    )
    .await;
    assert!(
        output.status.success(),
        "init failed without a key: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = first_json(&output).expect("init prints one JSON line");
    assert_eq!(report["model_configured"], true);
    assert!(!String::from_utf8_lossy(&output.stderr).contains("未配置模型"));
}

#[tokio::test]
async fn missing_model_reports_conf_and_workspace_profile_path_without_blocking() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    private_dir(root.path());

    let output = run_holding_stdin(init_command(home.path(), workspace.path(), root.path())).await;
    assert!(
        output.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let models_file = only_state_dir(root.path()).join("models.json");
    let models_text = models_file.to_str().unwrap();
    let report = first_json(&output).expect("init prints one JSON line");
    assert_eq!(report["model_configured"], false);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("未配置模型"), "stderr: {stderr}");
    assert!(stderr.contains("cayw conf"), "stderr: {stderr}");
    assert!(stderr.contains(models_text), "stderr: {stderr}");

    let mut child = serve_command(home.path(), workspace.path(), root.path())
        .spawn()
        .unwrap();
    let _stdin = child.stdin.take().expect("serve stdin is piped");
    let mut serve_stderr = child.stderr.take().expect("serve stderr is piped");
    let ready = read_ready(&mut child).await;
    assert_eq!(ready["model_configured"], false);

    let client = connect(&ready).await;
    let session = client.request("session.create", json!({})).await.unwrap();
    let error = client
        .request(
            "task.enqueue",
            json!({"session_id": session["id"], "text": "must not run"}),
        )
        .await
        .expect_err("an unconfigured model must refuse execution");
    assert_eq!(error.code, "model_not_configured");
    assert!(error.message.contains(models_text), "{}", error.message);
    assert!(error.message.contains("cayw conf"), "{}", error.message);
    client.close().await;

    child.start_kill().unwrap();
    child.wait().await.unwrap();
    let mut serve_text = String::new();
    serve_stderr.read_to_string(&mut serve_text).await.unwrap();
    assert!(serve_text.contains("未配置模型"), "stderr: {serve_text}");
    assert!(serve_text.contains("cayw conf"), "stderr: {serve_text}");
    assert!(serve_text.contains(models_text), "stderr: {serve_text}");
    assert!(
        !serve_text.contains("现在配置模型吗"),
        "ordinary startup must never launch the model wizard: {serve_text}"
    );
}

#[tokio::test]
async fn removed_configure_command_is_rejected_without_creating_state() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    private_dir(root.path());

    let mut command = cli(home.path(), workspace.path(), root.path());
    command.arg("configure");
    let output = run_holding_stdin(command).await;
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("expected `start`"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!root.path().join("config.json").exists());
    assert!(!root.path().join("workspaces").exists());
}

#[tokio::test]
async fn selected_profile_wins_and_malformed_legacy_model_is_removed() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    private_dir(root.path());

    let initialized = run_with_input(
        init_command(home.path(), workspace.path(), root.path()),
        b"",
    )
    .await;
    assert!(initialized.status.success());
    let state_dir = only_state_dir(root.path());
    let models_file = state_dir.join("models.json");
    let profiles = json!({
        "selected": "keep",
        "models": {"keep": model_config("keep-model", 64)}
    });
    let profile_bytes = serde_json::to_vec_pretty(&profiles).unwrap();
    write_private(&models_file, &profile_bytes);

    let config_file = root.path().join("config.json");
    let remaining = json!({
        "budgets": {"max_turns": 3},
        "permissions": [{"capability": "fs.read", "arguments": {"path": "notes.txt"}}]
    });
    let mut legacy = remaining.clone();
    legacy
        .as_object_mut()
        .unwrap()
        .insert("model".into(), json!({"obsolete_invalid_shape": true}));
    write_private(&config_file, &serde_json::to_vec(&legacy).unwrap());

    let output = run_with_input(
        conf_command(home.path(), workspace.path(), root.path()),
        b"",
    )
    .await;
    assert!(
        output.status.success(),
        "conf failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = first_json(&output).unwrap();
    assert_eq!(report["configured"], false);
    assert_eq!(report["model_configured"], true);
    assert_eq!(std::fs::read(&models_file).unwrap(), profile_bytes);
    assert_eq!(
        serde_json::from_slice::<Value>(&std::fs::read(&config_file).unwrap()).unwrap(),
        remaining
    );
}

#[tokio::test]
async fn explicit_conf_cancel_and_save_do_not_create_local_state() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    private_dir(root.path());
    let config_file = root.path().join("headless.json");

    let mut cancelled = cli(home.path(), workspace.path(), root.path());
    cancelled.args(["conf", "--config"]).arg(&config_file);
    let output = run_with_input(cancelled, b"").await;
    assert!(
        output.status.success(),
        "cancelled explicit conf failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = first_json(&output).unwrap();
    assert_eq!(report["configured"], false);
    assert_eq!(report["model_configured"], false);
    assert_eq!(report["config_file"], config_file.to_str().unwrap());
    assert!(!config_file.exists());
    assert!(!root.path().join("workspaces").exists());
    assert!(!home.path().join(".cayw").exists());

    let mut configured = cli(home.path(), workspace.path(), root.path());
    configured.args(["conf", "--config"]).arg(&config_file);
    let output = run_with_input(
        configured,
        b"openai\n\nhttp://127.0.0.1:9/v1\nheadless-model\n\n128\n\n",
    )
    .await;
    assert!(
        output.status.success(),
        "explicit conf failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = first_json(&output).unwrap();
    assert_eq!(report["configured"], true);
    assert_eq!(report["model_configured"], true);
    let saved: Value = serde_json::from_slice(&std::fs::read(&config_file).unwrap()).unwrap();
    assert_eq!(saved["model"], model_config("headless-model", 128));
    assert!(!root.path().join("workspaces").exists());
    assert!(!home.path().join(".cayw").exists());
}
