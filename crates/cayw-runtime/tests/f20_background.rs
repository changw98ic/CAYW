//! F20 integration test: background ownership survives reopen.
//!
//! Validates:
//! 1) Fixed legal usage model (reporting tokens via AssistantMessage / script_response)
//! 2) Actual CodeMode Luau calling `bg.run`
//! 3) Fixed Host profile (`bg-f20-worker`)
//! 4) Authenticated IPC (Server + Client with Bearer token) for inspection, logs, and cancellation
//! 5) Device descendant cleanup terminating processes
//! 6) Drop runtime and reopen, preserving Session, Task, logs, and ownership without prompt replay.

use cayw_ai::{AssistantEvent, AssistantStream, CancellationToken, Model, ModelRequest, Provider};
use cayw_host::{Profile, ProfileKind};
use cayw_ipc::{Client, ServerConfig};
use cayw_runtime::{
    BackgroundConfig, BackgroundStatus, Runtime, RuntimeConfig, ScopedPermission,
    fixture::script_response,
};
use serde_json::json;
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::net::TcpListener;

const IPC_TOKEN: &str = "0123456789abcdef0123456789abcdef";

struct FixedUsageProvider {
    requests: AtomicUsize,
}

impl Provider for FixedUsageProvider {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        self.requests.fetch_add(1, Ordering::SeqCst);
        let message = script_response(
            &request,
            "return cayw.call('bg.run', {profile='bg-f20-worker', name='f20-bg-task'})",
            &format!("f20-call-{}", request.messages.len()),
        );
        Box::pin(futures::stream::iter([AssistantEvent::Done { message }]))
    }
}

