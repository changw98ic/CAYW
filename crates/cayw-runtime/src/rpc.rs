use crate::{
    ApprovalChoice, Runtime, RuntimeError,
    input::{ImageInput, TaskInput},
};
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
            RuntimeError::NeedsReconciliation => "needs_reconciliation_outcome_unknown",
            RuntimeError::Frozen(_) => "session_frozen",
            RuntimeError::StaleApproval => "stale_approval",
            RuntimeError::StaleCancellation => "stale_cancellation",
            RuntimeError::RecoveryRequired {
                outcome_unknown: true,
                ..
            } => "runtime_outcome_unknown",
            RuntimeError::RecoveryRequired { .. } => "recovery_required",
            RuntimeError::Core(_) => "session_error",
            RuntimeError::Request(error) => match error {
                cayw_requests::RequestError::InvalidInput(_) => "invalid_params",
                cayw_requests::RequestError::Stale => "request_stale",
                cayw_requests::RequestError::NotFound => "request_not_found",
                cayw_requests::RequestError::Denied(_) => "request_denied",
                cayw_requests::RequestError::Budget => "request_budget_exceeded",
                cayw_requests::RequestError::PolicyMismatch => "request_policy_changed",
                _ => "request_authority_unavailable_outcome_unknown",
            },
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
    #[serde(default)]
    images: Vec<ImageInput>,
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
struct AnswerQuestion {
    session_id: String,
    request_id: String,
    answer: String,
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
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelGet {
    #[serde(default)]
    session_id: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelConfigure {
    provider: String,
    model: String,
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    api: Option<String>,
    #[serde(default)]
    max_output_tokens: Option<u64>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    offline_fallback: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EnqueueWithIntent {
    session_id: String,
    text: String,
    #[serde(default)]
    images: Vec<ImageInput>,
    intent: crate::TaskIntent,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestStatus {
    task_id: String,
    request_id: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestEvents {
    task_id: String,
    #[serde(default)]
    after: u64,
    #[serde(default = "request_page_limit")]
    limit: usize,
}
fn request_page_limit() -> usize {
    64
}
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum RequestDecision {
    Allow { lifetime_ms: u64 },
    Deny,
    Unavailable,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DecideRequest {
    task_id: String,
    request_id: String,
    decision: RequestDecision,
    reason: String,
}
impl DecideRequest {
    fn validate(&self, limits: &cayw_requests::RequestConfig) -> Result<(), RpcError> {
        if self.reason.trim().is_empty()
            || self.reason.len() > 2048
            || self
                .reason
                .chars()
                .any(|c| c.is_control() && c != '\n' && c != '\t')
        {
            return Err(RpcError::new(
                "invalid_params",
                "decision reason must contain 1..2048 ordinary bytes",
            ));
        }
        if let RequestDecision::Allow { lifetime_ms } = self.decision
            && (lifetime_ms == 0 || lifetime_ms > limits.max_grant_lifetime_ms)
        {
            return Err(RpcError::new(
                "invalid_params",
                "grant lifetime exceeds host maximum",
            ));
        }
        Ok(())
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RevokeRequest {
    task_id: String,
    request_id: String,
    reason: String,
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
            "runtime.model.get" => {
                let p: ModelGet = parse(params)?;
                Ok(self.model_config_view(p.session_id.as_deref()))
            }
            "runtime.model.configure" => {
                let p: ModelConfigure = parse(params)?;
                let provider_name = p.provider.trim();
                let model_id = p.model.trim();
                if provider_name.is_empty() || model_id.is_empty() {
                    return Err(RpcError::new(
                        "invalid_params",
                        "provider and model must be non-empty",
                    ));
                }
                let mut model = cayw_ai::Model::new(provider_name, model_id);
                model.max_output_tokens = Some(p.max_output_tokens.unwrap_or(4096).max(1));
                let has_live_credentials =
                    p.base_url.as_deref().is_some_and(|u| !u.trim().is_empty())
                        && p.api_key.as_deref().is_some_and(|k| !k.trim().is_empty())
                        && !p.offline_fallback;
                let provider_arc: std::sync::Arc<dyn cayw_ai::Provider> = if has_live_credentials {
                    let base = p.base_url.as_deref().unwrap_or("").trim();
                    let key = p.api_key.as_deref().unwrap_or("").trim().to_owned();
                    match p.api.as_deref().unwrap_or("chat_completions") {
                        "messages" | "anthropic" => std::sync::Arc::new(
                            cayw_providers::AnthropicProvider::new(base, key)
                                .map_err(|e| RpcError::new("invalid_params", e.to_string()))?,
                        ),
                        "responses" => std::sync::Arc::new(
                            cayw_providers::ResponsesProvider::new(base, key)
                                .map_err(|e| RpcError::new("invalid_params", e.to_string()))?,
                        ),
                        "stream_generate_content" | "generate_content" | "gemini" => {
                            std::sync::Arc::new(
                                cayw_providers::GeminiProvider::new(base, key)
                                    .map_err(|e| RpcError::new("invalid_params", e.to_string()))?,
                            )
                        }
                        _ => std::sync::Arc::new(
                            cayw_ai::providers::OpenAiProvider::new(base, key)
                                .map_err(|e| RpcError::new("invalid_params", e.to_string()))?,
                        ),
                    }
                } else {
                    std::sync::Arc::new(crate::fixture::OfflineProvider::default())
                };
                let meta = json!({
                    "provider": provider_name,
                    "model": model_id,
                    "base_url": p.base_url.unwrap_or_default(),
                    "api": p.api.unwrap_or_else(|| "chat_completions".to_owned()),
                    "live_http": has_live_credentials,
                });
                Ok(self.configure_model(
                    provider_arc,
                    model,
                    Some(meta),
                    p.session_id.as_deref(),
                )?)
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
                let input = TaskInput {
                    text: p.text,
                    images: p.images,
                };
                encode(self.prompt_input(&p.session_id, input, cancel).await?)
            }
            "session.cancel" => {
                let p: crate::CancelTarget = parse(params)?;
                Ok(json!({"cancelled":self.cancel(&p)?}))
            }
            "session.recover" => {
                let p: Session = parse(params)?;
                encode(self.recover(&p.session_id)?)
            }
            "session.reconcile" => {
                let mut p: Reconcile = parse(params)?;
                p.reconciliation.actor = "rpc-user".into();
                encode(self.reconcile(&p.session_id, p.reconciliation)?)
            }
            "capability.reconcile" => {
                let mut p: ReconcileCapability = parse(params)?;
                p.reconciliation.actor = "rpc-user".into();
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
            "session.questions" => {
                let p: Session = parse(params)?;
                encode(self.questions(&p.session_id)?)
            }
            "session.pending_questions" => {
                let p: Session = parse(params)?;
                encode(self.pending_questions(&p.session_id)?)
            }
            "session.plan" => {
                let p: Session = parse(params)?;
                encode(self.plan(&p.session_id)?)
            }
            "question.answer" => {
                let p: AnswerQuestion = parse(params)?;
                encode(self.answer_question(&p.session_id, &p.request_id, &p.answer, "rpc-user")?)
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
                let input = TaskInput {
                    text: p.text,
                    images: p.images,
                };
                encode(self.enqueue_input(&p.session_id, input)?)
            }
            "task.enqueue_with_intent" => {
                let p: EnqueueWithIntent = parse(params)?;
                encode(self.enqueue_input_with_intent(
                    &p.session_id,
                    TaskInput {
                        text: p.text,
                        images: p.images,
                    },
                    p.intent,
                )?)
            }
            "task.resume" => {
                let p: Task = parse(params)?;
                encode(self.resume(&p.task_id, cancel).await?)
            }
            "request.operations" => {
                let _: Empty = parse(params)?;
                encode(self.request_operations()?)
            }
            "request.status" => {
                let p: RequestStatus = parse(params)?;
                encode(self.request_status(&p.task_id, &p.request_id)?)
            }
            "request.events" => {
                let p: RequestEvents = parse(params)?;
                encode(self.request_events(&p.task_id, p.after, p.limit)?)
            }
            "request.task_state" => {
                let p: Task = parse(params)?;
                encode(self.request_task_state(&p.task_id)?)
            }
            "request.decide" => {
                let p: DecideRequest = parse(params)?;
                // Validate before claiming evaluation: rejected JSON must not
                // leave a request waiting for an inaccessible private ticket.
                p.validate(&self.inner.config.request_limits)?;
                let ticket = self.request_evaluate(&p.task_id, &p.request_id, "rpc-user")?;
                // These remain separate durable transitions. Cancellation,
                // expiry or a failed write can reject the original decision;
                // never issue a replacement ticket or retry it automatically.
                let record = match p.decision {
                    RequestDecision::Allow { lifetime_ms } => self.request_approve(
                        &p.task_id,
                        &ticket,
                        Duration::from_millis(lifetime_ms),
                        &p.reason,
                    )?,
                    RequestDecision::Deny => self.request_deny(&p.task_id, &ticket, &p.reason)?,
                    RequestDecision::Unavailable => {
                        self.request_evaluator_unavailable(&p.task_id, &ticket, &p.reason)?
                    }
                };
                encode(record)
            }
            "request.revoke" => {
                let p: RevokeRequest = parse(params)?;
                encode(self.request_revoke(&p.task_id, &p.request_id, "rpc-user", &p.reason)?)
            }
            "request.close_task" => {
                let p: Task = parse(params)?;
                self.close_request_task(&p.task_id)?;
                Ok(json!({"closed":true}))
            }
            _ => Err(RpcError::new(
                "method_not_found",
                format!("unknown runtime method {method}"),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unresolved_session_error_keeps_unknown_outcome_on_rpc_transport() {
        let error = RpcError::from(RuntimeError::NeedsReconciliation);
        assert_eq!(error.code, "needs_reconciliation_outcome_unknown");
        let error = cayw_ipc::ClientError::from(error);
        assert!(error.outcome_unknown);
        let known = cayw_ipc::ClientError::from(RpcError::from(RuntimeError::Busy));
        assert_eq!(known.code, "busy");
        assert!(!known.outcome_unknown);
    }
}
