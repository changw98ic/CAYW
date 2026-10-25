//! Issue 11 item 6 boundary probe: a real Host terminal process that is still
//! alive after `exec_command` returned must keep the task's admitted execution
//! slot while its next `write_stdin` call waits for explicit permission.
//!
//! It adds no product code, no fixture binary, no third-party crate and no
//! shell. The host fixes one terminal profile whose program is this test
//! binary. The child branch writes its PID marker, waits for a release file
//! with a guard timeout only, writes a completion marker, prints its stdout and
//! exits 0.
//!
//! K = 2, C = 3, A = 2. Optional receipts:
//! CAYW_SCHEDULER_TOOLS_EVIDENCE_DIR, falling back to
//! CAYW_SCHEDULER_EVIDENCE_DIR.
#![cfg(target_os = "macos")]

use cayw_ai::{
    AssistantEvent, AssistantStream, CancellationToken, Message, Model, ModelRequest, Provider,
    UsageStatus,
};
use cayw_core::{
    ApprovalDecision, ApprovalRequest, CapabilityAuthorization, CapabilityCallRecord,
    CapabilityOutcome, CoreEvent, EventRecord,
};
use cayw_host::{
    HostConfig, ProcessIdentity, ProcessIo, Profile, ProfileKind, TerminalConfig, TerminalLimits,
};
use cayw_runtime::{
    ApprovalChoice, AuditRecord, ModelCostStatus, ModelRequestTerminal, ModelReservationPolicy,
    Runtime, RuntimeConfig, RuntimeError, ScopedPermission, SessionView, TaskRecord, TaskState,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::task::JoinHandle;

const TEST_NAME: &str = "terminal_process_live_across_permission_park_must_keep_its_slot";
const DUAL_TEST: &str = "two_real_host_tools_run_together_and_only_one_is_cancelled";
const CHILD_ENV: &str = "CAYW_SCHEDULER_TOOLS_CHILD";
const CHILD_ROOT: &str = "CAYW_SCHEDULER_TOOLS_ROOT";
const CHILD_ROLE: &str = "CAYW_SCHEDULER_TOOLS_ROLE";
const PROFILE_A: &str = "term-a";
const PROFILE_B: &str = "term-b";
const PROFILE_C: &str = "term-c";
const DUAL_PREFIX: &str = "dual-";
const LIVE_DUAL_PREFIX: &str = "live-dual-";

/// One Luau cell starts the real terminal process and immediately waits for a
/// permission that is deliberately not granted. The wait is the point where
/// the scheduler must not hand the slot to another task.
const SCRIPT: &str = "local t = cayw.call('exec_command',{profile='term-a'})\n\
local w = cayw.call('write_stdin',{session_id=t.session_id,action='wait',wait_ms=60000,request_id='a-wait'})\n\
return {session=t.session_id,state=w.status.state}";

/// Task A runs one real terminal process, one approved wait and then one EOF
/// read: exec + wait + read, the same finite shape as B and C. In the safe
/// return mode the child exits 0 after the wait, and the read stays pending on
/// its exact permission while the unread output keeps the admitted slot. In
/// the live mode the parent cancels the original task while the approved wait
/// is still running against the live child. No repeating loop, no widened
/// instruction budget and no larger call budget are needed.
const DUAL_A_SCRIPT: &str = "local t = cayw.call('exec_command',{profile='term-a'})\n\
local w = cayw.call('write_stdin',{session_id=t.session_id,action='wait',wait_ms=60000,request_id='a-wait'})\n\
local r\n\
repeat\n\
  r = cayw.call('write_stdin',{session_id=t.session_id,action='read',cursor=0,max_bytes=65536,request_id='a-read'})\n\
until r.eof\n\
return {session=t.session_id,wait_state=w.status.state,text=r.text,eof=r.eof,next_cursor=r.next_cursor,consumed_bytes=r.consumed_bytes,output_cursor=r.status.output_cursor,exit_code=r.status.exit_code,termination=r.status.termination,output_complete=r.status.output_complete}";

const DUAL_B_SCRIPT: &str = "local t = cayw.call('exec_command',{profile='term-b'})\n\
local w = cayw.call('write_stdin',{session_id=t.session_id,action='wait',wait_ms=60000,request_id='b-wait'})\n\
local r\n\
repeat\n\
  r = cayw.call('write_stdin',{session_id=t.session_id,action='read',cursor=0,max_bytes=65536,request_id='b-read'})\n\
until r.eof\n\
return {session=t.session_id,wait_state=w.status.state,text=r.text,eof=r.eof,next_cursor=r.next_cursor,consumed_bytes=r.consumed_bytes,output_cursor=r.status.output_cursor,exit_code=r.status.exit_code,termination=r.status.termination,output_complete=r.status.output_complete}";

const DUAL_C_SCRIPT: &str = "local t = cayw.call('exec_command',{profile='term-c'})\n\
local w = cayw.call('write_stdin',{session_id=t.session_id,action='wait',wait_ms=60000,request_id='c-wait'})\n\
local r\n\
repeat\n\
  r = cayw.call('write_stdin',{session_id=t.session_id,action='read',cursor=0,max_bytes=65536,request_id='c-read'})\n\
until r.eof\n\
return {session=t.session_id,wait_state=w.status.state,text=r.text,eof=r.eof,next_cursor=r.next_cursor,consumed_bytes=r.consumed_bytes,output_cursor=r.status.output_cursor,exit_code=r.status.exit_code,termination=r.status.termination,output_complete=r.status.output_complete}";

type Worker = JoinHandle<Result<SessionView, RuntimeError>>;

#[derive(Clone, Debug)]
struct ModelCall {
    text: String,
    tool_result_turn: bool,
    messages: usize,
}

struct FixtureProvider {
    calls: Mutex<Vec<ModelCall>>,
}
impl Default for FixtureProvider {
    fn default() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
        }
    }
}
impl FixtureProvider {
    fn calls_for(&self, text: &str) -> usize {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call.text == text)
            .count()
    }
    fn calls(&self) -> Vec<ModelCall> {
        self.calls.lock().unwrap().clone()
    }
}
impl Provider for FixtureProvider {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let text = request
            .messages
            .iter()
            .rev()
            .find(|message| matches!(message, Message::User { .. }))
            .expect("every task has a user input")
            .text();
        let tool_result_turn = matches!(request.messages.last(), Some(Message::ToolResult(_)));
        self.calls.lock().unwrap().push(ModelCall {
            text: text.clone(),
            tool_result_turn,
            messages: request.messages.len(),
        });
        let message = cayw_runtime::fixture::script_response(
            &request,
            script_for(&text),
            &format!(
                "scheduler-tools-{}-{}",
                text.replace(' ', "-"),
                request.messages.len()
            ),
        );
        Box::pin(async_stream::stream! {
            yield AssistantEvent::Done { message };
        })
    }
}
fn script_for(text: &str) -> &'static str {
    match text {
        "tool a" => SCRIPT,
        "dual a" => DUAL_A_SCRIPT,
        "dual b" => DUAL_B_SCRIPT,
        "queued c" => DUAL_C_SCRIPT,
        other => panic!("unexpected task text {other}"),
    }
}

struct Project {
    root: PathBuf,
    _temporary: Option<tempfile::TempDir>,
}
impl Project {
    fn new(label: &str) -> Self {
        if let Some(parent) = evidence_dir() {
            fs::create_dir_all(&parent).unwrap();
            let temporary = tempfile::Builder::new()
                .prefix(&format!("{label}-"))
                .tempdir_in(&parent)
                .unwrap();
            let root = temporary.keep();
            fs::create_dir(root.join("workspace")).unwrap();
            Self {
                root,
                _temporary: None,
            }
        } else {
            let temporary = tempfile::tempdir().unwrap();
            fs::create_dir(temporary.path().join("workspace")).unwrap();
            Self {
                root: temporary.path().to_owned(),
                _temporary: Some(temporary),
            }
        }
    }
    fn db(&self) -> PathBuf {
        self.root.join("session.db")
    }
    fn workspace(&self) -> PathBuf {
        self.root.join("workspace")
    }
    fn marker(&self, role: &str) -> Option<Value> {
        fs::read(self.root.join(format!("{role}.pid")))
            .ok()
            .map(|bytes| serde_json::from_slice(&bytes).unwrap())
    }
    fn done(&self, role: &str) -> Option<String> {
        fs::read_to_string(self.root.join(format!("{role}.done"))).ok()
    }
}

/// Writes the release file if the test unwinds early, so the child never waits
/// for its guard timeout after a failed assertion.
struct ReleaseGuard {
    root: PathBuf,
}
impl Drop for ReleaseGuard {
    fn drop(&mut self) {
        let _ = fs::write(self.root.join(format!("{PROFILE_A}.release")), "guard");
    }
}

/// Releases every real child of the dual test on unwind so a failed assertion
/// cannot leave a process waiting for its guard timeout.
struct DualReleaseGuard {
    root: PathBuf,
    roles: Vec<String>,
}
impl Drop for DualReleaseGuard {
    fn drop(&mut self) {
        for role in &self.roles {
            let _ = fs::write(self.root.join(format!("{role}.release")), "guard");
        }
    }
}

fn evidence_dir() -> Option<PathBuf> {
    std::env::var_os("CAYW_SCHEDULER_TOOLS_EVIDENCE_DIR")
        .or_else(|| std::env::var_os("CAYW_SCHEDULER_EVIDENCE_DIR"))
        .map(PathBuf::from)
}

/// Writes a new receipt. An existing path is never replaced, so an earlier
/// run cannot be silently overwritten by a rerun.
fn receipt(name: &str, value: Value) {
    let Some(dir) = evidence_dir() else {
        return;
    };
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{name}.json"));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .unwrap_or_else(|error| panic!("receipt {} must be new: {error}", path.display()));
    std::io::Write::write_all(&mut file, &serde_json::to_vec_pretty(&value).unwrap()).unwrap();
    eprintln!("SCHEDULER_TOOLS_RECEIPT {}", path.display());
}

fn config(project: &Project) -> RuntimeConfig {
    let environment = BTreeMap::from([
        (CHILD_ENV.to_string(), "1".to_string()),
        (
            CHILD_ROOT.to_string(),
            project.root.to_string_lossy().into_owned(),
        ),
        (CHILD_ROLE.to_string(), PROFILE_A.to_string()),
    ]);
    let profile = Profile {
        kind: ProfileKind::Check,
        program: std::env::current_exe().unwrap(),
        args: vec![
            "--exact".into(),
            TEST_NAME.into(),
            "--test-threads=1".into(),
            "--nocapture".into(),
        ],
        env: environment,
        timeout: Duration::from_secs(150),
        max_output_bytes: 64 * 1024,
        allow_filter: false,
        io: ProcessIo::Pipes,
    };
    RuntimeConfig {
        max_concurrent_runs: 2,
        max_queued_runs: 3,
        fairness_after_dispatches: 2,
        max_run_time: Duration::from_secs(180),
        max_queue_time: Duration::from_secs(120),
        max_capability_calls: 8,
        codemode: cayw_codemode::Limits {
            timeout: Duration::from_secs(120),
            ..Default::default()
        },
        host: HostConfig {
            call_timeout: Duration::from_secs(90),
            process_timeout: Duration::from_secs(60),
            max_calls: 8,
            max_process_output_bytes: 64 * 1024,
            terminal_handoffs: Some(TerminalConfig {
                profiles: BTreeMap::from([(PROFILE_A.to_string(), profile)]),
                limits: TerminalLimits {
                    max_sessions: 2,
                    max_sessions_total: 8,
                    max_input_bytes_per_write: 64,
                    max_input_bytes_per_session: 256,
                    max_output_bytes_per_session: 64 * 1024,
                    max_read_bytes: 64 * 1024,
                    max_wait: Duration::from_secs(120),
                },
            }),
            ..Default::default()
        },
        permissions: vec![ScopedPermission::exact(
            "exec_command",
            json!({"profile":PROFILE_A}),
        )],
        ..Default::default()
    }
}

