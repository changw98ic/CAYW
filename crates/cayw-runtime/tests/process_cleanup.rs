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
            io: cayw_host::ProcessIo::Pipes,
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
/// The kernel's own process state word for a PID, or `None` when the kernel
/// reports no such process. Read-only: this never signals or reaps anything.
fn ps_state(pid: u32) -> Option<String> {
    let output = std::process::Command::new("/bin/ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    if !output.status.success() {
        return None;
    }
    let state = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (!state.is_empty()).then_some(state)
}
/// What the kernel currently says about one *original* incarnation.
#[derive(Debug, PartialEq, Eq)]
enum Incarnation {
    /// The original incarnation is provably not alive, with the reason.
    NotAlive(String),
    /// The original incarnation still exists and can run or resume.
    Alive(String),
}
/// Verify one original incarnation of a recorded process, not merely a PID.
///
/// A numeric PID proves nothing on its own: an unrelated process can reuse it,
/// and a PID that is absent can hide a read that simply failed. This re-reads the
/// kernel identity of the recorded PID and compares the recorded start time:
/// - `ESRCH` (no such process): the original incarnation is not alive;
/// - the same PID with a *different* start time: a different incarnation reuses
///   the PID, so the recorded one is not alive;
/// - the same start time: only a zombie (`Z`) has exited; a suspended (`T`)
///   incarnation can resume and must still be treated as alive;
/// - any other read failure, or an inconsistent pair of reads, is a refusal —
///   never an assumption that cleanup happened.
fn verify_incarnation(recorded: &cayw_host::ProcessIdentity, label: &str) -> Incarnation {
    // POSIX `ESRCH`. This platform is macOS only, and `ErrorKind` does not map
    // `ESRCH` to `NotFound` (that is `ENOENT`), so the raw number is the only
    // exact check.
    const ESRCH: i32 = 3;
    match cayw_host::ProcessIdentity::observe(recorded.pid) {
        Err(error) if error.raw_os_error() == Some(ESRCH) => Incarnation::NotAlive(format!(
            "pid {} -> ESRCH: the kernel reports no such process",
            recorded.pid
        )),
        Err(error) => panic!(
            "{label}: refusing to read pid {}: an unreadable identity is not proof of \
             cleanup: {error}",
            recorded.pid
        ),
        Ok(current)
            if current.started_seconds != recorded.started_seconds
                || current.started_microseconds != recorded.started_microseconds =>
        {
            Incarnation::NotAlive(format!(
                "pid {} now starts at {}:{} but the recorded incarnation started at {}:{}, \
                 so the recorded incarnation is not alive",
                recorded.pid,
                current.started_seconds,
                current.started_microseconds,
                recorded.started_seconds,
                recorded.started_microseconds
            ))
        }
        Ok(_) => match ps_state(recorded.pid) {
            Some(state) if state.starts_with('Z') => Incarnation::NotAlive(format!(
                "pid {} is still the recorded incarnation and ps reports it as a zombie ({state})",
                recorded.pid
            )),
            Some(state) => Incarnation::Alive(format!(
                "pid {} is still the recorded incarnation and ps reports a non-exited state ({state})",
                recorded.pid
            )),
            None => panic!(
                "{label}: refusing to judge pid {}: the kernel still reports the recorded \
                 incarnation but ps has no state for it, so the reads disagree",
                recorded.pid
            ),
        },
    }
}
#[cfg(target_os = "macos")]
#[test]
fn a_suspended_original_incarnation_is_not_cleanup_evidence() {
    struct OwnedChild(std::process::Child);
    impl Drop for OwnedChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let child = OwnedChild(
        std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .unwrap(),
    );
    let original = cayw_host::ProcessIdentity::observe(child.0.id()).unwrap();
    assert!(
        std::process::Command::new("/bin/kill")
            .args(["-STOP", &original.pid.to_string()])
            .status()
            .unwrap()
            .success()
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !ps_state(original.pid).is_some_and(|state| state.starts_with('T')) {
        assert!(
            std::time::Instant::now() < deadline,
            "the owned child must pause"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let verdict = verify_incarnation(&original, "suspended-child");
    eprintln!("SUSPENDED_INCARNATION identity={original:?} verdict={verdict:?}");
    assert!(
        matches!(verdict, Incarnation::Alive(_)),
        "a suspended original process can resume and is not cleanup evidence: {verdict:?}"
    );
}

/// A read-only snapshot of the current kernel view of one recorded incarnation,
/// captured for evidence. It never panics and never signals.
fn incarnation_snapshot(recorded: &cayw_host::ProcessIdentity) -> serde_json::Value {
    match cayw_host::ProcessIdentity::observe(recorded.pid) {
        Ok(current) => json!({
            "pid": recorded.pid,
            "readable": true,
            "same_start": current.started_seconds == recorded.started_seconds
                && current.started_microseconds == recorded.started_microseconds,
            "started_seconds": current.started_seconds,
            "started_microseconds": current.started_microseconds,
            "process_group": current.process_group,
            "ps_state": ps_state(recorded.pid),
        }),
        Err(error) => json!({
            "pid": recorded.pid,
            "readable": false,
            "errno": error.raw_os_error(),
            "error": error.to_string(),
            "ps_state": ps_state(recorded.pid),
        }),
    }
}
fn identities(dir: &Path) -> Vec<IdentityRow> {
    let connection = rusqlite::Connection::open(dir.join("session.db")).unwrap();
    let mut statement = connection
        .prepare(
            "SELECT task,session,attempt,worker,fence,execution,sequence,
            pid,parent_pid,process_group,started_seconds,started_microseconds,state
            FROM runtime_process_identities ORDER BY pid",
        )
        .unwrap();
    statement
        .query_map([], |row| {
            Ok(IdentityRow {
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
#[derive(Debug, PartialEq, Eq)]
struct IdentityRow {
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

// A PID alone cannot prove which process the Runtime owned, and an absent row
// must never read as "no process ever ran". The durable record has to carry the
// real OS incarnation together with the original lease and the exact planned
// call that owned it. Nothing else survives PID reuse or an attempt replacement.
#[tokio::test]
async fn runtime_records_the_complete_os_identity_of_every_owned_process() {
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
    let task = rt.tasks(&id).unwrap().remove(0);
    assert!(peer.cancel(&id).unwrap());
    let _ = tokio::time::timeout(Duration::from_secs(10), worker)
        .await
        .unwrap()
        .unwrap();
    wait_stopped(&leader).await;
    wait_stopped(&leaf).await;
    rt.wait_idle(&id).await;

    let rows = identities(dir.path());
    assert!(
        !rows.is_empty(),
        "an owned process tree finished without any durable OS process identity record"
    );
    let worker_id = task.worker_id.clone().unwrap();
    let attempt_id = task.attempt_id.clone().unwrap();
    for row in &rows {
        assert_eq!(row.task, task.id, "identity record belongs to another task");
        assert_eq!(
            row.session, id,
            "identity record belongs to another session"
        );
        assert_eq!(
            row.attempt, attempt_id,
            "identity record belongs to another attempt"
        );
        assert_eq!(
            row.worker, worker_id,
            "identity record belongs to another worker"
        );
        assert_eq!(
            row.fence as u64, task.fence,
            "identity record carries another fence"
        );
        assert!(
            !row.execution.is_empty(),
            "identity record has no execution identity"
        );
        assert!(row.sequence >= 0, "identity record has no call sequence");
        assert!(row.pid > 0, "identity record has no OS process ID");
        assert!(
            row.process_group > 0,
            "identity record has no process group"
        );
        assert!(
            row.started_seconds > 0,
            "identity record has no OS start second"
        );
        assert!(
            (0..1_000_000).contains(&row.started_microseconds),
            "identity record has an invalid OS start microsecond"
        );
        assert_eq!(
            row.state, "stopped",
            "identity record was not confirmed stopped"
        );
    }
    let leader_pid: i64 = leader.trim().parse().unwrap();
    let recorded_leader = rows
        .iter()
        .find(|row| row.pid == leader_pid)
        .expect("the process the fixture actually ran is missing from the identity records");
    assert_eq!(
        recorded_leader.process_group, leader_pid,
        "the leader's recorded process group is not its own group"
    );
    let recorded_leaf = rows
        .iter()
        .find(|row| row.pid == leaf.trim().parse::<i64>().unwrap())
        .expect("the descendant of the same process group was never recorded");
    assert_eq!(
        recorded_leaf.parent_pid, leader_pid,
        "the descendant's recorded parent is not the leader that actually spawned it"
    );
}

/// Any process of this test's own fixture, found from the OS instead of from a
/// file the fixture may not have written yet. The unique temporary directory is
/// part of the fixture's arguments, so it identifies exactly these processes.
fn fixture_pids(root: &Path) -> Vec<i64> {
    let needle = root.to_string_lossy().into_owned();
    let output = std::process::Command::new("/bin/ps")
        .args(["-axo", "pid=,command="])
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| line.contains(&needle))
        .filter_map(|line| line.split_whitespace().next()?.parse::<i64>().ok())
        .collect()
}

/// The frozen condition itself: the OS process really started, and its durable
/// registration never happened.
///
/// The denial is on the `started` INSERT, so this is the "started, then the
/// registration failed" order — the case the cleanup-record test above is NOT.
///
/// What this test can and cannot establish, measured rather than assumed:
/// * The refused statement runs in the registration's own transaction, so a
///   read-only SQL shadow of it is rolled back with the denial and cannot survive.
///   A trigger-side copy was tried and produced no row.
/// * The real child is read from the kernel, not inferred. The refusal is preceded
///   by a bounded delay inside the refused statement itself, so the child that
///   `spawn` already exec'd is still alive while the registration is failing; this
///   test then reads the fixture's own readiness PID and re-observes that exact
///   PID's OS identity (`pid`/`parent_pid`/`process_group`/start) from the kernel
///   before the denial completes. The delay is rolled back with the denial.
/// * Without that delay the child loses the race and is terminated before the
///   fixture reaches its own first statement, which is why the plain-denial
///   variant of this test cannot observe it. That is a race, not a guarantee:
///   `spawn` has already exec'd the image, so "before anything executes" is not
///   what is claimed here.
#[tokio::test]
async fn started_registration_failure_still_cleans_the_spawned_process_and_keeps_the_slot() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("workspace")).unwrap();
    let rt = open(dir.path());
    let peer = open(dir.path());
    let id = rt.create(None).unwrap().id;
    let db = rusqlite::Connection::open(dir.path().join("session.db")).unwrap();
    // The bounded delay is inside the refused statement's own transaction: it is
    // rolled back with the denial, and it is what keeps the really-spawned child
    // alive long enough to be observed rather than inferred.
    db.execute_batch(
        "CREATE TABLE registration_gate(n INTEGER);
        CREATE TRIGGER fail_process_identity_start BEFORE INSERT ON runtime_process_identities
        BEGIN
            INSERT INTO registration_gate(n) SELECT count(*) FROM (
                WITH RECURSIVE counter(x) AS (
                    SELECT 1 UNION ALL SELECT x+1 FROM counter WHERE x < 4000000)
                SELECT x FROM counter);
            SELECT RAISE(ABORT, 'injected process identity registration failure');
        END;",
    )
    .unwrap();
    let worker = {
        let rt = rt.clone();
        let id = id.clone();
        tokio::spawn(async move {
            rt.prompt(&id, "register a started process", CancellationToken::new())
                .await
        })
    };
    // The PID comes from the fixture's own readiness write, and the kernel confirms
    // it is still the same live process while the registration is still failing.
    let leader = marker(&dir.path().join("leader.pid")).await;
    let pid: u32 = leader.trim().parse().unwrap();
    assert!(
        live(&leader),
        "the child that really started must still be alive during the denial"
    );
    let observed = cayw_host::ProcessIdentity::observe(pid).unwrap();
    assert_eq!(
        observed.pid, pid,
        "the observed PID is not the fixture's own"
    );
    assert_eq!(
        observed.process_group, observed.pid,
        "the child must really be its managed process group leader"
    );
    assert!(observed.started_seconds > 0);
    eprintln!("STARTED_DENIED identity={observed:?}");
    let error = tokio::time::timeout(Duration::from_secs(30), worker)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    let text = error.to_string();
    // The really-observed child is stopped by its original owner, and the fixture
    // never reaches its release gate because nothing ever releases it.
    wait_stopped(&leader).await;
    assert!(!live(&leader));
    assert!(
        text.contains("injected process identity registration failure"),
        "the injected registration failure must be the reported cause, got: {text}"
    );
    // The Device now classifies a failed start record as an unconfirmed resource
    // cleanup rather than reaching the stopped-record path, so the retained permit
    // and the unconfirmed cleanup are what must be reported.
    assert!(
        text.contains("cleanup is not confirmed") || text.contains("unconfirmed"),
        "a failed start record must be reported as an unconfirmed cleanup, got: {text}"
    );
    assert!(
        text.contains("explicit recovery"),
        "the denied registration must leave an explicit-recovery terminal, got: {text}"
    );
    assert!(
        fixture_pids(dir.path()).is_empty(),
        "the spawned process was abandoned instead of cleaned by its original owner"
    );
    // Nothing was registered: the denied statement left no durable identity row.
    assert!(
        identities(dir.path()).is_empty(),
        "a denied registration must not leave an identity record"
    );
    let task = rt.tasks(&id).unwrap().remove(0);
    assert_eq!(task.state, TaskState::Draining);
    assert_eq!(
        rt.health().unwrap().running,
        1,
        "an unconfirmed process cleanup must retain the original execution slot"
    );
    assert!(
        rt.enqueue(&id, "do not replay an unregistered process")
            .is_err()
    );
    assert!(matches!(
        rt.recover(&id),
        Err(RuntimeError::RecoveryRequired { .. })
    ));
    assert!(
        !rt.audit(&id, 0, 1000)
            .unwrap()
            .iter()
            .any(|event| event.kind == "processes_stopped"),
        "nothing may claim the process cleanup was confirmed"
    );
    assert!(
        rt.drain_task_processes(&task.id).await.is_err(),
        "an unrecorded incarnation must not be reported as confirmed"
    );
    let _ = peer;
}

/// The operating system really started a process tree — both real PIDs are read
/// from the fixture's own readiness files and observed alive — and the process
/// ran to the end of its life, but the durable record of its *cleanup* could not
/// be written.
///
/// Scope, stated exactly: this denies the `stopped` record only. The `started`
/// registration and the descendant observation both succeeded, so this is NOT the
/// frozen "start succeeded but the registration failed" condition and must never
/// be reported as it. It is the post-startup *cleanup* record failure.
///
/// It is also a *resource* record failure, not a late business result: the task's
/// terminal outcome was already fixed before cleanup ran, so nothing here is a
/// late-result or exit/cancel race. What it proves is the required consequence:
/// the original execution slot stays retained, capacity is not released, and the
/// real process tree is still cleaned up and observed stopped, not abandoned.
#[tokio::test]
async fn failed_identity_record_still_cleans_the_real_process_tree_and_keeps_the_slot() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("workspace")).unwrap();
    let rt = open(dir.path());
    let peer = open(dir.path());
    let id = rt.create(None).unwrap().id;
    let other = rt.create(None).unwrap().id;
    let db = rusqlite::Connection::open(dir.path().join("session.db")).unwrap();
    // The start and observation records succeed; only the incarnation's final
    // cleanup record is denied, so the failure is provably post-exec.
    db.execute_batch(
        "CREATE TRIGGER fail_process_identity_stop BEFORE UPDATE ON runtime_process_identities
        WHEN NEW.state='stopped'
        BEGIN SELECT RAISE(ABORT, 'injected process identity cleanup recording failure'); END;",
    )
    .unwrap();
    let worker = {
        let rt = rt.clone();
        let id = id.clone();
        tokio::spawn(async move {
            rt.prompt(
                &id,
                "record a started process tree",
                CancellationToken::new(),
            )
            .await
        })
    };
    let leader = marker(&dir.path().join("leader.pid")).await;
    let leaf = marker(&dir.path().join("leaf.pid")).await;
    // The PIDs come from the fixture's own readiness, never from a table that the
    // injected failure could have emptied.
    assert!(
        live(&leader) && live(&leaf),
        "the fixture tree must really be running before its record is denied"
    );
    let task = rt.tasks(&id).unwrap().remove(0);
    assert!(peer.cancel(&id).unwrap());
    let error = tokio::time::timeout(Duration::from_secs(10), worker)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    // The denied durable record is the reported cause, whatever terminal variant
    // carries it. Nothing may be presented as a confirmed cleanup.
    assert!(
        error
            .to_string()
            .contains("injected process identity cleanup recording failure"),
        "the injected record failure must be the reported cause, got: {error:?}"
    );
    // Real OS cleanup still happened, and it was observed rather than assumed.
    wait_stopped(&leader).await;
    wait_stopped(&leaf).await;
    assert!(!live(&leader) && !live(&leaf));
    // The incarnations that really ran were durably recorded before the denied
    // cleanup record; the rows exist and were never marked stopped.
    let rows = identities(dir.path());
    let leader_pid: i64 = leader.trim().parse().unwrap();
    let leaf_pid: i64 = leaf.trim().parse().unwrap();
    assert!(
        rows.iter().any(|row| row.pid == leader_pid),
        "the leader that really ran has no durable identity record"
    );
    assert!(
        rows.iter().any(|row| row.pid == leaf_pid),
        "the descendant that really ran has no durable identity record"
    );
    assert!(
        rows.iter().all(|row| row.state != "stopped"),
        "a denied cleanup record must not be written as stopped"
    );
    // The slot is retained and no capacity is released or reused.
    assert_eq!(rt.task(&task.id).unwrap().state, TaskState::Draining);
    assert_eq!(peer.health().unwrap().running, 1);
    assert!(matches!(
        peer.enqueue(&other, "capacity must remain held"),
        Err(RuntimeError::QueueFull)
    ));
    assert!(
        peer.enqueue(&id, "a blocked task must not be reused")
            .is_err()
    );
    let before = peer.get(&id).unwrap();
    assert!(matches!(
        peer.recover(&id),
        Err(RuntimeError::RecoveryRequired { .. })
    ));
    assert_eq!(peer.get(&id).unwrap(), before);
    assert!(
        peer.drain_task_processes(&task.id).await.is_err(),
        "an unrecorded cleanup must not be reported as confirmed"
    );
    // The device cleanup ledger never retries or resets itself, so removing the
    // denial cannot retroactively confirm a cleanup that was never recorded. The
    // original slot therefore stays retained and the task stays draining: this is
    // the conservative contract, and it must never silently auto-replay.
    db.execute_batch("DROP TRIGGER fail_process_identity_stop")
        .unwrap();
    assert!(rt.drain_task_processes(&task.id).await.is_err());
    assert_eq!(rt.task(&task.id).unwrap().state, TaskState::Draining);
    assert_eq!(rt.health().unwrap().running, 1);
    assert!(matches!(
        rt.recover(&id),
        Err(RuntimeError::RecoveryRequired { .. })
    ));
    assert!(
        rt.enqueue(&id, "do not replay an unrecorded process")
            .is_err()
    );
    assert!(
        !rt.audit(&id, 0, 1000)
            .unwrap()
            .iter()
            .any(|event| event.kind == "processes_stopped"),
        "nothing may claim the process cleanup was confirmed"
    );
    let tasks = rt.tasks(&id).unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].id, task.id);
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

/// The process-scope registration write happens inside `CallScope::start`, which
/// the Host invokes from its audit `started` hook. That hook runs inside
/// `Host::admit_owned`, before the owned call is dispatched to the process
/// runner. So a registration failure is a *prelaunch* rejection: this test
/// asserts the runner was never reached, and does not claim "started, then the
/// registration failed".
#[tokio::test]
async fn process_scope_registration_failure_rejects_before_any_process_launch() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("workspace")).unwrap();
    let rt = open(dir.path());
    let peer = open(dir.path());
    let id = rt.create(None).unwrap().id;
    let db = rusqlite::Connection::open(dir.path().join("session.db")).unwrap();
    db.execute_batch(
        "CREATE TRIGGER fail_process_scope BEFORE INSERT ON runtime_process_scopes
        BEGIN SELECT RAISE(ABORT, 'injected process scope registration failure'); END;",
    )
    .unwrap();
    let error = tokio::time::timeout(
        Duration::from_secs(10),
        rt.prompt(&id, "register a process scope", CancellationToken::new()),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("injected process scope registration failure"),
        "registration must fail with the injected reason, got: {error}"
    );
    // Prelaunch: the fixture never ran, so neither its leader nor its leaf marker
    // can exist, and no cleanup record may claim something was stopped.
    assert!(!dir.path().join("leader.pid").exists());
    assert!(!dir.path().join("leaf.pid").exists());
    assert!(
        !rt.audit(&id, 0, 1000)
            .unwrap()
            .iter()
            .any(|event| event.kind == "processes_stopped"),
        "a prelaunch rejection must not record a process cleanup"
    );
    let task = rt.tasks(&id).unwrap().remove(0);
    // Observed current behavior, recorded rather than assumed: a failed durable
    // scope write is treated as an unconfirmed outcome, so the run settles as
    // RecoveryRequired and the queue blocks the task. Nothing was launched, yet
    // the task still needs explicit recovery. The frozen criterion 4:7 requires
    // exactly this conservative retention ("keep draining and the original
    // execution slot, never auto-reuse"), so it is asserted as the contract
    // requires and the over-conservatism is reported to the coordinator.
    assert_eq!(task.state, TaskState::Draining);
    assert_eq!(
        peer.health().unwrap().running,
        1,
        "the original execution slot must stay retained"
    );
    assert!(
        peer.enqueue(&id, "a blocked task must not be reused")
            .is_err(),
        "a task awaiting recovery must never admit a new task"
    );
    // The scope row was never written, so recovery needs no host process proof.
    assert!(rt.drain_task_processes(&task.id).await.is_ok());
    let recovered = rt.recover(&id).unwrap();
    assert!(recovered.attempt.is_none());
    assert_eq!(rt.health().unwrap().running, 0);
    db.execute_batch("DROP TRIGGER fail_process_scope").unwrap();
}

/// Scope: the recorded terminal and ownership stay stable once process cleanup
/// has already finished.
///
/// This test does NOT inject a late result, and it is NOT a natural-exit/cancel
/// race. It cancels while the fixture tree is running, proves both real PIDs
/// have stopped, and only then touches the fixture's exit gate. Because the
/// processes are already observed stopped, that write cannot reach any live
/// process; it only shows that a later external event leaves the already
/// recorded cancel terminal, the single cleanup record and the original task
/// ownership unchanged. A genuine late-result or exit/cancel race needs a
/// process observer able to inject a deterministic late outcome.
#[tokio::test]
async fn cancelled_task_terminal_and_ownership_stay_stable_after_process_cleanup() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("workspace")).unwrap();
    let rt = open(dir.path());
    let peer = open(dir.path());
    let id = rt.create(None).unwrap().id;
    let worker = {
        let rt = rt.clone();
        let id = id.clone();
        tokio::spawn(async move {
            rt.prompt(&id, "cancel then exit late", CancellationToken::new())
                .await
        })
    };
    let leader = marker(&dir.path().join("leader.pid")).await;
    let leaf = marker(&dir.path().join("leaf.pid")).await;
    assert!(live(&leader) && live(&leaf));
    let task = rt.tasks(&id).unwrap().remove(0);
    assert!(peer.cancel(&id).unwrap());
    let _ = tokio::time::timeout(Duration::from_secs(10), worker)
        .await
        .unwrap()
        .unwrap();
    wait_stopped(&leader).await;
    wait_stopped(&leaf).await;
    assert!(!live(&leader) && !live(&leaf));
    let terminal = rt.task(&task.id).unwrap();
    assert!(matches!(
        terminal.state,
        TaskState::Cancelled | TaskState::OutcomeUnknown
    ));
    let audit_before = rt.audit(&id, 0, 1000).unwrap();
    // AuditRecord is not PartialEq; compare the serialized rows instead.
    let snapshot_before = serde_json::to_value(&audit_before).unwrap();
    let stopped_before: Vec<_> = audit_before
        .iter()
        .filter(|event| event.kind == "processes_stopped")
        .collect();
    assert_eq!(stopped_before.len(), 1);
    assert_eq!(stopped_before[0].task_id.as_ref(), Some(&task.id));
    // The fixture tree would exit successfully if it were still alive. Releasing
    // its gate afterwards must not add a second terminal or change ownership.
    std::fs::write(dir.path().join("release"), b"late release").unwrap();
    assert_eq!(rt.task(&task.id).unwrap(), terminal);
    assert_eq!(
        serde_json::to_value(rt.audit(&id, 0, 1000).unwrap()).unwrap(),
        snapshot_before
    );
    assert_eq!(peer.health().unwrap().running, 0);
    // The released gate belongs to the original task only: it must not have been
    // attributed to anything else, and the session still holds exactly that one
    // task with its original terminal.
    let tasks = rt.tasks(&id).unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].id, task.id);
    assert_eq!(rt.task(&task.id).unwrap(), terminal);
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
fn chain_settings(
    root: &Path,
    io: cayw_host::ProcessIo,
    profile_timeout: Duration,
    max_output_bytes: usize,
    emit: bool,
) -> RuntimeConfig {
    let mut config = RuntimeConfig {
        max_concurrent_runs: 1,
        max_queued_runs: 1,
        max_run_time: Duration::from_secs(90),
        ..Default::default()
    };
    config.codemode.timeout = Duration::from_secs(60);
    config.host.call_timeout = Duration::from_secs(60);
    let mut args = vec![
        "chain".into(),
        root.to_string_lossy().into_owned(),
        "0".into(),
    ];
    if emit {
        args.push("emit".into());
    }
    config.host.profiles.insert(
        "chain".into(),
        Profile {
            kind: ProfileKind::Test,
            program: env!("CARGO_BIN_EXE_cayw-runtime-process-fixture").into(),
            args,
            env: BTreeMap::new(),
            timeout: profile_timeout,
            max_output_bytes,
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

/// The initial registration binds the whole identity, not only the primary key.
/// A row carrying the same task/execution/sequence/pid/start but a different
/// recorded topology is not this incarnation's record, so it must be rejected at
/// registration — not accepted now and only discovered when cleanup runs.
///
/// The observable consequence is that nothing survives: the rejected write is
/// rolled back with the registration's own transaction, so the table stays empty.
/// If the binding were only the primary key, the scrambled row would be accepted
/// and would still be there.
#[tokio::test]
async fn registration_rejects_a_record_whose_recorded_topology_differs() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("workspace")).unwrap();
    let rt = open(dir.path());
    let id = rt.create(None).unwrap().id;
    let db = rusqlite::Connection::open(dir.path().join("session.db")).unwrap();
    db.execute_batch(
        "CREATE TRIGGER scramble_recorded_topology BEFORE INSERT ON runtime_process_identities
        BEGIN
            INSERT OR IGNORE INTO runtime_process_identities
                (task,session,attempt,worker,fence,execution,sequence,pid,parent_pid,
                 process_group,started_seconds,started_microseconds,state)
            VALUES (NEW.task,NEW.session,NEW.attempt,NEW.worker,NEW.fence,NEW.execution,
                    NEW.sequence,NEW.pid,NEW.parent_pid + 1,NEW.process_group + 1,
                    NEW.started_seconds,NEW.started_microseconds,NEW.state);
        END;",
    )
    .unwrap();
    let error = {
        let rt = rt.clone();
        let id = id.clone();
        let task = tokio::spawn(async move {
            rt.prompt(&id, "register a process", CancellationToken::new())
                .await
        });
        let mut accepted = Vec::new();
        let mut finished = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let rows = identities(dir.path());
            if !rows.is_empty() {
                accepted = rows;
                break;
            }
            if task.is_finished() {
                finished = true;
                break;
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        task.abort();
        let outcome = if finished {
            Some(task.await.unwrap())
        } else {
            None
        };
        assert!(
            accepted.is_empty(),
            "a record whose recorded topology differs from the observed incarnation \
             was accepted at registration and left behind: {accepted:?}"
        );
        outcome
            .expect("the run neither recorded a scrambled identity nor finished")
            .unwrap_err()
    };
    assert!(
        error.to_string().contains("explicit recovery"),
        "a rejected initial identity must leave an explicit-recovery terminal, got: {error:?}"
    );
    assert!(
        identities(dir.path()).is_empty(),
        "a record whose topology does not match the observed incarnation was accepted"
    );
}

/// Parent, child and grandchild over both real I/O modes. The topology is the
/// fixture's own — each generation spawns the next and writes its own PID — so
/// the recorded parent links are checked against processes that really ran, not
/// against a topology the test merely asserted. Each mode must record all three
/// generations, keep them attributed to the original task, stop every one of
/// them, and release the slot only after that cleanup is confirmed.
///
/// Scope, stated exactly: this is a *management* boundary, not strong isolation.
/// Every generation here stays inside the managed process group it was spawned
/// into, so the group is what is recorded and stopped. A descendant that calls
/// `setsid` leaves that group and is therefore outside what this test covers; the
/// Device reconciles those separately by re-checking every observed PID/start
/// after the group is empty and never signalling an escaped group. This test
/// makes no claim about that escape case.
///
/// This matrix covers a tree that is already running. It deliberately does not
/// cover cancellation during startup: at that trigger point only the pre-exec
/// child exists and no descendant has been created yet, so a parent/child/
/// grandchild topology is not applicable there and must never be asserted here.
/// That case is evidenced at its own lower layer — the Device's startup-cancel
/// record of the real PID/start/group, the still-reserved permit and active
/// supervisor, and the reaped `ECHILD` afterwards — and is not re-derived here.
/// Nor is this a general executor matrix: normal completion, cancellation,
/// timeout, output budget, a panicking worker, graceful host shutdown and host
/// kill are separate behaviours with their own existing coverage, and this test
/// adds only the process-identity half of the two real I/O modes.
#[tokio::test]
async fn pipe_and_pty_process_chains_record_and_clean_all_three_generations() {
    let mut modes = vec![("pipes", cayw_host::ProcessIo::Pipes)];
    #[cfg(target_os = "macos")]
    modes.push((
        "pty",
        cayw_host::ProcessIo::Pty {
            columns: 80,
            rows: 24,
        },
    ));
    for (label, io) in modes {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("workspace")).unwrap();
        let rt = Runtime::open(
            dir.path().join("session.db"),
            dir.path().join("workspace"),
            Arc::new(ChainProvider),
            Model::new("fixture", "process-owner"),
            chain_settings(dir.path(), io, Duration::from_secs(60), 4096, false),
        )
        .unwrap();
        let peer = open(dir.path());
        let id = rt.create(None).unwrap().id;
        let worker = {
            let rt = rt.clone();
            let id = id.clone();
            tokio::spawn(async move {
                rt.prompt(&id, "run a process chain", CancellationToken::new())
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
        assert!(
            pids.iter().all(|pid| live(&pid.to_string())),
            "{label}: all three real generations must be running"
        );
        let observed = wait_recorded_and_observed(dir.path(), &pids).await;
        let task = rt.tasks(&id).unwrap().remove(0);
        assert!(peer.cancel(&id).unwrap());
        let _ = tokio::time::timeout(Duration::from_secs(10), worker)
            .await
            .unwrap()
            .unwrap();
        for pid in &pids {
            wait_stopped(&pid.to_string()).await;
        }
        assert!(
            pids.iter().all(|pid| !live(&pid.to_string())),
            "{label}: every generation must be stopped, not abandoned"
        );
        let rows = identities(dir.path());
        assert_recorded_chain(
            &rows,
            &task.id,
            &pids,
            &observed,
            i64::from(std::process::id()),
            &format!("{label}/basic-cancel"),
        );
        assert!(
            rows.iter().all(|row| row.state == "stopped"),
            "{label}: every generation must be confirmed stopped: {rows:?}"
        );
        assert!(
            rt.audit(&id, 0, 1000)
                .unwrap()
                .iter()
                .any(|event| event.kind == "processes_stopped"),
            "{label}: the confirmed cleanup was not recorded"
        );
        assert_eq!(
            rt.health().unwrap().running,
            0,
            "{label}: the slot must be released only after confirmed cleanup"
        );
    }
}

/// One trigger point per cell. Every cell drives the fixture's own real
/// parent/child/grandchild chain and fires its trigger only after all three OS
/// identities are durably recorded and re-read from the kernel, so no cell can
/// pass by racing the recorder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChainTrigger {
    /// All three generations exit by themselves; no cancellation is involved.
    Normal,
    /// The public session cancel path.
    Cancel,
    /// The host profile deadline.
    Timeout,
    /// The host output budget, driven by the leader's own slow output.
    OutputBudget,
}

fn chain_matrix_config(
    root: &Path,
    io: cayw_host::ProcessIo,
    trigger: ChainTrigger,
) -> RuntimeConfig {
    match trigger {
        ChainTrigger::Timeout => chain_settings(root, io, Duration::from_secs(4), 4096, false),
        ChainTrigger::OutputBudget => chain_settings(root, io, Duration::from_secs(60), 4096, true),
        ChainTrigger::Normal | ChainTrigger::Cancel => {
            chain_settings(root, io, Duration::from_secs(60), 4096, false)
        }
    }
}

/// Wait until every generation is durably recorded, then re-read each one from
/// the kernel and return the kernel identities. The trigger of a cell must never
/// fire before this returns.
async fn wait_recorded_and_observed(root: &Path, pids: &[i64]) -> Vec<cayw_host::ProcessIdentity> {
    let recorded = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let rows = identities(root);
            if pids
                .iter()
                .all(|pid| rows.iter().any(|row| row.pid == *pid))
            {
                return rows;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("every generation must be durably recorded before its trigger fires");
    pids.iter()
        .map(|pid| {
            let identity = cayw_host::ProcessIdentity::observe(*pid as u32)
                .unwrap_or_else(|error| panic!("generation {pid} is not observable: {error}"));
            let row = recorded
                .iter()
                .find(|row| row.pid == *pid)
                .expect("generation was recorded");
            assert_eq!(
                row.started_seconds, identity.started_seconds as i64,
                "the recorded start second is not the kernel's"
            );
            assert_eq!(
                row.started_microseconds,
                i64::from(identity.started_microseconds),
                "the recorded start microsecond is not the kernel's"
            );
            assert_eq!(row.parent_pid, i64::from(identity.parent_pid));
            assert_eq!(row.process_group, i64::from(identity.process_group));
            identity
        })
        .collect()
}

/// After cleanup, every generation must retain its original task, lease, call,
/// startup identity and observed topology, and that original incarnation must
/// no longer be alive. Durable stop state is checked by each caller separately.
fn assert_recorded_chain(
    rows: &[IdentityRow],
    task: &str,
    pids: &[i64],
    observed: &[cayw_host::ProcessIdentity],
    spawner: i64,
    label: &str,
) {
    let find = |pid: i64| {
        rows.iter()
            .find(|row| row.pid == pid)
            .unwrap_or_else(|| panic!("{label}: generation {pid} has no durable identity record"))
    };
    let leader = find(pids[0]);
    for (index, pid) in pids.iter().enumerate() {
        let row = find(*pid);
        assert_eq!(row.task, task, "{label}: wrong task attribution");
        assert_eq!(row.session, leader.session, "{label}: different session");
        assert_eq!(row.attempt, leader.attempt, "{label}: different attempt");
        assert_eq!(row.worker, leader.worker, "{label}: different worker");
        assert_eq!(row.fence, leader.fence, "{label}: different fence");
        assert_eq!(row.execution, leader.execution, "{label}: different cell");
        assert_eq!(row.sequence, leader.sequence, "{label}: different call");
        assert_eq!(
            row.process_group, pids[0],
            "{label}: generation {pid} is outside the managed process group"
        );
        let identity = &observed[index];
        assert_eq!(
            row.pid,
            i64::from(identity.pid),
            "{label}: recorded pid is not the kernel's"
        );
        assert_eq!(
            row.started_seconds, identity.started_seconds as i64,
            "{label}: recorded start second changed after cleanup"
        );
        assert_eq!(
            row.started_microseconds,
            i64::from(identity.started_microseconds),
            "{label}: recorded start microsecond changed after cleanup"
        );
        assert_eq!(
            row.parent_pid,
            i64::from(identity.parent_pid),
            "{label}: recorded parent differs from the original observation"
        );
        assert_eq!(
            row.process_group,
            i64::from(identity.process_group),
            "{label}: recorded group differs from the original observation"
        );
        let verdict = verify_incarnation(identity, label);
        assert!(
            matches!(verdict, Incarnation::NotAlive(_)),
            "{label}: generation {index} is still the original live incarnation: {verdict:?}"
        );
        eprintln!(
            "CHAIN_INCARNATION_STOPPED {}",
            json!({
                "scenario": label,
                "generation": index,
                "task": row.task,
                "session": row.session,
                "attempt": row.attempt,
                "worker": row.worker,
                "fence": row.fence,
                "execution": row.execution,
                "sequence": row.sequence,
                "pid": identity.pid,
                "parent": identity.parent_pid,
                "group": identity.process_group,
                "started_seconds": identity.started_seconds,
                "started_microseconds": identity.started_microseconds,
                "durable_state": row.state,
                "verdict": format!("{verdict:?}"),
            })
        );
    }
    assert_eq!(
        leader.parent_pid, spawner,
        "{label}: the leader's real parent is not the process that spawned it"
    );
    assert_eq!(
        find(pids[1]).parent_pid,
        pids[0],
        "{label}: the child's recorded parent is not the process that spawned it"
    );
    assert_eq!(
        find(pids[2]).parent_pid,
        pids[1],
        "{label}: the grandchild's recorded parent is not the process that spawned it"
    );
}

/// The management-boundary matrix, one cell per trigger point, over the real
/// parent/child/grandchild chain in both real I/O modes. Each cell records the
/// kernel identity, the terminal, the cleanup event, the slot and the limits it
/// ran under, so a cell cannot be mapped onto a different trigger point.
///
/// Scope, stated exactly: this is a *management* boundary, not strong isolation.
/// Every generation stays inside the managed process group it was spawned into.
/// A descendant that calls `setsid` leaves that group and is outside this matrix;
/// the Device reconciles those separately and never signals an escaped group.
/// Cancellation during startup is deliberately absent: at that trigger point only
/// the pre-exec child exists and no descendant has been created, so a parent/
/// child/grandchild topology is not applicable and is never asserted here.
#[tokio::test]
async fn chain_matrix_records_and_cleans_every_generation_per_trigger_point() {
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
        for trigger in [
            ChainTrigger::Normal,
            ChainTrigger::Cancel,
            ChainTrigger::Timeout,
            ChainTrigger::OutputBudget,
        ] {
            let label = format!("{io_label}/{:?}", trigger);
            let dir = tempfile::tempdir().unwrap();
            std::fs::create_dir(dir.path().join("workspace")).unwrap();
            let rt = Runtime::open(
                dir.path().join("session.db"),
                dir.path().join("workspace"),
                Arc::new(ChainProvider),
                Model::new("fixture", "process-owner"),
                chain_matrix_config(dir.path(), io, trigger),
            )
            .unwrap();
            let id = rt.create(None).unwrap().id;
            let worker = {
                let rt = rt.clone();
                let id = id.clone();
                tokio::spawn(async move {
                    rt.prompt(&id, "run a process chain", CancellationToken::new())
                        .await
                })
            };
            let chain_started = std::time::Instant::now();
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
            // The trigger may not fire before all three identities are recorded.
            let observed = wait_recorded_and_observed(dir.path(), &pids).await;
            let recorded_after = chain_started.elapsed();
            let task = rt.tasks(&id).unwrap().remove(0);
            match trigger {
                ChainTrigger::Normal => {
                    std::fs::write(dir.path().join("release"), "release").unwrap();
                }
                ChainTrigger::Cancel => assert!(rt.cancel(&id).unwrap()),
                ChainTrigger::OutputBudget => {
                    // The leader emits only after this file appears, so the output
                    // budget cannot fire before all three generations are durably
                    // recorded and re-read from the kernel. This cell therefore
                    // gates on the recorder, not on a readiness marker.
                    std::fs::write(dir.path().join("output-release"), "release").unwrap();
                }
                ChainTrigger::Timeout => {
                    // The deadline is a real product deadline and is never gated by
                    // this test. What is measured here is only that *in this run*
                    // the recording provably completed before it fired: the task is
                    // still running and the elapsed time is below the deadline.
                    assert!(
                        recorded_after < Duration::from_secs(4),
                        "{label}: the real deadline fired before the chain was recorded \
                         (recorded after {recorded_after:?})"
                    );
                    assert!(
                        matches!(
                            rt.task(&task.id).unwrap().state,
                            TaskState::Running | TaskState::Draining
                        ),
                        "{label}: the deadline had already terminated the task when the \
                         chain was recorded"
                    );
                }
            }
            let outcome = tokio::time::timeout(Duration::from_secs(40), worker)
                .await
                .expect("the cell must reach a terminal")
                .unwrap();
            for pid in &pids {
                wait_stopped(&pid.to_string()).await;
            }
            assert!(
                pids.iter().all(|pid| !live(&pid.to_string())),
                "{label}: every generation must be stopped, not abandoned"
            );
            let rows = identities(dir.path());
            assert_recorded_chain(
                &rows,
                &task.id,
                &pids,
                &observed,
                i64::from(std::process::id()),
                &label,
            );
            assert!(
                rows.iter().all(|row| row.state == "stopped"),
                "{label}: every generation must be confirmed stopped: {rows:?}"
            );
            let audit = rt.audit(&id, 0, 1000).unwrap();
            let stopped = audit
                .iter()
                .find(|event| event.kind == "processes_stopped")
                .expect("a confirmed cleanup must be recorded");
            assert_eq!(stopped.task_id.as_ref(), Some(&task.id));
            assert_eq!(stopped.data["state"], "drained");
            assert_eq!(
                rt.health().unwrap().running,
                0,
                "{label}: the slot must be released only after confirmed cleanup"
            );
            // The terminal distinguishes the cells: only the fully normal chain
            // finishes idle; every cell that stops a running tree leaves the
            // original effects to be reconciled.
            let view = outcome
                .as_ref()
                .expect("every matrix cell must reach a terminal view");
            assert_eq!(
                view.state,
                match trigger {
                    ChainTrigger::Normal => SessionState::Idle,
                    _ => SessionState::NeedsReconciliation,
                },
                "{label}: unexpected terminal"
            );
            let cause = view
                .unresolved_capabilities
                .iter()
                .find(|call| call.name == "test.run")
                .and_then(|call| call.outcome.as_ref())
                .map(|outcome| format!("{outcome:?}"))
                .unwrap_or_default();
            let cause_lower = cause.to_lowercase();
            match trigger {
                ChainTrigger::Cancel => assert!(
                    cause_lower.contains("cancel"),
                    "{label}: the public cancel was not the reported cause: {cause}"
                ),
                ChainTrigger::Timeout => assert!(
                    cause_lower.contains("timedout"),
                    "{label}: the deadline was not the reported cause: {cause}"
                ),
                ChainTrigger::OutputBudget => assert!(
                    cause_lower.contains("output"),
                    "{label}: the output budget was not the reported cause: {cause}"
                ),
                ChainTrigger::Normal => assert!(
                    cause.is_empty(),
                    "{label}: a normal chain must not leave an unresolved cause: {cause}"
                ),
            }
            eprintln!(
                "CHAIN_CELL {}",
                json!({
                    "io": io_label,
                    "trigger": format!("{trigger:?}"),
                    "identities": pids.iter().enumerate().map(|(index, pid)| json!({
                        "pid": pid,
                        "parent": observed[index].parent_pid,
                        "group": observed[index].process_group,
                        "started_seconds": observed[index].started_seconds,
                        "started_microseconds": observed[index].started_microseconds,
                    })).collect::<Vec<_>>(),
                    "terminal": format!("{:?}", outcome.as_ref().map(|view| view.state)),
                    "cause": cause,
                    "cleanup_event": stopped.kind,
                    "cleanup_state": stopped.data["state"],
                    "running": rt.health().unwrap().running,
                    "profile_timeout_seconds": match trigger {
                        ChainTrigger::Timeout => 4,
                        _ => 60,
                    },
                    "recorded_after_ms": recorded_after.as_millis() as u64,
                    "trigger_gate": match trigger {
                        ChainTrigger::OutputBudget => "output-release written after all three identities were durably recorded and re-read from the kernel",
                        ChainTrigger::Timeout => "no gate: the real 4s deadline was measured to have not fired when recording completed",
                        _ => "recorded and observed before the trigger",
                    },
                    "max_output_bytes": 4096,
                    "permission": "test.run {\"profile\":\"chain\"}",
                })
            );
        }
    }
}

/// A durable identity record that cannot be written is a persistence failure of
/// the original run, not an ordinary known capability failure: the Runtime cannot
/// prove which OS processes it owned. This denies the *observation* write only, so
/// the leader's own registration and its cleanup record can still succeed.
///
/// The propagation is measured end to end rather than read off a comment: the
/// injected cause reaches the caller, the original call settles as
/// `OutcomeUnknown`, and the session requires reconciliation. The Host classifies
/// every post-launch supervisor or I/O failure this way, so the unknown marking
/// does not depend on `record_process_identity` calling `context.persistence_failed`
/// itself — it is a backstop, not a substitute, and this test fixes which one
/// actually runs.
///
/// This deliberately does not depend on whether the call reports through a
/// returned view or through an explicit recovery error: the Device's own
/// observer-failure handling decides that, and the business facts asserted here
/// must hold either way. The resource facts are recorded alongside them.
#[tokio::test]
async fn a_failed_observation_record_marks_the_original_run_unknown() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("workspace")).unwrap();
    let rt = open(dir.path());
    let id = rt.create(None).unwrap().id;
    let db = rusqlite::Connection::open(dir.path().join("session.db")).unwrap();
    db.execute_batch(
        "CREATE TRIGGER fail_observation BEFORE INSERT ON runtime_process_identities
        WHEN NEW.state='observed'
        BEGIN SELECT RAISE(ABORT,'injected process observation failure'); END;",
    )
    .unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(30),
        rt.prompt(&id, "register a process", CancellationToken::new()),
    )
    .await
    .unwrap();
    if let Err(error) = &result {
        assert!(
            matches!(error, RuntimeError::RecoveryRequired { .. }),
            "a failed identity record must not report an ordinary failure, got: {error:?}"
        );
    }
    let returned_view = result.is_ok();
    let view = result.unwrap_or_else(|_| rt.get(&id).unwrap());
    assert_eq!(
        view.state,
        SessionState::NeedsReconciliation,
        "a failed identity record must leave the original run requiring reconciliation"
    );
    let call = view
        .unresolved_capabilities
        .iter()
        .find(|call| call.name == "test.run")
        .expect("the denied call must remain unresolved");
    let outcome = call
        .outcome
        .clone()
        .expect("the denied call must have a settled outcome");
    assert!(
        matches!(
            &outcome,
            cayw_core::CapabilityOutcome::OutcomeUnknown { error }
                if error.contains("injected process observation failure")
        ),
        "a failed identity record must settle as unknown with its own cause, got: {outcome:?}"
    );
    let task = rt.tasks(&id).unwrap().remove(0);
    // The strict target, measured: the failed record keeps the original execution
    // slot draining with its capacity held, while the business outcome is unknown.
    assert_eq!(task.state, TaskState::Draining);
    assert_eq!(rt.health().unwrap().running, 1);
    eprintln!(
        "OBSERVED_UNKNOWN {}",
        json!({
            "returned": if returned_view { "view" } else { "recovery_required" },
            "task_state": format!("{:?}", task.state),
            "running": rt.health().unwrap().running,
            "session_state": format!("{:?}", view.state),
            "outcome": format!("{outcome:?}"),
        })
    );
}

