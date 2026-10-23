use cayw_ai::{CancellationToken, Model};
use cayw_ipc::{Client, ServerConfig, serve};
use cayw_runtime::{Runtime, RuntimeConfig, ScopedPermission, fixture::OfflineProvider};
use serde_json::json;
use std::{sync::Arc, time::Duration};

#[tokio::test]
async fn authenticated_rpc_uses_real_runtime_and_durable_cursor_pages() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("workspace")).unwrap();
    let config = RuntimeConfig {
        permissions: vec![
            ScopedPermission::exact(
                "fs.write",
                json!({"path":"cayw-demo.txt","content":"headless runtime demo"}),
            ),
            ScopedPermission::exact("fs.read", json!({"path":"cayw-demo.txt"})),
        ],
        ..Default::default()
    };
    let rt = Runtime::open(
        directory.path().join("sessions.db"),
        directory.path().join("workspace"),
        Arc::new(OfflineProvider::default()),
        Model::new("fixture", "ipc-runtime"),
        config,
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let shutdown = CancellationToken::new();
    let server = tokio::spawn(serve(
        listener,
        Arc::new(rt),
        ServerConfig::new("runtime-test-secret-unique"),
        shutdown.clone(),
    ));
    let client = Client::connect(address, "runtime-test-secret-unique")
        .await
        .unwrap();
    let health = client.request("runtime.health", json!({})).await.unwrap();
    assert_eq!(health["status"], "ok");
    assert_eq!(health["protocol_version"], cayw_core::PROTOCOL_VERSION);
    assert!(client.request("health", json!({})).await.is_err());
    assert!(client.request("capabilities", json!({})).await.is_err());
    assert!(
        client
            .request("capabilities.list", json!({}))
            .await
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .any(|cap| cap["name"] == "fs.read")
    );
    let session = client
        .request("session.create", json!({"label":"ipc fixture"}))
        .await
        .unwrap();
    let id = session["id"].as_str().unwrap();
    let answer = client
        .request(
            "session.prompt",
            json!({"session_id":id,"text":"write and read"}),
        )
        .await
        .unwrap();
    assert_eq!(answer["id"], id);
    assert_eq!(answer["state"], "idle");
    assert!(answer["cursor"].as_u64().unwrap() > 0);
    let messages = answer["messages"].as_array().unwrap();
    assert_eq!(messages.last().unwrap()["stop_reason"], "stop");
    assert_eq!(
        std::fs::read_to_string(directory.path().join("workspace/cayw-demo.txt")).unwrap(),
        "headless runtime demo"
    );
    let first = client
        .request(
            "session.events",
            json!({"session_id":id,"after":0,"limit":3}),
        )
        .await
        .unwrap();
    assert_eq!(first.as_array().unwrap().len(), 3);
    let cursor = first[2]["cursor"].as_u64().unwrap();
    let next = client
        .request(
            "session.events",
            json!({"session_id":id,"after":cursor,"limit":1000}),
        )
        .await
        .unwrap();
    assert_eq!(next[0]["cursor"].as_u64().unwrap(), cursor + 1);
    assert!(
        client
            .request(
                "session.events",
                json!({"session_id":id,"after":u64::MAX,"limit":100})
            )
            .await
            .is_err()
    );
    assert!(
        client
            .request(
                "approval.decide",
                json!({"session_id":id,"approval_id":"made-up","decision":"allow_task"})
            )
            .await
            .is_err()
    );
    assert!(
        client
            .request(
                "session.fork",
                json!({"session_id":id,"label":"no","confirm":false,"mode":"chat"})
            )
            .await
            .is_err()
    );
    assert!(
        client
            .request(
                "session.fork",
                json!({"session_id":id,"label":"no","confirm":true,"mode":"workflow"})
            )
            .await
            .is_err()
    );
    let fork = client
        .request(
            "session.fork",
            json!({"session_id":id,"label":"confirmed chat","confirm":true,"mode":"chat"}),
        )
        .await
        .unwrap();
    assert_ne!(fork["id"], session["id"]);
    assert_eq!(fork["messages"], answer["messages"]);
    client.close().await;
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn binary_process_serves_real_luau_file_round_trip() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_cayw"))
        .arg("serve")
        .arg("--db")
        .arg(directory.path().join("sessions.db"))
        .arg("--workspace")
        .arg(&workspace)
        .arg("--listen")
        .arg("127.0.0.1:0")
        .arg("--token-env")
        .arg("CAYW_RUNTIME_TEST_TOKEN")
        .arg("--offline")
        .arg("--demo-authorized")
        .env("CAYW_RUNTIME_TEST_TOKEN", "runtime-process-test-token")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    use tokio::io::{AsyncBufReadExt, BufReader};
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout).lines();
    let line = tokio::time::timeout(Duration::from_secs(10), reader.next_line())
        .await
        .unwrap()
        .unwrap()
        .expect("server ready line");
    let ready: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(ready["offline_fixture"], true);
    let client = Client::connect(
        ready["listening"].as_str().unwrap().parse().unwrap(),
        "runtime-process-test-token",
    )
    .await
    .unwrap();
    let session = client.request("session.create", json!({})).await.unwrap();
    client
        .request(
            "session.prompt",
            json!({"session_id":session["id"],"text":"offline real file effects"}),
        )
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(workspace.join("cayw-demo.txt")).unwrap(),
        "headless runtime demo"
    );
    client.close().await;
    child.kill().await.unwrap();
    child.wait().await.unwrap();
}

