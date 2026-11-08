#![cfg(target_os = "macos")]
use serde_json::{Value, json};
use std::{
    path::Path,
    process::{Command, Output},
};

const MODEL_KEY: &str = "synthetic-client-selected-key";

fn configuration(service: &str, api: &str, version: &str, base: &str) -> Value {
    json!({"model": {
        "service":service, "api":api, "id":"fixture-model", "base_url":base,
        "api_version":version, "max_output_tokens":64,
        "api_key":MODEL_KEY
    }})
}

fn public_model(input: &Value) -> Value {
    let mut model = input["model"].clone();
    let authenticated = model.as_object_mut().unwrap().remove("api_key").is_some();
    model["authentication"] = json!(if authenticated { "plain_file" } else { "none" });
    model
}

fn sdk_command(root: &Path) -> Command {
    std::fs::create_dir_all(root.join("workspace")).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_cayw-client"));
    command
        .env_clear()
        .args(["--mode", "sdk", "--db"])
        .arg(root.join("session.db"))
        .arg("--workspace")
        .arg(root.join("workspace"));
    command
}

fn command(root: &Path, input: &Value) -> Command {
    std::fs::write(root.join("config.json"), serde_json::to_vec(input).unwrap()).unwrap();
    let mut command = sdk_command(root);
    command.arg("--config").arg(root.join("config.json"));
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

fn run(mut command: Command) -> Vec<Value> {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect()
}

fn result_id(lines: &[Value]) -> String {
    lines
        .iter()
        .rev()
        .find(|row| row["type"] == "result")
        .unwrap()["result"]["id"]
        .as_str()
        .unwrap()
        .to_owned()
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
        let output = command(root.path(), &input).arg("create").output().unwrap();
        let row = metadata(&output);
        assert_eq!(row["model_configuration"], public_model(&input));
        assert_eq!(row["requested_model"], "fixture-model");
        assert_eq!(row["model_execution"], "not_requested");
        assert!(!String::from_utf8_lossy(&output.stdout).contains(MODEL_KEY));
    }
}

// Break caught: the old environment can silently supply an SDK model without --config.
#[test]
fn sdk_does_not_fall_back_to_legacy_environment() {
    let root = tempfile::tempdir().unwrap();
    let mut create = sdk_command(root.path());
    create
        .env("CAYW_PROVIDER_BASE_URL", "http://127.0.0.1:9/v1")
        .env("CAYW_PROVIDER_API_KEY", "synthetic-old-key")
        .env("CAYW_MODEL", "synthetic-old-model")
        .arg("create");
    let created = run(create);
    let row = created.iter().find(|row| row["type"] == "runtime").unwrap();
    assert!(row["model_configuration"].is_null());
    assert!(row["requested_model"].is_null());
    let id = result_id(&created);
    let mut prompt = sdk_command(root.path());
    prompt
        .env("CAYW_PROVIDER_BASE_URL", "http://127.0.0.1:9/v1")
        .env("CAYW_PROVIDER_API_KEY", "synthetic-old-key")
        .env("CAYW_MODEL", "synthetic-old-model")
        .args(["prompt", "--session", &id, "--text", "must not run"]);
    let output = prompt.output().unwrap();
    assert!(!output.status.success());
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error["code"], "model_not_configured");
    assert!(!error["outcome_unknown"].as_bool().unwrap());
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("显式 --config")
    );
    assert!(!String::from_utf8_lossy(&output.stderr).contains("synthetic-old-key"));
}

// Break caught: legacy environment values override the explicit plain-file key
// or the public Runtime metadata prints either credential.
#[test]
fn sdk_uses_only_the_plain_file_key_and_never_prints_it() {
    let root = tempfile::tempdir().unwrap();
    let input = configuration(
        "gemini",
        "generate_content",
        "v1beta",
        "http://127.0.0.1:9/v1beta",
    );
    let mut create = command(root.path(), &input);
    create
        .env("CAYW_PROVIDER_API_KEY", "synthetic-unselected-key")
        .env("CAYW_MODEL", "legacy-environment-model")
        .arg("create");
    let output = create.output().unwrap();
    let runtime = metadata(&output);
    assert_eq!(runtime["model_configuration"], public_model(&input));
    assert_eq!(runtime["requested_model"], "fixture-model");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains(MODEL_KEY));
    assert!(!stdout.contains("synthetic-unselected-key"));
    assert!(!stdout.contains("legacy-environment-model"));
}

