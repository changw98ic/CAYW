//! Real SIGKILL at the Core start boundary of a *registered* process scope.
//!
//! `test.run` is a real process capability. `CallScope::start` registers the
//! process scope and its original supervisor *before* the existing
//! `core.start.before` / `core.start.committed` hooks, while the leaf executable
//! is only dispatched after `start` returns. Killing at either hook therefore
//! proves the registered-but-never-launched boundary: the durable scope and
//! supervisor must survive the crash, `recover` must refuse until a trusted host
//! verifies that the recorded supervisor incarnation stopped, and no process,
//! model request or Cell intent may be replayed.
//!
//! The already-launched process scenario (a real PID tree that must be observed
//! stopped) is covered by `process_cleanup.rs`; this test does not repeat it.
#![cfg(all(feature = "fault-injection", target_os = "macos"))]

#[allow(dead_code)]
#[path = "fixtures/recovery.rs"]
mod fixture;

use cayw_ai::CancellationToken;
use cayw_cells::{CellRecord, IntentStatus};
use cayw_core::{
    CapabilityCallRecord, CapabilityOutcome, CapabilityStage, CoreEvent, EventRecord, SessionState,
};
use cayw_host::ProcessIdentity;
use cayw_requests::HostActor;
use cayw_runtime::{
    CancelTarget, ProcessCleanupInspection, Runtime, RuntimeError, SessionView, TaskRecord,
    TaskState,
};
use fixture::{Hook, Identity, Result, WATCHDOG};
use futures::FutureExt;
use rusqlite::{Connection, OpenFlags, types::ValueRef};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    fs::{self, File},
    os::unix::process::ExitStatusExt,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

const BINARY: &str = env!("CARGO_BIN_EXE_cayw-runtime-recovery-fixture");
const PROCESS_BINARY: &str = env!("CARGO_BIN_EXE_cayw-runtime-process-fixture");
const POINTS: &[&str] = &["core.start.before", "core.start.committed"];
const EVIDENCE: &str = "Test host killed and reaped the original Runtime process; independent inspection \
     confirms the recorded supervisor incarnation is gone and the leaf profile was never launched";

