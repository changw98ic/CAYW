#![cfg(unix)]
use cayw_ai::{AssistantEvent, AssistantStream, CancellationToken, Model, ModelRequest, Provider};
use cayw_core::{CoreEvent, SessionState};
use cayw_host::{Profile, ProfileKind};
use cayw_runtime::{Runtime, RuntimeConfig, RuntimeError, ScopedPermission, TaskState};
use serde_json::json;
use std::{collections::BTreeMap, path::Path, sync::Arc, time::Duration};

struct ProfileProvider;
impl Provider for ProfileProvider {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let message = cayw_runtime::fixture::script_response(
            &request,
            "return cayw.call('test.run',{profile='tree'})",
            &format!("process-{}", request.messages.len()),
        );
        Box::pin(futures::stream::iter([AssistantEvent::Done { message }]))
    }
}
fn settings(root: &Path) -> RuntimeConfig {
    let mut config = RuntimeConfig {
        max_concurrent_runs: 1,
        max_queued_runs: 1,
        max_run_time: Duration::from_secs(90),
        ..Default::default()
    };
    config.codemode.timeout = Duration::from_secs(60);
    config.host.call_timeout = Duration::from_secs(60);
    config.host.profiles.insert(
        "tree".into(),
        Profile {
            kind: ProfileKind::Test,
            program: env!("CARGO_BIN_EXE_cayw-runtime-process-fixture").into(),
            args: vec!["tree".into(), root.to_string_lossy().into_owned()],
            env: BTreeMap::new(),
            timeout: Duration::from_secs(60),
            max_output_bytes: 4096,
            allow_filter: false,
        },
    );
    config.permissions = vec![ScopedPermission::exact(
        "test.run",
        json!({"profile":"tree"}),
    )];
    config
}
fn open(root: &Path) -> Runtime {
    Runtime::open(
        root.join("session.db"),
        root.join("workspace"),
        Arc::new(ProfileProvider),
        Model::new("fixture", "process-owner"),
        settings(root),
    )
    .unwrap()
}
async fn marker(path: &Path) -> String {
    tokio::time::timeout(Duration::from_secs(10), async {
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
    .unwrap()
}
fn live(pid: &str) -> bool {
    let output = std::process::Command::new("/bin/ps")
        .args(["-o", "stat=", "-p", pid])
        .output()
        .unwrap();
    output.status.success()
        && String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .any(|state| !state.starts_with('Z'))
}
async fn wait_stopped(pid: &str) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while live(pid) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn cancelled_profile_joins_process_tree_before_freeing_the_task_slot() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("workspace")).unwrap();
    let rt = open(dir.path());
    let peer = open(dir.path());
    let id = rt.create(None).unwrap().id;
    let worker = {
        let rt = rt.clone();
        let id = id.clone();
        tokio::spawn(async move {
            rt.prompt(&id, "run process tree", CancellationToken::new())
                .await
        })
    };
    let leader = marker(&dir.path().join("leader.pid")).await;
    let leaf = marker(&dir.path().join("leaf.pid")).await;
    assert!(live(&leader) && live(&leaf));
    let task = rt.tasks(&id).unwrap().remove(0);
    assert_eq!(peer.health().unwrap().running, 1);
    assert!(matches!(peer.recover(&id), Err(RuntimeError::Busy)));
    assert!(peer.cancel(&id).unwrap());
    let _ = tokio::time::timeout(Duration::from_secs(10), worker)
        .await
        .unwrap()
        .unwrap();
    wait_stopped(&leader).await;
    wait_stopped(&leaf).await;
    assert!(!live(&leader) && !live(&leaf));
    assert_eq!(rt.health().unwrap().running, 0);
    assert!(matches!(
        rt.task(&task.id).unwrap().state,
        TaskState::Cancelled | TaskState::OutcomeUnknown
    ));
    let audit = rt.audit(&id, 0, 1000).unwrap();
    let stopped = audit
        .iter()
        .find(|event| event.kind == "processes_stopped")
        .unwrap();
    let terminal = audit
        .iter()
        .find(|event| matches!(event.kind.as_str(), "run_cancelled" | "run_outcome_unknown"))
        .unwrap();
    assert!(stopped.cursor < terminal.cursor);
    assert_eq!(stopped.task_id.as_ref(), Some(&task.id));
    assert_eq!(stopped.data["state"], "drained");
}