fn io_modes() -> Vec<(&'static str, cayw_host::ProcessIo)> {
    let mut modes = vec![("pipes", cayw_host::ProcessIo::Pipes)];
    #[cfg(target_os = "macos")]
    modes.push((
        "pty",
        cayw_host::ProcessIo::Pty {
            columns: 80,
            rows: 24,
        },
    ));
    modes
}

/// The three real generations of one cell, read from the fixture's own readiness
/// writes. Each generation spawns the next, so the topology is the operating
/// system's, not a claim the test made.
async fn chain_generations(root: &Path) -> Vec<i64> {
    let mut pids = Vec::new();
    for depth in 0..3 {
        pids.push(
            marker(&root.join(format!("p{depth}.pid")))
                .await
                .trim()
                .parse::<i64>()
                .unwrap(),
        );
    }
    pids
}

fn spawn_chain_host(root: &Path, io_label: &str, shutdown: bool) -> tokio::process::Child {
    let mut command =
        tokio::process::Command::new(env!("CARGO_BIN_EXE_cayw-runtime-process-fixture"));
    command
        .arg("chainhost")
        .arg(root)
        .arg(io_label)
        .kill_on_drop(true)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if shutdown {
        command.arg("shutdown");
    }
    command.spawn().unwrap()
}

/// A real provider crash while the real three-generation chain is running. The
/// panic is gated on the test's own file so it can never fire before every
/// generation has been durably recorded and re-read from the kernel.
struct ChainPanicProvider {
    root: std::path::PathBuf,
}
impl Provider for ChainPanicProvider {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        if matches!(
            request.messages.last(),
            Some(cayw_ai::Message::ToolResult(_))
        ) {
            let root = self.root.clone();
            return Box::pin(futures::stream::once(async move {
                for depth in 0..3 {
                    marker(&root.join(format!("p{depth}.pid"))).await;
                }
                marker(&root.join("panic")).await;
                panic!("injected provider panic while the real three-generation chain is running");
            }));
        }
        let mut message = cayw_ai::AssistantMessage::new(&request.model);
        message
            .content
            .push(cayw_ai::Content::ToolCall(cayw_ai::ToolCall::new(
                "panic-chain",
                "execute_code",
                json!({
                    "action":"exec",
                    "script":"return cayw.call('test.run',{profile='chain'})",
                    "yield_time_ms":0,
                    "max_output_tokens":4096,
                }),
            )));
        message.stop_reason = cayw_ai::StopReason::ToolUse;
        Box::pin(futures::stream::iter([AssistantEvent::Done { message }]))
    }
}

