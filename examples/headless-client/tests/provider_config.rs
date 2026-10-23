#![cfg(target_os = "macos")]
use serde_json::{Value, json};
use std::{
    path::Path,
    process::{Command, Output},
};

fn configuration(service: &str, api: &str, version: &str, base: &str) -> Value {
    json!({"model": {
        "service":service, "api":api, "id":"fixture-model", "base_url":base,
        "api_version":version, "max_output_tokens":64,
        "api_key_env":"CAYW_CLIENT_CONFIG_TEST_KEY"
    }})
}

fn command(root: &Path, input: &Value) -> Command {
    std::fs::create_dir_all(root.join("workspace")).unwrap();
    std::fs::write(root.join("config.json"), serde_json::to_vec(input).unwrap()).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_cayw-client"));
    command
        .env_clear()
        .env(
            "CAYW_CLIENT_CONFIG_TEST_KEY",
            "synthetic-client-selected-key",
        )
        .args(["--mode", "sdk", "--db"])
        .arg(root.join("session.db"))
        .arg("--workspace")
        .arg(root.join("workspace"))
        .arg("--config")
        .arg(root.join("config.json"))
        .arg("create");
    command
}

fn metadata(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout.clone())
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|row| row["type"] == "runtime")
        .unwrap()
}

// Break caught: SDK ignores the canonical model or requires the removed environment trio.
#[test]
fn sdk_selects_every_service_from_the_same_model_configuration() {
    for (service, api, version, prefix) in [
        ("openai", "chat_completions", "v1", "v1"),
        ("magpie", "chat_completions", "v1", "v1"),
        ("anthropic", "messages", "2023-06-01", "v1"),
        ("gemini", "stream_generate_content", "v1beta", "v1beta"),
        ("gemini", "generate_content", "v1beta", "v1beta"),
    ] {
        let root = tempfile::tempdir().unwrap();
        let input = configuration(
            service,
            api,
            version,
            &format!("http://127.0.0.1:9/{prefix}"),
        );
        let output = command(root.path(), &input).output().unwrap();
        let row = metadata(&output);
        assert_eq!(row["model_configuration"], input["model"]);
        assert_eq!(row["requested_model"], "fixture-model");
        assert!(!String::from_utf8_lossy(&output.stdout).contains("synthetic-client-selected-key"));
    }
}

// Break caught: the old environment can silently supply an SDK model without --config.
#[test]
fn sdk_does_not_fall_back_to_legacy_environment() {
    let root = tempfile::tempdir().unwrap();
    let output = command(root.path(), &json!({}))
        .env("CAYW_PROVIDER_BASE_URL", "http://127.0.0.1:9/v1")
        .env("CAYW_PROVIDER_API_KEY", "synthetic-old-key")
        .env("CAYW_MODEL", "synthetic-old-model")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!root.path().join("session.db").exists());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("model configuration"), "{error}");
    assert!(!error.contains("synthetic-old-key"));
}

// Break caught: an unrelated credential hides a missing selected credential.
#[test]
fn sdk_requires_only_the_selected_credential_and_never_prints_it() {
    let root = tempfile::tempdir().unwrap();
    let input = configuration(
        "gemini",
        "generate_content",
        "v1beta",
        "http://127.0.0.1:9/v1beta",
    );
    let output = command(root.path(), &input)
        .env_remove("CAYW_CLIENT_CONFIG_TEST_KEY")
        .env("CAYW_PROVIDER_API_KEY", "synthetic-unselected-key")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!root.path().join("session.db").exists());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("CAYW_CLIENT_CONFIG_TEST_KEY"), "{error}");
    assert!(!error.contains("synthetic-unselected-key"));
}
