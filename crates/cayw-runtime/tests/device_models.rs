//! Device model list and the main-session workspace.
//!
//! Each test starts the real `cayw` binary with a cleared environment. Credentials
//! are synthetic. No test calls a model service or touches the user's home.
#![cfg(target_os = "macos")]

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    os::unix::{
        ffi::OsStrExt,
        fs::{OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::process::Command;

const WATCHDOG: Duration = Duration::from_secs(30);

fn private_dir(path: &Path) {
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn write_private(path: &Path, bytes: &[u8]) {
    if let Some(parent) = path.parent() {
        private_dir(parent);
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    file.write_all(bytes).unwrap();
    file.sync_all().unwrap();
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn workspace_key(workspace: &Path) -> String {
    let canonical = workspace.canonicalize().unwrap();
    format!("{:x}", Sha256::digest(canonical.as_os_str().as_bytes()))
}

fn command(home: &Path, workspace: &Path, cayw_home: &Path, main_session: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cayw"));
    command
        .env_clear()
        .env("HOME", home)
        .env("CAYW_HOME", cayw_home)
        .env("CAYW_MAIN_SESSION", main_session)
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

async fn init(
    home: &Path,
    workspace: &Path,
    cayw_home: &Path,
    main_session: &Path,
) -> std::process::Output {
    let mut command = command(home, workspace, cayw_home, main_session);
    command.arg("init").arg("--workspace").arg(workspace);
    run(command).await
}

fn profile(id: &str, key: &str) -> Value {
    json!({
        "service": "openai",
        "api": "chat_completions",
        "id": id,
        "base_url": "http://127.0.0.1:9/v1",
        "api_version": "v1",
        "max_output_tokens": 64,
        "api_key": key
    })
}

fn plant_legacy(cayw_home: &Path, workspace: &Path, name: &str, model: &Value) {
    let path = cayw_home
        .join("workspaces")
        .join(workspace_key(workspace))
        .join("models.json");
    write_private(
        &path,
        &serde_json::to_vec_pretty(&json!({"selected": name, "models": {name: model}})).unwrap(),
    );
}

fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn name_for_key(catalog: &Value, key: &str) -> String {
    catalog["models"]
        .as_object()
        .unwrap()
        .iter()
        .find(|(_, model)| model["api_key"] == key)
        .map(|(name, _)| name.clone())
        .unwrap_or_else(|| panic!("missing profile for key {key}: {catalog}"))
}

struct RestoreMode {
    path: PathBuf,
}

impl Drop for RestoreMode {
    fn drop(&mut self) {
        let _ = fs::set_permissions(&self.path, fs::Permissions::from_mode(0o700));
    }
}

#[tokio::test]
async fn identical_profiles_collapse_and_doctor_hides_the_key() {
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    private_dir(root.path());
    let main_session = home.path().join("main-session");
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let key = "synthetic-collapse-key-never-print";
    for workspace in [first.path(), second.path()] {
        assert!(
            init(home.path(), workspace, root.path(), &main_session)
                .await
                .status
                .success()
        );
    }
    let model = profile("shared-model", key);
    plant_legacy(root.path(), first.path(), "daily", &model);
    plant_legacy(root.path(), second.path(), "daily", &model);

    let migrated = init(home.path(), first.path(), root.path(), &main_session).await;
    assert!(
        migrated.status.success(),
        "migration failed: {}",
        String::from_utf8_lossy(&migrated.stderr)
    );
    assert!(
        !main_session.exists(),
        "init must not create the main session"
    );

    let device = root.path().join("models.json");
    assert_eq!(mode(&device), 0o600);
    let catalog = read_json(&device);
    assert_eq!(catalog["models"].as_object().unwrap().len(), 1);
    let name = name_for_key(&catalog, key);
    assert_eq!(catalog["default"], name);
    for workspace in [first.path(), second.path()] {
        let legacy = root
            .path()
            .join("workspaces")
            .join(workspace_key(workspace))
            .join("models.json");
        assert!(!legacy.exists());
        let selection_path = root
            .path()
            .join("workspaces")
            .join(workspace_key(workspace))
            .join("model-selection.json");
        assert_eq!(mode(&selection_path), 0o600);
        assert_eq!(read_json(&selection_path)["name"], name);
    }

    let mut doctor = command(home.path(), first.path(), root.path(), &main_session);
    doctor.arg("doctor").arg("--workspace").arg(first.path());
    let output = run(doctor).await;
    assert!(
        output.status.success(),
        "doctor failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!text.contains(key), "doctor printed the synthetic key");
}

#[tokio::test]
async fn same_name_with_different_keys_stays_two_profiles() {
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    private_dir(root.path());
    let main_session = home.path().join("main-session");
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    for workspace in [first.path(), second.path()] {
        assert!(
            init(home.path(), workspace, root.path(), &main_session)
                .await
                .status
                .success()
        );
    }
    let key_a = "synthetic-rename-key-a-never-print";
    let key_b = "synthetic-rename-key-b-never-print";
    plant_legacy(
        root.path(),
        first.path(),
        "daily",
        &profile("model-a", key_a),
    );
    plant_legacy(
        root.path(),
        second.path(),
        "daily",
        &profile("model-b", key_b),
    );

    let migrated = init(home.path(), first.path(), root.path(), &main_session).await;
    assert!(
        migrated.status.success(),
        "migration failed: {}",
        String::from_utf8_lossy(&migrated.stderr)
    );
    let catalog = read_json(&root.path().join("models.json"));
    assert_eq!(catalog["models"].as_object().unwrap().len(), 2);
    assert!(catalog.get("default").is_none());
    let name_a = name_for_key(&catalog, key_a);
    let name_b = name_for_key(&catalog, key_b);
    assert_ne!(name_a, name_b);
    for (workspace, name) in [(first.path(), &name_a), (second.path(), &name_b)] {
        let state = root
            .path()
            .join("workspaces")
            .join(workspace_key(workspace));
        assert!(!state.join("models.json").exists());
        assert_eq!(
            read_json(&state.join("model-selection.json"))["name"],
            *name
        );
    }
}

#[tokio::test]
async fn failed_selection_write_keeps_the_legacy_file() {
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    private_dir(root.path());
    let main_session = home.path().join("main-session");
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    for workspace in [first.path(), second.path()] {
        assert!(
            init(home.path(), workspace, root.path(), &main_session)
                .await
                .status
                .success()
        );
    }
    plant_legacy(
        root.path(),
        first.path(),
        "keep",
        &profile("keep-model", "synthetic-keep-key-never-print"),
    );
    plant_legacy(
        root.path(),
        second.path(),
        "keep",
        &profile("keep-model", "synthetic-keep-key-never-print"),
    );
    let blocked = root
        .path()
        .join("workspaces")
        .join(workspace_key(first.path()))
        .join("model-selection.json");
    fs::create_dir(&blocked).unwrap();
    let legacy = root
        .path()
        .join("workspaces")
        .join(workspace_key(first.path()))
        .join("models.json");
    let before = fs::read(&legacy).unwrap();

    let failed = init(home.path(), first.path(), root.path(), &main_session).await;
    assert!(
        !failed.status.success(),
        "a blocked selection file must fail migration"
    );
    assert_eq!(fs::read(&legacy).unwrap(), before);
    assert!(
        root.path()
            .join("workspaces")
            .join(workspace_key(second.path()))
            .join("models.json")
            .is_file()
    );
    assert!(!root.path().join("models.json").exists());
}

#[tokio::test]
async fn explicit_headless_ignores_the_device_list_and_main_session() {
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    private_dir(root.path());
    let workspace = tempfile::tempdir().unwrap();
    let main_session = home.path().join("main-session");
    let database = root.path().join("session.db");
    let config = root.path().join("headless.json");
    write_private(config.as_path(), b"{\"permissions\":[]}\n");
    let models = root.path().join("models.json");
    let planted = serde_json::to_vec_pretty(&json!({
        "default": "hidden",
        "models": {"hidden": profile("must-not-load", "synthetic-hidden-key-never-print")}
    }))
    .unwrap();
    write_private(&models, &planted);

    let mut serve = command(home.path(), workspace.path(), root.path(), &main_session);
    serve
        .args(["serve", "--db"])
        .arg(&database)
        .arg("--workspace")
        .arg(workspace.path())
        .args([
            "--listen",
            "127.0.0.1:0",
            "--token-env",
            "CAYW_EXPLICIT_TOKEN",
            "--config",
        ])
        .arg(&config)
        .env("CAYW_EXPLICIT_TOKEN", "explicit-headless-token");
    let mut child = serve.spawn().unwrap();
    let stdout = child.stdout.take().unwrap();
    let ready = tokio::time::timeout(WATCHDOG, async {
        use tokio::io::AsyncBufReadExt;
        let mut lines = tokio::io::BufReader::new(stdout).lines();
        lines.next_line().await
    })
    .await
    .expect("explicit serve must print readiness")
    .unwrap()
    .unwrap();
    let ready: Value = serde_json::from_str(&ready).unwrap();
    assert_eq!(ready["model_configured"], false);
    assert_eq!(ready["model"], "unconfigured");
    assert!(!main_session.exists());
    assert_eq!(fs::read(&models).unwrap(), planted);
    assert!(!root.path().join("workspaces").exists());
    child.kill().await.unwrap();
    child.wait().await.unwrap();
}

#[tokio::test]
async fn discoverable_serve_creates_a_private_main_session_and_init_does_not() {
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    private_dir(root.path());
    let workspace = tempfile::tempdir().unwrap();
    let main_session = home.path().join("main-session");
    let prepared = init(home.path(), workspace.path(), root.path(), &main_session).await;
    assert!(prepared.status.success());
    assert!(!main_session.exists());
    assert!(!root.path().join("models.json").exists());

    let mut serve = command(home.path(), workspace.path(), root.path(), &main_session);
    serve
        .arg("serve")
        .arg("--workspace")
        .arg(workspace.path())
        .args(["--listen", "127.0.0.1:0", "--offline"]);
    let mut child = serve.spawn().unwrap();
    let ready = tokio::time::timeout(WATCHDOG, async {
        use tokio::io::AsyncBufReadExt;
        let mut lines = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
        lines.next_line().await
    })
    .await
    .expect("serve must print readiness")
    .unwrap()
    .unwrap();
    let ready: Value = serde_json::from_str(&ready).unwrap();
    assert_eq!(ready["model_configured"], false);
    assert_eq!(mode(&main_session), 0o700);
    let record_path = root
        .path()
        .join("workspaces")
        .join(workspace_key(&main_session))
        .join("main-session.json");
    assert_eq!(mode(&record_path), 0o600);
    let record = read_json(&record_path);
    assert!(
        record["session_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty())
    );
    assert!(!root.path().join("models.json").exists());
    child.kill().await.unwrap();
    child.wait().await.unwrap();
}

#[tokio::test]
async fn unreadable_main_session_is_not_replaced() {
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    private_dir(root.path());
    let workspace = tempfile::tempdir().unwrap();
    let main_session = home.path().join("blocked-main");
    fs::create_dir(&main_session).unwrap();
    fs::set_permissions(&main_session, fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(&main_session, fs::Permissions::from_mode(0o000)).unwrap();
    let _restore = RestoreMode {
        path: main_session.clone(),
    };

    let mut serve = command(home.path(), workspace.path(), root.path(), &main_session);
    serve.arg("serve").arg("--workspace").arg(workspace.path());
    let output = run(serve).await;
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("未重新创建"),
        "stderr should keep the existing directory: {stderr}"
    );
    assert!(main_session.is_dir());
}
