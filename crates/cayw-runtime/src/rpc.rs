use crate::{ApprovalChoice, Runtime, RuntimeError};
use async_trait::async_trait;
use cayw_ai::CancellationToken;
use cayw_core::{CapabilityReconciliation, Reconciliation};
use cayw_ipc::{RpcError, RpcHandler};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::time::Duration;

fn parse<T: DeserializeOwned>(params: Value) -> Result<T, RpcError> {
    serde_json::from_value(params)
        .map_err(|error| RpcError::new("invalid_params", error.to_string()))
}
fn encode(value: impl serde::Serialize) -> Result<Value, RpcError> {
    serde_json::to_value(value).map_err(|error| RpcError::new("serialization", error.to_string()))
}
impl From<RuntimeError> for RpcError {
    fn from(error: RuntimeError) -> Self {
        let code = match &error {
            RuntimeError::InvalidInput(_) | RuntimeError::Json(_) => "invalid_params",
            RuntimeError::Busy => "busy",
            RuntimeError::QueueFull => "queue_full",
            RuntimeError::Cancelled => "cancelled",
            RuntimeError::Budget(_) => "budget_exceeded",
            RuntimeError::NeedsReconciliation => "needs_reconciliation",
            RuntimeError::Frozen(_) => "session_frozen",
            RuntimeError::StaleApproval => "stale_approval",
            RuntimeError::RecoveryRequired {
                outcome_unknown: true,
                ..
            } => "runtime_outcome_unknown",
            RuntimeError::RecoveryRequired { .. } => "recovery_required",
            RuntimeError::Core(_) => "session_error",
            _ => "runtime_error",
        };
        Self::new(code, error.to_string())
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Create {
    label: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Session {
    session_id: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Prompt {
    session_id: String,
    text: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Events {
    session_id: String,
    #[serde(default)]
    after: u64,
    #[serde(default = "page_limit")]
    limit: usize,
    #[serde(default)]
    wait_ms: u64,
}
fn page_limit() -> usize {
    100
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Approval {
    session_id: String,
    approval_id: String,
    decision: ApprovalChoice,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Revoke {
    session_id: String,
    grant_id: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Reconcile {
    session_id: String,
    reconciliation: Reconciliation,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReconcileCapability {
    session_id: String,
    reconciliation: CapabilityReconciliation,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Fork {
    session_id: String,
    label: String,
    confirm: bool,
    mode: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Task {
    task_id: String,
}

/// The IPC transport owns framing, authentication, request ids and disconnect
/// cancellation. Local SDK calls and this adapter execute the same Runtime API.
#[async_trait]
impl RpcHandler for Runtime {
    async fn call(
        &self,
        method: &str,
        params: Value,
        cancel: CancellationToken,
    ) -> Result<Value, RpcError> {
        if cancel.is_cancelled() {
            return Err(RuntimeError::Cancelled.into());
        }
        match method {
            "runtime.health" => {
                let _: Empty = parse(params)?;
                encode(self.health()?)
            }
            "session.create" => {
                let p: Create = parse(params)?;
                encode(self.create(p.label)?)
            }
            "session.list" => {
                let _: Empty = parse(params)?;
                encode(self.list()?)
            }
            "session.get" => {
                let p: Session = parse(params)?;
                encode(self.get(&p.session_id)?)
            }
            "session.events" => {
                let p: Events = parse(params)?;
                encode(
                    self.wait_events(
                        &p.session_id,
                        p.after,
                        p.limit,
                        Duration::from_millis(p.wait_ms),
                        cancel,
                    )
                    .await?,
                )
            }
            "session.audit" => {
                let p: Events = parse(params)?;
                encode(self.audit(&p.session_id, p.after, p.limit)?)
            }
            "session.grants" => {
                let p: Session = parse(params)?;
                encode(self.grants(&p.session_id)?)
            }
            "session.prompt" => {
                let p: Prompt = parse(params)?;
                encode(self.prompt(&p.session_id, &p.text, cancel).await?)
            }
            "session.cancel" => {
                let p: Session = parse(params)?;
                Ok(json!({"cancelled":self.cancel(&p.session_id)?}))
            }
            "session.recover" => {
                let p: Session = parse(params)?;
                encode(self.recover(&p.session_id)?)
            }
            "session.reconcile" => {
                let p: Reconcile = parse(params)?;
                encode(self.reconcile(&p.session_id, p.reconciliation)?)
            }
            "capability.reconcile" => {
                let p: ReconcileCapability = parse(params)?;
                encode(self.reconcile_capability(&p.session_id, p.reconciliation)?)
            }
            "session.fork" => {
                let p: Fork = parse(params)?;
                if p.mode != "chat" {
                    return Err(RpcError::new("invalid_params", "fork mode must be chat"));
                }
                encode(self.fork(&p.session_id, &p.label, p.confirm)?)
            }
            "approval.decide" => {
                let p: Approval = parse(params)?;
                encode(self.approve(&p.session_id, &p.approval_id, p.decision, "rpc-user")?)
            }
            "approval.revoke" => {
                let p: Revoke = parse(params)?;
                self.revoke(&p.session_id, &p.grant_id, "rpc-user")?;
                Ok(json!({"revoked":true}))
            }
            "capabilities.list" => {
                let _: Empty = parse(params)?;
                encode(self.capabilities()?)
            }
            "session.tasks" => {
                let p: Session = parse(params)?;
                encode(self.tasks(&p.session_id)?)
            }
            "task.get" => {
                let p: Task = parse(params)?;
                encode(self.task(&p.task_id)?)
            }
            "task.enqueue" => {
                let p: Prompt = parse(params)?;
                encode(self.enqueue(&p.session_id, &p.text)?)
            }
            "task.resume" => {
                let p: Task = parse(params)?;
                encode(self.resume(&p.task_id, cancel).await?)
            }
            _ => Err(RpcError::new(
                "method_not_found",
                format!("unknown runtime method {method}"),
            )),
        }
    }
}
