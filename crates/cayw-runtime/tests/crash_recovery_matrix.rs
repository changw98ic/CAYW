//! Real SIGKILL boundaries through the public Runtime and authenticated IPC.
#![cfg(all(feature = "fault-injection", target_os = "macos"))]

#[allow(dead_code)]
#[path = "fixtures/recovery.rs"]
mod fixture;

use cayw_ai::CancellationToken;
use cayw_cells::{CellRecord, IntentStatus};
use cayw_core::{
    CapabilityCallRecord, CapabilityOutcome, CapabilityStage, CoreEvent, EventRecord, SessionState,
};
use cayw_runtime::{Runtime, RuntimeError, SessionView, TaskRecord};
use fixture::{Context, Hook, Identity, Result, WATCHDOG};
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
const SDK_POINTS: &[&str] = &[
    "cells.intent.before",
    "cells.intent.committed",
    "core.plan.before",
    "core.plan.committed",
    "core.start.before",
    "core.start.committed",
    "host.effect.before",
    "host.effect.visible",
    "host.effect.durable",
    "core.capability_terminal.before",
    "core.capability_terminal.committed",
    "cells.call_terminal.before",
    "cells.call_terminal.committed",
    "cells.response_ack.before",
    "cells.execution_terminal.committed",
    "core.execution_terminal.before",
    "core.execution_terminal.committed",
    "core.tool_result.before",
    "core.tool_result.committed",
    "core.attempt_terminal.before",
    "core.attempt_terminal.committed",
    "requests.task_end.before",
    "requests.task_end.committed",
    "queue.task_terminal.commit.before",
    "queue.task_terminal.commit.committed",
    "sdk.reply.before",
];
const IPC_POINTS: &[&str] = &[
    "cells.intent.committed",
    "core.start.committed",
    "host.effect.before",
    "host.effect.visible",
    "host.effect.durable",
    "core.capability_terminal.before",
    "core.capability_terminal.committed",
    "cells.call_terminal.committed",
    "core.tool_result.committed",
    "requests.task_end.before",
    "requests.task_end.committed",
    "queue.task_terminal.commit.before",
    "queue.task_terminal.commit.committed",
    "rpc.reply.before",
];

