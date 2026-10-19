//! Runtime session state is a projection of the Core log. Plugin liveness can
//! freeze a binding, but a newly loaded instance can never revive it implicitly.
use crate::{Runtime, RuntimeError, lock};
use cayw_core::{
    ApprovalRequest, AttemptLease, CallRecord, CoreError, CoreEvent, SessionSnapshot, SessionState,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The single SDK and RPC session result. Core fields stay at the top level.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionView {
    pub id: String,
    pub label: String,
    pub messages: Vec<cayw_ai::Message>,
    pub cursor: u64,
    pub state: SessionState,
    pub attempt: Option<AttemptLease>,
    pub pending_approvals: Vec<ApprovalRequest>,
    pub unresolved_calls: Vec<CallRecord>,
    pub executions: Vec<cayw_core::ExecutionRecord>,
    pub unresolved_capabilities: Vec<cayw_core::CapabilityCallRecord>,
    pub frozen: bool,
    pub binding: Option<SessionBindingView>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionBindingView {
    pub plugin: String,
    pub version: String,
    pub registry_id: String,
    pub generation: u64,
    pub manifest: Value,
    pub frozen: bool,
    pub reason: Option<String>,
    /// Full plans, statuses and host-verified evidence from the durable checkpoint.
    pub workflow_state: Option<Value>,
}

/// Portable even when optional plugin support is disabled. The plugin's complete
/// SessionBinding is preserved as data, never reconstructed from a name/version.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BindingRecord {
    pub manifest: Value,
    pub registry_id: String,
    pub generation: u64,
    pub reason: Option<String>,
    pub strongflow: Option<Value>,
}
impl BindingRecord {
    pub(crate) fn plugin(&self) -> &str {
        self.manifest["name"].as_str().unwrap_or_default()
    }
    fn validate(&self, session: &str) -> Result<(), RuntimeError> {
        if self.plugin().is_empty()
            || self.manifest["version"]
                .as_str()
                .is_none_or(|v| v.trim().is_empty())
            || uuid::Uuid::parse_str(&self.registry_id).is_err()
            || self.generation == 0
            || self
                .reason
                .as_ref()
                .is_some_and(|reason| reason.trim().is_empty())
            || (self.plugin() == "strongflow") != self.strongflow.is_some()
        {
            return Err(CoreError::CorruptStore("invalid canonical plugin binding".into()).into());
        }
        if let Some(binding) = &self.strongflow
            && (binding["session_id"] != session
                || binding["plugin_manifest"] != self.manifest
                || binding["bound_plugin_version"]["registry_id"] != self.registry_id
                || binding["bound_plugin_version"]["generation"] != self.generation
                || binding["frozen_reason"] != serde_json::to_value(&self.reason)?
                || !binding["workflow_state"].is_object())
        {
            return Err(
                CoreError::CorruptStore("incomplete StrongFlow binding checkpoint".into()).into(),
            );
        }
        #[cfg(feature = "strongflow")]
        if self.strongflow.is_some() {
            self.strongflow_binding()?.validate().map_err(|error| {
                CoreError::CorruptStore(format!("invalid StrongFlow checkpoint: {error}"))
            })?;
        }
        Ok(())
    }
    pub(crate) fn freeze(&mut self, reason: String) {
        self.reason = Some(reason.clone());
        if let Some(binding) = self.strongflow.as_mut() {
            binding["frozen_reason"] = Value::String(reason);
        }
    }
    fn view(&self) -> SessionBindingView {
        SessionBindingView {
            plugin: self.plugin().into(),
            version: self.manifest["version"].as_str().unwrap_or_default().into(),
            registry_id: self.registry_id.clone(),
            generation: self.generation,
            manifest: self.manifest.clone(),
            frozen: self.reason.is_some(),
            reason: self.reason.clone(),
            workflow_state: self
                .strongflow
                .as_ref()
                .map(|state| state["workflow_state"].clone()),
        }
    }
    #[cfg(feature = "plugins")]
    pub(crate) fn description(&self) -> Result<cayw_plugins::PluginDescription, RuntimeError> {
        Ok(cayw_plugins::PluginDescription {
            manifest: serde_json::from_value(self.manifest.clone())?,
            registry_id: self.registry_id.clone(),
            generation: self.generation,
        })
    }
    #[cfg(feature = "strongflow")]
    pub(crate) fn strongflow_binding(
        &self,
    ) -> Result<cayw_plugins::strongflow::SessionBinding, RuntimeError> {
        serde_json::from_value(self.strongflow.clone().ok_or_else(|| {
            RuntimeError::InvalidInput("session has no StrongFlow binding".into())
        })?)
        .map_err(Into::into)
    }
    fn unavailable_reason(&self, runtime: &Runtime) -> Option<String> {
        if self.reason.is_some() {
            return None;
        }
        #[cfg(feature = "plugins")]
        {
            let result = runtime.inner.registry.check_available(self.plugin());
            let expected = self.description();
            match (result, expected) {
                (Ok(actual), Ok(expected)) if actual == expected => {
                    #[cfg(not(feature = "strongflow"))]
                    if self.strongflow.is_some() {
                        return Some("StrongFlow support is disabled in this runtime".into());
                    }
                    None
                }
                (Err(error), _) => Some(format!("{} is unavailable: {error}", self.plugin())),
                _ => Some(format!(
                    "{} registry identity, generation or manifest changed",
                    self.plugin()
                )),
            }
        }
        #[cfg(not(feature = "plugins"))]
        {
            let _ = runtime;
            Some(format!(
                "{} plugin support is disabled in this runtime",
                self.plugin()
            ))
        }
    }
}
impl SessionView {
    fn from_snapshot(snapshot: SessionSnapshot, binding: Option<BindingRecord>) -> Self {
        let binding = binding.map(|binding| binding.view());
        Self {
            id: snapshot.id,
            label: snapshot.label,
            messages: snapshot.messages,
            cursor: snapshot.cursor,
            state: snapshot.state,
            attempt: snapshot.attempt,
            pending_approvals: snapshot.pending_approvals,
            unresolved_calls: snapshot.unresolved_calls,
            executions: snapshot.executions,
            unresolved_capabilities: snapshot.unresolved_capabilities,
            frozen: binding.as_ref().is_some_and(|binding| binding.frozen),
            binding,
        }
    }
}

impl Runtime {
    pub(crate) fn binding_at(
        &self,
        snapshot: &SessionSnapshot,
    ) -> Result<Option<BindingRecord>, RuntimeError> {
        let mut cursor = 0;
        let mut binding = None;
        while cursor < snapshot.cursor {
            let limit = (snapshot.cursor - cursor).min(cayw_core::MAX_EVENT_PAGE as u64) as usize;
            for record in self.inner.store.events(&snapshot.id, cursor, limit)? {
                cursor = record.cursor;
                // Decisions recorded before the module -> plugin rename are not
                // replayed; skipping them would silently thaw a frozen session.
                if let CoreEvent::Host { kind, .. } = &record.event
                    && kind.starts_with("module_")
                {
                    return Err(CoreError::MigrationRequired(format!(
                        "session {} has a pre-rename {kind} binding decision; explicit host migration is required",
                        snapshot.id
                    ))
                    .into());
                }
                if let CoreEvent::Host { kind, data } = record.event
                    && matches!(
                        kind.as_str(),
                        "plugin_bound" | "plugin_checkpoint" | "plugin_frozen" | "plugin_resumed"
                    )
                {
                    if data["actor"]
                        .as_str()
                        .is_none_or(|actor| actor.trim().is_empty())
                    {
                        return Err(CoreError::CorruptStore(
                            "plugin decision has no host actor".into(),
                        )
                        .into());
                    }
                    let next: BindingRecord =
                        serde_json::from_value(data["decision"]["binding"].clone())?;
                    next.validate(&snapshot.id)?;
                    binding = Some(next);
                }
            }
        }
        Ok(binding)
    }

    pub(crate) fn record_binding(
        &self,
        snapshot: &SessionSnapshot,
        kind: &str,
        binding: &BindingRecord,
        actor: &str,
    ) -> Result<(), RuntimeError> {
        binding.validate(&snapshot.id)?;
        let result = self.inner.store.append_control_event(
            &snapshot.id,
            snapshot.cursor,
            snapshot.attempt.as_ref(),
            actor,
            kind,
            serde_json::json!({"binding":binding}),
        );
        match result {
            Ok(_) => {
                self.inner.changed.notify_waiters();
                Ok(())
            }
            Err(error) => {
                // A failed lifecycle write cannot allow an active worker to keep
                // spending an approval against an unrecorded plugin decision.
                if !cursor_conflict(&error) && !matches!(error, CoreError::StaleLease) {
                    self.control_failed(&snapshot.id);
                }
                Err(error.into())
            }
        }
    }

    pub(crate) fn control_failed(&self, session: &str) {
        if let Some(run) = lock(&self.inner.active).get(session) {
            run.unknown
                .store(true, std::sync::atomic::Ordering::Release);
            run.cancel.cancel();
        }
        let _ = self.inner.queue.cancel(session);
        self.inner.approvals.notify_waiters();
        self.inner.changed.notify_waiters();
    }

    pub(crate) fn cancel_frozen(&self, session: &str) -> Result<(), RuntimeError> {
        if let Some(run) = lock(&self.inner.active).get(session) {
            run.cancel.cancel();
        }
        self.inner.queue.cancel(session)?;
        self.inner
            .journal
            .revoke_session(session, "plugin binding frozen")?;
        self.inner.approvals.notify_waiters();
        self.inner.changed.notify_waiters();
        Ok(())
    }

    pub(crate) fn session_view(&self, id: &str) -> Result<SessionView, RuntimeError> {
        // Optimistic retries preserve lease/cursor checks without holding a
        // Runtime lifecycle lock across an SDK read or an active worker write.
        for _ in 0..32 {
            let snapshot = self.inner.store.snapshot(id)?;
            let mut binding = self.binding_at(&snapshot)?;
            if let Some(current) = &mut binding
                && let Some(reason) = current.unavailable_reason(self)
            {
                current.freeze(reason);
                match self.record_binding(
                    &snapshot,
                    "plugin_frozen",
                    current,
                    "runtime-plugin-lifecycle",
                ) {
                    Ok(()) => {
                        self.cancel_frozen(id)?;
                        continue;
                    }
                    Err(RuntimeError::Core(error))
                        if cursor_conflict(&error) || matches!(error, CoreError::StaleLease) =>
                    {
                        continue;
                    }
                    Err(error) => return Err(error),
                }
            }
            let mut view = SessionView::from_snapshot(snapshot, binding);
            if self
                .inner
                .cells
                .is_frozen(id)
                .map_err(|error| RuntimeError::Workspace(error.to_string()))?
            {
                // A Core commit alone cannot make a partially repaired Cells
                // projection ready for another prompt in either SDK or UI.
                view.state = SessionState::NeedsReconciliation;
            }
            return Ok(view);
        }
        Err(RuntimeError::Busy)
    }
}

pub(crate) fn cursor_conflict(error: &CoreError) -> bool {
    matches!(error, CoreError::InvalidEvent(reason) if reason == "Host decision requires the current session cursor")
}
