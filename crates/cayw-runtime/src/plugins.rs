use crate::{
    Runtime, RuntimeError,
    cells::CallScope,
    execution::RunContext,
    session::{BindingRecord, cursor_conflict},
};
use async_trait::async_trait;
use cayw_ai::{ArgumentValidator, CancellationToken, ToolDefinition};
use cayw_codemode::{CapabilityError, CapabilityHost};
use cayw_plugins::{Plugin, PluginContext, PluginDescription};
use serde_json::Value;
use std::{collections::BTreeMap, sync::Arc};

impl Runtime {
    /// Read-only plugin catalog. Lifecycle mutations go through Runtime so an
    /// unload cannot bypass the session's durable checkpoint and freeze.
    pub fn list_plugins(&self) -> Vec<PluginDescription> {
        self.inner.registry.list()
    }
    /// Exact call against a loaded description. Host tests use this for process
    /// operations that are not agent capabilities.
    pub async fn call_plugin(
        &self,
        expected: &PluginDescription,
        method: &str,
        arguments: Value,
        cancel: CancellationToken,
    ) -> Result<Value, RuntimeError> {
        self.inner
            .registry
            .call_exact(
                expected,
                method,
                arguments,
                cancel,
                self.inner.config.host.call_timeout,
            )
            .await
            .map_err(plugin_error)
    }
    pub async fn load_external(
        &self,
        config: cayw_plugins::ExternalConfig,
        context: PluginContext,
        cancel: CancellationToken,
    ) -> Result<PluginDescription, RuntimeError> {
        self.check_plugin_catalog(&config.expected_manifest)?;
        self.inner
            .registry
            .load_external(config, context, cancel)
            .await
            .map_err(plugin_error)
    }
    pub fn load_native(
        &self,
        plugin: Arc<dyn Plugin>,
        context: PluginContext,
    ) -> Result<PluginDescription, RuntimeError> {
        self.check_plugin_catalog(plugin.manifest())?;
        self.inner
            .registry
            .load_native(plugin, context)
            .map_err(plugin_error)
    }
    fn check_plugin_catalog(
        &self,
        manifest: &cayw_plugins::PluginManifest,
    ) -> Result<(), RuntimeError> {
        let catalog = self.capabilities()?;
        if manifest
            .capabilities
            .iter()
            .any(|cap| catalog.iter().any(|existing| existing.name == cap.name))
        {
            return Err(RuntimeError::InvalidInput(
                "plugin capability conflicts with the host catalog".into(),
            ));
        }
        Ok(())
    }
    /// Checkpoint the original obligations and freeze before unloading. An
    /// unavailable instance still freezes; a storage failure cancels execution.
    pub fn unload_plugin(&self, name: &str) -> Result<PluginDescription, RuntimeError> {
        let _coordinate = self.inner.coordination.enter()?;
        let description = self.inner.registry.describe(name).map_err(plugin_error)?;
        for session in self.inner.store.list_sessions()? {
            if self
                .binding_at(&session)?
                .is_some_and(|binding| binding.plugin() == name)
            {
                self.freeze_plugin_session(&session.id, name)?;
            }
        }
        let description = self
            .inner
            .registry
            .unload(name)
            .map_err(plugin_error)
            .map(|_| description)?;
        self.inner.changed.notify_waiters();
        Ok(description)
    }
    fn freeze_plugin_session(&self, session: &str, name: &str) -> Result<(), RuntimeError> {
        for _ in 0..32 {
            let snapshot = self.inner.store.snapshot(session)?;
            let Some(mut binding) = self.binding_at(&snapshot)? else {
                return Ok(());
            };
            if binding.plugin() != name || binding.reason.is_some() {
                return Ok(());
            }
            let reason = format!("plugin {name} was unloaded by the host");
            #[cfg(feature = "strongflow")]
            let reason = if binding.strongflow.is_some() {
                let mut state = binding.strongflow_binding()?;
                match state.checkpoint_strongflow(&self.inner.registry) {
                    Ok(_) => {
                        binding.strongflow = Some(serde_json::to_value(state)?);
                        reason
                    }
                    Err(error) => {
                        format!("plugin {name} unavailable before unload checkpoint: {error}")
                    }
                }
            } else {
                reason
            };
            binding.freeze(reason);
            match self.record_binding(&snapshot, "plugin_frozen", &binding, "runtime-host-unload") {
                Ok(()) => return self.cancel_frozen(session),
                Err(RuntimeError::Core(error))
                    if cursor_conflict(&error)
                        || matches!(error, cayw_core::CoreError::StaleLease) =>
                {
                    continue;
                }
                Err(error) => return Err(error),
            }
        }
        self.control_failed(session);
        Err(RuntimeError::Busy)
    }
    pub fn bind_plugin(
        &self,
        session: &str,
        name: &str,
    ) -> Result<PluginDescription, RuntimeError> {
        let _coordinate = self.inner.coordination.enter()?;
        let view = self.check_ready(session)?;
        self.ensure_no_task(session)?;
        if view.binding.is_some() {
            return Err(RuntimeError::InvalidInput(
                "session already has a plugin binding".into(),
            ));
        }
        let plugin = self
            .inner
            .registry
            .check_available(name)
            .map_err(plugin_error)?;
        let binding = BindingRecord {
            manifest: serde_json::to_value(&plugin.manifest)?,
            registry_id: plugin.registry_id.clone(),
            generation: plugin.generation,
            reason: None,
            strongflow: None,
        };
        #[cfg(feature = "strongflow")]
        let binding = if name == "strongflow" {
            let mut state =
                cayw_plugins::strongflow::SessionBinding::strongflow(session.into(), &plugin)
                    .map_err(plugin_error)?;
            state
                .checkpoint_strongflow(&self.inner.registry)
                .map_err(plugin_error)?;
            BindingRecord {
                strongflow: Some(serde_json::to_value(state)?),
                ..binding
            }
        } else {
            binding
        };
        #[cfg(not(feature = "strongflow"))]
        if name == "strongflow" {
            return Err(RuntimeError::InvalidInput(
                "StrongFlow support is disabled".into(),
            ));
        }
        let snapshot = self.inner.store.snapshot(session)?;
        self.record_binding(&snapshot, "plugin_bound", &binding, "runtime-host-bind")?;
        Ok(plugin)
    }
    fn ensure_no_task(&self, session: &str) -> Result<(), RuntimeError> {
        if self.inner.queue.list(session)?.iter().any(|task| {
            matches!(
                task.state,
                crate::TaskState::Queued | crate::TaskState::Running | crate::TaskState::Draining
            )
        }) {
            return Err(RuntimeError::Busy);
        }
        Ok(())
    }
    #[cfg(feature = "strongflow")]
    pub fn bind_strongflow(
        &self,
        session: &str,
    ) -> Result<cayw_plugins::strongflow::SessionBinding, RuntimeError> {
        self.bind_plugin(session, "strongflow")?;
        self.binding_at(&self.inner.store.snapshot(session)?)?
            .ok_or_else(|| RuntimeError::InvalidInput("session has no plugin binding".into()))?
            .strongflow_binding()
    }
    /// Trusted host checkpoint after plan/evidence changes. Unavailable or frozen
    /// bindings cannot use checkpointing to replace their original obligations.
    #[cfg(feature = "strongflow")]
    pub fn checkpoint_strongflow(
        &self,
        session: &str,
    ) -> Result<cayw_plugins::strongflow::SessionBinding, RuntimeError> {
        let _coordinate = self.inner.coordination.enter()?;
        self.check_ready(session)?;
        self.ensure_no_task(session)?;
        if self
            .binding_at(&self.inner.store.snapshot(session)?)?
            .is_none_or(|binding| binding.strongflow.is_none())
        {
            return Err(RuntimeError::InvalidInput(
                "session has no StrongFlow binding".into(),
            ));
        }
        self.checkpoint_strongflow_record(session, None)?;
        self.binding_at(&self.inner.store.snapshot(session)?)?
            .ok_or_else(|| RuntimeError::InvalidInput("session has no plugin binding".into()))?
            .strongflow_binding()
    }
    #[cfg(feature = "strongflow")]
    fn checkpoint_strongflow_record(
        &self,
        session: &str,
        expected_attempt: Option<&cayw_core::AttemptLease>,
    ) -> Result<(), RuntimeError> {
        for _ in 0..32 {
            let view = self.get(session)?;
            if view.frozen {
                return Err(RuntimeError::Frozen(
                    view.binding
                        .and_then(|binding| binding.reason)
                        .unwrap_or_default(),
                ));
            }
            let snapshot = self.inner.store.snapshot(session)?;
            if snapshot.attempt.as_ref() != expected_attempt {
                return Err(cayw_core::CoreError::StaleLease.into());
            }
            let Some(mut binding) = self.binding_at(&snapshot)? else {
                return Ok(());
            };
            if binding.strongflow.is_none() {
                return Ok(());
            }
            let mut state = binding.strongflow_binding()?;
            if let Err(error) = state.checkpoint_strongflow(&self.inner.registry) {
                binding.freeze(format!("StrongFlow checkpoint failed: {error}"));
                self.record_binding(
                    &snapshot,
                    "plugin_frozen",
                    &binding,
                    "runtime-host-checkpoint",
                )?;
                self.cancel_frozen(session)?;
                return Err(RuntimeError::Frozen(error.to_string()));
            }
            let next = serde_json::to_value(state)?;
            binding.strongflow = Some(next);
            match self.record_binding(
                &snapshot,
                "plugin_checkpoint",
                &binding,
                "runtime-host-checkpoint",
            ) {
                Ok(()) => return Ok(()),
                Err(RuntimeError::Core(error)) if cursor_conflict(&error) => continue,
                Err(error) => return Err(error),
            }
        }
        self.control_failed(session);
        Err(RuntimeError::Busy)
    }
    /// Explicit host resume after restoring the full checkpoint into the exact
    /// target instance returned by load. This is deliberately absent from RPC.
    #[cfg(feature = "strongflow")]
    pub fn resume_strongflow(
        &self,
        session: &str,
        target: &PluginDescription,
    ) -> Result<cayw_plugins::strongflow::SessionBinding, RuntimeError> {
        let _coordinate = self.inner.coordination.enter()?;
        self.get(session)?;
        self.ensure_no_task(session)?;
        let snapshot = self.inner.store.snapshot(session)?;
        if snapshot.attempt.is_some() {
            return Err(RuntimeError::Busy);
        }
        if snapshot.state == cayw_core::SessionState::NeedsReconciliation {
            return Err(RuntimeError::NeedsReconciliation);
        }
        let mut binding = self
            .binding_at(&snapshot)?
            .ok_or_else(|| RuntimeError::InvalidInput("session has no plugin binding".into()))?;
        let mut state = binding.strongflow_binding()?;
        state
            .resume_strongflow(&self.inner.registry, target)
            .map_err(plugin_error)?;
        binding.registry_id = target.registry_id.clone();
        binding.generation = target.generation;
        binding.reason = None;
        binding.strongflow = Some(serde_json::to_value(&state)?);
        self.record_binding(&snapshot, "plugin_resumed", &binding, "runtime-host-resume")?;
        Ok(state)
    }
}

