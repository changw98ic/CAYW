//! End-to-end checks for the default local CLI: `cayw init` and a bare
//! `cayw serve`. Each test spawns the real binary with a cleared environment,
//! so only the explicit HOME/CAYW_HOME and the current directory can decide
//! where private state lands. The IPC client is the same public interface a
//! host uses; no test opens the SQLite database directly.
#![cfg(target_os = "macos")]

use cayw_ipc::Client;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    ffi::OsStr,
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, PermissionsExt, symlink},
    },
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
};

/// Generous guard for a local process to start or stop. It is not a timing
/// assertion: a healthy run finishes long before it, and a hung run fails.
const WATCHDOG: Duration = Duration::from_secs(30);

fn command(home: &Path, workspace: &Path, cayw_home: Option<&OsStr>) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cayw"));
    command
        .env_clear()
        .env("HOME", home)
        .current_dir(workspace)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(root) = cayw_home {
        command.env("CAYW_HOME", root);
    }
    command
}

fn serve(home: &Path, workspace: &Path, cayw_home: &Path, extra: &[&str]) -> Command {
    let mut command = command(home, workspace, Some(cayw_home.as_os_str()));
    command.arg("serve").args(extra);
    command
}

async fn run(mut command: Command) -> std::process::Output {
    tokio::time::timeout(WATCHDOG, command.output())
        .await
        .expect("CLI must finish within the watchdog")
        .expect("CLI should start")
}

async fn init(home: &Path, workspace: &Path, cayw_home: &Path) -> std::process::Output {
    let mut command = command(home, workspace, Some(cayw_home.as_os_str()));
    command.arg("init");
    run(command).await
}