/// The frozen condition's Worker-crash half, in both real I/O modes, with all
/// three generations really running when the Worker dies.
///
/// Scope: the panic is the Runtime's own Worker/provider failure path, not a
/// fault injected into the Device. Every generation is inside the managed
/// process group the Device created, so the group is what is recorded and
/// stopped; a descendant that called `setsid` would leave that group and is not
/// covered here.
#[tokio::test]
async fn provider_panic_stops_and_records_every_generation_in_both_io_modes() {
    for (io_label, io) in io_modes() {
        let label = format!("{io_label}/provider-panic");
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("workspace")).unwrap();
        let rt = Runtime::open(
            dir.path().join("session.db"),
            dir.path().join("workspace"),
            Arc::new(ChainPanicProvider {
                root: dir.path().into(),
            }),
            Model::new("fixture", "process-owner"),
            chain_settings(dir.path(), io, Duration::from_secs(60), 4096, false),
        )
        .unwrap();
        let id = rt.create(None).unwrap().id;
        let worker = {
            let rt = rt.clone();
            let id = id.clone();
            tokio::spawn(async move {
                rt.prompt(&id, "panic with a live chain", CancellationToken::new())
                    .await
            })
        };
        let pids = chain_generations(dir.path()).await;
        let observed = wait_recorded_and_observed(dir.path(), &pids).await;
        let task = rt.tasks(&id).unwrap().remove(0);
        std::fs::write(dir.path().join("panic"), "panic").unwrap();
        let outcome = tokio::time::timeout(Duration::from_secs(40), worker)
            .await
            .expect("the cell must reach a terminal")
            .unwrap();
        if let Ok(view) = &outcome {
            assert_eq!(view.state, SessionState::NeedsReconciliation, "{label}");
            assert!(
                view.messages.iter().any(|message| matches!(message,
                    cayw_ai::Message::Assistant(answer)
                    if answer.error_message.as_deref() == Some("Provider stream panicked"))),
                "{label}: the provider panic was not recorded"
            );
        }
        for pid in &pids {
            wait_stopped(&pid.to_string()).await;
        }
        assert!(
            pids.iter().all(|pid| !live(&pid.to_string())),
            "{label}: every generation must be stopped, not abandoned"
        );
        let rows = identities(dir.path());
        assert_recorded_chain(
            &rows,
            &task.id,
            &pids,
            &observed,
            i64::from(std::process::id()),
            &label,
        );
        assert!(
            rows.iter().all(|row| row.state == "stopped"),
            "{label}: every generation must be confirmed stopped: {rows:?}"
        );
        let audit = rt.audit(&id, 0, 1000).unwrap();
        let stopped = audit
            .iter()
            .find(|event| event.kind == "processes_stopped")
            .expect("a confirmed cleanup must be recorded");
        assert_eq!(stopped.data["state"], "drained");
        assert_eq!(stopped.task_id.as_ref(), Some(&task.id));
        let unknown = audit
            .iter()
            .find(|event| event.kind == "run_outcome_unknown")
            .expect("a panicking worker must leave the original effects unknown");
        assert!(
            stopped.cursor < unknown.cursor,
            "{label}: cleanup must be recorded before the unknown terminal"
        );
        assert_eq!(
            rt.health().unwrap().running,
            0,
            "{label}: the slot must be released only after confirmed cleanup"
        );
        assert_eq!(
            rt.task(&task.id).unwrap().state,
            TaskState::OutcomeUnknown,
            "{label}: unexpected terminal"
        );
        assert!(
            rt.enqueue(&id, "do not replay provider panic effects")
                .is_err()
        );
        eprintln!(
            "CHAIN_PANIC {}",
            json!({
                "io": io_label,
                "identities": pids.iter().enumerate().map(|(index, pid)| json!({
                    "pid": pid, "parent": observed[index].parent_pid,
                    "group": observed[index].process_group,
                })).collect::<Vec<_>>(),
                "terminal": format!("{:?}", rt.task(&task.id).unwrap().state),
                "cleanup_state": stopped.data["state"],
                "running": rt.health().unwrap().running,
            })
        );
    }
}