/// The watchdog is a hang guard, never a performance assertion. Even unwinding
/// kills and reaps the exact owned child, including a gate holding a DB lock.
struct Process {
    child: Arc<Mutex<Child>>,
    expired: Arc<AtomicBool>,
    stop: Option<mpsc::Sender<()>>,
    watchdog: Option<thread::JoinHandle<()>>,
}
impl Process {
    fn start(mode: &str, root: &Path, point: &str) -> Result<Self> {
        let child = Command::new(BINARY)
            .args(["run", mode])
            .arg(root)
            .arg(point)
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
    fn ready(&self, root: &Path, point: &str) -> Result<Value> {
        let deadline = Instant::now() + WATCHDOG;
        loop {
            if root.join("ready.json").exists() {
                let ready = fixture::read_json(&root.join("ready.json"))?;
                assert_eq!(ready["checkpoint"], point);
                assert_eq!(ready["pid"], self.child.lock().unwrap().id());
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
/// Covers synchronous parent recovery too. If a regression deadlocks after the
/// child has already been reaped, save the failure before terminating the test
/// process. There is no elapsed-time assertion on a successful operation.
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
/// Capture actual row originals, including Requests, without opening a write
/// connection or copying an incomplete WAL database file.
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
fn file_json(path: &Path) -> Result<Value> {
    if path.exists() {
        fixture::read_json(path)
    } else {
        Ok(Value::Null)
    }
}
fn effects(root: &Path) -> Result<Value> {
    Ok(json!({
        "entries":fixture::read_lines(&root.join("external-entries.jsonl"))?,
        "effect_count":usize::from(root.join("effect.json").exists()),
        "effect":file_json(&root.join("effect.json"))?,
        "receipt":file_json(&root.join("effect-receipt.json"))?,
    }))
}
fn forbidden_provider_calls(root: &Path) -> Result<Value> {
    Ok(json!({
        "observer":fixture::read_lines(&root.join("provider-observer.jsonl"))?,
        "reopened":fixture::read_lines(&root.join("provider-reopened.jsonl"))?,
        "reopened_again":fixture::read_lines(&root.join("provider-reopened-again.jsonl"))?,
    }))
}
#[derive(Serialize)]
struct Snapshot {
    session: SessionView,
    task: TaskRecord,
    tasks: Vec<TaskRecord>,
    events: Vec<EventRecord>,
    core: Value,
    cells: Vec<CellRecord>,
    databases: Value,
    effects: Value,
    provider_original: Vec<Value>,
    forbidden_provider_calls: Value,
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
        databases: json!({
            "core_cells_runtime":database_originals(&root.join("session.sqlite"))?,
            "requests":database_originals(&root.join("session.sqlite.requests.sqlite"))?,
        }),
        effects: effects(root)?,
        provider_original: fixture::read_lines(&root.join("provider-original.jsonl"))?,
        forbidden_provider_calls: forbidden_provider_calls(root)?,
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
        before.effects, after.effects,
        "recovery changed external entries, effects or receipts"
    );
    assert_eq!(
        before.provider_original, after.provider_original,
        "original model was invoked again"
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
}
fn assert_origin(data: &Value, call: &CapabilityCallRecord, identity: &Identity) {
    assert_eq!(data["session_id"], identity.session_id);
    assert_eq!(data["task_id"], identity.task_id);
    assert_eq!(data["lease"], serde_json::to_value(&call.lease).unwrap());
    assert_eq!(data["key"], serde_json::to_value(&call.key).unwrap());
    assert_eq!(data["parent_call_id"], call.parent_call_id);
    assert_eq!(data["source_hash"], call.source_hash);
    assert_eq!(data["name"], call.name);
    assert_eq!(data["arguments"], call.arguments);
    let execution = &data["execution"];
    assert_eq!(execution["cell_id"], call.key.execution_id);
    assert_eq!(execution["parent_call_id"], call.parent_call_id);
    assert_eq!(execution["source_hash"], call.source_hash);
    assert_eq!(
        execution["identity"],
        json!({
            "session_id":identity.session_id,"task_id":identity.task_id,
            "attempt_id":call.lease.attempt_id,
            "permission_context":format!("{}:{}", call.lease.worker_id, call.lease.fence),
        })
    );
}
fn validate_live_originals(
    root: &Path,
    point: &str,
    identity: &Identity,
    live: &Snapshot,
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
        "offline Provider was never called"
    );
    let calls = core_calls(&live.core);
    assert!(
        calls.len() <= 1,
        "fixture must have only one external capability"
    );
    for call in &calls {
        assert_eq!(call.name, "search.query");
        assert_eq!(call.arguments, fixture::arguments());
        assert_eq!(call.task_id, identity.task_id);
        assert_eq!(call.lease.session_id, identity.session_id);
        assert_eq!(live.task.worker_id.as_ref(), Some(&call.lease.worker_id));
        assert_eq!(live.task.attempt_id.as_ref(), Some(&call.lease.attempt_id));
        let cell = live
            .cells
            .iter()
            .find(|cell| cell.cell_id == call.key.execution_id)
            .expect("Core call has no original Cells execution");
        assert_eq!(cell.identity.session_id, identity.session_id);
        assert_eq!(cell.identity.task_id, call.task_id);
        assert_eq!(cell.identity.attempt_id, call.lease.attempt_id);
        assert_eq!(
            cell.identity.permission_context,
            format!("{}:{}", call.lease.worker_id, call.lease.fence)
        );
        assert_eq!(cell.source, fixture::SCRIPT);
        assert_eq!(cell.source_hash, call.source_hash);
        assert_eq!(cell.parent_call_id, call.parent_call_id);
        let intent = cell
            .calls
            .iter()
            .find(|intent| intent.sequence.checked_sub(1) == Some(call.key.sequence))
            .expect("Core call has no original one-based Cells intent");
        assert_eq!(intent.name, call.name);
        assert_eq!(intent.arguments, call.arguments);
        let planned = live
            .events
            .iter()
            .find_map(|record| match &record.event {
                CoreEvent::CapabilityPlanned { call: planned } if planned.key == call.key => {
                    Some(planned)
                }
                _ => None,
            })
            .expect("Core projection has no original plan event");
        for hook in hooks
            .iter()
            .filter(|hook| hook.point == "core.plan.committed")
        {
            assert_origin(&hook.data, call, identity);
            assert_eq!(hook.data["call"], serde_json::to_value(planned)?);
        }
        for hook in hooks
            .iter()
            .filter(|hook| hook.point == "core.start.committed")
        {
            assert_origin(&hook.data, call, identity);
        }
    }
    let entries = live.effects["entries"]
        .as_array()
        .expect("actual entry log array");
    let before_dispatch = matches!(
        point,
        "cells.intent.before"
            | "cells.intent.committed"
            | "core.plan.before"
            | "core.plan.committed"
            | "core.start.before"
            | "core.start.committed"
    );
    let before_effect = before_dispatch || point == "host.effect.before";
    assert_eq!(
        entries.len(),
        usize::from(!before_dispatch),
        "actual external entry count at {point}"
    );
    assert_eq!(
        live.effects["effect_count"],
        usize::from(!before_effect),
        "actual visible effect count at {point}"
    );
    for entry in entries {
        assert_eq!(entry["entry_count"], 1);
        let started = live
            .events
            .iter()
            .find_map(|record| match &record.event {
                CoreEvent::CapabilityStarted { call } => Some(call),
                _ => None,
            })
            .expect("external entry has no durable Core started original");
        assert_origin(&entry["trusted_origin"]["plan"], started, identity);
        assert_origin(&entry["trusted_origin"]["start"], started, identity);
        assert_eq!(entry["arguments"], fixture::arguments());
    }
    if !before_effect {
        let receipt = &live.effects["receipt"];
        assert_eq!(receipt["trusted_origin"], entries[0]["trusted_origin"]);
        assert_eq!(
            live.effects["effect"]["trusted_origin"],
            receipt["trusted_origin"]
        );
        assert_eq!(live.effects["effect"]["result"], receipt["result"]);
        assert_eq!(receipt["entry_count"], 1);
        assert_eq!(receipt["arguments"], fixture::arguments());
        assert_eq!(
            receipt["effect_path"],
            root.join("effect.json").to_str().unwrap()
        );
        assert_eq!(
            receipt["receipt_path"],
            root.join("effect-receipt.json").to_str().unwrap()
        );
    }
    Ok(())
}
fn validate_recovery(before: &Snapshot, after: &Snapshot) {
    assert_no_replay(before, after);
    let old = core_calls(&before.core);
    let recovered = core_calls(&after.core);
    assert_eq!(old.len(), recovered.len(), "recovery invented a capability");
    let mut has_unknown = false;
    for call in &old {
        let next = recovered
            .iter()
            .find(|next| next.key == call.key)
            .expect("original call disappeared");
        let mut identity_before = serde_json::to_value(call).unwrap();
        let mut identity_after = serde_json::to_value(next).unwrap();
        for value in [&mut identity_before, &mut identity_after] {
            value.as_object_mut().unwrap().remove("outcome");
            value.as_object_mut().unwrap().remove("stage");
        }
        assert_eq!(
            identity_before, identity_after,
            "recovery changed original call identity"
        );
        match &call.outcome {
            Some(outcome) => assert_eq!(
                next.outcome.as_ref(),
                Some(outcome),
                "known terminal changed"
            ),
            None if call.stage == CapabilityStage::Started => {
                assert!(
                    matches!(next.outcome, Some(CapabilityOutcome::OutcomeUnknown { .. })),
                    "started call was classified as known"
                );
                has_unknown = true;
            }
            None => assert!(
                matches!(next.outcome, Some(CapabilityOutcome::NotExecuted { .. })),
                "unstarted call was not proved unexecuted"
            ),
        }
        assert_eq!(next.stage, CapabilityStage::Settled);
        if let Some(CapabilityOutcome::Completed { result }) = &next.outcome {
            assert_eq!(
                result, &before.effects["receipt"]["result"],
                "known Core result differs from the actual original service receipt"
            );
        }
    }
    assert_eq!(
        before.cells.len(),
        after.cells.len(),
        "recovery invented a cell"
    );
    for cell in &before.cells {
        let next = after
            .cells
            .iter()
            .find(|next| next.cell_id == cell.cell_id)
            .expect("original cell disappeared");
        assert_eq!(cell.execution(), next.execution());
        assert_eq!(cell.source, next.source);
        assert_eq!(cell.calls.len(), next.calls.len());
        assert!(
            next.outputs.starts_with(&cell.outputs),
            "existing Cells output prefix changed"
        );
        for intent in &cell.calls {
            let terminal = next
                .calls
                .iter()
                .find(|next| next.sequence == intent.sequence)
                .expect("original Cells intent disappeared");
            assert_eq!(terminal.name, intent.name);
            assert_eq!(terminal.arguments, intent.arguments);
            let core = recovered.iter().find(|call| {
                call.key.execution_id == cell.cell_id
                    && intent.sequence.checked_sub(1) == Some(call.key.sequence)
            });
            match core.and_then(|call| call.outcome.as_ref()) {
                Some(CapabilityOutcome::Completed { result }) => {
                    assert_eq!(terminal.status, IntentStatus::Succeeded);
                    assert_eq!(
                        terminal.result.as_ref(),
                        Some(result),
                        "known Core bytes were not reused in Cells"
                    );
                    assert!(!terminal.result_omitted);
                }
                Some(CapabilityOutcome::OutcomeUnknown { .. }) => {
                    has_unknown = true;
                    assert!(matches!(
                        terminal.status,
                        IntentStatus::Pending | IntentStatus::OutcomeUnknown
                    ));
                    assert!(terminal.result.is_none());
                    assert!(next.unresolved(), "unknown cell was released");
                }
                Some(CapabilityOutcome::NotExecuted { .. }) | None => {
                    assert_eq!(
                        terminal.status,
                        IntentStatus::NotExecuted,
                        "unstarted intent was not classified unexecuted"
                    );
                    assert!(terminal.result.is_none());
                }
                Some(CapabilityOutcome::Failed { .. }) => {
                    panic!("offline fixture unexpectedly failed its service")
                }
            }
        }
    }
    if has_unknown {
        assert_eq!(after.session.state, SessionState::NeedsReconciliation);
        assert!(!after.session.unresolved_capabilities.is_empty());
        let frozen_rows = &after.databases["core_cells_runtime"]["cells_frozen_v1"]["rows"];
        assert!(
            frozen_rows
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row[0] == after.session.id),
            "unknown Cells session is not frozen"
        );
    }
}

fn matrix_root(mode: &str, points: &[&str]) -> Result<PathBuf> {
    let base = std::env::var_os("CAYW_CRASH_MATRIX_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("cayw-runtime-crash-matrix"));
    fs::create_dir_all(&base)?;
    let root = base.join(format!("{mode}-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&root)?;
    fixture::write_json(
        &root.join("matrix-input.json"),
        &json!({
            "mode":mode,"checkpoints":points,"fixture_binary":BINARY,
            "architecture":std::env::consts::ARCH,"os":std::env::consts::OS,
            "fault_injection":true,"provider":"fixed offline script_response",
            "script":fixture::SCRIPT,"arguments":fixture::arguments(),
            "kill_signal":9,"watchdog_seconds":WATCHDOG.as_secs(),
            "live_busy_points":["core.start.committed","host.effect.before","host.effect.visible","host.effect.durable"],
        }),
    )?;
    eprintln!("CRASH_MATRIX_EVIDENCE={}", root.display());
    Ok(root)
}
async fn run_window(mode: &str, root: &Path, point: &str) -> Result<()> {
    fs::create_dir(root)?;
    fs::create_dir(root.join("workspace"))?;
    // Open every observer connection before the child can enter a checkpoint
    // that holds an IMMEDIATE transaction. Later observations are read-only.
    let observer = fixture::open(root, "observer", Arc::new(Context::default()))?;
    let session = observer.create(Some(format!("real kill at {point}")))?;
    let task = observer.enqueue(&session.id, "offline external effect through Luau and Host")?;
    let identity = Identity {
        session_id: session.id,
        task_id: task.id,
    };
    fixture::write_json(&root.join("identity.json"), &identity)?;
    let mut child = Process::start(mode, root, point)?;
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
    assert!(
        fixture::read_lines(&root.join("hooks.jsonl"))?
            .iter()
            .any(|original| original == &ready["hook"])
    );
    let before = snapshot(&observer, root, &identity)?;
    fixture::write_json(&root.join("live-originals.json"), &before)?;
    validate_live_originals(root, point, &identity, &before)?;
    if mode == "ipc" {
        assert!(
            root.join("ipc-authenticated-health.json").exists(),
            "no authenticated IPC round trip"
        );
        assert_eq!(
            fixture::read_json(&root.join("ipc-authenticated-health.json"))?["status"],
            "ok"
        );
    }
    if matches!(
        point,
        "core.start.committed"
            | "host.effect.before"
            | "host.effect.visible"
            | "host.effect.durable"
    ) {
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
            "live original worker must refuse recovery"
        );
        let after_refusal = snapshot(&observer, root, &identity)?;
        fixture::write_json(&root.join("live-after-refusal.json"), &after_refusal)?;
        assert_same(
            &before,
            &after_refusal,
            "live Busy refusal changed original facts",
        );
    }
    let _parent_watchdog = WindowWatchdog::start(root, point);
    let status = child.kill_wait()?;
    fixture::write_json(&root.join("child-exit.json"), &exit_json(status))?;
    drop(child);
    drop(observer);
    let reopened = fixture::open(root, "reopened", Arc::new(Context::default()))?;
    let opened = snapshot(&reopened, root, &identity)?;
    fixture::write_json(&root.join("reopen-before-recover.json"), &opened)?;
    assert_same(
        &before,
        &opened,
        "opening a Runtime after SIGKILL altered original facts",
    );
    let recovered = reopened.recover(&identity.session_id);
    fixture::write_json(
        &root.join("recover-result.json"),
        &match &recovered {
            Ok(session) => json!({"session":session}),
            Err(error) => json!({"error":error.to_string()}),
        },
    )?;
    recovered?;
    let after = snapshot(&reopened, root, &identity)?;
    fixture::write_json(&root.join("recovered-originals.json"), &after)?;
    validate_recovery(&before, &after);
    let tail = all_events(&reopened, &identity, before.session.cursor)?;
    assert_eq!(
        tail.as_slice(),
        &after.events[before.events.len()..],
        "reconnect cursor duplicated or lost events"
    );
    let replay = reopened
        .resume(&identity.task_id, CancellationToken::new())
        .await;
    assert!(replay.is_err(), "original nonqueued task was replayed");
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
        "refusing task replay changed original facts",
    );
    drop(reopened);
    let again = fixture::open(root, "reopened-again", Arc::new(Context::default()))?;
    let reopened_again = snapshot(&again, root, &identity)?;
    fixture::write_json(&root.join("second-reopen-originals.json"), &reopened_again)?;
    assert_same(
        &after,
        &reopened_again,
        "second open changed recovered facts",
    );
    if after.session.state == SessionState::NeedsReconciliation {
        again.recover(&identity.session_id)?;
        let still_unknown = snapshot(&again, root, &identity)?;
        fixture::write_json(&root.join("recover-again-originals.json"), &still_unknown)?;
        assert_no_replay(&after, &still_unknown);
        assert_eq!(
            still_unknown.session.state,
            SessionState::NeedsReconciliation
        );
        assert_eq!(
            still_unknown.session.unresolved_capabilities,
            after.session.unresolved_capabilities
        );
        assert!(matches!(
            again.enqueue(&identity.session_id, "unknown remains frozen"),
            Err(RuntimeError::NeedsReconciliation)
        ));
    }
    eprintln!(
        "CRASH_WINDOW_VERIFIED mode={mode} point={point} originals={}",
        root.display()
    );
    Ok(())
}
async fn matrix(mode: &str, points: &[&str]) -> Result<()> {
    let filter = std::env::var("CAYW_CRASH_CHECKPOINT").ok();
    let points: Vec<_> = points
        .iter()
        .copied()
        .filter(|point| filter.as_ref().is_none_or(|filter| filter == point))
        .collect();
    assert!(!points.is_empty(), "checkpoint filter selected no windows");
    let root = matrix_root(mode, &points)?;
    let mut failures = Vec::new();
    for point in &points {
        let window = root.join(point.replace('.', "-"));
        let outcome = std::panic::AssertUnwindSafe(run_window(mode, &window, point))
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
                "mode":mode,"checkpoint":point,"failure":failure,
            }),
        )?;
        eprintln!(
            "CRASH_WINDOW_FAILED mode={mode} point={point} originals={}",
            window.display()
        );
        failures.push(json!({"checkpoint":point,"failure":failure,"originals":window}));
    }
    if !failures.is_empty() {
        fixture::write_json(&root.join("failed-windows.json"), &failures)?;
        return Err(format!(
            "{} real kill windows failed; originals {}",
            failures.len(),
            root.display()
        )
        .into());
    }
    // Each claim above is derived from saved events, DB rows, effects and the
    // waited OS exit. This receipt lists only the actual executed windows.
    fixture::write_json(
        &root.join("executed-windows.json"),
        &json!({"mode":mode,"checkpoints":points}),
    )?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sdk_real_sigkill_matrix_preserves_original_facts_without_replay() -> Result<()> {
    matrix("sdk", SDK_POINTS).await
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn authenticated_ipc_real_sigkill_matrix_preserves_original_facts_without_replay()
-> Result<()> {
    matrix("ipc", IPC_POINTS).await
}