fn dual_profile(project: &Project, role: &str) -> Profile {
    Profile {
        kind: ProfileKind::Check,
        program: std::env::current_exe().unwrap(),
        args: vec![
            "--exact".into(),
            DUAL_TEST.into(),
            "--test-threads=1".into(),
            "--nocapture".into(),
        ],
        env: BTreeMap::from([
            (CHILD_ENV.to_string(), "1".to_string()),
            (
                CHILD_ROOT.to_string(),
                project.root.to_string_lossy().into_owned(),
            ),
            (CHILD_ROLE.to_string(), role.to_string()),
        ]),
        timeout: Duration::from_secs(150),
        max_output_bytes: 64 * 1024,
        allow_filter: false,
        io: ProcessIo::Pipes,
    }
}

/// K = 2, C = 3, A = 2 for three sessions. Each cell makes exactly one
/// `exec_command`, one approved wait and one EOF-bounded read, so the real
/// 8-call budgets and the default finite instruction budget cover it. No
/// permission is widened and no process is started without a declared profile.
fn dual_config(project: &Project) -> RuntimeConfig {
    RuntimeConfig {
        max_concurrent_runs: 2,
        max_queued_runs: 3,
        fairness_after_dispatches: 2,
        max_run_time: Duration::from_secs(180),
        max_queue_time: Duration::from_secs(120),
        max_capability_calls: 8,
        codemode: cayw_codemode::Limits {
            timeout: Duration::from_secs(120),
            ..Default::default()
        },
        host: HostConfig {
            call_timeout: Duration::from_secs(90),
            process_timeout: Duration::from_secs(60),
            max_calls: 8,
            max_process_output_bytes: 64 * 1024,
            terminal_handoffs: Some(TerminalConfig {
                profiles: BTreeMap::from([
                    (PROFILE_A.to_string(), dual_profile(project, PROFILE_A)),
                    (PROFILE_B.to_string(), dual_profile(project, PROFILE_B)),
                    (PROFILE_C.to_string(), dual_profile(project, PROFILE_C)),
                ]),
                limits: TerminalLimits {
                    max_sessions: 2,
                    max_sessions_total: 8,
                    max_input_bytes_per_write: 64,
                    max_input_bytes_per_session: 256,
                    max_output_bytes_per_session: 64 * 1024,
                    max_read_bytes: 64 * 1024,
                    max_wait: Duration::from_secs(120),
                },
            }),
            ..Default::default()
        },
        permissions: vec![
            ScopedPermission::exact("exec_command", json!({"profile":PROFILE_A})),
            ScopedPermission::exact("exec_command", json!({"profile":PROFILE_B})),
            ScopedPermission::exact("exec_command", json!({"profile":PROFILE_C})),
        ],
        ..Default::default()
    }
}