/// A real, independent host process performs the original graceful shutdown —
/// public cancel, then wait until the owned worker has finished its own process
/// cleanup — and only then exits normally. Nothing here drops an in-process
/// prompt future: the host is a separate OS process with its own Runtime.
#[tokio::test]
async fn a_real_host_process_cancels_and_waits_for_every_generation_before_exiting() {
    for (io_label, _io) in io_modes() {
        let label = format!("{io_label}/host-exit");
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("workspace")).unwrap();
        let mut host = spawn_chain_host(dir.path(), io_label, true);
        let host_pid = i64::from(host.id().unwrap());
        let id = marker(&dir.path().join("session.id")).await;
        marker(&dir.path().join("host.ready")).await;
        let pids = chain_generations(dir.path()).await;
        let observed = wait_recorded_and_observed(dir.path(), &pids).await;
        std::fs::write(dir.path().join("shutdown"), "shutdown").unwrap();
        let status = tokio::time::timeout(Duration::from_secs(40), host.wait())
            .await
            .expect("the real host process must exit")
            .unwrap();
        assert!(
            status.success(),
            "{label}: the real host must exit normally, got {status:?}"
        );
        let evidence = std::fs::read_to_string(dir.path().join("host.exited")).unwrap();
        for pid in &pids {
            wait_stopped(&pid.to_string()).await;
        }
        assert!(
            pids.iter().all(|pid| !live(&pid.to_string())),
            "{label}: the original cancel must have stopped every generation"
        );
        let rt = open(dir.path());
        let task = rt.tasks(&id).unwrap().remove(0);
        let rows = identities(dir.path());
        assert_recorded_chain(&rows, &task.id, &pids, &observed, host_pid, &label);
        assert!(
            rows.iter().all(|row| row.state == "stopped"),
            "{label}: the original host must confirm every generation stopped: {rows:?}"
        );
        let audit = rt.audit(&id, 0, 1000).unwrap();
        let stopped: Vec<_> = audit
            .iter()
            .filter(|event| event.kind == "processes_stopped")
            .collect();
        assert_eq!(stopped.len(), 1, "{label}: exactly one confirmed cleanup");
        assert_eq!(stopped[0].data["state"], "drained");
        assert_eq!(stopped[0].task_id.as_ref(), Some(&task.id));
        let terminal = audit
            .iter()
            .find(|event| matches!(event.kind.as_str(), "run_cancelled" | "run_outcome_unknown"))
            .expect("the cancelled run must record its terminal");
        assert!(
            stopped[0].cursor < terminal.cursor,
            "{label}: cleanup must precede the terminal"
        );
        assert_eq!(
            rt.health().unwrap().running,
            0,
            "{label}: the original host released the slot only after confirmed cleanup"
        );
        assert!(
            matches!(
                rt.task(&task.id).unwrap().state,
                TaskState::Cancelled | TaskState::OutcomeUnknown
            ),
            "{label}: unexpected terminal"
        );
        eprintln!(
            "CHAIN_HOST_EXIT {}",
            json!({
                "io": io_label,
                "host_pid": host_pid,
                "host_status": format!("{status:?}"),
                "host_evidence": evidence,
                "identities": pids.iter().enumerate().map(|(index, pid)| json!({
                    "pid": pid, "parent": observed[index].parent_pid,
                    "group": observed[index].process_group,
                })).collect::<Vec<_>>(),
                "terminal": format!("{:?}", rt.task(&task.id).unwrap().state),
                "cleanup_state": stopped[0].data["state"],
                "running": rt.health().unwrap().running,
            })
        );
    }
}

