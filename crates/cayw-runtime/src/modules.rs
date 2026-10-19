use crate::{
    Runtime, RuntimeError,
    cells::CallScope,
    execution::RunContext,
    session::{BindingRecord, cursor_conflict},
};
use async_trait::async_trait;
use cayw_ai::{ArgumentValidator, CancellationToken, ToolDefinition};
use cayw_codemode::{CapabilityError, CapabilityHost};
use cayw_modules::{Module, ModuleContext, ModuleDescription};
use serde_json::Value;
use std::{collections::BTreeMap, sync::Arc};

impl Runtime {
    /// Read-only module catalog. Lifecycle mutations go through Runtime so an
    /// unload cannot bypass the session's durable checkpoint and freeze.
    pub fn list_modules(&self) -> Vec<ModuleDescription> {
        self.inner.registry.list()
    }
    pub async fn load_external(
        &self,
        config: cayw_modules::ExternalConfig,
        context: ModuleContext,
        cancel: CancellationToken,
    ) -> Result<ModuleDescription, RuntimeError> {
        self.check_module_catalog(&config.expected_manifest)?;
        self.inner
            .registry
            .load_external(config, context, cancel)
            .await
            .map_err(module_error)
    }
    pub fn load_native(
        &self,
        module: Arc<dyn Module>,
        context: ModuleContext,
    ) -> Result<ModuleDescription, RuntimeError> {
        self.check_module_catalog(module.manifest())?;
        self.inner
            .registry
            .load_native(module, context)
            .map_err(module_error)
    }
    fn check_module_catalog(
        &self,
        manifest: &cayw_modules::ModuleManifest,
    ) -> Result<(), RuntimeError> {
        let catalog = self.capabilities()?;
        if manifest
            .capabilities
            .iter()
            .any(|cap| catalog.iter().any(|existing| existing.name == cap.name))
        {
            return Err(RuntimeError::InvalidInput(
                "module capability conflicts with the host catalog".into(),
            ));
        }
        Ok(())
    }
    /// Checkpoint the original obligations and freeze before unloading. An
    /// unavailable instance still freezes; a storage failure cancels execution.
    pub fn unload_module(&self, name: &str) -> Result<ModuleDescription, RuntimeError> {
        let _coordinate = self.inner.coordination.enter()?;
        let description = self.inner.registry.describe(name).map_err(module_error)?;
        for session in self.inner.store.list_sessions()? {
            if self
                .binding_at(&session)?
                .is_some_and(|binding| binding.module() == name)
            {
                self.freeze_module_session(&session.id, name)?;
            }
        }
        let description = self
            .inner
            .registry
            .unload(name)
            .map_err(module_error)
            .map(|_| description)?;
        self.inner.changed.notify_waiters();
        Ok(description)
    }
    fn freeze_module_session(&self, session: &str, name: &str) -> Result<(), RuntimeError> {
        for _ in 0..32 {
            let snapshot = self.inner.store.snapshot(session)?;
            let Some(mut binding) = self.binding_at(&snapshot)? else {
                return Ok(());
            };
            if binding.module() != name || binding.reason.is_some() {
                return Ok(());
            }
            let reason = format!("module {name} was unloaded by the host");
            #[cfg(feature = "strongflow")]
            let reason = if binding.strongflow.is_some() {
                let mut state = binding.strongflow_binding()?;
                match state.checkpoint_strongflow(&self.inner.registry) {
                    Ok(_) => {
                        binding.strongflow = Some(serde_json::to_value(state)?);
                        reason
                    }
                    Err(error) => {
                        format!("module {name} unavailable before unload checkpoint: {error}")
                    }
                }
            } else {
                reason
            };
            binding.freeze(reason);
            match self.record_binding(&snapshot, "module_frozen", &binding, "runtime-host-unload") {
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
    pub fn bind_module(
        &self,
        session: &str,
        name: &str,
    ) -> Result<ModuleDescription, RuntimeError> {
        let _coordinate = self.inner.coordination.enter()?;
        let view = self.check_ready(session)?;
        self.ensure_no_task(session)?;
        if view.binding.is_some() {
            return Err(RuntimeError::InvalidInput(
                "session already has a module binding".into(),
            ));
        }
        let module = self
            .inner
            .registry
            .check_available(name)
            .map_err(module_error)?;
        let binding = BindingRecord {
            manifest: serde_json::to_value(&module.manifest)?,
            registry_id: module.registry_id.clone(),
            generation: module.generation,
            reason: None,
            strongflow: None,
        };
        #[cfg(feature = "strongflow")]
        let binding = if name == "strongflow" {
            let mut state =
                cayw_modules::strongflow::SessionBinding::strongflow(session.into(), &module)
                    .map_err(module_error)?;
            state
                .checkpoint_strongflow(&self.inner.registry)
                .map_err(module_error)?;
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
        self.record_binding(&snapshot, "module_bound", &binding, "runtime-host-bind")?;
        Ok(module)
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
    ) -> Result<cayw_modules::strongflow::SessionBinding, RuntimeError> {
        self.bind_module(session, "strongflow")?;
        self.binding_at(&self.inner.store.snapshot(session)?)?
            .ok_or_else(|| RuntimeError::InvalidInput("session has no module binding".into()))?
            .strongflow_binding()
    }
    /// Trusted host checkpoint after plan/evidence changes. Unavailable or frozen
    /// bindings cannot use checkpointing to replace their original obligations.
    #[cfg(feature = "strongflow")]
    pub fn checkpoint_strongflow(
        &self,
        session: &str,
    ) -> Result<cayw_modules::strongflow::SessionBinding, RuntimeError> {
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
            .ok_or_else(|| RuntimeError::InvalidInput("session has no module binding".into()))?
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
                    "module_frozen",
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
                "module_checkpoint",
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
        target: &ModuleDescription,
    ) -> Result<cayw_modules::strongflow::SessionBinding, RuntimeError> {
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
            .ok_or_else(|| RuntimeError::InvalidInput("session has no module binding".into()))?;
        let mut state = binding.strongflow_binding()?;
        state
            .resume_strongflow(&self.inner.registry, target)
            .map_err(module_error)?;
        binding.registry_id = target.registry_id.clone();
        binding.generation = target.generation;
        binding.reason = None;
        binding.strongflow = Some(serde_json::to_value(&state)?);
        self.record_binding(&snapshot, "module_resumed", &binding, "runtime-host-resume")?;
        Ok(state)
    }
}

fn module_error(error: cayw_modules::ModuleError) -> RuntimeError {
    RuntimeError::Workspace(error.to_string())
}

struct ModuleCapability {
    module: String,
    description: ModuleDescription,
    definition: ToolDefinition,
    validator: ArgumentValidator,
}
/// Captured once for the task; later calls cannot select a newly loaded module.
pub(crate) struct ModuleCatalog {
    capabilities: BTreeMap<String, ModuleCapability>,
}
impl ModuleCatalog {
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
            let module = binding.description()?;
            for definition in module.manifest.capabilities.iter().cloned() {
                let validator = ArgumentValidator::new(&definition.parameters)
                    .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?;
                capabilities.insert(
                    definition.name.clone(),
                    ModuleCapability {
                        module: binding.module().into(),
                        description: module.clone(),
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
pub(crate) struct ModuleHost {
    host: cayw_host::Host,
    scope: Arc<CallScope>,
    catalog: Arc<ModuleCatalog>,
}
impl ModuleHost {
    pub(crate) fn new(
        host: cayw_host::Host,
        scope: Arc<CallScope>,
        catalog: Arc<ModuleCatalog>,
    ) -> Self {
        Self {
            host,
            scope,
            catalog,
        }
    }
}
#[async_trait]
impl CapabilityHost for ModuleHost {
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
                &ModuleService {
                    scope: &self.scope,
                    cap,
                },
            )
            .await
    }
}
struct ModuleService<'a> {
    scope: &'a Arc<CallScope>,
    cap: &'a ModuleCapability,
}
#[async_trait]
impl cayw_host::ExternalHost for ModuleService<'_> {
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
            .check_available(&cap.module)
            .map_err(|error| CapabilityError::new(error.to_string()))?;
        if current != cap.description {
            return Err(CapabilityError::new(
                "module identity, generation or manifest changed",
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
        if cap.module == "strongflow" {
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
