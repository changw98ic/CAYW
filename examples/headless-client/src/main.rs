mod api;
mod cursor;
mod options;

use api::{Api, Connections};
use cayw_runtime::{RuntimeError, SessionView, TaskState};
use cursor::Cursor;
use options::Options;
use serde_json::{Value, json};
use std::io::Write;

type Result<T> = std::result::Result<T, Failure>;
struct Failure {
    code: String,
    message: String,
    outcome_unknown: bool,
}
impl Failure {
    fn invalid(message: impl Into<String>) -> Self {
        Self {
            code: "invalid_input".into(),
            message: message.into(),
            outcome_unknown: false,
        }
    }
    fn record(&self, command: &str, session: Option<&str>) -> Value {
        json!({"type":"error","command":command,"session_id":session,"code":self.code,"message":self.message,"outcome_unknown":self.outcome_unknown})
    }
}
impl From<std::io::Error> for Failure {
    fn from(error: std::io::Error) -> Self {
        Self {
            code: "io_error".into(),
            message: error.to_string(),
            outcome_unknown: false,
        }
    }
}
impl From<serde_json::Error> for Failure {
    fn from(error: serde_json::Error) -> Self {
        Self {
            code: "invalid_json".into(),
            message: error.to_string(),
            outcome_unknown: false,
        }
    }
}
impl From<RuntimeError> for Failure {
    fn from(error: RuntimeError) -> Self {
        let code = match &error {
            RuntimeError::Cancelled => "cancelled",
            RuntimeError::NeedsReconciliation => "needs_reconciliation",
            RuntimeError::Busy => "busy",
            RuntimeError::QueueFull => "queue_full",
            RuntimeError::Budget(_) => "budget_exceeded",
            RuntimeError::StaleApproval => "stale_approval",
            RuntimeError::StaleCancellation => "stale_cancellation",
            RuntimeError::RecoveryRequired { .. } => "recovery_required",
            _ => "runtime_error",
        };
        let unknown = matches!(
            &error,
            RuntimeError::NeedsReconciliation
                | RuntimeError::RecoveryRequired {
                    outcome_unknown: true,
                    ..
                }
        );
        Self {
            code: code.into(),
            message: error.to_string(),
            outcome_unknown: unknown,
        }
    }
}
impl From<cayw_ipc::ClientError> for Failure {
    fn from(error: cayw_ipc::ClientError) -> Self {
        let code = match error.code.as_str() {
            "needs_reconciliation_outcome_unknown" => "needs_reconciliation".into(),
            _ => error.code,
        };
        Self {
            code,
            message: error.message,
            outcome_unknown: error.outcome_unknown,
        }
    }
}
fn emit(value: &Value) -> Result<()> {
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    serde_json::to_writer(&mut output, value)?;
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(())
}
fn result(command: &str, session: Option<&str>, value: impl serde::Serialize) -> Result<()> {
    emit(&json!({"type":"result","command":command,"session_id":session,"result":value}))
}
#[tokio::main]
async fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args == ["--help"] || args == ["-h"] {
        print!("{}", options::USAGE);
        return;
    }
    let session = args
        .windows(2)
        .find(|pair| pair[0] == "--session")
        .map(|pair| pair[1].clone());
    let options = match Options::parse(args) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("{}", error.record("parse", session.as_deref()));
            std::process::exit(1);
        }
    };
    let outcome = run(&options).await;
    if let Err(error) = outcome {
        eprintln!(
            "{}",
            error.record(&options.command, options.session.as_deref())
        );
        std::process::exit(1);
    }
}
async fn run(options: &Options) -> Result<()> {
    let needs_model = options.command == "prompt";
    let connections = Connections::open(&options.connection, needs_model).await?;
    let outcome = async {
        emit(&connections.metadata)?;
        let id = options.session.as_deref().unwrap_or_default();
        match options.command.as_str() {
            "create" => {
                let view = connections.execute.create(options.label.clone()).await?;
                result("create", Some(&view.id), &view)
            }
            "get" => result("get", Some(id), connections.execute.get(id).await?),
            "tasks" => result("tasks", Some(id), connections.execute.tasks(id).await?),
            "audit" => result(
                "audit",
                Some(id),
                connections
                    .execute
                    .audit(id, options.after.unwrap_or(0), options.limit)
                    .await?,
            ),
            "approve" => result(
                "approve",
                Some(id),
                connections
                    .execute
                    .approve(
                        id,
                        options.approval.as_deref().unwrap(),
                        options.decision.as_deref().unwrap(),
                    )
                    .await?,
            ),
            "cancel" => {
                let task_id = options.task.as_deref().unwrap();
                let task = connections.observe.task(task_id).await?;
                if task.session_id != id {
                    return Err(Failure::invalid("task belongs to another Session"));
                }
                let target = task.cancel_target();
                emit(&json!({"type":"cancel_requested","session_id":id,"target":target}))?;
                let cancelled = connections.execute.cancel(&target).await?;
                emit(&json!({"type":"cancel_ack","session_id":id,"target":target,"cancelled":cancelled}))?;
                result("cancel", Some(id), json!({"target":target,"cancelled":cancelled}))
            }
            "recover" => result("recover", Some(id), connections.execute.recover(id).await?),
            "events" => {
                let mut cursor = Cursor::load(id, options.after, options.cursor_file.clone())?;
                let page = connections
                    .observe
                    .events(id, cursor.after, options.limit, options.wait_ms)
                    .await?;
                cursor.process(&page)?;
                result(
                    "events",
                    Some(id),
                    json!({"after":cursor.after,"count":page.len()}),
                )
            }
            "prompt" => prompt(&connections.execute, &connections.observe, options).await,
            _ => unreachable!(),
        }
    }
    .await;
    connections.close().await;
    outcome
}

