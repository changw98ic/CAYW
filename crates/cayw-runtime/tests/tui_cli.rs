#![cfg(target_os = "macos")]

use cayw_ipc::Client;
use serde_json::{Value, json};
use std::{os::unix::fs::PermissionsExt, path::Path, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
};

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

#[tokio::test]
async fn tui_uses_the_selected_workspace_and_sibling_program_without_secret_arguments() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let installed = tempfile::tempdir().unwrap();
    let mut server = command(
        Path::new(env!("CARGO_BIN_EXE_cayw")),
        home.path(),
        workspace.path(),
    )
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
    let binary = installed.path().join("cayw");
    std::fs::copy(env!("CARGO_BIN_EXE_cayw"), &binary).unwrap();
    let sibling = installed.path().join("cayw-tui");
    std::fs::write(
        &sibling,
        b"#!/bin/sh\n[ \"$#\" -eq 0 ] || exit 91\n[ \"$CAYW_IPC_ADDR\" = \"$EXPECTED_ADDR\" ] || exit 92\n[ \"$CAYW_IPC_TOKEN\" = \"$EXPECTED_TOKEN\" ] || exit 93\nprintf 'workspace connection received\\n'\nexit 7\n",
    )
    .unwrap();
    std::fs::set_permissions(&sibling, std::fs::Permissions::from_mode(0o700)).unwrap();
    let output = tokio::time::timeout(
        WATCHDOG,
        command(&binary, home.path(), other.path())
            .arg("tui")
            .arg("--workspace")
            .arg(workspace.path())
            .env("EXPECTED_ADDR", ready["listening"].as_str().unwrap())
            .env("EXPECTED_TOKEN", &token)
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
    assert_eq!(output.status.code(), Some(7));
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
    assert!(!Path::new(ready["connection_file"].as_str().unwrap()).exists());
}