/// Hang guard only. Even unwinding kills and reaps the exact owned child.
struct Process {
    child: Arc<Mutex<Child>>,
    expired: Arc<AtomicBool>,
    stop: Option<mpsc::Sender<()>>,
    watchdog: Option<thread::JoinHandle<()>>,
}
impl Process {
    fn start(root: &Path, point: &str) -> Result<Self> {
        let child = Command::new(BINARY)
            .args(["run", "process"])
            .arg(root)
            .arg(point)
            .env("CAYW_PROCESS_FIXTURE", PROCESS_BINARY)
            .stdin(Stdio::null())
            .stdout(Stdio::from(File::create(root.join("child.stdout"))?))
            .stderr(Stdio::from(File::create(root.join("child.stderr"))?))
            .spawn()?;
        let child = Arc::new(Mutex::new(child));
        let expired = Arc::new(AtomicBool::new(false));
        let (sender, receiver) = mpsc::channel();
        let watched = child.clone();
        let timed_out = expired.clone();
        let root = root.to_path_buf();
        let watchdog = thread::spawn(move || {
            if receiver.recv_timeout(WATCHDOG) == Err(mpsc::RecvTimeoutError::Timeout) {
                timed_out.store(true, Ordering::SeqCst);
                let mut child = watched.lock().expect("watchdog child lock");
                let _ = child.kill();
                if let Ok(status) = child.wait() {
                    let _ =
                        fixture::write_json(&root.join("watchdog-exit.json"), &exit_json(status));
                }
            }
        });
        Ok(Self {
            child,
            expired,
            stop: Some(sender),
            watchdog: Some(watchdog),
        })
    }
    fn pid(&self) -> u32 {
        self.child.lock().expect("owned child lock").id()
    }
    fn ready(&self, root: &Path, point: &str) -> Result<Value> {
        let deadline = Instant::now() + WATCHDOG;
        loop {
            if root.join("ready.json").exists() {
                let ready = fixture::read_json(&root.join("ready.json"))?;
                assert_eq!(ready["checkpoint"], point);
                assert_eq!(ready["hook"]["point"], point);
                assert_eq!(ready["pid"], self.pid());
                assert!(
                    !self.expired.load(Ordering::SeqCst),
                    "watchdog killed the child"
                );
                assert!(
                    self.child.lock().unwrap().try_wait()?.is_none(),
                    "ready child exited"
                );
                return Ok(ready);
            }
            if let Some(status) = self.child.lock().unwrap().try_wait()? {
                fixture::write_json(
                    &root.join("child-exit-before-checkpoint.json"),
                    &exit_json(status),
                )?;
                return Err(format!(
                    "child exited before {point}: {status}; stderr: {}",
                    fs::read_to_string(root.join("child.stderr"))?,
                )
                .into());
            }
            if Instant::now() >= deadline || self.expired.load(Ordering::SeqCst) {
                return Err(
                    format!("watchdog waiting for {point}; evidence {}", root.display()).into(),
                );
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
    fn stop_watchdog(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(watchdog) = self.watchdog.take() {
            watchdog.join().expect("watchdog thread panicked");
        }
    }
    fn kill_wait(&mut self) -> Result<ExitStatus> {
        self.stop_watchdog();
        assert!(
            !self.expired.load(Ordering::SeqCst),
            "watchdog expired before planned kill"
        );
        let mut child = self.child.lock().unwrap();
        assert!(
            child.try_wait()?.is_none(),
            "child exited before planned SIGKILL"
        );
        child.kill()?;
        let status = child.wait()?;
        assert_eq!(
            status.signal(),
            Some(9),
            "actual child exit was not SIGKILL"
        );
        Ok(status)
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        self.stop_watchdog();
        if let Ok(mut child) = self.child.lock() {
            if child.try_wait().ok().flatten().is_none() {
                let _ = child.kill();
            }
            let _ = child.wait();
        }
    }
}

/// Covers synchronous parent observation/recovery. A regression that deadlocks
/// after the child was reaped saves the failure instead of hanging the harness.
struct WindowWatchdog {
    stop: Option<mpsc::Sender<()>>,
    worker: Option<thread::JoinHandle<()>>,
}
impl WindowWatchdog {
    fn start(root: &Path, point: &str) -> Self {
        let (sender, receiver) = mpsc::channel();
        let root = root.to_owned();
        let point = point.to_owned();
        let worker = thread::spawn(move || {
            if receiver.recv_timeout(WATCHDOG) == Err(mpsc::RecvTimeoutError::Timeout) {
                let _ = fixture::write_json(
                    &root.join("parent-watchdog.json"),
                    &json!({
                        "pid":std::process::id(),"checkpoint":point,
                        "failure":"parent observation or recovery did not finish",
                        "watchdog_seconds":WATCHDOG.as_secs(),
                    }),
                );
                std::process::abort();
            }
        });
        Self {
            stop: Some(sender),
            worker: Some(worker),
        }
    }
}
impl Drop for WindowWatchdog {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
fn exit_json(status: ExitStatus) -> Value {
    json!({"display":status.to_string(),"code":status.code(),"signal":status.signal(),"success":status.success()})
}

fn readonly(path: &Path) -> Result<Connection> {
    let db = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    db.busy_timeout(WATCHDOG)?;
    db.execute_batch("PRAGMA query_only=ON; BEGIN;")?;
    Ok(db)
}
/// Capture actual row originals without opening a write connection or copying
/// an incomplete WAL database file.
fn database_originals(path: &Path) -> Result<Value> {
    let db = readonly(path)?;
    let names = db.prepare(
        "SELECT name FROM sqlite_schema WHERE type='table' AND substr(name,1,7)<>'sqlite_' ORDER BY name",
    )?.query_map([], |row| row.get::<_, String>(0))?.collect::<std::result::Result<Vec<_>, _>>()?;
    let mut tables = serde_json::Map::new();
    for name in names {
        let mut statement =
            db.prepare(&format!("SELECT * FROM \"{}\"", name.replace('"', "\"\"")))?;
        let columns: Vec<_> = statement
            .column_names()
            .iter()
            .map(|name| (*name).to_owned())
            .collect();
        let width = columns.len();
        let mut rows: Vec<Value> = statement
            .query_map([], |row| {
                let mut values = Vec::new();
                for column in 0..width {
                    values.push(match row.get_ref(column)? {
                        ValueRef::Null => Value::Null,
                        ValueRef::Integer(value) => json!(value),
                        ValueRef::Real(value) => json!(value),
                        ValueRef::Text(value) => json!(String::from_utf8_lossy(value)),
                        ValueRef::Blob(value) => json!({"blob":value}),
                    });
                }
                Ok(Value::Array(values))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows.sort_by_key(Value::to_string);
        tables.insert(name, json!({"columns":columns,"rows":rows}));
    }
    Ok(Value::Object(tables))
}
fn projection(root: &Path, identity: &Identity) -> Result<Value> {
    let db = readonly(&root.join("session.sqlite"))?;
    let raw: String = db.query_row(
        "SELECT projection FROM core_sessions WHERE id=?1",
        [&identity.session_id],
        |row| row.get(0),
    )?;
    Ok(serde_json::from_str(&raw)?)
}
fn cells(root: &Path, identity: &Identity) -> Result<Vec<CellRecord>> {
    let db = readonly(&root.join("session.sqlite"))?;
    let mut statement =
        db.prepare("SELECT body FROM cells_v1 WHERE session=?1 AND task=?2 ORDER BY id")?;
    let rows = statement.query_map([&identity.session_id, &identity.task_id], |row| {
        row.get::<_, String>(0)
    })?;
    rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
}
fn core_calls(projection: &Value) -> Vec<CapabilityCallRecord> {
    serde_json::from_value(projection["capabilities"].clone())
        .expect("canonical Core capability originals")
}
fn all_events(runtime: &Runtime, identity: &Identity, after: u64) -> Result<Vec<EventRecord>> {
    let mut cursor = after;
    let mut records = Vec::new();
    loop {
        let page = runtime.events(&identity.session_id, cursor, 17)?;
        if page.is_empty() {
            return Ok(records);
        }
        for record in &page {
            assert_eq!(record.session_id, identity.session_id);
            assert_eq!(
                record.cursor,
                cursor + 1,
                "session cursor has a gap or duplicate"
            );
            cursor = record.cursor;
        }
        records.extend(page);
    }
}
fn scope_record(root: &Path, task_id: &str) -> Result<Value> {
    let db = readonly(&root.join("session.sqlite"))?;
    Ok(db.query_row(
        "SELECT session,attempt,worker,fence,state,evidence
        FROM runtime_process_scopes WHERE task=?1",
        [task_id],
        |row| {
            Ok(json!({
                "task":task_id,
                "session":row.get::<_, String>(0)?,
                "attempt":row.get::<_, String>(1)?,
                "worker":row.get::<_, String>(2)?,
                "fence":row.get::<_, i64>(3)?,
                "state":row.get::<_, String>(4)?,
                "evidence":row.get::<_, Option<String>>(5)?,
            }))
        },
    )?)
}
fn supervisor_record(root: &Path, task_id: &str) -> Result<Value> {
    let db = readonly(&root.join("session.sqlite"))?;
    Ok(db.query_row(
        "SELECT session,attempt,worker,fence,
        pid,parent_pid,process_group,started_seconds,started_microseconds
        FROM runtime_process_supervisors WHERE task=?1",
        [task_id],
        |row| {
            Ok(json!({
                "session":row.get::<_, String>(0)?,
                "attempt":row.get::<_, String>(1)?,
                "worker":row.get::<_, String>(2)?,
                "fence":row.get::<_, i64>(3)?,
                "pid":row.get::<_, i64>(4)?,
                "parent_pid":row.get::<_, i64>(5)?,
                "process_group":row.get::<_, i64>(6)?,
                "started_seconds":row.get::<_, i64>(7)?,
                "started_microseconds":row.get::<_, i64>(8)?,
            }))
        },
    )?)
}
fn identity_from(value: &Value) -> ProcessIdentity {
    ProcessIdentity {
        pid: u32::try_from(value["pid"].as_u64().expect("supervisor pid")).unwrap(),
        parent_pid: u32::try_from(value["parent_pid"].as_u64().expect("supervisor parent"))
            .unwrap(),
        process_group: u32::try_from(value["process_group"].as_u64().expect("supervisor group"))
            .unwrap(),
        started_seconds: value["started_seconds"].as_u64().expect("supervisor start"),
        started_microseconds: u32::try_from(
            value["started_microseconds"]
                .as_u64()
                .expect("supervisor start microseconds"),
        )
        .unwrap(),
    }
}
fn text_or_null(path: &Path) -> Result<Value> {
    if path.exists() {
        Ok(json!(fs::read_to_string(path)?))
    } else {
        Ok(Value::Null)
    }
}
#[derive(Serialize)]
struct Snapshot {
    session: SessionView,
    task: TaskRecord,
    tasks: Vec<TaskRecord>,
    events: Vec<EventRecord>,
    core: Value,
    cells: Vec<CellRecord>,
    runtime_tables: Value,
    requests_tables: Value,
    provider_original: Vec<Value>,
    forbidden_provider_calls: Value,
    fixture_process: Value,
}
fn snapshot(runtime: &Runtime, root: &Path, identity: &Identity) -> Result<Snapshot> {
    let session = runtime.get(&identity.session_id)?;
    let task = runtime.task(&identity.task_id)?;
    let tasks = runtime.tasks(&identity.session_id)?;
    let events = all_events(runtime, identity, 0)?;
    assert_eq!(session.id, identity.session_id);
    assert_eq!(task.session_id, identity.session_id);
    assert_eq!(task.id, identity.task_id);
    assert_eq!(
        tasks,
        vec![task.clone()],
        "recovery must retain the one original task"
    );
    assert_eq!(
        session.cursor,
        events.last().map_or(0, |event| event.cursor)
    );
    let core = projection(root, identity)?;
    assert_eq!(core["id"], identity.session_id);
    assert_eq!(core["cursor"], session.cursor);
    Ok(Snapshot {
        session,
        task,
        tasks,
        events,
        core,
        cells: cells(root, identity)?,
        runtime_tables: database_originals(&root.join("session.sqlite"))?,
        requests_tables: database_originals(&root.join("session.sqlite.requests.sqlite"))?,
        provider_original: fixture::read_lines(&root.join("provider-original.jsonl"))?,
        forbidden_provider_calls: json!({
            "observer":fixture::read_lines(&root.join("provider-observer.jsonl"))?,
            "reopened":fixture::read_lines(&root.join("provider-reopened.jsonl"))?,
            "reopened_again":fixture::read_lines(&root.join("provider-reopened-again.jsonl"))?,
        }),
        fixture_process: json!({
            "leaf_pid":text_or_null(&root.join("leaf.pid"))?,
            "leaf_exists":root.join("leaf.pid").exists(),
            "release_exists":root.join("release").exists(),
        }),
    })
}
fn assert_same(before: &Snapshot, after: &Snapshot, reason: &str) {
    assert_eq!(
        serde_json::to_value(before).unwrap(),
        serde_json::to_value(after).unwrap(),
        "{reason}"
    );
}
fn assert_no_replay(before: &Snapshot, after: &Snapshot) {
    assert_eq!(
        before.provider_original, after.provider_original,
        "the original model was invoked again"
    );
    assert_eq!(
        after.forbidden_provider_calls,
        json!({"observer":[],"reopened":[],"reopened_again":[]})
    );
    assert_eq!(before.task.id, after.task.id);
    assert_eq!(before.task.worker_id, after.task.worker_id);
    assert_eq!(before.task.attempt_id, after.task.attempt_id);
    assert!(
        after.events.starts_with(&before.events),
        "recovery rewrote the original event prefix"
    );
    assert_eq!(
        after.runtime_tables["runtime_process_identities"]["rows"],
        json!([]),
        "recovery invented a launched process identity"
    );
    assert_eq!(
        after.fixture_process,
        json!({"leaf_pid":null,"leaf_exists":false,"release_exists":false}),
        "the leaf profile must never have launched"
    );
}
fn validate_live(
    root: &Path,
    point: &str,
    identity: &Identity,
    live: &Snapshot,
    supervisor: &Value,
    scope: &Value,
) -> Result<()> {
    let hooks: Vec<Hook> = fixture::read_lines(&root.join("hooks.jsonl"))?
        .into_iter()
        .map(serde_json::from_value)
        .collect::<std::result::Result<_, _>>()?;
    for (index, hook) in hooks.iter().enumerate() {
        assert_eq!(hook.sequence, index as u64 + 1);
    }
    assert!(
        hooks.iter().any(|hook| hook.point == point),
        "ready has no original hook"
    );
    assert!(
        !live.provider_original.is_empty(),
        "the offline Provider was never called"
    );
    assert_eq!(
        live.runtime_tables["runtime_process_identities"]["rows"],
        json!([]),
        "the leaf process must not have launched before the start boundary"
    );
    assert_eq!(
        live.fixture_process,
        json!({"leaf_pid":null,"leaf_exists":false,"release_exists":false})
    );

    // The durable scope still belongs to the exact original task and lease.
    let task = &live.task;
    assert_eq!(task.state, TaskState::Running);
    assert_eq!(scope["task"], identity.task_id);
    assert_eq!(scope["session"], identity.session_id);
    assert_eq!(scope["attempt"].as_str(), task.attempt_id.as_deref());
    assert_eq!(scope["worker"].as_str(), task.worker_id.as_deref());
    assert_eq!(scope["fence"], json!(task.fence));
    assert_eq!(scope["state"], "open");
    assert_eq!(scope["evidence"], Value::Null);
    assert_eq!(supervisor["session"], identity.session_id);
    assert_eq!(supervisor["attempt"].as_str(), task.attempt_id.as_deref());
    assert_eq!(supervisor["worker"].as_str(), task.worker_id.as_deref());
    assert_eq!(supervisor["fence"], json!(task.fence));

    let calls = core_calls(&live.core);
    assert_eq!(calls.len(), 1, "fixture must have one process capability");
    let call = &calls[0];
    assert_eq!(call.name, "test.run");
    assert_eq!(call.arguments, fixture::process_arguments());
    assert_eq!(call.task_id, identity.task_id);
    assert_eq!(call.lease.session_id, identity.session_id);
    assert_eq!(task.worker_id.as_ref(), Some(&call.lease.worker_id));
    assert_eq!(task.attempt_id.as_ref(), Some(&call.lease.attempt_id));
    assert!(call.outcome.is_none());
    match point {
        "core.start.before" => assert_eq!(call.stage, CapabilityStage::Planned),
        "core.start.committed" => assert_eq!(call.stage, CapabilityStage::Started),
        _ => panic!("unexpected checkpoint {point}"),
    }
    for hook in hooks.iter().filter(|hook| hook.point == point) {
        assert_eq!(hook.data["task_id"], identity.task_id);
        assert_eq!(hook.data["name"], "test.run");
        assert_eq!(hook.data["arguments"], fixture::process_arguments());
        assert_eq!(hook.data["key"], serde_json::to_value(&call.key)?);
        assert_eq!(hook.data["lease"], serde_json::to_value(&call.lease)?);
    }

    assert_eq!(live.cells.len(), 1, "one original Cell execution");
    let cell = live
        .cells
        .iter()
        .find(|cell| cell.cell_id == call.key.execution_id)
        .expect("the process call has no original Cells execution");
    assert_eq!(cell.source, fixture::PROCESS_SCRIPT);
    assert_eq!(cell.source_hash, call.source_hash);
    assert_eq!(cell.parent_call_id, call.parent_call_id);
    assert_eq!(
        cell.identity.permission_context,
        format!("{}:{}", call.lease.worker_id, call.lease.fence)
    );
    let intent = cell
        .calls
        .iter()
        .find(|intent| intent.sequence.checked_sub(1) == Some(call.key.sequence))
        .expect("the process call has no original Cells intent");
    assert_eq!(intent.name, "test.run");
    assert_eq!(intent.arguments, fixture::process_arguments());
    assert_eq!(intent.status, IntentStatus::Pending);
    Ok(())
}
fn validate_recovered(before: &Snapshot, after: &Snapshot) {
    assert_no_replay(before, after);
    let old = core_calls(&before.core);
    let recovered = core_calls(&after.core);
    assert_eq!(old.len(), 1);
    assert_eq!(recovered.len(), 1);
    let mut left = serde_json::to_value(&old[0]).unwrap();
    let mut right = serde_json::to_value(&recovered[0]).unwrap();
    for value in [&mut left, &mut right] {
        value.as_object_mut().unwrap().remove("outcome");
        value.as_object_mut().unwrap().remove("stage");
    }
    assert_eq!(
        left, right,
        "recovery changed the original process call identity"
    );
    assert_eq!(recovered[0].stage, CapabilityStage::Settled);

    assert_eq!(before.cells.len(), 1);
    assert_eq!(after.cells.len(), 1);
    assert_eq!(before.cells[0].execution(), after.cells[0].execution());
    assert_eq!(before.cells[0].source, after.cells[0].source);
    let intent = after.cells[0]
        .calls
        .iter()
        .find(|intent| intent.sequence.checked_sub(1) == Some(recovered[0].key.sequence))
        .expect("the recovered process call has no Cells intent");
    match &old[0].stage {
        CapabilityStage::Planned => {
            assert!(
                matches!(
                    recovered[0].outcome,
                    Some(CapabilityOutcome::NotExecuted { .. })
                ),
                "an unstarted process call was not proved unexecuted"
            );
            assert_eq!(intent.status, IntentStatus::NotExecuted);
            assert!(!after.cells[0].unresolved());
        }
        CapabilityStage::Started => {
            assert!(
                matches!(
                    recovered[0].outcome,
                    Some(CapabilityOutcome::OutcomeUnknown { .. })
                ),
                "a started process call was classified as known"
            );
            assert!(matches!(
                intent.status,
                IntentStatus::Pending | IntentStatus::OutcomeUnknown
            ));
            assert!(after.cells[0].unresolved());
        }
        _ => panic!("unexpected stage before recovery"),
    }
}
fn validate_expectations(point: &str, recovered: &SessionView, after: &Snapshot) -> Result<()> {
    let frozen = after.runtime_tables["cells_frozen_v1"]["rows"]
        .as_array()
        .expect("cells_frozen_v1 rows")
        .iter()
        .any(|row| row[0].as_str() == Some(recovered.id.as_str()));
    match point {
        "core.start.before" => {
            assert_eq!(recovered.state, SessionState::Idle);
            assert!(recovered.attempt.is_none());
            assert!(recovered.unresolved_capabilities.is_empty());
            assert!(!frozen, "a known recovery must thaw the frozen session");
        }
        "core.start.committed" => {
            assert_eq!(recovered.state, SessionState::NeedsReconciliation);
            assert_eq!(recovered.unresolved_capabilities.len(), 1);
            assert_eq!(recovered.unresolved_capabilities[0].name, "test.run");
            assert!(frozen, "an unknown process outcome must stay frozen");
        }
        _ => panic!("unexpected checkpoint {point}"),
    }
    assert!(
        after
            .events
            .iter()
            .any(|record| matches!(&record.event, CoreEvent::AttemptRecovered { .. })),
        "recovery did not record the original attempt as recovered"
    );
    Ok(())
}
/// The trusted host's independent verification of the *registered* scope. The
/// leaf process was never launched, so no process identity exists; the recorded
/// supervisor incarnation must be provably gone.
fn verify_lost(
    inspection: &ProcessCleanupInspection,
    target: &CancelTarget,
    original: &CapabilityCallRecord,
    supervisor: &ProcessIdentity,
) -> std::result::Result<(), String> {
    if inspection.target != *target {
        return Err("cleanup inspection differs from the original task target".into());
    }
    if inspection.lease != original.lease {
        return Err("cleanup inspection differs from the original Core lease".into());
    }
    if inspection.supervisor != *supervisor {
        return Err("cleanup inspection differs from the recorded supervisor".into());
    }
    if !inspection.processes.is_empty() {
        return Err("a prelaunch scope must not claim launched process identities".into());
    }
    match inspection.supervisor.is_running() {
        Ok(false) => Ok(()),
        Ok(true) => Err("the recorded supervisor incarnation is still running".into()),
        Err(error) => Err(format!(
            "cannot observe the recorded supervisor incarnation: {error}"
        )),
    }
}

async fn run_window(root: &Path, point: &str) -> Result<()> {
    fs::create_dir(root)?;
    fs::create_dir(root.join("workspace"))?;
    let program = Path::new(PROCESS_BINARY);
    // Open every observer connection before the child can hold a transaction.
    let observer = fixture::open_process(root, "observer", program)?;
    let session = observer.create(Some(format!("registered process scope killed at {point}")))?;
    let task = observer.enqueue(&session.id, "offline process profile through Luau and Host")?;
    let identity = Identity {
        session_id: session.id,
        task_id: task.id.clone(),
    };
    fixture::write_json(&root.join("identity.json"), &identity)?;

    let mut child = Process::start(root, point)?;
    let ready = match child.ready(root, point) {
        Ok(ready) => ready,
        Err(error) => {
            let originals = snapshot(&observer, root, &identity)?;
            fixture::write_json(
                &root.join("unreached-checkpoint-originals.json"),
                &originals,
            )?;
            return Err(error);
        }
    };
    assert_eq!(ready["identity"], serde_json::to_value(&identity)?);
    let hook: Hook = serde_json::from_value(ready["hook"].clone())?;
    assert_eq!(hook.point, point);

    // The child observed its own supervisor when it registered the scope.
    let supervisor = supervisor_record(root, &identity.task_id)?;
    let observed = ProcessIdentity::observe(child.pid())?;
    assert_eq!(identity_from(&supervisor), observed);
    let scope = scope_record(root, &identity.task_id)?;
    assert_eq!(scope["state"], "open");
    let current = observer.task(&identity.task_id)?;
    assert_eq!(current.state, TaskState::Running);
    assert!(current.worker_id.is_some() && current.attempt_id.is_some());

    let before = snapshot(&observer, root, &identity)?;
    fixture::write_json(&root.join("live-originals.json"), &before)?;
    validate_live(root, point, &identity, &before, &supervisor, &scope)?;

    let refusal = observer.recover(&identity.session_id);
    fixture::write_json(
        &root.join("live-recover.json"),
        &json!({
            "result":refusal.as_ref().err().map(ToString::to_string),
            "returned_session":refusal.as_ref().ok(),
        }),
    )?;
    assert!(
        matches!(refusal, Err(RuntimeError::Busy)),
        "a live original supervisor must refuse recovery"
    );
    assert_same(
        &before,
        &snapshot(&observer, root, &identity)?,
        "live Busy refusal changed original facts",
    );

    let _parent_watchdog = WindowWatchdog::start(root, point);
    let status = child.kill_wait()?;
    fixture::write_json(&root.join("child-exit.json"), &exit_json(status))?;
    drop(child);
    drop(observer);
    assert!(
        !observed.is_running()?,
        "the killed supervisor incarnation is still running"
    );

    let reopened = fixture::open_process(root, "reopened", program)?;
    let opened = snapshot(&reopened, root, &identity)?;
    fixture::write_json(&root.join("reopen-before-recover.json"), &opened)?;
    assert_same(
        &before,
        &opened,
        "opening a Runtime after SIGKILL altered original facts",
    );
    assert_eq!(
        scope_record(root, &identity.task_id)?["state"],
        "open",
        "opening must preserve the unconfirmed process scope"
    );

    let error = reopened.recover(&identity.session_id).unwrap_err();
    fixture::write_json(
        &root.join("recovery-required.json"),
        &json!({"error":error.to_string()}),
    )?;
    assert!(
        matches!(
            error,
            RuntimeError::RecoveryRequired {
                outcome_unknown: true,
                ..
            }
        ),
        "an open process scope must block recovery"
    );
    assert_same(
        &before,
        &snapshot(&reopened, root, &identity)?,
        "refused recovery fenced original facts",
    );
    assert!(
        reopened
            .drain_task_processes(&identity.task_id)
            .await
            .is_err(),
        "a new Runtime cannot prove the old process cleanup"
    );
    assert_same(
        &before,
        &snapshot(&reopened, root, &identity)?,
        "rejecting drain_task_processes changed original process facts",
    );
    assert_eq!(reopened.health()?.running, 1);

    // Trusted host evidence. This confirms resource cleanup only; no business
    // outcome is reconciled and no capacity is released here.
    let reopened_task = reopened.task(&identity.task_id)?;
    let target = reopened_task.cancel_target();
    let original = core_calls(&opened.core)
        .into_iter()
        .next()
        .expect("the registered process call must be durable");
    Runtime::confirm_lost_processes_stopped(
        root.join("session.sqlite"),
        fixture::process_config(root, program),
        &target,
        &HostActor::new("fixture-host")?,
        EVIDENCE,
        |inspection| verify_lost(inspection, &target, &original, &observed),
    )?;
    let verified = scope_record(root, &identity.task_id)?;
    assert_eq!(verified["state"], "host_verified");
    assert_eq!(verified["evidence"], EVIDENCE);
    assert_eq!(
        reopened.health()?.running,
        1,
        "cleanup evidence alone must not release the task"
    );
    let audit = reopened.audit(&identity.session_id, 0, 1000)?;
    let stopped: Vec<_> = audit
        .iter()
        .filter(|event| event.kind == "processes_stopped")
        .collect();
    assert_eq!(stopped.len(), 1, "exactly one cleanup record");
    assert_eq!(stopped[0].data["state"], "host_verified");
    assert_eq!(stopped[0].data["actor"], "fixture-host");
    assert_eq!(stopped[0].task_id.as_ref(), Some(&identity.task_id));

    let ledger_before = reopened.model_requests(&identity.session_id)?;
    assert!(
        !ledger_before.requests.is_empty(),
        "the offline fixture model request must be accounted before recovery"
    );
    assert!(
        !ledger_before.budget_unknown,
        "this fixture reports complete usage; recovery must not create a model unknown hold"
    );
    let recovered = reopened.recover(&identity.session_id)?;
    let after = snapshot(&reopened, root, &identity)?;
    fixture::write_json(&root.join("recovered-originals.json"), &after)?;
    validate_recovered(&before, &after);
    validate_expectations(point, &recovered, &after)?;
    assert_eq!(
        ledger_before,
        reopened.model_requests(&identity.session_id)?,
        "final recovery changed the reported fixture model-request ledger"
    );
    assert_eq!(reopened.health()?.running, 0);
    assert_eq!(
        reopened.task(&identity.task_id)?.state,
        TaskState::Interrupted
    );
    assert!(
        after.task.fence > before.task.fence,
        "recovery must fence the original queue lease: before={}, after={}",
        before.task.fence,
        after.task.fence
    );
    let scope_after = scope_record(root, &identity.task_id)?;
    let supervisor_after = supervisor_record(root, &identity.task_id)?;
    assert_eq!(
        scope_after["session"], scope["session"],
        "recovery changed the original scope session identity"
    );
    assert_eq!(
        scope_after["attempt"], scope["attempt"],
        "recovery changed the original scope attempt identity"
    );
    assert_eq!(
        scope_after["worker"], scope["worker"],
        "recovery changed the original scope worker identity"
    );
    assert_eq!(
        scope_after["fence"], scope["fence"],
        "recovery must retain the original scope fence"
    );
    assert_eq!(scope_after["fence"], json!(before.task.fence));
    assert_eq!(
        supervisor_after, supervisor,
        "recovery must retain the original supervisor identity and fence"
    );
    let tail = all_events(&reopened, &identity, before.session.cursor)?;
    assert_eq!(
        tail.as_slice(),
        &after.events[before.events.len()..],
        "reconnect cursor duplicated or lost events"
    );

    let replay = reopened
        .resume(&identity.task_id, CancellationToken::new())
        .await;
    assert!(replay.is_err(), "an interrupted process task was replayed");
    if after.session.state == SessionState::NeedsReconciliation {
        assert!(matches!(
            reopened.enqueue(&identity.session_id, "must stay frozen"),
            Err(RuntimeError::NeedsReconciliation)
        ));
    }
    let after_refusal = snapshot(&reopened, root, &identity)?;
    fixture::write_json(&root.join("replay-refusal-originals.json"), &after_refusal)?;
    assert_same(
        &after,
        &after_refusal,
        "refusing task replay changed recovered facts",
    );
    drop(reopened);

    let again = fixture::open_process(root, "reopened-again", program)?;
    let second = snapshot(&again, root, &identity)?;
    fixture::write_json(&root.join("second-reopen-originals.json"), &second)?;
    assert_same(&after, &second, "second open changed recovered facts");
    if after.session.state == SessionState::NeedsReconciliation {
        again.recover(&identity.session_id)?;
        let still = snapshot(&again, root, &identity)?;
        fixture::write_json(&root.join("recover-again-originals.json"), &still)?;
        assert_no_replay(&after, &still);
        assert_eq!(still.session.state, SessionState::NeedsReconciliation);
        assert_eq!(
            still.session.unresolved_capabilities,
            after.session.unresolved_capabilities
        );
        assert!(matches!(
            again.enqueue(&identity.session_id, "unknown remains frozen"),
            Err(RuntimeError::NeedsReconciliation)
        ));
    }
    eprintln!(
        "PROCESS_RECOVERY_WINDOW_VERIFIED point={point} originals={}",
        root.display()
    );
    Ok(())
}

fn evidence_root(points: &[&str]) -> Result<PathBuf> {
    let base = std::env::var_os("CAYW_PROCESS_RECOVERY_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("cayw-process-recovery"));
    fs::create_dir_all(&base)?;
    let root = base.join(format!("process-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&root)?;
    fixture::write_json(
        &root.join("matrix-input.json"),
        &json!({
            "mode":"process",
            "checkpoints":points,
            "fixture_binary":BINARY,
            "process_binary":PROCESS_BINARY,
            "architecture":std::env::consts::ARCH,
            "os":std::env::consts::OS,
            "fault_injection":true,
            "provider":"fixed offline script_response",
            "script":fixture::PROCESS_SCRIPT,
            "arguments":fixture::process_arguments(),
            "profile":"leaf",
            "kill_signal":9,
            "watchdog_seconds":WATCHDOG.as_secs(),
            "launched_before_checkpoint":false,
        }),
    )?;
    eprintln!("PROCESS_RECOVERY_EVIDENCE={}", root.display());
    Ok(root)
}

async fn matrix(points: &[&str]) -> Result<()> {
    let filter = std::env::var("CAYW_PROCESS_RECOVERY_POINT").ok();
    let points: Vec<_> = points
        .iter()
        .copied()
        .filter(|point| filter.as_ref().is_none_or(|filter| filter == point))
        .collect();
    assert!(!points.is_empty(), "checkpoint filter selected no windows");
    let root = evidence_root(&points)?;
    let mut failures = Vec::new();
    for point in &points {
        let window = root.join(point.replace('.', "-"));
        let outcome = std::panic::AssertUnwindSafe(run_window(&window, point))
            .catch_unwind()
            .await;
        let failure = match outcome {
            Ok(Ok(())) => continue,
            Ok(Err(error)) => error.to_string(),
            Err(payload) => payload
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| {
                    payload
                        .downcast_ref::<&str>()
                        .map(|message| (*message).to_owned())
                })
                .unwrap_or_else(|| "matrix assertion panicked".into()),
        };
        fixture::write_json(
            &window.join("window-failure.json"),
            &json!({
                "mode":"process","checkpoint":point,"failure":failure,
            }),
        )?;
        eprintln!(
            "PROCESS_RECOVERY_WINDOW_FAILED point={point} originals={}",
            window.display()
        );
        failures.push(json!({"checkpoint":point,"failure":failure,"originals":window}));
    }
    if !failures.is_empty() {
        fixture::write_json(&root.join("failed-windows.json"), &failures)?;
        return Err(format!(
            "{} process recovery windows failed; originals {}",
            failures.len(),
            root.display()
        )
        .into());
    }
    fixture::write_json(
        &root.join("executed-windows.json"),
        &json!({"mode":"process","checkpoints":points}),
    )?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn registered_process_scope_requires_host_cleanup_evidence_after_sigkill() -> Result<()> {
    matrix(POINTS).await
}