/// A killed host is not an automatic-cleanup promise. The Runtime must keep the
/// original task draining with its original capacity until the *original*
/// identities are verified stopped one by one and that verification is explicitly
/// confirmed by a trusted actor; only then may recovery release the slot.
///
/// The fixture release below is the test's own action on its own fixture, and is
/// labelled as such in the recorded evidence. It is deliberately not presented as
/// product behaviour: nothing in the Runtime releases another owner's process.
#[tokio::test]
async fn a_killed_host_keeps_the_chain_draining_until_every_identity_is_verified_stopped() {
    for (io_label, _io) in io_modes() {
        let label = format!("{io_label}/host-kill");
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("workspace")).unwrap();
        let mut host = spawn_chain_host(dir.path(), io_label, false);
        let host_pid = i64::from(host.id().unwrap());
        let id = marker(&dir.path().join("session.id")).await;
        marker(&dir.path().join("host.ready")).await;
        let pids = chain_generations(dir.path()).await;
        let observed = wait_recorded_and_observed(dir.path(), &pids).await;
        host.kill().await.unwrap();
        host.wait().await.unwrap();
        let alive_after_kill: Vec<bool> = pids.iter().map(|pid| live(&pid.to_string())).collect();
        // The kernel view of each *original* incarnation right after the host was
        // killed, saved as evidence. A bare PID list would not say which
        // incarnation the Runtime owned.
        let identities_after_kill: Vec<_> = observed.iter().map(incarnation_snapshot).collect();
        let rt = open(dir.path());
        let task = rt.tasks(&id).unwrap().remove(0);
        let before = rt.get(&id).unwrap();
        assert_eq!(
            rt.health().unwrap().running,
            1,
            "{label}: a killed host must not release the original slot"
        );
        // The original worker's row still occupies its slot; nothing about the
        // killed host turned it into a confirmed terminal.
        assert!(
            matches!(
                rt.task(&task.id).unwrap().state,
                TaskState::Running | TaskState::Draining
            ),
            "{label}: a killed host must leave the original task occupying its slot"
        );
        assert!(matches!(
            rt.recover(&id),
            Err(RuntimeError::RecoveryRequired {
                outcome_unknown: true,
                ..
            })
        ));
        assert_eq!(
            rt.get(&id).unwrap(),
            before,
            "{label}: recovery must not fence Core before process cleanup"
        );
        assert_eq!(rt.task(&task.id).unwrap(), task);
        assert!(
            rt.drain_task_processes(&task.id).await.is_err(),
            "{label}: a new Host cannot prove old cleanup"
        );
        assert!(
            !rt.audit(&id, 0, 1000)
                .unwrap()
                .iter()
                .any(|event| event.kind == "processes_stopped"),
            "{label}: a killed host must never be recorded as having confirmed cleanup"
        );
        assert_eq!(rt.health().unwrap().running, 1);
        // The test releases its own fixture and verifies the recorded identities
        // stopped one by one before submitting evidence.
        std::fs::write(dir.path().join("release"), b"test fixture release").unwrap();
        for pid in &pids {
            wait_stopped(&pid.to_string()).await;
        }
        assert!(
            pids.iter().all(|pid| !live(&pid.to_string())),
            "{label}: the test's own fixture release must have stopped every generation"
        );
        // Per-incarnation verification, not a single PID check: each original
        // incarnation must be provably not alive on its own. The test's own
        // fixture release and the product's cleanup stay separate facts; this
        // release is the test's action on its own fixture, and the verification
        // below is what the test then asserts to the Runtime.
        let identities_after_release: Vec<_> = observed.iter().map(incarnation_snapshot).collect();
        let verified: Vec<String> = observed
            .iter()
            .enumerate()
            .map(|(index, identity)| {
                let verdict = verify_incarnation(identity, &label);
                assert!(
                    matches!(verdict, Incarnation::NotAlive(_)),
                    "{label}: generation {index} (pid {}) is not provably stopped: {verdict:?}",
                    identity.pid
                );
                match verdict {
                    Incarnation::NotAlive(detail) => detail,
                    Incarnation::Alive(detail) => unreachable!("asserted above: {detail}"),
                }
            })
            .collect();
        let evidence = format!(
            "Test host released its own fixture and verified each original incarnation \
             stopped, one by one, from the kernel identity plus ps: {}",
            verified.join("; ")
        );
        rt.confirm_processes_stopped(&task.id, "fixture-host", &evidence)
            .unwrap();
        assert_eq!(
            rt.health().unwrap().running,
            1,
            "{label}: cleanup evidence alone does not release the task"
        );
        let recovered = rt.recover(&id).unwrap();
        assert_eq!(recovered.state, SessionState::NeedsReconciliation);
        assert!(!recovered.unresolved_capabilities.is_empty());
        assert_eq!(rt.health().unwrap().running, 0);
        assert_eq!(rt.task(&task.id).unwrap().state, TaskState::Interrupted);
        let rows = identities(dir.path());
        assert_recorded_chain(&rows, &task.id, &pids, &observed, host_pid, &label);
        assert!(
            rows.iter().all(|row| row.state != "stopped"),
            "{label}: a killed host cannot confirm a per-incarnation stop: {rows:?}"
        );
        let audit = rt.audit(&id, 0, 1000).unwrap();
        let stopped: Vec<_> = audit
            .iter()
            .filter(|event| event.kind == "processes_stopped")
            .collect();
        assert_eq!(stopped.len(), 1, "{label}: exactly one confirmed cleanup");
        assert_eq!(stopped[0].data["state"], "host_verified");
        assert_eq!(stopped[0].data["actor"], "fixture-host");
        assert_eq!(stopped[0].task_id.as_ref(), Some(&task.id));
        assert!(rt.enqueue(&id, "do not replay process effects").is_err());
        eprintln!(
            "CHAIN_HOST_KILL {}",
            json!({
                "io": io_label,
                "host_pid": host_pid,
                "alive_after_kill": alive_after_kill,
                "identities": pids.iter().enumerate().map(|(index, pid)| json!({
                    "pid": pid, "parent": observed[index].parent_pid,
                    "group": observed[index].process_group,
                    "recorded_started_seconds": observed[index].started_seconds,
                    "recorded_started_microseconds": observed[index].started_microseconds,
                })).collect::<Vec<_>>(),
                "identities_after_kill": identities_after_kill,
                "identities_after_release": identities_after_release,
                "per_incarnation_verdicts": verified,
                "verification_source": "cayw_host::ProcessIdentity::observe per recorded incarnation plus ps -o stat=",
                "terminal": format!("{:?}", rt.task(&task.id).unwrap().state),
                "cleanup_state": stopped[0].data["state"],
                "cleanup_actor": stopped[0].data["actor"],
                "running": rt.health().unwrap().running,
            })
        );
    }
}

