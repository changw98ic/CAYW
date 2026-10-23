use crate::{
    Inner, RuntimeError,
    audit::GrantDecision,
    cells::{Authorization, CallScope},
    lock,
};
use cayw_ai::{CancellationToken, timestamp_ms};
use cayw_core::{ApprovalDecision, ApprovalInput, ApprovalRequest, ApprovalTarget};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{sync::Arc, time::Duration};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalChoice {
    AllowOnce,
    AllowTask,
    Deny,
}
impl ApprovalChoice {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::AllowOnce => "allow_once",
            Self::AllowTask => "allow_task",
            Self::Deny => "deny",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScopedPermission {
    pub capability: String,
    pub scope: String,
}
impl ScopedPermission {
    pub fn exact(capability: impl Into<String>, arguments: Value) -> Self {
        Self {
            capability: capability.into(),
            scope: canonical_scope(&arguments),
        }
    }
}
/// Argument identity is deliberately exact. A grant for one file/content/profile
/// never expands to a prefix, glob, sibling file, or new command arguments.
pub fn canonical_scope(value: &Value) -> String {
    fn canonical(value: &Value) -> Value {
        match value {
            Value::Object(object) => {
                let sorted: std::collections::BTreeMap<_, _> = object
                    .iter()
                    .map(|(key, value)| (key.clone(), canonical(value)))
                    .collect();
                serde_json::to_value(sorted).expect("JSON values serialize")
            }
            Value::Array(array) => Value::Array(array.iter().map(canonical).collect()),
            _ => value.clone(),
        }
    }
    canonical(value).to_string()
}

pub(crate) fn decide(
    inner: &Arc<Inner>,
    session: &str,
    id: &str,
    choice: ApprovalChoice,
    actor: &str,
) -> Result<ApprovalRequest, RuntimeError> {
    if actor.trim().is_empty() || actor.len() > 1024 {
        return Err(RuntimeError::InvalidInput(
            "approval actor must contain 1..1024 bytes".into(),
        ));
    }
    let _coordinate = inner.coordination.enter()?;
    let local = lock(&inner.active)
        .get(session)
        .map(|run| (run.task.clone(), run.cancel.clone(), run.unknown.clone()));
    if local.as_ref().is_some_and(|(_, cancel, unknown)| {
        cancel.is_cancelled() || unknown.load(std::sync::atomic::Ordering::Acquire)
    }) {
        return Err(RuntimeError::StaleApproval);
    }
    let view = crate::Runtime {
        inner: inner.clone(),
    }
    .get(session)?;
    if view.frozen || view.state == cayw_core::SessionState::NeedsReconciliation {
        return Err(RuntimeError::StaleApproval);
    }
    let lease = view.attempt.ok_or(RuntimeError::StaleApproval)?;
    let approval = view
        .pending_approvals
        .into_iter()
        .find(|approval| approval.id == id && approval.attempt_id == lease.attempt_id)
        .ok_or(RuntimeError::StaleApproval)?;
    let record = inner.queue.get(&approval.input.task_id)?;
    if record.state != crate::TaskState::Running
        || record.cancel_requested
        || record.session_id != session
        || record.attempt_id.as_deref() != Some(lease.attempt_id.as_str())
        || record.worker_id.as_deref() != Some(lease.worker_id.as_str())
    {
        return Err(RuntimeError::StaleApproval);
    }
    let task = crate::queue::TaskLease {
        id: record.id,
        worker: lease.worker_id.clone(),
        fence: record.fence,
    };
    if local.as_ref().is_some_and(|(owned, _, _)| {
        owned.id != task.id || owned.worker != task.worker || owned.fence != task.fence
    }) || inner.queue.check(&task).is_err()
    {
        return Err(RuntimeError::StaleApproval);
    }
    // Queue/Core facts alone may outlive a crashed worker. Its original task
    // lock must still be held; recovery uses lifecycle coordination as well.
    match inner.coordination.task(&task.id) {
        Err(RuntimeError::Busy) => {}
        Ok(_) => return Err(RuntimeError::StaleApproval),
        Err(error) => return Err(error),
    }
    inner.store.validate_lease(&lease)?;
    if inner.journal.decision_pending(&task.id)? {
        return Err(RuntimeError::RecoveryRequired {
            cause: Box::new(RuntimeError::Worker(
                "approval decision did not commit; explicit recovery required".into(),
            )),
            cleanup: None,
            outcome_unknown: true,
        });
    }
    let lifetime = u64::try_from(inner.config.grant_lifetime.as_millis()).unwrap_or(u64::MAX);
    // Persist intent before changing either approval projection. Failing before
    // intent cannot grant permission. Once it exists, only recovery may repair
    // an unfinished decision, even after public revocation.
    let decision = inner
        .journal
        .begin_decision(&approval, choice.as_str(), actor)?;
    let outcome = (|| {
        let grant = inner.journal.insert_grant(
            &approval,
            choice.as_str(),
            actor,
            timestamp_ms().saturating_add(lifetime),
        )?;
        let approved = choice != ApprovalChoice::Deny;
        let resolution = inner.store.decide_approval(
            session,
            id,
            if approved {
                ApprovalDecision::Approve
            } else {
                ApprovalDecision::Deny
            },
            actor,
        )?;
        inner.store.append_host_event(&lease,"approval_scope_decided",serde_json::json!({"task_id":task.id,"approval_id":id,"grant_id":grant.id,"choice":choice,"scope":approval.input.scope,"actor":actor}))?;
        inner
            .journal
            .activate_grant(&grant.id, approved, decision)?;
        Ok(resolution)
    })();
    let outcome = outcome.map_err(|cause| {
        if let Some((_, cancel, unknown)) = &local {
            unknown.store(true, std::sync::atomic::Ordering::Release);
            cancel.cancel();
        }
        // An independent controller cannot signal another process's tokens.
        // Persistently close admission and retain capacity for recovery instead.
        let cleanup = inner
            .queue
            .block(
                &task,
                "approval persistence failed; explicit recovery required",
            )
            .err();
        RuntimeError::RecoveryRequired {
            cause: Box::new(cause),
            cleanup: cleanup.map(Box::new),
            outcome_unknown: true,
        }
    });
    inner.approvals.notify_waiters();
    inner.changed.notify_waiters();
    outcome
}

pub(crate) struct Gate {
    pub scope: Arc<CallScope>,
}
impl Gate {
    pub(crate) async fn check(
        &self,
        name: &str,
        args: &Value,
        cancel: CancellationToken,
    ) -> Result<(), RuntimeError> {
        let context = &self.scope.context;
        context.check(&cancel)?;
        if name != self.scope.name || args != &self.scope.arguments {
            return Err(RuntimeError::InvalidInput(
                "permission target does not match its fixed capability".into(),
            ));
        }
        let inner = &context.inner;
        let scope = canonical_scope(args);
        if context
            .scope
            .permissions()
            .iter()
            .any(|grant| grant.capability == name && grant.scope == scope)
        {
            self.scope.authorize(Authorization {
                proof: self.scope.proof("runtime-config".into(), None),
                grant: None,
            })?;
            return context.check(&cancel);
        }
        if let Some(decision) = inner
            .journal
            .consume_grant(&context.lease, name, &scope, None)?
        {
            return self.accept(decision, None, &cancel);
        }
        let request = inner.store.request_approval(
            &context.lease,
            ApprovalInput {
                task_id: context.task.id.clone(),
                target: ApprovalTarget::CapabilityCall {
                    key: self.scope.key.clone(),
                },
                capability: name.into(),
                scope: scope.clone(),
                reason: format!(
                    "Execution {} call {} requires explicit permission for this exact capability",
                    self.scope.key.execution_id, self.scope.key.sequence
                ),
            },
        )?;
        inner.changed.notify_waiters();
        loop {
            let notified = inner.approvals.notified();
            context.check(&cancel)?;
            if let Some(decision) =
                inner
                    .journal
                    .consume_grant(&context.lease, name, &scope, Some(&request.id))?
            {
                return self.accept(decision, Some(request.id.clone()), &cancel);
            }
            let recorded = inner
                .store
                .approval(&context.lease.session_id, &request.id)?;
            if let Some(decision) = recorded.decision {
                let expired = timestamp_ms()
                    >= decision.decided_at.saturating_add(
                        u64::try_from(inner.config.grant_lifetime.as_millis()).unwrap_or(u64::MAX),
                    );
                if decision.decision == ApprovalDecision::Deny || expired {
                    return Err(RuntimeError::InvalidInput(
                        "capability permission denied or expired".into(),
                    ));
                }
            }
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(RuntimeError::Cancelled),
                _ = context.cancel.cancelled() => return Err(RuntimeError::Cancelled),
                _ = notified => {},
                _ = tokio::time::sleep(Duration::from_millis(25)) => {},
            }
        }
        // CallScope settles a cancelled permission wait as NotExecuted. Core
        // atomically closes its pending approval together with that terminal.
    }
    fn accept(
        &self,
        decision: GrantDecision,
        approval_id: Option<String>,
        cancel: &CancellationToken,
    ) -> Result<(), RuntimeError> {
        let grant = match decision {
            GrantDecision::Deny => {
                return Err(RuntimeError::InvalidInput(
                    "capability permission denied or revoked".into(),
                ));
            }
            GrantDecision::Allow(grant) => *grant,
        };
        self.scope.context.check(cancel)?;
        self.scope
            .context
            .host_event(
                "permission_admitted",
                serde_json::json!({
                    "key": self.scope.key, "task_id":self.scope.context.task.id,
                    "grant_id":grant.id, "approval_id":grant.approval_id, "grant":grant,
                }),
            )
            .map_err(|error| RuntimeError::Worker(error.to_string()))?;
        self.scope.authorize(Authorization {
            proof: self.scope.proof(grant.actor.clone(), approval_id),
            grant: Some(grant),
        })?;
        self.scope.context.check(cancel)
    }
}