#[tokio::test]
async fn dropping_prompt_keeps_the_owned_worker_until_process_cleanup_finishes() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("workspace")).unwrap();
    let rt = open(dir.path());
    let peer = open(dir.path());
    let id = rt.create(None).unwrap().id;
    let worker = {
        let rt = rt.clone();
        let id = id.clone();
        tokio::spawn(async move {
            rt.prompt(
                &id,
                "drop the caller, not process ownership",
                CancellationToken::new(),
            )
            .await
        })
    };
    let leader = marker(&dir.path().join("leader.pid")).await;
    let leaf = marker(&dir.path().join("leaf.pid")).await;
    let task = rt.tasks(&id).unwrap().remove(0);
    assert_eq!(peer.health().unwrap().running, 1);
    worker.abort();
    assert!(worker.await.unwrap_err().is_cancelled());
    tokio::time::timeout(Duration::from_secs(10), rt.wait_idle(&id))
        .await
        .unwrap();
    wait_stopped(&leader).await;
    wait_stopped(&leaf).await;
    assert_eq!(peer.health().unwrap().running, 0);
    let audit = peer.audit(&id, 0, 1000).unwrap();
    let stopped = audit
        .iter()
        .find(|event| event.kind == "processes_stopped")
        .unwrap();
    let terminal = audit
        .iter()
        .find(|event| matches!(event.kind.as_str(), "run_cancelled" | "run_outcome_unknown"))
        .unwrap();
    assert!(stopped.cursor < terminal.cursor);
    assert_eq!(stopped.task_id.as_ref(), Some(&task.id));
    assert_eq!(stopped.data["state"], "drained");
}

struct PanicAfterLaunch {
    root: std::path::PathBuf,
}
impl Provider for PanicAfterLaunch {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        if matches!(
            request.messages.last(),
            Some(cayw_ai::Message::ToolResult(_))
        ) {
            let root = self.root.clone();
            return Box::pin(futures::stream::once(async move {
                marker(&root.join("leader.pid")).await;
                marker(&root.join("leaf.pid")).await;
                panic!("injected provider panic while a process cell is running");
            }));
        }
        let mut message = cayw_ai::AssistantMessage::new(&request.model);
        message
            .content
            .push(cayw_ai::Content::ToolCall(cayw_ai::ToolCall::new(
                "panic-launch",
                "execute_code",
                json!({
                    "action":"exec", "script":"return cayw.call('test.run',{profile='tree'})",
                    "yield_time_ms":0, "max_output_tokens":4096,
                }),
            )));
        message.stop_reason = cayw_ai::StopReason::ToolUse;
        Box::pin(futures::stream::iter([AssistantEvent::Done { message }]))
    }
}

