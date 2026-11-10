//! Model decisions are explicit CodeMode calls, separate from user acceptance.
use crate::{Runtime, RuntimeConfig, cells::CallScope, permissions::ApprovalChoice};
use async_trait::async_trait;
use cayw_ai::{CancellationToken, ToolDefinition};
use cayw_codemode::CapabilityError;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalMode {
    ModelAuto,
    #[default]
    Explicit,
}

pub(crate) fn is_tool(name: &str) -> bool {
    matches!(
        name,
        "approval.pending" | "approval.decide" | "capability.decide"
    )
}

pub(crate) fn definitions(config: &RuntimeConfig) -> Vec<ToolDefinition> {
    if config.approval_mode != ApprovalMode::ModelAuto {
        return vec![];
    }
    let mut definitions = vec![
        ToolDefinition {
            name: "approval.pending".into(),
            description: "Read this task's exact pending capability approvals. In model-auto mode you decide them yourself with approval.decide, then wait for the original cell instead of replaying it.".into(),
            parameters: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {}
            }),
        },
        ToolDefinition {
            name: "approval.decide".into(),
            description: "Decide one pending approval from this task as model-auto. This grants or denies execution permission, not user acceptance or proof of completion.".into(),
            parameters: json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["approval_id", "decision"],
                "properties": {
                    "approval_id": {"type": "string", "minLength": 1},
                    "decision": {"enum": ["allow_once", "allow_task", "deny"]}
                }
            }),
        },
    ];
    if config.request_policy.is_some() {
        definitions.push(ToolDefinition {
            name: "capability.decide".into(),
            description: "Evaluate and decide this task's capability.request by its request_id as model-auto. This is request authority, distinct from approval.decide. An allow needs lifetime_ms; reason explains the decision.".into(),
            parameters: json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["request_id", "decision", "reason"],
                "properties": {
                    "request_id": {"type": "string", "minLength": 1},
                    "decision": {"enum": ["allow", "deny"]},
                    "reason": {"type": "string", "minLength": 1},
                    "lifetime_ms": {"type": "integer", "minimum": 1}
                }
            }),
        });
    }
    definitions
}

struct ToolGate(Arc<CallScope>);

#[async_trait]
impl cayw_host::PermissionGate for ToolGate {
    async fn authorize(
        &self,
        name: &str,
        args: &Value,
        cancel: CancellationToken,
    ) -> Result<(), CapabilityError> {
        let scope = &self.0;
        if scope.context.inner.config.approval_mode != ApprovalMode::ModelAuto
            || !is_tool(name)
            || name != scope.name
            || args != &scope.arguments
        {
            return Err(error("model approval tool is not enabled for this call"));
        }
        scope.context.check(&cancel).map_err(error)?;
        scope.request.check(&scope.context)?;
        scope
            .authorize(crate::cells::Authorization {
                proof: scope.proof("model-auto-control".into(), None),
                grant: None,
            })
            .map_err(error)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Decide {
    approval_id: String,
    decision: ApprovalChoice,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestDecide {
    request_id: String,
    decision: RequestDecision,
    reason: String,
    lifetime_ms: Option<u64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum RequestDecision {
    Allow,
    Deny,
}

struct Service<'a>(&'a Arc<CallScope>);

#[async_trait]
impl cayw_host::ExternalHost for Service<'_> {
    async fn call(
        &self,
        name: &str,
        args: Value,
        cancel: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        let scope = self.0;
        scope.check_authority(&cancel)?;
        let runtime = Runtime {
            inner: scope.context.inner.clone(),
        };
        let session = &scope.context.lease.session_id;
        let task = &scope.context.task.id;
        match name {
            "approval.pending" => {
                let pending: Vec<_> = runtime
                    .get(session)
                    .map_err(error)?
                    .pending_approvals
                    .into_iter()
                    .filter(|a| a.input.task_id == *task)
                    .collect();
                serde_json::to_value(pending).map_err(error)
            }
            "approval.decide" => {
                let input: Decide = serde_json::from_value(args).map_err(error)?;
                let pending = runtime
                    .get(session)
                    .map_err(error)?
                    .pending_approvals
                    .into_iter()
                    .any(|a| a.id == input.approval_id && a.input.task_id == *task);
                if !pending {
                    return Err(error("approval is not pending for this task"));
                }
                let result = crate::permissions::decide(
                    &runtime.inner,
                    session,
                    &input.approval_id,
                    input.decision,
                    "model-auto",
                )
                .map_err(error)?;
                serde_json::to_value(result).map_err(error)
            }
            "capability.decide" => {
                let input: RequestDecide = serde_json::from_value(args).map_err(error)?;
                // Validate before claiming the single evaluation ticket.
                let lifetime = match input.decision {
                    RequestDecision::Allow => {
                        let ms = input
                            .lifetime_ms
                            .ok_or_else(|| error("allow requires lifetime_ms"))?;
                        if ms == 0 || ms > runtime.request_limits().max_grant_lifetime_ms {
                            return Err(error("lifetime_ms exceeds request grant limits"));
                        }
                        Some(Duration::from_millis(ms))
                    }
                    RequestDecision::Deny => {
                        if input.lifetime_ms.is_some() {
                            return Err(error("deny does not take lifetime_ms"));
                        }
                        None
                    }
                };
                if input.reason.trim().is_empty()
                    || input.reason.len() > 2048
                    || input
                        .reason
                        .chars()
                        .any(|c| c.is_control() && c != '\n' && c != '\t')
                {
                    return Err(error("reason must not be empty"));
                }
                let ticket = runtime
                    .request_evaluate(task, &input.request_id, "model-auto")
                    .map_err(error)?;
                let result = match lifetime {
                    Some(lifetime) => runtime.request_approve(task, &ticket, lifetime, &input.reason),
                    None => runtime.request_deny(task, &ticket, &input.reason),
                }
                .map_err(error)?;
                serde_json::to_value(result).map_err(error)
            }
            _ => Err(error("unknown model approval tool")),
        }
    }
}

fn error(value: impl std::fmt::Display) -> CapabilityError {
    CapabilityError::new(value.to_string())
}

pub(crate) async fn invoke(
    scope: &Arc<CallScope>,
    base: &cayw_host::Host,
    args: Value,
    cancel: CancellationToken,
) -> Result<Value, CapabilityError> {
    let definition = definitions(&scope.context.inner.config)
        .into_iter()
        .find(|d| d.name == scope.name)
        .ok_or_else(|| error("model approval tool is not configured"))?;
    let host = base.scoped(Arc::new(ToolGate(scope.clone())), scope.clone())?;
    host.call_service(&definition, args, cancel, &Service(scope))
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controls_exist_only_in_model_auto_and_request_decisions_require_request_policy() {
        let mut config = RuntimeConfig::default();
        assert!(definitions(&config).is_empty());
        config.approval_mode = ApprovalMode::ModelAuto;
        let names: Vec<_> = definitions(&config).into_iter().map(|d| d.name).collect();
        assert_eq!(names, vec!["approval.pending", "approval.decide"]);
    }

    #[test]
    fn decision_ids_are_distinct_and_actor_is_not_model_input() {
        assert!(
            serde_json::from_value::<Decide>(json!({
                "request_id": "r1",
                "decision": "allow_once"
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<RequestDecide>(json!({
                "approval_id": "a1",
                "decision": "allow",
                "reason": "run",
                "lifetime_ms": 1000
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<Decide>(json!({
                "approval_id": "a1",
                "decision": "allow_once",
                "actor": "user"
            }))
            .is_err()
        );
    }
}
