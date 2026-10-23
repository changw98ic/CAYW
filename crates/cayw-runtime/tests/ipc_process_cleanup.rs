//! The authenticated local IPC path over the real process chain.
//!
//! The SDK matrix lives in `process_cleanup.rs` and is unchanged. This file adds
//! the one chain the frozen condition still needs on the *IPC* path: a real
//! authenticated client drives `session.prompt`, `session.cancel` and the event
//! and task queries, over both real I/O modes, and the same three-generation
//! chain must be recorded, attributed to its own task, and confirmed stopped.
//!
//! Everything below the transport is the same product code the SDK tests use:
//! the same Runtime, Host, Device and process fixture. Only the caller changes.
#![cfg(target_os = "macos")]
use cayw_ai::{AssistantEvent, AssistantStream, CancellationToken, Model, ModelRequest, Provider};
use cayw_host::{Profile, ProfileKind};
use cayw_ipc::{Client, ServerConfig, serve};
use cayw_runtime::{Runtime, RuntimeConfig, ScopedPermission};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path, sync::Arc, time::Duration};

const TOKEN: &str = "runtime-ipc-chain-secret";
const WATCHDOG: Duration = Duration::from_secs(60);

/// The same real chain provider `process_cleanup.rs` drives: one real
/// parent/child/grandchild chain through the real process profile.
struct ChainProvider;
impl Provider for ChainProvider {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let message = cayw_runtime::fixture::script_response(
            &request,
            "return cayw.call('test.run',{profile='chain'})",
            &format!("chain-{}", request.messages.len()),
        );
        Box::pin(futures::stream::iter([AssistantEvent::Done { message }]))
    }
}

fn config(root: &Path, io: cayw_host::ProcessIo) -> RuntimeConfig {
    let mut config = RuntimeConfig {
        max_concurrent_runs: 1,
        max_queued_runs: 1,
        max_run_time: Duration::from_secs(90),
        ..Default::default()
    };
    config.codemode.timeout = Duration::from_secs(60);
    config.host.call_timeout = Duration::from_secs(60);
    config.host.profiles.insert(
        "chain".into(),
        Profile {
            kind: ProfileKind::Test,
            program: env!("CARGO_BIN_EXE_cayw-runtime-process-fixture").into(),
            args: vec![
                "chain".into(),
                root.to_string_lossy().into_owned(),
                "0".into(),
            ],
            env: BTreeMap::new(),
            timeout: Duration::from_secs(60),
            max_output_bytes: 4096,
            allow_filter: false,
            io,
        },
    );
    config.permissions = vec![ScopedPermission::exact(
        "test.run",
        json!({"profile":"chain"}),
    )];
    config
}

