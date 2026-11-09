#![cfg(target_os = "macos")]

#[cfg(feature = "plugins")]
use cayw_ipc::Client;
#[cfg(feature = "plugins")]
use serde_json::{Value, json};
#[cfg(feature = "plugins")]
use std::{os::unix::fs::PermissionsExt, time::Duration};
use std::{path::Path, process::Stdio};
#[cfg(feature = "plugins")]
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

#[cfg(feature = "plugins")]
const WATCHDOG: Duration = Duration::from_secs(30);

fn command(binary: &Path, home: &Path, workspace: &Path) -> Command {
    let mut command = Command::new(binary);
    command
        .env_clear()
        .env("HOME", home)
        .current_dir(workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

#[tokio::test]
async fn tui_help_leaves_state_absent_and_missing_service_prints_the_next_step() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let binary = Path::new(env!("CARGO_BIN_EXE_cayw"));
    let help = command(binary, home.path(), workspace.path())
        .args(["tui", "--help"])
        .output()
        .await
        .unwrap();
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("cayw serve"));
    assert!(!home.path().join(".cayw").exists());

    let missing = command(binary, home.path(), workspace.path())
        .arg("tui")
        .output()
        .await
        .unwrap();
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("cayw serve"));
    assert!(!home.path().join(".cayw").exists());
}

#[cfg(feature = "plugins")]
#[tokio::test]
async fn tui_uses_the_selected_workspace_and_sibling_program_without_secret_arguments() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let installed = tempfile::tempdir().unwrap();
    let binary = installed.path().join("cayw");
    std::fs::copy(env!("CARGO_BIN_EXE_cayw"), &binary).unwrap();
    let sibling = installed.path().join("cayw-tui");
    std::fs::copy(env!("CARGO_BIN_EXE_cayw-runtime-plugin-fixture"), &sibling).unwrap();
    std::fs::set_permissions(&sibling, std::fs::Permissions::from_mode(0o700)).unwrap();
    // The unique Client validates its own packaged sibling. Both processes
    // must use this isolated installation, not companions in target/debug.
    let mut server = command(&binary, home.path(), workspace.path())
        .arg("serve")
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(server.stdout.take().unwrap()).lines();
    let ready = tokio::time::timeout(WATCHDOG, lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let ready: Value = serde_json::from_str(&ready).unwrap();
    let token = std::fs::read_to_string(ready["token_file"].as_str().unwrap()).unwrap();
    let output = tokio::time::timeout(
        WATCHDOG,
        command(&binary, home.path(), other.path())
            .arg("tui")
            .arg("--workspace")
            .arg(workspace.path())
            .env("EXPECTED_ADDR", ready["listening"].as_str().unwrap())
            .env("EXPECTED_TOKEN", &token)
            .env("EXPECTED_WORKSPACE_ID", "local")
            .env("CAYW_TEST_TUI_FIXTURE", "1")
            .env("CAYW_IPC_ADDR", "127.0.0.1:1")
            .env(
                "CAYW_IPC_TOKEN",
                "ambient-value-must-not-select-another-host",
            )
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        output.status.success(),
        "TUI launcher failed with {}; stdout: {}; stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(output.stdout, b"workspace connection received\n");
    assert!(output.stderr.is_empty());
    let client = Client::connect(ready["listening"].as_str().unwrap().parse().unwrap(), token)
        .await
        .unwrap();
    assert_eq!(
        client.request("session.list", json!({})).await.unwrap(),
        json!([])
    );
    client.close().await;
    let signalled = std::process::Command::new("/bin/kill")
        .args(["-INT", &server.id().unwrap().to_string()])
        .status()
        .unwrap();
    assert!(signalled.success());
    assert!(
        tokio::time::timeout(WATCHDOG, server.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    assert!(!Path::new(ready["client_file"].as_str().unwrap()).exists());
}