async fn prompt(execute: &Api, observe: &Api, options: &Options) -> Result<()> {
    let id = options.session.as_deref().unwrap();
    let mut cursor = Cursor::load(id, None, options.cursor_file.clone())?;
    // Check saved position before admitting a task, and process the old suffix first.
    drain(observe, id, &mut cursor).await?;
    let mut interrupts = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let task = execute
        .enqueue(id, options.text.as_deref().unwrap())
        .await?;
    if let Err(error) = emit(&json!({"type":"task","session_id":id,"task":task})) {
        let _ = cancel_task(observe, &task.id).await;
        return Err(error);
    }
    let execution = execute.resume(&task.id);
    tokio::pin!(execution);
    let mut ended: Option<Result<SessionView>> = None;
    let mut cancelled = false;
    let mut cancel_acknowledged = false;
    let monitoring:Result<()>=async {
        while ended.is_none() || (cancelled && !cancel_acknowledged) {
            // Keep each admitted events request alive until its paired reply.
            // Losing a select branch must never drop an IPC request future.
            let page=observe.events(id,cursor.after,1000,200);
            tokio::pin!(page);
            loop {
                tokio::select! {
                    response=&mut execution, if ended.is_none()=>{ended=Some(response);},
                    response=&mut page=>{
                        cursor.process(&response?)?;
                        if cancelled && !cancel_acknowledged {
                            let observed=observe.task(&task.id).await?;
                            // resume can claim a queued Task while we read. Wait for
                            // that original Task's owner instead of sending fence 0.
                            if observed.state != TaskState::Queued || ended.is_some() {
                                let target=observed.cancel_target();
                                let accepted=observe.cancel(&target).await?;
                                cancel_acknowledged=true;
                                emit(&json!({"type":"cancel_ack","session_id":id,"target":target,"cancelled":accepted}))?;
                            }
                        }
                        break;
                    },
                    signal=interrupts.recv(), if !cancelled=>{
                        if signal.is_none() { return Err(Failure::invalid("interrupt listener stopped")); }
                        cancelled=true;
                        emit(&json!({"type":"cancel_requested","session_id":id,"task_id":task.id}))?;
                    },
                }
            }
        }
        Ok(())
    }.await;
    if let Err(mut error) = monitoring {
        // An output/cursor failure must not abandon a running SDK worker.
        let _ = cancel_task(observe, &task.id).await;
        if ended.is_none() {
            ended = Some((&mut execution).await);
        }
        if ended
            .as_ref()
            .is_some_and(|result| result.as_ref().is_err_and(|error| error.outcome_unknown))
        {
            error.outcome_unknown = true;
        }
        return Err(error);
    }
    drain(observe, id, &mut cursor).await?;
    let terminal = observe.task(&task.id).await?;
    emit(&json!({"type":"task","session_id":id,"task":terminal}))?;
    let execution = ended.unwrap();
    let session = observe.get(id).await?;
    emit(
        &json!({"type":"prompt_finished","session_id":id,"session":session,"task":terminal,"after":cursor.after}),
    )?;
    execution?;
    if terminal.state != TaskState::Completed {
        return Err(Failure {
            code: "task_not_completed".into(),
            message: format!(
                "original task ended as {:?}; inspect task and audit records",
                terminal.state
            ),
            outcome_unknown: matches!(
                terminal.state,
                TaskState::OutcomeUnknown | TaskState::Draining
            ),
        });
    }
    Ok(())
}
async fn cancel_task(api: &Api, task_id: &str) -> Result<bool> {
    let observed = api.task(task_id).await?;
    api.cancel(&observed.cancel_target()).await
}
async fn drain(api: &Api, id: &str, cursor: &mut Cursor) -> Result<()> {
    loop {
        let page = api.events(id, cursor.after, 1000, 0).await?;
        if page.is_empty() {
            return Ok(());
        }
        cursor.process(&page)?;
    }
}
