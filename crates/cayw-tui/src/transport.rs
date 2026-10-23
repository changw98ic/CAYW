//! Concurrent RPC ownership, consumed event cursors and explicit request settlement.
//! Reconnection never replays a mutation, including a prompt.
use cayw_ipc::{Client, ClientConfig, ClientError};
use serde_json::{Value, json};
use std::{net::SocketAddr, time::Duration};
use tokio::{
    sync::{mpsc, oneshot},
    task::{JoinHandle, JoinSet},
    time::{Instant, sleep_until, timeout},
};

const RECONNECT_DELAY: Duration = Duration::from_secs(2);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const EVENT_DELAY: Duration = Duration::from_millis(250);
const STREAM_EVENT_DELAY: Duration = Duration::from_millis(40);
const MAX_JOBS: usize = 16;

#[derive(Clone, Debug)]
pub struct Request {
    pub tag: u64,
    pub method: &'static str,
    pub params: Value,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Watch {
    pub session_id: String,
    pub after: u64,
}

#[derive(Debug)]
pub enum Update {
    Connected,
    Disconnected(String),
    TaskAdmitted {
        session_id: String,
        task_id: String,
    },
    Response {
        request: Request,
        result: Result<Value, String>,
    },
    Events {
        watch: Watch,
        result: Result<Value, String>,
    },
}

enum Control {
    Call(Request),
    Watch(Option<Watch>),
    Reconnect,
    Shutdown,
}

struct Failure {
    message: String,
    disconnected: bool,
}
type Reply = Result<Value, Failure>;

struct TaskAdmission {
    epoch: u64,
    session_id: String,
    task_id: String,
    proceed: oneshot::Sender<bool>,
}

enum Finished {
    Connect {
        epoch: u64,
        result: Result<Client, String>,
    },
    Call {
        epoch: u64,
        request: Request,
        result: Reply,
    },
    Poll {
        epoch: u64,
        watch: Watch,
        result: Reply,
    },
    Closed,
}

pub struct Transport {
    control: mpsc::Sender<Control>,
    updates: mpsc::UnboundedReceiver<Update>,
    task: Option<JoinHandle<()>>,
}

impl Transport {
    pub fn start(address: SocketAddr, token: String) -> Self {
        let (control, controls) = mpsc::channel(64);
        let (updates, receiver) = mpsc::unbounded_channel();
        let task = tokio::spawn(run(address, token, controls, updates));
        Self {
            control,
            updates: receiver,
            task: Some(task),
        }
    }

    pub fn call(&self, request: Request) -> Result<(), &'static str> {
        self.control
            .try_send(Control::Call(request))
            .map_err(|_| "IPC request queue unavailable")
    }

    /// The UI supplies only a cursor it has successfully consumed.
    pub fn watch(&self, watch: Option<Watch>) -> Result<(), &'static str> {
        self.control
            .try_send(Control::Watch(watch))
            .map_err(|_| "IPC request queue unavailable")
    }

    pub fn reconnect(&self) -> Result<(), &'static str> {
        self.control
            .try_send(Control::Reconnect)
            .map_err(|_| "IPC request queue unavailable")
    }

    pub async fn next(&mut self) -> Option<Update> {
        self.updates.recv().await
    }

    /// Close this client's IPC connection and join every owned request.
    /// The connection cancels only its RPCs; it proves neither run ownership nor rollback.
    pub async fn shutdown(&mut self) {
        if self.task.is_none() {
            return;
        }
        let _ = self.control.send(Control::Shutdown).await;
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
        self.updates.close();
        while self.updates.try_recv().is_ok() {}
    }
}

