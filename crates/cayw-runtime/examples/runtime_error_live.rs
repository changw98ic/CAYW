//! Opt-in real-provider evidence through the public SDK and authenticated IPC.
//! Run only with explicit model authorization; each execution has a fresh database.
#[path = "runtime_error_live/writes.rs"]
mod writes;

use cayw_ai::{AssistantStream, CancellationToken, ModelRequest, Provider};
use cayw_ipc::{Client, ClientConfig, ServerConfig};
use cayw_runtime::{HeadlessConfig, Runtime, RuntimeError, TaskInput};
use serde_json::{Value, json};
use std::{
    error::Error,
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

type Result<T = ()> = std::result::Result<T, Box<dyn Error>>;

fn argument(args: &[String], name: &str) -> Result<String> {
    let index = args
        .iter()
        .position(|arg| arg == name)
        .ok_or_else(|| format!("{name} is required"))?;
    args.get(index + 1)
        .cloned()
        .ok_or_else(|| format!("{name} needs a value").into())
}
fn save(path: &Path, value: &impl serde::Serialize) -> Result {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

#[derive(Debug, serde::Serialize)]
struct Failure {
    code: String,
    message: String,
    outcome_unknown: bool,
    sources: Vec<String>,
}
impl From<RuntimeError> for Failure {
    fn from(error: RuntimeError) -> Self {
        let mut sources = Vec::new();
        let mut source = error.source();
        while let Some(cause) = source {
            sources.push(cause.to_string());
            source = cause.source();
        }
        Self {
            code: error.code().into(),
            message: error.to_string(),
            outcome_unknown: error.outcome_unknown(),
            sources,
        }
    }
}
impl From<cayw_ipc::ClientError> for Failure {
    fn from(error: cayw_ipc::ClientError) -> Self {
        Self {
            code: error.code,
            message: error.message,
            outcome_unknown: error.outcome_unknown,
            sources: vec![],
        }
    }
}
impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}
impl Error for Failure {}

struct CountedProvider {
    actual: Arc<dyn Provider>,
    calls: AtomicUsize,
}
impl Provider for CountedProvider {
    fn stream(&self, request: ModelRequest, cancel: CancellationToken) -> AssistantStream {
        let before = self.calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(
            before, 0,
            "probe must never dispatch a second model request"
        );
        self.actual.stream(request, cancel)
    }
}

struct Access {
    runtime: Runtime,
    client: Option<Client>,
    stop: CancellationToken,
    server: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
}
impl Access {
    async fn new(runtime: Runtime, mode: &str) -> Result<Self> {
        let stop = CancellationToken::new();
        if mode == "sdk" {
            return Ok(Self {
                runtime,
                client: None,
                stop,
                server: None,
            });
        }
        assert_eq!(mode, "ipc");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let token = format!("live-probe-{}", uuid::Uuid::new_v4());
        let mut config = ServerConfig::new(token.clone());
        config.request_timeout = Duration::from_secs(240);
        let handler = Arc::new(runtime.clone());
        let shutdown = stop.clone();
        let server = tokio::spawn(cayw_ipc::serve(listener, handler, config, shutdown));
        let client = Client::connect_with_config(
            address,
            token,
            ClientConfig {
                request_timeout: Duration::from_secs(240),
                ..ClientConfig::default()
            },
        )
        .await?;
        Ok(Self {
            runtime,
            client: Some(client),
            stop,
            server: Some(server),
        })
    }
    async fn call(&self, method: &str, params: Value) -> std::result::Result<Value, Failure> {
        if let Some(client) = &self.client {
            return client.request(method, params).await.map_err(Into::into);
        }
        let session = params["session_id"].as_str().unwrap_or_default();
        let task = params["task_id"].as_str().unwrap_or_default();
        let result = match method {
            "session.create" => self
                .runtime
                .create(Some("real model runtime error validation".into()))
                .map(|value| json!(value)),
            "session.get" => self.runtime.get(session).map(|value| json!(value)),
            "session.tasks" => self.runtime.tasks(session).map(|value| json!(value)),
            "task.get" => self.runtime.task(task).map(|value| json!(value)),
            "task.enqueue" => self
                .runtime
                .enqueue_input(
                    session,
                    TaskInput {
                        text: params["text"].as_str().unwrap().into(),
                        images: vec![],
                    },
                )
                .map(|value| json!(value)),
            "task.resume" => self
                .runtime
                .resume(task, CancellationToken::new())
                .await
                .map(|value| json!(value)),
            "session.fork" => self
                .runtime
                .fork(session, params["label"].as_str().unwrap(), true)
                .map(|value| json!(value)),
            "runtime.health" => self.runtime.health().map(|value| json!(value)),
            _ => panic!("unexpected probe method {method}"),
        };
        result.map_err(Into::into)
    }
    async fn close(self) -> Result {
        if let Some(client) = self.client {
            client.close().await;
        }
        self.stop.cancel();
        if let Some(server) = self.server {
            tokio::time::timeout(Duration::from_secs(10), server).await???;
        }
        Ok(())
    }
}

fn events(runtime: &Runtime, session: &str) -> Result<Vec<cayw_core::EventRecord>> {
    let mut rows = Vec::new();
    loop {
        let after = rows
            .last()
            .map_or(0, |row: &cayw_core::EventRecord| row.cursor);
        let page = runtime.events(session, after, 1000)?;
        if page.is_empty() {
            break;
        }
        for row in page {
            assert_eq!(row.cursor, rows.len() as u64 + 1);
            rows.push(row);
        }
    }
    assert_eq!(rows.len() as u64, runtime.get(session)?.cursor);
    Ok(rows)
}
fn assert_assistant_reply(view: &Value, expected: &str) {
    let assistant = view["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|message| message["role"] == "assistant")
        .expect("a real assistant reply");
    assert!(
        serde_json::to_string(&assistant["content"])
            .unwrap()
            .contains(expected)
    );
    assert_eq!(assistant["usage_status"], "reported");
    assert!(assistant["response_id"].is_string() && assistant["response_model"].is_string());
}

fn completed_ledger(runtime: &Runtime, session: &str) -> Result<Value> {
    let ledger = runtime.model_requests(session)?;
    assert!(!ledger.budget_unknown);
    assert_eq!(ledger.reserved_tokens, 0);
    assert_eq!(ledger.requests.len(), 1);
    let request = &ledger.requests[0];
    assert_eq!(
        request.terminal,
        Some(cayw_runtime::ModelRequestTerminal::Completed)
    );
    assert!(request.settled_tokens.is_some_and(|tokens| tokens > 0));
    assert!(request.started_cursor.is_some() && request.terminal_cursor.is_some());
    let final_report = request
        .observations
        .last()
        .expect("final real usage report");
    assert_eq!(
        final_report.observation.usage_status,
        cayw_ai::UsageStatus::Reported
    );
    assert_eq!(
        request.settled_tokens,
        Some(final_report.observation.usage.total_tokens)
    );
    assert_eq!(request.outstanding_reservation_tokens, 0);
    assert_eq!(ledger.settled_tokens, request.settled_tokens.unwrap());
    Ok(json!(ledger))
}

async fn storage(access: &Access, database: &Path, provider: &CountedProvider) -> Result<Value> {
    let created = access.call("session.create", json!({})).await?;
    let session = created["id"].as_str().unwrap();
    let task = access.call("task.enqueue", json!({"session_id":session,"text":"Do not call any tools. Reply exactly CAYW_REAL_STORAGE_BASELINE_OK."})).await?;
    assert_eq!(task["state"], "queued");
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    let view = access
        .call("task.resume", json!({"task_id":task["id"]}))
        .await?;
    let baseline_task = access
        .call("task.get", json!({"task_id":task["id"]}))
        .await?;
    assert_eq!(baseline_task["state"], "completed");
    assert_assistant_reply(&view, "CAYW_REAL_STORAGE_BASELINE_OK");
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    let ledger = completed_ledger(&access.runtime, session)?;
    let baseline_events = events(&access.runtime, session)?;
    let mut children = Vec::new();
    for label in [
        "input-json-corrupt",
        "intent-corrupt",
        "write-failure",
        "write-busy",
    ] {
        children.push(
            access
                .call(
                    "session.fork",
                    json!({"session_id":session,"label":label,"confirm":true,"mode":"chat"}),
                )
                .await?,
        );
    }
    let connection = rusqlite::Connection::open(database)?;
    let mut corruptions = Vec::new();
    for (index, column) in ["input_json", "intent"].into_iter().enumerate() {
        let id = children[index]["id"].as_str().unwrap();
        let before = access.runtime.get(id)?;
        let prior_events = events(&access.runtime, id)?;
        let queued = access
            .call(
                "task.enqueue",
                json!({"session_id":id,"text":"This queued input must never be executed."}),
            )
            .await?;
        let task_id = queued["id"].as_str().unwrap();
        assert_eq!(
            connection.execute(
                &format!("UPDATE runtime_tasks SET {column}=?1 WHERE id=?2"),
                rusqlite::params!["{", task_id]
            )?,
            1
        );
        let mut errors = Vec::new();
        for method in ["task.get", "session.tasks", "task.resume"] {
            let params = if method == "session.tasks" {
                json!({"session_id":id})
            } else {
                json!({"task_id":task_id})
            };
            let error = access.call(method, params).await.unwrap_err();
            assert_eq!(error.code, "storage_corrupt", "{error}");
            assert!(!error.outcome_unknown);
            assert!(error.message.contains("EOF while parsing an object"));
            if access.client.is_some() {
                assert!(error.message.contains(&format!("method={method}")));
            }
            errors.push(json!({"method":method,"error":error}));
        }
        let row: (String, String, i64, i64, Option<String>, Option<String>) = connection.query_row(&format!("SELECT {column},state,fence,holds_slot,worker,attempt FROM runtime_tasks WHERE id=?1"), [task_id], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?)))?;
        assert_eq!(row.0, "{");
        assert_eq!(row.1, "queued");
        assert_eq!(row.2, 0);
        assert_eq!(row.3, 0);
        assert!(row.4.is_none() && row.5.is_none());
        assert_eq!(access.runtime.get(id)?, before);
        assert_eq!(events(&access.runtime, id)?, prior_events);
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        corruptions.push(json!({"column":column,"session_id":id,"original_task":queued,"errors":errors,"persisted_row":row,"session_unchanged":true,"events_unchanged":true,"additional_provider_calls":0}));
    }
    let mut writes = Vec::new();
    for (index, kind) in ["constraint", "busy"].into_iter().enumerate() {
        let id = children[index + 2]["id"].as_str().unwrap();
        let before = access.runtime.get(id)?;
        let prior_events = events(&access.runtime, id)?;
        if kind == "constraint" {
            connection.execute_batch("CREATE TRIGGER live_probe_fail_insert BEFORE INSERT ON runtime_tasks BEGIN SELECT RAISE(FAIL, 'live validation forced queue write failure'); END;")?;
        } else {
            connection.execute_batch("BEGIN IMMEDIATE")?;
        }
        let error = access.call("task.enqueue", json!({"session_id":id,"text":"Storage write must reject before any model dispatch."})).await.unwrap_err();
        if kind == "constraint" {
            assert_eq!(error.code, "storage_error");
            assert!(
                error
                    .message
                    .contains("live validation forced queue write failure")
            );
            connection.execute_batch("DROP TRIGGER live_probe_fail_insert")?;
        } else {
            assert_eq!(error.code, "storage_busy");
            assert!(error.message.contains("locked"));
            connection.execute_batch("ROLLBACK")?;
        }
        assert!(!error.outcome_unknown);
        assert_eq!(access.runtime.get(id)?, before);
        assert_eq!(events(&access.runtime, id)?, prior_events);
        let count: i64 = connection.query_row(
            "SELECT count(*) FROM runtime_tasks WHERE session=?1",
            [id],
            |row| row.get(0),
        )?;
        assert_eq!(count, 0);
        assert!(access.runtime.tasks(id)?.is_empty());
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        writes.push(json!({"kind":kind,"session_id":id,"error":error,"task_rows":count,"session_unchanged":true,"events_unchanged":true,"additional_provider_calls":0}));
    }
    assert_eq!(
        access.runtime.get(session)?,
        serde_json::from_value(view.clone())?
    );
    assert_eq!(events(&access.runtime, session)?, baseline_events);
    assert_eq!(completed_ledger(&access.runtime, session)?, ledger);
    Ok(
        json!({"status":"ok","case":"storage","baseline_session":view,"baseline_task":baseline_task,"model_requests":ledger,"baseline_events":baseline_events,"corruptions":corruptions,"write_failures":writes,"actual_provider_calls":1,"unexpected_provider_calls":0,"mutated_records_retained":true}),
    )
}