#[tokio::test]
async fn provider_panic_preserves_unknown_effects_and_drains_the_original_process_owner() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("workspace")).unwrap();
    let rt = Runtime::open(
        dir.path().join("session.db"),
        dir.path().join("workspace"),
        Arc::new(PanicAfterLaunch {
            root: dir.path().into(),
        }),
        Model::new("fixture", "process-owner"),
        settings(dir.path()),
    )
    .unwrap();
    let id = rt.create(None).unwrap().id;
    let view = tokio::time::timeout(
        Duration::from_secs(10),
        rt.prompt(
            &id,
            "panic with live process work",
            CancellationToken::new(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(view.state, SessionState::NeedsReconciliation);
    assert!(view.messages.iter().any(|message| matches!(message,
        cayw_ai::Message::Assistant(answer)
        if answer.error_message.as_deref() == Some("Provider stream panicked"))));
    assert!(!view.unresolved_capabilities.is_empty());
    let leader = marker(&dir.path().join("leader.pid")).await;
    let leaf = marker(&dir.path().join("leaf.pid")).await;
    wait_stopped(&leader).await;
    wait_stopped(&leaf).await;
    let task = rt.tasks(&id).unwrap().remove(0);
    assert_eq!(task.state, TaskState::OutcomeUnknown);
    assert_eq!(rt.health().unwrap().running, 0);
    let audit = rt.audit(&id, 0, 1000).unwrap();
    let stopped = audit
        .iter()
        .find(|event| event.kind == "processes_stopped")
        .unwrap();
    let terminal = audit
        .iter()
        .find(|event| event.kind == "run_outcome_unknown")
        .unwrap();
    assert!(stopped.cursor < terminal.cursor);
    assert_eq!(stopped.task_id.as_ref(), Some(&task.id));
    assert_eq!(stopped.data["state"], "drained");
    assert!(
        rt.enqueue(&id, "do not replay provider panic effects")
            .is_err()
    );
    let recovered = rt.recover(&id).unwrap();
    assert_eq!(recovered.state, SessionState::NeedsReconciliation);
    assert_eq!(rt.task(&task.id).unwrap().state, TaskState::OutcomeUnknown);
}

#[tokio::test]
async fn failed_cleanup_record_keeps_capacity_until_the_original_owner_retries() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("workspace")).unwrap();
    let rt = open(dir.path());
    let peer = open(dir.path());
    let id = rt.create(None).unwrap().id;
    let other = rt.create(None).unwrap().id;
    let db = rusqlite::Connection::open(dir.path().join("session.db")).unwrap();
    db.execute_batch(
        "CREATE TRIGGER fail_process_terminal BEFORE INSERT ON runtime_audit
        WHEN NEW.kind='processes_stopped'
        BEGIN SELECT RAISE(ABORT, 'injected process cleanup recording failure'); END;",
    )
    .unwrap();
    let worker = {
        let rt = rt.clone();
        let id = id.clone();
        tokio::spawn(async move {
            rt.prompt(
                &id,
                "retain the original process owner",
                CancellationToken::new(),
            )
            .await
        })
    };
    let leader = marker(&dir.path().join("leader.pid")).await;
    let leaf = marker(&dir.path().join("leaf.pid")).await;
    let task = rt.tasks(&id).unwrap().remove(0);
    peer.cancel(&id).unwrap();
    let error = tokio::time::timeout(Duration::from_secs(10), worker)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("injected process cleanup recording failure")
    );
    assert!(matches!(
        error,
        RuntimeError::RecoveryRequired {
            outcome_unknown: true,
            ..
        }
    ));
    wait_stopped(&leader).await;
    wait_stopped(&leaf).await;
    assert_eq!(rt.task(&task.id).unwrap().state, TaskState::Draining);
    assert_eq!(peer.health().unwrap().running, 1);
    assert!(matches!(
        peer.enqueue(&other, "capacity must remain held"),
        Err(RuntimeError::QueueFull)
    ));
    let before = peer.get(&id).unwrap();
    assert!(matches!(
        peer.recover(&id),
        Err(RuntimeError::RecoveryRequired { .. })
    ));
    assert_eq!(peer.get(&id).unwrap(), before);
    assert!(peer.drain_task_processes(&task.id).await.is_err());
    assert!(
        !rt.audit(&id, 0, 1000)
            .unwrap()
            .iter()
            .any(|event| event.kind == "processes_stopped")
    );
    db.execute_batch("DROP TRIGGER fail_process_terminal")
        .unwrap();
    // A concurrent test's fork may briefly inherit a task lock before exec.
    // Busy preserves ownership; retry only that admission refusal and retain
    // both original cleanup calls and the held-capacity assertion below.
    tokio::time::timeout(Duration::from_secs(10), async {
        for _ in 0..2 {
            loop {
                match rt.drain_task_processes(&task.id).await {
                    Err(RuntimeError::Busy) => tokio::task::yield_now().await,
                    result => {
                        result.unwrap();
                        break;
                    }
                }
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(
        rt.health().unwrap().running,
        1,
        "drain alone must not release admission"
    );
    let recovered = peer.recover(&id).unwrap();
    assert!(recovered.attempt.is_none());
    assert_eq!(peer.health().unwrap().running, 0);
    let audit = peer.audit(&id, 0, 1000).unwrap();
    let stopped: Vec<_> = audit
        .iter()
        .filter(|event| event.kind == "processes_stopped")
        .collect();
    assert_eq!(stopped.len(), 1);
    assert_eq!(stopped[0].data["state"], "drained");
    assert_eq!(stopped[0].task_id.as_ref(), Some(&task.id));
}

#[tokio::test]
async fn killed_host_requires_observed_process_cleanup_before_recovery_releases_capacity() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("workspace")).unwrap();
    let mut child =
        tokio::process::Command::new(env!("CARGO_BIN_EXE_cayw-runtime-process-fixture"))
            .arg("host")
            .arg(dir.path())
            .kill_on_drop(true)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
    let id = marker(&dir.path().join("session.id")).await;
    let leader = marker(&dir.path().join("leader.pid")).await;
    let leaf = marker(&dir.path().join("leaf.pid")).await;
    assert!(live(&leader) && live(&leaf));
    child.kill().await.unwrap();
    child.wait().await.unwrap();
    let rt = open(dir.path());
    let task = rt.tasks(&id).unwrap().remove(0);
    let before = rt.get(&id).unwrap();
    assert_eq!(rt.health().unwrap().running, 1);
    let error = rt.recover(&id).unwrap_err();
    assert!(matches!(
        error,
        RuntimeError::RecoveryRequired {
            outcome_unknown: true,
            ..
        }
    ));
    assert_eq!(
        rt.get(&id).unwrap(),
        before,
        "recovery must not fence Core before process cleanup"
    );
    assert_eq!(rt.task(&task.id).unwrap(), task);
    assert!(
        rt.drain_task_processes(&task.id).await.is_err(),
        "a new Host cannot prove old cleanup"
    );
    assert_eq!(rt.health().unwrap().running, 1);
    // The trusted test host releases its own fixture, observes both actual PIDs
    // stopped, then submits evidence. Runtime never infers that from a file lock.
    std::fs::write(dir.path().join("release"), b"explicit fixture cleanup").unwrap();
    wait_stopped(&leader).await;
    wait_stopped(&leaf).await;
    rt.confirm_processes_stopped(&task.id, "fixture-host", &format!(
        "Released the original fixture and observed leader {leader} and child {leaf} absent or zombie via ps",
    )).unwrap();
    assert_eq!(
        rt.health().unwrap().running,
        1,
        "cleanup evidence alone does not release the task"
    );
    let recovered = rt.recover(&id).unwrap();
    assert_eq!(recovered.state, SessionState::NeedsReconciliation);
    assert!(!recovered.unresolved_capabilities.is_empty());
    assert_eq!(rt.health().unwrap().running, 0);
    assert_eq!(rt.task(&task.id).unwrap().state, TaskState::Interrupted);
    assert!(
        rt.events(&id, 0, 1000)
            .unwrap()
            .iter()
            .any(|event| matches!(event.event, CoreEvent::AttemptRecovered { .. }))
    );
    assert!(rt.enqueue(&id, "do not replay process effects").is_err());
    assert_eq!(
        rt.audit(&id, 0, 1000)
            .unwrap()
            .iter()
            .filter(|event| event.kind == "processes_stopped")
            .count(),
        1
    );
}
