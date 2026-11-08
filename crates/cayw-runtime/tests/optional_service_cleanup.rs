#![cfg(target_os = "macos")]

use serde_json::json;
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::Command,
    time::{Duration, Instant},
};

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\"'\"'"))
}

fn process_exists(pid: u32) -> bool {
    let pid = pid.to_string();
    Command::new("/bin/ps")
        .args(["-p", &pid, "-o", "pid="])
        .output()
        .is_ok_and(|output| {
            output.status.success()
                && String::from_utf8_lossy(&output.stdout)
                    .split_whitespace()
                    .any(|value| value == pid)
        })
}

fn assert_process_stopped(marker: &Path) {
    let pid: u32 = fs::read_to_string(marker)
        .expect("MCP fixture must start before later setup fails")
        .parse()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while process_exists(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !process_exists(pid),
        "MCP fixture {pid} survived failed setup"
    );
}

fn write_eager_mcp(root: &Path, prefix: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let marker = root.join(format!("{prefix}.pid"));
    let wrapper = root.join(format!("{prefix}-wrapper.sh"));
    let fixture = Path::new(env!("CARGO_BIN_EXE_cayw-runtime-mcp-fixture"));
    fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\numask 077\nprintf '%s' \"$$\" > {}\nexec {}\n",
            shell_quote(&marker),
            shell_quote(fixture),
        ),
    )
    .unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).unwrap();
    let mcp_file = root.join(format!("{prefix}-mcp.json"));
    fs::write(
        &mcp_file,
        serde_json::to_vec(&json!({
            "mcpServers": {
                (prefix): {
                    "command": wrapper,
                    "lifecycle": "eager"
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();
    (mcp_file, marker)
}

#[test]
fn failed_pi_setup_closes_mcp_started_earlier() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    let (mcp_file, marker) = write_eager_mcp(root.path(), "pi-rollback-fixture");
    let config = root.path().join("config.json");
    fs::write(
        &config,
        serde_json::to_vec(&json!({
            "mcp_servers_file": mcp_file,
            "pi_compat": {
                "enabled": true,
                "sample_extension": root.path().join("missing-pi-extension.js")
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_cayw"))
        .arg("serve")
        .arg("--workspace")
        .arg(&workspace)
        .arg("--db")
        .arg(root.path().join("sessions.db"))
        .arg("--listen")
        .arg("127.0.0.1:0")
        .arg("--token-env")
        .arg("CAYW_OPTIONAL_CLEANUP_TOKEN")
        .arg("--config")
        .arg(&config)
        .arg("--offline")
        .env(
            "CAYW_OPTIONAL_CLEANUP_TOKEN",
            "optional-cleanup-token-1234567890",
        )
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("install pi sample extension"),
        "unexpected stderr: {stderr}"
    );

    assert_process_stopped(&marker);
}

#[test]
fn failed_runtime_open_closes_prepared_mcp() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    let (mcp_file, marker) = write_eager_mcp(root.path(), "runtime-rollback-fixture");
    let config = root.path().join("runtime-config.json");
    fs::write(
        &config,
        serde_json::to_vec(&json!({"mcp_servers_file": mcp_file})).unwrap(),
    )
    .unwrap();
    let database_directory = root.path().join("database-is-a-directory");
    fs::create_dir(&database_directory).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_cayw"))
        .arg("serve")
        .arg("--workspace")
        .arg(&workspace)
        .arg("--db")
        .arg(&database_directory)
        .arg("--listen")
        .arg("127.0.0.1:0")
        .arg("--token-env")
        .arg("CAYW_OPTIONAL_CLEANUP_TOKEN")
        .arg("--config")
        .arg(&config)
        .arg("--offline")
        .env(
            "CAYW_OPTIONAL_CLEANUP_TOKEN",
            "optional-cleanup-token-1234567890",
        )
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        output.stdout.is_empty(),
        "failed startup published readiness"
    );
    assert_process_stopped(&marker);
}