async fn marker(path: &Path) -> String {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Ok(value) = std::fs::read_to_string(path)
                && !value.is_empty()
            {
                return value;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the fixture must write its readiness marker")
}

fn live(pid: i64) -> bool {
    let output = std::process::Command::new("/bin/ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    output.status.success()
        && String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .any(|state| !state.starts_with('Z'))
}

async fn wait_stopped(pid: i64) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while live(pid) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("every generation must stop");
}

#[derive(Debug)]
struct Row {
    task: String,
    session: String,
    attempt: String,
    worker: String,
    fence: i64,
    execution: String,
    sequence: i64,
    pid: i64,
    parent_pid: i64,
    process_group: i64,
    started_seconds: i64,
    started_microseconds: i64,
    state: String,
}

fn identities(dir: &Path) -> Vec<Row> {
    let connection = rusqlite::Connection::open(dir.join("session.db")).unwrap();
    let mut statement = connection
        .prepare(
            "SELECT task,session,attempt,worker,fence,execution,sequence,pid,parent_pid,
            process_group,started_seconds,started_microseconds,state
            FROM runtime_process_identities ORDER BY pid",
        )
        .unwrap();
    statement
        .query_map([], |row| {
            Ok(Row {
                task: row.get(0)?,
                session: row.get(1)?,
                attempt: row.get(2)?,
                worker: row.get(3)?,
                fence: row.get(4)?,
                execution: row.get(5)?,
                sequence: row.get(6)?,
                pid: row.get(7)?,
                parent_pid: row.get(8)?,
                process_group: row.get(9)?,
                started_seconds: row.get(10)?,
                started_microseconds: row.get(11)?,
                state: row.get(12)?,
            })
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

async fn wait_recorded(dir: &Path, pids: &[i64]) {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let rows = identities(dir);
            if pids
                .iter()
                .all(|pid| rows.iter().any(|row| row.pid == *pid))
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("every generation must be durably recorded before the cancel");
}

/// The authenticated local IPC path, over the real three-generation chain, in
/// both real I/O modes.
///
/// The client is a real authenticated peer on loopback TCP: it creates the
/// session, runs the prompt, cancels it, and reads the events and tasks back
/// through the same wire protocol a UI would use. The chain itself is the same
/// real parent/child/grandchild fixture, and the assertions are the ones the
/// frozen condition needs from the *public* chain: every generation recorded
/// against its own task and original call, every generation stopped, exactly one
/// confirmed cleanup, and the slot released only after it.
#[tokio::test]
async fn authenticated_ipc_cancel_stops_and_records_every_chain_generation_in_both_io_modes() {
    let mut modes = vec![("pipes", cayw_host::ProcessIo::Pipes)];
    #[cfg(target_os = "macos")]
    modes.push((
        "pty",
        cayw_host::ProcessIo::Pty {
            columns: 80,
            rows: 24,
        },
    ));
    for (io_label, io) in modes {
        let label = format!("{io_label}/ipc-cancel");
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("workspace")).unwrap();
        let rt = Runtime::open(
            dir.path().join("session.db"),
            dir.path().join("workspace"),
            Arc::new(ChainProvider),
            Model::new("fixture", "process-owner"),
            config(dir.path(), io),
        )
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let stop = CancellationToken::new();
        let mut server_config = ServerConfig::new(TOKEN);
        server_config.request_timeout = Duration::from_secs(90);
        let server = tokio::spawn(serve(
            listener,
            Arc::new(rt.clone()),
            server_config,
            stop.clone(),
        ));
        // Two authenticated peers on the same Runtime: one owns the blocking
        // prompt, the other issues the cancel and the queries, exactly as a UI
        // and a separate control path would.
        let prompt_peer = Client::connect(address, TOKEN).await.unwrap();
        let control_peer = Client::connect(address, TOKEN).await.unwrap();
        let health = control_peer
            .request("runtime.health", json!({}))
            .await
            .unwrap();
        assert_eq!(health["status"], "ok");
        assert_eq!(health["protocol_version"], cayw_core::PROTOCOL_VERSION);

        let session = control_peer
            .request("session.create", json!({"label":"ipc chain"}))
            .await
            .unwrap();
        let id = session["id"].as_str().unwrap().to_owned();

        let prompt = {
            let peer = prompt_peer.clone();
            let id = id.clone();
            tokio::spawn(async move {
                peer.request(
                    "session.prompt",
                    json!({"session_id":id,"text":"run a chain"}),
                )
                .await
            })
        };
        let mut pids = Vec::new();
        for depth in 0..3 {
            pids.push(
                marker(&dir.path().join(format!("p{depth}.pid")))
                    .await
                    .trim()
                    .parse::<i64>()
                    .unwrap(),
            );
        }
        wait_recorded(dir.path(), &pids).await;
        let observed: Vec<_> = pids
            .iter()
            .map(|pid| cayw_host::ProcessIdentity::observe((*pid).try_into().unwrap()).unwrap())
            .collect();

        // The task as the wire reports it, before the cancel.
        let tasks = control_peer
            .request("session.tasks", json!({"session_id":id}))
            .await
            .unwrap();
        let tasks = tasks.as_array().unwrap();
        assert_eq!(tasks.len(), 1, "{label}: exactly one task");
        let task_id = tasks[0]["id"].as_str().unwrap().to_owned();
        let attempt = tasks[0]["attempt_id"].as_str().unwrap().to_owned();
        assert_eq!(tasks[0]["state"], "running");

        let original_task: cayw_runtime::TaskRecord =
            serde_json::from_value(tasks[0].clone()).unwrap();
        let cancelled = control_peer
            .request(
                "session.cancel",
                serde_json::to_value(original_task.cancel_target()).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(cancelled["cancelled"], true, "{label}: cancel accepted");

        let reply = tokio::time::timeout(WATCHDOG, prompt)
            .await
            .expect("the IPC prompt must return")
            .unwrap();
        let view: Value = reply.unwrap_or_else(|error| {
            panic!("the cancelled IPC prompt must still return its session view: {error:?}")
        });
        for pid in &pids {
            wait_stopped(*pid).await;
        }
        assert!(
            pids.iter().all(|pid| !live(*pid)),
            "{label}: every generation must be stopped, not abandoned"
        );

        // Queried back over the wire, not from a local Runtime handle.
        let events = control_peer
            .request(
                "session.events",
                json!({"session_id":id,"after":0,"limit":1000}),
            )
            .await
            .unwrap();
        let audit = control_peer
            .request(
                "session.audit",
                json!({"session_id":id,"after":0,"limit":1000}),
            )
            .await
            .unwrap();
        let tasks_after = control_peer
            .request("session.tasks", json!({"session_id":id}))
            .await
            .unwrap();

        let rows = identities(dir.path());
        let leader = rows
            .iter()
            .find(|row| row.pid == pids[0])
            .expect("the leader must be recorded");
        for (index, pid) in pids.iter().enumerate() {
            let row = rows
                .iter()
                .find(|row| row.pid == *pid)
                .unwrap_or_else(|| panic!("{label}: generation {pid} is not recorded"));
            assert_eq!(row.task, task_id, "{label}: wrong task attribution");
            assert_eq!(row.session, id, "{label}: wrong session");
            assert_eq!(row.attempt, attempt, "{label}: wrong attempt");
            assert_eq!(row.worker, leader.worker, "{label}: different worker");
            assert_eq!(row.fence, leader.fence, "{label}: different fence");
            assert_eq!(row.execution, leader.execution, "{label}: different cell");
            assert_eq!(row.sequence, leader.sequence, "{label}: different call");
            assert_eq!(row.started_seconds, observed[index].started_seconds as i64);
            assert_eq!(
                row.started_microseconds,
                i64::from(observed[index].started_microseconds)
            );
            assert_eq!(row.parent_pid, i64::from(observed[index].parent_pid));
            assert_eq!(row.process_group, i64::from(observed[index].process_group));
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    match cayw_host::ProcessIdentity::observe(observed[index].pid) {
                        Err(error) => {
                            assert_eq!(
                                error.raw_os_error(),
                                Some(3),
                                "{label}: unreadable identity is not cleanup evidence: {error}"
                            );
                            break;
                        }
                        Ok(current)
                            if current.started_seconds != observed[index].started_seconds
                                || current.started_microseconds
                                    != observed[index].started_microseconds =>
                        {
                            break;
                        }
                        Ok(_) => tokio::time::sleep(Duration::from_millis(10)).await,
                    }
                }
            })
            .await
            .expect("the original IPC process incarnation must be gone");
            assert_eq!(
                row.process_group, pids[0],
                "{label}: generation {index} is outside the managed process group"
            );
            assert_eq!(
                row.state, "stopped",
                "{label}: generation {index} not stopped"
            );
        }
        assert_eq!(
            rows.iter()
                .find(|row| row.pid == pids[1])
                .unwrap()
                .parent_pid,
            pids[0],
            "{label}: the child's real parent is not the leader"
        );
        assert_eq!(
            rows.iter()
                .find(|row| row.pid == pids[2])
                .unwrap()
                .parent_pid,
            pids[1],
            "{label}: the grandchild's real parent is not the child"
        );

        let stopped: Vec<_> = audit
            .as_array()
            .unwrap()
            .iter()
            .filter(|event| event["kind"] == "processes_stopped")
            .collect();
        assert_eq!(stopped.len(), 1, "{label}: exactly one confirmed cleanup");
        assert_eq!(stopped[0]["task_id"], task_id);
        assert_eq!(stopped[0]["data"]["state"], "drained");
        let after_health = control_peer
            .request("runtime.health", json!({}))
            .await
            .unwrap();
        assert_eq!(
            after_health["running"], 0,
            "{label}: the slot must be released only after confirmed cleanup"
        );
        let terminals: Vec<_> = audit
            .as_array()
            .unwrap()
            .iter()
            .filter(|event| {
                matches!(
                    event["kind"].as_str(),
                    Some("run_completed" | "run_cancelled" | "run_outcome_unknown" | "run_failed")
                )
            })
            .collect();
        assert_eq!(terminals.len(), 1, "{label}: exactly one terminal");
        let task_after = tasks_after
            .as_array()
            .unwrap()
            .iter()
            .find(|task| task["id"] == task_id.as_str())
            .cloned()
            .expect("the task must still be listed");
        assert_ne!(task_after["state"], "running");
        assert_eq!(
            events
                .as_array()
                .unwrap()
                .iter()
                .filter(|event| event["event"]["type"] == "capability_settled")
                .count(),
            1,
            "{label}: the original call must settle exactly once"
        );

        eprintln!(
            "IPC_CHAIN_CANCEL {}",
            json!({
                "io": io_label,
                "session": id,
                "task": task_id,
                "attempt": attempt,
                "identities": rows.iter().filter(|row| row.task == task_id).map(|row| json!({
                    "pid": row.pid, "parent": row.parent_pid, "group": row.process_group,
                    "started_seconds": row.started_seconds,
                    "started_microseconds": row.started_microseconds,
                    "worker": row.worker, "fence": row.fence,
                    "execution": row.execution, "sequence": row.sequence,
                    "state": row.state, "original_incarnation_gone": true,
                })).collect::<Vec<_>>(),
                "prompt_view_state": view["state"],
                "terminal": task_after["state"],
                "cleanup_state": stopped[0]["data"]["state"],
                "cleanup_events": stopped.len(),
                "run_terminals": terminals.len(),
                "running": after_health["running"],
                "transport": "authenticated loopback IPC (hello + session.prompt/session.cancel/session.events/session.audit/session.tasks)",
            })
        );

        prompt_peer.close().await;
        control_peer.close().await;
        stop.cancel();
        server.await.unwrap().unwrap();
    }
}