impl Drop for Transport {
    fn drop(&mut self) {
        // The interactive exit always awaits shutdown; this is a panic fallback.
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

async fn run(
    address: SocketAddr,
    token: String,
    mut controls: mpsc::Receiver<Control>,
    updates: mpsc::UnboundedSender<Update>,
) {
    let mut jobs = JoinSet::new();
    let (admissions, mut admitted) = mpsc::unbounded_channel::<TaskAdmission>();
    let mut client: Option<Client> = None;
    let mut connecting = false;
    let mut next_connect = Instant::now();
    let mut next_poll = Instant::now();
    let mut watch: Option<Watch> = None;
    let mut polling = false;
    let mut awaiting_consumption = false;
    let mut epoch = 0_u64;

    loop {
        tokio::select! {
            Some(admission) = admitted.recv() => {
                let current = admission.epoch == epoch;
                if current {
                    let _ = updates.send(Update::TaskAdmitted {
                        session_id: admission.session_id,
                        task_id: admission.task_id,
                    });
                }
                let _ = admission.proceed.send(current);
            },
            control = controls.recv() => match control {
                Some(Control::Shutdown) | None => break,
                Some(Control::Watch(next)) => { watch = next; awaiting_consumption = false; }
                Some(Control::Reconnect) => {
                    close_connection(client.take(), &mut jobs);
                    epoch = epoch.wrapping_add(1);
                    connecting = false;
                    polling = false;
                    awaiting_consumption = false;
                    next_connect = Instant::now();
                    let _ = updates.send(Update::Disconnected("Reconnecting. Inspecting session state; requests will not be replayed.".into()));
                }
                Some(Control::Call(request)) => {
                    let Some(connection) = client.clone() else {
                        let _ = updates.send(Update::Response { request, result: Err("Disconnected; request was not sent. Reconnect and inspect the session before trying again.".into()) });
                        continue;
                    };
                    if jobs.len() >= MAX_JOBS {
                        let _ = updates.send(Update::Response { request, result: Err("Too many pending requests; request was not sent.".into()) });
                        continue;
                    }
                    let admissions = admissions.clone();
                    jobs.spawn(async move {
                        let result = if request.method == "session.prompt" {
                            prompt_task(&connection, request.params.clone(), epoch, &admissions).await
                        } else {
                            request_value(&connection, request.method, request.params.clone()).await
                        };
                        Finished::Call { epoch, request, result }
                    });
                }
            },
            _ = sleep_until(next_connect), if client.is_none() && !connecting => {
                connecting = true;
                let secret = token.clone();
                jobs.spawn(async move {
                    // Prompts may last as long as the host's run budget; short RPCs
                    // retain their separate timeout below. The UI remains responsive.
                    let config = ClientConfig { request_timeout: Duration::from_secs(3600), ..ClientConfig::default() };
                    let result = match timeout(CONNECT_TIMEOUT, Client::connect_with_config(address, secret.clone(), config)).await {
                        Ok(result) => result.map_err(|error| redact(&error.to_string(), &secret)),
                        Err(_) => Err("IPC connection timed out".into()),
                    };
                    Finished::Connect { epoch, result }
                });
            }
            _ = sleep_until(next_poll), if client.is_some() && watch.is_some() && !polling && !awaiting_consumption => {
                let connection = client.as_ref().expect("guarded connection").clone();
                let current = watch.as_ref().expect("guarded watch").clone();
                polling = true;
                jobs.spawn(async move {
                    let result = request_value(&connection, "session.events", json!({"session_id":current.session_id,"after":current.after,"limit":100})).await;
                    Finished::Poll { epoch, watch: current, result }
                });
            }
            completed = jobs.join_next(), if !jobs.is_empty() => match completed {
                Some(Ok(Finished::Connect { epoch: job_epoch, result })) => {
                    if job_epoch != epoch {
                        if let Ok(connection) = result { close_connection(Some(connection), &mut jobs); }
                        continue;
                    }
                    connecting = false;
                    match result {
                        Ok(connection) => { client = Some(connection); let _ = updates.send(Update::Connected); next_poll = Instant::now(); }
                        Err(error) => { let _ = updates.send(Update::Disconnected(error)); next_connect = Instant::now() + RECONNECT_DELAY; }
                    }
                }
                Some(Ok(Finished::Call { epoch: job_epoch, request, result })) => {
                    // Disconnect already invalidated the UI projection. A response
                    // from that connection cannot settle a newer prompt or snapshot.
                    if job_epoch != epoch { continue; }
                    if result.as_ref().is_err_and(|error| error.disconnected) {
                        close_connection(client.take(), &mut jobs);
                        epoch = epoch.wrapping_add(1);
                        connecting = false;
                        polling = false;
                        awaiting_consumption = false;
                        next_connect = Instant::now() + RECONNECT_DELAY;
                        let _ = updates.send(Update::Disconnected("IPC connection failed; reconnecting without replaying requests".into()));
                    }
                    let result = result.map_err(|error| redact(&error.message, &token));
                    let _ = updates.send(Update::Response { request, result });
                }
                Some(Ok(Finished::Poll { epoch: job_epoch, watch: polled, result })) => {
                    if job_epoch != epoch { continue; }
                    polling = false;
                    next_poll = Instant::now() + EVENT_DELAY;
                    if result.as_ref().is_err_and(|error| error.disconnected) {
                        close_connection(client.take(), &mut jobs);
                        epoch = epoch.wrapping_add(1);
                        connecting = false;
                        awaiting_consumption = false;
                        next_connect = Instant::now() + RECONNECT_DELAY;
                        let _ = updates.send(Update::Disconnected("Event read failed; reconnecting from the consumed cursor".into()));
                    }
                    if watch.as_ref() != Some(&polled) { continue; }
                    let mut result = result.map_err(|error| redact(&error.message, &token));
                    match &result {
                        Ok(Value::Array(records)) => {
                            awaiting_consumption = !records.is_empty();
                            if records.len() >= 100 {
                                next_poll = Instant::now();
                            } else if !records.is_empty() {
                                next_poll = Instant::now() + STREAM_EVENT_DELAY;
                            }
                        }
                        Ok(_) => { result = Err("session.events returned a non-array result; cursor retained".into()); awaiting_consumption = true; }
                        Err(_) => { awaiting_consumption = true; }
                    }
                    let _ = updates.send(Update::Events { watch: polled, result });
                }
                Some(Ok(Finished::Closed)) | None => {},
                Some(Err(error)) => { let _ = updates.send(Update::Disconnected(format!("IPC worker stopped: {}", redact(&error.to_string(), &token)))); break; }
            }
        }
    }

    // A prompt request may be rejected or still awaiting admission. Closing our
    // connection cancels only its outstanding RPCs, including in that race.
    // session.cancel remains an explicit user action in the driver.
    if let Some(connection) = client {
        connection.close().await;
    }
    jobs.abort_all();
    while jobs.join_next().await.is_some() {}
}

fn close_connection(client: Option<Client>, jobs: &mut JoinSet<Finished>) {
    if let Some(connection) = client {
        jobs.spawn(async move {
            connection.close().await;
            Finished::Closed
        });
    }
}

async fn prompt_task(
    client: &Client,
    params: Value,
    epoch: u64,
    admissions: &mpsc::UnboundedSender<TaskAdmission>,
) -> Reply {
    let session_id = params["session_id"].as_str().unwrap_or("").to_owned();
    let task = client
        .request("task.enqueue", params)
        .await
        .map_err(failure)?;
    let task_id = task["id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .ok_or_else(|| Failure {
            message: "Task admission did not return an identity; inspect the original request"
                .into(),
            disconnected: false,
        })?
        .to_owned();
    if task["session_id"].as_str() != Some(session_id.as_str()) {
        return Err(Failure {
            message: "Task admission belongs to another Session".into(),
            disconnected: false,
        });
    }
    let (proceed, permitted) = oneshot::channel();
    let admission = TaskAdmission {
        epoch,
        session_id,
        task_id: task_id.clone(),
        proceed,
    };
    if admissions.send(admission).is_err() || permitted.await != Ok(true) {
        return Err(Failure {
            message: "Task was admitted on a closed connection; inspect its original identity before resuming".into(),
            disconnected: true,
        });
    }
    client
        .request("task.resume", json!({"task_id":task_id}))
        .await
        .map_err(failure)
}

async fn cancel_original_task(client: &Client, params: &Value) -> Reply {
    let task_id = params["task_id"].as_str().ok_or_else(|| Failure {
        message: "Cancellation requires the original Task ID".into(),
        disconnected: false,
    })?;
    let task = client
        .request("task.get", json!({"task_id":task_id}))
        .await
        .map_err(failure)?;
    if task["id"].as_str() != Some(task_id) || task["session_id"] != params["session_id"] {
        return Err(Failure {
            message: "Task response differs from the original cancellation target".into(),
            disconnected: false,
        });
    }
    let target = json!({"session_id":task["session_id"],"task_id":task["id"],
        "worker_id":task["worker_id"],"attempt_id":task["attempt_id"],"fence":task["fence"]});
    client
        .request("session.cancel", target)
        .await
        .map_err(failure)
}

async fn request_value(client: &Client, method: &str, params: Value) -> Reply {
    if method == "session.prompt" {
        return client.request(method, params).await.map_err(failure);
    }
    if method == "session.cancel" {
        return match timeout(REQUEST_TIMEOUT, cancel_original_task(client, &params)).await {
            Ok(result) => result,
            Err(_) => Err(Failure {
                message: "Original Task cancellation observation timed out; inspect that Task"
                    .into(),
                disconnected: true,
            }),
        };
    }
    match timeout(REQUEST_TIMEOUT, client.request(method, params)).await {
        Ok(result) => result.map_err(failure),
        Err(_) => Err(Failure {
            message: format!("{method} timed out; the request may have reached the runtime"),
            disconnected: true,
        }),
    }
}

fn failure(error: ClientError) -> Failure {
    let disconnected = matches!(
        error.code.as_str(),
        "connection_closed"
            | "connection_lost"
            | "connection_limit"
            | "protocol_error"
            | "request_timeout"
            | "request_dropped"
            | "write_failed"
            | "cancel_failed"
    );
    Failure {
        message: error.to_string(),
        disconnected,
    }
}

fn redact(message: &str, token: &str) -> String {
    if token.is_empty() {
        message.to_owned()
    } else {
        message.replace(token, "[redacted]")
    }
}