// Break caught: query and control commands require a model object or credential
// before they even dispatch to the persisted Runtime.
#[test]
fn sdk_read_and_control_commands_need_no_model_configuration_or_credential() {
    let root = tempfile::tempdir().unwrap();
    let mut create = sdk_command(root.path());
    create.arg("create");
    let created = run(create);
    let row = created.iter().find(|row| row["type"] == "runtime").unwrap();
    assert!(row["model_configuration"].is_null());
    assert!(row["requested_model"].is_null());
    assert_eq!(row["model_execution"], "not_requested");
    let id = result_id(&created);

    for arguments in [
        vec!["get", "--session", id.as_str()],
        vec!["tasks", "--session", id.as_str()],
        vec![
            "audit",
            "--session",
            id.as_str(),
            "--after",
            "0",
            "--limit",
            "10",
        ],
        vec![
            "events",
            "--session",
            id.as_str(),
            "--after",
            "0",
            "--limit",
            "10",
        ],
        vec!["recover", "--session", id.as_str()],
    ] {
        let mut query = sdk_command(root.path());
        query.args(arguments);
        let lines = run(query);
        let runtime = lines.iter().find(|row| row["type"] == "runtime").unwrap();
        assert!(runtime["model_configuration"].is_null());
        assert_eq!(runtime["model_execution"], "not_requested");
    }

    for arguments in [
        vec!["cancel", "--session", id.as_str(), "--task", "missing-task"],
        vec![
            "approve",
            "--session",
            id.as_str(),
            "--approval",
            "missing-approval",
            "--decision",
            "allow_once",
        ],
    ] {
        let output = sdk_command(root.path()).args(arguments).output().unwrap();
        assert!(!output.status.success());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(!error.contains("model configuration"), "{error}");
        assert!(!error.contains(MODEL_KEY), "{error}");
    }
}

// Break caught: a host-selected no-authentication deployment is rejected before
// a read-only query even though its model adapter is never used.
#[test]
fn sdk_queries_accept_host_selected_deployment_without_authentication() {
    let root = tempfile::tempdir().unwrap();
    let mut input = configuration(
        "openai",
        "chat_completions",
        "v1",
        "http://gateway.invalid/gateway/v1",
    );
    input["model"].as_object_mut().unwrap().remove("api_key");
    let mut create = command(root.path(), &input);
    create.arg("create");
    let created = run(create);
    let runtime = created.iter().find(|row| row["type"] == "runtime").unwrap();
    assert_eq!(runtime["model_configuration"], public_model(&input));
    assert_eq!(runtime["requested_model"], "fixture-model");
    assert_eq!(runtime["model_execution"], "not_requested");
    let id = result_id(&created);

    let mut get = command(root.path(), &input);
    get.args(["get", "--session", &id]);
    let lines = run(get);
    assert_eq!(result_id(&lines), id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sdk_controls_approve_and_cancel_real_tasks_without_model_configuration() {
    use cayw_ai::{
        AssistantEvent, AssistantStream, CancellationToken, Model, ModelRequest, Provider,
    };
    use cayw_runtime::{Runtime, RuntimeConfig, TaskState};
    use std::{sync::Arc, time::Duration};

    struct Write;
    impl Provider for Write {
        fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
            let message = cayw_runtime::fixture::script_response(
                &request,
                "return cayw.call('fs.write',{path='approved.txt',content='from original owner'})",
                &format!("control-{}", request.messages.len()),
            );
            Box::pin(futures::stream::iter([AssistantEvent::Done { message }]))
        }
    }
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("workspace")).unwrap();
    let runtime = Runtime::open(
        root.path().join("session.db"),
        root.path().join("workspace"),
        Arc::new(Write),
        Model::new("fixture", "client-control"),
        RuntimeConfig::default(),
    )
    .unwrap();
    let session = runtime.create(None).unwrap();
    let task = runtime
        .enqueue(&session.id, "write after explicit approval")
        .unwrap();
    let execution = tokio::spawn({
        let runtime = runtime.clone();
        let task = task.clone();
        async move { runtime.resume(&task.id, CancellationToken::new()).await }
    });
    let approval = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Some(approval) = runtime
                .get(&session.id)
                .unwrap()
                .pending_approvals
                .into_iter()
                .next()
            {
                break approval;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("original Runtime must request approval");
    let mut approve = sdk_command(root.path());
    approve.args([
        "approve",
        "--session",
        &session.id,
        "--approval",
        &approval.id,
        "--decision",
        "allow_once",
    ]);
    let rows = run(approve);
    assert_eq!(rows.last().unwrap()["command"], "approve");
    tokio::time::timeout(Duration::from_secs(20), execution)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(runtime.task(&task.id).unwrap().state, TaskState::Completed);
    assert_eq!(
        std::fs::read(root.path().join("workspace/approved.txt")).unwrap(),
        b"from original owner"
    );

    let queued = runtime
        .enqueue(&session.id, "cancel before any model execution")
        .unwrap();
    let mut cancel = sdk_command(root.path());
    cancel.args(["cancel", "--session", &session.id, "--task", &queued.id]);
    let rows = run(cancel);
    assert_eq!(rows.last().unwrap()["result"]["cancelled"], true);
    assert_eq!(
        runtime.task(&queued.id).unwrap().state,
        TaskState::Cancelled
    );
}
