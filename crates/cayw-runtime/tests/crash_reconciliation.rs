//! Trusted decisions after a real killed worker, using independently observed
//! service receipts and the original Core/Cells identities. Entirely offline.
#![cfg(all(feature = "fault-injection", target_os = "macos"))]

#[allow(dead_code)]
#[path = "fixtures/recovery.rs"]
mod fixture;

use async_trait::async_trait;
use cayw_ai::{
    AssistantEvent, AssistantMessage, AssistantStream, CancellationToken, Content, Message, Model,
    ModelRequest, Provider, StopReason,
};
use cayw_cells::{CellRecord, IntentStatus};
use cayw_codemode::CapabilityError;
use cayw_core::{
    CapabilityCallRecord, CapabilityOutcome, CapabilityReconciliation, CoreEvent, EventRecord,
    ReconciliationOrigin, ReconciliationRequest, ReconciliationVerifier, SessionState,
};
use cayw_host::ExternalHost;
use cayw_ipc::{Client, ClientConfig, ServerConfig};
use cayw_runtime::{Runtime, RuntimeConfig, RuntimeError, ScopedPermission};
use fixture::{Identity, Result, SCRIPT, WATCHDOG};
use futures::FutureExt;
use rusqlite::{Connection, OpenFlags, types::ValueRef};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    os::unix::process::ExitStatusExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

const TOKEN: &str = "offline-reconciliation-ipc-token";
const BUDGET: Duration = Duration::from_secs(600);
const NEXT_PROMPT: &str =
    "Begin a new task after host verification. Do not replay the old capability.";
const NEXT_REPLY: &str = "New offline task completed after trusted reconciliation.";
const SDK_ACTOR: &str = "receipt-recovery-user";