fn first_json_line(output: &std::process::Output) -> Value {
    let text = String::from_utf8(output.stdout.clone()).expect("CLI stdout is UTF-8");
    let line = text
        .lines()
        .next()
        .expect("the CLI prints one JSON line on success");
    serde_json::from_str(line).expect("the CLI success line is JSON")
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

fn private_dir(path: &Path) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn assert_private_dir(path: &Path) {
    assert_eq!(
        mode(path),
        0o700,
        "{} must be a 0700 directory",
        path.display()
    );
}

fn assert_private_file(path: &Path) {
    assert_eq!(mode(path), 0o600, "{} must be a 0600 file", path.display());
}

/// The state directory is identified by the canonical workspace path, so two
/// spellings of the same directory must not create two databases.
fn workspace_key(workspace: &Path) -> String {
    let canonical = workspace.canonicalize().unwrap();
    format!("{:x}", Sha256::digest(canonical.as_os_str().as_bytes()))
}

#[derive(Debug, PartialEq, Eq)]
struct TreeEntry {
    kind: &'static str,
    mode: u32,
    uid: u32,
    gid: u32,
    size: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    bytes: Vec<u8>,
}

fn snapshot_tree(root: &Path) -> BTreeMap<PathBuf, TreeEntry> {
    fn visit(base: &Path, path: &Path, entries: &mut BTreeMap<PathBuf, TreeEntry>) {
        let metadata = path.symlink_metadata().unwrap();
        let kind = if metadata.file_type().is_dir() {
            "directory"
        } else if metadata.file_type().is_file() {
            "file"
        } else if metadata.file_type().is_symlink() {
            "symlink"
        } else {
            "other"
        };
        let bytes = if metadata.file_type().is_file() {
            std::fs::read(path).unwrap()
        } else if metadata.file_type().is_symlink() {
            std::fs::read_link(path)
                .unwrap()
                .as_os_str()
                .as_bytes()
                .to_vec()
        } else {
            Vec::new()
        };
        entries.insert(
            path.strip_prefix(base).unwrap().to_path_buf(),
            TreeEntry {
                kind,
                mode: metadata.mode(),
                uid: metadata.uid(),
                gid: metadata.gid(),
                size: metadata.size(),
                modified_seconds: metadata.mtime(),
                modified_nanoseconds: metadata.mtime_nsec(),
                bytes,
            },
        );
        if metadata.file_type().is_dir() {
            let mut children = std::fs::read_dir(path)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect::<Vec<_>>();
            children.sort();
            for child in children {
                visit(base, &child, entries);
            }
        }
    }

    let mut entries = BTreeMap::new();
    if root.exists() {
        visit(root, root, &mut entries);
    }
    entries
}

// Break caught: init requires a pre-existing database, writes state outside the
// private root, publishes a world-readable secret, or replaces existing files.
#[tokio::test]
async fn init_creates_private_state_and_keeps_existing_files() {
    let root = tempfile::tempdir().unwrap();
    private_dir(root.path());
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();

    let output = init(home.path(), workspace.path(), root.path()).await;
    assert!(
        output.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = first_json_line(&output);
    assert!(report["initialized"].as_bool().unwrap());
    assert!(!report["model_configured"].as_bool().unwrap());

    let canonical_workspace = workspace.path().canonicalize().unwrap();
    let state = root
        .path()
        .join("workspaces")
        .join(workspace_key(workspace.path()));
    let database = state.join("session.sqlite");
    let config_file = root.path().join("config.json");
    let token_file = root.path().join("ipc.token");

    assert_eq!(
        report["workspace"].as_str().unwrap(),
        canonical_workspace.to_str().unwrap()
    );
    assert_eq!(
        report["data_dir"].as_str().unwrap(),
        root.path().to_str().unwrap()
    );
    assert_eq!(
        report["database"].as_str().unwrap(),
        database.to_str().unwrap()
    );
    assert_eq!(
        report["config_file"].as_str().unwrap(),
        config_file.to_str().unwrap()
    );
    assert_eq!(
        report["token_file"].as_str().unwrap(),
        token_file.to_str().unwrap()
    );

    // The directory holding session data and the two secrets are owner-only.
    assert_private_dir(root.path());
    assert_private_dir(&root.path().join("workspaces"));
    assert_private_dir(&state);
    assert_private_file(&config_file);
    assert_private_file(&token_file);

    // init opens a real database; it is not an empty placeholder file.
    let database_bytes = std::fs::read(&database).unwrap();
    assert_eq!(&database_bytes[..16], b"SQLite format 3\0".as_slice());

    let config: Value = serde_json::from_slice(&std::fs::read(&config_file).unwrap()).unwrap();
    assert_eq!(config, json!({"permissions": []}));

    let first_token = std::fs::read(&token_file).unwrap();
    assert!(
        first_token.len() >= 32 && first_token.iter().all(|byte| byte.is_ascii_graphic()),
        "the generated IPC token must be an unguessable graphic secret"
    );

    // A second fresh root gets its own random token; tokens are not constants.
    let other_root = tempfile::tempdir().unwrap();
    private_dir(other_root.path());
    let other = init(home.path(), workspace.path(), other_root.path()).await;
    assert!(other.status.success());
    assert_ne!(
        first_token,
        std::fs::read(other_root.path().join("ipc.token")).unwrap()
    );

    // Repeated init keeps valid existing files byte-for-byte. Only missing
    // files are created, never a reset of configuration or the token.
    std::fs::write(&config_file, b"{\"permissions\": []}").unwrap();
    std::fs::set_permissions(&config_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    let custom_token = "a".repeat(64);
    std::fs::write(&token_file, custom_token.as_bytes()).unwrap();
    std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600)).unwrap();

    let again = init(home.path(), workspace.path(), root.path()).await;
    assert!(
        again.status.success(),
        "repeat init failed: {}",
        String::from_utf8_lossy(&again.stderr)
    );
    let config_after = std::fs::read(&config_file).unwrap();
    assert_eq!(config_after.as_slice(), b"{\"permissions\": []}".as_slice());
    let token_after = std::fs::read(&token_file).unwrap();
    assert_eq!(token_after.as_slice(), custom_token.as_bytes());
}

// Break caught: without CAYW_HOME the CLI guesses a different default root, so
// the documented per-user location is never created.
#[tokio::test]
async fn init_defaults_to_home_dot_cayw_when_cayw_home_is_absent() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();

    let mut command = command(home.path(), workspace.path(), None);
    command.arg("init");
    let output = run(command).await;
    assert!(
        output.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = first_json_line(&output);

    let data = home.path().join(".cayw");
    assert_eq!(report["data_dir"].as_str().unwrap(), data.to_str().unwrap());
    assert_private_dir(&data);
    assert_private_file(&data.join("config.json"));
    assert_private_file(&data.join("ipc.token"));
    assert!(
        data.join("workspaces")
            .join(workspace_key(workspace.path()))
            .join("session.sqlite")
            .is_file()
    );
}

#[tokio::test]
async fn doctor_is_read_only_before_and_after_initialization() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let absent_root = home.path().join("not-created-by-doctor");
    let mut absent = command(home.path(), workspace.path(), Some(absent_root.as_os_str()));
    absent.arg("doctor");
    let output = run(absent).await;
    assert!(
        output.status.success(),
        "doctor failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["schema"], "cayw.doctor.v1");
    assert_eq!(report["network_model_request"], false);
    assert!(!absent_root.exists());

    let root = tempfile::tempdir().unwrap();
    private_dir(root.path());
    assert!(
        init(home.path(), workspace.path(), root.path())
            .await
            .status
            .success()
    );
    let mut server = serve(home.path(), workspace.path(), root.path(), &["--offline"])
        .spawn()
        .unwrap();
    read_ready(&mut server).await;

    let cleanup_file = root.path().join("client-plugin-cleanup.json");
    std::fs::write(
        &cleanup_file,
        serde_json::to_vec_pretty(&json!({
            "schema":"cayw.client-plugin-cleanup.v1",
            "processes":[{
                "launch_id":"stale-launch",
                "plugin":"webui",
                "workspace_id":"local",
                "process":{"pid":u32::MAX,"started":"stale","command":"stale"}
            }]
        }))
        .unwrap(),
    )
    .unwrap();
    std::fs::set_permissions(&cleanup_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    let state_dir = root
        .path()
        .join("workspaces")
        .join(workspace_key(workspace.path()));
    for name in [
        "connection.json",
        "server.lock",
        "launcher-web.lock",
        "launcher-retired.json",
    ] {
        std::fs::write(state_dir.join(name), b"legacy fixture").unwrap();
    }

    let before = snapshot_tree(root.path());
    let mut running = command(home.path(), workspace.path(), Some(root.path().as_os_str()));
    running.arg("doctor");
    let output = run(running).await;
    assert!(
        output.status.success(),
        "doctor failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["schema"], "cayw.doctor.v1");
    assert_eq!(report["network_model_request"], false);
    let client = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == "client")
        .unwrap();
    assert_eq!(client["status"], "ok");
    assert_eq!(client["unique"], true);
    assert_eq!(
        client["client_file"],
        root.path().join("client.json").to_str().unwrap()
    );
    assert!(client.get("connection_file").is_none());
    assert_eq!(client["health"]["client"], true);
    let cleanup = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == "client_plugin_cleanup")
        .unwrap();
    assert_eq!(cleanup["status"], "warning");
    assert_eq!(cleanup["details"]["exists"], true);
    assert_eq!(cleanup["details"]["live_processes"], 0);
    let legacy = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == "legacy_client_state")
        .unwrap();
    assert_eq!(legacy["status"], "warning");
    assert_eq!(legacy["entries"].as_array().unwrap().len(), 4);
    let client_plugins = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == "client_plugins")
        .unwrap();
    assert_eq!(client_plugins["status"], "ok");
    assert_eq!(snapshot_tree(root.path()), before);

    let pid = server.id().unwrap();
    std::process::Command::new("/bin/kill")
        .arg("-INT")
        .arg(pid.to_string())
        .status()
        .unwrap();
    assert!(
        tokio::time::timeout(WATCHDOG, server.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
}

// Break caught: the Client publishes discovery and readiness before the Client
// plugin supervisor has accepted its persistent cleanup state.
#[tokio::test]
async fn serve_never_reports_readiness_when_client_plugin_initialization_fails() {
    let root = tempfile::tempdir().unwrap();
    private_dir(root.path());
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    assert!(
        init(home.path(), workspace.path(), root.path())
            .await
            .status
            .success()
    );

    let cleanup_file = root.path().join("client-plugin-cleanup.json");
    std::fs::write(
        &cleanup_file,
        serde_json::to_vec_pretty(&json!({
            "schema":"unsupported-client-plugin-cleanup-schema",
            "processes":[]
        }))
        .unwrap(),
    )
    .unwrap();
    std::fs::set_permissions(&cleanup_file, std::fs::Permissions::from_mode(0o600)).unwrap();

    let output = run(serve(
        home.path(),
        workspace.path(),
        root.path(),
        &["--offline"],
    ))
    .await;
    assert!(!output.status.success());
    assert!(
        output.stdout.is_empty(),
        "failed initialization must not publish readiness: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("unsupported schema"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!root.path().join("client.json").exists());
}

// Break caught: a bare serve still demands --db/--token-env, silently falls back
// to offline, admits work without a model, or reports an incomplete ready line.
#[tokio::test]
async fn serve_auto_initializes_and_rejects_execution_without_a_model() {
    let root = tempfile::tempdir().unwrap();
    private_dir(root.path());
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();

    let mut child = serve(home.path(), workspace.path(), root.path(), &[])
        .spawn()
        .unwrap();
    let ready = read_ready(&mut child).await;

    // The server starts without a model and says so; it does not pretend to be
    // the offline fixture.
    assert!(!ready["model_configured"].as_bool().unwrap());
    assert_eq!(ready["provider"], "unconfigured");
    assert_eq!(ready["model"], "unconfigured");

    let canonical_workspace = workspace.path().canonicalize().unwrap();
    let state = root
        .path()
        .join("workspaces")
        .join(workspace_key(workspace.path()));
    let database = state.join("session.sqlite");
    let config_file = root.path().join("config.json");
    let token_file = root.path().join("ipc.token");
    let client_file = root.path().join("client.json");

    assert_eq!(
        ready["workspace"].as_str().unwrap(),
        canonical_workspace.to_str().unwrap()
    );
    assert_eq!(
        ready["database"].as_str().unwrap(),
        database.to_str().unwrap()
    );
    assert_eq!(
        ready["config_file"].as_str().unwrap(),
        config_file.to_str().unwrap()
    );
    assert_eq!(
        ready["token_file"].as_str().unwrap(),
        token_file.to_str().unwrap()
    );
    assert_eq!(
        ready["client_file"].as_str().unwrap(),
        client_file.to_str().unwrap()
    );
    assert!(ready.get("connection_file").is_none());
    assert!(database.is_file());
    assert_private_dir(root.path());
    assert_private_dir(&state);
    assert_private_file(&config_file);
    assert_private_file(&token_file);

    // client.json is the one global Client discovery record. It identifies the
    // live loopback endpoint without assigning connection ownership to a workspace.
    let connection: Value = serde_json::from_slice(&std::fs::read(&client_file).unwrap()).unwrap();
    assert_eq!(connection["schema"], "cayw.client-endpoint.v1");
    assert_eq!(connection["protocol_version"], cayw_ipc::PROTOCOL_VERSION);
    assert_eq!(connection["address"], ready["listening"]);
    assert!(
        connection["generation"]
            .as_str()
            .is_some_and(|value| !value.is_empty())
    );
    assert!(connection["pid"].as_u64().is_some_and(|value| value > 0));
    assert!(connection.get("workspace").is_none());
    assert!(connection.get("token_file").is_none());

    let client = connect(&ready).await;
    let client_health = client.request("client.health", json!({})).await.unwrap();
    assert_eq!(client_health["client"], true);
    assert_eq!(
        client_health["client_plugins"]["schema"],
        "cayw.client-plugins.v1"
    );
    assert_eq!(client_health["client_plugins"]["entries"], json!([]));
    let client_plugins = client
        .request("client.plugins.list", json!({}))
        .await
        .unwrap();
    assert_eq!(client_plugins["schema"], "cayw.client-plugins.v1");
    assert_eq!(client_plugins["entries"], json!([]));

    // Queries and session creation need no model and must stay available.
    let session = client
        .request("session.create", json!({"label": "unconfigured"}))
        .await
        .unwrap();
    let id = session["id"].as_str().unwrap().to_owned();
    let sessions = client.request("session.list", json!({})).await.unwrap();
    assert!(
        sessions
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["id"].as_str() == Some(id.as_str()))
    );
    let fetched = client
        .request("session.get", json!({"session_id": id}))
        .await
        .unwrap();
    assert_eq!(fetched["id"].as_str(), Some(id.as_str()));
    let health = client.request("runtime.health", json!({})).await.unwrap();
    assert_eq!(health["provider"], "unconfigured");
    assert_eq!(health["model"], "unconfigured");

    // Every execution entry point is refused before admission with the same
    // terminal code, including the resource and scheduled enqueue paths.
    let cases: [(&str, Value); 6] = [
        (
            "session.prompt",
            json!({"session_id": id.as_str(), "text": "must not run"}),
        ),
        (
            "task.enqueue",
            json!({"session_id": id.as_str(), "text": "must not run"}),
        ),
        (
            "task.enqueue_with_intent",
            json!({"session_id": id.as_str(), "text": "must not run", "intent": {"kind": "general"}}),
        ),
        (
            "task.enqueue_scheduled",
            json!({"session_id": id.as_str(), "text": "must not run", "intent": {"kind": "general"}, "priority": "normal"}),
        ),
        (
            "task.enqueue_with_resources",
            json!({"session_id": id.as_str(), "text": "must not run", "intent": {"kind": "general"}, "priority": "normal", "resources": {}}),
        ),
        ("task.resume", json!({"task_id": "no-such-task"})),
    ];
    for (method, params) in cases {
        let error = client
            .request(method, params)
            .await
            .expect_err("an unconfigured model must refuse execution");
        assert_eq!(error.code, "model_not_configured", "{method}");
        assert!(!error.outcome_unknown, "{method} fails before dispatch");
    }

    // A refused call must leave no task ledger and no model request record.
    let tasks = client
        .request("session.tasks", json!({"session_id": id}))
        .await
        .unwrap();
    assert_eq!(tasks, json!([]));
    let scheduler = client
        .request("runtime.scheduler", json!({"limit": 100}))
        .await
        .unwrap();
    assert_eq!(scheduler["tasks"], json!([]));
    let audit = client
        .request("session.audit", json!({"session_id": id, "limit": 100}))
        .await
        .unwrap();
    assert!(
        audit.as_array().unwrap().iter().all(|event| {
            let kind = event["kind"].as_str().unwrap_or_default();
            !kind.starts_with("run_") && !kind.contains("model")
        }),
        "refused execution must not write task or model ledger events"
    );

    client.close().await;
    child.kill().await.unwrap();
    child.wait().await.unwrap();
}

// Break caught: a second local Client starts for another workspace, or routing
// lets one workspace read the other's SQLite-backed Session state.
#[tokio::test]
async fn one_client_routes_separate_in_process_workspaces() {
    let root = tempfile::tempdir().unwrap();
    private_dir(root.path());
    let home = tempfile::tempdir().unwrap();
    let first_workspace = tempfile::tempdir().unwrap();
    let second_workspace = tempfile::tempdir().unwrap();

    let second_init = init(home.path(), second_workspace.path(), root.path()).await;
    assert!(second_init.status.success());
    let second_report = first_json_line(&second_init);

    let mut first = serve(home.path(), first_workspace.path(), root.path(), &[])
        .spawn()
        .unwrap();
    let first_ready = read_ready(&mut first).await;

    let rejected = run(serve(
        home.path(),
        second_workspace.path(),
        root.path(),
        &[],
    ))
    .await;
    assert!(!rejected.status.success());
    assert!(
        String::from_utf8_lossy(&rejected.stderr).contains("local Client is already running"),
        "stderr: {}",
        String::from_utf8_lossy(&rejected.stderr)
    );
    assert!(rejected.stdout.is_empty());

    let client = connect(&first_ready).await;
    let catalog = client
        .request("client.workspaces.list", json!({}))
        .await
        .unwrap();
    let second_path = second_workspace.path().canonicalize().unwrap();
    let second_id = catalog["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|workspace| workspace["path"].as_str() == second_path.to_str())
        .and_then(|workspace| workspace["id"].as_str())
        .unwrap()
        .to_owned();
    assert!(second_id.starts_with("workspace-"));

    let opened = client
        .request("client.workspaces.open", json!({"id":second_id}))
        .await
        .unwrap();
    assert_eq!(opened["state"], "open");
    assert_eq!(opened["reused"], false);
    assert!(opened.get("ipc_addr").is_none());
    let reopened = client
        .request("client.workspaces.open", json!({"id":second_id}))
        .await
        .unwrap();
    assert_eq!(reopened["reused"], true);

    let secondary = client
        .request(
            "client.workspace.call",
            json!({
                "workspace_id":second_id,
                "rpc":{"method":"session.create","params":{"label":"secondary"}}
            }),
        )
        .await
        .unwrap();
    let secondary_id = secondary["id"].as_str().unwrap();
    let secondary_sessions = client
        .request(
            "client.workspace.call",
            json!({
                "workspace_id":second_id,
                "rpc":{"method":"session.list","params":{}}
            }),
        )
        .await
        .unwrap();
    assert!(
        secondary_sessions
            .as_array()
            .unwrap()
            .iter()
            .any(|session| session["id"].as_str() == Some(secondary_id))
    );
    assert!(
        client
            .request("session.list", json!({}))
            .await
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .all(|session| session["id"].as_str() != Some(secondary_id))
    );

    assert_ne!(first_ready["database"], second_report["database"]);
    assert!(Path::new(first_ready["database"].as_str().unwrap()).is_file());
    assert!(Path::new(second_report["database"].as_str().unwrap()).is_file());
    assert_eq!(
        first_ready["client_file"].as_str(),
        root.path().join("client.json").to_str()
    );
    assert!(root.path().join("client.json").is_file());
    for workspace in [&first_workspace, &second_workspace] {
        let state = root
            .path()
            .join("workspaces")
            .join(workspace_key(workspace.path()));
        assert!(!state.join("connection.json").exists());
        assert!(!state.join("server.lock").exists());
        for ui in ["none", "tui", "web"] {
            assert!(!state.join(format!("launcher-{ui}.lock")).exists());
            assert!(!state.join(format!("launcher-{ui}.json")).exists());
        }
    }

    client.close().await;
    first.kill().await.unwrap();
    first.wait().await.unwrap();
}

#[tokio::test]
async fn legacy_local_model_moves_once_to_the_workspace_profile_file() {
    let root = tempfile::tempdir().unwrap();
    private_dir(root.path());
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    assert!(
        init(home.path(), workspace.path(), root.path())
            .await
            .status
            .success()
    );
    let key = "synthetic-migration-key-never-print";
    let legacy = json!({
        "permissions":[],
        "model":{
            "service":"openai",
            "api":"chat_completions",
            "id":"legacy-local-model",
            "base_url":"http://127.0.0.1:9/v1",
            "api_version":"v1",
            "max_output_tokens":64,
            "api_key":key
        }
    });
    std::fs::write(
        root.path().join("config.json"),
        serde_json::to_vec_pretty(&legacy).unwrap(),
    )
    .unwrap();
    std::fs::set_permissions(
        root.path().join("config.json"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();

    let mut server = serve(home.path(), workspace.path(), root.path(), &[])
        .spawn()
        .unwrap();
    let ready = read_ready(&mut server).await;
    assert_eq!(ready["provider"], "openai");
    assert_eq!(ready["model"], "legacy-local-model");
    assert_eq!(ready["model_configured"], true);
    assert!(!ready.to_string().contains(key));

    let config: Value =
        serde_json::from_slice(&std::fs::read(root.path().join("config.json")).unwrap()).unwrap();
    assert_eq!(config, json!({"permissions":[]}));
    let models_path = root
        .path()
        .join("workspaces")
        .join(workspace_key(workspace.path()))
        .join("models.json");
    let models: Value = serde_json::from_slice(&std::fs::read(&models_path).unwrap()).unwrap();
    assert_eq!(models["selected"], "configured");
    assert_eq!(models["models"]["configured"]["id"], "legacy-local-model");
    assert_eq!(models["models"]["configured"]["api_key"], key);
    assert_private_file(&models_path);

    let client = connect(&ready).await;
    let public = client
        .request("settings.models.list", json!({}))
        .await
        .unwrap();
    assert_eq!(public["selected"], "configured");
    assert!(!public.to_string().contains(key));
    client.close().await;
    let pid = server.id().unwrap();
    std::process::Command::new("/bin/kill")
        .arg("-INT")
        .arg(pid.to_string())
        .status()
        .unwrap();
    assert!(
        tokio::time::timeout(WATCHDOG, server.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );

    let mut restarted = serve(home.path(), workspace.path(), root.path(), &[])
        .spawn()
        .unwrap();
    let ready = read_ready(&mut restarted).await;
    assert_eq!(ready["model"], "legacy-local-model");
    assert!(
        serde_json::from_slice::<Value>(&std::fs::read(root.path().join("config.json")).unwrap())
            .unwrap()
            .get("model")
            .is_none()
    );
    restarted.kill().await.unwrap();
    restarted.wait().await.unwrap();
}

#[tokio::test]
async fn model_cli_routes_to_the_requested_nonprimary_workspace() {
    let root = tempfile::tempdir().unwrap();
    private_dir(root.path());
    let home = tempfile::tempdir().unwrap();
    let first_workspace = tempfile::tempdir().unwrap();
    let second_workspace = tempfile::tempdir().unwrap();
    assert!(
        init(home.path(), second_workspace.path(), root.path())
            .await
            .status
            .success()
    );
    let mut server = serve(home.path(), first_workspace.path(), root.path(), &[])
        .spawn()
        .unwrap();
    let ready = read_ready(&mut server).await;
    let model_path = root.path().join("second-model.json");
    let key = "synthetic-secondary-key-never-print";
    std::fs::write(
        &model_path,
        serde_json::to_vec(&json!({
            "service":"openai",
            "api":"chat_completions",
            "id":"secondary-model",
            "base_url":"http://127.0.0.1:9/v1",
            "api_version":"v1",
            "max_output_tokens":64,
            "api_key":key
        }))
        .unwrap(),
    )
    .unwrap();

    let mut add = command(
        home.path(),
        first_workspace.path(),
        Some(root.path().as_os_str()),
    );
    add.args(["models", "add", "secondary", "--file"])
        .arg(&model_path)
        .arg("--workspace")
        .arg(second_workspace.path());
    let added = run(add).await;
    assert!(
        added.status.success(),
        "add failed: {}",
        String::from_utf8_lossy(&added.stderr)
    );
    assert!(!String::from_utf8_lossy(&added.stdout).contains(key));

    let mut select = command(
        home.path(),
        first_workspace.path(),
        Some(root.path().as_os_str()),
    );
    select
        .args(["models", "use", "secondary", "--workspace"])
        .arg(second_workspace.path());
    let selected = run(select).await;
    assert!(
        selected.status.success(),
        "use failed: {}",
        String::from_utf8_lossy(&selected.stderr)
    );
    assert!(!String::from_utf8_lossy(&selected.stdout).contains(key));

    let client = connect(&ready).await;
    let primary_health = client.request("runtime.health", json!({})).await.unwrap();
    assert_eq!(primary_health["provider"], "unconfigured");
    let catalog = client
        .request("client.workspaces.list", json!({}))
        .await
        .unwrap();
    let second_path = second_workspace.path().canonicalize().unwrap();
    let second_id = catalog["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["path"].as_str() == second_path.to_str())
        .and_then(|entry| entry["id"].as_str())
        .unwrap();
    let secondary_health = client
        .request(
            "client.workspace.call",
            json!({
                "workspace_id":second_id,
                "rpc":{"method":"runtime.health","params":{}}
            }),
        )
        .await
        .unwrap();
    assert_eq!(secondary_health["provider"], "openai");
    assert_eq!(secondary_health["model"], "secondary-model");
    assert!(
        !root
            .path()
            .join("workspaces")
            .join(workspace_key(first_workspace.path()))
            .join("models.json")
            .exists()
    );
    let second_models = root
        .path()
        .join("workspaces")
        .join(workspace_key(second_workspace.path()))
        .join("models.json");
    assert!(second_models.is_file());
    assert!(String::from_utf8_lossy(&std::fs::read(second_models).unwrap()).contains(key));

    client.close().await;
    server.kill().await.unwrap();
    server.wait().await.unwrap();
}

#[tokio::test]
async fn start_owns_one_client_without_workspace_launcher_state() {
    let root = tempfile::tempdir().unwrap();
    private_dir(root.path());
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let mut start = command(home.path(), workspace.path(), Some(root.path().as_os_str()));
    start.args(["start", "--ui", "none", "--offline"]);
    let mut start = start.spawn().unwrap();
    let mut lines = BufReader::new(start.stdout.take().unwrap()).lines();
    let line = tokio::time::timeout(WATCHDOG, lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let ready: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(ready["ui"], "none");
    assert_eq!(ready["owned_client"], true);
    assert_eq!(
        ready["workspace"].as_str(),
        workspace.path().canonicalize().unwrap().to_str()
    );

    let address = ready["client"].as_str().unwrap().parse().unwrap();
    let token = std::fs::read_to_string(root.path().join("ipc.token")).unwrap();
    let client = Client::connect(address, token.trim()).await.unwrap();
    let health = client.request("client.health", json!({})).await.unwrap();
    assert_eq!(health["client"], true);
    assert_eq!(health["client_plugins"]["entries"], json!([]));

    let missing_workspace = format!("workspace-{}", "0".repeat(64));
    let rejected = client
        .request(
            "client.plugins.start",
            json!({"plugin":"tui","workspace_id":missing_workspace}),
        )
        .await
        .unwrap_err();
    assert_eq!(rejected.code, "workspace_unavailable");
    assert!(rejected.message.contains("workspace is not registered"));
    let plugins = client
        .request("client.plugins.list", json!({}))
        .await
        .unwrap();
    assert_eq!(plugins["entries"], json!([]));
    client.close().await;

    let state = root
        .path()
        .join("workspaces")
        .join(workspace_key(workspace.path()));
    for ui in ["none", "tui", "web"] {
        assert!(!state.join(format!("launcher-{ui}.lock")).exists());
        assert!(!state.join(format!("launcher-{ui}.json")).exists());
    }

    std::process::Command::new("/bin/kill")
        .arg("-INT")
        .arg(start.id().unwrap().to_string())
        .status()
        .unwrap();
    assert!(
        tokio::time::timeout(WATCHDOG, start.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    assert!(!root.path().join("client.json").exists());
}

#[cfg(feature = "plugins")]
#[tokio::test]
async fn client_owns_global_plugin_inventory_and_workspace_activations() {
    let root = tempfile::tempdir().unwrap();
    private_dir(root.path());
    let home = tempfile::tempdir().unwrap();
    let first_workspace = tempfile::tempdir().unwrap();
    let second_workspace = tempfile::tempdir().unwrap();
    assert!(
        init(home.path(), second_workspace.path(), root.path())
            .await
            .status
            .success()
    );

    let mut server = serve(
        home.path(),
        first_workspace.path(),
        root.path(),
        &["--offline"],
    )
    .spawn()
    .unwrap();
    let ready = read_ready(&mut server).await;
    let client = connect(&ready).await;
    let catalog = client
        .request("client.workspaces.list", json!({}))
        .await
        .unwrap();
    let second_path = second_workspace.path().canonicalize().unwrap();
    let second_id = catalog["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|workspace| workspace["path"].as_str() == second_path.to_str())
        .and_then(|workspace| workspace["id"].as_str())
        .unwrap()
        .to_owned();
    client
        .request("client.workspaces.open", json!({"id":second_id}))
        .await
        .unwrap();

    let plugin_executable = root.path().join("plugin-fixture");
    std::fs::copy(
        env!("CARGO_BIN_EXE_cayw-runtime-plugin-fixture"),
        &plugin_executable,
    )
    .unwrap();
    std::fs::set_permissions(&plugin_executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    let descriptor = json!({
        "executable":plugin_executable,
        "argv":["normal","/tmp/cayw-unused-plugin-gate"],
        "manifest":{
            "name":"process",
            "version":"1.0.0",
            "protocol_version":1,
            "role":"workspace_architecture",
            "kind":"external",
            "capabilities":[{
                "name":"process.echo",
                "description":"Offline process lifecycle fixture",
                "parameters":{"type":"object","additionalProperties":false}
            }],
            "permissions":{}
        }
    });
    let mut client_descriptor = descriptor.clone();
    client_descriptor["manifest"]["role"] = json!("client");
    let role_error = client
        .request(
            "settings.plugins.install",
            json!({"name":null,"descriptor":client_descriptor}),
        )
        .await
        .unwrap_err();
    assert_eq!(role_error.code, "settings_error");
    assert!(role_error.message.contains("Client plugins"));
    assert!(!root.path().join("plugins.json").exists());

    let installed = client
        .request(
            "settings.plugins.install",
            json!({"name":null,"descriptor":descriptor}),
        )
        .await
        .unwrap();
    assert_eq!(installed["entries"][0]["enabled"], false);
    assert_eq!(installed["entries"][0]["available"], false);

    let primary_enabled = client
        .request(
            "settings.plugins.enable",
            json!({"name":"process","approve_process":true}),
        )
        .await
        .unwrap();
    assert_eq!(primary_enabled["entries"][0]["enabled"], true);
    assert_eq!(primary_enabled["entries"][0]["available"], true);
    let secondary_before = client
        .request(
            "client.workspace.call",
            json!({
                "workspace_id":second_id,
                "rpc":{"method":"settings.plugins.list","params":{}}
            }),
        )
        .await
        .unwrap();
    assert_eq!(secondary_before["entries"][0]["enabled"], false);
    assert_eq!(secondary_before["entries"][0]["available"], false);

    let secondary_enabled = client
        .request(
            "client.workspace.call",
            json!({
                "workspace_id":second_id,
                "rpc":{
                    "method":"settings.plugins.enable",
                    "params":{"name":"process","approve_process":true}
                }
            }),
        )
        .await
        .unwrap();
    assert_eq!(secondary_enabled["entries"][0]["enabled"], true);
    assert_eq!(secondary_enabled["entries"][0]["available"], true);

    let inventory: Value =
        serde_json::from_slice(&std::fs::read(root.path().join("plugins.json")).unwrap()).unwrap();
    assert_eq!(inventory["schema"], "cayw.plugin-inventory.v1");
    assert!(inventory["entries"]["process"].get("enabled").is_none());
    assert!(inventory["entries"]["process"].get("last_error").is_none());
    for workspace in [&first_workspace, &second_workspace] {
        let activation = root
            .path()
            .join("workspaces")
            .join(workspace_key(workspace.path()))
            .join("architecture-plugins.json");
        let state: Value = serde_json::from_slice(&std::fs::read(&activation).unwrap()).unwrap();
        assert_eq!(state["schema"], "cayw.workspace-architecture-plugins.v2");
        assert_eq!(state["entries"]["process"]["enabled"], true);
        assert_private_file(&activation);
    }

    client.close().await;
    let pid = server.id().unwrap();
    std::process::Command::new("/bin/kill")
        .arg("-INT")
        .arg(pid.to_string())
        .status()
        .unwrap();
    assert!(
        tokio::time::timeout(WATCHDOG, server.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    server = serve(
        home.path(),
        first_workspace.path(),
        root.path(),
        &["--offline"],
    )
    .spawn()
    .unwrap();
    let ready = read_ready(&mut server).await;
    let client = connect(&ready).await;
    let restored_primary = client
        .request("settings.plugins.list", json!({}))
        .await
        .unwrap();
    assert_eq!(restored_primary["entries"][0]["available"], true);
    client
        .request("client.workspaces.open", json!({"id":second_id}))
        .await
        .unwrap();
    let restored_secondary = client
        .request(
            "client.workspace.call",
            json!({
                "workspace_id":second_id,
                "rpc":{"method":"settings.plugins.list","params":{}}
            }),
        )
        .await
        .unwrap();
    assert_eq!(restored_secondary["entries"][0]["available"], true);

    client.close().await;
    let pid = server.id().unwrap();
    std::process::Command::new("/bin/kill")
        .arg("-INT")
        .arg(pid.to_string())
        .status()
        .unwrap();
    assert!(
        tokio::time::timeout(WATCHDOG, server.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    let mut changed = std::fs::read(&plugin_executable).unwrap();
    changed.push(0);
    std::fs::write(&plugin_executable, changed).unwrap();

    server = serve(
        home.path(),
        first_workspace.path(),
        root.path(),
        &["--offline"],
    )
    .spawn()
    .unwrap();
    let ready = read_ready(&mut server).await;
    let client = connect(&ready).await;
    let failed_primary = client
        .request("settings.plugins.list", json!({}))
        .await
        .unwrap();
    assert_eq!(failed_primary["entries"][0]["enabled"], true);
    assert_eq!(failed_primary["entries"][0]["available"], false);
    assert!(
        failed_primary["entries"][0]["last_error"]["message"]
            .as_str()
            .unwrap()
            .contains("executable changed")
            && failed_primary["entries"][0]["last_error"]["outcome_unknown"] == false
    );
    client
        .request("client.workspaces.open", json!({"id":second_id}))
        .await
        .unwrap();
    let failed_secondary = client
        .request(
            "client.workspace.call",
            json!({
                "workspace_id":second_id,
                "rpc":{"method":"settings.plugins.list","params":{}}
            }),
        )
        .await
        .unwrap();
    assert_eq!(failed_secondary["entries"][0]["enabled"], true);
    assert_eq!(failed_secondary["entries"][0]["available"], false);
    for workspace in [&first_workspace, &second_workspace] {
        let activation = root
            .path()
            .join("workspaces")
            .join(workspace_key(workspace.path()))
            .join("architecture-plugins.json");
        let state: Value = serde_json::from_slice(&std::fs::read(&activation).unwrap()).unwrap();
        assert_eq!(state["entries"]["process"]["enabled"], true);
        assert!(
            state["entries"]["process"]["last_error"]["message"]
                .as_str()
                .unwrap()
                .contains("executable changed")
                && state["entries"]["process"]["last_error"]["outcome_unknown"] == false
        );
    }

    client
        .request("settings.plugins.disable", json!({"name":"process"}))
        .await
        .unwrap();
    let remove_error = client
        .request("settings.plugins.remove", json!({"name":"process"}))
        .await
        .unwrap_err();
    assert!(remove_error.message.contains(second_path.to_str().unwrap()));
    let secondary_disabled = client
        .request(
            "client.workspace.call",
            json!({
                "workspace_id":second_id,
                "rpc":{
                    "method":"settings.plugins.disable",
                    "params":{"name":"process"}
                }
            }),
        )
        .await
        .unwrap();
    assert_eq!(secondary_disabled["entries"][0]["enabled"], false);
    let removed = client
        .request("settings.plugins.remove", json!({"name":"process"}))
        .await
        .unwrap();
    assert_eq!(removed["entries"], json!([]));
    let bind_error = client
        .request(
            "settings.plugins.bind",
            json!({"name":"process","session_id":"removed"}),
        )
        .await
        .unwrap_err();
    assert_eq!(bind_error.code, "settings_error");

    client.close().await;
    let pid = server.id().unwrap();
    std::process::Command::new("/bin/kill")
        .arg("-INT")
        .arg(pid.to_string())
        .status()
        .unwrap();
    assert!(
        tokio::time::timeout(WATCHDOG, server.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
}

#[tokio::test]
async fn explicit_token_keeps_default_database_lock_and_reports_its_actual_source() {
    let root = tempfile::tempdir().unwrap();
    private_dir(root.path());
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let token = "explicit-local-test-token-for-one-host";
    let mut command = serve(
        home.path(),
        workspace.path(),
        root.path(),
        &["--token-env", "LOCAL_TEST_TOKEN"],
    );
    command.env("LOCAL_TEST_TOKEN", token);
    let mut first = command.spawn().unwrap();
    let ready = read_ready(&mut first).await;
    assert!(ready["token_file"].is_null());
    assert!(ready["client_file"].is_null());
    assert_ne!(
        std::fs::read_to_string(root.path().join("ipc.token")).unwrap(),
        token
    );
    let client = Client::connect(ready["listening"].as_str().unwrap().parse().unwrap(), token)
        .await
        .unwrap();
    let session = client.request("session.create", json!({})).await.unwrap();
    client.close().await;
    let rejected = run(serve(home.path(), workspace.path(), root.path(), &[])).await;
    assert!(!rejected.status.success());
    assert!(rejected.stdout.is_empty());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("local Client is already running"));
    first.kill().await.unwrap();
    first.wait().await.unwrap();

    let mut second = serve(home.path(), workspace.path(), root.path(), &[])
        .spawn()
        .unwrap();
    let ready = read_ready(&mut second).await;
    let client = connect(&ready).await;
    assert_eq!(
        client
            .request("session.get", json!({"session_id": session["id"]}))
            .await
            .unwrap(),
        session
    );
    let mut duplicate = serve(
        home.path(),
        workspace.path(),
        root.path(),
        &["--token-env", "LOCAL_TEST_TOKEN"],
    );
    duplicate.env("LOCAL_TEST_TOKEN", token);
    let rejected = run(duplicate).await;
    assert!(!rejected.status.success());
    assert!(rejected.stdout.is_empty());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("local Client is already running"));
    client.close().await;
    second.kill().await.unwrap();
    second.wait().await.unwrap();
}

#[tokio::test]
async fn concurrent_first_initializations_share_complete_config_and_token() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path().join("new-state");
    let workspaces = (0..8)
        .map(|_| tempfile::tempdir().unwrap())
        .collect::<Vec<_>>();
    let mut children = Vec::new();
    for workspace in &workspaces {
        let mut command = command(home.path(), workspace.path(), Some(root.as_os_str()));
        command.arg("init");
        children.push(command.spawn().unwrap());
    }
    let mut reports = Vec::new();
    for child in children {
        let output = tokio::time::timeout(WATCHDOG, child.wait_with_output())
            .await
            .unwrap()
            .unwrap();
        assert!(
            output.status.success(),
            "parallel init failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        reports.push(first_json_line(&output));
    }
    let first = &reports[0];
    for report in &reports {
        assert_eq!(report["config_file"], first["config_file"]);
        assert_eq!(report["token_file"], first["token_file"]);
        assert!(Path::new(report["database"].as_str().unwrap()).is_file());
    }
    assert_eq!(
        serde_json::from_slice::<Value>(&std::fs::read(root.join("config.json")).unwrap()).unwrap(),
        json!({"permissions": []})
    );
    let token = std::fs::read_to_string(root.join("ipc.token")).unwrap();
    assert_eq!(token.len(), 64);
    assert!(token.bytes().all(|byte| byte.is_ascii_graphic()));
    assert_private_file(&root.join("init.lock"));
    assert_private_file(&root.join("config.json"));
    assert_private_file(&root.join("ipc.token"));
    let again = init(home.path(), workspaces[0].path(), &root).await;
    assert!(again.status.success());
    assert_eq!(
        std::fs::read_to_string(root.join("ipc.token")).unwrap(),
        token
    );
}

// Break caught: the documented offline demo cannot start from a bare workspace,
// its two file calls are not authorized, or SIGINT leaves the service running.
#[tokio::test]
async fn offline_demo_writes_the_authorized_file_and_stops_on_sigint() {
    let root = tempfile::tempdir().unwrap();
    private_dir(root.path());
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();

    let mut child = serve(
        home.path(),
        workspace.path(),
        root.path(),
        &["--offline", "--demo-authorized"],
    )
    .spawn()
    .unwrap();
    let ready = read_ready(&mut child).await;
    assert!(ready["offline_fixture"].as_bool().unwrap());
    assert!(ready["demo_authorized"].as_bool().unwrap());
    // Offline is an explicit fixture, not a configured real model.
    assert!(!ready["model_configured"].as_bool().unwrap());
    assert_eq!(ready["provider"], "fixture");
    assert_eq!(ready["model"], "offline-runtime");

    let client = connect(&ready).await;
    let session = client
        .request("session.create", json!({"label": "offline demo"}))
        .await
        .unwrap();
    let id = session["id"].as_str().unwrap().to_owned();
    tokio::time::timeout(
        WATCHDOG,
        client.request(
            "session.prompt",
            json!({"session_id": id.as_str(), "text": "run the offline demo"}),
        ),
    )
    .await
    .expect("the offline prompt must finish within the watchdog")
    .expect("the two exact demo permissions allow the file round trip");

    // The synthetic model wrote through the real Host into this workspace.
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("cayw-demo.txt")).unwrap(),
        "headless runtime demo"
    );

    client.close().await;

    // SIGINT is the documented stop signal and must end the process cleanly.
    let pid = child.id().expect("serve child has a pid");
    std::process::Command::new("/bin/kill")
        .arg("-INT")
        .arg(pid.to_string())
        .status()
        .unwrap();
    let status = tokio::time::timeout(WATCHDOG, child.wait())
        .await
        .expect("serve must exit after SIGINT")
        .unwrap();
    assert!(
        status.success(),
        "SIGINT must be a clean shutdown: {status:?}"
    );
}

// Local Client authority state must never be part of a model workspace, even
// when an exact tool read of a seemingly ordinary file would be authorized.
#[tokio::test]
async fn home_workspace_is_rejected_before_default_state_is_created() {
    let home = tempfile::tempdir().unwrap();
    let before = snapshot_tree(home.path());
    for action in ["init", "conf", "serve", "start"] {
        let mut command = command(home.path(), home.path(), None);
        command.arg(action).stdin(Stdio::null());
        let output = run(command).await;
        assert!(!output.status.success(), "{action} must reject HOME as workspace");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("must not overlap"),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(snapshot_tree(home.path()), before);
    }
}

#[tokio::test]
async fn custom_state_overlap_is_rejected_without_creating_or_changing_files() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let existing = workspace.path().join("private-state");
    std::fs::create_dir(&existing).unwrap();
    private_dir(&existing);
    // No real credential is used. Even already-existing state must stay intact.
    std::fs::write(existing.join("ipc.token"), b"SYNTHETIC_STATE_SENTINEL").unwrap();
    let before = snapshot_tree(workspace.path());
    for state in [
        workspace.path().to_path_buf(),
        existing,
        workspace.path().join("not-created/nested-state"),
        workspace.path().join("not-created/../other-state"),
    ] {
        let output = init(home.path(), workspace.path(), &state).await;
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("must not overlap"));
        assert_eq!(snapshot_tree(workspace.path()), before);
    }

    // A workspace inside private state is also invalid, including when it
    // directly names a workspace's models/database directory.
    let state = tempfile::tempdir().unwrap();
    let nested_workspace = state.path().join("project");
    std::fs::create_dir(&nested_workspace).unwrap();
    let before = snapshot_tree(state.path());
    let output = init(home.path(), &nested_workspace, state.path()).await;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("must not overlap"));
    assert_eq!(snapshot_tree(state.path()), before);
}

#[tokio::test]
async fn canonical_aliases_cannot_hide_state_overlap() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let alias = home.path().join("project-alias");
    symlink(workspace.path(), &alias).unwrap();
    let before = snapshot_tree(workspace.path());
    for (project, state) in [
        (workspace.path().to_path_buf(), alias.join("new-state")),
        (alias.clone(), workspace.path().join("new-state")),
        (
            workspace.path().to_path_buf(),
            alias.join("missing/../new-state"),
        ),
    ] {
        let output = init(home.path(), &project, &state).await;
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("must not overlap"));
        assert_eq!(snapshot_tree(workspace.path()), before);
    }
}

#[tokio::test]
async fn separate_default_state_and_canonical_sibling_prefix_are_allowed() {
    let home = tempfile::tempdir().unwrap();
    let workspace = home.path().join("project");
    std::fs::create_dir(&workspace).unwrap();
    let mut default = command(home.path(), &workspace, None);
    default.arg("init");
    let output = run(default).await;
    assert!(output.status.success(), "{:?}", output.stderr);
    assert!(home.path().join(".cayw/ipc.token").is_file());

    let alias = home.path().join("home-alias");
    symlink(home.path(), &alias).unwrap();
    // A path string prefix is not a directory ancestor. Resolve the symlinked
    // parent without rejecting an actually separate custom state directory.
    let state = alias.join("project-state");
    let output = init(home.path(), &workspace, &state).await;
    assert!(output.status.success(), "{:?}", output.stderr);
    assert!(home.path().join("project-state/ipc.token").is_file());
}

#[tokio::test]
async fn legacy_overlapping_registration_cannot_open_a_workspace_runtime() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let state = home.path().join("private-state");
    assert!(init(home.path(), workspace.path(), &state).await.status.success());
    // Simulate the catalog left by an older version that allowed HOME as a
    // workspace. Opening it must fail before creating a database or Host.
    let legacy = state.join("workspaces").join(workspace_key(home.path()));
    std::fs::create_dir(&legacy).unwrap();
    private_dir(&legacy);
    let registration = legacy.join("workspace.json");
    std::fs::write(
        &registration,
        serde_json::to_vec(&json!({"workspace":home.path().canonicalize().unwrap()})).unwrap(),
    )
    .unwrap();
    std::fs::set_permissions(&registration, std::fs::Permissions::from_mode(0o600)).unwrap();
    let before = snapshot_tree(&legacy);
    let mut server = serve(home.path(), workspace.path(), &state, &[]).spawn().unwrap();
    let ready = read_ready(&mut server).await;
    let client = connect(&ready).await;
    let error = client
        .request(
            "client.workspaces.open",
            json!({"id":format!("workspace-{}", workspace_key(home.path()))}),
        )
        .await
        .expect_err("overlapping catalog entry must not create a Runtime");
    assert!(error.message.contains("must not overlap"), "{error:?}");
    assert_eq!(snapshot_tree(&legacy), before);
    client.request("session.create", json!({})).await.unwrap();
    client.close().await;
    server.kill().await.unwrap();
    server.wait().await.unwrap();
}

// Break caught: a relative, empty, symlinked or world-accessible CAYW_HOME is
// accepted, letting private tokens land outside the intended root.
#[tokio::test]
async fn unsafe_or_invalid_cayw_home_is_rejected() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();

    for relative in ["relative-state", ""] {
        let mut command = command(home.path(), workspace.path(), Some(OsStr::new(relative)));
        command.arg("init");
        let output = run(command).await;
        assert!(!output.status.success(), "CAYW_HOME={relative:?} must fail");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("CAYW_HOME"),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!workspace.path().join("relative-state").exists());
    }

    // A symlinked root could redirect private state to another location.
    let real = tempfile::tempdir().unwrap();
    let link = home.path().join("linked-root");
    symlink(real.path(), &link).unwrap();
    let mut linked = command(home.path(), workspace.path(), Some(link.as_os_str()));
    linked.arg("init");
    let linked = run(linked).await;
    assert!(!linked.status.success());
    assert!(
        String::from_utf8_lossy(&linked.stderr).contains("private directory"),
        "stderr: {}",
        String::from_utf8_lossy(&linked.stderr)
    );
    assert!(!real.path().join("config.json").exists());

    // Group/other access to the root must be refused before a secret is written.
    let public = home.path().join("public-root");
    std::fs::create_dir(&public).unwrap();
    std::fs::set_permissions(&public, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut world = command(home.path(), workspace.path(), Some(public.as_os_str()));
    world.arg("init");
    let world = run(world).await;
    assert!(!world.status.success());
    assert!(
        String::from_utf8_lossy(&world.stderr).contains("private directory"),
        "stderr: {}",
        String::from_utf8_lossy(&world.stderr)
    );
    assert!(!public.join("config.json").exists());
    assert!(!public.join("ipc.token").exists());
}

// Break caught: a damaged config or token file is overwritten with a fresh
// default, destroying the user's configuration or rotating their secret.
#[tokio::test]
async fn damaged_existing_config_or_token_is_reported_and_never_rebuilt() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();

    // Invalid JSON is reported by init and the exact bytes are preserved.
    let config_root = tempfile::tempdir().unwrap();
    private_dir(config_root.path());
    let config_file = config_root.path().join("config.json");
    std::fs::write(&config_file, b"not json").unwrap();
    std::fs::set_permissions(&config_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    let config_output = init(home.path(), workspace.path(), config_root.path()).await;
    assert!(!config_output.status.success());
    let config_after = std::fs::read(&config_file).unwrap();
    assert_eq!(config_after.as_slice(), b"not json".as_slice());

    // An invalid token is reported and is not silently replaced with a new one.
    let token_root = tempfile::tempdir().unwrap();
    private_dir(token_root.path());
    let token_file = token_root.path().join("ipc.token");
    std::fs::write(&token_file, b"short").unwrap();
    std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    let token_output = init(home.path(), workspace.path(), token_root.path()).await;
    assert!(!token_output.status.success());
    assert!(
        String::from_utf8_lossy(&token_output.stderr).contains("token"),
        "stderr: {}",
        String::from_utf8_lossy(&token_output.stderr)
    );
    let token_after = std::fs::read(&token_file).unwrap();
    assert_eq!(token_after.as_slice(), b"short".as_slice());
}