fn plugin_error(error: cayw_plugins::PluginError) -> RuntimeError {
    RuntimeError::Workspace(error.to_string())
}

struct PluginCapability {
    plugin: String,
    description: PluginDescription,
    definition: ToolDefinition,
    validator: ArgumentValidator,
}
/// Captured once for the task; later calls cannot select a newly loaded plugin.
pub(crate) struct PluginCatalog {
    capabilities: BTreeMap<String, PluginCapability>,
}
impl PluginCatalog {
    pub(crate) fn new(context: &RunContext) -> Result<Self, RuntimeError> {
        let mut capabilities = BTreeMap::new();
        let rt = Runtime {
            inner: context.inner.clone(),
        };
        let view = rt.get(&context.lease.session_id)?;
        if view.frozen {
            return Err(RuntimeError::Frozen(
                view.binding
                    .and_then(|binding| binding.reason)
                    .unwrap_or_default(),
            ));
        }
        if let Some(binding) =
            rt.binding_at(&context.inner.store.snapshot(&context.lease.session_id)?)?
        {
            let plugin = binding.description()?;
            for definition in plugin.manifest.capabilities.iter().cloned() {
                let validator = ArgumentValidator::new(&definition.parameters)
                    .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?;
                capabilities.insert(
                    definition.name.clone(),
                    PluginCapability {
                        plugin: binding.plugin().into(),
                        description: plugin.clone(),
                        definition,
                        validator,
                    },
                );
            }
        }
        Ok(Self { capabilities })
    }
    pub(crate) fn definitions(&self) -> Vec<ToolDefinition> {
        self.capabilities
            .values()
            .map(|cap| cap.definition.clone())
            .collect()
    }
}
pub(crate) struct PluginHost {
    host: cayw_host::Host,
    scope: Arc<CallScope>,
    catalog: Arc<PluginCatalog>,
}
impl PluginHost {
    pub(crate) fn new(
        host: cayw_host::Host,
        scope: Arc<CallScope>,
        catalog: Arc<PluginCatalog>,
    ) -> Self {
        Self {
            host,
            scope,
            catalog,
        }
    }
}
#[async_trait]
impl CapabilityHost for PluginHost {
    fn capabilities(&self) -> Vec<ToolDefinition> {
        let mut catalog = self.host.capabilities();
        catalog.extend(self.catalog.definitions());
        catalog
    }
    async fn call(
        &self,
        name: &str,
        args: Value,
        cancel: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        let Some(cap) = self.catalog.capabilities.get(name) else {
            return self.host.call(name, args, cancel).await;
        };
        self.host
            .call_service(
                &cap.definition,
                args,
                cancel,
                &PluginService {
                    scope: &self.scope,
                    cap,
                },
            )
            .await
    }
}
struct PluginService<'a> {
    scope: &'a Arc<CallScope>,
    cap: &'a PluginCapability,
}
#[async_trait]
impl cayw_host::ExternalHost for PluginService<'_> {
    async fn call(
        &self,
        name: &str,
        args: Value,
        cancel: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        let cap = self.cap;
        let context = &self.scope.context;
        self.scope.check_authority(&cancel)?;
        cap.validator
            .validate(&args)
            .map_err(|error| CapabilityError::new(error.to_string()))?;
        let current = context
            .inner
            .registry
            .check_available(&cap.plugin)
            .map_err(|error| CapabilityError::new(error.to_string()))?;
        if current != cap.description {
            return Err(CapabilityError::new(
                "plugin identity, generation or manifest changed",
            ));
        }
        context
            .check(&cancel)
            .map_err(|error| CapabilityError::new(error.to_string()))?;
        let result = context
            .inner
            .registry
            .call_exact(
                &cap.description,
                name,
                args,
                cancel,
                context.inner.config.host.call_timeout,
            )
            .await;
        // Preserve the full workflow obligations before releasing an effect.
        #[cfg(feature = "strongflow")]
        if cap.plugin == "strongflow" {
            let rt = Runtime {
                inner: context.inner.clone(),
            };
            if let Err(error) =
                rt.checkpoint_strongflow_record(&context.lease.session_id, Some(&context.lease))
            {
                return Err(context.persistence_failed(error));
            }
        }
        result.map_err(|error| {
            if error.outcome_unknown() {
                CapabilityError::unknown(error.to_string())
            } else {
                CapabilityError::new(error.to_string())
            }
        })
    }
}