fn test_config() -> RuntimeConfig {
    let mut config = RuntimeConfig::default();
    config.codemode.timeout = Duration::from_secs(20);
    config.max_run_time = Duration::from_secs(30);
    config.background = Some(BackgroundConfig::default());
    config.host.profiles.insert(
        "bg-f20-worker".into(),
        Profile {
            kind: ProfileKind::Check,
            program: "/bin/sh".into(),
            args: vec![
                "-c".into(),
                "echo 'f20-worker-active'; /bin/sleep 60 & wait".into(),
            ],
            env: BTreeMap::new(),
            timeout: Duration::from_secs(60),
            max_output_bytes: 4096,
            allow_filter: false,
            io: cayw_host::ProcessIo::Pipes,
        },
    );
    config.permissions.push(ScopedPermission::exact(
        "bg.run",
        json!({"profile":"bg-f20-worker","name":"f20-bg-task"}),
    ));
    config
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn background_ownership_survives_reopen() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let workspace = root.join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let db = root.join("sessions.sqlite");
    let cfg = test_config();

    let provider = Arc::new(FixedUsageProvider {
        requests: AtomicUsize::new(0),
    });
    let mut model = Model::new("fixture", "background");
    model.max_output_tokens = Some(4096);

    let rt = Runtime::open(
        &db,
        &workspace,
        provider.clone(),
        model.clone(),
        cfg.clone(),
    )
    .unwrap();

    let session_view = rt.create(Some("F20 Background Session".into())).unwrap();
    let session_id = session_view.id;

    // Start background task via CodeMode
    rt.prompt(
        &session_id,
        "start f20 background task",
        CancellationToken::new(),
    )
    .await
    .unwrap();

    assert_eq!(
        provider.requests.load(Ordering::SeqCst),
        2,
        "initial prompt executed tool call round trip"
    );

    // Bind authenticated IPC server
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let ipc_cancel = CancellationToken::new();
    let server_cancel = ipc_cancel.clone();
    let rt_handler = Arc::new(rt.clone());
    let server_handle = tokio::spawn(async move {
        cayw_ipc::serve(
            listener,
            rt_handler,
            ServerConfig::new(IPC_TOKEN),
            server_cancel,
        )
        .await
    });

    // Verify unauthenticated / wrong token connection is rejected
    assert!(
        Client::connect(addr, "wrong-token-must-fail-12345678")
            .await
            .is_err(),
        "unauthenticated IPC client connection must be rejected"
    );

    // Connect authenticated IPC client
    let client = Client::connect(addr, IPC_TOKEN)
        .await
        .expect("authenticated IPC client connects successfully");

    // 1) Authenticated IPC 查询 (list & inspect)
    let list_val = client
        .request("runtime.background.list", json!({"session_id": session_id}))
        .await
        .unwrap();
    let tasks_arr = list_val.as_array().expect("array of background tasks");
    assert_eq!(tasks_arr.len(), 1, "exactly one background task listed");
    let bg_id = tasks_arr[0]["id"].as_str().unwrap().to_owned();

    let inspect_val = client
        .request("runtime.background.inspect", json!({"id": bg_id}))
        .await
        .unwrap();
    assert_eq!(inspect_val["id"], bg_id);
    assert_eq!(inspect_val["name"], "f20-bg-task");
    assert_eq!(inspect_val["profile"], "bg-f20-worker");
    assert_eq!(inspect_val["session_id"], session_id);
    assert_eq!(inspect_val["status"], "running");

    let original_processes = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let connection = rusqlite::Connection::open(&db).unwrap();
            let mut statement = connection
                .prepare(
                    "SELECT pid,parent_pid,process_group,started_seconds,started_microseconds
                 FROM runtime_background_processes WHERE task_id=?1 AND state!='stopped'",
                )
                .unwrap();
            let identities = statement
                .query_map([&bg_id], |row| {
                    Ok(cayw_host::ProcessIdentity {
                        pid: row.get(0)?,
                        parent_pid: row.get(1)?,
                        process_group: row.get(2)?,
                        started_seconds: u64::try_from(row.get::<_, i64>(3)?)
                            .expect("valid original start time"),
                        started_microseconds: row.get(4)?,
                    })
                })
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            if identities.len() >= 2
                && identities.iter().any(|child| {
                    identities
                        .iter()
                        .any(|parent| child.parent_pid == parent.pid)
                })
            {
                for identity in &identities {
                    assert!(
                        identity
                            .is_running()
                            .expect("observe original process before cancellation")
                    );
                }
                break identities;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("Device records shell and actual descendant before cancellation");

    // 2) Authenticated IPC 取消 (kill)
    let kill_val = client
        .request("runtime.background.kill", json!({"id": bg_id}))
        .await
        .unwrap();
    let kill_status = kill_val["status"].as_str().unwrap_or("");
    assert!(
        kill_status == "running" || kill_status == "killed",
        "kill request must return running or killed, got: {kill_status}"
    );

    // Wait until status reaches terminal Killed
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let inspect_val = client
            .request("runtime.background.inspect", json!({"id": bg_id}))
            .await
            .unwrap();
        if inspect_val["status"] == "killed" {
            break;
        }
        if tokio::time::Instant::now() > deadline {
            panic!("background task did not reach killed status in time: {inspect_val:?}");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // 3) Authenticated IPC 日志 (logs)
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let logs_val = client
            .request(
                "runtime.background.logs",
                json!({"id": bg_id, "offset": 0, "limit": 1024}),
            )
            .await
            .unwrap();
        let log_text = logs_val["text"].as_str().unwrap_or("");
        if log_text.contains("f20-worker-active") {
            break;
        }
        if tokio::time::Instant::now() > deadline {
            panic!("logs did not contain expected output in time: {log_text}");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // 4) Check each original incarnation, including the real descendant.
    let connection = rusqlite::Connection::open(&db).unwrap();
    for identity in original_processes {
        assert!(
            !identity
                .is_running()
                .expect("observe original process after cleanup"),
            "original process remains alive: {identity:?}"
        );
        let stopped: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM runtime_background_processes WHERE task_id=?1
             AND pid=?2 AND started_seconds=?3 AND started_microseconds=?4 AND state='stopped')",
                rusqlite::params![
                    bg_id,
                    identity.pid,
                    i64::try_from(identity.started_seconds).expect("start time fits journal"),
                    identity.started_microseconds
                ],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            stopped,
            "Device must durably confirm each original process stopped"
        );
    }
    let cleanup: String = connection
        .query_row(
            "SELECT cleanup_state FROM runtime_background_executors WHERE task_id=?1",
            [&bg_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(cleanup, "device_confirmed");
    drop(connection);

    // Close IPC connection and stop server
    client.close().await;
    ipc_cancel.cancel();
    let _ = server_handle.await;

    // Drop first runtime instance
    drop(rt);

    // 5) Reopen runtime on the same database and workspace
    let reopen_provider = Arc::new(FixedUsageProvider {
        requests: AtomicUsize::new(0),
    });
    let rt2 = Runtime::open(&db, &workspace, reopen_provider.clone(), model.clone(), cfg).unwrap();

    // Verify Session is preserved
    let session = rt2.get(&session_id).unwrap();
    assert_eq!(session.id, session_id);
    assert_eq!(session.label, "F20 Background Session");

    // Verify Task is preserved
    let tasks = rt2.tasks(&session_id).unwrap();
    assert_eq!(tasks.len(), 1, "task count must be preserved across reopen");
    assert_eq!(tasks[0].session_id, session_id);

    // Verify background task logs are preserved
    let logs = rt2.background_logs(&bg_id, 0, 1024).unwrap();
    assert!(
        logs.text.contains("f20-worker-active"),
        "logs must be preserved across reopen"
    );

    // Verify ownership and terminal status are preserved
    let bg = rt2
        .background_inspect(&bg_id)
        .unwrap()
        .expect("background task view must exist on reopened runtime");
    assert_eq!(
        bg.session_id, session_id,
        "ownership must belong to original session"
    );
    assert_eq!(bg.name, "f20-bg-task");
    assert_eq!(bg.profile, "bg-f20-worker");
    assert_eq!(bg.status, BackgroundStatus::Killed);

    // Verify NO prompt replay occurred on reopen
    assert_eq!(
        reopen_provider.requests.load(Ordering::SeqCst),
        0,
        "no prompt replay must occur on reopen"
    );
}