fn child_main() -> ! {
    let root = PathBuf::from(
        std::env::var_os(CHILD_ROOT).expect("the Host profile must set the child root"),
    );
    let role = std::env::var(CHILD_ROLE).expect("the Host profile must set the child role");
    fs::create_dir_all(&root).unwrap();
    let pid = std::process::id();
    write_atomic(
        &root.join(format!("{role}.pid")),
        &serde_json::to_vec(&json!({"role": &role, "pid": pid})).unwrap(),
    );
    let release = root.join(format!("{role}.release"));
    let deadline = Instant::now() + Duration::from_secs(90);
    while !release.is_file() {
        assert!(
            Instant::now() < deadline,
            "guard timeout: {role} release file was not written"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let done = format!("{role} complete pid={pid}\n");
    write_atomic(&root.join(format!("{role}.done")), done.as_bytes());
    {
        use std::io::Write;
        let mut stdout = std::io::stdout();
        writeln!(stdout, "{role}-complete pid={pid}").unwrap();
        stdout.flush().unwrap();
    }
    std::process::exit(0);
}

fn write_atomic(path: &Path, bytes: &[u8]) {
    let temporary = path.with_extension("tmp");
    fs::write(&temporary, bytes).unwrap();
    fs::rename(&temporary, path).unwrap();
}

fn spawn_worker(rt: &Runtime, task: &str) -> Worker {
    let rt = rt.clone();
    let id = task.to_owned();
    tokio::spawn(async move { rt.resume(&id, CancellationToken::new()).await })
}

async fn await_worker_result(what: &str, worker: Worker) -> Result<SessionView, RuntimeError> {
    match tokio::time::timeout(Duration::from_secs(90), worker).await {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => panic!("{what} worker did not join: {error}"),
        Err(_) => panic!("{what} worker did not finish"),
    }
}

async fn wait_until(what: &str, mut check: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if check() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

fn all_events(rt: &Runtime, session: &str) -> Vec<EventRecord> {
    let mut events = Vec::new();
    loop {
        let after = events
            .last()
            .map_or(0, |record: &EventRecord| record.cursor);
        let page = rt.events(session, after, 1000).unwrap();
        if page.is_empty() {
            return events;
        }
        events.extend(page);
    }
}

fn all_audit(rt: &Runtime, session: &str) -> Vec<AuditRecord> {
    let mut records = Vec::new();
    loop {
        let after = records
            .last()
            .map_or(0, |record: &AuditRecord| record.cursor);
        let page = rt.audit(session, after, 1000).unwrap();
        if page.is_empty() {
            return records;
        }
        records.extend(page);
    }
}

fn calls_named(
    rt: &Runtime,
    session: &str,
    name: &str,
    settled: bool,
) -> Vec<CapabilityCallRecord> {
    all_events(rt, session)
        .into_iter()
        .filter_map(|record| match record.event {
            CoreEvent::CapabilityStarted { call } if !settled && call.name == name => Some(call),
            CoreEvent::CapabilitySettled { call } if settled && call.name == name => Some(call),
            _ => None,
        })
        .collect()
}

fn ownership(rt: &Runtime, session: &str) {
    let events = all_events(rt, session);
    assert!(!events.is_empty(), "{session} lost its Core events");
    for (index, record) in events.iter().enumerate() {
        assert_eq!(record.protocol_version, cayw_core::PROTOCOL_VERSION);
        assert_eq!(record.session_id, session);
        assert_eq!(
            record.cursor,
            index as u64 + 1,
            "{session} Core cursor has a gap or a foreign owner"
        );
    }
    // The audit sequence is global (AUTOINCREMENT), not per Session. Its
    // ownership is still exact; only monotonicity can be asserted here.
    let audit = all_audit(rt, session);
    let mut previous = None;
    for record in &audit {
        assert_eq!(record.session_id, session);
        if let Some(previous) = previous {
            assert!(
                record.cursor > previous,
                "{session} audit cursors are not strictly increasing"
            );
        }
        previous = Some(record.cursor);
    }
}

/// Checks that every call crossed the execution boundary under the exact
/// trusted authorization recorded in Core.
fn authorization(
    call: &CapabilityCallRecord,
    capability: &str,
    scope: Value,
    actor: &str,
    approval: Option<&str>,
) {
    assert_eq!(call.name, capability);
    assert_eq!(call.arguments, scope);
    let proof: CapabilityAuthorization =
        serde_json::from_value(call.authorization.clone().expect("authorization proof")).unwrap();
    assert_eq!(proof.actor, actor);
    assert_eq!(proof.target, call.key);
    assert_eq!(proof.task_id, call.task_id);
    assert_eq!(proof.capability, capability);
    assert_eq!(
        proof.scope,
        ScopedPermission::exact(capability, scope.clone()).scope
    );
    assert_eq!(proof.decision, ApprovalDecision::Approve);
    assert_eq!(proof.approval_id.as_deref(), approval);
    assert!(proof.external_provenance.is_none());
}

/// Trusted approval proof without assuming whether this particular repeat used
/// the original approval id or the already-active reusable task grant.
fn authorization_scope(
    call: &CapabilityCallRecord,
    capability: &str,
    scope: Value,
) -> CapabilityAuthorization {
    let proof: CapabilityAuthorization =
        serde_json::from_value(call.authorization.clone().expect("authorization proof")).unwrap();
    assert_eq!(proof.actor, "scheduler-tools");
    assert_eq!(proof.target, call.key);
    assert_eq!(proof.task_id, call.task_id);
    assert_eq!(proof.capability, capability);
    assert_eq!(
        proof.scope,
        ScopedPermission::exact(capability, scope.clone()).scope
    );
    assert_eq!(proof.decision, ApprovalDecision::Approve);
    assert!(proof.external_provenance.is_none());
    proof
}

/// Checks the original model ledger, not a derived counter. The fixture reports
/// 4 input + 4 output tokens per completed request. Those counters exercise the
/// reservation protocol; they are not a price, and the cost stays Unknown.
fn assert_model_ledger(rt: &Runtime, session: &str, task: &TaskRecord, requests: usize) {
    let ledger = rt.model_requests(session).unwrap();
    assert_eq!(ledger.budget_session_id, session);
    assert_eq!(
        ledger.policy,
        ModelReservationPolicy::EntireSessionRemainder
    );
    assert_eq!(ledger.token_limit, 128_000);
    assert_eq!(ledger.requests.len(), requests);
    assert_eq!(ledger.settled_tokens, 8 * requests as u64);
    assert_eq!(ledger.reserved_tokens, 0);
    assert_eq!(ledger.remaining_tokens, 128_000 - 8 * requests as u64);
    assert!(
        !ledger.budget_unknown,
        "every completed model request must release its original reservation"
    );
    assert!(!ledger.overflowed);
    assert_eq!(ledger.cost_status, ModelCostStatus::Unknown);
    assert!(ledger.cost_amount.is_none());
    assert!(ledger.cost_currency.is_none());
    for (index, record) in ledger.requests.iter().enumerate() {
        assert_eq!(record.identity.lease.session_id, session);
        assert_eq!(record.identity.task_id, task.id);
        assert_eq!(record.identity.budget_session_id, ledger.budget_session_id);
        assert_eq!(record.identity.turn, index as u64 + 1);
        assert_eq!(record.identity.provider, "fixture");
        assert_eq!(record.identity.model, "scheduler-tools");
        assert_eq!(
            record.policy,
            ModelReservationPolicy::EntireSessionRemainder
        );
        assert_eq!(record.terminal, Some(ModelRequestTerminal::Completed));
        assert_eq!(record.settled_tokens, Some(8));
        assert_eq!(record.outstanding_reservation_tokens, 0);
        assert_eq!(record.cost_status, ModelCostStatus::Unknown);
        assert!(record.cost_amount.is_none());
        assert!(record.cost_currency.is_none());
        assert_eq!(
            record.observations.len(),
            1,
            "the fixture emits exactly one terminal usage observation"
        );
        let observed = &record.observations[0].observation;
        assert_eq!(observed.event, "done");
        assert_eq!(observed.usage_status, UsageStatus::Reported);
        assert_eq!(observed.usage.input, 4);
        assert_eq!(observed.usage.output, 4);
        assert_eq!(observed.usage.total_tokens, 8);
    }
}

fn marker_identity(project: &Project, role: &str) -> (ProcessIdentity, Vec<u8>) {
    let bytes =
        fs::read(project.root.join(format!("{role}.pid"))).expect("the child wrote its PID marker");
    let marker: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(marker["role"].as_str(), Some(role));
    let pid = u32::try_from(marker["pid"].as_u64().unwrap()).unwrap();
    let identity = ProcessIdentity::observe(pid).expect("the original process is alive");
    (identity, bytes)
}

async fn stopped(original: &ProcessIdentity) {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match ProcessIdentity::observe(original.pid) {
                Err(error) if error.raw_os_error() == Some(3) => return,
                Ok(current)
                    if current.started_seconds != original.started_seconds
                        || current.started_microseconds != original.started_microseconds =>
                {
                    return;
                }
                Ok(_) => {}
                Err(error) => panic!("the original process identity cannot be checked: {error}"),
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the original process did not stop");
}

fn process_rows(db: &Path) -> Vec<Value> {
    let connection = rusqlite::Connection::open(db).unwrap();
    connection.busy_timeout(Duration::from_secs(5)).unwrap();
    let mut statement = connection
        .prepare(
            "SELECT task,session,attempt,worker,fence,execution,sequence,
            pid,parent_pid,process_group,started_seconds,started_microseconds,state
            FROM runtime_process_identities ORDER BY task,pid",
        )
        .unwrap();
    statement
        .query_map([], |row| {
            Ok(json!({
                "task":row.get::<_, String>(0)?,"session":row.get::<_, String>(1)?,
                "attempt":row.get::<_, String>(2)?,"worker":row.get::<_, String>(3)?,
                "fence":row.get::<_, i64>(4)?,"execution":row.get::<_, String>(5)?,
                "sequence":row.get::<_, i64>(6)?,"pid":row.get::<_, i64>(7)?,
                "parent_pid":row.get::<_, i64>(8)?,"process_group":row.get::<_, i64>(9)?,
                "started_seconds":row.get::<_, i64>(10)?,
                "started_microseconds":row.get::<_, i64>(11)?,"state":row.get::<_, String>(12)?,
            }))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

fn process_scopes(db: &Path) -> Vec<Value> {
    let connection = rusqlite::Connection::open(db).unwrap();
    connection.busy_timeout(Duration::from_secs(5)).unwrap();
    let mut statement = connection
        .prepare(
            "SELECT task,session,attempt,worker,fence,state,evidence
            FROM runtime_process_scopes ORDER BY task",
        )
        .unwrap();
    statement
        .query_map([], |row| {
            Ok(json!({
                "task":row.get::<_, String>(0)?,"session":row.get::<_, String>(1)?,
                "attempt":row.get::<_, String>(2)?,"worker":row.get::<_, String>(3)?,
                "fence":row.get::<_, i64>(4)?,"state":row.get::<_, String>(5)?,
                "evidence":row.get::<_, Option<String>>(6)?,
            }))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

fn rows_for_task<'a>(rows: &'a [Value], task: &str) -> Vec<&'a Value> {
    rows.iter()
        .filter(|row| row["task"].as_str() == Some(task))
        .collect()
}

fn recorded_process(project: &Project, call: &CapabilityCallRecord, identity: &ProcessIdentity) {
    let rows = process_rows(&project.db());
    let task_rows = rows_for_task(&rows, &call.task_id);
    assert_eq!(
        task_rows.len(),
        1,
        "one planned call must own exactly one original OS process"
    );
    let row = task_rows[0];
    assert_eq!(
        row["session"].as_str(),
        Some(call.lease.session_id.as_str())
    );
    assert_eq!(
        row["attempt"].as_str(),
        Some(call.lease.attempt_id.as_str())
    );
    assert_eq!(row["worker"].as_str(), Some(call.lease.worker_id.as_str()));
    assert_eq!(row["fence"].as_u64(), Some(call.lease.fence));
    assert_eq!(
        row["execution"].as_str(),
        Some(call.key.execution_id.as_str())
    );
    assert_eq!(row["sequence"].as_u64(), Some(call.key.sequence));
    assert_eq!(row["pid"].as_u64(), Some(u64::from(identity.pid)));
    assert_eq!(
        row["parent_pid"].as_u64(),
        Some(u64::from(identity.parent_pid))
    );
    assert_eq!(
        row["process_group"].as_u64(),
        Some(u64::from(identity.process_group))
    );
    assert_eq!(
        row["started_seconds"].as_u64(),
        Some(identity.started_seconds)
    );
    assert_eq!(
        row["started_microseconds"].as_u64(),
        Some(u64::from(identity.started_microseconds))
    );
    let state = row["state"].as_str().unwrap();
    assert!(
        matches!(state, "started" | "observed"),
        "a live original process has state {state}"
    );
}

async fn pending_approval(rt: &Runtime, session: &str) -> ApprovalRequest {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Some(approval) = rt.get(session).unwrap().pending_approvals.first() {
                return approval.clone();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the write_stdin approval did not appear")
}

async fn wait_for_approval(
    rt: &Runtime,
    session: &str,
    capability: &str,
    action: &str,
) -> ApprovalRequest {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Some(approval) =
                rt.get(session)
                    .unwrap()
                    .pending_approvals
                    .into_iter()
                    .find(|approval| {
                        approval.input.capability == capability
                            && serde_json::from_str::<Value>(&approval.input.scope)
                                .ok()
                                .and_then(|scope| {
                                    scope["action"].as_str().map(|value| value == action)
                                })
                                .unwrap_or(false)
                    })
            {
                return approval;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the {capability} {action} approval did not appear"))
}

fn approval_target(approval: &ApprovalRequest) -> Value {
    serde_json::to_value(&approval.input.target).unwrap()
}

/// The wait is the next planned call in the original execution. Every field is
/// compared literally; a prefix, wildcard or different request is rejected.
fn assert_wait_approval(
    approval: &ApprovalRequest,
    session: &str,
    task: &TaskRecord,
    exec: &CapabilityCallRecord,
    host_session: &str,
    request_id: &str,
) {
    assert_eq!(approval.session_id, session);
    assert_eq!(approval.input.capability, "write_stdin");
    assert_eq!(approval.input.task_id, task.id);
    assert_eq!(approval.attempt_id, exec.lease.attempt_id);
    let scope = json!({
        "session_id": host_session,
        "action": "wait",
        "request_id": request_id,
        "wait_ms": 60000,
    });
    assert_eq!(
        serde_json::from_str::<Value>(&approval.input.scope).unwrap(),
        scope
    );
    assert_eq!(
        approval.input.scope,
        ScopedPermission::exact("write_stdin", scope.clone()).scope,
        "the pending wait scope must be the canonical exact arguments"
    );
    let target = approval_target(approval);
    assert_eq!(target["type"].as_str(), Some("capability_call"));
    assert_eq!(
        target["key"]["execution_id"].as_str(),
        Some(exec.key.execution_id.as_str())
    );
    assert_eq!(
        target["key"]["sequence"].as_u64(),
        Some(exec.key.sequence + 1)
    );
}

/// The read follows the wait. Its cursor and byte limit are fixed by the cell,
/// so the same exact call may be retried until the process output reaches EOF.
fn assert_read_approval(
    approval: &ApprovalRequest,
    session: &str,
    task: &TaskRecord,
    exec: &CapabilityCallRecord,
    host_session: &str,
    request_id: &str,
) {
    assert_eq!(approval.session_id, session);
    assert_eq!(approval.input.capability, "write_stdin");
    assert_eq!(approval.input.task_id, task.id);
    assert_eq!(approval.attempt_id, exec.lease.attempt_id);
    let scope = json!({
        "session_id": host_session,
        "action": "read",
        "cursor": 0,
        "max_bytes": 65536,
        "request_id": request_id,
    });
    assert_eq!(
        serde_json::from_str::<Value>(&approval.input.scope).unwrap(),
        scope
    );
    assert_eq!(
        approval.input.scope,
        ScopedPermission::exact("write_stdin", scope.clone()).scope,
        "the pending read scope must be the canonical exact arguments"
    );
    let target = approval_target(approval);
    assert_eq!(target["type"].as_str(), Some("capability_call"));
    assert_eq!(
        target["key"]["execution_id"].as_str(),
        Some(exec.key.execution_id.as_str())
    );
    assert_eq!(
        target["key"]["sequence"].as_u64(),
        Some(exec.key.sequence + 2),
        "the read is the second planned call after the original exec"
    );
}

struct ExecInfo {
    call: CapabilityCallRecord,
    identity: ProcessIdentity,
    host_session: String,
}

fn exec_info(
    rt: &Runtime,
    session: &str,
    task: &TaskRecord,
    project: &Project,
    profile: &str,
) -> ExecInfo {
    let calls = calls_named(rt, session, "exec_command", true);
    assert_eq!(
        calls.len(),
        1,
        "each task must settle exactly one real exec_command"
    );
    let call = calls.into_iter().next().unwrap();
    assert_eq!(call.task_id, task.id);
    let (identity, _) = marker_identity(project, profile);
    let host_session = match &call.outcome {
        Some(CapabilityOutcome::Completed { result }) => {
            assert_eq!(result["profile"].as_str(), Some(profile));
            assert_eq!(result["state"].as_str(), Some("running"));
            result["session_id"]
                .as_str()
                .expect("exec_command must return the Host session id")
                .to_string()
        }
        other => panic!("exec_command did not complete: {other:?}"),
    };
    assert!(host_session.starts_with("term-"), "{host_session}");
    authorization(
        &call,
        "exec_command",
        json!({"profile":profile}),
        "runtime-config",
        None,
    );
    recorded_process(project, &call, &identity);
    assert!(identity.is_running().unwrap());
    ExecInfo {
        call,
        identity,
        host_session,
    }
}

fn dual_state(
    rt: &Runtime,
    sessions: &[&str],
    provider: &FixtureProvider,
    project: &Project,
) -> Value {
    let mut value = state(rt, sessions, provider, project);
    value["markers"] = json!({
        "a": {"role": PROFILE_A, "pid": project.marker(PROFILE_A), "done": project.done(PROFILE_A)},
        "b": {"role": PROFILE_B, "pid": project.marker(PROFILE_B), "done": project.done(PROFILE_B)},
        "c": {"role": PROFILE_C, "pid": project.marker(PROFILE_C), "done": project.done(PROFILE_C)},
    });
    value
}

fn scope_for_task<'a>(scopes: &'a [Value], task: &str) -> &'a Value {
    let rows = rows_for_task(scopes, task);
    assert_eq!(rows.len(), 1, "one task must own exactly one process scope");
    rows[0]
}

fn stopped_process_rows(project: &Project, task: &str) {
    let rows = process_rows(&project.db());
    let task_rows = rows_for_task(&rows, task);
    assert_eq!(
        task_rows.len(),
        1,
        "one real call owns one original process"
    );
    assert_eq!(
        task_rows[0]["state"].as_str(),
        Some("stopped"),
        "the original OS process must be recorded stopped"
    );
}

fn processes_stopped_audit(rt: &Runtime, session: &str, task: &str) -> AuditRecord {
    let matching: Vec<AuditRecord> = all_audit(rt, session)
        .into_iter()
        .filter(|record| record.kind == "processes_stopped")
        .filter(|record| record.task_id.as_deref() == Some(task))
        .collect();
    assert_eq!(
        matching.len(),
        1,
        "the exact task must have one cleanup audit record"
    );
    let record = matching.into_iter().next().unwrap();
    assert_eq!(record.data["state"].as_str(), Some("drained"));
    assert_eq!(
        record.data["actor"].as_str(),
        Some("runtime-process-supervisor")
    );
    assert_eq!(
        record.data["evidence"].as_str(),
        Some("All owned process supervisors exited after managed process cleanup")
    );
    record
}

fn completed_result(call: &CapabilityCallRecord) -> &Value {
    match &call.outcome {
        Some(CapabilityOutcome::Completed { result }) => result,
        other => panic!("the call did not complete: {other:?}"),
    }
}

fn state(rt: &Runtime, sessions: &[&str], provider: &FixtureProvider, project: &Project) -> Value {
    let health = rt.health().unwrap();
    let sessions = sessions
        .iter()
        .map(|session| {
            json!({
                "id": session,
                "view": rt.get(session).unwrap(),
                "tasks": rt.tasks(session).unwrap(),
                "events": all_events(rt, session),
                "audit": all_audit(rt, session),
                "started": all_events(rt, session).into_iter().filter_map(|record| match record.event {
                    CoreEvent::CapabilityStarted { call } => Some(call),
                    _ => None,
                }).collect::<Vec<_>>(),
                "settled": all_events(rt, session).into_iter().filter_map(|record| match record.event {
                    CoreEvent::CapabilitySettled { call } => Some(call),
                    _ => None,
                }).collect::<Vec<_>>(),
                "model_requests": rt.model_requests(session).unwrap(),
            })
        })
        .collect::<Vec<_>>();
    let model_calls = provider
        .calls()
        .iter()
        .map(|call| {
            json!({
                "text": call.text,
                "tool_result_turn": call.tool_result_turn,
                "messages": call.messages,
            })
        })
        .collect::<Vec<_>>();
    json!({
        "project_root": project.root.to_string_lossy().to_string(),
        "health": {
            "status": health.status,
            "protocol_version": health.protocol_version,
            "running": health.running,
            "queued": health.queued,
            "max_concurrent_runs": health.max_concurrent_runs,
        },
        "configuration": {
            "k": 2, "c": 3, "a": 2,
            "terminal_profile_timeout_seconds": 150,
            "max_capability_calls": 8,
            "host_max_calls": 8,
            "codemode_timeout_seconds": 120,
            "codemode_instruction_budget": 100_000,
            "codemode_host_calls": 64,
        },
        "sessions": sessions,
        "processes": process_rows(&project.db()),
        "process_scopes": process_scopes(&project.db()),
        "model_calls": model_calls,
        "markers": {
            "role": PROFILE_A,
            "pid": project.marker(PROFILE_A),
            "done": project.done(PROFILE_A),
        },
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_process_live_across_permission_park_must_keep_its_slot() {
    if std::env::var_os(CHILD_ENV).is_some() {
        child_main();
    }

    let project = Project::new("scheduler-terminal-slot");
    let _release = ReleaseGuard {
        root: project.root.clone(),
    };
    let provider = Arc::new(FixtureProvider::default());
    let runtime_provider: Arc<dyn Provider> = provider.clone();
    let rt = Runtime::open(
        project.db(),
        project.workspace(),
        runtime_provider,
        Model::new("fixture", "scheduler-tools"),
        config(&project),
    )
    .unwrap();

    // A second session exists only to prove that an approval or cancellation
    // naming another Session is rejected without touching this task.
    let other_session = rt.create(Some("other".into())).unwrap().id;
    let session = rt.create(Some("tool a".into())).unwrap().id;
    let task = rt.enqueue(&session, "tool a").unwrap();
    let worker = spawn_worker(&rt, &task.id);

    wait_until("exec_command settled with a live original process", || {
        calls_named(&rt, &session, "exec_command", true).len() == 1
            && project.marker(PROFILE_A).is_some()
            && rows_for_task(&process_rows(&project.db()), &task.id).len() == 1
    })
    .await;

    let exec = calls_named(&rt, &session, "exec_command", true)
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(exec.task_id, task.id);
    let (identity, marker) = marker_identity(&project, PROFILE_A);
    let exec_session = match &exec.outcome {
        Some(CapabilityOutcome::Completed { result }) => {
            assert_eq!(result["profile"].as_str(), Some(PROFILE_A));
            assert_eq!(result["state"].as_str(), Some("running"));
            result["session_id"]
                .as_str()
                .expect("exec_command must return the Host session id")
                .to_string()
        }
        other => panic!("exec_command did not complete: {other:?}"),
    };
    assert!(exec_session.starts_with("term-"), "{exec_session}");
    authorization(
        &exec,
        "exec_command",
        json!({"profile":PROFILE_A}),
        "runtime-config",
        None,
    );
    recorded_process(&project, &exec, &identity);
    assert!(identity.is_running().unwrap());
    ownership(&rt, &session);
    receipt(
        "exec-started",
        state(&rt, &[session.as_str()], &provider, &project),
    );

    // The cell now asks for write_stdin. Nothing grants that capability
    // statically, so the task must park on its original approval request.
    let approval = pending_approval(&rt, &session).await;
    assert_eq!(approval.session_id, session);
    assert_eq!(approval.input.capability, "write_stdin");
    assert_eq!(approval.input.task_id, task.id);
    assert_eq!(approval.attempt_id.as_str(), exec.lease.attempt_id.as_str());
    let scope: Value = serde_json::from_str(&approval.input.scope).unwrap();
    assert_eq!(
        scope,
        json!({
            "session_id": exec_session,
            "action": "wait",
            "request_id": "a-wait",
            "wait_ms": 60000,
        })
    );
    assert_eq!(
        approval.input.scope,
        ScopedPermission::exact("write_stdin", scope.clone()).scope,
        "the pending scope must be the canonical exact arguments"
    );
    let target: Value = serde_json::to_value(&approval.input.target).unwrap();
    assert_eq!(target["type"].as_str(), Some("capability_call"));
    assert_eq!(
        target["key"]["execution_id"].as_str(),
        Some(exec.key.execution_id.as_str())
    );
    assert_eq!(
        target["key"]["sequence"].as_u64(),
        Some(exec.key.sequence + 1),
        "the wait is the next planned call in the same execution"
    );

    // Wait until the original approval, the completed exec_command and the
    // original PID coexist. A safe scheduler may keep the task Running with its
    // slot or park it in WaitingApproval; the final assertion requires the
    // former and therefore must not depend on the parked state.
    wait_until(
        "the original approval and live original process coexist",
        || {
            let current = rt.task(&task.id).unwrap();
            matches!(
                current.state,
                TaskState::Running | TaskState::WaitingApproval
            ) && identity.is_running().unwrap()
                && rt
                    .get(&session)
                    .unwrap()
                    .pending_approvals
                    .iter()
                    .any(|pending| pending.id == approval.id)
        },
    )
    .await;
    let parked = rt.task(&task.id).unwrap();
    assert_eq!(
        parked.worker_id.as_deref(),
        Some(exec.lease.worker_id.as_str())
    );
    assert_eq!(
        parked.attempt_id.as_deref(),
        Some(exec.lease.attempt_id.as_str())
    );
    assert_eq!(parked.fence, exec.lease.fence);
    assert!(!parked.cancel_requested);

    // Another Session cannot approve or cancel this original task.
    assert!(matches!(
        rt.approve(
            &other_session,
            &approval.id,
            ApprovalChoice::AllowOnce,
            "scheduler-tools"
        ),
        Err(RuntimeError::StaleApproval)
    ));
    let mut wrong_session = parked.cancel_target();
    wrong_session.session_id = other_session.clone();
    assert!(matches!(
        rt.cancel(&wrong_session),
        Err(RuntimeError::StaleCancellation)
    ));
    assert!(!rt.task(&task.id).unwrap().cancel_requested);
    assert!(
        rt.get(&session)
            .unwrap()
            .pending_approvals
            .iter()
            .any(|pending| pending.id == approval.id),
        "the wrong-session attempt must not consume the original approval"
    );

    // The permission wait must not hide a live original process. Sample the
    // same task and PID for a short window; the original attempt may stay
    // Running or park, but it must not be replaced and the PID must keep
    // running.
    let mut samples = 0;
    for _ in 0..20 {
        let current = rt.task(&task.id).unwrap();
        assert!(
            matches!(
                current.state,
                TaskState::Running | TaskState::WaitingApproval
            ),
            "the permission wait must stay on the original task"
        );
        assert_eq!(
            current.attempt_id.as_deref(),
            Some(exec.lease.attempt_id.as_str())
        );
        assert_eq!(
            current.worker_id.as_deref(),
            Some(exec.lease.worker_id.as_str())
        );
        assert_eq!(current.fence, exec.lease.fence);
        assert!(
            identity.is_running().unwrap(),
            "the original terminal process must still run during the permission wait"
        );
        samples += 1;
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(samples, 20);
    let parked = rt.task(&task.id).unwrap();
    let health = rt.health().unwrap();
    let view = rt.get(&session).unwrap();
    let original_running = identity.is_running().unwrap();
    assert!(original_running);

    assert_model_ledger(&rt, &session, &parked, provider.calls_for("tool a"));
    assert_eq!(provider.calls_for("tool a"), 1);
    assert_eq!(all_events(&rt, &session).len() as u64, view.cursor);
    assert_eq!(
        view.pending_approvals
            .iter()
            .filter(|pending| pending.id == approval.id)
            .count(),
        1
    );

    receipt(
        "permission-park",
        json!({
            "state": state(&rt, &[session.as_str(), other_session.as_str()], &provider, &project),
            "boundary": {
                "task": parked,
                "health": health,
                "original_pid": identity.pid,
                "original_marker": marker,
                "original_is_running": original_running,
                "pending_approval": approval,
            },
        }),
    );

    // Preserve the park snapshot above, then prove cleanup actually reaped the
    // original OS process before the invariant assertion below fails.
    let target = parked.cancel_target();
    assert!(rt.cancel(&target).unwrap());
    let _ = await_worker_result("parked tool", worker).await;
    stopped(&identity).await;

    assert!(
        parked.holds_slot && parked.state == TaskState::Running && health.running == 1,
        "a live original terminal process (pid {}, running={}) must keep its admitted \
         execution slot while the task waits for permission; task={:?} holds_slot={} \
         health.running={} approval={} receipt=permission-park",
        identity.pid,
        original_running,
        parked.state,
        parked.holds_slot,
        health.running,
        approval.id
    );
}

fn exited_wait(rt: &Runtime, session: &str) -> CapabilityCallRecord {
    let mut matching = Vec::new();
    for call in calls_named(rt, session, "write_stdin", true) {
        if call.arguments["action"].as_str() != Some("wait") {
            continue;
        }
        if let Some(CapabilityOutcome::Completed { result }) = &call.outcome
            && result["status"]["state"].as_str() == Some("exited")
        {
            matching.push(call);
        }
    }
    assert!(
        !matching.is_empty(),
        "the approved wait must record the real normal exit"
    );
    matching
        .into_iter()
        .min_by_key(|call| call.key.sequence)
        .unwrap()
}

fn eof_read(rt: &Runtime, session: &str) -> CapabilityCallRecord {
    let mut matching = Vec::new();
    for call in calls_named(rt, session, "write_stdin", true) {
        if call.arguments["action"].as_str() != Some("read") {
            continue;
        }
        if let Some(CapabilityOutcome::Completed { result }) = &call.outcome
            && result["eof"].as_bool() == Some(true)
        {
            matching.push(call);
        }
    }
    assert_eq!(
        matching.len(),
        1,
        "the read loop must stop on exactly one EOF page"
    );
    matching.into_iter().next().unwrap()
}

/// Two real Host tools run together under the same K = 2 admission window.
/// `SafeReturn` ends A's child normally and cancels the pending read; its
/// confirmed cleanup frees A's slot for C. `LiveCancel` cancels A while its
/// approved wait still runs against the live child; terminal cleanup stays
/// unconfirmed, so A keeps its slot and only B's confirmed completion admits C.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DualMode {
    SafeReturn,
    LiveCancel,
}

fn dual_prefix(mode: DualMode) -> &'static str {
    match mode {
        DualMode::SafeReturn => DUAL_PREFIX,
        DualMode::LiveCancel => LIVE_DUAL_PREFIX,
    }
}

struct ToolCompletion {
    task: TaskRecord,
    read: CapabilityCallRecord,
    wait: CapabilityCallRecord,
    scope: Value,
    cleanup: AuditRecord,
    stdout: String,
}

/// One task's exact read/wait/exit pair, bound to the exec's original planned
/// call and lease. B and C use the same finite cell: exec, wait, read until EOF.
struct ToolCase<'a> {
    session: &'a str,
    task: &'a TaskRecord,
    exec: &'a ExecInfo,
    profile: &'a str,
    request_id: &'a str,
    text: &'a str,
}

/// C's admission observation needs the original A/B task identities, so the
/// caller can assert which two slots are live in each mode.
struct AdmissionCase<'a> {
    session_c: &'a str,
    task_a_id: &'a str,
    task_b_id: &'a str,
    task_c: &'a TaskRecord,
}

/// The one settled call that owns this exact planned key. A duplicated or
/// replayed execution cannot satisfy it.
fn settled_call(
    rt: &Runtime,
    session: &str,
    name: &str,
    execution_id: &str,
    sequence: u64,
) -> CapabilityCallRecord {
    let mut matching: Vec<CapabilityCallRecord> = calls_named(rt, session, name, true)
        .into_iter()
        .filter(|call| call.key.execution_id == execution_id && call.key.sequence == sequence)
        .collect();
    assert_eq!(
        matching.len(),
        1,
        "exactly one settled {name} call must own {execution_id}:{sequence}"
    );
    matching.pop().unwrap()
}

/// The earliest durable audit record for a Session. C has no audit row at
/// admission; its first row is written after the admitted attempt starts.
fn first_audit(rt: &Runtime, session: &str) -> AuditRecord {
    all_audit(rt, session)
        .into_iter()
        .next()
        .expect("the session must have a durable audit record")
}

fn finished_audit(rt: &Runtime, session: &str, task: &str) -> AuditRecord {
    let matching: Vec<AuditRecord> = all_audit(rt, session)
        .into_iter()
        .filter(|record| record.kind == "run_finished")
        .filter(|record| record.task_id.as_deref() == Some(task))
        .collect();
    assert_eq!(
        matching.len(),
        1,
        "the completed task must have exactly one run_finished audit record"
    );
    matching.into_iter().next().unwrap()
}

fn attempt_started_at(rt: &Runtime, session: &str) -> u64 {
    let mut matching: Vec<EventRecord> = all_events(rt, session)
        .into_iter()
        .filter(|record| matches!(record.event, CoreEvent::AttemptStarted { .. }))
        .collect();
    assert_eq!(
        matching.len(),
        1,
        "one admitted task must record exactly one attempt_started event"
    );
    matching.pop().unwrap().timestamp
}

/// Waits for C to be admitted and its exact wait to start. The mode decides
/// which two slots are live: after the safe return A is gone and B holds the
/// second slot; after the live cancel A still holds its unknown slot.
async fn admit_queued_c(
    rt: &Runtime,
    project: &Project,
    case: AdmissionCase<'_>,
    mode: DualMode,
    exec_b: &ExecInfo,
    identity_b: &ProcessIdentity,
) -> (TaskRecord, ExecInfo) {
    wait_until(
        "task C was admitted only after the original cleanup",
        || {
            let c = rt.task(&case.task_c.id).unwrap();
            c.state == TaskState::Running
                && c.holds_slot
                && calls_named(rt, case.session_c, "exec_command", true).len() == 1
                && project.marker(PROFILE_C).is_some()
        },
    )
    .await;
    let task_c_running = rt.task(&case.task_c.id).unwrap();
    match mode {
        DualMode::SafeReturn => {
            let b = rt.task(case.task_b_id).unwrap();
            assert_eq!(b.state, TaskState::Running);
            assert!(b.holds_slot);
            assert_eq!(
                b.attempt_id.as_deref(),
                Some(exec_b.call.lease.attempt_id.as_str())
            );
            assert_eq!(
                b.worker_id.as_deref(),
                Some(exec_b.call.lease.worker_id.as_str())
            );
            assert_eq!(b.fence, exec_b.call.lease.fence);
            assert!(identity_b.is_running().unwrap());
            let health = rt.health().unwrap();
            assert_eq!(
                (health.running, health.queued),
                (2, 0),
                "B and C must hold both slots after A's confirmed cleanup"
            );
        }
        DualMode::LiveCancel => {
            let a = rt.task(case.task_a_id).unwrap();
            assert_eq!(a.state, TaskState::Draining);
            assert!(
                a.holds_slot,
                "A's unconfirmed terminal cleanup must keep its admitted slot"
            );
            let b = rt.task(case.task_b_id).unwrap();
            assert_eq!(b.state, TaskState::Completed);
            assert!(!b.holds_slot);
            assert!(!identity_b.is_running().unwrap());
            let health = rt.health().unwrap();
            assert_eq!(
                (health.running, health.queued),
                (2, 0),
                "A's unknown slot and C's admitted slot are the two live slots"
            );
        }
    }

    let exec_c = exec_info(rt, case.session_c, &task_c_running, project, PROFILE_C);
    let approval_c = wait_for_approval(rt, case.session_c, "write_stdin", "wait").await;
    assert_wait_approval(
        &approval_c,
        case.session_c,
        &task_c_running,
        &exec_c.call,
        &exec_c.host_session,
        "c-wait",
    );
    rt.approve(
        case.session_c,
        &approval_c.id,
        ApprovalChoice::AllowOnce,
        "scheduler-tools",
    )
    .unwrap();
    wait_until("C's approved wait started in its admitted slot", || {
        calls_named(rt, case.session_c, "write_stdin", false).len() == 1
    })
    .await;
    (task_c_running, exec_c)
}

/// Releases one real child, approves its exact EOF read once, joins the worker
/// and proves the original exec, wait, stdout, exit code and cleanup. The
/// returned cleanup record is the durable `processes_stopped` audit row.
async fn complete_released_tool(
    rt: &Runtime,
    project: &Project,
    provider: &FixtureProvider,
    case: ToolCase<'_>,
    worker: Worker,
) -> ToolCompletion {
    fs::write(
        project.root.join(format!("{}.release", case.profile)),
        "release",
    )
    .unwrap();
    wait_until("the child wrote its completion marker", || {
        project.done(case.profile).is_some()
    })
    .await;
    stopped(&case.exec.identity).await;
    let read_approval = wait_for_approval(rt, case.session, "write_stdin", "read").await;
    assert_read_approval(
        &read_approval,
        case.session,
        case.task,
        &case.exec.call,
        &case.exec.host_session,
        case.request_id,
    );
    rt.approve(
        case.session,
        &read_approval.id,
        ApprovalChoice::AllowTask,
        "scheduler-tools",
    )
    .unwrap();
    let result = await_worker_result(case.text, worker).await;
    assert!(
        result.is_ok(),
        "task {} must complete through its real result: {:?}",
        case.profile,
        result.as_ref().err().map(ToString::to_string)
    );
    let task = rt.task(&case.task.id).unwrap();
    assert_eq!(task.state, TaskState::Completed);
    assert!(!task.holds_slot);
    let read = eof_read(rt, case.session);
    let read_result = completed_result(&read);
    let read_scope = json!({
        "session_id": case.exec.host_session,
        "action": "read",
        "cursor": 0,
        "max_bytes": 65536,
        "request_id": case.request_id,
    });
    let read_proof = authorization_scope(&read, "write_stdin", read_scope);
    assert_eq!(
        read_proof.target, read.key,
        "the read proof must bind the original planned call"
    );
    let stdout = read_result["text"].as_str().unwrap().to_string();
    let exact = format!("{}-complete pid={}", case.profile, case.exec.identity.pid);
    assert_eq!(
        stdout.matches(exact.as_str()).count(),
        1,
        "the real child stdout line must appear exactly once; got {stdout:?}"
    );
    assert_eq!(read_result["eof"].as_bool(), Some(true));
    assert_eq!(read_result["truncated"].as_bool(), Some(false));
    assert_eq!(
        read_result["next_cursor"].as_u64(),
        read_result["status"]["output_cursor"].as_u64()
    );
    assert_eq!(
        read_result["consumed_bytes"].as_u64(),
        read_result["status"]["output_cursor"].as_u64()
    );
    assert_eq!(read_result["status"]["state"].as_str(), Some("exited"));
    assert_eq!(read_result["status"]["exit_code"].as_i64(), Some(0));
    assert_eq!(
        read_result["status"]["termination"].as_str(),
        Some("exited")
    );
    assert_eq!(
        read_result["status"]["output_complete"].as_bool(),
        Some(true)
    );
    assert_eq!(read_result["status"]["running"].as_bool(), Some(false));
    assert_eq!(
        project.done(case.profile).unwrap(),
        format!("{} complete pid={}\n", case.profile, case.exec.identity.pid)
    );
    let wait = exited_wait(rt, case.session);
    assert_eq!(
        completed_result(&wait)["status"]["exit_code"].as_i64(),
        Some(0)
    );
    assert_eq!(
        completed_result(&wait)["status"]["state"].as_str(),
        Some("exited")
    );
    let scopes = process_scopes(&project.db());
    let scope = scope_for_task(&scopes, &task.id).clone();
    assert_eq!(scope["state"].as_str(), Some("drained"));
    stopped_process_rows(project, &task.id);
    let cleanup = processes_stopped_audit(rt, case.session, &task.id);
    assert_eq!(
        calls_named(rt, case.session, "exec_command", true).len(),
        1,
        "the completed exec_command must remain the original one"
    );
    assert_eq!(
        completed_result(
            &calls_named(rt, case.session, "exec_command", true)
                .into_iter()
                .next()
                .unwrap()
        )["session_id"]
            .as_str(),
        Some(case.exec.host_session.as_str())
    );
    assert_model_ledger(rt, case.session, &task, provider.calls_for(case.text));
    assert_eq!(provider.calls_for(case.text), 2);
    ownership(rt, case.session);
    ToolCompletion {
        task,
        read,
        wait,
        scope,
        cleanup,
        stdout,
    }
}

/// Issue 11 item 6, safe-return mode: two real Host tools run together under
/// K = 2, C waits in the queue, A's child exits 0, the next read stays pending
/// on its exact permission while unread output keeps the slot, and cancelling
/// A confirms cleanup before C is admitted. B stays live until its own release
/// and must produce its real stdout, EOF and exit code before completion.
///
/// Live-cancel mode: A is cancelled inside its approved wait while its child is
/// still live. The terminal cleanup is unconfirmed, so A stays Draining with
/// its slot and scope open; only B's confirmed completion can admit C.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_real_host_tools_run_together_and_only_one_is_cancelled() {
    dual_scenario(DualMode::SafeReturn).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_cancel_keeps_the_unknown_slot_and_only_b_cleanup_admits_c() {
    dual_scenario(DualMode::LiveCancel).await;
}

async fn dual_scenario(mode: DualMode) {
    if std::env::var_os(CHILD_ENV).is_some() {
        child_main();
    }

    let prefix = dual_prefix(mode);
    let project = Project::new(match mode {
        DualMode::SafeReturn => "scheduler-dual",
        DualMode::LiveCancel => "scheduler-live-dual",
    });
    let _release = DualReleaseGuard {
        root: project.root.clone(),
        roles: vec![PROFILE_A.into(), PROFILE_B.into(), PROFILE_C.into()],
    };
    let provider = Arc::new(FixtureProvider::default());
    let runtime_provider: Arc<dyn Provider> = provider.clone();
    let rt = Runtime::open(
        project.db(),
        project.workspace(),
        runtime_provider,
        Model::new("fixture", "scheduler-tools"),
        dual_config(&project),
    )
    .unwrap();

    let other_session = rt.create(Some("other".into())).unwrap().id;
    let session_a = rt.create(Some("dual a".into())).unwrap().id;
    let session_b = rt.create(Some("dual b".into())).unwrap().id;
    let session_c = rt.create(Some("queued c".into())).unwrap().id;
    let task_a = rt.enqueue(&session_a, "dual a").unwrap();
    let task_b = rt.enqueue(&session_b, "dual b").unwrap();
    let worker_a = spawn_worker(&rt, &task_a.id);
    let worker_b = spawn_worker(&rt, &task_b.id);

    wait_until(
        "both real exec_command calls completed with live processes",
        || {
            calls_named(&rt, &session_a, "exec_command", true).len() == 1
                && calls_named(&rt, &session_b, "exec_command", true).len() == 1
                && project.marker(PROFILE_A).is_some()
                && project.marker(PROFILE_B).is_some()
                && rows_for_task(&process_rows(&project.db()), &task_a.id).len() == 1
                && rows_for_task(&process_rows(&project.db()), &task_b.id).len() == 1
        },
    )
    .await;
    let exec_a = exec_info(&rt, &session_a, &task_a, &project, PROFILE_A);
    let exec_b = exec_info(&rt, &session_b, &task_b, &project, PROFILE_B);
    let identity_a = exec_a.identity;
    let identity_b = exec_b.identity;
    assert_ne!(identity_a.pid, identity_b.pid);
    ownership(&rt, &session_a);
    ownership(&rt, &session_b);
    receipt(
        &format!("{prefix}exec-started"),
        dual_state(
            &rt,
            &[session_a.as_str(), session_b.as_str()],
            &provider,
            &project,
        ),
    );

    let task_c = rt.enqueue(&session_c, "queued c").unwrap();
    let worker_c = spawn_worker(&rt, &task_c.id);
    wait_until("task C is queued behind the two admitted tasks", || {
        let health = rt.health().unwrap();
        rt.task(&task_c.id).unwrap().state == TaskState::Queued
            && health.running == 2
            && health.queued >= 1
    })
    .await;

    let approval_a = wait_for_approval(&rt, &session_a, "write_stdin", "wait").await;
    let approval_b = wait_for_approval(&rt, &session_b, "write_stdin", "wait").await;
    assert_wait_approval(
        &approval_a,
        &session_a,
        &task_a,
        &exec_a.call,
        &exec_a.host_session,
        "a-wait",
    );
    assert_wait_approval(
        &approval_b,
        &session_b,
        &task_b,
        &exec_b.call,
        &exec_b.host_session,
        "b-wait",
    );
    assert_eq!(
        approval_a.attempt_id.as_str(),
        exec_a.call.lease.attempt_id.as_str()
    );
    assert_eq!(
        approval_b.attempt_id.as_str(),
        exec_b.call.lease.attempt_id.as_str()
    );

    // Another Session cannot read, approve or cancel the original task. The
    // pending approval remains exactly where it was created.
    assert!(
        rt.get(&session_b)
            .unwrap()
            .pending_approvals
            .iter()
            .all(|pending| pending.id != approval_a.id)
    );
    assert!(rt.get(&other_session).unwrap().pending_approvals.is_empty());
    assert!(rt.tasks(&other_session).unwrap().is_empty());
    assert!(
        rt.tasks(&session_b)
            .unwrap()
            .iter()
            .all(|task| task.id != task_a.id)
    );
    assert!(matches!(
        rt.approve(
            &session_b,
            &approval_a.id,
            ApprovalChoice::AllowOnce,
            "scheduler-tools"
        ),
        Err(RuntimeError::StaleApproval)
    ));
    assert!(matches!(
        rt.approve(
            &other_session,
            &approval_a.id,
            ApprovalChoice::AllowOnce,
            "scheduler-tools"
        ),
        Err(RuntimeError::StaleApproval)
    ));
    let mut wrong_session = rt.task(&task_a.id).unwrap().cancel_target();
    wrong_session.session_id = session_b.clone();
    assert!(matches!(
        rt.cancel(&wrong_session),
        Err(RuntimeError::StaleCancellation)
    ));
    assert!(!rt.task(&task_a.id).unwrap().cancel_requested);
    assert!(
        rt.get(&session_a)
            .unwrap()
            .pending_approvals
            .iter()
            .any(|pending| pending.id == approval_a.id)
    );

    // One reusable grant covers A's single wait. B's wait is one-time.
    let approved_a = rt
        .approve(
            &session_a,
            &approval_a.id,
            ApprovalChoice::AllowTask,
            "scheduler-tools",
        )
        .unwrap();
    let approved_b = rt
        .approve(
            &session_b,
            &approval_b.id,
            ApprovalChoice::AllowOnce,
            "scheduler-tools",
        )
        .unwrap();
    assert!(approved_a.decision.is_some());
    assert!(approved_b.decision.is_some());

    wait_until("both Host waits started under both admitted slots", || {
        let health = rt.health().unwrap();
        let a = rt.task(&task_a.id).unwrap();
        let b = rt.task(&task_b.id).unwrap();
        a.state == TaskState::Running
            && a.holds_slot
            && b.state == TaskState::Running
            && b.holds_slot
            && rt.task(&task_c.id).unwrap().state == TaskState::Queued
            && identity_a.is_running().unwrap()
            && identity_b.is_running().unwrap()
            && health.running == 2
            && calls_named(&rt, &session_a, "write_stdin", false).len() == 1
            && calls_named(&rt, &session_b, "write_stdin", false).len() == 1
    })
    .await;
    let barrier_a = rt.task(&task_a.id).unwrap();
    let barrier_b = rt.task(&task_b.id).unwrap();
    assert_eq!(
        barrier_a.attempt_id.as_deref(),
        Some(exec_a.call.lease.attempt_id.as_str())
    );
    assert_eq!(
        barrier_a.worker_id.as_deref(),
        Some(exec_a.call.lease.worker_id.as_str())
    );
    assert_eq!(barrier_a.fence, exec_a.call.lease.fence);
    assert_eq!(
        barrier_b.attempt_id.as_deref(),
        Some(exec_b.call.lease.attempt_id.as_str())
    );
    assert_eq!(
        barrier_b.worker_id.as_deref(),
        Some(exec_b.call.lease.worker_id.as_str())
    );
    assert_eq!(barrier_b.fence, exec_b.call.lease.fence);
    assert_eq!(
        completed_result(&exec_a.call)["state"].as_str(),
        Some("running")
    );
    assert_eq!(
        completed_result(&exec_b.call)["state"].as_str(),
        Some("running")
    );
    receipt(
        &format!("{prefix}barrier"),
        json!({
            "state": dual_state(
                &rt,
                &[session_a.as_str(), session_b.as_str(), session_c.as_str()],
                &provider,
                &project,
            ),
            "barrier": {
                "task_a": barrier_a,
                "task_b": barrier_b,
                "health": rt.health().unwrap(),
                "pid_a": identity_a.pid,
                "pid_b": identity_b.pid,
                "queued_task": rt.task(&task_c.id).unwrap(),
            },
        }),
    );

    match mode {
        DualMode::SafeReturn => {
            // A's child exits 0 while the cell is inside the approved wait.
            // The next read needs its own exact permission; unread output keeps
            // A's slot until cancellation confirms cleanup.
            fs::write(project.root.join(format!("{PROFILE_A}.release")), "release").unwrap();
            wait_until("A's child wrote its completion marker", || {
                project.done(PROFILE_A).is_some()
            })
            .await;
            stopped(&identity_a).await;
            wait_until("A's approved wait recorded the real normal exit", || {
                let mut exited = false;
                for call in calls_named(&rt, &session_a, "write_stdin", true) {
                    if call.arguments["action"].as_str() != Some("wait") {
                        continue;
                    }
                    if let Some(CapabilityOutcome::Completed { result }) = &call.outcome
                        && result["status"]["state"].as_str() == Some("exited")
                        && result["status"]["exit_code"].as_i64() == Some(0)
                        && result["status"]["output_complete"].as_bool() == Some(true)
                    {
                        exited = true;
                    }
                }
                exited
            })
            .await;
            let read_approval_a = wait_for_approval(&rt, &session_a, "write_stdin", "read").await;
            assert_read_approval(
                &read_approval_a,
                &session_a,
                &task_a,
                &exec_a.call,
                &exec_a.host_session,
                "a-read",
            );
            wait_until(
                "A holds its slot with unread output and a pending exact read",
                || {
                    let task = rt.task(&task_a.id).unwrap();
                    task.state == TaskState::Running
                        && task.holds_slot
                        && rt
                            .get(&session_a)
                            .unwrap()
                            .pending_approvals
                            .iter()
                            .any(|pending| pending.id == read_approval_a.id)
                },
            )
            .await;
            let a_before = rt.task(&task_a.id).unwrap();
            assert_eq!(
                a_before.attempt_id.as_deref(),
                Some(exec_a.call.lease.attempt_id.as_str())
            );
            assert_eq!(
                a_before.worker_id.as_deref(),
                Some(exec_a.call.lease.worker_id.as_str())
            );
            assert_eq!(a_before.fence, exec_a.call.lease.fence);
            assert_eq!(
                calls_named(&rt, &session_a, "exec_command", true).len(),
                1,
                "the settled exec_command must stay the original one"
            );
            receipt(
                &format!("{prefix}a-read-pending"),
                json!({
                    "state": dual_state(
                        &rt,
                        &[session_a.as_str(), session_b.as_str(), session_c.as_str()],
                        &provider,
                        &project,
                    ),
                    "a": {
                        "task": a_before,
                        "read_approval": read_approval_a,
                        "pid": identity_a.pid,
                        "process_running": identity_a.is_running().unwrap(),
                        "done": project.done(PROFILE_A),
                    },
                }),
            );

            let read_a_scope = json!({
                "session_id": exec_a.host_session,
                "action": "read",
                "cursor": 0,
                "max_bytes": 65536,
                "request_id": "a-read",
            });

            assert!(rt.cancel(&a_before.cancel_target()).unwrap());
            assert_eq!(
                rt.task(&task_c.id).unwrap().state,
                TaskState::Queued,
                "C must not be admitted before A's cleanup is confirmed"
            );
            receipt(
                &format!("{prefix}cancel-issued"),
                json!({
                    "task_a": rt.task(&task_a.id).unwrap(),
                    "task_c": rt.task(&task_c.id).unwrap(),
                    "health": rt.health().unwrap(),
                }),
            );
            let a_result = await_worker_result("dual tool a", worker_a).await;
            wait_until("A's original process scope was drained", || {
                let scopes = process_scopes(&project.db());
                let rows = rows_for_task(&scopes, &task_a.id);
                rows.len() == 1 && rows[0]["state"].as_str() == Some("drained")
            })
            .await;
            let scopes_a = process_scopes(&project.db());
            let scope_a = scope_for_task(&scopes_a, &task_a.id);
            assert_eq!(
                scope_a["evidence"].as_str(),
                Some("All owned process supervisors exited after managed process cleanup")
            );
            stopped_process_rows(&project, &task_a.id);
            let cleanup_a = processes_stopped_audit(&rt, &session_a, &task_a.id);
            let a_after = rt.task(&task_a.id).unwrap();
            assert!(!a_after.holds_slot, "a terminal task must release its slot");
            assert!(
                matches!(
                    a_after.state,
                    TaskState::Cancelled | TaskState::OutcomeUnknown
                ),
                "the cancelled task must end in a real recorded terminal state, not {:?}",
                a_after.state
            );
            assert_ne!(a_after.state, TaskState::Completed);
            assert_eq!(
                a_after.attempt_id.as_deref(),
                Some(exec_a.call.lease.attempt_id.as_str())
            );
            assert_eq!(
                a_after.worker_id.as_deref(),
                Some(exec_a.call.lease.worker_id.as_str())
            );
            assert_eq!(a_after.fence, exec_a.call.lease.fence);
            assert_eq!(
                calls_named(&rt, &session_a, "exec_command", true).len(),
                1,
                "the settled exec_command must never be duplicated or rewritten"
            );
            let exec_a_after = calls_named(&rt, &session_a, "exec_command", true)
                .into_iter()
                .next()
                .unwrap();
            assert_eq!(
                completed_result(&exec_a_after)["session_id"].as_str(),
                Some(exec_a.host_session.as_str())
            );
            let wait_a = exited_wait(&rt, &session_a);
            assert_eq!(
                wait_a.key.execution_id.as_str(),
                exec_a.call.key.execution_id.as_str()
            );
            let wait_a_scope = json!({
                "session_id": exec_a.host_session,
                "action": "wait",
                "request_id": "a-wait",
                "wait_ms": 60000,
            });
            let wait_a_proof = authorization_scope(&wait_a, "write_stdin", wait_a_scope);
            assert_eq!(
                wait_a_proof.approval_id.as_deref(),
                Some(approval_a.id.as_str())
            );
            let read_a = settled_call(
                &rt,
                &session_a,
                "write_stdin",
                &exec_a.call.key.execution_id,
                exec_a.call.key.sequence + 2,
            );
            assert_eq!(read_a.arguments, read_a_scope);
            assert!(
                matches!(&read_a.outcome, Some(CapabilityOutcome::NotExecuted { .. })),
                "the never-approved read must be recorded not executed: {:?}",
                read_a.outcome
            );
            receipt(
                &format!("{prefix}cancel-a"),
                json!({
                    "state": dual_state(
                        &rt,
                        &[session_a.as_str(), session_b.as_str(), session_c.as_str()],
                        &provider,
                        &project,
                    ),
                    "cancel": {
                        "task_before": a_before,
                        "task_after": a_after,
                        "worker_result": a_result.as_ref().map(|_| "ok").map_err(ToString::to_string),
                        "scope": scope_a,
                        "cleanup_audit": cleanup_a,
                        "approved_wait": wait_a,
                        "pending_read": read_a,
                        "queued_task": rt.task(&task_c.id).unwrap(),
                    },
                }),
            );

            let (task_c_running, exec_c) = admit_queued_c(
                &rt,
                &project,
                AdmissionCase {
                    session_c: &session_c,
                    task_a_id: &task_a.id,
                    task_b_id: &task_b.id,
                    task_c: &task_c,
                },
                mode,
                &exec_b,
                &identity_b,
            )
            .await;
            receipt(
                &format!("{prefix}admit-c"),
                dual_state(
                    &rt,
                    &[session_a.as_str(), session_b.as_str(), session_c.as_str()],
                    &provider,
                    &project,
                ),
            );

            let b_case = ToolCase {
                session: &session_b,
                task: &task_b,
                exec: &exec_b,
                profile: PROFILE_B,
                request_id: "b-read",
                text: "dual b",
            };
            let b_done = complete_released_tool(&rt, &project, &provider, b_case, worker_b).await;
            let c_case = ToolCase {
                session: &session_c,
                task: &task_c_running,
                exec: &exec_c,
                profile: PROFILE_C,
                request_id: "c-read",
                text: "queued c",
            };
            let c_done = complete_released_tool(&rt, &project, &provider, c_case, worker_c).await;
            receipt(
                &format!("{prefix}b-complete"),
                json!({
                    "state": dual_state(
                        &rt,
                        &[session_a.as_str(), session_b.as_str(), session_c.as_str()],
                        &provider,
                        &project,
                    ),
                    "b": {
                        "task": b_done.task,
                        "read": b_done.read,
                        "wait": b_done.wait,
                        "scope": b_done.scope,
                        "cleanup_audit": b_done.cleanup,
                        "stdout": b_done.stdout,
                        "done": project.done(PROFILE_B),
                    },
                    "c": {
                        "task": rt.task(&task_c.id).unwrap(),
                        "scope": c_done.scope,
                        "cleanup_audit": c_done.cleanup,
                        "stdout": c_done.stdout,
                        "read": c_done.read,
                    },
                }),
            );

            let final_health = rt.health().unwrap();
            assert_eq!(final_health.running, 0);
            assert_eq!(final_health.queued, 0);
            assert!(!identity_a.is_running().unwrap());
            assert!(!identity_b.is_running().unwrap());
            assert!(!exec_c.identity.is_running().unwrap());
            assert_model_ledger(
                &rt,
                &session_a,
                &rt.task(&task_a.id).unwrap(),
                provider.calls_for("dual a"),
            );
            assert_eq!(provider.calls_for("dual a"), 1);
            let calls = provider.calls();
            assert_eq!(calls.iter().filter(|call| call.text == "dual a").count(), 1);
            assert_eq!(calls.iter().filter(|call| call.text == "dual b").count(), 2);
            assert_eq!(
                calls.iter().filter(|call| call.text == "queued c").count(),
                2
            );
            assert_eq!(
                calls
                    .iter()
                    .filter(|call| call.text == "dual b")
                    .filter(|call| call.tool_result_turn)
                    .count(),
                1
            );
            assert_eq!(
                calls
                    .iter()
                    .filter(|call| call.text == "queued c")
                    .filter(|call| call.tool_result_turn)
                    .count(),
                1
            );
            ownership(&rt, &session_a);
            receipt(
                &format!("{prefix}final"),
                json!({
                    "state": dual_state(
                        &rt,
                        &[session_a.as_str(), session_b.as_str(), session_c.as_str(), other_session.as_str()],
                        &provider,
                        &project,
                    ),
                    "final": {
                        "task_a": rt.task(&task_a.id).unwrap(),
                        "task_b": rt.task(&task_b.id).unwrap(),
                        "task_c": rt.task(&task_c.id).unwrap(),
                        "health": final_health,
                        "read_c": c_done.read,
                        "scope_c": c_done.scope,
                        "cleanup_c": c_done.cleanup,
                    },
                }),
            );
        }
        DualMode::LiveCancel => {
            // Cancel the original task while its approved wait is still running
            // against the live child. A's release file is never written.
            let a_before = rt.task(&task_a.id).unwrap();
            assert_eq!(a_before.state, TaskState::Running);
            assert!(a_before.holds_slot);
            assert_eq!(
                a_before.attempt_id.as_deref(),
                Some(exec_a.call.lease.attempt_id.as_str())
            );
            assert_eq!(
                a_before.worker_id.as_deref(),
                Some(exec_a.call.lease.worker_id.as_str())
            );
            assert_eq!(a_before.fence, exec_a.call.lease.fence);
            assert!(identity_a.is_running().unwrap());
            assert_eq!(rt.task(&task_c.id).unwrap().state, TaskState::Queued);
            assert!(rt.cancel(&a_before.cancel_target()).unwrap());
            assert_eq!(
                rt.task(&task_c.id).unwrap().state,
                TaskState::Queued,
                "C must stay queued while A's cleanup is unconfirmed"
            );
            receipt(
                &format!("{prefix}cancel-issued"),
                json!({
                    "task_a": rt.task(&task_a.id).unwrap(),
                    "task_c": rt.task(&task_c.id).unwrap(),
                    "health": rt.health().unwrap(),
                    "pid_a": identity_a.pid,
                    "process_running": identity_a.is_running().unwrap(),
                }),
            );

            let a_result = await_worker_result("live dual tool a", worker_a).await;
            stopped(&identity_a).await;
            wait_until("A's OS identity is recorded stopped", || {
                let rows = process_rows(&project.db());
                let task_rows = rows_for_task(&rows, &task_a.id);
                task_rows.len() == 1 && task_rows[0]["state"].as_str() == Some("stopped")
            })
            .await;

            let a_after = rt.task(&task_a.id).unwrap();
            assert_eq!(
                a_after.state,
                TaskState::Draining,
                "unconfirmed process cleanup must keep the task draining, not terminal"
            );
            assert!(
                a_after.holds_slot,
                "unconfirmed process cleanup must retain the admitted slot"
            );
            assert!(a_after.cancel_requested);
            assert_eq!(
                a_after.attempt_id.as_deref(),
                Some(exec_a.call.lease.attempt_id.as_str())
            );
            assert_eq!(
                a_after.worker_id.as_deref(),
                Some(exec_a.call.lease.worker_id.as_str())
            );
            assert_eq!(a_after.fence, exec_a.call.lease.fence);
            let scopes_a = process_scopes(&project.db());
            let scope_a = scope_for_task(&scopes_a, &task_a.id);
            assert_eq!(
                scope_a["state"].as_str(),
                Some("open"),
                "unconfirmed terminal cleanup must leave its process scope open"
            );
            assert!(scope_a["evidence"].is_null());
            assert!(
                all_audit(&rt, &session_a).iter().all(|record| {
                    record.kind != "processes_stopped"
                        && record.kind != "run_finished"
                        && record.kind != "run_cancelled"
                }),
                "A must not publish a cleanup or terminal record before confirmation"
            );

            assert_eq!(
                calls_named(&rt, &session_a, "exec_command", true).len(),
                1,
                "the already-completed exec_command must never be rewritten"
            );
            let exec_a_after = calls_named(&rt, &session_a, "exec_command", true)
                .into_iter()
                .next()
                .unwrap();
            assert_eq!(
                completed_result(&exec_a_after)["session_id"].as_str(),
                Some(exec_a.host_session.as_str())
            );
            let wait_a = settled_call(
                &rt,
                &session_a,
                "write_stdin",
                &exec_a.call.key.execution_id,
                exec_a.call.key.sequence + 1,
            );
            let wait_a_scope = json!({
                "session_id": exec_a.host_session,
                "action": "wait",
                "request_id": "a-wait",
                "wait_ms": 60000,
            });
            assert_eq!(wait_a.arguments, wait_a_scope);
            let wait_a_proof = authorization_scope(&wait_a, "write_stdin", wait_a_scope);
            assert_eq!(
                wait_a_proof.approval_id.as_deref(),
                Some(approval_a.id.as_str())
            );
            match wait_a.outcome.as_ref().expect("the wait must settle") {
                CapabilityOutcome::Failed { error } => assert!(
                    error.contains("cancelled while waiting for terminal"),
                    "a known wait cancellation must stay a known failure: {error}"
                ),
                CapabilityOutcome::OutcomeUnknown { error } => {
                    assert!(
                        error.contains("terminal outcome is unknown"),
                        "an unknown wait must name the terminal outcome: {error}"
                    );
                    assert!(
                        error.contains("terminal process stopped with Cancelled; complete=false"),
                        "an unknown wait must retain the real termination: {error}"
                    );
                }
                other => panic!("a live-cancelled wait must not complete: {other:?}"),
            }
            assert!(
                !matches!(&wait_a.outcome, Some(CapabilityOutcome::Completed { .. })),
                "a cancelled live wait must never be reported as completed"
            );

            let error_text = match &a_result {
                Err(error) => error.to_string(),
                Ok(_) => panic!(
                    "cancelling a live tool must return the worker error, not a session view"
                ),
            };
            assert!(
                matches!(
                    &a_result,
                    Err(RuntimeError::RecoveryRequired {
                        outcome_unknown: true,
                        cleanup: Some(_),
                        ..
                    })
                ),
                "the live cancellation must require recovery with unknown outcomes: {error_text}"
            );
            assert!(
                error_text.contains("process cleanup is not confirmed"),
                "{error_text}"
            );
            assert!(
                error_text.contains("process resource cleanup is unconfirmed"),
                "{error_text}"
            );
            assert!(
                error_text.contains("terminal process stopped with Cancelled; complete=false"),
                "{error_text}"
            );
            let task_error = a_after
                .error
                .clone()
                .expect("the draining task records its cleanup error");
            assert!(
                task_error.contains("terminal process stopped with Cancelled; complete=false"),
                "{task_error}"
            );

            let b_live = rt.task(&task_b.id).unwrap();
            assert_eq!(b_live.state, TaskState::Running);
            assert!(b_live.holds_slot);
            assert_eq!(
                b_live.attempt_id.as_deref(),
                Some(exec_b.call.lease.attempt_id.as_str())
            );
            assert_eq!(
                b_live.worker_id.as_deref(),
                Some(exec_b.call.lease.worker_id.as_str())
            );
            assert_eq!(b_live.fence, exec_b.call.lease.fence);
            assert!(identity_b.is_running().unwrap());
            assert_eq!(rt.task(&task_c.id).unwrap().state, TaskState::Queued);
            let live_health = rt.health().unwrap();
            assert_eq!(
                (live_health.running, live_health.queued),
                (2, 1),
                "A's unknown slot and B's live slot must still be the only two admitted slots"
            );
            receipt(
                &format!("{prefix}cancel-a"),
                json!({
                    "state": dual_state(
                        &rt,
                        &[session_a.as_str(), session_b.as_str(), session_c.as_str()],
                        &provider,
                        &project,
                    ),
                    "cancel": {
                        "task_before": a_before,
                        "task_after": a_after,
                        "worker_result": a_result.as_ref().map(|_| "ok").map_err(ToString::to_string),
                        "scope": scope_a,
                        "approved_wait": wait_a,
                        "queued_task": rt.task(&task_c.id).unwrap(),
                        "health": live_health,
                    },
                }),
            );

            // B's confirmed terminal is the only event that can free the second
            // admitted slot. C remains queued until B's real completion.
            let b_case = ToolCase {
                session: &session_b,
                task: &task_b,
                exec: &exec_b,
                profile: PROFILE_B,
                request_id: "b-read",
                text: "dual b",
            };
            let b_done = complete_released_tool(&rt, &project, &provider, b_case, worker_b).await;
            let (task_c_running, exec_c) = admit_queued_c(
                &rt,
                &project,
                AdmissionCase {
                    session_c: &session_c,
                    task_a_id: &task_a.id,
                    task_b_id: &task_b.id,
                    task_c: &task_c,
                },
                mode,
                &exec_b,
                &identity_b,
            )
            .await;

            // Global audit sequence: B's cleanup and terminal precede C's first
            // durable audit row. C's admitted Core attempt starts no earlier
            // than B's terminal, so its admission cannot precede B's cleanup.
            let finish_b = finished_audit(&rt, &session_b, &task_b.id);
            let first_c = first_audit(&rt, &session_c);
            assert!(
                b_done.cleanup.cursor < finish_b.cursor,
                "B's processes_stopped cursor must precede its run_finished cursor"
            );
            assert!(
                finish_b.cursor < first_c.cursor,
                "B's terminal audit must precede C's first durable audit record"
            );
            assert!(
                attempt_started_at(&rt, &session_c) >= finish_b.timestamp,
                "C's admitted attempt must start after B's recorded terminal"
            );
            receipt(
                &format!("{prefix}admit-c"),
                json!({
                    "state": dual_state(
                        &rt,
                        &[session_a.as_str(), session_b.as_str(), session_c.as_str()],
                        &provider,
                        &project,
                    ),
                    "ordering": {
                        "b_processes_stopped": b_done.cleanup,
                        "b_run_finished": finish_b,
                        "c_first_audit": first_c,
                        "c_attempt_started_at": attempt_started_at(&rt, &session_c),
                        "task_a": rt.task(&task_a.id).unwrap(),
                    },
                }),
            );

            let c_case = ToolCase {
                session: &session_c,
                task: &task_c_running,
                exec: &exec_c,
                profile: PROFILE_C,
                request_id: "c-read",
                text: "queued c",
            };
            let c_done = complete_released_tool(&rt, &project, &provider, c_case, worker_c).await;

            // A's unknown slot is never cleared by another task's success.
            let a_final = rt.task(&task_a.id).unwrap();
            assert_eq!(a_final.state, TaskState::Draining);
            assert!(a_final.holds_slot, "A's unknown slot must remain reserved");
            assert!(a_final.cancel_requested);
            let final_health = rt.health().unwrap();
            assert_eq!(
                (final_health.running, final_health.queued),
                (1, 0),
                "only A's unknown slot may remain live and nothing may remain queued"
            );
            assert!(!identity_a.is_running().unwrap());
            assert!(!identity_b.is_running().unwrap());
            assert!(!exec_c.identity.is_running().unwrap());
            let scopes_a = process_scopes(&project.db());
            let scope_a_final = scope_for_task(&scopes_a, &task_a.id);
            assert_eq!(scope_a_final["state"].as_str(), Some("open"));
            assert!(
                all_audit(&rt, &session_a)
                    .iter()
                    .all(|record| record.kind != "processes_stopped"),
                "A's unknown cleanup must never be recorded as confirmed"
            );
            assert_model_ledger(&rt, &session_a, &a_final, provider.calls_for("dual a"));
            assert_eq!(provider.calls_for("dual a"), 1);
            let calls = provider.calls();
            assert_eq!(calls.iter().filter(|call| call.text == "dual a").count(), 1);
            assert_eq!(calls.iter().filter(|call| call.text == "dual b").count(), 2);
            assert_eq!(
                calls.iter().filter(|call| call.text == "queued c").count(),
                2
            );
            assert_eq!(
                calls
                    .iter()
                    .filter(|call| call.text == "dual b")
                    .filter(|call| call.tool_result_turn)
                    .count(),
                1
            );
            assert_eq!(
                calls
                    .iter()
                    .filter(|call| call.text == "queued c")
                    .filter(|call| call.tool_result_turn)
                    .count(),
                1
            );
            ownership(&rt, &session_a);
            receipt(
                &format!("{prefix}final"),
                json!({
                    "state": dual_state(
                        &rt,
                        &[session_a.as_str(), session_b.as_str(), session_c.as_str(), other_session.as_str()],
                        &provider,
                        &project,
                    ),
                    "final": {
                        "task_a": a_final,
                        "task_b": rt.task(&task_b.id).unwrap(),
                        "task_c": rt.task(&task_c.id).unwrap(),
                        "health": final_health,
                        "scope_a": scope_a_final,
                        "read_c": c_done.read,
                        "scope_c": c_done.scope,
                        "cleanup_c": c_done.cleanup,
                    },
                }),
            );
        }
    }
}
