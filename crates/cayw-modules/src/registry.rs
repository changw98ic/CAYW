use crate::*;
use cayw_ai::ArgumentValidator;
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct Entry {
    module: Arc<dyn Module>,
    manifest: ModuleManifest,
    validators: BTreeMap<String, ArgumentValidator>,
    context: ModuleContext,
    generation: u64,
    cancel: CancellationToken,
}

impl Drop for Entry {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

#[derive(Default)]
struct State {
    entries: BTreeMap<String, Arc<Entry>>,
    next_generation: u64,
}

/// Independent modules can run concurrently. No registry lock is held across
/// user code or await. Each ExternalModule serializes its own wire requests.
pub struct ModuleRegistry {
    state: Mutex<State>,
    registry_id: String,
}

impl Default for ModuleRegistry {
    fn default() -> Self {
        Self {
            state: Mutex::default(),
            registry_id: uuid::Uuid::new_v4().to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModuleDescription {
    pub manifest: ModuleManifest,
    pub registry_id: String,
    pub generation: u64,
}

impl ModuleRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn load_native(
        &self,
        module: Arc<dyn Module>,
        context: ModuleContext,
    ) -> Result<ModuleDescription, ModuleError> {
        if module.manifest().kind != ModuleKind::Native {
            return Err(ModuleError::InvalidManifest(
                "expected native module".into(),
            ));
        }
        self.insert(module, context)
    }

    pub async fn load_external(
        &self,
        config: ExternalConfig,
        context: ModuleContext,
        cancel: CancellationToken,
    ) -> Result<ModuleDescription, ModuleError> {
        let module = ExternalModule::spawn(config, cancel).await?;
        self.insert(Arc::new(module), context)
    }

    fn insert(
        &self,
        module: Arc<dyn Module>,
        context: ModuleContext,
    ) -> Result<ModuleDescription, ModuleError> {
        let manifest = module.manifest().clone();
        let validators = validate_manifest(&manifest)?;
        let mut state = self.state.lock().expect("module registry poisoned");
        if state.entries.contains_key(&manifest.name) {
            return Err(ModuleError::Conflict(manifest.name));
        }
        for other in state.entries.values() {
            if let Some(name) = validators
                .keys()
                .find(|name| other.validators.contains_key(*name))
            {
                return Err(ModuleError::Conflict(format!("capability {name}")));
            }
        }
        state.next_generation = state
            .next_generation
            .checked_add(1)
            .ok_or_else(|| ModuleError::Unavailable("generation exhausted".into()))?;
        let generation = state.next_generation;
        let description = ModuleDescription {
            manifest: manifest.clone(),
            registry_id: self.registry_id.clone(),
            generation,
        };
        state.entries.insert(
            manifest.name.clone(),
            Arc::new(Entry {
                module,
                manifest,
                validators,
                context,
                generation,
                cancel: CancellationToken::new(),
            }),
        );
        Ok(description)
    }

    pub fn describe(&self, name: &str) -> Result<ModuleDescription, ModuleError> {
        let state = self.state.lock().expect("module registry poisoned");
        let entry = state
            .entries
            .get(name)
            .ok_or_else(|| ModuleError::NotFound(name.into()))?;
        Ok(ModuleDescription {
            manifest: entry.manifest.clone(),
            registry_id: self.registry_id.clone(),
            generation: entry.generation,
        })
    }

    /// Check the loaded instance's lifecycle without invoking any capability.
    /// External children are queried synchronously and may be cleaned up on exit.
    /// `describe` and `list` still include stopped instances until explicit unload.
    pub fn check_available(&self, name: &str) -> Result<ModuleDescription, ModuleError> {
        let entry = self
            .state
            .lock()
            .expect("module registry poisoned")
            .entries
            .get(name)
            .cloned()
            .ok_or_else(|| ModuleError::NotFound(name.into()))?;
        entry.module.check_available()?;
        let current = self.describe(name)?;
        if entry.cancel.is_cancelled() || current.generation != entry.generation {
            return Err(ModuleError::Unloaded);
        }
        Ok(current)
    }

    /// Capture state only from the exact currently loaded, live instance. This
    /// calls a trusted host hook, never the module's agent-facing `call` method.
    #[cfg(feature = "strongflow")]
    pub fn strongflow_state(
        &self,
        expected: &ModuleDescription,
    ) -> Result<strongflow::StrongFlowState, ModuleError> {
        if self.check_available(&expected.manifest.name)? != *expected {
            return Err(ModuleError::Unavailable(
                "loaded module identity changed".into(),
            ));
        }
        let entry = self
            .state
            .lock()
            .expect("module registry poisoned")
            .entries
            .get(&expected.manifest.name)
            .cloned()
            .ok_or_else(|| ModuleError::NotFound(expected.manifest.name.clone()))?;
        if entry.generation != expected.generation {
            return Err(ModuleError::Unloaded);
        }
        let snapshot = entry.module.strongflow_state()?;
        snapshot.validate()?;
        if snapshot.module_manifest != expected.manifest
            || self.check_available(&expected.manifest.name)? != *expected
        {
            return Err(ModuleError::Unavailable(
                "loaded module or manifest changed".into(),
            ));
        }
        Ok(snapshot)
    }

    pub fn list(&self) -> Vec<ModuleDescription> {
        self.state
            .lock()
            .expect("module registry poisoned")
            .entries
            .values()
            .map(|entry| ModuleDescription {
                manifest: entry.manifest.clone(),
                registry_id: self.registry_id.clone(),
                generation: entry.generation,
            })
            .collect()
    }

    /// Removing the entry is the unload boundary. All old snapshots are cancelled
    /// before another generation can be inserted. Shutdown runs outside the lock.
    pub fn unload(&self, name: &str) -> Result<ModuleDescription, ModuleError> {
        let entry = {
            let mut state = self.state.lock().expect("module registry poisoned");
            let entry = state
                .entries
                .remove(name)
                .ok_or_else(|| ModuleError::NotFound(name.into()))?;
            entry.cancel.cancel();
            entry
        };
        entry.module.shutdown();
        Ok(ModuleDescription {
            manifest: entry.manifest.clone(),
            registry_id: self.registry_id.clone(),
            generation: entry.generation,
        })
    }

    /// Call whichever instance is registered under this name at first poll.
    /// Callers bound to a description must use `call_exact` instead.
    pub async fn call(
        &self,
        module_name: &str,
        method: &str,
        arguments: Value,
        cancel: CancellationToken,
        timeout: Duration,
    ) -> Result<Value, ModuleError> {
        let entry = self
            .state
            .lock()
            .expect("module registry poisoned")
            .entries
            .get(module_name)
            .cloned()
            .ok_or_else(|| ModuleError::NotFound(module_name.into()))?;
        self.execute(entry, method, arguments, cancel, timeout)
            .await
    }

    /// Call only the described registry instance, generation and full manifest.
    /// Identity is checked and the entry captured atomically at first poll. A
    /// stale or foreign description is rejected before invoking module code;
    /// this never refreshes the caller's description to a replacement instance.
    pub async fn call_exact(
        &self,
        expected: &ModuleDescription,
        method: &str,
        arguments: Value,
        cancel: CancellationToken,
        timeout: Duration,
    ) -> Result<Value, ModuleError> {
        let entry = {
            let state = self.state.lock().expect("module registry poisoned");
            let entry = state
                .entries
                .get(&expected.manifest.name)
                .ok_or_else(|| ModuleError::NotFound(expected.manifest.name.clone()))?;
            if self.registry_id != expected.registry_id
                || entry.generation != expected.generation
                || entry.manifest != expected.manifest
            {
                return Err(ModuleError::Unavailable(
                    "loaded module identity changed".into(),
                ));
            }
            entry.clone()
        };
        self.execute(entry, method, arguments, cancel, timeout)
            .await
    }

    async fn execute(
        &self,
        entry: Arc<Entry>,
        method: &str,
        arguments: Value,
        cancel: CancellationToken,
        timeout: Duration,
    ) -> Result<Value, ModuleError> {
        let validator = entry
            .validators
            .get(method)
            .ok_or_else(|| ModuleError::Denied(method.into()))?;
        if !entry.context.allows(&entry.manifest, method) {
            return Err(ModuleError::Denied(method.into()));
        }
        validator
            .validate(&arguments)
            .map_err(|error| ModuleError::InvalidArguments(error.to_string()))?;
        if timeout.is_zero() {
            return Err(ModuleError::Timeout);
        }
        let call_cancel = entry.cancel.child_token();
        let _guard = CancelOnDrop(call_cancel.clone());
        let result = tokio::select! {
            biased;
            _ = entry.cancel.cancelled() => Err(ModuleError::Unloaded),
            _ = cancel.cancelled() => Err(ModuleError::Cancelled),
            _ = tokio::time::sleep(timeout) => Err(ModuleError::Timeout),
            result = entry.module.call(method, arguments, call_cancel) => result,
        };
        let state = self.state.lock().expect("module registry poisoned");
        if entry.cancel.is_cancelled()
            || state
                .entries
                .get(&entry.manifest.name)
                .is_none_or(|current| !Arc::ptr_eq(current, &entry))
        {
            return Err(ModuleError::Unloaded);
        }
        result
    }
}

impl Drop for ModuleRegistry {
    fn drop(&mut self) {
        let state = self.state.get_mut().expect("module registry poisoned");
        for entry in state.entries.values() {
            entry.cancel.cancel();
            entry.module.shutdown();
        }
    }
}

pub(crate) fn validate_manifest(
    manifest: &ModuleManifest,
) -> Result<BTreeMap<String, ArgumentValidator>, ModuleError> {
    if !valid_name(&manifest.name)
        || manifest.version.trim().is_empty()
        || manifest.version.len() > 128
    {
        return Err(ModuleError::InvalidManifest(
            "invalid name or version".into(),
        ));
    }
    if manifest.protocol_version != MODULE_PROTOCOL_VERSION {
        return Err(ModuleError::Protocol(format!(
            "unsupported version {}",
            manifest.protocol_version
        )));
    }
    let mut validators = BTreeMap::new();
    for capability in &manifest.capabilities {
        if !valid_name(&capability.name) || validators.contains_key(&capability.name) {
            return Err(ModuleError::InvalidManifest(
                "invalid or duplicate capability".into(),
            ));
        }
        let validator = ArgumentValidator::new(&capability.parameters)
            .map_err(|error| ModuleError::InvalidManifest(error.to_string()))?;
        validators.insert(capability.name.clone(), validator);
    }
    for (name, permissions) in &manifest.permissions {
        if !validators.contains_key(name)
            || permissions.iter().any(|permission| !valid_name(permission))
        {
            return Err(ModuleError::InvalidManifest(
                "invalid permission declaration".into(),
            ));
        }
    }
    Ok(validators)
}