/// The durable cell records of this session, exactly as the cell journal stored
/// them. This is the business half of the run, separate from process identities.
fn cell_bodies(dir: &Path) -> Vec<String> {
    let connection = rusqlite::Connection::open(dir.join("session.db")).unwrap();
    let mut statement = connection
        .prepare("SELECT body FROM cells_v1 ORDER BY id")
        .unwrap();
    statement
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

fn first_cell_id(rt: &Runtime, id: &str) -> String {
    rt.get(id)
        .unwrap()
        .messages
        .iter()
        .find_map(|message| match message {
            cayw_ai::Message::ToolResult(result) => result
                .structured_content
                .as_ref()
                .and_then(|value| value.get("cell_id"))
                .and_then(|value| value.as_str())
                .map(str::to_owned),
            _ => None,
        })
        .expect("a started cell must have a durable cell_id")
}

fn plain_answer(model: &Model, text: &str) -> cayw_ai::AssistantMessage {
    let mut message = cayw_ai::AssistantMessage::new(model);
    message.content.push(cayw_ai::Content::text(text));
    message.stop_reason = cayw_ai::StopReason::Stop;
    message
}

/// Wait for a gate file, with a bound, without blocking the runtime's thread.
async fn wait_for_file(path: &Path, limit: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + limit;
    loop {
        if path.exists() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Turn one yields the chain so the VM keeps running while the model continues.
/// Turn two is the *late* answer, and it is held behind a real completion gate:
/// it is produced only after the test opens `<root>/model-gate`, and the test
/// opens that only after the cancel has been observed. A fixed sleep would not
/// be a gate — it would only make "the cancel happened first" likely.
struct YieldChainProvider {
    root: std::path::PathBuf,
}
impl Provider for YieldChainProvider {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        if matches!(
            request.messages.last(),
            Some(cayw_ai::Message::ToolResult(_))
        ) {
            let model = request.model.clone();
            let gate = self.root.join("model-gate");
            return Box::pin(futures::stream::once(async move {
                let opened = wait_for_file(&gate, Duration::from_secs(20)).await;
                AssistantEvent::Done {
                    message: plain_answer(
                        &model,
                        &format!("the model finished after the chain (gate opened: {opened})"),
                    ),
                }
            }));
        }
        let mut message = cayw_ai::AssistantMessage::new(&request.model);
        message
            .content
            .push(cayw_ai::Content::ToolCall(cayw_ai::ToolCall::new(
                "yield-chain",
                "execute_code",
                json!({
                    "action":"exec",
                    "script":"return cayw.call('test.run',{profile='chain'})",
                    "yield_time_ms":0,
                    "max_output_tokens":4096,
                }),
            )));
        message.stop_reason = cayw_ai::StopReason::ToolUse;
        Box::pin(futures::stream::iter([AssistantEvent::Done { message }]))
    }
}

/// A new task that tries to collect the *old attempt's* cell through the ordinary
/// public `wait` action.
struct WaitOldCellProvider {
    cell_id: String,
}
impl Provider for WaitOldCellProvider {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        if matches!(
            request.messages.last(),
            Some(cayw_ai::Message::ToolResult(_))
        ) {
            let model = request.model.clone();
            return Box::pin(futures::stream::iter([AssistantEvent::Done {
                message: plain_answer(&model, "the new task answered on its own"),
            }]));
        }
        let mut message = cayw_ai::AssistantMessage::new(&request.model);
        message
            .content
            .push(cayw_ai::Content::ToolCall(cayw_ai::ToolCall::new(
                "wait-old-cell",
                "execute_code",
                json!({
                    "action":"wait",
                    "cell_id":self.cell_id,
                    "cursor":0,
                    "yield_time_ms":1000,
                    "max_output_tokens":4096,
                }),
            )));
        message.stop_reason = cayw_ai::StopReason::ToolUse;
        Box::pin(futures::stream::iter([AssistantEvent::Done { message }]))
    }
}

/// The forbidden vacuous pass, measured. If a cancel is issued while the real
/// chain is running and the fixture is released afterwards, the Runtime cancels
/// the CodeMode VM: the old business work is *terminated*, not completed, and the
/// recorded page says so. This test exists so that "wrote release after cancel"
/// can never be reported as "the late success arrived" — the recorded page, read
/// back from the session, is the proof of which of the two really happened.
#[tokio::test]
async fn a_cancelled_run_terminates_the_vm_instead_of_recording_a_late_success() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("workspace")).unwrap();
    let rt = Runtime::open(
        dir.path().join("session.db"),
        dir.path().join("workspace"),
        Arc::new(ChainProvider),
        Model::new("fixture", "process-owner"),
        chain_settings(
            dir.path(),
            cayw_host::ProcessIo::Pipes,
            Duration::from_secs(60),
            4096,
            false,
        ),
    )
    .unwrap();
    let id = rt.create(None).unwrap().id;
    let worker = {
        let rt = rt.clone();
        let id = id.clone();
        tokio::spawn(async move {
            rt.prompt(&id, "cancel while the chain runs", CancellationToken::new())
                .await
        })
    };
    let pids = chain_generations(dir.path()).await;
    let observed = wait_recorded_and_observed(dir.path(), &pids).await;
    let task = rt.tasks(&id).unwrap().remove(0);
    assert!(rt.cancel(&id).unwrap());
    std::fs::write(dir.path().join("release"), b"released after the cancel").unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(40), worker)
        .await
        .expect("the cell must reach a terminal")
        .unwrap();
    for pid in &pids {
        wait_stopped(&pid.to_string()).await;
    }
    let late = persisted_tool_results(&rt, &id);
    let bodies = cell_bodies(dir.path());
    let audit = rt.audit(&id, 0, 1000).unwrap();
    eprintln!(
        "CANCELLED_VM {}",
        json!({
            "identities": pids.iter().enumerate().map(|(index, pid)| json!({
                "pid": pid, "parent": observed[index].parent_pid,
                "group": observed[index].process_group,
            })).collect::<Vec<_>>(),
            "terminal": format!("{:?}", rt.task(&task.id).unwrap().state),
            "running": rt.health().unwrap().running,
            "cell_bodies": bodies,
            "tool_result": late,
        })
    );
    assert!(
        late.contains("cancelled") || late.contains("outcome_unknown"),
        "the Runtime must record what actually happened to the old work: {late}"
    );
    assert!(
        !late.contains("\"status\":\"completed\"")
            && !late.contains("\\\"status\\\":\\\"completed\\\""),
        "a cancelled run must not record the old work as completed: {late}"
    );
    assert!(
        !audit.iter().any(|event| event.kind == "run_completed"),
        "a cancelled run must not record a completed terminal"
    );
    assert!(matches!(
        rt.task(&task.id).unwrap().state,
        TaskState::Cancelled | TaskState::OutcomeUnknown
    ));
    assert_eq!(
        audit
            .iter()
            .filter(|event| matches!(
                event.kind.as_str(),
                "run_completed" | "run_cancelled" | "run_outcome_unknown" | "run_failed"
            ))
            .count(),
        1,
        "exactly one terminal may be recorded"
    );
    assert_eq!(rt.health().unwrap().running, 0);
    let rows = identities(dir.path());
    assert_recorded_chain(
        &rows,
        &task.id,
        &pids,
        &observed,
        i64::from(std::process::id()),
        "cancelled-vm",
    );
    assert!(rows.iter().all(|row| row.state == "stopped"));
}

/// Every tool result the Runtime persisted for this session, serialized exactly
/// as it was stored.
fn persisted_tool_results(rt: &Runtime, id: &str) -> String {
    rt.get(id)
        .unwrap()
        .messages
        .iter()
        .filter(|message| matches!(message, cayw_ai::Message::ToolResult(_)))
        .map(|message| serde_json::to_string(message).unwrap())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Every durable message of this session, serialized exactly as stored. Unlike
/// the tool-result-only view this includes assistant messages, so a *model*
/// event can actually be observed here.
fn persisted_history(rt: &Runtime, id: &str) -> String {
    rt.get(id)
        .unwrap()
        .messages
        .iter()
        .map(|message| serde_json::to_string(message).unwrap())
        .collect::<Vec<_>>()
        .join("\n")
}

/// What an *already durable* business success does when a cancel arrives after
/// it. Ordering, stated exactly: the old work really succeeds FIRST — the chain
/// is released while the run is alive, all three generations exit normally, and
/// the cell's durable record reaches a successful terminal — and only then is the
/// run cancelled. So this is **not** a test that a late result arrives after a
/// cancel; it is a test that a success which already exists is not rewritten by a
/// later cancel. That distinction is the whole point of the name.
///
/// It therefore measures two things, and only these two:
///
/// 1. A real, durable business success must not turn the cancelled terminal into
///    a completed one, and must not add a second terminal.
/// 2. A brand-new task in the same Runtime asks for that same cell through the
///    ordinary public `wait` action. The old attempt's durable result must not be
///    handed to the new task, and the new task must be entirely unaffected.
///
/// Scope, stated exactly: the durable result is scoped to its own session and
/// task, so a different task cannot see it at all; the identity guard in `wait`
/// is therefore reached only for the same task, and this test records the actual
/// refusal it observes rather than assuming which one fired.
///
/// The post-cancel case is a different test:
/// `a_success_submitted_after_the_cancel_through_the_original_sink_is_refused`.
#[tokio::test]
async fn an_already_durable_success_is_not_rewritten_by_a_later_cancel_or_adopted_by_a_new_task() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("workspace")).unwrap();
    let rt = Runtime::open(
        dir.path().join("session.db"),
        dir.path().join("workspace"),
        Arc::new(YieldChainProvider {
            root: dir.path().to_path_buf(),
        }),
        Model::new("fixture", "process-owner"),
        chain_settings(
            dir.path(),
            cayw_host::ProcessIo::Pipes,
            Duration::from_secs(60),
            4096,
            false,
        ),
    )
    .unwrap();
    let id = rt.create(None).unwrap().id;
    let worker = {
        let rt = rt.clone();
        let id = id.clone();
        tokio::spawn(async move {
            rt.prompt(&id, "yield then cancel", CancellationToken::new())
                .await
        })
    };
    let pids = chain_generations(dir.path()).await;
    let observed = wait_recorded_and_observed(dir.path(), &pids).await;
    let task = rt.tasks(&id).unwrap().remove(0);
    let cell_id = first_cell_id(&rt, &id);
    // The completion gate: the old business work really finishes successfully.
    std::fs::write(dir.path().join("release"), b"old work really succeeds").unwrap();
    for pid in &pids {
        wait_stopped(&pid.to_string()).await;
    }
    let success = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let bodies = cell_bodies(dir.path());
            if bodies.iter().any(|body| body.contains("completed")) {
                return bodies;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the old business work must really reach a successful terminal");
    assert!(
        pids.iter().all(|pid| !live(&pid.to_string())),
        "the released chain must have exited on its own"
    );
    assert!(rt.cancel(&id).unwrap());
    // The real completion gate: the model's late answer is released only now,
    // strictly after the cancel above returned. It is therefore a genuine
    // post-cancel model event, not a fixed sleep that merely makes the cancel
    // likely to have happened first.
    std::fs::write(
        dir.path().join("model-gate"),
        b"release the late model answer",
    )
    .unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(40), worker)
        .await
        .expect("the cell must reach a terminal")
        .unwrap();
    let audit = rt.audit(&id, 0, 1000).unwrap();
    let terminal = rt.task(&task.id).unwrap();
    let stopped: Vec<_> = audit
        .iter()
        .filter(|event| event.kind == "processes_stopped")
        .collect();
    // Observed over *every* durable message, assistant messages included. The
    // previous observation searched only tool results, so it could never have
    // seen a model event at all.
    let history = persisted_history(&rt, &id);
    let late_model_answer_in_history = history.contains("the model finished after the chain");
    eprintln!(
        "DURABLE_SUCCESS_ORDER {}",
        json!({
            "cell_id": cell_id,
            "cell_bodies": success,
            "identities": pids.iter().enumerate().map(|(index, pid)| json!({
                "pid": pid, "parent": observed[index].parent_pid,
                "group": observed[index].process_group,
            })).collect::<Vec<_>>(),
            "order": "the business success was durable before the cancel; the model answer was released only after it",
            "returned": format!("{:?}", outcome.as_ref().map(|view| view.state)),
            "terminal": format!("{:?}", terminal.state),
            "running": rt.health().unwrap().running,
            "cleanup_events": stopped.len(),
            "late_model_answer_in_history": late_model_answer_in_history,
            "history": history,
        })
    );
    // Measured, not assumed. A model turn that was already in flight when the
    // cancel was signalled can still be committed to the durable history: the
    // provider keeps producing until the abort is observed, and its turn can
    // finish first. That is a *model turn* fact and a separate one from the
    // frozen condition — the run's terminal is still the cancelled one and the
    // durable business result is unchanged, and both are asserted below. The
    // observation is recorded so this boundary can never be read as "a late
    // model answer is impossible".
    // The old business success is real and durable: it was read back above.
    assert!(
        success.iter().any(|body| body.contains("completed")),
        "the old business work never reached a successful durable terminal"
    );
    // A durable success must not change the cancelled terminal or add a second one.
    assert!(
        matches!(
            terminal.state,
            TaskState::Cancelled | TaskState::OutcomeUnknown
        ),
        "a durable old success must not turn a cancelled task into a completed one: {terminal:?}"
    );
    assert!(
        !audit.iter().any(|event| event.kind == "run_completed"),
        "a durable old success must not record a completed terminal"
    );
    assert_eq!(
        audit
            .iter()
            .filter(|event| matches!(
                event.kind.as_str(),
                "run_completed" | "run_cancelled" | "run_outcome_unknown" | "run_failed"
            ))
            .count(),
        1,
        "exactly one terminal may be recorded"
    );
    assert_eq!(stopped.len(), 1);
    assert_eq!(stopped[0].data["state"], "drained");
    assert_eq!(rt.health().unwrap().running, 0);
    let rows = identities(dir.path());
    assert_recorded_chain(
        &rows,
        &task.id,
        &pids,
        &observed,
        i64::from(std::process::id()),
        "late-success",
    );
    assert!(rows.iter().all(|row| row.state == "stopped"));
    // The durable success itself is untouched by the later cancellation.
    assert_eq!(
        cell_bodies(dir.path()),
        success,
        "the recorded business result must not be rewritten"
    );

    // A new task in the same Runtime, in its own session, must be unaffected and
    // must not adopt the old attempt's durable result.
    let other = rt.create(None).unwrap().id;
    let other_rt = Runtime::open(
        dir.path().join("session.db"),
        dir.path().join("workspace"),
        Arc::new(WaitOldCellProvider {
            cell_id: cell_id.clone(),
        }),
        Model::new("fixture", "process-owner"),
        chain_settings(
            dir.path(),
            cayw_host::ProcessIo::Pipes,
            Duration::from_secs(60),
            4096,
            false,
        ),
    )
    .unwrap();
    let view = tokio::time::timeout(
        Duration::from_secs(40),
        other_rt.prompt(
            &other,
            "collect the old attempt's cell",
            CancellationToken::new(),
        ),
    )
    .await
    .expect("the new task must reach a terminal")
    .unwrap();
    let wait_result = persisted_tool_results(&other_rt, &other);
    let other_record = other_rt.tasks(&other).unwrap().remove(0);
    let old_attempt = task.attempt_id.clone().unwrap();
    eprintln!(
        "DURABLE_SUCCESS_NEW_TASK {}",
        json!({
            "new_session": other,
            "new_task": other_record.id,
            "new_terminal": format!("{:?}", other_rt.task(&other_record.id).unwrap().state),
            "new_session_state": format!("{:?}", view.state),
            "old_terminal": format!("{:?}", rt.task(&task.id).unwrap().state),
            "wait_result": wait_result,
            "running": other_rt.health().unwrap().running,
        })
    );
    assert!(
        !wait_result.contains(&cell_id),
        "the new task was handed the old attempt's cell: {wait_result}"
    );
    assert!(
        wait_result.contains("not found") || wait_result.contains("another attempt"),
        "the new task's wait must be refused with a recorded reason: {wait_result}"
    );
    assert_ne!(other_record.id, task.id);
    assert!(other_record.attempt_id.is_some());
    assert_ne!(other_record.attempt_id, task.attempt_id);
    assert_eq!(
        other_rt.task(&other_record.id).unwrap().state,
        TaskState::Completed,
        "the new task must finish on its own"
    );
    assert_eq!(view.state, SessionState::Idle);
    // The cancelled task is exactly as it was.
    assert_eq!(rt.task(&task.id).unwrap(), terminal);
    assert_eq!(
        serde_json::to_value(rt.audit(&id, 0, 1000).unwrap()).unwrap(),
        serde_json::to_value(&audit).unwrap(),
        "the cancelled task's recorded audit must not change"
    );
    let rows_after = identities(dir.path());
    let old_rows: Vec<_> = rows_after
        .iter()
        .filter(|row| row.task == task.id)
        .map(|row| row.pid)
        .collect();
    assert_eq!(
        old_rows.len(),
        3,
        "the old attempt must keep exactly its own three generations"
    );
    assert!(
        rows_after
            .iter()
            .filter(|row| row.task == task.id)
            .all(|row| row.attempt == old_attempt),
        "the old attempt's records must keep their own attempt"
    );
}