fn append(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    let mut file = OpenOptions::new().append(true).create(true).open(path)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    fixture::sync_directory(path.parent().ok_or("log has no parent")?)?;
    Ok(())
}
fn file(root: &Path, name: &str) -> Result<Value> {
    let path = root.join(name);
    if path.exists() {
        fixture::read_json(&path)
    } else {
        Ok(Value::Null)
    }
}
fn database_rows(root: &Path) -> Result<Value> {
    let db = Connection::open_with_flags(
        root.join("session.sqlite"),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    db.execute_batch("PRAGMA query_only=ON; BEGIN;")?;
    let names = db.prepare("SELECT name FROM sqlite_schema WHERE type='table' AND substr(name,1,7)<>'sqlite_' ORDER BY name")?
        .query_map([], |row| row.get::<_, String>(0))?.collect::<std::result::Result<Vec<_>, _>>()?;
    let mut tables = serde_json::Map::new();
    for name in names {
        let mut statement =
            db.prepare(&format!("SELECT * FROM \"{}\"", name.replace('"', "\"\"")))?;
        let width = statement.column_count();
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
        tables.insert(name, json!(rows));
    }
    Ok(Value::Object(tables))
}
fn records(root: &Path) -> Result<(Identity, Vec<EventRecord>, Vec<CellRecord>)> {
    let identity: Identity = serde_json::from_value(file(root, "identity.json")?)?;
    let db = Connection::open_with_flags(
        root.join("session.sqlite"),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    db.execute_batch("PRAGMA query_only=ON; BEGIN;")?;
    let mut query =
        db.prepare("SELECT record FROM core_events WHERE session_id=?1 ORDER BY cursor")?;
    let events = query
        .query_map([&identity.session_id], |row| row.get::<_, String>(0))?
        .map(|row| Ok(serde_json::from_str(&row?)?))
        .collect::<Result<Vec<EventRecord>>>()?;
    let mut query =
        db.prepare("SELECT body FROM cells_v1 WHERE session=?1 AND task=?2 ORDER BY id")?;
    let cells = query
        .query_map([&identity.session_id, &identity.task_id], |row| {
            row.get::<_, String>(0)
        })?
        .map(|row| Ok(serde_json::from_str(&row?)?))
        .collect::<Result<Vec<CellRecord>>>()?;
    Ok((identity, events, cells))
}
fn original(root: &Path) -> Result<(Identity, CapabilityCallRecord, CellRecord)> {
    let (identity, events, cells) = records(root)?;
    let calls: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.event {
            CoreEvent::CapabilityStarted { call } => Some((event, call)),
            _ => None,
        })
        .collect();
    if calls.len() != 1 || cells.len() != 1 {
        return Err("expected one original started service call and cell".into());
    }
    let (event, call) = calls[0];
    let cell = &cells[0];
    if event.session_id != identity.session_id
        || event.attempt.as_ref() != Some(&call.lease)
        || call.lease.session_id != identity.session_id
        || call.task_id != identity.task_id
        || call.name != "search.query"
        || call.arguments != fixture::arguments()
        || cell.cell_id != call.key.execution_id
        || cell.parent_call_id != call.parent_call_id
        || cell.source_hash != call.source_hash
        || cell.source != SCRIPT
        || cell.identity.session_id != identity.session_id
        || cell.identity.task_id != identity.task_id
        || cell.identity.attempt_id != call.lease.attempt_id
        || cell.identity.permission_context
            != format!("{}:{}", call.lease.worker_id, call.lease.fence)
        || cell.calls.len() != 1
        || cell.calls[0].sequence.checked_sub(1) != Some(call.key.sequence)
        || cell.calls[0].name != call.name
        || cell.calls[0].arguments != call.arguments
    {
        return Err("independent Core/Cells origins disagree".into());
    }
    Ok((identity, call.clone(), cell.clone()))
}

struct Verdict {
    call: CapabilityCallRecord,
    outcome: CapabilityOutcome,
    evidence: String,
}
/// No reconciliation request is an input here. Every decision comes from the
/// stopped process's real gate, Core/Cells originals and external service files.
fn derive_verdict(root: &Path) -> Result<Verdict> {
    let (_, call, cell) = original(root)?;
    let exit = file(root, "child-exit.json")?;
    if exit["signal"] != 9 || !exit["code"].is_null() {
        return Err("original worker has no waited SIGKILL proof".into());
    }
    let ready = file(root, "ready.json")?;
    let hooks = fixture::read_lines(&root.join("hooks.jsonl"))?;
    if !hooks.iter().any(|hook| hook == &ready["hook"]) {
        return Err("ready hook has no original log entry".into());
    }
    let start = hooks
        .iter()
        .find(|hook| hook["point"] == "core.start.committed")
        .ok_or("missing trusted Core start hook")?;
    let origin = &start["data"];
    if origin["lease"] != serde_json::to_value(&call.lease)?
        || origin["key"] != serde_json::to_value(&call.key)?
        || origin["execution"] != serde_json::to_value(cell.execution())?
        || origin["arguments"] != call.arguments
        || origin["name"] != call.name
    {
        return Err("start hook differs from independent original identities".into());
    }
    let entries = fixture::read_lines(&root.join("external-entries.jsonl"))?;
    if ready["checkpoint"] == "core.start.committed" {
        if !entries.is_empty()
            || root.join("effect.json").exists()
            || root.join("effect-receipt.json").exists()
        {
            return Err("service evidence contradicts the original pre-entry gate".into());
        }
        return Ok(Verdict { call, outcome: CapabilityOutcome::NotExecuted {
            reason: "Original worker was gated before external entry and reaped by SIGKILL; service entry log and effect are absent".into(),
        }, evidence: root.join("ready.json").to_string_lossy().into_owned() });
    }
    if ready["checkpoint"] != "host.effect.durable"
        || entries.len() != 1
        || entries[0]["trusted_origin"] != *origin
        || entries[0]["entry_count"] != 1
    {
        return Err("no unique original external entry and durable gate".into());
    }
    let receipt = fixture::read_json(&root.join("effect-receipt.json"))?;
    if receipt["trusted_origin"] != *origin
        || receipt["entry_count"] != 1
        || receipt["capability"] != call.name
        || receipt["arguments"] != call.arguments
    {
        return Err("independent service receipt has a different original call".into());
    }
    let outcome: CapabilityOutcome = serde_json::from_value(receipt["outcome"].clone())?;
    match &outcome {
        CapabilityOutcome::Completed { result } => {
            let effect = fixture::read_json(&root.join("effect.json"))?;
            if effect["trusted_origin"] != *origin
                || effect["result"] != *result
                || result["value"] != "fixture"
                || uuid::Uuid::parse_str(result["effect_id"].as_str().unwrap_or_default()).is_err()
            {
                return Err("success receipt differs from actual unique effect bytes".into());
            }
        }
        CapabilityOutcome::Failed { error } => {
            if root.join("effect.json").exists()
                || !root.join("blocked-effect").is_dir()
                || receipt["attempted_path"] != root.join("blocked-effect").to_str().unwrap()
                || receipt["os_error"]["raw_os_error"].as_i64().is_none()
                || receipt["os_error"]["message"]
                    .as_str()
                    .is_none_or(|message| error != &format!("known service failure: {message}"))
            {
                return Err(
                    "failed receipt is unsupported by the real failed filesystem operation".into(),
                );
            }
        }
        _ => return Err("service receipt has no conclusive success or known failure".into()),
    }
    Ok(Verdict {
        call,
        outcome,
        evidence: root
            .join("effect-receipt.json")
            .to_string_lossy()
            .into_owned(),
    })
}
struct ReceiptVerifier {
    root: PathBuf,
    allowed_actor: String,
}
impl ReconciliationVerifier for ReceiptVerifier {
    fn verify(&self, request: &ReconciliationRequest) -> std::result::Result<(), String> {
        let result = (|| -> Result<()> {
            let verdict = derive_verdict(&self.root)?;
            let ReconciliationRequest::Capability { reconciliation } = request else {
                return Err(
                    "this service verifier authorizes only its original capability receipt".into(),
                );
            };
            if reconciliation.actor != self.allowed_actor {
                return Err("actor is not authorized by the service host".into());
            }
            if reconciliation.key != verdict.call.key
                || reconciliation.expected.as_ref() != &ReconciliationOrigin::from(&verdict.call)
                || reconciliation.external_provenance != verdict.call.external_provenance
            {
                return Err(
                    "request differs from the independent original Core/Cells identity".into(),
                );
            }
            if reconciliation.evidence != verdict.evidence {
                return Err(
                    "evidence does not name the independently verified original receipt".into(),
                );
            }
            if reconciliation.outcome != verdict.outcome {
                return Err(
                    "proposed outcome contradicts the independent service observation".into(),
                );
            }
            Ok(())
        })()
        .map_err(|error| error.to_string());
        let (actor, expected, outcome) = match request {
            ReconciliationRequest::Capability { reconciliation } => (
                reconciliation.actor.as_str(),
                serde_json::to_value(&reconciliation.expected).unwrap(),
                serde_json::to_value(&reconciliation.outcome).unwrap(),
            ),
            ReconciliationRequest::Tool { reconciliation } => (
                reconciliation.actor.as_str(),
                serde_json::to_value(&reconciliation.expected).unwrap(),
                serde_json::to_value(&reconciliation.outcome).unwrap(),
            ),
        };
        append(
            &self.root.join("verifier-observations.jsonl"),
            &json!({
                "actor":actor,"expected":expected,"proposed_outcome":outcome,
                "accepted":result.is_ok(),"error":result.as_ref().err(),
            }),
        )
        .map_err(|error| error.to_string())?;
        result
    }
}

#[derive(Default)]
struct Capture(Mutex<Option<Value>>);
struct Service {
    root: PathBuf,
    capture: Arc<Capture>,
    fail: bool,
}
#[async_trait]
impl ExternalHost for Service {
    async fn call(
        &self,
        name: &str,
        args: Value,
        _: CancellationToken,
    ) -> std::result::Result<Value, CapabilityError> {
        assert_eq!(name, "search.query");
        assert_eq!(args, fixture::arguments());
        let origin = self
            .capture
            .0
            .lock()
            .unwrap()
            .clone()
            .expect("external service has no trusted start origin");
        let entries = fixture::read_lines(&self.root.join("external-entries.jsonl")).unwrap();
        append(&self.root.join("external-entries.jsonl"), &json!({
            "entry_count":entries.len()+1,"trusted_origin":origin,"capability":name,"arguments":args,
        })).unwrap();
        assert!(entries.is_empty(), "old service was entered again");
        cayw_ai::faults::hit("host.effect.before", json!({"trusted_origin":origin}));
        let mut receipt =
            json!({"trusted_origin":origin,"capability":name,"arguments":args,"entry_count":1});
        let response = if self.fail {
            let target = self.root.join("blocked-effect");
            let error = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&target)
                .expect_err("the real operation must fail on the pre-existing directory");
            let message = format!("known service failure: {error}");
            receipt["attempted_path"] = json!(target);
            receipt["os_error"] =
                json!({"raw_os_error":error.raw_os_error(),"message":error.to_string()});
            receipt["outcome"] = serde_json::to_value(CapabilityOutcome::Failed {
                error: message.clone(),
            })
            .unwrap();
            Err(CapabilityError::new(message))
        } else {
            let result = json!({"effect_id":uuid::Uuid::new_v4().to_string(),"value":"fixture"});
            fixture::write_json(
                &self.root.join("effect.json"),
                &json!({"trusted_origin":origin,"result":result}),
            )
            .unwrap();
            receipt["outcome"] = serde_json::to_value(CapabilityOutcome::Completed {
                result: result.clone(),
            })
            .unwrap();
            Ok(result)
        };
        fixture::write_json(&self.root.join("effect-receipt.json"), &receipt).unwrap();
        fixture::sync_directory(&self.root).unwrap();
        cayw_ai::faults::hit(
            "host.effect.durable",
            json!({"trusted_origin":origin,"receipt_path":self.root.join("effect-receipt.json")}),
        );
        response
    }
}
struct Offline {
    root: PathBuf,
    original: bool,
    allow_next: Arc<AtomicBool>,
    calls: AtomicU64,
}
impl Provider for Offline {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let index = self.calls.fetch_add(1, Ordering::SeqCst);
        let message = if self.original {
            cayw_runtime::fixture::script_response(
                &request,
                SCRIPT,
                &format!("real-reconciliation-{index}"),
            )
        } else {
            if !self.allow_next.load(Ordering::SeqCst) {
                append(
                    &self.root.join("forbidden-provider.jsonl"),
                    &json!({"messages":request.messages}),
                )
                .unwrap();
                panic!("model was invoked before explicit new-task authorization");
            }
            assert!(
                request
                    .messages
                    .iter()
                    .any(|message| matches!(message, Message::User { content, .. }
                if cayw_ai::content_text(content) == NEXT_PROMPT))
            );
            let mut message = AssistantMessage::new(&request.model);
            message.content = vec![Content::text(NEXT_REPLY)];
            message.stop_reason = StopReason::Stop;
            message
        };
        append(
            &self.root.join(if self.original {
                "provider-original.jsonl"
            } else {
                "provider-next.jsonl"
            }),
            &json!({
                "index":index,"messages":request.messages,"response":message,
            }),
        )
        .unwrap();
        Box::pin(futures::stream::iter([AssistantEvent::Done { message }]))
    }
}
fn open(
    root: &Path,
    original: bool,
    capture: Arc<Capture>,
    allow_next: Arc<AtomicBool>,
    verifier: Option<Arc<dyn ReconciliationVerifier>>,
) -> Result<Runtime> {
    let mut config = RuntimeConfig {
        permissions: vec![ScopedPermission::exact(
            "search.query",
            fixture::arguments(),
        )],
        max_run_time: BUDGET,
        max_queue_time: BUDGET,
        reconciliation_verifier: verifier,
        ..RuntimeConfig::default()
    };
    config.cells.max_yield_time = Duration::from_secs(300);
    config.codemode.timeout = BUDGET;
    config.host.call_timeout = BUDGET;
    config.host.external = Some(Arc::new(Service {
        root: root.to_owned(),
        capture,
        fail: file(root, "window-input.json")?["outcome"] == "failed",
    }));
    Ok(Runtime::open(
        root.join("session.sqlite"),
        root.join("workspace"),
        Arc::new(Offline {
            root: root.to_owned(),
            original,
            allow_next,
            calls: AtomicU64::new(0),
        }),
        Model::new("offline-fixture", "trusted-reconciliation"),
        config,
    )?)
}
fn install_gate(root: &Path, checkpoint: &str, capture: Arc<Capture>) -> Result<()> {
    let root = root.to_owned();
    let checkpoint = checkpoint.to_owned();
    let sequence = AtomicU64::new(0);
    cayw_ai::faults::install(Arc::new(move |point, data| {
        if point == "core.start.committed" {
            *capture.0.lock().unwrap() = Some(data.clone());
        }
        let hook =
            json!({"sequence":sequence.fetch_add(1,Ordering::SeqCst)+1,"point":point,"data":data});
        append(&root.join("hooks.jsonl"), &hook).unwrap();
        if point == checkpoint {
            fixture::write_json(
                &root.join("ready.pending"),
                &json!({
                    "pid":std::process::id(),"checkpoint":checkpoint,"hook":hook,
                }),
            )
            .unwrap();
            fs::rename(root.join("ready.pending"), root.join("ready.json")).unwrap();
            fixture::sync_directory(&root).unwrap();
            loop {
                thread::park();
            }
        }
    }))
    .map_err(std::io::Error::other)?;
    Ok(())
}
struct Server {
    stop: CancellationToken,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
    client: Client,
}
impl Server {
    async fn start(runtime: Runtime) -> Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let stop = CancellationToken::new();
        let mut config = ServerConfig::new(TOKEN);
        config.request_timeout = BUDGET;
        let task = tokio::spawn(cayw_ipc::serve(
            listener,
            Arc::new(runtime),
            config,
            stop.clone(),
        ));
        let client = Client::connect_with_config(
            address,
            TOKEN,
            ClientConfig {
                request_timeout: BUDGET,
                ..ClientConfig::default()
            },
        )
        .await?;
        assert_eq!(
            client.request("runtime.health", json!({})).await?["status"],
            "ok"
        );
        Ok(Self { stop, task, client })
    }
    async fn close(self) -> Result<()> {
        self.client.close().await;
        self.stop.cancel();
        self.task.await??;
        Ok(())
    }
}
#[test]
fn reconciliation_child() -> Result<()> {
    let Some(root) = std::env::var_os("CAYW_RECONCILIATION_CHILD_ROOT") else {
        return Ok(());
    };
    let root = PathBuf::from(root);
    let input = file(&root, "window-input.json")?;
    let identity: Identity = serde_json::from_value(file(&root, "identity.json")?)?;
    let capture = Arc::new(Capture::default());
    install_gate(
        &root,
        input["checkpoint"].as_str().unwrap(),
        capture.clone(),
    )?;
    let runtime = open(&root, true, capture, Arc::new(AtomicBool::new(false)), None)?;
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()?
        .block_on(async {
            if input["mode"] == "ipc" {
                let server = Server::start(runtime).await?;
                fixture::write_json(
                    &root.join("child-ipc-authenticated.json"),
                    &server.client.request("runtime.health", json!({})).await?,
                )?;
                server
                    .client
                    .request("task.resume", json!({"task_id":identity.task_id}))
                    .await?;
                server.close().await?;
            } else {
                runtime
                    .resume(&identity.task_id, CancellationToken::new())
                    .await?;
            }
            Err::<(), _>("original worker missed the requested gate".into())
        })
}