async fn panic_case(
    access: &Access,
    provider: &Arc<CountedProvider>,
    database: &Path,
    workspace: &Path,
    config: &Path,
    mode: &str,
) -> Result<Value> {
    const REASON: &str = "live worker cleanup panic: 缓存\u{1b}[31m\nsecond line\u{202e}";
    let panicked = Arc::new(AtomicBool::new(false));
    let observer = panicked.clone();
    cayw_ai::faults::install(Arc::new(move |point, _| {
        if point == "core.attempt_terminal.committed" {
            assert!(
                !observer.swap(true, Ordering::SeqCst),
                "duplicate worker execution"
            );
            std::panic::panic_any(REASON.to_owned());
        }
    }))
    .map_err(std::io::Error::other)?;
    let created = access.call("session.create", json!({})).await?;
    let session = created["id"].as_str().unwrap();
    let queued = access.call("task.enqueue",json!({"session_id":session,"text":"Do not call tools. Reply exactly CAYW_REAL_WORKER_PANIC_OK."})).await?;
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    let error = access
        .call("task.resume", json!({"task_id":queued["id"]}))
        .await
        .unwrap_err();
    assert!(panicked.load(Ordering::SeqCst));
    assert_eq!(error.code, "worker_panicked_outcome_unknown");
    assert!(error.outcome_unknown);
    assert!(error.message.contains(&REASON.escape_debug().to_string()));
    assert!(!error.message.chars().any(char::is_control));
    assert!(!error.message.contains('\u{202e}'));
    let task = access
        .call("task.get", json!({"task_id":queued["id"]}))
        .await?;
    // The existing committed checkpoint still holds Core's connection lock.
    // Preserve that poisoned-connection error, then inspect committed state
    // through a new Runtime instead of clearing the poison or replaying work.
    let poisoned_read = access
        .call("session.get", json!({"session_id":session}))
        .await
        .unwrap_err();
    assert_eq!(poisoned_read.code, "session_error");
    assert!(poisoned_read.message.contains("mutex is poisoned"));
    let poisoned_rejection = access
        .call(
            "task.enqueue",
            json!({"session_id":session,"text":"Reject on the original poisoned connection."}),
        )
        .await
        .unwrap_err();
    assert_eq!(poisoned_rejection.code, "session_error");
    let health = access.call("runtime.health", json!({})).await?;
    assert_eq!(health["running"], 1);
    let loaded = HeadlessConfig::load(config)?;
    let model = loaded
        .model()
        .ok_or("explicit real model required")?
        .build()?
        .model;
    let reopened = Access::new(
        Runtime::open(
            database,
            workspace,
            provider.clone(),
            model,
            loaded.into_runtime_config()?,
        )?,
        mode,
    )
    .await?;
    let access = &reopened;
    let view = access
        .call("session.get", json!({"session_id":session}))
        .await?;
    assert_eq!(
        access
            .call("task.get", json!({"task_id":queued["id"]}))
            .await?,
        task
    );
    assert_assistant_reply(&view, "CAYW_REAL_WORKER_PANIC_OK");
    assert_eq!(task["state"], "draining");
    assert_eq!(task["holds_slot"], true);
    assert!(task["worker_id"].is_string() && task["attempt_id"].is_string());
    assert!(task["fence"].as_u64().unwrap() > 0);
    // RPC adds operation context to the returned error; the stored worker cause stays original.
    assert!(
        task["error"]
            .as_str()
            .unwrap()
            .contains(&REASON.escape_debug().to_string())
    );
    let health = access.call("runtime.health", json!({})).await?;
    assert_eq!(health["running"], 1);
    let ledger = completed_ledger(&access.runtime, session)?;
    let before_events = events(&access.runtime, session)?;
    let rejection = access
        .call(
            "task.enqueue",
            json!({"session_id":session,"text":"Reject new work. Never replay the original task."}),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(
            rejection.code.as_str(),
            "busy" | "needs_reconciliation_outcome_unknown"
        ),
        "{rejection}"
    );
    assert_eq!(
        rejection.outcome_unknown,
        rejection.code.ends_with("_outcome_unknown")
    );
    let same_task_resume = access
        .call("task.resume", json!({"task_id":queued["id"]}))
        .await
        .unwrap_err();
    assert_eq!(same_task_resume.code, "invalid_params");
    assert_eq!(
        access
            .call("task.get", json!({"task_id":queued["id"]}))
            .await?,
        task
    );
    assert_eq!(access.runtime.tasks(session)?.len(), 1);
    assert_eq!(events(&access.runtime, session)?, before_events);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    let evidence = json!({"status":"ok","case":"panic","session":view,"task":task,"error":error,"rejection":rejection,"same_task_resume":same_task_resume,"health":health,"model_requests":ledger,"events":before_events,"actual_provider_calls":1,"unexpected_provider_calls":0,"panic_checkpoint":"core.attempt_terminal.committed","cleanup_hold_retained":true,"original_connection_read_error":poisoned_read,"original_connection_enqueue_error":poisoned_rejection,"durable_state_read_from_fresh_runtime":true});
    reopened.close().await?;
    Ok(evidence)
}