/// Remove the chain fixture's own readiness markers between runs.
fn clear_chain_markers(root: &Path) {
    for name in ["p0.pid", "p1.pid", "p2.pid", "release"] {
        let _ = std::fs::remove_file(root.join(name));
    }
}

/// The real, durable successful business result of `name`, read from the Core
/// event log exactly as the Runtime committed it. This is a genuine production
/// result value produced by the real process profile, not a value the test
/// invented.
fn durable_success(rt: &Runtime, id: &str, name: &str) -> serde_json::Value {
    let mut cursor = 0u64;
    loop {
        let page = rt.events(id, cursor, cayw_core::MAX_EVENT_PAGE).unwrap();
        assert!(
            !page.is_empty(),
            "the real successful result must be durably recorded"
        );
        let last = page.len();
        for record in &page {
            cursor = record.cursor;
            if let CoreEvent::CapabilitySettled { call } = &record.event
                && call.name == name
                && let Some(cayw_core::CapabilityOutcome::Completed { result }) = &call.outcome
            {
                return result.clone();
            }
        }
        assert!(
            last == cayw_core::MAX_EVENT_PAGE,
            "the real successful result must be durably recorded"
        );
    }
}

/// A second public prompt replaces the session's lease while its real process
/// chain waits at the fixture's exit gate. Re-submitting the first task's actual
/// success must hit the stale-lease guard before it can change either task.
#[tokio::test]
async fn an_old_success_cannot_change_an_active_new_attempt_in_the_same_session() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("workspace")).unwrap();
    let rt = Runtime::open(
        dir.path().join("session.db"),
        dir.path().join("workspace"),
        Arc::new(ChainProvider),
        Model::new("fixture", "process-owner"),
        chain_settings(
            dir.path(),
            cayw_host::ProcessIo::Pipes,
            Duration::from_secs(60),
            4096,
            false,
        ),
    )
    .unwrap();
    let id = rt.create(None).unwrap().id;
    let first_worker = {
        let rt = rt.clone();
        let id = id.clone();
        tokio::spawn(async move {
            rt.prompt(&id, "complete the first chain", CancellationToken::new())
                .await
        })
    };
    let first_pids = chain_generations(dir.path()).await;
    let first_observed = wait_recorded_and_observed(dir.path(), &first_pids).await;
    let first_task = rt.tasks(&id).unwrap().remove(0);
    let original = rt
        .get(&id)
        .unwrap()
        .unresolved_capabilities
        .into_iter()
        .find(|call| call.name == "test.run")
        .expect("the first real process call must be started");
    assert_eq!(original.stage, cayw_core::CapabilityStage::Started);
    assert!(original.outcome.is_none());
    assert_eq!(original.task_id, first_task.id);
    assert_eq!(original.lease.session_id, id);
    assert_eq!(
        Some(&original.lease.attempt_id),
        first_task.attempt_id.as_ref()
    );
    assert_eq!(
        Some(&original.lease.worker_id),
        first_task.worker_id.as_ref()
    );
    assert_eq!(original.lease.fence, first_task.fence);
    std::fs::write(dir.path().join("release"), b"first chain succeeds").unwrap();
    let first_view = tokio::time::timeout(Duration::from_secs(40), first_worker)
        .await
        .expect("the first real chain must finish")
        .unwrap()
        .expect("the first real chain must succeed");
    assert_eq!(first_view.state, SessionState::Idle);
    let first_terminal = rt.task(&first_task.id).unwrap();
    assert_eq!(first_terminal.state, TaskState::Completed);
    let real_result = durable_success(&rt, &id, "test.run");
    let first_rows = identities(dir.path());
    assert_recorded_chain(
        &first_rows,
        &first_task.id,
        &first_pids,
        &first_observed,
        i64::from(std::process::id()),
        "same-session/first-completed",
    );
    assert!(first_rows.iter().all(|row| row.state == "stopped"));
    clear_chain_markers(dir.path());

    // The next attempt is created by the public Runtime, not a hand-written
    // store record. Its processes remain running until the second release.
    let second_worker = {
        let rt = rt.clone();
        let id = id.clone();
        tokio::spawn(async move {
            rt.prompt(
                &id,
                "keep the second chain active",
                CancellationToken::new(),
            )
            .await
        })
    };
    let second_pids = chain_generations(dir.path()).await;
    let second_observed = wait_recorded_and_observed(dir.path(), &second_pids).await;
    let session_before = rt.get(&id).unwrap();
    let current_lease = session_before.attempt.clone().expect("a new active lease");
    let tasks_before = rt.tasks(&id).unwrap();
    assert_eq!(tasks_before.len(), 2);
    let second_task = tasks_before
        .iter()
        .find(|task| task.id != first_task.id)
        .unwrap();
    assert_eq!(session_before.state, SessionState::Running);
    assert_eq!(second_task.state, TaskState::Running);
    assert_eq!(second_task.session_id, id);
    assert_eq!(current_lease.session_id, id);
    assert_eq!(
        Some(&current_lease.attempt_id),
        second_task.attempt_id.as_ref()
    );
    assert_eq!(
        Some(&current_lease.worker_id),
        second_task.worker_id.as_ref()
    );
    // Core increments the session fence; Queue increments each task's own
    // fence. The two counters do not share a numbering domain.
    assert!(second_task.fence > 0);
    assert_ne!(current_lease.attempt_id, original.lease.attempt_id);
    assert!(current_lease.fence > original.lease.fence);
    assert_eq!(rt.health().unwrap().running, 1);
    assert!(!second_worker.is_finished());
    assert!(!dir.path().join("release").exists());
    for identity in &second_observed {
        assert!(matches!(
            verify_incarnation(identity, "same-session/second-active"),
            Incarnation::Alive(_)
        ));
    }
    let current_call = session_before
        .unresolved_capabilities
        .iter()
        .find(|call| call.name == "test.run")
        .expect("the new attempt's own call must be active");
    assert_eq!(current_call.task_id, second_task.id);
    assert_eq!(current_call.lease, current_lease);
    assert_eq!(current_call.stage, cayw_core::CapabilityStage::Started);
    assert!(current_call.outcome.is_none());
    assert_ne!(current_call.key, original.key);
    let all_events = || {
        let mut cursor = 0;
        let mut events = Vec::new();
        loop {
            let page = rt.events(&id, cursor, cayw_core::MAX_EVENT_PAGE).unwrap();
            let count = page.len();
            if let Some(last) = page.last() {
                cursor = last.cursor;
            }
            events.extend(page);
            if count < cayw_core::MAX_EVENT_PAGE {
                return events;
            }
        }
    };
    let events_before = serde_json::to_value(all_events()).unwrap();
    let audit_before = rt.audit(&id, 0, 1000).unwrap();
    assert!(
        audit_before.len() < 1000,
        "the audit snapshot must be complete"
    );
    let audit_before = serde_json::to_value(audit_before).unwrap();
    let cells_before = cell_bodies(dir.path());
    let identities_before = identities(dir.path());
    assert_eq!(
        identities_before
            .iter()
            .filter(|row| row.task == first_task.id)
            .count(),
        3
    );
    assert!(
        identities_before
            .iter()
            .filter(|row| row.task == first_task.id)
            .all(|row| {
                row.session == original.lease.session_id
                    && row.attempt == original.lease.attempt_id
                    && row.worker == original.lease.worker_id
                    && row.fence == i64::try_from(original.lease.fence).unwrap()
                    && row.execution == original.key.execution_id
                    && row.sequence == i64::try_from(original.key.sequence).unwrap()
            })
    );

    let refusal = rt
        .store()
        .settle_capability(
            &original.lease,
            &original.key,
            cayw_core::CapabilityOutcome::Completed {
                result: real_result.clone(),
            },
        )
        .expect_err("the old result must not be accepted during the new attempt");
    assert!(
        matches!(refusal, cayw_core::CoreError::StaleLease),
        "the replacement lease must reject the old success: {refusal:?}"
    );
    let session_after = rt.get(&id).unwrap();
    let tasks_after = rt.tasks(&id).unwrap();
    let events_after = serde_json::to_value(all_events()).unwrap();
    let audit_after = serde_json::to_value(rt.audit(&id, 0, 1000).unwrap()).unwrap();
    let cells_after = cell_bodies(dir.path());
    let identities_after = identities(dir.path());
    assert_eq!(session_after, session_before);
    assert_eq!(tasks_after, tasks_before);
    assert_eq!(events_after, events_before);
    assert_eq!(audit_after, audit_before);
    assert_eq!(cells_after, cells_before);
    assert_eq!(identities_after, identities_before);
    assert_eq!(rt.task(&first_task.id).unwrap(), first_terminal);
    assert_eq!(rt.health().unwrap().running, 1);
    assert!(!second_worker.is_finished());
    eprintln!(
        "SAME_SESSION_OLD_SUCCESS {}",
        json!({
            "session": id,
            "old_task": first_task.id,
            "old_lease": original.lease,
            "old_key": original.key,
            "real_result": real_result,
            "new_task": second_task.id,
            "current_lease": current_lease,
            "current_key": current_call.key,
            "new_task_state": second_task.state,
            "session_state": session_after.state,
            "running": rt.health().unwrap().running,
            "refusal": format!("{refusal:?}"),
            "session_unchanged": session_after == session_before,
            "tasks_unchanged": tasks_after == tasks_before,
            "events_unchanged": events_after == events_before,
            "audit_unchanged": audit_after == audit_before,
            "cells_unchanged": cells_after == cells_before,
            "process_identities_unchanged": identities_after == identities_before,
        })
    );

    std::fs::write(dir.path().join("release"), b"second chain succeeds").unwrap();
    let second_view = tokio::time::timeout(Duration::from_secs(40), second_worker)
        .await
        .expect("the second real chain must finish")
        .unwrap()
        .expect("the second real chain must succeed on its own");
    assert_eq!(second_view.state, SessionState::Idle);
    assert!(second_view.attempt.is_none());
    assert_eq!(
        rt.task(&second_task.id).unwrap().state,
        TaskState::Completed
    );
    assert_eq!(rt.task(&first_task.id).unwrap(), first_terminal);
    assert_eq!(rt.health().unwrap().running, 0);
    assert!(all_events().iter().any(|event| matches!(
        &event.event,
        CoreEvent::CapabilitySettled { call }
            if call.task_id == second_task.id && call.lease == current_lease
                && call.key == current_call.key
                && matches!(call.outcome, Some(cayw_core::CapabilityOutcome::Completed { .. }))
    )));
    let rows = identities(dir.path());
    let second_rows: Vec<_> = rows
        .into_iter()
        .filter(|row| row.task == second_task.id)
        .collect();
    assert_recorded_chain(
        &second_rows,
        &second_task.id,
        &second_pids,
        &second_observed,
        i64::from(std::process::id()),
        "same-session/second-completed",
    );
    assert!(second_rows.iter().all(|row| row.state == "stopped"));
}

