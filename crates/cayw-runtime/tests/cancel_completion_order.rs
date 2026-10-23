//! Offline responses, real Runtime/Core/Queue/Requests and authenticated loopback IPC.
//! A synchronous gate inside one Provider poll fixes ordering without scheduler delays.
#![cfg(target_os = "macos")]

use cayw_agent::AgentEvent;
use cayw_ai::{
    AssistantEvent, AssistantMessage, AssistantStream, CancellationToken, Content, Message, Model,
    ModelRequest, Provider, StopReason,
};
use cayw_core::{AttemptLease, CoreEvent, EventRecord, SessionState};
use cayw_ipc::{Client, ServerConfig, serve};
use cayw_requests::{TaskIdentity, TaskOutcome, TaskState as RequestState};
use cayw_runtime::{CancelTarget, RequestPolicy, Runtime, RuntimeConfig, RuntimeError, TaskState};
use futures::Stream;
use rusqlite::{Connection, OpenFlags, types::ValueRef};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::Write,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};
use tokio::{net::TcpListener, sync::Semaphore, task::JoinHandle};
use uuid::Uuid;

const WATCHDOG: Duration = Duration::from_secs(120);
const TOKEN: &str = "offline-cancel-completion-order-token";
const PROMPT: &str = "Return the fixed offline completion for this ordering test.";
const ANSWER: &str = "The fixed offline response completed with Stop.";

