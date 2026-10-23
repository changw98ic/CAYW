#![cfg(unix)]

use async_trait::async_trait;
use cayw_ai::CancellationToken;
use cayw_codemode::{CapabilityError, CapabilityHost};
use cayw_host::terminal::{TerminalConfig, TerminalLimits};
use cayw_host::{AuditSink, Host, HostConfig, PermissionGate, Profile, ProfileKind};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Root(PathBuf);
impl Root {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "cayw-terminal-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Default)]
struct Gate(Mutex<Vec<Value>>);
#[async_trait]
impl PermissionGate for Gate {
    async fn authorize(
        &self,
        capability: &str,
        arguments: &Value,
        _: CancellationToken,
    ) -> Result<(), CapabilityError> {
        self.0.lock().unwrap().push(json!({
            "capability": capability,
            "arguments": arguments,
        }));
        Ok(())
    }
}

#[derive(Default)]
struct Audit(Mutex<Vec<Value>>);
#[async_trait]
impl AuditSink for Audit {
    async fn started(&self, name: &str, _: &Value) -> Result<String, CapabilityError> {
        let mut records = self.0.lock().unwrap();
        let id = records.len().to_string();
        records.push(json!({"id":id,"phase":"running","name":name}));
        Ok(id)
    }
    async fn settled(
        &self,
        id: &str,
        result: &Result<Value, CapabilityError>,
    ) -> Result<(), CapabilityError> {
        self.0.lock().unwrap().push(match result {
            Ok(value) => json!({"id":id,"phase":"succeeded","value":value}),
            Err(error) => json!({
                "id":id,
                "phase":if error.outcome_unknown {"unknown"} else {"failed"},
                "message":error.message,
            }),
        });
        Ok(())
    }
    fn dropped(&self, id: &str) -> Result<(), CapabilityError> {
        self.0
            .lock()
            .unwrap()
            .push(json!({"id":id,"phase":"unknown","dropped":true}));
        Ok(())
    }
}

fn fixture() -> PathBuf {
    env!("CARGO_BIN_EXE_cayw-host-test-process").into()
}

fn profile(
    program: impl Into<PathBuf>,
    args: &[&str],
    io: cayw_device::ProcessIo,
    timeout: Duration,
    max_output_bytes: usize,
) -> Profile {
    Profile {
        kind: ProfileKind::Check,
        program: program.into(),
        args: args.iter().map(|value| (*value).into()).collect(),
        env: BTreeMap::new(),
        timeout,
        max_output_bytes,
        allow_filter: false,
        io,
    }
}

fn limits() -> TerminalLimits {
    TerminalLimits {
        max_sessions: 2,
        max_sessions_total: 8,
        max_input_bytes_per_write: 64,
        max_input_bytes_per_session: 256,
        max_output_bytes_per_session: 4096,
        max_read_bytes: 64,
        max_wait: Duration::from_secs(10),
    }
}

fn config(profiles: BTreeMap<String, Profile>, limits: TerminalLimits) -> HostConfig {
    HostConfig {
        terminal_handoffs: Some(TerminalConfig { profiles, limits }),
        ..Default::default()
    }
}

fn host(root: &Path, config: HostConfig) -> (Arc<Host>, Arc<Gate>, Arc<Audit>) {
    let gate = Arc::new(Gate::default());
    let audit = Arc::new(Audit::default());
    let host =
        Arc::new(Host::new(root, gate.clone(), audit.clone(), config).expect("terminal host"));
    (host, gate, audit)
}

async fn call(host: &Host, name: &str, arguments: Value) -> Result<Value, CapabilityError> {
    host.call(name, arguments, CancellationToken::new()).await
}

async fn exec(host: &Host, profile: &str) -> Result<Value, CapabilityError> {
    call(host, "exec_command", json!({"profile": profile})).await
}

fn session_id(value: &Value) -> String {
    value["session_id"].as_str().unwrap().into()
}

async fn read_page(host: &Host, id: &str, cursor: u64, max_bytes: usize) -> Value {
    call(
        host,
        "write_stdin",
        json!({
            "session_id": id,
            "action": "read",
            "request_id": format!("read-{cursor}"),
            "cursor": cursor,
            "max_bytes": max_bytes,
        }),
    )
    .await
    .expect("terminal read")
}

fn page_bytes(value: &Value) -> Vec<u8> {
    value["bytes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|byte| byte.as_u64().unwrap() as u8)
        .collect()
}

/// A process can publish its exit before the reader task has drained the last
/// stdout chunk. Reads are non-blocking, so poll until the first chunk or EOF.
async fn read_page_ready(host: &Host, id: &str, cursor: u64, max_bytes: usize) -> Value {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let page = read_page(host, id, cursor, max_bytes).await;
            if !page_bytes(&page).is_empty() || page["eof"].as_bool() == Some(true) {
                return page;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("terminal output did not become readable")
}

async fn wait_status(host: &Host, id: &str, wait_ms: u64) -> Value {
    call(
        host,
        "write_stdin",
        json!({
            "session_id": id,
            "action": "wait",
            "request_id": format!("wait-{wait_ms}"),
            "wait_ms": wait_ms,
        }),
    )
    .await
    .expect("terminal wait")
}

async fn read_until_eof(host: &Host, id: &str, max_bytes: usize) -> Vec<u8> {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut output = Vec::new();
        let mut cursor = 0;
        loop {
            let page = read_page(host, id, cursor, max_bytes).await;
            let next = page["next_cursor"].as_u64().unwrap();
            let total = page["status"]["output_cursor"].as_u64().unwrap();
            output.extend_from_slice(&page_bytes(&page));
            if page["eof"].as_bool() == Some(true) && next == total {
                return output;
            }
            if next == cursor {
                tokio::time::sleep(Duration::from_millis(5)).await;
            } else {
                cursor = next;
            }
        }
    })
    .await
    .expect("terminal output did not reach EOF")
}

async fn ready_text(path: &Path) -> String {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Ok(bytes) = fs::read(path)
                && let Ok(value) = String::from_utf8(bytes)
                && !value.is_empty()
            {
                return value;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("fixture did not publish readiness")
}

async fn completion_error(host: &Host, needle: &str) -> String {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(error) = host.terminal_completion_error()
                && error.contains(needle)
            {
                return error;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("terminal completion error did not appear")
}

fn assert_gone(pid: u32) {
    let observed = std::process::Command::new("/bin/kill")
        .args(["-0", &pid.to_string()])
        .output()
        .unwrap();
    assert!(!observed.status.success(), "PID {pid} remained alive");
}

#[cfg(target_os = "macos")]
#[derive(Debug, Default)]
struct Observer {
    started: Mutex<Vec<u32>>,
    stopped: Mutex<Vec<u32>>,
}

#[cfg(target_os = "macos")]
impl cayw_host::ProcessObserver for Observer {
    fn started(&self, identity: &cayw_host::ProcessIdentity) -> std::io::Result<()> {
        self.started.lock().unwrap().push(identity.pid);
        Ok(())
    }
    fn observed(&self, _: &[cayw_host::ProcessIdentity]) -> std::io::Result<()> {
        Ok(())
    }
    fn stopped(&self, identity: &cayw_host::ProcessIdentity) -> std::io::Result<()> {
        self.stopped.lock().unwrap().push(identity.pid);
        Ok(())
    }
}

#[tokio::test]
async fn exec_selects_only_configured_profile_and_exact_permission() {
    let root = Root::new();
    let profiles = BTreeMap::from([(
        "echo".into(),
        profile(
            "/usr/bin/head",
            &["-n", "1"],
            cayw_device::ProcessIo::Pipes,
            Duration::from_secs(10),
            4096,
        ),
    )]);
    let (host, gate, _audit) = host(&root.0, config(profiles, limits()));

    let error = exec(&host, "missing").await.unwrap_err();
    assert!(!error.outcome_unknown);
    assert!(gate.0.lock().unwrap().is_empty());

    let error = call(
        &host,
        "exec_command",
        json!({"profile":"echo", "program":"/bin/sh"}),
    )
    .await
    .unwrap_err();
    assert!(!error.outcome_unknown);
    assert!(gate.0.lock().unwrap().is_empty());

    let started = exec(&host, "echo").await.unwrap();
    let id = session_id(&started);
    assert!(id.starts_with("term-"));
    assert_eq!(
        gate.0.lock().unwrap().last().unwrap()["arguments"],
        json!({"profile":"echo"})
    );

    let error = call(
        &host,
        "write_stdin",
        json!({"session_id":id,"action":"read","request_id":"bad","cursor":0,"data":"x"}),
    )
    .await
    .unwrap_err();
    assert!(!error.outcome_unknown);
    assert!(error.message.contains("read does not accept"), "{error}");

    let written = call(
        &host,
        "write_stdin",
        json!({"session_id":id,"action":"write","request_id":"w1","data":"hello\n"}),
    )
    .await
    .unwrap();
    assert_eq!(written["accepted_bytes"].as_u64(), Some(6));
    assert_eq!(
        gate.0.lock().unwrap().last().unwrap()["arguments"],
        json!({"session_id":id,"action":"write","request_id":"w1","data":"hello\n"})
    );

    let status = wait_status(&host, &id, 10_000).await;
    assert_eq!(status["status"]["state"], "exited");
    assert_eq!(status["status"]["exit_code"].as_i64(), Some(0));
    assert_eq!(read_until_eof(&host, &id, 64).await, b"hello\n");
    host.drain_processes().await.unwrap();
}

#[tokio::test]
async fn write_is_idempotent_and_read_pages_raw_utf8_bytes() {
    let root = Root::new();
    let profiles = BTreeMap::from([(
        "echo".into(),
        profile(
            "/usr/bin/head",
            &["-n", "1"],
            cayw_device::ProcessIo::Pipes,
            Duration::from_secs(10),
            4096,
        ),
    )]);
    let mut limits = limits();
    limits.max_read_bytes = 1;
    let (host, _gate, _audit) = host(&root.0, config(profiles, limits));

    let id = session_id(&exec(&host, "echo").await.unwrap());
    let payload = "é汉\n";
    let first = call(
        &host,
        "write_stdin",
        json!({"session_id":id,"action":"write","request_id":"same","data":payload}),
    )
    .await
    .unwrap();
    let second = call(
        &host,
        "write_stdin",
        json!({"session_id":id,"action":"write","request_id":"same","data":"evil\n"}),
    )
    .await
    .unwrap();
    assert_eq!(first["accepted_bytes"].as_u64(), Some(payload.len() as u64));
    assert_eq!(
        second["accepted_bytes"].as_u64(),
        Some(payload.len() as u64)
    );
    assert_eq!(second["input_bytes"].as_u64(), Some(payload.len() as u64));

    assert_eq!(
        wait_status(&host, &id, 10_000).await["status"]["state"],
        "exited"
    );
    let first_page = read_page_ready(&host, &id, 0, 1).await;
    assert_eq!(page_bytes(&first_page).len(), 1);
    let error = call(
        &host,
        "write_stdin",
        json!({"session_id":id,"action":"read","request_id":"skip","cursor":2,"max_bytes":1}),
    )
    .await
    .unwrap_err();
    assert!(!error.outcome_unknown);
    assert!(error.message.contains("skips unread"), "{error}");

    let output = read_until_eof(&host, &id, 1).await;
    assert_eq!(output, payload.as_bytes());
    let same_page = read_page(&host, &id, 0, 1).await;
    assert_eq!(page_bytes(&same_page), page_bytes(&first_page));
    host.drain_processes().await.unwrap();
}

#[tokio::test]
async fn session_and_input_budgets_do_not_reset_after_wait_or_close() {
    let root = Root::new();
    let profiles = BTreeMap::from([(
        "head".into(),
        profile(
            "/usr/bin/head",
            &["-c", "4"],
            cayw_device::ProcessIo::Pipes,
            Duration::from_secs(10),
            4096,
        ),
    )]);
    let mut limits = limits();
    limits.max_sessions = 1;
    limits.max_sessions_total = 1;
    limits.max_input_bytes_per_write = 4;
    limits.max_input_bytes_per_session = 4;
    limits.max_read_bytes = 16;
    let (host, _gate, _audit) = host(&root.0, config(profiles, limits));

    let id = session_id(&exec(&host, "head").await.unwrap());
    call(
        &host,
        "write_stdin",
        json!({"session_id":id,"action":"write","request_id":"w1","data":"abc"}),
    )
    .await
    .unwrap();
    assert_eq!(
        wait_status(&host, &id, 0).await["status"]["state"],
        "running"
    );
    let error = call(
        &host,
        "write_stdin",
        json!({"session_id":id,"action":"write","request_id":"w2","data":"de"}),
    )
    .await
    .unwrap_err();
    assert!(!error.outcome_unknown);
    assert!(error.message.contains("input budget"));
    call(
        &host,
        "write_stdin",
        json!({"session_id":id,"action":"write","request_id":"w3","data":"\n"}),
    )
    .await
    .unwrap();

    assert_eq!(
        wait_status(&host, &id, 10_000).await["status"]["state"],
        "exited"
    );
    assert_eq!(read_until_eof(&host, &id, 16).await, b"abc\n");
    let error = exec(&host, "head").await.unwrap_err();
    assert!(error.message.contains("lifetime budget"));
    host.drain_processes().await.unwrap();
}

#[tokio::test]
async fn another_task_cannot_address_this_sessions_handle() {
    let root = Root::new();
    let make = || {
        BTreeMap::from([(
            "cat".into(),
            profile(
                "/bin/cat",
                &[],
                cayw_device::ProcessIo::Pipes,
                Duration::from_secs(10),
                4096,
            ),
        )])
    };
    let (one, _, _) = host(&root.0, config(make(), limits()));
    let (two, _, _) = host(&root.0, config(make(), limits()));
    let id = session_id(&exec(&one, "cat").await.unwrap());
    let error = call(
        &two,
        "write_stdin",
        json!({"session_id":id,"action":"read","request_id":"r","cursor":0,"max_bytes":1}),
    )
    .await
    .unwrap_err();
    assert!(!error.outcome_unknown);
    assert!(error.message.contains("not owned"));
    two.drain_processes().await.unwrap();
    one.drain_processes().await.unwrap_err();
}

#[tokio::test]
async fn live_terminal_blocks_task_drain_and_reaps_the_real_process() {
    let root = Root::new();
    let profiles = BTreeMap::from([(
        "wait".into(),
        profile(
            fixture(),
            &["wait"],
            cayw_device::ProcessIo::Pipes,
            Duration::from_secs(20),
            4096,
        ),
    )]);
    let (host, _gate, _audit) = host(&root.0, config(profiles, limits()));
    let id = session_id(&exec(&host, "wait").await.unwrap());
    let pid = ready_text(&root.0.join("effect.pid"))
        .await
        .trim()
        .parse::<u32>()
        .unwrap();

    host.drain_processes().await.unwrap_err();
    assert_gone(pid);
    assert!(host.outcome_unknown());
    let _ = id;
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn pty_session_reports_controlling_terminal_and_normal_exit() {
    let root = Root::new();
    let profiles = BTreeMap::from([(
        "terminal".into(),
        profile(
            fixture(),
            &["terminal"],
            cayw_device::ProcessIo::Pty {
                columns: 80,
                rows: 24,
            },
            Duration::from_secs(10),
            4096,
        ),
    )]);
    let (host, _gate, _audit) = host(&root.0, config(profiles, limits()));
    let id = session_id(&exec(&host, "terminal").await.unwrap());
    let status = wait_status(&host, &id, 10_000).await;
    assert_eq!(status["status"]["state"], "exited");
    let output = read_until_eof(&host, &id, 64).await;
    let text = String::from_utf8_lossy(&output);
    assert!(text.contains("\"controlling_terminal\":true"), "{text}");
    host.drain_processes().await.unwrap();
}

#[tokio::test]
async fn output_limit_retains_unknown_and_blocks_further_reads() {
    let root = Root::new();
    let profiles = BTreeMap::from([(
        "cat".into(),
        profile(
            "/bin/cat",
            &[],
            cayw_device::ProcessIo::Pipes,
            Duration::from_secs(10),
            4096,
        ),
    )]);
    let mut limits = limits();
    limits.max_output_bytes_per_session = 8;
    limits.max_read_bytes = 8;
    let (host, _gate, _audit) = host(&root.0, config(profiles, limits));
    let id = session_id(&exec(&host, "cat").await.unwrap());
    call(
        &host,
        "write_stdin",
        json!({"session_id":id,"action":"write","request_id":"overflow","data":"0123456789abcdef"}),
    )
    .await
    .unwrap();
    let error = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match call(
                &host,
                "write_stdin",
                json!({"session_id":id,"action":"read","request_id":"read","cursor":0,"max_bytes":8}),
            )
            .await
            {
                Err(error) if error.outcome_unknown => break error,
                Ok(_) | Err(_) => tokio::time::sleep(Duration::from_millis(5)).await,
            }
        }
    })
    .await
    .expect("output limit did not become unknown");
    assert!(error.outcome_unknown);
    assert!(host.drain_processes().await.is_err());
}

#[tokio::test]
async fn close_and_cancel_never_claim_a_known_rollback() {
    for action in ["close", "cancel"] {
        let root = Root::new();
        let profiles = BTreeMap::from([(
            "cat".into(),
            profile(
                "/bin/cat",
                &[],
                cayw_device::ProcessIo::Pipes,
                Duration::from_secs(10),
                4096,
            ),
        )]);
        let (host, _gate, _audit) = host(&root.0, config(profiles, limits()));
        let id = session_id(&exec(&host, "cat").await.unwrap());
        call(
            &host,
            "write_stdin",
            json!({"session_id":id,"action":"write","request_id":"w","data":"x"}),
        )
        .await
        .unwrap();
        let error = call(
            &host,
            "write_stdin",
            json!({"session_id":id,"action":action,"request_id":action}),
        )
        .await
        .unwrap_err();
        assert!(error.outcome_unknown, "{action}: {error}");
        assert!(host.drain_processes().await.is_err());
    }
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn scoped_observer_sees_terminal_process_lifecycle() {
    let root = Root::new();
    let profiles = BTreeMap::from([(
        "effect".into(),
        profile(
            fixture(),
            &["effect"],
            cayw_device::ProcessIo::Pipes,
            Duration::from_secs(10),
            4096,
        ),
    )]);
    let (base, _gate, _audit) = host(&root.0, config(profiles, limits()));
    let observer = Arc::new(Observer::default());
    let scoped = base.with_process_observer(observer.clone());
    let id = session_id(&exec(&scoped, "effect").await.unwrap());
    let pid = ready_text(&root.0.join("effect.pid"))
        .await
        .trim()
        .parse::<u32>()
        .unwrap();

    assert_eq!(
        wait_status(&scoped, &id, 10_000).await["status"]["state"],
        "exited"
    );
    scoped.drain_processes().await.unwrap();
    assert_eq!(observer.started.lock().unwrap().as_slice(), &[pid]);
    assert_eq!(observer.stopped.lock().unwrap().as_slice(), &[pid]);
}

#[tokio::test]
async fn live_terminal_blocks_completion_before_drain_and_after_cancel() {
    let root = Root::new();
    let profiles = BTreeMap::from([(
        "cat".into(),
        profile(
            "/bin/cat",
            &[],
            cayw_device::ProcessIo::Pipes,
            Duration::from_secs(10),
            4096,
        ),
    )]);
    let (host, _gate, _audit) = host(&root.0, config(profiles, limits()));
    let id = session_id(&exec(&host, "cat").await.unwrap());
    assert_eq!(host.active_terminal_sessions(), 1);
    let error = host.terminal_completion_error().unwrap();
    assert!(error.contains("still running"), "{error}");

    assert!(
        call(
            &host,
            "write_stdin",
            json!({"session_id":id,"action":"cancel","request_id":"cancel"})
        )
        .await
        .unwrap_err()
        .outcome_unknown
    );
    assert!(host.drain_processes().await.is_err());
    let error = host.terminal_completion_error().unwrap();
    assert!(error.contains("unknown"), "{error}");
    assert_eq!(host.active_terminal_sessions(), 0);
}

#[tokio::test]
async fn nonzero_terminal_exit_blocks_completion_and_survives_drain() {
    let root = Root::new();
    let profiles = BTreeMap::from([(
        "false".into(),
        profile(
            "/usr/bin/false",
            &[],
            cayw_device::ProcessIo::Pipes,
            Duration::from_secs(10),
            4096,
        ),
    )]);
    let (host, _gate, _audit) = host(&root.0, config(profiles, limits()));
    let id = session_id(&exec(&host, "false").await.unwrap());
    assert_eq!(
        wait_status(&host, &id, 10_000).await["status"]["exit_code"].as_i64(),
        Some(1)
    );
    assert_eq!(read_until_eof(&host, &id, 64).await, b"");
    assert_eq!(host.active_terminal_sessions(), 0);
    let error = completion_error(&host, "exited with code 1").await;
    assert!(error.contains("exited with code 1"), "{error}");

    host.drain_processes().await.unwrap();
    let error = host.terminal_completion_error().unwrap();
    assert!(error.contains("exited with code 1"), "{error}");
}

#[tokio::test]
async fn unread_terminal_output_blocks_completion_and_survives_drain() {
    let root = Root::new();
    let profiles = BTreeMap::from([(
        "echo".into(),
        profile(
            "/usr/bin/head",
            &["-n", "1"],
            cayw_device::ProcessIo::Pipes,
            Duration::from_secs(10),
            4096,
        ),
    )]);
    let (host, _gate, _audit) = host(&root.0, config(profiles, limits()));
    let id = session_id(&exec(&host, "echo").await.unwrap());
    call(
        &host,
        "write_stdin",
        json!({"session_id":id,"action":"write","request_id":"w","data":"hello\n"}),
    )
    .await
    .unwrap();
    assert_eq!(
        wait_status(&host, &id, 10_000).await["status"]["state"],
        "exited"
    );
    assert_eq!(host.active_terminal_sessions(), 0);
    let error = completion_error(&host, "unread").await;
    assert!(error.contains("unread"), "{error}");

    host.drain_processes().await.unwrap();
    let error = host.terminal_completion_error().unwrap();
    assert!(error.contains("unread"), "{error}");
}

#[tokio::test]
async fn all_terminal_output_read_allows_completion() {
    let root = Root::new();
    let profiles = BTreeMap::from([(
        "echo".into(),
        profile(
            "/usr/bin/head",
            &["-n", "1"],
            cayw_device::ProcessIo::Pipes,
            Duration::from_secs(10),
            4096,
        ),
    )]);
    let (host, _gate, _audit) = host(&root.0, config(profiles, limits()));
    let id = session_id(&exec(&host, "echo").await.unwrap());
    call(
        &host,
        "write_stdin",
        json!({"session_id":id,"action":"write","request_id":"w","data":"hello\n"}),
    )
    .await
    .unwrap();
    assert_eq!(
        wait_status(&host, &id, 10_000).await["status"]["state"],
        "exited"
    );
    assert_eq!(read_until_eof(&host, &id, 64).await, b"hello\n");
    assert_eq!(host.active_terminal_sessions(), 0);
    assert!(host.terminal_completion_error().is_none());

    host.drain_processes().await.unwrap();
    assert!(host.terminal_completion_error().is_none());
}

#[tokio::test]
async fn wait_does_not_reset_the_session_deadline() {
    let root = Root::new();
    let profiles = BTreeMap::from([(
        "wait".into(),
        profile(
            fixture(),
            &["wait"],
            cayw_device::ProcessIo::Pipes,
            Duration::from_secs(20),
            4096,
        ),
    )]);
    let (host, _gate, _audit) = host(&root.0, config(profiles, limits()));
    let id = session_id(&exec(&host, "wait").await.unwrap());
    let first = wait_status(&host, &id, 0).await;
    let second = wait_status(&host, &id, 0).await;
    assert_eq!(first["status"]["state"], "running");
    assert!(
        first["status"]["remaining_ms"].as_u64().unwrap()
            >= second["status"]["remaining_ms"].as_u64().unwrap()
    );
    assert!(
        call(
            &host,
            "write_stdin",
            json!({"session_id":id,"action":"cancel","request_id":"cancel"})
        )
        .await
        .unwrap_err()
        .outcome_unknown
    );
    assert!(host.drain_processes().await.is_err());
}