/// The post-cancel half of the frozen condition, measured on the real production
/// write boundary.
///
/// Public cancellation stops the original chain and leaves its cell terminal
/// without success. Each original PID/start time is checked again after cleanup.
/// No successful exit of the cancelled chain is claimed.
///
/// A separate uncancelled run supplies a real success value. After cancellation,
/// that value is submitted with the cancelled call's original lease and key to
/// `SessionStore::settle_capability`, the write boundary used by `CallScope::settle`.
/// The refused submission must leave the terminal, cell, events and audit unchanged.
/// A later task must not adopt it or bypass the unresolved workspace owner.
#[tokio::test]
async fn a_success_submitted_after_the_cancel_through_the_original_sink_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("workspace")).unwrap();
    let rt = Runtime::open(
        dir.path().join("session.db"),
        dir.path().join("workspace"),
        Arc::new(ChainProvider),
        Model::new("fixture", "process-owner"),
        chain_settings(
            dir.path(),
            cayw_host::ProcessIo::Pipes,
            Duration::from_secs(60),
            4096,
            false,
        ),
    )
    .unwrap();

    // A real successful run first, so the value submitted below is a genuine
    // production result of the real profile rather than a number the test made up.
    let source = rt.create(None).unwrap().id;
    let source_worker = {
        let rt = rt.clone();
        let id = source.clone();
        tokio::spawn(async move {
            rt.prompt(&id, "run a real successful chain", CancellationToken::new())
                .await
        })
    };
    let source_pids = chain_generations(dir.path()).await;
    std::fs::write(dir.path().join("release"), b"the real chain succeeds").unwrap();
    let source_view = tokio::time::timeout(Duration::from_secs(40), source_worker)
        .await
        .expect("the real chain must finish")
        .unwrap()
        .expect("the real chain must succeed");
    assert_eq!(
        source_view.state,
        SessionState::Idle,
        "the source run must have completed normally"
    );
    for pid in &source_pids {
        wait_stopped(&pid.to_string()).await;
    }
    let real_result = durable_success(&rt, &source, "test.run");
    clear_chain_markers(dir.path());

    // The run that will be cancelled, in its own session.
    let id = rt.create(None).unwrap().id;
    let worker = {
        let rt = rt.clone();
        let id = id.clone();
        tokio::spawn(async move {
            rt.prompt(&id, "run a chain then cancel it", CancellationToken::new())
                .await
        })
    };
    let pids = chain_generations(dir.path()).await;
    let observed = wait_recorded_and_observed(dir.path(), &pids).await;
    let task = rt.tasks(&id).unwrap().remove(0);
    // The original lease and the original call key, taken from the live Core
    // record of the call itself — not reconstructed from the model's arguments.
    let before = rt.get(&id).unwrap();
    let original = before
        .unresolved_capabilities
        .iter()
        .find(|call| call.name == "test.run")
        .cloned()
        .expect("the original started call must be visible before the cancel");
    assert_eq!(original.stage, cayw_core::CapabilityStage::Started);
    assert!(original.outcome.is_none());
    assert_eq!(original.task_id, task.id);
    assert_eq!(original.lease.session_id, id);
    assert_eq!(Some(&original.lease.attempt_id), task.attempt_id.as_ref());
    assert_eq!(Some(&original.lease.worker_id), task.worker_id.as_ref());
    assert_eq!(original.lease.fence, task.fence);
    assert!(rt.cancel(&id).unwrap());
    let _ = tokio::time::timeout(Duration::from_secs(40), worker)
        .await
        .expect("the cancelled run must reach a terminal")
        .unwrap();
    for pid in &pids {
        wait_stopped(&pid.to_string()).await;
    }
    let stopped_incarnations: Vec<_> = observed
        .iter()
        .map(
            |identity| match verify_incarnation(identity, "cancelled-chain") {
                Incarnation::NotAlive(detail) => detail,
                verdict => panic!("the original cancelled incarnation still exists: {verdict:?}"),
            },
        )
        .collect();
    let cells_before = cell_bodies(dir.path());
    let cancelled_cell = cells_before
        .iter()
        .map(|body| serde_json::from_str::<cayw_cells::CellRecord>(body).unwrap())
        .find(|record| {
            record.cell_id == original.key.execution_id && record.identity.task_id == task.id
        })
        .expect("the cancelled call's own cell must be recorded");
    assert!(matches!(
        cancelled_cell.status,
        cayw_cells::CellStatus::Cancelled | cayw_cells::CellStatus::OutcomeUnknown
    ));
    assert!(
        cancelled_cell
            .calls
            .iter()
            .all(|call| { call.status != cayw_cells::IntentStatus::Succeeded })
    );
    let terminal_before = rt.task(&task.id).unwrap();
    assert!(matches!(
        terminal_before.state,
        TaskState::Cancelled | TaskState::OutcomeUnknown
    ));
    let audit_before = rt.audit(&id, 0, 1000).unwrap();
    assert_eq!(
        audit_before
            .iter()
            .filter(|event| matches!(
                event.kind.as_str(),
                "run_completed" | "run_cancelled" | "run_outcome_unknown" | "run_failed"
            ))
            .count(),
        1
    );
    let events_before =
        serde_json::to_value(rt.events(&id, 0, cayw_core::MAX_EVENT_PAGE).unwrap()).unwrap();
    // The cancelled terminal as it stands *after* the cancel and *before* the
    // late submission. The submission must not move it.
    let state_before_submission = rt.get(&id).unwrap().state;

    // Half two: submit the genuine successful result through the real production
    // write boundary, with the original lease and the original call key, strictly
    // after the cancel.
    let submitted = rt.store().settle_capability(
        &original.lease,
        &original.key,
        cayw_core::CapabilityOutcome::Completed {
            result: real_result.clone(),
        },
    );
    let refusal = match submitted {
        Ok(event) => panic!(
            "the real sink accepted a post-cancel success; the cancelled terminal could \
             have been rewritten: {event:?}"
        ),
        Err(error) => format!("{error:?}"),
    };
    let cells_after = cell_bodies(dir.path());
    let after = rt.get(&id).unwrap();
    eprintln!(
        "POST_CANCEL_SUBMISSION {}",
        json!({
            "original_key": original.key,
            "original_lease": original.lease,
            "original_stage": format!("{:?}", original.stage),
            "original_outcome": original.outcome.as_ref().map(|outcome| format!("{outcome:?}")),
            "real_result": real_result,
            "success_source_session": source,
            "refusal": refusal,
            "cancelled_incarnations_stopped": stopped_incarnations,
            "cancelled_cell_status": cancelled_cell.status,
            "terminal": format!("{:?}", rt.task(&task.id).unwrap().state),
            "session_state_before_submission": format!("{state_before_submission:?}"),
            "session_state_after_submission": format!("{:?}", after.state),
            "running": rt.health().unwrap().running,
            "cell_bodies_unchanged": cells_after == cells_before,
        })
    );
    // The refusal must be the sink's own lease guard, not a decode or shape
    // error: a format error would mean the submission never reached the guard.
    // The cancelled attempt's lease is no longer the session's current lease, and
    // `settle_capability` checks exactly that before it looks at anything else.
    assert_eq!(
        refusal, "StaleLease",
        "the post-cancel submission must be refused by the real lease guard"
    );
    assert!(
        !refusal.contains("CorruptStore")
            && !refusal.contains("InvalidEvent")
            && !refusal.contains("serde")
            && !refusal.contains("InvalidLimit"),
        "a decode, shape or argument error is not a refusal by the real guard: {refusal}"
    );
    // Nothing about the cancelled terminal changed.
    assert_eq!(rt.task(&task.id).unwrap(), terminal_before);
    assert_eq!(
        cells_after, cells_before,
        "the cell journal must not change"
    );
    assert_eq!(
        serde_json::to_value(rt.audit(&id, 0, 1000).unwrap()).unwrap(),
        serde_json::to_value(&audit_before).unwrap(),
        "the cancelled session's audit must not change"
    );
    assert_eq!(
        after.state, state_before_submission,
        "the refused submission must not move the cancelled session's state"
    );
    assert_eq!(
        serde_json::to_value(rt.events(&id, 0, cayw_core::MAX_EVENT_PAGE).unwrap()).unwrap(),
        events_before,
        "the refused submission must not append or rewrite Core events"
    );
    assert_eq!(rt.health().unwrap().running, 0);
    assert!(
        !audit_before
            .iter()
            .any(|event| event.kind == "run_completed"),
        "a refused late success must not have produced a completed terminal"
    );
    let rows = identities(dir.path());
    assert_recorded_chain(
        &rows,
        &task.id,
        &pids,
        &observed,
        i64::from(std::process::id()),
        "post-cancel-submission",
    );
    assert!(rows.iter().all(|row| row.state == "stopped"));
    // The original call is still the original unresolved call, not a success.
    let final_call = rt
        .get(&id)
        .unwrap()
        .unresolved_capabilities
        .iter()
        .find(|call| call.name == "test.run")
        .cloned()
        .expect("the original call must still be recorded");
    assert_eq!(final_call.key, original.key);
    assert_eq!(final_call.lease, original.lease);
    assert!(
        !matches!(
            final_call.outcome,
            Some(cayw_core::CapabilityOutcome::Completed { .. })
        ),
        "the refused submission must not have become the call's outcome: {:?}",
        final_call.outcome
    );

    // A new task in the same Runtime and workspace must not be handed the old
    // result — and it is not silently allowed to run either. The cancelled
    // attempt left a pending workspace mutation, so the Host refuses every later
    // mutation in this workspace with its own owner-recovery guard until the
    // original owner is reconciled. That is the correct safety behaviour, and it
    // is recorded here as the real guard rather than asserted away.
    let other = rt.create(None).unwrap().id;
    let other_worker = {
        let rt = rt.clone();
        let id = other.clone();
        tokio::spawn(async move {
            rt.prompt(&id, "an unrelated new task", CancellationToken::new())
                .await
        })
    };
    let other_view = tokio::time::timeout(Duration::from_secs(40), other_worker)
        .await
        .expect("the new task must reach a terminal")
        .unwrap()
        .unwrap();
    let other_task = rt.tasks(&other).unwrap().remove(0);
    let other_outcomes: Vec<String> = rt
        .events(&other, 0, cayw_core::MAX_EVENT_PAGE)
        .unwrap()
        .iter()
        .filter_map(|record| match &record.event {
            CoreEvent::CapabilitySettled { call } => {
                call.outcome.as_ref().map(|outcome| format!("{outcome:?}"))
            }
            _ => None,
        })
        .collect();
    eprintln!(
        "POST_CANCEL_NEW_TASK {}",
        json!({
            "new_session": other,
            "new_task": other_task.id,
            "new_terminal": format!("{:?}", rt.task(&other_task.id).unwrap().state),
            "new_session_state": format!("{:?}", other_view.state),
            "new_call_outcomes": other_outcomes,
            "old_terminal": format!("{:?}", rt.task(&task.id).unwrap().state),
        })
    );
    assert_ne!(other_task.id, task.id);
    assert_ne!(
        rt.task(&other_task.id).unwrap().state,
        TaskState::Completed,
        "a new task must not complete while the workspace owner is unresolved"
    );
    assert!(
        other_outcomes
            .iter()
            .any(|outcome| outcome.contains("workspace mutation requires original owner recovery")),
        "the new task must be refused by the real owner-recovery guard: {other_outcomes:?}"
    );
    assert!(
        !other_outcomes
            .iter()
            .any(|outcome| outcome.contains("Completed")),
        "the new task must never be handed the old successful result: {other_outcomes:?}"
    );
    assert_eq!(other_view.state, SessionState::NeedsReconciliation);
    // The cancelled session is still exactly as it was.
    assert_eq!(rt.task(&task.id).unwrap(), terminal_before);
    let cells_end = cell_bodies(dir.path());
    assert!(
        cells_before.iter().all(|body| cells_end.contains(body)),
        "the cancelled session's cell records must still be present, unchanged"
    );
}