struct Evidence {
    root: PathBuf,
    sequence: Mutex<u64>,
}
impl Evidence {
    fn new(root: PathBuf) -> Arc<Self> {
        write_new(&root.join("ordering.jsonl"), b"");
        Arc::new(Self {
            root,
            sequence: Mutex::new(0),
        })
    }
    fn record(&self, kind: &str, data: Value) {
        let mut sequence = self.sequence.lock().unwrap();
        *sequence += 1;
        let mut bytes = serde_json::to_vec(&json!({
            "sequence":*sequence, "kind":kind, "data":data,
        }))
        .unwrap();
        bytes.push(b'\n');
        let mut file = OpenOptions::new()
            .append(true)
            .open(self.root.join("ordering.jsonl"))
            .unwrap();
        file.write_all(&bytes).unwrap();
        file.sync_all().unwrap();
    }
    fn save(&self, name: &str, data: &impl Serialize) {
        write_json(&self.root.join(name), data);
    }
}
fn write_new(path: &Path, bytes: &[u8]) {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .unwrap();
    file.write_all(bytes).unwrap();
    file.sync_all().unwrap();
}
fn write_json(path: &Path, data: &impl Serialize) {
    write_new(path, &serde_json::to_vec_pretty(data).unwrap());
}
fn case_root(label: &str) -> (Option<tempfile::TempDir>, PathBuf) {
    let (temporary, base) = match std::env::var_os("CAYW_CANCEL_COMPLETION_ROOT") {
        Some(base) => {
            let base = PathBuf::from(base);
            assert!(
                base.is_absolute(),
                "retained evidence root must be absolute"
            );
            fs::create_dir_all(&base).unwrap();
            let base = base.canonicalize().unwrap();
            let source = Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .canonicalize()
                .unwrap();
            assert!(
                !base.starts_with(source),
                "retain evidence outside the source tree"
            );
            (None, base)
        }
        None => {
            let temporary = tempfile::tempdir().unwrap();
            let base = temporary.path().to_owned();
            (Some(temporary), base)
        }
    };
    let root = base.join(format!("{label}-{}", Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    fs::create_dir(root.join("storage")).unwrap();
    fs::create_dir(root.join("workspace")).unwrap();
    fs::create_dir(root.join("stages")).unwrap();
    println!("cancel-completion evidence: {}", root.display());
    (temporary, root)
}
fn operation() -> cayw_requests::OperationScope {
    cayw_requests::OperationScope::exact(
        "offline-read",
        "fs.read",
        json!({"path":"unused.txt"}),
        "offline-workspace-policy",
    )
}
fn config() -> RuntimeConfig {
    RuntimeConfig {
        request_policy: Some(RequestPolicy {
            permission_policy_id: "offline-ordering-policy".into(),
            operations: vec![operation()],
        }),
        ..Default::default()
    }
}
fn open(root: &Path, provider: Arc<dyn Provider>) -> Runtime {
    Runtime::open(
        root.join("storage/session.sqlite"),
        root.join("workspace"),
        provider,
        Model::new("fixture", "cancel-completion-order"),
        config(),
    )
    .unwrap()
}

struct Gate {
    entered: Semaphore,
    released: Mutex<bool>,
    changed: Condvar,
    emitted: AtomicUsize,
    dropped: AtomicUsize,
    evidence: Arc<Evidence>,
}
impl Gate {
    fn release(&self) {
        let mut released = self.released.lock().unwrap();
        assert!(!*released);
        self.evidence.record("gate_release", json!({}));
        *released = true;
        self.changed.notify_all();
    }
}
struct FixedProvider {
    gate: Arc<Gate>,
    calls: AtomicUsize,
    response_id: String,
}
impl Provider for FixedProvider {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        assert_eq!(
            self.calls.fetch_add(1, Ordering::SeqCst),
            0,
            "Provider replay"
        );
        let mut message = AssistantMessage::new(&request.model);
        message.content = vec![Content::text(ANSWER)];
        message.stop_reason = StopReason::Stop;
        message.response_id = Some(self.response_id.clone());
        self.gate.evidence.record(
            "provider_success_prepared",
            json!({"request":request,"response":message}),
        );
        Box::pin(ReadyTerminal {
            gate: self.gate.clone(),
            message: Some(message),
        })
    }
}
struct ReadyTerminal {
    gate: Arc<Gate>,
    message: Option<AssistantMessage>,
}
impl Stream for ReadyTerminal {
    type Item = AssistantEvent;
    fn poll_next(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.message.is_none() {
            return Poll::Ready(None);
        }
        // Keep this exact poll on its OS thread. Peer control uses other runtime
        // threads. A cancellation cannot drop this poll before the gate releases.
        let released = self.gate.released.lock().unwrap();
        self.gate
            .evidence
            .record("provider_poll_entered", json!({}));
        self.gate.entered.add_permits(1);
        let (released, timeout) = self
            .gate
            .changed
            .wait_timeout_while(released, WATCHDOG, |released| !*released)
            .unwrap();
        assert!(!timeout.timed_out() && *released, "gate watchdog expired");
        drop(released);
        let message = self.message.take().unwrap();
        assert_eq!(self.gate.emitted.fetch_add(1, Ordering::SeqCst), 0);
        self.gate
            .evidence
            .record("provider_done_emitted", json!({"message":message}));
        Poll::Ready(Some(AssistantEvent::Done { message }))
    }
}
impl Drop for ReadyTerminal {
    fn drop(&mut self) {
        self.gate.dropped.fetch_add(1, Ordering::SeqCst);
        self.gate.evidence.record(
            "provider_stream_dropped",
            json!({"done_emitted":self.message.is_none()}),
        );
    }
}

#[derive(Debug, Serialize)]
struct ApiError {
    code: String,
    message: String,
    outcome_unknown: bool,
}
fn sdk_error(error: RuntimeError) -> ApiError {
    ApiError {
        code: if matches!(error, RuntimeError::InvalidInput(_)) {
            "invalid_params"
        } else {
            "runtime_error"
        }
        .into(),
        outcome_unknown: matches!(
            error,
            RuntimeError::RecoveryRequired {
                outcome_unknown: true,
                ..
            }
        ),
        message: error.to_string(),
    }
}
#[derive(Clone)]
struct Api {
    runtime: Runtime,
    client: Option<Client>,
    role: &'static str,
    evidence: Arc<Evidence>,
}
impl Api {
    async fn call(&self, method: &str, params: Value) -> Result<Value, ApiError> {
        let operation_id = Uuid::new_v4().to_string();
        self.evidence.record(
            "request",
            json!({"operation_id":operation_id,"transport":if self.client.is_some() {"ipc"} else {"sdk"},
                "role":self.role,"method":method,"params":params}),
        );
        let result = if let Some(client) = &self.client {
            client
                .request(method, params)
                .await
                .map_err(|error| ApiError {
                    code: error.code,
                    message: error.message,
                    outcome_unknown: error.outcome_unknown,
                })
        } else {
            let result = match method {
                "session.create" => self
                    .runtime
                    .create(Some(params["label"].as_str().unwrap().into()))
                    .map(|value| json!(value)),
                "task.enqueue" => self
                    .runtime
                    .enqueue(
                        params["session_id"].as_str().unwrap(),
                        params["text"].as_str().unwrap(),
                    )
                    .map(|value| json!(value)),
                "task.resume" => self
                    .runtime
                    .resume(
                        params["task_id"].as_str().unwrap(),
                        CancellationToken::new(),
                    )
                    .await
                    .map(|value| json!(value)),
                "task.get" => self
                    .runtime
                    .task(params["task_id"].as_str().unwrap())
                    .map(|value| json!(value)),
                "session.cancel" => self
                    .runtime
                    .cancel(&serde_json::from_value(params).unwrap())
                    .map(|cancelled| json!({"cancelled":cancelled})),
                _ => panic!("unsupported test SDK operation"),
            };
            result.map_err(sdk_error)
        };
        self.evidence.record(
            "response",
            json!({"operation_id":operation_id,"role":self.role,"method":method,"result":result}),
        );
        result
    }
}
struct Endpoint {
    api: Api,
    stop: CancellationToken,
    server: Option<JoinHandle<std::io::Result<()>>>,
}
impl Endpoint {
    async fn new(runtime: Runtime, ipc: bool, role: &'static str, evidence: Arc<Evidence>) -> Self {
        let stop = CancellationToken::new();
        let (client, server) = if ipc {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(serve(
                listener,
                Arc::new(runtime.clone()),
                ServerConfig::new(TOKEN),
                stop.clone(),
            ));
            let client = Client::connect(address, TOKEN).await.unwrap();
            evidence.record(
                "authenticated_connection",
                json!({"role":role,"address":address}),
            );
            let health = client.request("runtime.health", json!({})).await.unwrap();
            evidence.record(
                "authenticated_health",
                json!({"role":role,"response":health}),
            );
            (Some(client), Some(server))
        } else {
            (None, None)
        };
        Self {
            api: Api {
                runtime,
                client,
                role,
                evidence,
            },
            stop,
            server,
        }
    }
    async fn close(self) {
        if let Some(client) = self.api.client {
            client.close().await;
        }
        self.stop.cancel();
        if let Some(server) = self.server {
            tokio::time::timeout(WATCHDOG, server)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
        }
        self.api
            .evidence
            .record("endpoint_closed", json!({"role":self.api.role}));
    }
}

fn readonly(path: &Path) -> Connection {
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    connection.busy_timeout(WATCHDOG).unwrap();
    connection
        .execute_batch("PRAGMA query_only=ON; BEGIN;")
        .unwrap();
    let check: String = connection
        .query_row("PRAGMA quick_check", [], |row| row.get(0))
        .unwrap();
    assert_eq!(check, "ok");
    connection
}
fn cell(value: ValueRef<'_>) -> Value {
    match value {
        ValueRef::Null => json!({"type":"null"}),
        ValueRef::Integer(value) => json!({"type":"integer","value":value}),
        ValueRef::Real(value) => json!({"type":"real","value":value}),
        ValueRef::Text(value) => json!({"type":"text","value":std::str::from_utf8(value).unwrap()}),
        ValueRef::Blob(value) => {
            json!({"type":"blob","hex":value.iter().map(|byte| format!("{byte:02x}")).collect::<String>()})
        }
    }
}
fn export(connection: &Connection) -> Value {
    let mut schema_statement = connection
        .prepare("SELECT type,name,tbl_name,sql FROM sqlite_schema ORDER BY type,name,tbl_name")
        .unwrap();
    let schema: Vec<Value> = schema_statement
        .query_map([], |row| {
            Ok(json!({
                "type":row.get::<_,String>(0)?,"name":row.get::<_,String>(1)?,
                "table":row.get::<_,String>(2)?,"sql":row.get::<_,Option<String>>(3)?,
            }))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let mut tables = BTreeMap::new();
    for entry in &schema {
        if entry["type"] != "table" {
            continue;
        }
        let name = entry["name"].as_str().unwrap();
        let mut statement = connection
            .prepare(&format!("SELECT * FROM \"{}\"", name.replace('"', "\"\"")))
            .unwrap();
        let columns: Vec<_> = statement
            .column_names()
            .into_iter()
            .map(str::to_owned)
            .collect();
        let mut cursor = statement.query([]).unwrap();
        let mut rows = vec![];
        while let Some(row) = cursor.next().unwrap() {
            rows.push(Value::Array(
                (0..columns.len())
                    .map(|index| cell(row.get_ref(index).unwrap()))
                    .collect(),
            ));
        }
        rows.sort_by_cached_key(Value::to_string);
        tables.insert(name.to_owned(), json!({"columns":columns,"rows":rows}));
    }
    json!({"schema":schema,"tables":tables})
}
fn business(mut exported: Value) -> Value {
    // RequestStore reads legitimately advance this monotonic clock. Preserve it
    // in the raw export and exclude only that column from business equality.
    if let Some(table) = exported["tables"].get_mut("request_meta") {
        let index = table["columns"]
            .as_array()
            .unwrap()
            .iter()
            .position(|name| name == "last_now_ms")
            .unwrap();
        table["columns"].as_array_mut().unwrap().remove(index);
        for row in table["rows"].as_array_mut().unwrap() {
            row.as_array_mut().unwrap().remove(index);
        }
    }
    exported
}
fn copy_storage(source: &Path, destination: &Path, relative: &Path, files: &mut Vec<Value>) {
    fs::create_dir(destination).unwrap();
    let mut entries: Vec<_> = fs::read_dir(source).unwrap().map(Result::unwrap).collect();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let next = relative.join(entry.file_name());
        let metadata = entry.metadata().unwrap();
        assert!(!entry.file_type().unwrap().is_symlink());
        if metadata.is_dir() {
            copy_storage(
                &entry.path(),
                &destination.join(entry.file_name()),
                &next,
                files,
            );
        } else {
            assert!(metadata.is_file());
            let bytes = fs::read(entry.path()).unwrap();
            write_new(&destination.join(entry.file_name()), &bytes);
            files.push(json!({"path":next,"bytes":bytes.len(),"device":metadata.dev(),"inode":metadata.ino()}));
        }
    }
}
#[derive(Debug, PartialEq)]
struct Stored {
    business: Value,
    identity: Value,
    task_data: Value,
}
fn capture_storage(evidence: &Evidence, stage: &str) -> Stored {
    let destination = evidence.root.join("stages").join(stage);
    fs::create_dir(&destination).unwrap();
    let path = evidence.root.join("storage/session.sqlite");
    let requests_path = evidence.root.join("storage/session.sqlite.requests.sqlite");
    let sidecar_path = evidence
        .root
        .join("storage/session.sqlite.requests.sqlite.authority");
    let core = readonly(&path);
    let requests = readonly(&requests_path);
    let core_export = export(&core);
    let requests_export = export(&requests);
    let sidecar: Value = serde_json::from_slice(&fs::read(&sidecar_path).unwrap()).unwrap();
    let (authority_id, stopped, clock): (String, i64, i64) = requests
        .query_row(
            "SELECT authority_id,authority_stopped,last_now_ms FROM request_meta WHERE singleton=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(sidecar["id"], authority_id);
    assert_eq!(sidecar["phase"], "ready");
    assert_eq!(sidecar["last_now_ms"], clock);
    assert_eq!(stopped, 0);
    let request_rows: Vec<Vec<u8>> = requests
        .prepare("SELECT data FROM request_tasks ORDER BY id")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        request_rows.len(),
        1,
        "the real Runtime must start one Requests authority"
    );
    let task_data: Value = serde_json::from_slice(&request_rows[0]).unwrap();
    let mut files = vec![];
    copy_storage(
        &evidence.root.join("storage"),
        &destination.join("storage"),
        Path::new(""),
        &mut files,
    );
    let identity = json!({
        "authority_id":authority_id,
        "files":([path,requests_path,sidecar_path].map(|path| {
            let metadata = fs::metadata(&path).unwrap();
            json!({"name":path.file_name().unwrap().to_str().unwrap(),"device":metadata.dev(),"inode":metadata.ino()})
        })),
    });
    let mut authority_business = sidecar.clone();
    authority_business
        .as_object_mut()
        .unwrap()
        .remove("last_now_ms");
    let business = json!({"core":business(core_export.clone()),"requests":business(requests_export.clone()),"authority":authority_business});
    write_json(
        &destination.join("readonly-tables.json"),
        &json!({
            "core":core_export,"requests":requests_export,"authority":sidecar,"files":files,"identity":identity,
            "comparison_exclusion":"request_meta.last_now_ms and matching authority.last_now_ms only",
        }),
    );
    Stored {
        business,
        identity,
        task_data,
    }
}
fn core_events(runtime: &Runtime, session: &str) -> Vec<EventRecord> {
    let mut events = vec![];
    loop {
        let after = events
            .last()
            .map_or(0, |record: &EventRecord| record.cursor);
        let page = runtime.events(session, after, 1000).unwrap();
        if page.is_empty() {
            break;
        }
        for record in page {
            assert_eq!(record.cursor, events.len() as u64 + 1);
            assert_eq!(record.session_id, session);
            events.push(record);
        }
    }
    events
}
#[derive(Debug, PartialEq)]
struct Observation {
    facts: Value,
    stored: Stored,
}
fn observe(
    runtime: &Runtime,
    evidence: &Evidence,
    stage: &str,
    session: &str,
    task: &str,
) -> Observation {
    let events = core_events(runtime, session);
    let mut audit = vec![];
    loop {
        let after = audit
            .last()
            .map_or(0, |record: &cayw_runtime::AuditRecord| record.cursor);
        let page = runtime.audit(session, after, 1000).unwrap();
        if page.is_empty() {
            break;
        }
        for record in page {
            assert_eq!(record.cursor, audit.len() as u64 + 1);
            assert_eq!(record.session_id, session);
            audit.push(record);
        }
    }
    let mut request_events = vec![];
    loop {
        let after = request_events
            .last()
            .map_or(0, |record: &cayw_requests::RequestEvent| record.sequence);
        let page = runtime.request_events(task, after, 64).unwrap();
        if page.is_empty() {
            break;
        }
        for record in page {
            assert_eq!(record.sequence, request_events.len() as u64 + 1);
            assert_eq!(record.task_id, task);
            request_events.push(record);
        }
    }
    let facts = json!({
        "snapshot":runtime.store().snapshot(session).unwrap(),"view":runtime.get(session).unwrap(),
        "core_events":events,"audit":audit,"task":runtime.task(task).unwrap(),
        "tasks":runtime.tasks(session).unwrap(),"grants":runtime.grants(session).unwrap(),
        "model_requests":runtime.model_requests(session).unwrap(),
        "request_state":runtime.request_task_state(task).unwrap(),"request_events":request_events,
        "request_operations":runtime.request_operations().unwrap(),
    });
    let stored = capture_storage(evidence, stage);
    write_json(
        &evidence
            .root
            .join("stages")
            .join(stage)
            .join("public-facts.json"),
        &facts,
    );
    evidence.record(
        "observation_committed",
        json!({"stage":stage,"core_cursor":facts["snapshot"]["cursor"],
        "queue_state":facts["task"]["state"],"requests_state":facts["request_state"]}),
    );
    Observation { facts, stored }
}
fn assert_terminal(
    observed: &Observation,
    lease: &AttemptLease,
    target: &CancelTarget,
    cancelled: bool,
    response_id: &str,
) -> u64 {
    let facts = &observed.facts;
    assert_eq!(facts["snapshot"]["state"], json!(SessionState::Idle));
    assert!(facts["snapshot"]["attempt"].is_null());
    assert_eq!(facts["snapshot"]["messages"], facts["view"]["messages"]);
    for field in [
        "pending_approvals",
        "unresolved_calls",
        "unresolved_capabilities",
        "executions",
    ] {
        assert_eq!(facts["snapshot"][field], json!([]));
    }
    assert_eq!(facts["grants"], json!([]));
    assert_eq!(facts["request_operations"], json!([operation()]));
    assert_eq!(facts["tasks"].as_array().unwrap().len(), 1);
    let state = if cancelled {
        TaskState::Cancelled
    } else {
        TaskState::Completed
    };
    let outcome = if cancelled {
        TaskOutcome::Cancelled
    } else {
        TaskOutcome::Completed
    };
    let task: cayw_runtime::TaskRecord = serde_json::from_value(facts["task"].clone()).unwrap();
    assert_eq!(task.cancel_target(), *target);
    assert_eq!(task.state, state);
    assert_eq!(task.cancel_requested, cancelled);
    let ledger: cayw_runtime::ModelRequestLedger =
        serde_json::from_value(facts["model_requests"].clone()).unwrap();
    assert_eq!(ledger.requests.len(), 1);
    let model_request = &ledger.requests[0];
    assert_eq!(model_request.identity.lease, *lease);
    assert_eq!(model_request.identity.task_id, target.task_id);
    assert_eq!(model_request.identity.turn, 1);
    assert_eq!(
        model_request.terminal,
        Some(if cancelled {
            cayw_runtime::ModelRequestTerminal::Cancelled
        } else {
            cayw_runtime::ModelRequestTerminal::Completed
        })
    );
    assert_eq!(model_request.observations.len(), 1);
    let received = &model_request.observations[0].observation;
    assert_eq!(received.event, "done");
    assert_eq!(received.response_id.as_deref(), Some(response_id));
    assert_eq!(received.stop_reason, StopReason::Stop);
    assert_eq!(received.usage_status, cayw_ai::UsageStatus::Unknown);
    assert_eq!(model_request.settled_tokens, None);
    assert_eq!(
        model_request.outstanding_reservation_tokens,
        ledger.token_limit
    );
    assert_eq!(ledger.settled_tokens, 0);
    assert_eq!(ledger.reserved_tokens, ledger.token_limit);
    assert!(ledger.budget_unknown);
    assert_eq!(ledger.cost_status, cayw_runtime::ModelCostStatus::Unknown);
    assert!(ledger.cost_amount.is_none() && ledger.cost_currency.is_none());
    let request_state: RequestState =
        serde_json::from_value(facts["request_state"].clone()).unwrap();
    assert!(
        matches!(request_state, RequestState::Ended { outcome: actual, .. } if actual == outcome)
    );
    let identity =
        TaskIdentity::new(&lease.session_id, &target.task_id, &lease.attempt_id).unwrap();
    assert_eq!(observed.stored.task_data["identity"], json!(identity));
    assert_eq!(
        observed.stored.task_data["attempts"],
        json!([lease.attempt_id])
    );
    assert_eq!(observed.stored.task_data["request_count"], 0);
    assert_eq!(observed.stored.task_data["intent"], facts["task"]["intent"]);
    assert_eq!(observed.stored.task_data["state"], facts["request_state"]);
    assert_eq!(
        observed.stored.task_data["catalog"]["operations"][0]["scope"],
        json!(operation())
    );
    let events: Vec<EventRecord> = serde_json::from_value(facts["core_events"].clone()).unwrap();
    assert_eq!(facts["snapshot"]["cursor"], events.len() as u64);
    assert_eq!(events.iter().filter(|record| matches!(&record.event, CoreEvent::AttemptStarted { lease: actual } if actual == lease)).count(), 1);
    assert_eq!(events.iter().filter(|record| matches!(&record.event, CoreEvent::AttemptFinished { lease: actual } if actual == lease)).count(), 1);
    let mut ended = 0;
    let mut history = vec![];
    let mut consumed = vec![];
    for record in &events {
        if record.attempt.is_some() {
            assert_eq!(record.attempt.as_ref(), Some(lease));
        }
        if let CoreEvent::Agent { event } = &record.event {
            match event.as_ref() {
                AgentEvent::AgentEnd { messages } => {
                    ended += 1;
                    assert_eq!(json!(messages), facts["snapshot"]["messages"]);
                }
                AgentEvent::MessageEnd { message } => {
                    history.push(message.clone());
                    if let Message::Assistant(message) = message
                        && message.response_id.as_deref() == Some(response_id)
                    {
                        // The ledger retains the upstream Stop. Accepted cancellation
                        // makes the Agent-visible terminal Aborted without losing text.
                        assert_eq!(
                            message.stop_reason,
                            if cancelled {
                                StopReason::Aborted
                            } else {
                                StopReason::Stop
                            }
                        );
                        assert_eq!(message.content, vec![Content::text(ANSWER)]);
                        assert!(message.tool_calls().is_empty());
                        consumed.push(record.cursor);
                    }
                }
                _ => {}
            }
        }
    }
    assert_eq!(ended, 1);
    assert_eq!(json!(history), facts["snapshot"]["messages"]);
    assert_eq!(
        history
            .iter()
            .filter(|message| matches!(message, Message::User { .. }))
            .count(),
        1
    );
    assert_eq!(
        history
            .iter()
            .find(|message| matches!(message, Message::User { .. }))
            .unwrap()
            .text(),
        PROMPT
    );
    assert_eq!(
        consumed.len(),
        1,
        "prepared success must actually reach Core MessageEnd"
    );
    let terminals: Vec<_> = facts["audit"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|record| record["kind"].as_str().unwrap().starts_with("run_"))
        .collect();
    assert_eq!(terminals.len(), 1);
    assert_eq!(
        terminals[0]["kind"],
        if cancelled {
            "run_cancelled"
        } else {
            "run_finished"
        }
    );
    assert_eq!(terminals[0]["task_id"], target.task_id);
    assert_eq!(terminals[0]["data"]["worker_id"], lease.worker_id);
    let requests = facts["request_events"].as_array().unwrap();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0]["kind"], "task_started");
    assert_eq!(requests[0]["actor"], "runtime-task-start");
    assert_eq!(requests[0]["payload"]["identity"], json!(identity));
    assert_eq!(requests[1]["kind"], "task_closing");
    assert_eq!(requests[1]["actor"], "runtime-task-close");
    assert_eq!(requests[2]["kind"], "task_ended");
    assert_eq!(requests[2]["actor"], "runtime-task-end");
    assert_eq!(requests[2]["payload"], facts["request_state"]);
    assert!(requests.iter().all(|record| record["request_id"].is_null()));
    consumed[0]
}
async fn cancel(api: &Api, target: &CancelTarget, expected: bool) {
    let result = api.call("session.cancel", json!(target)).await.unwrap();
    assert_eq!(result, json!({"cancelled":expected}));
}
async fn rejected_resume(api: &Api, task: &str) {
    let error = api
        .call("task.resume", json!({"task_id":task}))
        .await
        .unwrap_err();
    assert_eq!(error.code, "invalid_params");
    assert!(!error.outcome_unknown);
}

