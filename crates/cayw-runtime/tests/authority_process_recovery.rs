//! Combined loss of the Requests authority and its original process supervisor.
#![cfg(target_os = "macos")]
use cayw_ai::{AssistantEvent, AssistantStream, CancellationToken, Model, ModelRequest, Provider};
use cayw_requests::{AuthorityRepairRequest, HostActor, OperationScope, RequestError};
use cayw_runtime::{
    CancelTarget, RequestPolicy, ResourcePolicy, Runtime, RuntimeConfig, RuntimeError,
    ScopedPermission, TaskInput, TaskPriority, TaskRecord, TaskResources,
};
use serde_json::json;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};

const CHILD_ROOT: &str = "CAYW_AUTHORITY_PROCESS_CHILD_ROOT";
struct Offline;
impl Provider for Offline {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let message = cayw_runtime::fixture::script_response(
            &request,
            "return cayw.call('test.run',{profile='tree'})",
            "original-tree",
        );
        Box::pin(futures::stream::iter([AssistantEvent::Done { message }]))
    }
}
fn config(root: &Path) -> RuntimeConfig {
    let mut config = RuntimeConfig {
        max_concurrent_runs: 1,
        max_queued_runs: 1,
        max_run_time: Duration::from_secs(180),
        resources: ResourcePolicy {
            max_declared_memory_bytes: 100,
        },
        request_policy: Some(RequestPolicy {
            permission_policy_id: "combined-recovery".into(),
            operations: vec![OperationScope::exact(
                "unused-read",
                "fs.read",
                json!({"path":"unused.txt"}),
                "combined-recovery",
            )],
        }),
        permissions: vec![ScopedPermission::exact(
            "test.run",
            json!({"profile":"tree"}),
        )],
        ..Default::default()
    };
    config.codemode.timeout = Duration::from_secs(120);
    config.host.call_timeout = Duration::from_secs(120);
    config.host.profiles.insert(
        "tree".into(),
        cayw_host::Profile {
            kind: cayw_host::ProfileKind::Test,
            program: env!("CARGO_BIN_EXE_cayw-runtime-process-fixture").into(),
            args: vec!["tree".into(), root.to_string_lossy().into_owned()],
            env: BTreeMap::new(),
            timeout: Duration::from_secs(120),
            max_output_bytes: 4096,
            allow_filter: false,
            io: cayw_host::ProcessIo::Pipes,
        },
    );
    config
}
fn open(root: &Path) -> Result<Runtime, RuntimeError> {
    Runtime::open(
        root.join("session.sqlite"),
        root.join("workspace"),
        Arc::new(Offline),
        Model::new("fixture", "combined-recovery"),
        config(root),
    )
}
fn enqueue_resources(runtime: &Runtime, session: &str, text: &str) -> TaskRecord {
    runtime
        .enqueue_with_resources(
            session,
            TaskInput {
                text: text.into(),
                images: vec![],
            },
            cayw_requests::TaskIntent::General,
            TaskPriority::Normal,
            TaskResources {
                declared_memory_bytes: 80,
                ..Default::default()
            },
        )
        .unwrap()
}
fn assert_resources(runtime: &Runtime, original: &TaskRecord, reserved: bool) {
    let view = runtime.resources().unwrap();
    assert_eq!(view.policy.max_declared_memory_bytes, 100);
    assert_eq!(view.usage.max_declared_memory_bytes, 100);
    assert_eq!(
        view.usage.reserved_declared_memory_bytes,
        if reserved { 80 } else { 0 }
    );
    assert_eq!(view.usage.holding_tasks, usize::from(reserved));
    let task = runtime.task(&original.id).unwrap();
    assert_eq!(task.id, original.id);
    assert_eq!(task.session_id, original.session_id);
    assert_eq!(task.resources.declared_memory_bytes, 80);
    assert_eq!(task.holds_slot, reserved);
    if original.worker_id.is_some() {
        assert_eq!(task.worker_id, original.worker_id);
        assert_eq!(task.attempt_id, original.attempt_id);
    }
    eprintln!(
        "AUTHORITY_RESOURCE_STATE {}",
        json!({"task":task,"usage":view.usage})
    );
}
fn assert_persisted_resources(root: &Path, original: &TaskRecord) {
    let database = rusqlite::Connection::open(root.join("session.sqlite")).unwrap();
    let row: (String, String, Option<String>, Option<String>, i64, i64, bool) = database.query_row(
        "SELECT id,session,worker,attempt,fence,declared_memory_bytes,holds_slot FROM runtime_tasks WHERE id=?1",
        [&original.id], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?,row.get(6)?)),
    ).unwrap();
    assert_eq!(
        row,
        (
            original.id.clone(),
            original.session_id.clone(),
            original.worker_id.clone(),
            original.attempt_id.clone(),
            i64::try_from(original.fence).unwrap(),
            80,
            true
        )
    );
    let total: i64 = database
        .query_row(
            "SELECT max_declared_memory_bytes FROM runtime_queue_config WHERE singleton=1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(total, 100);
    eprintln!(
        "AUTHORITY_RESOURCE_PERSISTED {}",
        json!({"target":original.cancel_target(),"policy":total,"reserved":row.5,"holds_slot":row.6})
    );
}
fn retry_authority<T>(
    label: &str,
    mut operation: impl FnMut() -> Result<T, RuntimeError>,
    mut check_retry: impl FnMut(),
) -> T {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match operation() {
            Err(RuntimeError::Request(RequestError::Database(rusqlite::Error::SqliteFailure(
                error,
                Some(message),
            )))) if error.code == rusqlite::ErrorCode::DatabaseBusy
                && message == "request authority is busy" =>
            {
                // Only lock acquisition failed. Do not retry a stopped authority,
                // SQLite transaction failure, or a cancellation already committed.
                check_retry();
                eprintln!("AUTHORITY_CONTENTION_RETRY operation={label}");
                assert!(Instant::now() < deadline, "authority stayed busy: {label}");
                std::thread::yield_now();
            }
            result => return result.unwrap_or_else(|error| panic!("{label}: {error:?}")),
        }
    }
}
fn open_when_available(root: &Path) -> Runtime {
    retry_authority("open original authority", || open(root), || {})
}
fn cancel_when_available(runtime: &Runtime, target: &CancelTarget) -> bool {
    retry_authority(
        "cancel original task",
        || runtime.cancel(target),
        || {
            let observed = runtime.task(&target.task_id).unwrap();
            eprintln!("AUTHORITY_CANCEL_AFTER_BUSY task={observed:?}");
            assert_eq!(observed.cancel_target(), *target);
            assert_eq!(observed.state, cayw_runtime::TaskState::Running);
            assert!(
                !observed.cancel_requested,
                "cancellation committed before its authority error; do not retry"
            );
        },
    )
}
fn wait_file(path: &Path) -> String {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(value) = std::fs::read_to_string(path)
            && !value.is_empty()
        {
            return value;
        }
        assert!(
            Instant::now() < deadline,
            "fixture did not reach {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}
struct Original {
    child: Child,
    root: PathBuf,
}
impl Drop for Original {
    fn drop(&mut self) {
        let _ = std::fs::write(self.root.join("release"), b"owned fixture cleanup");
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
#[test]
fn authority_process_child() {
    let Some(root) = std::env::var_os(CHILD_ROOT).map(PathBuf::from) else {
        return;
    };
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let runtime = open(&root).unwrap();
            let session = runtime.create(None).unwrap().id;
            let task = enqueue_resources(
                &runtime,
                &session,
                "original process with Requests authority",
            );
            std::fs::write(
                root.join("identity.json"),
                serde_json::to_vec(&task).unwrap(),
            )
            .unwrap();
            runtime
                .resume(&task.id, CancellationToken::new())
                .await
                .unwrap();
        });
}
fn stopped(identity: &cayw_host::ProcessIdentity) -> bool {
    match cayw_host::ProcessIdentity::observe(identity.pid) {
        Err(error) if error.raw_os_error() == Some(3) => true,
        Err(error) => panic!("original process identity is unreadable: {error}"),
        Ok(current)
            if current.started_seconds != identity.started_seconds
                || current.started_microseconds != identity.started_microseconds =>
        {
            true
        }
        Ok(_) => {
            let state = Command::new("/bin/ps")
                .args(["-o", "stat=", "-p", &identity.pid.to_string()])
                .output()
                .unwrap();
            state.status.success()
                && String::from_utf8_lossy(&state.stdout)
                    .trim_start()
                    .starts_with('Z')
        }
    }
}
fn inspection(root: &Path) -> cayw_requests::AuthorityInspection {
    Runtime::inspect_request_authority(root.join("session.sqlite"), config(root).request_limits)
        .unwrap()
}
fn request(root: &Path) -> AuthorityRepairRequest {
    AuthorityRepairRequest::new(
        inspection(root),
        uuid::Uuid::new_v4().to_string(),
        HostActor::new("combined-recovery-host").unwrap(),
        "Original Task lock, stopped Cells scope and independently observed process cleanup",
    )
    .unwrap()
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_live_original_host_keeps_ownership_after_its_cleanup_record_fails() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    std::fs::create_dir(root.join("workspace")).unwrap();
    let runtime = open(root).unwrap();
    let peer = open(root).unwrap();
    let session = runtime.create(None).unwrap().id;
    let other = runtime.create(None).unwrap().id;
    let database = rusqlite::Connection::open(root.join("session.sqlite")).unwrap();
    database
        .execute_batch(
            "CREATE TRIGGER deny_cleanup BEFORE INSERT ON runtime_audit
        WHEN NEW.kind='processes_stopped'
        BEGIN SELECT RAISE(ABORT,'live owner cleanup recording failure'); END;",
        )
        .unwrap();
    let admitted = enqueue_resources(&runtime, &session, "keep the original process owner");
    let worker = {
        let runtime = runtime.clone();
        let task = admitted.id.clone();
        tokio::spawn(async move { runtime.resume(&task, CancellationToken::new()).await })
    };
    let leader: u32 = wait_file(&root.join("leader.pid")).trim().parse().unwrap();
    let leaf: u32 = wait_file(&root.join("leaf.pid")).trim().parse().unwrap();
    let identities = [
        cayw_host::ProcessIdentity::observe(leader).unwrap(),
        cayw_host::ProcessIdentity::observe(leaf).unwrap(),
    ];
    let original_task = runtime.task(&admitted.id).unwrap();
    let target = original_task.cancel_target();
    assert_resources(&peer, &original_task, true);
    assert!(cancel_when_available(&peer, &target));
    let error = tokio::time::timeout(Duration::from_secs(30), worker)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("live owner cleanup recording failure")
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    while !identities.iter().all(stopped) {
        assert!(
            Instant::now() < deadline,
            "original process cleanup did not finish"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let supervisor = cayw_host::ProcessIdentity::observe(std::process::id()).unwrap();
    assert!(supervisor.is_running().unwrap());
    let task = peer.task(&admitted.id).unwrap();
    assert_eq!(task.state, cayw_runtime::TaskState::Draining);
    assert_eq!(task.cancel_target(), target);
    assert_resources(&peer, &original_task, true);
    let reopened = open(root).unwrap();
    assert_resources(&reopened, &original_task, true);
    assert_eq!(reopened.task(&task.id).unwrap(), task);
    let before = peer.get(&session).unwrap();
    let events = peer.events(&session, 0, 1000).unwrap();
    let audit = serde_json::to_value(peer.audit(&session, 0, 1000).unwrap()).unwrap();
    assert_eq!(peer.health().unwrap().running, 1);
    assert!(matches!(
        peer.enqueue(&other, "capacity is still held"),
        Err(RuntimeError::QueueFull)
    ));
    database.execute_batch("DROP TRIGGER deny_cleanup").unwrap();
    assert!(matches!(
        Runtime::confirm_lost_processes_stopped(
            root.join("session.sqlite"),
            config(root),
            &target,
            &HostActor::new("independent-peer").unwrap(),
            "processes stopped but their original supervisor remains alive",
            |_| panic!("a live original supervisor must not reach the host verifier"),
        ),
        Err(RuntimeError::Busy)
    ));
    assert!(peer.drain_task_processes(&task.id).await.is_err());
    assert!(matches!(
        peer.recover(&session),
        Err(RuntimeError::RecoveryRequired { .. })
    ));
    assert_eq!(peer.get(&session).unwrap(), before);
    assert_eq!(peer.task(&task.id).unwrap(), task);
    assert_eq!(peer.events(&session, 0, 1000).unwrap(), events);
    assert_eq!(
        serde_json::to_value(peer.audit(&session, 0, 1000).unwrap()).unwrap(),
        audit
    );
    assert_eq!(peer.health().unwrap().running, 1);
    let scope: String = database
        .query_row(
            "SELECT state FROM runtime_process_scopes WHERE task=?1",
            [&task.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(scope, "open");
    // Only the original in-memory supervisor can retry its own cleanup record.
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match runtime.drain_task_processes(&task.id).await {
                Err(RuntimeError::Busy) => tokio::task::yield_now().await,
                result => break result.unwrap(),
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(peer.health().unwrap().running, 1);
    assert_resources(&peer, &original_task, true);
    peer.recover(&session).unwrap();
    assert_eq!(peer.health().unwrap().running, 0);
    assert_resources(&peer, &original_task, false);
    assert!(supervisor.is_running().unwrap());
    let cleanup: Vec<_> = peer
        .audit(&session, 0, 1000)
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == "processes_stopped")
        .collect();
    assert_eq!(cleanup.len(), 1);
    assert_eq!(cleanup[0].data["state"], "drained");
    assert!(
        peer.enqueue(&session, "unknown effects cannot be replayed")
            .is_err()
    );
}

#[test]
fn stopped_authority_and_lost_process_owner_have_an_explicit_recovery_path() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    std::fs::create_dir(root.join("workspace")).unwrap();
    let mut original = Original {
        child: Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "authority_process_child", "--nocapture"])
            .env(CHILD_ROOT, root)
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
        root: root.into(),
    };
    let admitted: cayw_runtime::TaskRecord =
        serde_json::from_str(&wait_file(&root.join("identity.json"))).unwrap();
    let leader: u32 = wait_file(&root.join("leader.pid")).trim().parse().unwrap();
    let leaf: u32 = wait_file(&root.join("leaf.pid")).trim().parse().unwrap();
    let identities = [
        cayw_host::ProcessIdentity::observe(leader).unwrap(),
        cayw_host::ProcessIdentity::observe(leaf).unwrap(),
    ];
    assert_eq!(identities[0].process_group, leader);
    assert_eq!(identities[1].parent_pid, leader);
    assert!(!stopped(&identities[0]) && !stopped(&identities[1]));
    // A PID marker precedes the supervisor observer's durable callback. Wait for
    // both original incarnations, rather than racing cleanup against that commit.
    let process_records = rusqlite::Connection::open(root.join("session.sqlite")).unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let recorded = identities.iter().all(|identity| process_records.query_row(
            "SELECT EXISTS(SELECT 1 FROM runtime_process_identities WHERE task=?1 AND pid=?2 AND parent_pid=?3 AND process_group=?4 AND started_seconds=?5 AND started_microseconds=?6)",
            rusqlite::params![admitted.id, identity.pid, identity.parent_pid, identity.process_group, i64::try_from(identity.started_seconds).unwrap(), identity.started_microseconds],
            |row| row.get::<_, bool>(0),
        ).unwrap());
        if recorded {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "original process records did not commit"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let peer = open_when_available(root);
    let task = peer.task(&admitted.id).unwrap();
    let target: CancelTarget = task.cancel_target();
    assert_resources(&peer, &task, true);
    let snapshot = peer.get(&admitted.session_id).unwrap();
    let events = peer.events(&admitted.session_id, 0, 1000).unwrap();
    let actor = HostActor::new("combined-recovery-host").unwrap();
    let confirm = |evidence: &str| {
        Runtime::confirm_lost_processes_stopped(
            root.join("session.sqlite"),
            config(root),
            &target,
            &actor,
            evidence,
            |_| Ok(()),
        )
    };
    assert!(matches!(
        confirm("original owner is still alive"),
        Err(RuntimeError::Busy)
    ));
    original.child.kill().unwrap();
    let exit = original.child.wait().unwrap();
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(exit.signal(), Some(9));
    assert_resources(&peer, &task, true);
    assert_persisted_resources(root, &task);
    assert!(
        identities.iter().all(|identity| !stopped(identity)),
        "the original tree must survive its host before the authority fault"
    );
    // Stop Requests only after losing the original supervisor. A fault detected
    // by a live original Host is allowed to cancel and clean its own processes.
    let requests = rusqlite::Connection::open(root.join("session.sqlite.requests.sqlite")).unwrap();
    requests.execute_batch("CREATE TABLE force_commit_failure(id INTEGER REFERENCES request_meta(singleton) DEFERRABLE INITIALLY DEFERRED); CREATE TRIGGER fail_clock AFTER UPDATE OF last_now_ms ON request_meta BEGIN INSERT INTO force_commit_failure VALUES(2); END;").unwrap();
    assert!(peer.request_task_state(&task.id).is_err());
    requests.execute_batch("DROP TRIGGER fail_clock").unwrap();
    drop(peer);
    assert!(matches!(
        open(root),
        Err(RuntimeError::Request(RequestError::AuthorityStopped))
    ));
    assert_persisted_resources(root, &task);
    let error = Runtime::repair_request_authority(
        root.join("session.sqlite"),
        config(root),
        &request(root),
    )
    .unwrap_err();
    assert!(matches!(
        error,
        RuntimeError::RecoveryRequired {
            outcome_unknown: true,
            ..
        }
    ));
    assert_persisted_resources(root, &task);
    let original_core = cayw_core::SessionStore::open(root.join("session.sqlite"))
        .unwrap()
        .snapshot(&admitted.session_id)
        .unwrap();
    eprintln!(
        "COMBINED_AFTER_KILL identities={:?} states={:?} records={:?}",
        identities,
        identities
            .iter()
            .map(|identity| (stopped(identity), identity.is_running()))
            .collect::<Vec<_>>(),
        process_records
            .prepare("SELECT pid,state FROM runtime_process_identities WHERE task=?1")
            .unwrap()
            .query_map([&task.id], |row| Ok((
                row.get::<_, u32>(0)?,
                row.get::<_, String>(1)?
            )))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    );
    assert!(confirm("the original process tree is still alive").is_err());
    std::fs::write(
        root.join("release"),
        b"host explicitly releases only its original tree",
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while !identities.iter().all(stopped) {
        assert!(
            Instant::now() < deadline,
            "the original tree is still alive"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let evidence = format!(
        "Waited original host SIGKILL; released original managed tree; kernel PID/start/group verified stopped: {identities:?}"
    );
    eprintln!("COMBINED_CLEANUP target={target:?} evidence={evidence}");
    let mut wrong = target.clone();
    wrong.fence += 1;
    assert!(
        Runtime::confirm_lost_processes_stopped(
            root.join("session.sqlite"),
            config(root),
            &wrong,
            &actor,
            &evidence,
            |_| panic!("wrong original identity reached the trusted verifier"),
        )
        .is_err()
    );
    assert!(
        Runtime::confirm_lost_processes_stopped(
            root.join("session.sqlite"),
            config(root),
            &target,
            &actor,
            &evidence,
            |_| Err("independent resource evidence is unavailable".into()),
        )
        .is_err()
    );
    assert_persisted_resources(root, &task);
    Runtime::confirm_lost_processes_stopped(
        root.join("session.sqlite"),
        config(root),
        &target,
        &actor,
        &evidence,
        |observed| {
            assert_eq!(observed.target, target);
            assert_eq!(observed.lease.session_id, target.session_id);
            assert_eq!(Some(&observed.lease.attempt_id), target.attempt_id.as_ref());
            assert_eq!(observed.supervisor.pid, original.child.id());
            assert!(stopped(&observed.supervisor));
            for identity in identities {
                assert!(
                    observed
                        .processes
                        .iter()
                        .any(|process| process.identity == identity)
                );
                assert!(stopped(&identity));
            }
            assert!(
                observed
                    .processes
                    .iter()
                    .all(|process| stopped(&process.identity))
            );
            Ok(())
        },
    )
    .unwrap();
    // Cleanup confirmation cannot activate Requests or settle the original Task.
    assert_persisted_resources(root, &task);
    assert!(matches!(
        open(root),
        Err(RuntimeError::Request(RequestError::AuthorityStopped))
    ));
    Runtime::repair_request_authority(root.join("session.sqlite"), config(root), &request(root))
        .unwrap();
    let reopened = open(root).unwrap();
    assert_eq!(
        cayw_core::SessionStore::open(root.join("session.sqlite"))
            .unwrap()
            .snapshot(&admitted.session_id)
            .unwrap(),
        original_core
    );
    let blocked = reopened.get(&admitted.session_id).unwrap();
    assert_eq!(blocked.state, cayw_core::SessionState::NeedsReconciliation);
    assert_eq!(blocked.messages, snapshot.messages);
    assert_eq!(blocked.attempt, snapshot.attempt);
    assert_eq!(blocked.executions, snapshot.executions);
    assert_eq!(
        blocked.unresolved_capabilities,
        snapshot.unresolved_capabilities
    );
    assert_eq!(
        reopened.events(&admitted.session_id, 0, 1000).unwrap(),
        events
    );
    assert_eq!(reopened.task(&task.id).unwrap(), task);
    assert_eq!(reopened.health().unwrap().running, 1);
    assert_resources(&reopened, &task, true);
    reopened.recover(&admitted.session_id).unwrap();
    assert_eq!(reopened.health().unwrap().running, 0);
    assert_resources(&reopened, &task, false);
    let recovered_task = reopened.task(&task.id).unwrap();
    assert_eq!(recovered_task.fence, task.fence + 1);
    assert_eq!(recovered_task.state, cayw_runtime::TaskState::Interrupted);
    assert!(
        reopened
            .enqueue(&admitted.session_id, "do not replay unknown effects")
            .is_err()
    );
}