#[tokio::main]
async fn main() -> Result {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let case = argument(&args, "--case")?;
    let database = PathBuf::from(argument(&args, "--db")?);
    let out = PathBuf::from(argument(&args, "--out")?);
    if case == "writes" {
        writes::verify(&database, &argument(&args, "--session")?, &out)?;
        println!(
            "{}",
            json!({"status":"ok","case":"writes","out":out,"actual_provider_calls":0})
        );
        return Ok(());
    }
    let workspace = PathBuf::from(argument(&args, "--workspace")?);
    let config = HeadlessConfig::load(argument(&args, "--config")?)?;
    if case == "inspect" {
        let runtime = Runtime::open(
            &database,
            &workspace,
            Arc::new(cayw_runtime::fixture::OfflineProvider::default()),
            cayw_ai::Model::new("inspection", "no-network"),
            config.into_runtime_config()?,
        )?;
        let id = argument(&args, "--session")?;
        let evidence = json!({"status":"ok","case":"inspect","session":runtime.get(&id)?,"tasks":runtime.tasks(&id)?,"model_requests":runtime.model_requests(&id)?,"events":events(&runtime,&id)?,"actual_provider_calls":0});
        save(&out, &evidence)?;
        println!(
            "{}",
            json!({"status":"ok","case":"inspect","out":out,"actual_provider_calls":0})
        );
        return Ok(());
    }
    assert!(
        !database.exists(),
        "live fault scenarios require a fresh database"
    );
    assert!(matches!(case.as_str(), "storage" | "panic"));
    let configured = config
        .model()
        .ok_or("explicit real model required")?
        .build()?;
    let provider = Arc::new(CountedProvider {
        actual: configured.provider,
        calls: AtomicUsize::new(0),
    });
    let runtime = Runtime::open(
        &database,
        &workspace,
        provider.clone(),
        configured.model,
        config.into_runtime_config()?,
    )?;
    let mode = argument(&args, "--mode")?;
    let access = Access::new(runtime, &mode).await?;
    let outcome = if case == "storage" {
        storage(&access, &database, &provider).await
    } else {
        panic_case(
            &access,
            &provider,
            &database,
            &workspace,
            &PathBuf::from(argument(&args, "--config")?),
            &mode,
        )
        .await
    };
    // Keep failure artifacts and the original database even when an assertion fails.
    let evidence = outcome?;
    save(&out, &evidence)?;
    access.close().await?;
    println!(
        "{}",
        json!({"status":"ok","case":case,"mode":mode,"out":out,"actual_provider_calls":provider.calls.load(Ordering::SeqCst),"unexpected_provider_calls":0})
    );
    Ok(())
}