async fn ordered_case(ipc: bool, cancelled: bool) {
    let label = format!(
        "{}-{}",
        if ipc { "ipc" } else { "sdk" },
        if cancelled {
            "cancel-first"
        } else {
            "completion-first"
        }
    );
    let (_temporary, root) = case_root(&label);
    let evidence = Evidence::new(root.clone());
    evidence.save("case.json", &json!({"label":label,"transport":if ipc {"authenticated-loopback-ipc"} else {"sdk"},
        "order":if cancelled {"cancel-before-release"} else {"resume-and-both-commits-before-cancel"},
        "feature_requirement":"none","real_model_calls":0,"request_policy":operation()}));
    let gate = Arc::new(Gate {
        entered: Semaphore::new(0),
        released: Mutex::new(false),
        changed: Condvar::new(),
        emitted: AtomicUsize::new(0),
        dropped: AtomicUsize::new(0),
        evidence: evidence.clone(),
    });
    let provider = Arc::new(FixedProvider {
        gate: gate.clone(),
        calls: AtomicUsize::new(0),
        response_id: format!("g2-{}", Uuid::new_v4()),
    });
    let owner = Endpoint::new(
        open(&root, provider.clone()),
        ipc,
        "owner",
        evidence.clone(),
    )
    .await;
    let peer = Endpoint::new(open(&root, provider.clone()), ipc, "peer", evidence.clone()).await;
    let session = owner
        .api
        .call("session.create", json!({"label":label}))
        .await
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let admitted = owner
        .api
        .call("task.enqueue", json!({"session_id":session,"text":PROMPT}))
        .await
        .unwrap();
    let task_id = admitted["id"].as_str().unwrap().to_owned();
    let run = {
        let api = owner.api.clone();
        let task_id = task_id.clone();
        tokio::spawn(async move { api.call("task.resume", json!({"task_id":task_id})).await })
    };
    tokio::time::timeout(WATCHDOG, gate.entered.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    let active = peer
        .api
        .call("task.get", json!({"task_id":task_id}))
        .await
        .unwrap();
    let original: cayw_runtime::TaskRecord = serde_json::from_value(active).unwrap();
    let lease = owner.api.runtime.get(&session).unwrap().attempt.unwrap();
    let target = original.cancel_target();
    assert_eq!(original.state, TaskState::Running);
    assert!(!original.cancel_requested);
    assert_eq!(target.worker_id.as_deref(), Some(lease.worker_id.as_str()));
    assert_eq!(
        target.attempt_id.as_deref(),
        Some(lease.attempt_id.as_str())
    );
    assert_eq!(target.session_id, lease.session_id);
    assert!(target.fence > 0);
    let ready = observe(
        &owner.api.runtime,
        &evidence,
        "01-ready",
        &session,
        &task_id,
    );
    let request_identity: TaskIdentity =
        serde_json::from_value(ready.stored.task_data["identity"].clone()).unwrap();
    assert_eq!(
        request_identity,
        TaskIdentity::new(&session, &task_id, &lease.attempt_id).unwrap()
    );
    assert_eq!(ready.facts["request_state"], json!(RequestState::Active));
    assert_eq!(ready.facts["request_events"].as_array().unwrap().len(), 1);
    assert_eq!(gate.emitted.load(Ordering::SeqCst), 0);
    evidence.save("original.json", &json!({"admitted":admitted,"task":original,"attempt":lease,"cancel_target":target,"request_identity":request_identity}));
    if cancelled {
        cancel(&peer.api, &target, true).await;
        let accepted = observe(
            &owner.api.runtime,
            &evidence,
            "02-cancel-accepted",
            &session,
            &task_id,
        );
        assert_eq!(accepted.facts["task"]["cancel_requested"], true);
        assert_eq!(accepted.facts["task"]["state"], json!(TaskState::Running));
        assert_eq!(accepted.facts["snapshot"]["attempt"], json!(lease));
        assert_eq!(accepted.facts["request_state"]["state"], "closing");
        assert_eq!(
            accepted.facts["request_events"].as_array().unwrap().len(),
            2
        );
        assert_eq!(accepted.stored.identity, ready.stored.identity);
        assert_eq!(accepted.facts["snapshot"], ready.facts["snapshot"]);
        assert_eq!(accepted.facts["core_events"], ready.facts["core_events"]);
        assert_eq!(accepted.facts["audit"], ready.facts["audit"]);
        assert_eq!(accepted.facts["grants"], ready.facts["grants"]);
        assert_eq!(
            accepted.stored.task_data["identity"],
            ready.stored.task_data["identity"]
        );
        assert_eq!(
            accepted.facts["request_events"][0],
            ready.facts["request_events"][0]
        );
        assert_eq!(gate.emitted.load(Ordering::SeqCst), 0);
        evidence.record(
            "accepted_cancel_before_release",
            json!({"target":target,"provider_done_emitted":false}),
        );
    }
    gate.release();
    let resumed = tokio::time::timeout(WATCHDOG, run)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    evidence.save("resume-response.json", &resumed);
    let committed = observe(
        &owner.api.runtime,
        &evidence,
        "03-original-terminal",
        &session,
        &task_id,
    );
    assert_eq!(resumed, committed.facts["view"]);
    assert_eq!(committed.stored.identity, ready.stored.identity);
    for field in ["core_events", "audit", "request_events"] {
        let prefix = ready.facts[field].as_array().unwrap();
        assert!(
            committed.facts[field]
                .as_array()
                .unwrap()
                .starts_with(prefix)
        );
    }
    let cursor = assert_terminal(
        &committed,
        &lease,
        &target,
        cancelled,
        &provider.response_id,
    );
    assert_eq!(gate.emitted.load(Ordering::SeqCst), 1);
    assert_eq!(gate.dropped.load(Ordering::SeqCst), 1);
    evidence.record(
        "prepared_success_consumed",
        json!({"response_id":provider.response_id,"message_end_cursor":cursor}),
    );
    evidence.record("original_resume_and_terminal_commits_confirmed", json!({"task":committed.facts["task"],"requests":committed.facts["request_state"],"core":committed.facts["snapshot"]}));
    for (index, action) in ["late-cancel", "duplicate-cancel", "original-resume"]
        .into_iter()
        .enumerate()
    {
        if action == "original-resume" {
            rejected_resume(&peer.api, &task_id).await;
        } else {
            cancel(&peer.api, &target, false).await;
        }
        let observed = observe(
            &owner.api.runtime,
            &evidence,
            &format!("04-{index}-{action}"),
            &session,
            &task_id,
        );
        assert_eq!(observed, committed, "late control changed committed facts");
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    }
    owner.close().await;
    peer.close().await;
    let closed = capture_storage(&evidence, "05-closed");
    assert_eq!(closed, committed.stored);
    for reopen in 1..=2 {
        let endpoint = Endpoint::new(
            open(&root, provider.clone()),
            ipc,
            "reopened",
            evidence.clone(),
        )
        .await;
        let observed = observe(
            &endpoint.api.runtime,
            &evidence,
            &format!("06-reopen-{reopen}"),
            &session,
            &task_id,
        );
        assert_eq!(observed, committed, "reopen changed original facts");
        cancel(&endpoint.api, &target, false).await;
        rejected_resume(&endpoint.api, &task_id).await;
        let after = observe(
            &endpoint.api.runtime,
            &evidence,
            &format!("07-reopen-{reopen}-late-controls"),
            &session,
            &task_id,
        );
        assert_eq!(after, committed);
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        endpoint.close().await;
        assert_eq!(
            capture_storage(&evidence, &format!("08-reopen-{reopen}-closed")),
            committed.stored
        );
    }
    assert!(
        fs::read_dir(root.join("workspace"))
            .unwrap()
            .next()
            .is_none()
    );
    evidence.save("result.json", &json!({"session_id":session,"task_id":task_id,"cancel_target":target,
        "cancelled":cancelled,"provider_calls":provider.calls.load(Ordering::SeqCst),
        "provider_success_emitted":gate.emitted.load(Ordering::SeqCst),"provider_stream_dropped":gate.dropped.load(Ordering::SeqCst),
        "prepared_success_consumed_cursor":cursor,"reopens":2,"late_controls_preserved_facts":true,
        "core_state":SessionState::Idle,"queue_state":committed.facts["task"]["state"],"requests_state":committed.facts["request_state"],
        "real_model_calls":0}));
    File::open(&root).unwrap().sync_all().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sdk_cancel_before_ready_terminal_preserves_canonical_cancelled() {
    ordered_case(false, true).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sdk_completed_before_cancel_preserves_canonical_completed() {
    ordered_case(false, false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn authenticated_ipc_cancel_before_ready_terminal_preserves_canonical_cancelled() {
    ordered_case(true, true).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn authenticated_ipc_completed_before_cancel_preserves_canonical_completed() {
    ordered_case(true, false).await;
}