async fn start_offline_binary(dir: &tempfile::TempDir) -> (tokio::process::Child, Client) {
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_cayw"))
        .arg("serve")
        .arg("--db")
        .arg(dir.path().join("session.db"))
        .arg("--workspace")
        .arg(dir.path().join("workspace"))
        .arg("--listen")
        .arg("127.0.0.1:0")
        .arg("--token-env")
        .arg("CAYW_RESTART_TOKEN")
        .arg("--offline")
        .env("CAYW_RESTART_TOKEN", "restart-process-test-secret")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    use tokio::io::{AsyncBufReadExt, BufReader};
    let ready = tokio::time::timeout(
        Duration::from_secs(10),
        BufReader::new(child.stdout.take().unwrap())
            .lines()
            .next_line(),
    )
    .await
    .unwrap()
    .unwrap()
    .unwrap();
    let ready: serde_json::Value = serde_json::from_str(&ready).unwrap();
    let client = Client::connect(
        ready["listening"].as_str().unwrap().parse().unwrap(),
        "restart-process-test-secret",
    )
    .await
    .unwrap();
    (child, client)
}

#[tokio::test]
async fn killed_binary_before_approval_is_recovered_as_not_executed_without_replay() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("workspace")).unwrap();
    let (mut child, client) = start_offline_binary(&dir).await;
    let client = Arc::new(client);
    let session = client
        .request("session.create", json!({"label":"crash recovery"}))
        .await
        .unwrap();
    let id = session["id"].as_str().unwrap().to_owned();
    let run_client = client.clone();
    let run_id = id.clone();
    let prompt = tokio::spawn(async move {
        run_client
            .request(
                "session.prompt",
                json!({"session_id":run_id,"text":"pause at explicit approval"}),
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let snapshot = client
                .request("session.get", json!({"session_id":id}))
                .await
                .unwrap();
            if !snapshot["pending_approvals"].as_array().unwrap().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let before = client
        .request("session.tasks", json!({"session_id":id}))
        .await
        .unwrap();
    let task_id = before[0]["id"].clone();
    let pending = client
        .request("session.get", json!({"session_id":id}))
        .await
        .unwrap()["pending_approvals"][0]["id"]
        .clone();
    child.kill().await.unwrap();
    child.wait().await.unwrap();
    assert!(prompt.await.unwrap().is_err());
    client.close().await;
    let (mut restarted, client) = start_offline_binary(&dir).await;
    let tasks = client
        .request("session.tasks", json!({"session_id":id}))
        .await
        .unwrap();
    assert_eq!(tasks[0]["id"], task_id);
    assert_eq!(tasks[0]["state"], "running");
    // Durable Running/approval rows survive the owner. They cannot authorize a
    // new effect once the original process has released its task lock.
    assert!(
        client
            .request(
                "approval.decide",
                json!({
                    "session_id":id,"approval_id":pending,"decision":"allow_once"
                })
            )
            .await
            .is_err()
    );
    assert_eq!(
        client
            .request("session.tasks", json!({"session_id":id}))
            .await
            .unwrap(),
        tasks
    );
    assert!(!dir.path().join("workspace/cayw-demo.txt").exists());
    let health = client.request("runtime.health", json!({})).await.unwrap();
    assert_eq!(health["running"], 1);
    assert!(
        client
            .request("task.resume", json!({"task_id":task_id}))
            .await
            .is_err()
    );
    assert!(
        client
            .request(
                "session.prompt",
                json!({"session_id":id,"text":"no replay"})
            )
            .await
            .is_err()
    );
    let recovered = client
        .request("session.recover", json!({"session_id":id}))
        .await
        .unwrap();
    assert_eq!(recovered["state"], "idle");
    assert!(
        recovered["pending_approvals"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(recovered["unresolved_calls"].as_array().unwrap().is_empty());
    assert!(
        recovered["unresolved_capabilities"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let events = client
        .request(
            "session.events",
            json!({"session_id":id,"after":0,"limit":1000}),
        )
        .await
        .unwrap();
    assert!(events.as_array().unwrap().iter().any(|event| {
        event["event"]["type"] == "attempt_recovered"
            && event["event"]["capabilities"]
                .as_array()
                .unwrap()
                .iter()
                .any(|call| call["outcome"]["type"] == "not_executed")
    }));
    assert!(!dir.path().join("workspace/cayw-demo.txt").exists());
    let task = client
        .request("task.get", json!({"task_id":task_id}))
        .await
        .unwrap();
    assert_eq!(task["state"], "interrupted");
    assert_eq!(
        client.request("runtime.health", json!({})).await.unwrap()["running"],
        0
    );
    client.close().await;
    restarted.kill().await.unwrap();
    restarted.wait().await.unwrap();
}

struct PendingProvider;
impl cayw_ai::Provider for PendingProvider {
    fn stream(&self, _: cayw_ai::ModelRequest, _: CancellationToken) -> cayw_ai::AssistantStream {
        Box::pin(futures::stream::pending())
    }
}

#[tokio::test]
async fn disconnect_cancels_prompt_and_keeps_observable_durable_terminal() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("workspace")).unwrap();
    let rt = Runtime::open(
        dir.path().join("session.db"),
        dir.path().join("workspace"),
        Arc::new(PendingProvider),
        Model::new("fixture", "disconnect"),
        RuntimeConfig::default(),
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let shutdown = CancellationToken::new();
    let server = tokio::spawn(serve(
        listener,
        Arc::new(rt.clone()),
        ServerConfig::new("disconnect-fixture-secret"),
        shutdown.clone(),
    ));
    let client = Arc::new(
        Client::connect(address, "disconnect-fixture-secret")
            .await
            .unwrap(),
    );
    let id = rt.create(None).unwrap().id;
    let run_client = client.clone();
    let run_id = id.clone();
    let run = tokio::spawn(async move {
        run_client
            .request("session.prompt", json!({"session_id":run_id,"text":"wait"}))
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while rt.get(&id).unwrap().attempt.is_none() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    // A different request can still read this running session on the same connection.
    let during = client
        .request("session.get", json!({"session_id":id}))
        .await
        .unwrap();
    assert_eq!(during["state"], "running");
    client.close().await;
    assert!(run.await.unwrap().is_err());
    tokio::time::timeout(Duration::from_secs(5), rt.wait_idle(&id))
        .await
        .unwrap();
    assert!(rt.get(&id).unwrap().attempt.is_none());
    assert_eq!(
        rt.tasks(&id).unwrap()[0].state,
        cayw_runtime::TaskState::Cancelled
    );
    assert_eq!(rt.health().unwrap().running, 0);
    assert!(
        rt.audit(&id, 0, 100)
            .unwrap()
            .iter()
            .any(|record| record.kind == "run_cancelled")
    );
    shutdown.cancel();
    server.await.unwrap().unwrap();
}

#[tokio::test]
async fn approval_over_ipc_uses_fixed_host_identity_and_runs_without_blocking_events() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("workspace")).unwrap();
    let rt = Runtime::open(
        dir.path().join("session.db"),
        dir.path().join("workspace"),
        Arc::new(OfflineProvider::default()),
        Model::new("fixture", "rpc-approval"),
        RuntimeConfig::default(),
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let shutdown = CancellationToken::new();
    let server = tokio::spawn(serve(
        listener,
        Arc::new(rt.clone()),
        ServerConfig::new("rpc-approval-test-secret"),
        shutdown.clone(),
    ));
    let client = Arc::new(
        Client::connect(address, "rpc-approval-test-secret")
            .await
            .unwrap(),
    );
    let id = rt.create(None).unwrap().id;
    let request_id = id.clone();
    let request_client = client.clone();
    let prompt = tokio::spawn(async move {
        request_client
            .request(
                "session.prompt",
                json!({"session_id":request_id,"text":"write and read after approval"}),
            )
            .await
    });
    for _ in 0..2 {
        let approval = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(a) = rt.get(&id).unwrap().pending_approvals.first() {
                    break a.clone();
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let events = client
            .request("session.events", json!({"session_id":id,"after":0}))
            .await
            .unwrap();
        assert!(!events.as_array().unwrap().is_empty());
        let forged=client.request("approval.decide",json!({"session_id":id,"approval_id":approval.id,"decision":"allow_once","actor":"model"})).await.unwrap_err();
        assert_eq!(forged.code, "invalid_params");
        let decided = client
            .request(
                "approval.decide",
                json!({"session_id":id,"approval_id":approval.id,"decision":"allow_once"}),
            )
            .await
            .unwrap();
        assert_eq!(decided["decision"]["actor"], "rpc-user");
        assert!(decided["decision"]["decided_at"].as_u64().unwrap() > 0);
    }
    let result = prompt.await.unwrap().unwrap();
    assert_eq!(result["state"], "idle");
    assert_eq!(result["id"], id);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("workspace/cayw-demo.txt")).unwrap(),
        "headless runtime demo"
    );
    client.close().await;
    shutdown.cancel();
    server.await.unwrap().unwrap();
}
