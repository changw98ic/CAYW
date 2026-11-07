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
    ffi::OsStr,
    os::unix::{
        ffi::OsStrExt,
        fs::{PermissionsExt, symlink},
    },
    path::Path,
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
    let connection_file = state.join("connection.json");

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
        ready["connection_file"].as_str().unwrap(),
        connection_file.to_str().unwrap()
    );
    assert!(database.is_file());
    assert_private_dir(root.path());
    assert_private_dir(&state);
    assert_private_file(&config_file);
    assert_private_file(&token_file);

    // connection.json is the discovery record a client reads from the same
    // workspace. It must point at this live loopback service and token.
    let connection: Value =
        serde_json::from_slice(&std::fs::read(&connection_file).unwrap()).unwrap();
    assert_eq!(connection["address"], ready["listening"]);
    assert_eq!(
        connection["workspace"].as_str().unwrap(),
        canonical_workspace.to_str().unwrap()
    );
    assert_eq!(connection["token_file"], ready["token_file"]);

    let client = connect(&ready).await;

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

// Break caught: two serve processes race on the same database, or a second
// workspace silently reuses the first workspace's database.
#[tokio::test]
async fn one_server_per_workspace_and_a_separate_database_per_workspace() {
    let root = tempfile::tempdir().unwrap();
    private_dir(root.path());
    let home = tempfile::tempdir().unwrap();
    let first_workspace = tempfile::tempdir().unwrap();
    let second_workspace = tempfile::tempdir().unwrap();

    let mut first = serve(home.path(), first_workspace.path(), root.path(), &[])
        .spawn()
        .unwrap();
    let first_ready = read_ready(&mut first).await;

    // The same workspace cannot run a second service. Failure happens before a
    // second database, listener or ready line is produced.
    let rejected = run(serve(home.path(), first_workspace.path(), root.path(), &[])).await;
    assert!(!rejected.status.success());
    assert!(
        String::from_utf8_lossy(&rejected.stderr).contains("local server already running"),
        "stderr: {}",
        String::from_utf8_lossy(&rejected.stderr)
    );
    assert!(
        rejected.stdout.is_empty(),
        "a rejected serve prints no ready line"
    );

    let mut second = serve(home.path(), second_workspace.path(), root.path(), &[])
        .spawn()
        .unwrap();
    let second_ready = read_ready(&mut second).await;

    assert_ne!(first_ready["database"], second_ready["database"]);
    assert_ne!(
        first_ready["connection_file"],
        second_ready["connection_file"]
    );
    assert!(Path::new(first_ready["database"].as_str().unwrap()).is_file());
    assert!(Path::new(second_ready["database"].as_str().unwrap()).is_file());
    assert!(Path::new(first_ready["connection_file"].as_str().unwrap()).is_file());
    assert!(Path::new(second_ready["connection_file"].as_str().unwrap()).is_file());

    // One private root is shared, but each workspace keeps its own state.
    assert_eq!(first_ready["token_file"], second_ready["token_file"]);
    assert_eq!(first_ready["config_file"], second_ready["config_file"]);

    first.kill().await.unwrap();
    first.wait().await.unwrap();
    second.kill().await.unwrap();
    second.wait().await.unwrap();
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
    assert!(ready["connection_file"].is_null());
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
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("local server already running"));
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
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("local server already running"));
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
