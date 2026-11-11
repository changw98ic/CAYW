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

    /// The only surface text a user may send for this choice. The answer is
    /// matched exactly and never passed through a model.
    pub fn reply(self) -> &'static str {
        match self {
            Self::AllowOnce => ALLOW_ONCE_REPLY,
            Self::AllowTask => ALLOW_TASK_REPLY,
            Self::Deny => DENY_REPLY,
        }
    }

    /// Exact option matching. Partial text, mixed wording and model-authored
    /// "approval" strings do not match.
    pub fn from_reply(reply: &str) -> Option<Self> {
        match reply {
            ALLOW_ONCE_REPLY => Some(Self::AllowOnce),
            ALLOW_TASK_REPLY => Some(Self::AllowTask),
            DENY_REPLY => Some(Self::Deny),
            _ => None,
        }
    }
}

pub const ALLOW_ONCE_REPLY: &str = "允许一次";
pub const ALLOW_TASK_REPLY: &str = "本任务允许";
pub const DENY_REPLY: &str = "拒绝";

/// One approval shown in the main-session conversation. `text` already contains
/// the question, the exact capability scope and the three reply options.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ApprovalQuestion {
    pub session_id: String,
    pub approval_id: String,
    pub task_id: String,
    pub capability: String,
    pub scope: String,
    pub reason: String,
    pub approval_mode: crate::ApprovalMode,
    pub text: String,
    pub options: Vec<&'static str>,
}

pub(crate) fn question(
    approval: &ApprovalRequest,
    approval_mode: crate::ApprovalMode,
) -> ApprovalQuestion {
    let options = match approval_mode {
        crate::ApprovalMode::Explicit => vec![
            ApprovalChoice::AllowOnce.reply(),
            ApprovalChoice::AllowTask.reply(),
            ApprovalChoice::Deny.reply(),
        ],
        crate::ApprovalMode::ModelAuto => Vec::new(),
    };
    let text = match approval_mode {
        crate::ApprovalMode::Explicit => format!(
            "请求你的授权：{}。参数：{}。原因：{}。请只回复以下选项之一：{} / {} / {}",
            approval.input.capability,
            approval.input.scope,
            approval.input.reason,
            options[0],
            options[1],
            options[2],
        ),
        crate::ApprovalMode::ModelAuto => format!(
            "模型正在自动评估授权：{}。参数：{}。原因：{}。",
            approval.input.capability, approval.input.scope, approval.input.reason,
        ),
    };
    ApprovalQuestion {
        session_id: approval.session_id.clone(),
        approval_id: approval.id.clone(),
        task_id: approval.input.task_id.clone(),
        capability: approval.input.capability.clone(),
        scope: approval.input.scope.clone(),
        reason: approval.input.reason.clone(),
        approval_mode,
        text,
        options,
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
    if view.frozen {
        return Err(RuntimeError::StaleApproval);
    }
    let lease = view.attempt.ok_or(RuntimeError::StaleApproval)?;
    let approval = view
        .pending_approvals
        .into_iter()
        .find(|approval| approval.id == id && approval.attempt_id == lease.attempt_id)
        .ok_or(RuntimeError::StaleApproval)?;
    let record = inner.queue.get(&approval.input.task_id)?;
    if !matches!(
        record.state,
        crate::TaskState::Running | crate::TaskState::WaitingApproval | crate::TaskState::Queued
    ) || record.cancel_requested
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
            cleanup: cleanup.map(|error| Box::new(RuntimeError::from(error))),
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
        // The approval rules file is user-owned configuration. Even an exact
        // RuntimeConfig permission, a durable grant, or a user rule must never
        // let a capability rewrite the file that decides future releases.
        if crate::approval_rules::is_workspace_mutation(name)
            && let Some(protected) = inner
                .config
                .approval_rules
                .as_ref()
                .and_then(|source| source.protected_path())
            && crate::approval_rules::mutation_targets_protected(name, args, &inner.root, protected)
        {
            return Err(RuntimeError::InvalidInput(
                "the user-owned approval rules file cannot be changed by a capability".into(),
            ));
        }
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
        if !inner
            .journal
            .blocks_session_result_read(&context.lease.session_id, name, &scope)?
            && crate::results::is_session_result_read(inner, &context.lease.session_id, name, args)
        {
            self.scope.authorize(Authorization {
                proof: self.scope.proof("session-results".into(), None),
                grant: None,
            })?;
            return context.check(&cancel);
        }
        let rules = context.scope.rules();
        let recovery_scope = inner.recovery_scope();
        let reason = match rules.evaluate(
            name,
            args,
            &crate::approval_rules::RuleContext {
                workspace: &inner.root,
                recovery: recovery_scope.as_ref(),
                main_session: inner.config.main_session,
                notification_targets: &inner.config.notification_targets,
            },
        ) {
            crate::approval_rules::RuleDecision::Allow {
                kind,
                rule_version,
                rule_index,
            } => {
                context
                    .host_event(
                        "permission_rule_allowed",
                        serde_json::json!({
                            "task_id": context.task.id,
                            "key": self.scope.key,
                            "capability": name,
                            "scope": scope,
                            "rule": kind,
                            "rule_version": rule_version,
                            "rule_index": rule_index,
                        }),
                    )
                    .map_err(|error| RuntimeError::Worker(error.to_string()))?;
                self.scope.authorize(Authorization {
                    proof: self.scope.proof("approval-rule".into(), None),
                    grant: None,
                })?;
                return context.check(&cancel);
            }
            crate::approval_rules::RuleDecision::Ask { reason }
                if !matches!(rules, crate::approval_rules::RulesSnapshot::ExplicitOnly) =>
            {
                reason.to_string()
            }
            crate::approval_rules::RuleDecision::Ask { .. } => format!(
                "Execution {} call {} requires explicit permission for this exact capability",
                self.scope.key.execution_id, self.scope.key.sequence
            ),
        };
        let request = inner.store.request_approval(
            &context.lease,
            ApprovalInput {
                task_id: context.task.id.clone(),
                target: ApprovalTarget::CapabilityCall {
                    key: self.scope.key.clone(),
                },
                capability: name.into(),
                scope: scope.clone(),
                reason,
            },
        )?;
        if self.scope.has_started() {
            return Err(RuntimeError::Worker(
                "approval cannot release a started capability".into(),
            ));
        }
        context
            .activity
            .waiting(context, &self.scope.key.execution_id)?;
        inner.changed.notify_waiters();
        loop {
            let notified = inner.approvals.notified();
            context.check(&cancel)?;
            if let Some(decision) =
                inner
                    .journal
                    .consume_grant(&context.lease, name, &scope, Some(&request.id))?
            {
                context
                    .activity
                    .resume_cell(context, &self.scope.key.execution_id, &cancel)
                    .await?;
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
                    context
                        .activity
                        .resume_cell(context, &self.scope.key.execution_id, &cancel)
                        .await?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_exact_option_text_maps_to_a_choice() {
        assert_eq!(
            ApprovalChoice::from_reply("允许一次"),
            Some(ApprovalChoice::AllowOnce)
        );
        assert_eq!(
            ApprovalChoice::from_reply("本任务允许"),
            Some(ApprovalChoice::AllowTask)
        );
        assert_eq!(
            ApprovalChoice::from_reply("拒绝"),
            Some(ApprovalChoice::Deny)
        );
        for reply in [
            "允许",
            "批准",
            "已批准",
            "允许一次 ",
            " 允许一次",
            "允许此任务",
            "本任务允许。",
            "allow_once",
            "",
        ] {
            assert_eq!(ApprovalChoice::from_reply(reply), None, "{reply:?}");
        }
    }
}
