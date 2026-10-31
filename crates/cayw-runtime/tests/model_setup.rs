//! First-run model setup for the local CLI.
//!
//! Every test spawns the real `cayw` binary with a cleared environment and a
//! temporary HOME/CAYW_HOME. Only the public CLI, the config file on disk and
//! the public IPC client are used. No test reads a real credential, opens a
//! network connection or touches the user's own `~/.cayw`.
#![cfg(target_os = "macos")]

use cayw_ipc::Client;
use serde_json::{Value, json};
use std::{
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::Path,
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

/// A credential value that must never reach the config file or either output
/// stream. It is deliberately distinctive so a leak is easy to spot.
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

fn configure_command(home: &Path, workspace: &Path, cayw_home: &Path) -> Command {
    let mut command = cli(home, workspace, cayw_home);
    command.args(["configure", "--workspace"]).arg(workspace);
    command
}

fn serve_command(home: &Path, workspace: &Path, cayw_home: &Path) -> Command {
    let mut command = cli(home, workspace, cayw_home);
    command.args(["serve", "--workspace"]).arg(workspace);
    command
}

/// Write the wizard answers and then close stdin, so the child sees EOF.
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

/// Keep the stdin pipe open (an ordinary pipe, not a terminal, and no EOF).
/// A command that wrongly waits for an answer blocks here and fails the guard.
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

fn assert_not_configured(output: &std::process::Output) {
    if let Some(report) = first_json(output) {
        assert_ne!(
            report["configured"],
            json!(true),
            "a cancelled or EOF run must not report a completed configuration"
        );
    }
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

fn model_config(id: &str, max_output_tokens: u64) -> Value {
    json!({
        "service": "openai",
        "api": "chat_completions",
        "id": id,
        "base_url": "http://127.0.0.1:9/v1",
        "api_version": "v1",
        "max_output_tokens": max_output_tokens,
        "api_key_env": "OPENAI_API_KEY"
    })
}

// Break caught: `cayw configure` from an ordinary pipe does not save every
// model field, writes the credential value, or replaces host-owned settings.
#[tokio::test]
async fn pipe_configure_saves_the_model_and_preserves_existing_settings() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    private_dir(root.path());
    let config_file = root.path().join("config.json");
    write_private(
        &config_file,
        &serde_json::to_vec(&json!({
            "budgets": {"max_total_tokens": 123456, "max_turns": 7},
            "scheduler": {"max_concurrent_runs": 2},
            "permissions": [{"capability": "fs.read", "arguments": {"path": "notes.txt"}}]
        }))
        .unwrap(),
    );

    let mut command = configure_command(home.path(), workspace.path(), root.path());
    // The wizard must save only the variable name, never the value it points at.
    command.env("OPENAI_API_KEY", SECRET);
    let output = run_with_input(
        command,
        b"openai\n\nhttp://127.0.0.1:9/v1\nfixture-model\n\n\n\n",
    )
    .await;
    assert!(
        output.status.success(),
        "configure failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let report = first_json(&output).expect("configure prints one JSON line");
    assert_eq!(report["configured"], json!(true));
    assert_eq!(report["model_configured"], json!(true));
    assert_eq!(
        report["config_file"].as_str(),
        Some(config_file.to_str().unwrap())
    );

    let saved: Value = serde_json::from_slice(&std::fs::read(&config_file).unwrap()).unwrap();
    // Blank answers select the documented defaults: chat_completions,
    // OPENAI_API_KEY and 4096 tokens (the explicit base URL is kept as typed).
    assert_eq!(saved["model"], model_config("fixture-model", 4096));

    // Host-owned configuration survives the new model write.
    assert_eq!(saved["budgets"]["max_total_tokens"], json!(123456));
    assert_eq!(saved["budgets"]["max_turns"], json!(7));
    assert_eq!(saved["scheduler"]["max_concurrent_runs"], json!(2));
    assert_eq!(saved["permissions"][0]["capability"], json!("fs.read"));
    assert_eq!(
        saved["permissions"][0]["arguments"]["path"],
        json!("notes.txt")
    );

    let text = String::from_utf8(std::fs::read(&config_file).unwrap()).unwrap();
    assert!(
        !text.contains(SECRET),
        "the credential value must never be written to the config"
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains(SECRET));
    assert!(!String::from_utf8_lossy(&output.stderr).contains(SECRET));
}

// Break caught: cancelling at the confirmation prompt or closing stdin halfway
// rewrites the existing config, losing the user's current settings.
#[tokio::test]
async fn cancel_or_eof_leaves_the_existing_config_untouched() {
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
    let before: Value = serde_json::from_slice(&std::fs::read(&config_file).unwrap()).unwrap();

    // Explicit "n" at the save confirmation.
    let cancel = run_with_input(
        configure_command(home.path(), workspace.path(), root.path()),
        b"openai\n\nhttp://127.0.0.1:9/v1\ncancelled-model\n\n\nn\n",
    )
    .await;
    assert_not_configured(&cancel);
    assert_eq!(
        serde_json::from_slice::<Value>(&std::fs::read(&config_file).unwrap()).unwrap(),
        before,
        "cancelling must not change the existing config"
    );

    // EOF before the confirmation prompt, after a valid service and URL.
    let eof = run_with_input(
        configure_command(home.path(), workspace.path(), root.path()),
        b"openai\n\nhttp://127.0.0.1:9/v1\n",
    )
    .await;
    assert_not_configured(&eof);
    assert_eq!(
        serde_json::from_slice::<Value>(&std::fs::read(&config_file).unwrap()).unwrap(),
        before,
        "EOF must not change the existing config"
    );
}

// Break caught: `init` tries to resolve the credential while validating a
// configured model, so a machine with no key can never finish first-run setup.
#[tokio::test]
async fn init_after_configure_does_not_require_the_credential() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    private_dir(root.path());

    // No OPENAI_API_KEY is placed in the cleared environment.
    let configured = run_with_input(
        configure_command(home.path(), workspace.path(), root.path()),
        b"openai\n\nhttp://127.0.0.1:9/v1\nfixture-model\n\n\n\n",
    )
    .await;
    assert!(
        configured.status.success(),
        "configure failed: {}",
        String::from_utf8_lossy(&configured.stderr)
    );

    let output = run_with_input(
        init_command(home.path(), workspace.path(), root.path()),
        b"",
    )
    .await;
    assert!(
        output.status.success(),
        "init failed without a credential: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = first_json(&output).expect("init prints one JSON line");
    assert_eq!(report["model_configured"], json!(true));
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("未配置模型"),
        "init must not claim the model is missing after configure"
    );
}

// Break caught: a non-terminal `init`/`serve` gives no actionable setup path,
// blocks waiting on an ordinary pipe, or omits the real config file location.
#[tokio::test]
async fn missing_model_reports_configure_and_the_actual_path_without_blocking() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    private_dir(root.path());
    let config_file = root.path().join("config.json");
    let config_text = config_file.to_str().unwrap();

    let output = run_holding_stdin(init_command(home.path(), workspace.path(), root.path())).await;
    assert!(
        output.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = first_json(&output).expect("init prints one JSON line");
    assert_eq!(report["model_configured"], json!(false));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("未配置模型"), "stderr: {stderr}");
    assert!(stderr.contains("cayw configure"), "stderr: {stderr}");
    assert!(stderr.contains(config_text), "stderr: {stderr}");

    // A bare `serve` whose stdin is an ordinary open pipe must start, not ask.
    let mut child = serve_command(home.path(), workspace.path(), root.path())
        .spawn()
        .unwrap();
    let _stdin = child.stdin.take().expect("serve stdin is piped");
    let mut serve_stderr = child.stderr.take().expect("serve stderr is piped");
    let ready = read_ready(&mut child).await;
    assert_eq!(ready["model_configured"], json!(false));
    assert_eq!(ready["config_file"].as_str(), Some(config_text));

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
    assert!(
        error.message.contains(config_text),
        "RPC error must name the actual config file: {}",
        error.message
    );
    assert!(
        error.message.contains("cayw configure"),
        "RPC error must name the setup command: {}",
        error.message
    );
    client.close().await;

    child.kill().await.unwrap();
    child.wait().await.unwrap();
    let mut serve_text = String::new();
    serve_stderr.read_to_string(&mut serve_text).await.unwrap();
    assert!(serve_text.contains("未配置模型"), "stderr: {serve_text}");
    assert!(
        serve_text.contains("cayw configure"),
        "stderr: {serve_text}"
    );
    assert!(serve_text.contains(config_text), "stderr: {serve_text}");
}

// Break caught: `configure` runs the wizard again even though a model already
// exists, or it overwrites the existing model instead of keeping it.
#[tokio::test]
async fn configure_keeps_an_existing_model_without_asking_again() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    private_dir(root.path());
    let config_file = root.path().join("config.json");
    let existing = json!({
        "model": model_config("keep-model", 64),
        "budgets": {"max_turns": 3},
        "permissions": [{"capability": "fs.read", "arguments": {"path": "notes.txt"}}]
    });
    write_private(&config_file, &serde_json::to_vec(&existing).unwrap());
    let before: Value = serde_json::from_slice(&std::fs::read(&config_file).unwrap()).unwrap();

    // A re-prompt would block forever because this pipe is neither closed nor
    // written to; the watchdog turns that into a clear failure.
    let output = run_holding_stdin(configure_command(
        home.path(),
        workspace.path(),
        root.path(),
    ))
    .await;
    assert!(
        output.status.success(),
        "configure failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let report = first_json(&output).expect("configure prints one JSON line");
    assert_eq!(report["model_configured"], json!(true));
    assert_eq!(
        serde_json::from_slice::<Value>(&std::fs::read(&config_file).unwrap()).unwrap(),
        before,
        "an existing model and its settings must be kept unchanged"
    );
}