/// One broad hang guard includes child gates and synchronous host verification.
struct KilledWorker {
    child: Arc<Mutex<Child>>,
    stop: Option<mpsc::Sender<()>>,
    watchdog: Option<thread::JoinHandle<()>>,
}
impl KilledWorker {
    fn start(root: &Path) -> Result<Self> {
        let child = Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "reconciliation_child",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("CAYW_RECONCILIATION_CHILD_ROOT", root)
            .stdin(Stdio::null())
            .stdout(File::create(root.join("child.stdout"))?)
            .stderr(File::create(root.join("child.stderr"))?)
            .spawn()?;
        let child = Arc::new(Mutex::new(child));
        let (sender, receiver) = mpsc::channel();
        let watched = child.clone();
        let root = root.to_owned();
        let watchdog = thread::spawn(move || {
            if receiver.recv_timeout(WATCHDOG) == Err(mpsc::RecvTimeoutError::Timeout) {
                let mut child = watched.lock().unwrap();
                let _ = child.kill();
                let status = child.wait().ok();
                let _ = fixture::write_json(
                    &root.join("watchdog-failure.json"),
                    &json!({
                        "failure":"reconciliation window did not finish", "signal":status.and_then(|status| status.signal()),
                    }),
                );
                std::process::abort();
            }
        });
        Ok(Self {
            child,
            stop: Some(sender),
            watchdog: Some(watchdog),
        })
    }
    fn ready(&self, root: &Path, checkpoint: &str) -> Result<Value> {
        let deadline = Instant::now() + WATCHDOG;
        loop {
            if root.join("ready.json").exists() {
                let ready = file(root, "ready.json")?;
                assert_eq!(ready["checkpoint"], checkpoint);
                assert_eq!(ready["pid"], self.child.lock().unwrap().id());
                assert!(self.child.lock().unwrap().try_wait()?.is_none());
                return Ok(ready);
            }
            if let Some(status) = self.child.lock().unwrap().try_wait()? {
                return Err(format!(
                    "child exited before gate: {status}; {}",
                    fs::read_to_string(root.join("child.stderr"))?
                )
                .into());
            }
            if Instant::now() >= deadline {
                return Err("ready watchdog expired".into());
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
    fn kill_wait(&self, root: &Path) -> Result<()> {
        let mut child = self.child.lock().unwrap();
        assert!(child.try_wait()?.is_none());
        child.kill()?;
        let status = child.wait()?;
        fixture::write_json(
            &root.join("child-exit.json"),
            &json!({"pid":child.id(),"signal":status.signal(),"code":status.code(),"display":status.to_string()}),
        )?;
        assert_eq!(status.signal(), Some(9));
        Ok(())
    }
}
impl Drop for KilledWorker {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(watchdog) = self.watchdog.take() {
            let _ = watchdog.join();
        }
        if let Ok(mut child) = self.child.lock() {
            if child.try_wait().ok().flatten().is_none() {
                let _ = child.kill();
            }
            let _ = child.wait();
        }
    }
}
async fn submit(
    runtime: &Runtime,
    client: Option<&Client>,
    identity: &Identity,
    request: CapabilityReconciliation,
    root: &Path,
    label: &str,
) -> std::result::Result<Value, String> {
    let result = match client {
        Some(client) => client
            .request(
                "capability.reconcile",
                json!({"session_id":identity.session_id,"reconciliation":request}),
            )
            .await
            .map_err(|error| format!("{}: {}", error.code, error.message)),
        None => runtime
            .reconcile_capability(&identity.session_id, request.clone())
            .map_err(|error| error.to_string())
            .and_then(|session| serde_json::to_value(session).map_err(|error| error.to_string())),
    };
    fixture::write_json(
        &root.join(format!("{label}.json")),
        &json!({"request":request,"result":result.as_ref().ok(),"error":result.as_ref().err()}),
    )
    .unwrap();
    result
}
fn request(verdict: &Verdict, actor: &str) -> CapabilityReconciliation {
    CapabilityReconciliation {
        key: verdict.call.key.clone(),
        expected: Box::new(ReconciliationOrigin::from(&verdict.call)),
        actor: actor.into(),
        evidence: verdict.evidence.clone(),
        outcome: verdict.outcome.clone(),
        external_provenance: verdict.call.external_provenance.clone(),
    }
}
fn service_files(root: &Path) -> Result<Value> {
    Ok(
        json!({"entries":fixture::read_lines(&root.join("external-entries.jsonl"))?,"effect":file(root,"effect.json")?,"receipt":file(root,"effect-receipt.json")?}),
    )
}
fn saved(runtime: &Runtime, identity: &Identity, root: &Path) -> Result<Value> {
    let session = runtime.get(&identity.session_id)?;
    let events = public_events(runtime, &identity.session_id)?;
    Ok(
        json!({"session":session,"tasks":runtime.tasks(&identity.session_id)?,"events":events,
        "databases":database_rows(root)?,"service":service_files(root)?,
        "provider_original":fixture::read_lines(&root.join("provider-original.jsonl"))?,
        "provider_next":fixture::read_lines(&root.join("provider-next.jsonl"))?,
        "provider_forbidden":fixture::read_lines(&root.join("forbidden-provider.jsonl"))?,}),
    )
}
fn public_events(runtime: &Runtime, session: &str) -> Result<Vec<EventRecord>> {
    let mut events = Vec::new();
    let mut cursor = 0;
    loop {
        let page = runtime.events(session, cursor, 11)?;
        if page.is_empty() {
            return Ok(events);
        }
        for event in page {
            assert_eq!(event.cursor, cursor + 1);
            assert_eq!(event.session_id, session);
            cursor = event.cursor;
            events.push(event);
        }
    }
}
async fn rejection(
    runtime: &Runtime,
    client: Option<&Client>,
    identity: &Identity,
    request: CapabilityReconciliation,
    root: &Path,
    label: &str,
) -> Result<()> {
    let before = saved(runtime, identity, root)?;
    let result = submit(runtime, client, identity, request, root, label).await;
    assert!(
        result.is_err(),
        "{label} unexpectedly authorized an unsupported decision"
    );
    let after = saved(runtime, identity, root)?;
    fixture::write_json(
        &root.join(format!("{label}-originals.json")),
        &json!({"before":before,"after":after}),
    )?;
    assert_eq!(before, after, "{label} changed durable facts on refusal");
    assert_eq!(
        runtime.get(&identity.session_id)?.state,
        SessionState::NeedsReconciliation
    );
    assert!(matches!(
        runtime.enqueue(&identity.session_id, "unknown cannot continue"),
        Err(RuntimeError::NeedsReconciliation)
    ));
    Ok(())
}
fn tool_results(messages: &[Message]) -> Vec<Value> {
    messages
        .iter()
        .filter_map(|message| match message {
            Message::ToolResult(result) => Some(serde_json::to_value(result).unwrap()),
            _ => None,
        })
        .collect()
}

async fn window(root: &Path, mode: &str, outcome: &str) -> Result<()> {
    fs::create_dir(root)?;
    fs::create_dir(root.join("workspace"))?;
    if outcome == "failed" {
        fs::create_dir(root.join("blocked-effect"))?;
    }
    let checkpoint = if outcome == "not_executed" {
        "core.start.committed"
    } else {
        "host.effect.durable"
    };
    fixture::write_json(
        &root.join("window-input.json"),
        &json!({"mode":mode,"outcome":outcome,"checkpoint":checkpoint,"script":SCRIPT}),
    )?;
    let actor = if mode == "ipc" { "rpc-user" } else { SDK_ACTOR };
    let verifier = Arc::new(ReceiptVerifier {
        root: root.to_owned(),
        allowed_actor: actor.into(),
    });
    let allowed = Arc::new(AtomicBool::new(false));
    let observer = open(
        root,
        false,
        Arc::new(Capture::default()),
        allowed.clone(),
        Some(verifier.clone()),
    )?;
    let session = observer.create(Some(format!("kill then verify {mode} {outcome}")))?;
    let task = observer.enqueue(&session.id, "original offline service task")?;
    let identity = Identity {
        session_id: session.id,
        task_id: task.id,
    };
    fixture::write_json(&root.join("identity.json"), &identity)?;
    let child = KilledWorker::start(root)?;
    child.ready(root, checkpoint)?;
    let (_, call, _) = original(root)?;
    let live_input = CapabilityReconciliation {
        key: call.key.clone(),
        expected: Box::new(ReconciliationOrigin::from(&call)),
        actor: actor.into(),
        evidence: "cannot verify while original worker is alive".into(),
        outcome: CapabilityOutcome::Completed {
            result: json!({"must":"not authorize"}),
        },
        external_provenance: call.external_provenance.clone(),
    };
    let live_before = saved(&observer, &identity, root)?;
    let live_server = if mode == "ipc" {
        Some(Server::start(observer.clone()).await?)
    } else {
        None
    };
    let live_result = submit(
        &observer,
        live_server.as_ref().map(|server| &server.client),
        &identity,
        live_input,
        root,
        "live-reconciliation",
    )
    .await;
    assert!(
        live_result
            .as_ref()
            .is_err_and(|error| error.contains("busy") || error.contains("queued or running"))
    );
    assert_eq!(
        live_before,
        saved(&observer, &identity, root)?,
        "live rejection changed facts"
    );
    fixture::write_json(&root.join("live-originals.json"), &live_before)?;
    if let Some(server) = live_server {
        server.close().await?;
    }
    child.kill_wait(root)?;
    drop(observer);
    let untrusted = open(
        root,
        false,
        Arc::new(Capture::default()),
        allowed.clone(),
        None,
    )?;
    let before_open = saved(&untrusted, &identity, root)?;
    assert_eq!(
        live_before, before_open,
        "opening implicitly recovered original worker"
    );
    let session = untrusted.recover(&identity.session_id)?;
    assert_eq!(session.state, SessionState::NeedsReconciliation);
    let recovered = saved(&untrusted, &identity, root)?;
    fixture::write_json(&root.join("unknown-originals.json"), &recovered)?;
    let verdict = derive_verdict(root)?;
    assert!(matches!(
        (outcome, &verdict.outcome),
        ("completed", CapabilityOutcome::Completed { .. })
            | ("failed", CapabilityOutcome::Failed { .. })
            | ("not_executed", CapabilityOutcome::NotExecuted { .. })
    ));
    let submitted_actor = if mode == "ipc" {
        "forged-owner-from-ipc-payload"
    } else {
        actor
    };
    let valid = request(&verdict, submitted_actor);
    let default_server = if mode == "ipc" {
        Some(Server::start(untrusted.clone()).await?)
    } else {
        None
    };
    rejection(
        &untrusted,
        default_server.as_ref().map(|server| &server.client),
        &identity,
        valid.clone(),
        root,
        "default-no-verifier",
    )
    .await?;
    if let Some(server) = default_server {
        server.close().await?;
    }
    drop(untrusted);
    let runtime = open(
        root,
        false,
        Arc::new(Capture::default()),
        allowed.clone(),
        Some(verifier),
    )?;
    let server = if mode == "ipc" {
        Some(Server::start(runtime.clone()).await?)
    } else {
        None
    };
    let client = server.as_ref().map(|server| &server.client);
    let mut insufficient = valid.clone();
    insufficient.evidence = root
        .join("missing-independent-receipt.json")
        .to_string_lossy()
        .into_owned();
    rejection(
        &runtime,
        client,
        &identity,
        insufficient,
        root,
        "insufficient-reference",
    )
    .await?;
    if outcome != "not_executed" {
        let raw = fs::read(root.join("effect-receipt.json"))?;
        fs::rename(
            root.join("effect-receipt.json"),
            root.join("retained-original-receipt.json"),
        )?;
        let missing = rejection(
            &runtime,
            client,
            &identity,
            valid.clone(),
            root,
            "missing-actual-receipt",
        )
        .await;
        fs::rename(
            root.join("retained-original-receipt.json"),
            root.join("effect-receipt.json"),
        )?;
        assert_eq!(raw, fs::read(root.join("effect-receipt.json"))?);
        missing?;
    }
    let mut contradiction = valid.clone();
    contradiction.outcome = match verdict.outcome {
        CapabilityOutcome::Completed { .. } => CapabilityOutcome::Failed {
            error: "contradicts the successful service effect".into(),
        },
        _ => CapabilityOutcome::Completed {
            result: json!({"invented":"success"}),
        },
    };
    rejection(
        &runtime,
        client,
        &identity,
        contradiction,
        root,
        "contradictory-outcome",
    )
    .await?;
    let mut mutations = Vec::new();
    for field in [
        "session_id",
        "attempt_id",
        "worker_id",
        "fence",
        "task_id",
        "execution_id",
        "parent_call_id",
        "source_hash",
    ] {
        let mut changed = serde_json::to_value(&valid)?;
        let pointer = if ["session_id", "attempt_id", "worker_id", "fence"].contains(&field) {
            format!("/expected/lease/{field}")
        } else {
            format!("/expected/{field}")
        };
        let value = changed.pointer_mut(&pointer).unwrap();
        *value = if field == "fence" {
            json!(value.as_u64().unwrap() + 1)
        } else {
            json!(format!("changed-{field}"))
        };
        mutations.push((
            field,
            serde_json::from_value::<CapabilityReconciliation>(changed)?,
        ));
    }
    for (field, changed) in mutations {
        rejection(
            &runtime,
            client,
            &identity,
            changed,
            root,
            &format!("changed-{field}"),
        )
        .await?;
    }
    if mode == "sdk" {
        let mut unauthorized = valid.clone();
        unauthorized.actor = "untrusted-self-report".into();
        rejection(
            &runtime,
            client,
            &identity,
            unauthorized,
            root,
            "unauthorized-actor",
        )
        .await?;
    }
    let original_messages = runtime.get(&identity.session_id)?.messages;
    let original_tool_results = tool_results(&original_messages);
    assert!(!original_tool_results.is_empty());
    let event_prefix = public_events(&runtime, &identity.session_id)?;
    let service_prefix = service_files(root)?;
    let provider_prefix = fixture::read_lines(&root.join("provider-original.jsonl"))?;
    let success = submit(
        &runtime,
        client,
        &identity,
        valid.clone(),
        root,
        "accepted-reconciliation",
    )
    .await;
    assert!(
        success.is_ok(),
        "independently verified decision rejected: {success:?}"
    );
    let clean = runtime.get(&identity.session_id)?;
    assert_eq!(clean.state, SessionState::Idle);
    assert!(clean.unresolved_capabilities.is_empty());
    assert_eq!(
        clean.messages, original_messages,
        "trusted decision rewrote original ToolResult history"
    );
    let (_, events, cell_records) = records(root)?;
    assert!(events.starts_with(&event_prefix));
    let decision = events
        .iter()
        .find_map(|event| match &event.event {
            CoreEvent::CapabilityReconciled {
                reconciliation,
                call,
            } => Some((reconciliation, call)),
            _ => None,
        })
        .expect("accepted decision has no original event");
    assert_eq!(
        decision.0.actor, actor,
        "IPC must replace caller's claimed actor"
    );
    assert_eq!(
        decision.0.expected.as_ref(),
        &ReconciliationOrigin::from(&verdict.call)
    );
    assert_eq!(decision.1.outcome.as_ref(), Some(&verdict.outcome));
    let intent = &cell_records[0].calls[0];
    match &verdict.outcome {
        CapabilityOutcome::Completed { result } => {
            assert_eq!(intent.status, IntentStatus::Succeeded);
            assert_eq!(intent.result.as_ref(), Some(result));
        }
        CapabilityOutcome::Failed { error } => {
            assert_eq!(intent.status, IntentStatus::Failed);
            assert_eq!(intent.error.as_ref(), Some(error));
        }
        CapabilityOutcome::NotExecuted { reason } => {
            assert_eq!(intent.status, IntentStatus::NotExecuted);
            assert_eq!(intent.error.as_ref(), Some(reason));
        }
        _ => unreachable!(),
    }
    let accepted = saved(&runtime, &identity, root)?;
    fixture::write_json(&root.join("accepted-originals.json"), &accepted)?;
    assert!(
        submit(
            &runtime,
            client,
            &identity,
            valid.clone(),
            root,
            "exact-retry"
        )
        .await
        .is_ok()
    );
    assert_eq!(
        accepted,
        saved(&runtime, &identity, root)?,
        "exact retry changed durable facts"
    );
    let mut later_conflict = valid.clone();
    later_conflict.outcome = CapabilityOutcome::Completed {
        result: json!({"invented":"later replacement"}),
    };
    let before_conflict = saved(&runtime, &identity, root)?;
    assert!(
        submit(
            &runtime,
            client,
            &identity,
            later_conflict,
            root,
            "conflict-after-settlement"
        )
        .await
        .is_err()
    );
    assert_eq!(before_conflict, saved(&runtime, &identity, root)?);
    let old_task = runtime.task(&identity.task_id)?;
    assert!(
        runtime
            .resume(&identity.task_id, CancellationToken::new())
            .await
            .is_err()
    );
    allowed.store(true, Ordering::SeqCst);
    let next = runtime.enqueue(&identity.session_id, NEXT_PROMPT)?;
    assert_ne!(next.id, identity.task_id);
    fixture::write_json(&root.join("explicit-next-task.json"), &next)?;
    if let Some(client) = client {
        client
            .request("task.resume", json!({"task_id":next.id}))
            .await?;
    } else {
        runtime.resume(&next.id, CancellationToken::new()).await?;
    }
    let finished = runtime.get(&identity.session_id)?;
    assert_eq!(finished.state, SessionState::Idle);
    assert_eq!(
        runtime.task(&identity.task_id)?,
        old_task,
        "new task changed original task identity or terminal"
    );
    assert!(finished.messages.starts_with(&original_messages));
    assert!(tool_results(&finished.messages).starts_with(&original_tool_results));
    assert!(public_events(&runtime, &identity.session_id)?.starts_with(&events));
    assert!(finished.messages.iter().any(|message|matches!(message,Message::Assistant(message) if cayw_ai::content_text(&message.content)==NEXT_REPLY)));
    assert_eq!(
        service_prefix,
        service_files(root)?,
        "new task replayed the original external service"
    );
    assert_eq!(
        provider_prefix,
        fixture::read_lines(&root.join("provider-original.jsonl"))?
    );
    assert_eq!(
        fixture::read_lines(&root.join("provider-next.jsonl"))?.len(),
        1
    );
    assert!(fixture::read_lines(&root.join("forbidden-provider.jsonl"))?.is_empty());
    let observations = fixture::read_lines(&root.join("verifier-observations.jsonl"))?;
    assert!(
        observations
            .iter()
            .any(|observation| observation["accepted"] == true)
    );
    assert!(
        observations
            .iter()
            .any(|observation| observation["accepted"] == false)
    );
    if mode == "ipc" {
        assert!(
            observations
                .iter()
                .all(|observation| observation["actor"] == "rpc-user")
        );
    }
    let final_originals = saved(&runtime, &identity, root)?;
    fixture::write_json(&root.join("continued-originals.json"), &final_originals)?;
    if let Some(server) = server {
        server.close().await?;
    }
    drop(runtime);
    let reopened = open(root, false, Arc::new(Capture::default()), allowed, None)?;
    assert_eq!(
        final_originals,
        saved(&reopened, &identity, root)?,
        "reopen rewrote trusted decision or new task"
    );
    fixture::write_json(
        &root.join("second-reopen-originals.json"),
        &saved(&reopened, &identity, root)?,
    )?;
    eprintln!(
        "CRASH_RECONCILIATION_VERIFIED mode={mode} outcome={outcome} originals={}",
        root.display()
    );
    Ok(())
}
async fn scenarios(mode: &str) -> Result<()> {
    let base = std::env::var_os("CAYW_CRASH_RECONCILIATION_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("cayw-crash-reconciliation"));
    fs::create_dir_all(&base)?;
    let root = base.join(format!("{mode}-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&root)?;
    fixture::write_json(
        &root.join("run-input.json"),
        &json!({"mode":mode,"outcomes":["completed","failed","not_executed"],"os":std::env::consts::OS,"architecture":std::env::consts::ARCH,"real_model_requests":0}),
    )?;
    eprintln!("CRASH_RECONCILIATION_EVIDENCE={}", root.display());
    let mut failures = Vec::new();
    for outcome in ["completed", "failed", "not_executed"] {
        let directory = root.join(outcome);
        let result = std::panic::AssertUnwindSafe(window(&directory, mode, outcome))
            .catch_unwind()
            .await;
        let failure = match result {
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
                .unwrap_or_else(|| "reconciliation assertion failed".into()),
        };
        fixture::write_json(
            &directory.join("window-failure.json"),
            &json!({"failure":failure}),
        )?;
        failures.push(json!({"outcome":outcome,"failure":failure,"originals":directory}));
    }
    if !failures.is_empty() {
        fixture::write_json(&root.join("failed-windows.json"), &failures)?;
        return Err(format!(
            "{} reconciliation windows failed; originals {}",
            failures.len(),
            root.display()
        )
        .into());
    }
    Ok(())
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sdk_reconciles_real_receipts_and_continues_same_session_without_replay() -> Result<()> {
    scenarios("sdk").await
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn authenticated_ipc_reconciles_real_receipts_and_continues_same_session_without_replay()
-> Result<()> {
    scenarios("ipc").await
}
