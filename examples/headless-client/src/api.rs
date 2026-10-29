use crate::{Failure, Result, options::Connection};
use cayw_ai::{
    AssistantEvent, AssistantMessage, AssistantStream, CancellationToken, Model, ModelRequest,
    Provider, StopReason,
};
use cayw_core::EventRecord;
use cayw_ipc::{Client, ClientConfig};
use cayw_runtime::{
    ApprovalChoice, CancelTarget, HeadlessConfig, Runtime, SessionView, TaskRecord,
};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

const UNCONFIGURED_PROVIDER: &str = "unconfigured";
const UNCONFIGURED_MODEL: &str = "unconfigured";

/// The Runtime contract always needs a provider. Query and control commands
/// never stream it. Streaming fails explicitly instead of inventing model output.
struct UnconfiguredProvider;

impl Provider for UnconfiguredProvider {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let start = AssistantMessage::new(&request.model);
        let error = start.clone().failure(
            StopReason::Error,
            "model configuration is required for model execution",
        );
        Box::pin(futures::stream::iter([
            AssistantEvent::Start { message: start },
            AssistantEvent::Error { message: error },
        ]))
    }
}

pub enum Api {
    Sdk(Runtime),
    Ipc(Client),
}
pub struct Connections {
    pub execute: Api,
    pub observe: Api,
    pub metadata: Value,
}
impl Connections {
    /// `needs_model` is true only for commands that can execute a model turn.
    /// Query and control commands open the same persisted Runtime without
    /// requiring a model object or reading the selected credential.
    pub async fn open(connection: &Connection, needs_model: bool) -> Result<Self> {
        match connection {
            Connection::Sdk {
                db,
                workspace,
                config,
            } => {
                let headless = match config {
                    Some(path) => HeadlessConfig::load(path)?,
                    None => HeadlessConfig::default(),
                };
                let selected = headless.model().cloned();
                let runtime = headless.into_runtime_config()?;
                let (provider, model, model_configuration, requested_model, model_execution) =
                    if needs_model {
                        let selected = selected.ok_or_else(|| {
                            Failure::invalid("model configuration is required for prompt")
                        })?;
                        let configured = selected.build()?;
                        (
                            configured.provider,
                            configured.model,
                            serde_json::to_value(&selected)?,
                            Some(selected.id().to_owned()),
                            "configured",
                        )
                    } else {
                        let configuration = selected
                            .as_ref()
                            .map(serde_json::to_value)
                            .transpose()?
                            .unwrap_or(Value::Null);
                        (
                            Arc::new(UnconfiguredProvider) as Arc<dyn Provider>,
                            Model::new(UNCONFIGURED_PROVIDER, UNCONFIGURED_MODEL),
                            configuration,
                            selected.as_ref().map(|config| config.id().to_owned()),
                            "not_requested",
                        )
                    };
                let execute = Runtime::open(
                    db,
                    workspace,
                    provider.clone(),
                    model.clone(),
                    runtime.clone(),
                )?;
                let observe = Runtime::open(db, workspace, provider, model, runtime)?;
                Ok(Self {
                    execute: Api::Sdk(execute),
                    observe: Api::Sdk(observe),
                    metadata: json!({
                        "type":"runtime",
                        "mode":"sdk",
                        "model_configuration":model_configuration,
                        "requested_model":requested_model,
                        "model_execution":model_execution
                    }),
                })
            }
            Connection::Ipc { address, token_env } => {
                let token = environment(token_env)?;
                let probe = Client::connect(*address, token.clone()).await?;
                let probe_api = Api::Ipc(probe);
                let health = probe_api.health().await?;
                probe_api.close().await;

                let timeout_ms = health
                    .get("request_timeout_ms")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| {
                        Failure::invalid(
                            "server health response missing valid integer request_timeout_ms",
                        )
                    })?;
                if timeout_ms == 0 {
                    return Err(Failure::invalid(
                        "server request_timeout_ms must be non-zero",
                    ));
                }
                let request_timeout = Duration::from_millis(timeout_ms);
                if request_timeout.is_zero()
                    || std::time::Instant::now()
                        .checked_add(request_timeout)
                        .is_none()
                {
                    return Err(Failure::invalid(
                        "server request_timeout_ms is invalid or non-representable",
                    ));
                }
                let config = ClientConfig {
                    request_timeout,
                    ..ClientConfig::default()
                };
                let execute =
                    Client::connect_with_config(*address, token.clone(), config.clone()).await?;
                let observe = match Client::connect_with_config(*address, token, config).await {
                    Ok(client) => client,
                    Err(error) => {
                        execute.close().await;
                        return Err(error.into());
                    }
                };
                Ok(Self {
                    execute: Api::Ipc(execute),
                    observe: Api::Ipc(observe),
                    metadata: json!({"type":"runtime","mode":"ipc","health":health,"requested_model":health["model"]}),
                })
            }
        }
    }
    pub async fn close(&self) {
        self.execute.close().await;
        self.observe.close().await;
    }
}
fn environment(name: &str) -> Result<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            Failure::invalid("required environment value is missing or empty; use --help")
        })
}
impl Api {
    async fn close(&self) {
        if let Self::Ipc(client) = self {
            client.close().await;
        }
    }
    pub async fn health(&self) -> Result<Value> {
        match self {
            Self::Sdk(rt) => Ok(serde_json::to_value(rt.health()?)?),
            Self::Ipc(c) => Ok(c.request("runtime.health", json!({})).await?),
        }
    }
    pub async fn create(&self, label: Option<String>) -> Result<SessionView> {
        match self {
            Self::Sdk(rt) => Ok(rt.create(label)?),
            Self::Ipc(c) => rpc(c, "session.create", json!({"label":label})).await,
        }
    }
    pub async fn get(&self, id: &str) -> Result<SessionView> {
        match self {
            Self::Sdk(rt) => Ok(rt.get(id)?),
            Self::Ipc(c) => rpc(c, "session.get", json!({"session_id":id})).await,
        }
    }
    pub async fn tasks(&self, id: &str) -> Result<Vec<TaskRecord>> {
        match self {
            Self::Sdk(rt) => Ok(rt.tasks(id)?),
            Self::Ipc(c) => rpc(c, "session.tasks", json!({"session_id":id})).await,
        }
    }
    pub async fn task(&self, id: &str) -> Result<TaskRecord> {
        match self {
            Self::Sdk(rt) => Ok(rt.task(id)?),
            Self::Ipc(c) => rpc(c, "task.get", json!({"task_id":id})).await,
        }
    }
    pub async fn events(
        &self,
        id: &str,
        after: u64,
        limit: usize,
        wait_ms: u64,
    ) -> Result<Vec<EventRecord>> {
        match self {
            Self::Sdk(rt) => Ok(rt
                .wait_events(
                    id,
                    after,
                    limit,
                    Duration::from_millis(wait_ms),
                    CancellationToken::new(),
                )
                .await?),
            Self::Ipc(c) => {
                let mut limit = limit;
                loop {
                    match c
                        .request(
                            "session.events",
                            json!({"session_id":id,"after":after,"limit":limit,"wait_ms":wait_ms}),
                        )
                        .await
                    {
                        Ok(page) => return Ok(serde_json::from_value(page)?),
                        // Only this read may repeat, at the same unconsumed cursor.
                        Err(error)
                            if error.code == "response_too_large_outcome_unknown" && limit > 1 =>
                        {
                            limit /= 2;
                        }
                        Err(error) => return Err(error.into()),
                    }
                }
            }
        }
    }
    pub async fn audit(&self, id: &str, after: u64, limit: usize) -> Result<Value> {
        match self {
            Self::Sdk(rt) => Ok(serde_json::to_value(rt.audit(id, after, limit)?)?),
            Self::Ipc(c) => Ok(c
                .request(
                    "session.audit",
                    json!({"session_id":id,"after":after,"limit":limit}),
                )
                .await?),
        }
    }
    pub async fn enqueue(&self, id: &str, text: &str) -> Result<TaskRecord> {
        match self {
            Self::Sdk(rt) => Ok(rt.enqueue(id, text)?),
            Self::Ipc(c) => rpc(c, "task.enqueue", json!({"session_id":id,"text":text})).await,
        }
    }
    pub async fn resume(&self, id: &str) -> Result<SessionView> {
        match self {
            Self::Sdk(rt) => Ok(rt.resume(id, CancellationToken::new()).await?),
            Self::Ipc(c) => rpc(c, "task.resume", json!({"task_id":id})).await,
        }
    }
    pub async fn cancel(&self, target: &CancelTarget) -> Result<bool> {
        match self {
            Self::Sdk(rt) => Ok(rt.cancel(target)?),
            Self::Ipc(c) => {
                let response = c
                    .request("session.cancel", serde_json::to_value(target)?)
                    .await?;
                response["cancelled"].as_bool().ok_or_else(|| {
                    Failure::invalid("cancellation response has no boolean acknowledgment")
                })
            }
        }
    }
    pub async fn recover(&self, id: &str) -> Result<SessionView> {
        match self {
            Self::Sdk(rt) => Ok(rt.recover(id)?),
            Self::Ipc(c) => rpc(c, "session.recover", json!({"session_id":id})).await,
        }
    }
    pub async fn approve(&self, id: &str, approval: &str, decision: &str) -> Result<Value> {
        let choice = match decision {
            "allow_once" => ApprovalChoice::AllowOnce,
            "allow_task" => ApprovalChoice::AllowTask,
            "deny" => ApprovalChoice::Deny,
            _ => return Err(Failure::invalid("invalid approval decision")),
        };
        match self {
            Self::Sdk(rt) => Ok(serde_json::to_value(
                rt.approve(id, approval, choice, "sdk-user")?,
            )?),
            Self::Ipc(c) => Ok(c
                .request(
                    "approval.decide",
                    json!({"session_id":id,"approval_id":approval,"decision":choice}),
                )
                .await?),
        }
    }
}
async fn rpc<T: DeserializeOwned>(client: &Client, method: &str, params: Value) -> Result<T> {
    Ok(serde_json::from_value(
        client.request(method, params).await?,
    )?)
}
