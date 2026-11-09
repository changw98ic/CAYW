//! Client-owned External plugin inventory and per-workspace activation state.
//! Installation is global. Only the Client starts or stops plugin processes.
use crate::cli_settings::uncertain;
use cayw_ai::CancellationToken;
use cayw_plugins::{
    ExternalConfig, PluginContext, PluginDescription, PluginKind, PluginManifest,
    PluginProcessIdentity, PluginRole,
};
use cayw_runtime::{
    Runtime, RuntimeError,
    local_cli::{LocalPaths, read_private, save_config},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    io::Read,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    sync::Arc,
};

type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

fn boxed_uncertain(error: impl Into<Box<dyn Error + Send + Sync>>) -> Box<dyn Error + Send + Sync> {
    Box::new(uncertain(error))
}

const INVENTORY_SCHEMA: &str = "cayw.plugin-inventory.v1";
const LEGACY_ACTIVATION_SCHEMA: &str = "cayw.workspace-architecture-plugins.v1";
const ACTIVATION_SCHEMA: &str = "cayw.workspace-architecture-plugins.v2";
const ACTIVATION_FILE: &str = "architecture-plugins.json";
const CLEANUP_SCHEMA: &str = "cayw.workspace-plugin-cleanup.v2";
const LEGACY_CLEANUP_SCHEMA: &str = "cayw.workspace-plugin-cleanup.v1";
const CLEANUP_FILE: &str = "workspace-plugin-cleanup.json";

pub fn doctor(paths: &LocalPaths) -> Result<Value> {
    let inventory_path = paths.data_dir.join("plugins.json");
    let inventory = read_inventory(&inventory_path)?.1;
    let activation_path = paths.state_file(ACTIVATION_FILE);
    let activations = read_activations(&activation_path)?.1;
    let cleanup_path = paths.data_dir.join(CLEANUP_FILE);
    let cleanup = read_cleanup(&cleanup_path)?.1;
    let cleanup_blocks = cleanup
        .entries
        .get(&workspace_key(&paths.workspace))
        .map(|entry| entry.plugins.clone())
        .unwrap_or_default();
    let unavailable = inventory
        .entries
        .iter()
        .filter_map(|(name, entry)| {
            executable_available(entry)
                .err()
                .map(|error| json!({"name":name,"error":error}))
        })
        .collect::<Vec<_>>();
    Ok(json!({
        "name":"plugins",
        "status":"ok",
        "message":"已读取 Client 全局插件清单、当前工作区架构插件启用关系和清理阻断记录",
        "inventory_file":inventory_path,
        "inventory_schema":inventory.schema,
        "installed":inventory.entries.len(),
        "unavailable":unavailable,
        "workspace_activation_file":activation_path,
        "workspace_activation_schema":activations.schema,
        "workspace_activations":activations.entries,
        "workspace_cleanup_file":cleanup_path,
        "workspace_cleanup_schema":cleanup.schema,
        "workspace_cleanup_blocks":cleanup_blocks,
        "global_cleanup_block_count":cleanup.block_count(),
    }))
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Descriptor {
    executable: PathBuf,
    #[serde(default)]
    argv: Vec<String>,
    manifest: PluginManifest,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Installed {
    descriptor: Descriptor,
    executable_sha256: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Inventory {
    schema: String,
    entries: BTreeMap<String, Installed>,
}

impl Default for Inventory {
    fn default() -> Self {
        Self {
            schema: INVENTORY_SCHEMA.into(),
            entries: BTreeMap::new(),
        }
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ActivationError {
    message: String,
    outcome_unknown: bool,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Activation {
    enabled: bool,
    last_error: Option<ActivationError>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyActivation {
    enabled: bool,
    last_error: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyActivations {
    schema: String,
    entries: BTreeMap<String, LegacyActivation>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Activations {
    schema: String,
    entries: BTreeMap<String, Activation>,
}

impl Default for Activations {
    fn default() -> Self {
        Self {
            schema: ACTIVATION_SCHEMA.into(),
            entries: BTreeMap::new(),
        }
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CleanupRecord {
    message: String,
    instance: Option<PluginDescription>,
    #[serde(default)]
    process: Option<PluginProcessIdentity>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyCleanupRecord {
    message: String,
    instance: Option<PluginDescription>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyWorkspaceCleanup {
    workspace: PathBuf,
    plugins: BTreeMap<String, LegacyCleanupRecord>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyCleanupLedger {
    schema: String,
    entries: BTreeMap<String, LegacyWorkspaceCleanup>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceCleanup {
    workspace: PathBuf,
    plugins: BTreeMap<String, CleanupRecord>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CleanupLedger {
    schema: String,
    entries: BTreeMap<String, WorkspaceCleanup>,
}

impl Default for CleanupLedger {
    fn default() -> Self {
        Self {
            schema: CLEANUP_SCHEMA.into(),
            entries: BTreeMap::new(),
        }
    }
}

impl CleanupLedger {
    fn block_count(&self) -> usize {
        self.entries.values().map(|entry| entry.plugins.len()).sum()
    }

    fn get(&self, workspace: &Path, plugin: &str) -> Option<&CleanupRecord> {
        self.entries
            .get(&workspace_key(workspace))
            .and_then(|entry| entry.plugins.get(plugin))
    }

    fn blocked_workspace(&self, plugin: &str) -> Option<&Path> {
        self.entries.values().find_map(|entry| {
            entry
                .plugins
                .contains_key(plugin)
                .then_some(entry.workspace.as_path())
        })
    }

    fn insert(
        &mut self,
        workspace: &Path,
        plugin: &str,
        message: String,
        instance: Option<PluginDescription>,
        process: Option<PluginProcessIdentity>,
    ) -> bool {
        let key = workspace_key(workspace);
        let entry = self.entries.entry(key).or_insert_with(|| WorkspaceCleanup {
            workspace: workspace.to_owned(),
            plugins: BTreeMap::new(),
        });
        let record = CleanupRecord {
            message,
            instance,
            process,
        };
        entry.plugins.insert(plugin.into(), record.clone()).as_ref() != Some(&record)
    }

    fn remove(&mut self, workspace: &Path, plugin: &str) -> bool {
        let key = workspace_key(workspace);
        let Some(entry) = self.entries.get_mut(&key) else {
            return false;
        };
        let changed = entry.plugins.remove(plugin).is_some();
        if entry.plugins.is_empty() {
            self.entries.remove(&key);
        }
        changed
    }
}

fn workspace_key(workspace: &Path) -> String {
    format!("{:x}", Sha256::digest(workspace.as_os_str().as_bytes()))
}

fn valid_plugin_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b'@' | b'/')
        })
}

/// Cleanup is confirmed only when the exact macOS PID incarnation is gone and
/// no process remains in the original process group. Observation errors keep the
/// block; a reused PID or group is never evidence that effects stopped.
#[cfg(target_os = "macos")]
fn cleanup_process_stopped(identity: &PluginProcessIdentity) -> std::io::Result<bool> {
    if identity.is_running()? || identity.process_group_running()? {
        return Ok(false);
    }
    Ok(true)
}

#[cfg(not(target_os = "macos"))]
fn cleanup_process_stopped(_identity: &PluginProcessIdentity) -> std::io::Result<bool> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "plugin process identity checks are unsupported on this platform",
    ))
}

fn cleanup_process(error: &(dyn Error + 'static)) -> Option<PluginProcessIdentity> {
    crate::cli_settings::runtime_error(error).and_then(|error| match error {
        RuntimeError::Plugin(plugin) => plugin.process_identity().cloned(),
        _ => None,
    })
}

fn read_inventory(path: &Path) -> Result<(Option<Vec<u8>>, Inventory)> {
    let original = match read_private(path, 1024 * 1024) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let inventory: Inventory = original
        .as_deref()
        .map(serde_json::from_slice)
        .transpose()?
        .unwrap_or_default();
    if inventory.schema != INVENTORY_SCHEMA {
        return Err("unsupported Client plugin inventory schema".into());
    }
    for (name, entry) in &inventory.entries {
        if name != &entry.descriptor.manifest.name {
            return Err("installed plugin key differs from its manifest name".into());
        }
        validate_structure(entry.descriptor.clone())?;
    }
    Ok((original, inventory))
}

fn read_activations(path: &Path) -> Result<(Option<Vec<u8>>, Activations, bool)> {
    let original = match read_private(path, 1024 * 1024) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let (activations, migrated) = match original.as_deref() {
        None => (Activations::default(), false),
        Some(bytes) => {
            let schema = serde_json::from_slice::<Value>(bytes)?
                .get("schema")
                .and_then(Value::as_str)
                .ok_or("workspace architecture plugin schema is missing")?
                .to_owned();
            match schema.as_str() {
                ACTIVATION_SCHEMA => (serde_json::from_slice(bytes)?, false),
                LEGACY_ACTIVATION_SCHEMA => {
                    let legacy: LegacyActivations = serde_json::from_slice(bytes)?;
                    if legacy.schema != LEGACY_ACTIVATION_SCHEMA {
                        return Err("unsupported workspace architecture plugin schema".into());
                    }
                    let entries = legacy
                        .entries
                        .into_iter()
                        .map(|(name, activation)| {
                            let last_error = activation.last_error.map(|message| ActivationError {
                                message,
                                // v1 discarded the original error type. Conservatively
                                // require explicit inspection before another auto-start.
                                outcome_unknown: true,
                            });
                            (
                                name,
                                Activation {
                                    enabled: activation.enabled,
                                    last_error,
                                },
                            )
                        })
                        .collect();
                    (
                        Activations {
                            schema: ACTIVATION_SCHEMA.into(),
                            entries,
                        },
                        true,
                    )
                }
                _ => return Err("unsupported workspace architecture plugin schema".into()),
            }
        }
    };
    if activations.entries.iter().any(|(name, activation)| {
        name.is_empty()
            || activation
                .last_error
                .as_ref()
                .is_some_and(|error| error.message.trim().is_empty())
    }) {
        return Err("invalid workspace architecture plugin activation".into());
    }
    Ok((original, activations, migrated))
}

fn read_cleanup(path: &Path) -> Result<(Option<Vec<u8>>, CleanupLedger)> {
    let original = match read_private(path, 1024 * 1024) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let cleanup = match original.as_deref() {
        None => CleanupLedger::default(),
        Some(bytes) => {
            let value: Value = serde_json::from_slice(bytes)?;
            let schema = value
                .get("schema")
                .and_then(Value::as_str)
                .ok_or("workspace plugin cleanup schema is missing")?;
            match schema {
                CLEANUP_SCHEMA => serde_json::from_value(value)?,
                LEGACY_CLEANUP_SCHEMA => {
                    let legacy: LegacyCleanupLedger = serde_json::from_value(value)?;
                    if legacy.schema != LEGACY_CLEANUP_SCHEMA {
                        return Err("unsupported workspace plugin cleanup schema".into());
                    }
                    CleanupLedger {
                        schema: CLEANUP_SCHEMA.into(),
                        entries: legacy
                            .entries
                            .into_iter()
                            .map(|(key, entry)| {
                                (
                                    key,
                                    WorkspaceCleanup {
                                        workspace: entry.workspace,
                                        plugins: entry
                                            .plugins
                                            .into_iter()
                                            .map(|(name, record)| {
                                                (
                                                    name,
                                                    CleanupRecord {
                                                        message: record.message,
                                                        instance: record.instance,
                                                        process: None,
                                                    },
                                                )
                                            })
                                            .collect(),
                                    },
                                )
                            })
                            .collect(),
                    }
                }
                _ => return Err("unsupported workspace plugin cleanup schema".into()),
            }
        }
    };
    if cleanup.schema != CLEANUP_SCHEMA || cleanup.entries.len() > 256 {
        return Err("invalid workspace plugin cleanup record".into());
    }
    for (key, entry) in &cleanup.entries {
        if !entry.workspace.is_absolute()
            || key != &workspace_key(&entry.workspace)
            || entry.plugins.is_empty()
            || entry.plugins.len() > 64
            || entry.plugins.iter().any(|(name, record)| {
                !valid_plugin_name(name)
                    || record.message.trim().is_empty()
                    || record.instance.as_ref().is_some_and(|instance| {
                        instance.manifest.name != *name
                            || instance.registry_id.is_empty()
                            || instance.generation == 0
                    })
                    || record.process.as_ref().is_some_and(|process| {
                        process.pid <= 1
                            || process.started_seconds == 0
                            || process.started_microseconds >= 1_000_000
                            || process.command.trim().is_empty()
                            || process.process_group.is_some_and(|group| group <= 1)
                    })
            })
        {
            return Err("invalid workspace plugin cleanup record".into());
        }
    }
    Ok((original, cleanup))
}

fn save_cleanup(path: &Path, original: Option<&[u8]>, cleanup: &CleanupLedger) -> Result<()> {
    save_config(path, original, &serde_json::to_vec_pretty(cleanup)?).map_err(boxed_uncertain)
}

fn save_inventory(path: &Path, original: Option<&[u8]>, inventory: &Inventory) -> Result<()> {
    save_config(path, original, &serde_json::to_vec_pretty(inventory)?).map_err(boxed_uncertain)
}

fn save_activations(path: &Path, original: Option<&[u8]>, activations: &Activations) -> Result<()> {
    save_config(path, original, &serde_json::to_vec_pretty(activations)?).map_err(boxed_uncertain)
}

fn activation_error(error: &(dyn Error + 'static)) -> ActivationError {
    ActivationError {
        message: error.to_string(),
        outcome_unknown: crate::cli_settings::error_outcome_unknown(error),
    }
}

async fn rollback_restore<T>(
    plugins: &Plugins,
    paths: &LocalPaths,
    runtime: &Runtime,
    loaded: &[PluginDescription],
    primary: Box<dyn Error + Send + Sync>,
) -> Result<T> {
    let mut cleanup = Vec::new();
    for description in loaded.iter().rev() {
        if let Err(error) = runtime.unload_plugin_exact(description).await {
            let message = format!("{}: {error}", description.manifest.name);
            cleanup.push(message.clone());
            if let Err(persistence) = plugins.persist_cleanup_block(
                paths,
                &description.manifest.name,
                format!("restore rollback cleanup was not confirmed: {error}"),
                Some(description.clone()),
                cleanup_process(&error),
            ) {
                cleanup.push(format!(
                    "{} cleanup block was not persisted: {persistence}",
                    description.manifest.name
                ));
            }
        }
    }
    if cleanup.is_empty() {
        Err(primary)
    } else {
        Err(boxed_uncertain(std::io::Error::other(format!(
            "{primary}; restore rollback was not confirmed: {}",
            cleanup.join("; ")
        ))))
    }
}

/// One instance is created by the local Client and cloned into every open workspace.
/// Its operation lock serializes global inventory mutations and process transitions.
#[derive(Clone)]
pub struct Plugins {
    inventory_path: PathBuf,
    cleanup_path: PathBuf,
    operation: Arc<tokio::sync::Mutex<()>>,
}

impl Plugins {
    pub fn new(inventory_path: PathBuf) -> Self {
        let cleanup_path = inventory_path.with_file_name(CLEANUP_FILE);
        Self {
            inventory_path,
            cleanup_path,
            operation: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    fn persist_cleanup_block(
        &self,
        paths: &LocalPaths,
        plugin: &str,
        message: String,
        instance: Option<PluginDescription>,
        process: Option<PluginProcessIdentity>,
    ) -> Result<()> {
        let (original, mut cleanup) = read_cleanup(&self.cleanup_path)?;
        if cleanup.insert(&paths.workspace, plugin, message, instance, process) {
            save_cleanup(&self.cleanup_path, original.as_deref(), &cleanup)?;
        }
        Ok(())
    }

    /// Start only plugins enabled for this workspace. Failed starts retain the
    /// user's enable intent and record the typed error instead of silently disabling it.
    pub async fn restore(&self, paths: &LocalPaths, runtime: &Runtime) -> Result<()> {
        let _operation = self.operation.lock().await;
        let (_, inventory) = read_inventory(&self.inventory_path)?;
        let (cleanup_original, mut cleanup) = read_cleanup(&self.cleanup_path)?;
        let activation_path = paths.state_file(ACTIVATION_FILE);
        let (original, mut activations, migrated) = read_activations(&activation_path)?;
        let names = activations
            .entries
            .iter()
            .filter(|(_, activation)| activation.enabled)
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>();
        let mut changed = migrated;
        let mut cleanup_changed = false;
        let mut loaded = Vec::new();
        let mut fatal = None;
        for name in names {
            let activation = activations
                .entries
                .get_mut(&name)
                .expect("enabled activation disappeared");
            if let Some(block) = cleanup.get(&paths.workspace, &name).cloned() {
                if let Some(process) = block.process.as_ref() {
                    match cleanup_process_stopped(process) {
                        Ok(true) => {
                            cleanup_changed |= cleanup.remove(&paths.workspace, &name);
                            // The exact process recorded by this block is gone, so
                            // the matching cleanup outcome is now confirmed. Retry
                            // the plugin instead of turning the stale activation
                            // error into a new identity-less permanent block.
                            if activation
                                .last_error
                                .as_ref()
                                .is_some_and(|error| error.outcome_unknown)
                            {
                                activation.last_error = None;
                                changed = true;
                            }
                        }
                        Ok(false) => {
                            fatal = Some(boxed_uncertain(std::io::Error::other(format!(
                                "plugin {name} cleanup is still unconfirmed: {}; exact process {} is still running or its process group still has members",
                                block.message, process.pid
                            ))));
                            break;
                        }
                        Err(error) => {
                            fatal = Some(boxed_uncertain(std::io::Error::other(format!(
                                "plugin {name} cleanup is still unconfirmed: {}; could not re-check exact process {}: {error}",
                                block.message, process.pid
                            ))));
                            break;
                        }
                    }
                } else {
                    let next = if block.instance.is_some() {
                        "explicit disable can retry that exact retained instance; the block remains until cleanup is confirmed"
                    } else {
                        "the exact process instance is unavailable, so cleanup cannot be confirmed automatically"
                    };
                    fatal = Some(boxed_uncertain(std::io::Error::other(format!(
                        "plugin {name} cleanup is still unconfirmed: {}; {next}",
                        block.message
                    ))));
                    break;
                }
            }
            if let Some(prior) = activation
                .last_error
                .as_ref()
                .filter(|error| error.outcome_unknown)
                .cloned()
            {
                let message = format!(
                    "plugin {name} has an unconfirmed prior outcome: {}",
                    prior.message
                );
                fatal = Some(
                    match self.persist_cleanup_block(paths, &name, message.clone(), None, None) {
                        Ok(()) => boxed_uncertain(std::io::Error::other(format!(
                            "{message}; the exact process instance is unavailable, so cleanup cannot be confirmed automatically"
                        ))),
                        Err(persistence) => boxed_uncertain(std::io::Error::other(format!(
                            "{message}; cleanup block was not persisted: {persistence}"
                        ))),
                    },
                );
                break;
            }
            let result = match inventory.entries.get(&name) {
                Some(entry) if runtime.plugin_registry().describe(&name).is_err() => {
                    load(runtime, entry, CancellationToken::new()).await
                }
                Some(_) => continue,
                None => Err("enabled architecture plugin is not installed".into()),
            };
            match result {
                Ok(description) => {
                    loaded.push(description);
                    if activation.last_error.take().is_some() {
                        changed = true;
                    }
                }
                Err(error) => {
                    let recorded = activation_error(error.as_ref());
                    if activation.last_error.as_ref() != Some(&recorded) {
                        activation.last_error = Some(recorded.clone());
                        changed = true;
                    }
                    if recorded.outcome_unknown {
                        let message = error.to_string();
                        fatal = Some(
                            match self.persist_cleanup_block(
                                paths,
                                &name,
                                format!("plugin start cleanup was not confirmed: {message}"),
                                None,
                                cleanup_process(error.as_ref()),
                            ) {
                                Ok(()) => error,
                                Err(persistence) => {
                                    boxed_uncertain(std::io::Error::other(format!(
                                        "{message}; cleanup block was not persisted: {persistence}"
                                    )))
                                }
                            },
                        );
                        break;
                    }
                }
            }
        }
        if cleanup_changed
            && let Err(error) =
                save_cleanup(&self.cleanup_path, cleanup_original.as_deref(), &cleanup)
        {
            return rollback_restore(
                self,
                paths,
                runtime,
                &loaded,
                boxed_uncertain(std::io::Error::other(format!(
                    "confirmed plugin cleanup could not be persisted: {error}"
                ))),
            )
            .await;
        }
        if changed
            && let Err(error) =
                save_activations(&activation_path, original.as_deref(), &activations)
        {
            return rollback_restore(self, paths, runtime, &loaded, error).await;
        }
        if let Some(error) = fatal {
            return rollback_restore(self, paths, runtime, &loaded, error).await;
        }
        Ok(())
    }

    pub async fn shutdown_workspace(&self, paths: &LocalPaths, runtime: &Runtime) -> Result<()> {
        let _operation = self.operation.lock().await;
        let activation_path = paths.state_file(ACTIVATION_FILE);
        let (original, mut activations, migrated) = read_activations(&activation_path)?;
        let (cleanup_original, mut cleanup) = read_cleanup(&self.cleanup_path)?;
        let mut failures = Vec::new();
        let mut persistence_failures = Vec::new();
        let mut changed = migrated;
        let mut cleanup_changed = false;
        let enabled = activations
            .entries
            .iter()
            .filter(|(_, activation)| activation.enabled)
            .map(|(name, _)| name.clone())
            .collect::<BTreeSet<_>>();
        for description in runtime
            .list_plugins()
            .into_iter()
            .filter(|description| enabled.contains(&description.manifest.name))
        {
            if let Err(error) = runtime.unload_plugin_exact(&description).await {
                let recorded = ActivationError {
                    message: error.to_string(),
                    outcome_unknown: error.outcome_unknown(),
                };
                if recorded.outcome_unknown {
                    cleanup_changed |= cleanup.insert(
                        &paths.workspace,
                        &description.manifest.name,
                        format!("workspace shutdown cleanup was not confirmed: {error}"),
                        Some(description.clone()),
                        cleanup_process(&error),
                    );
                }
                let activation = activations
                    .entries
                    .entry(description.manifest.name.clone())
                    .or_insert_with(|| Activation {
                        enabled: true,
                        last_error: None,
                    });
                activation.enabled = true;
                if activation.last_error.as_ref() != Some(&recorded) {
                    activation.last_error = Some(recorded);
                    changed = true;
                }
                failures.push(error);
            }
        }
        if cleanup_changed
            && let Err(error) =
                save_cleanup(&self.cleanup_path, cleanup_original.as_deref(), &cleanup)
        {
            persistence_failures.push(format!("cleanup block was not persisted: {error}"));
        }
        if changed
            && let Err(error) =
                save_activations(&activation_path, original.as_deref(), &activations)
        {
            persistence_failures.push(format!("activation failure was not persisted: {error}"));
        }
        let outcome_unknown =
            failures.iter().any(RuntimeError::outcome_unknown) || !persistence_failures.is_empty();
        let mut messages = failures
            .into_iter()
            .map(|error| error.to_string())
            .collect::<Vec<_>>();
        messages.extend(persistence_failures);
        if messages.is_empty() {
            Ok(())
        } else if outcome_unknown {
            Err(boxed_uncertain(std::io::Error::other(messages.join("; "))))
        } else {
            Err(messages.join("; ").into())
        }
    }

    pub async fn call(
        &self,
        paths: &LocalPaths,
        runtime: &Runtime,
        method: &str,
        params: Value,
        cancel: CancellationToken,
    ) -> Result<Value> {
        let _operation = self.operation.lock().await;
        let (inventory_bytes, mut inventory) = read_inventory(&self.inventory_path)?;
        let (mut cleanup_bytes, mut cleanup) = read_cleanup(&self.cleanup_path)?;
        let activation_path = paths.state_file(ACTIVATION_FILE);
        let (mut activation_bytes, mut activations, _) = read_activations(&activation_path)?;
        match method {
            "settings.plugins.list" => {
                let _: Empty = serde_json::from_value(params)?;
            }
            "settings.plugins.install" | "settings.plugins.update" => {
                let put: Put = serde_json::from_value(params)?;
                let descriptor = validate(put.descriptor)?;
                let id = descriptor.manifest.name.clone();
                if let Some(workspace) = cleanup.blocked_workspace(&id) {
                    return Err(boxed_uncertain(std::io::Error::other(format!(
                        "plugin {id} has unconfirmed cleanup in {}; resolve that exact instance first (disable only clears it after confirmed cleanup)",
                        workspace.display()
                    ))));
                }
                if let Some(workspace) = blocked_workspace(paths, &id)? {
                    return Err(boxed_uncertain(std::io::Error::other(format!(
                        "plugin {id} is still enabled or has an unconfirmed outcome in {}; reconcile that workspace before changing the global installation",
                        workspace.display()
                    ))));
                }
                if method.ends_with("install") {
                    if put.name.is_some() || inventory.entries.contains_key(&id) {
                        return Err("plugin already installed or unexpected name".into());
                    }
                    if inventory.entries.len() >= 64 {
                        return Err("at most 64 local plugins are supported".into());
                    }
                } else {
                    if put.name.as_deref() != Some(&id) {
                        return Err("update must preserve the installed plugin name".into());
                    }
                    if !inventory.entries.contains_key(&id) {
                        return Err("plugin is not installed".into());
                    }
                    if runtime.plugin_registry().describe(&id).is_ok() {
                        return Err(
                            "plugin process is still loaded; disable it before updating".into()
                        );
                    }
                }
                let digest = executable_digest(&descriptor)?;
                inventory.entries.insert(
                    id,
                    Installed {
                        descriptor,
                        executable_sha256: digest,
                    },
                );
                save_inventory(&self.inventory_path, inventory_bytes.as_deref(), &inventory)?;
            }
            "settings.plugins.enable" => {
                let enable: Enable = serde_json::from_value(params)?;
                if !enable.approve_process {
                    return Err("explicit process approval is required: local code runs as you, without a security sandbox; declared plugin capabilities remain subject to Host approvals and budgets".into());
                }
                let entry = inventory
                    .entries
                    .get(&enable.name)
                    .ok_or("plugin is not installed")?;
                if let Some(block) = cleanup.get(&paths.workspace, &enable.name).cloned() {
                    let Some(process) = block.process.as_ref().filter(|_| block.instance.is_none())
                    else {
                        let next = if block.instance.is_some() {
                            "explicit disable can retry that exact retained instance; the block remains until cleanup is confirmed"
                        } else {
                            "the exact process instance is unavailable, so cleanup cannot be confirmed automatically"
                        };
                        return Err(boxed_uncertain(std::io::Error::other(format!(
                            "plugin cleanup is still unconfirmed: {}; {next}",
                            block.message
                        ))));
                    };
                    match cleanup_process_stopped(process) {
                        Ok(true) => {
                            if cleanup.remove(&paths.workspace, &enable.name) {
                                if let Err(persistence) = save_cleanup(
                                    &self.cleanup_path,
                                    cleanup_bytes.as_deref(),
                                    &cleanup,
                                ) {
                                    return Err(boxed_uncertain(std::io::Error::other(format!(
                                        "confirmed plugin cleanup could not be persisted: {persistence}"
                                    ))));
                                }
                                cleanup_bytes = Some(serde_json::to_vec_pretty(&cleanup)?);
                            }
                            // The exact recorded process is gone, so the matching
                            // prior cleanup outcome is confirmed. Retry the plugin
                            // instead of creating an identity-less new block.
                            if let Some(activation) = activations.entries.get_mut(&enable.name)
                                && activation
                                    .last_error
                                    .as_ref()
                                    .is_some_and(|error| error.outcome_unknown)
                            {
                                activation.last_error = None;
                                save_activations(
                                    &activation_path,
                                    activation_bytes.as_deref(),
                                    &activations,
                                )?;
                                activation_bytes = Some(serde_json::to_vec_pretty(&activations)?);
                            }
                        }
                        Ok(false) => {
                            return Err(boxed_uncertain(std::io::Error::other(format!(
                                "plugin cleanup is still unconfirmed: {}; exact process {} is still running or its process group still has members",
                                block.message, process.pid
                            ))));
                        }
                        Err(error) => {
                            return Err(boxed_uncertain(std::io::Error::other(format!(
                                "plugin cleanup is still unconfirmed: {}; could not re-check exact process {}: {error}",
                                block.message, process.pid
                            ))));
                        }
                    }
                }
                if activations
                    .entries
                    .get(&enable.name)
                    .is_some_and(|activation| activation.enabled)
                {
                    return Err(
                        "plugin already enabled for this workspace; inspect live status".into(),
                    );
                }
                if let Some(prior) = activations
                    .entries
                    .get(&enable.name)
                    .and_then(|activation| activation.last_error.as_ref())
                    .filter(|error| error.outcome_unknown)
                    .cloned()
                {
                    let message =
                        format!("plugin has an unconfirmed prior outcome: {}", prior.message);
                    if cleanup.insert(&paths.workspace, &enable.name, message.clone(), None, None)
                        && let Err(persistence) =
                            save_cleanup(&self.cleanup_path, cleanup_bytes.as_deref(), &cleanup)
                    {
                        return Err(boxed_uncertain(std::io::Error::other(format!(
                            "{message}; cleanup block was not persisted: {persistence}"
                        ))));
                    }
                    return Err(boxed_uncertain(std::io::Error::other(format!(
                        "{message}; the exact process instance is unavailable, so cleanup cannot be confirmed automatically"
                    ))));
                }
                if runtime.plugin_registry().describe(&enable.name).is_ok() {
                    return Err("plugin process is already loaded for this workspace".into());
                }
                let description = match load(runtime, entry, cancel).await {
                    Ok(description) => description,
                    Err(error) => {
                        let recorded = activation_error(error.as_ref());
                        activations.entries.insert(
                            enable.name.clone(),
                            Activation {
                                enabled: false,
                                last_error: Some(recorded.clone()),
                            },
                        );
                        let mut persistence = Vec::new();
                        if recorded.outcome_unknown
                            && cleanup.insert(
                                &paths.workspace,
                                &enable.name,
                                format!("plugin start cleanup was not confirmed: {error}"),
                                None,
                                cleanup_process(error.as_ref()),
                            )
                            && let Err(cleanup_error) =
                                save_cleanup(&self.cleanup_path, cleanup_bytes.as_deref(), &cleanup)
                        {
                            persistence
                                .push(format!("cleanup block was not persisted: {cleanup_error}"));
                        }
                        if let Err(activation_error) = save_activations(
                            &activation_path,
                            activation_bytes.as_deref(),
                            &activations,
                        ) {
                            persistence.push(format!(
                                "plugin start failure was not persisted: {activation_error}"
                            ));
                        }
                        if persistence.is_empty() {
                            return Err(error);
                        }
                        return Err(boxed_uncertain(std::io::Error::other(format!(
                            "{error}; {}",
                            persistence.join("; ")
                        ))));
                    }
                };
                activations.entries.insert(
                    enable.name.clone(),
                    Activation {
                        enabled: true,
                        last_error: None,
                    },
                );
                if let Err(error) =
                    save_activations(&activation_path, activation_bytes.as_deref(), &activations)
                {
                    return rollback_restore(self, paths, runtime, &[description], error).await;
                }
                return self.status(paths, runtime, inventory, activations, &cleanup);
            }
            "settings.plugins.disable" => {
                let id: Name = serde_json::from_value(params)?;
                if !inventory.entries.contains_key(&id.name) {
                    return Err("plugin is not installed".into());
                }
                let mut cleanup_confirmed = false;
                if let Some(block) = cleanup.get(&paths.workspace, &id.name).cloned() {
                    if let Some(instance) = block.instance.as_ref() {
                        if let Err(error) = runtime.retry_plugin_cleanup_exact(instance).await {
                            // Keep the exact identity persisted by the previous
                            // attempt when this Client can no longer find that
                            // retiring registry entry.
                            let process = cleanup_process(&error).or_else(|| block.process.clone());
                            let message =
                                format!("plugin cleanup remains unconfirmed after retry: {error}");
                            if cleanup.insert(
                                &paths.workspace,
                                &id.name,
                                message,
                                Some(instance.clone()),
                                process.clone(),
                            ) {
                                if let Err(persistence) = save_cleanup(
                                    &self.cleanup_path,
                                    cleanup_bytes.as_deref(),
                                    &cleanup,
                                ) {
                                    return Err(boxed_uncertain(std::io::Error::other(format!(
                                        "cleanup block update was not persisted: {persistence}"
                                    ))));
                                }
                                cleanup_bytes = Some(serde_json::to_vec_pretty(&cleanup)?);
                            }
                            if let Some(process) = process.as_ref() {
                                match cleanup_process_stopped(process) {
                                    Ok(true) => cleanup_confirmed = true,
                                    Ok(false) => {
                                        return Err(boxed_uncertain(std::io::Error::other(
                                            format!(
                                                "plugin cleanup remains unconfirmed: {}; exact process {} is still running or its process group still has members",
                                                block.message, process.pid
                                            ),
                                        )));
                                    }
                                    Err(check) => {
                                        return Err(boxed_uncertain(std::io::Error::other(
                                            format!(
                                                "plugin cleanup remains unconfirmed: {}; could not re-check exact process {}: {check}",
                                                block.message, process.pid
                                            ),
                                        )));
                                    }
                                }
                            } else {
                                return Err(error.into());
                            }
                        } else {
                            cleanup_confirmed = true;
                        }
                    } else if let Some(process) = block.process.as_ref() {
                        match cleanup_process_stopped(process) {
                            Ok(true) => cleanup_confirmed = true,
                            Ok(false) => {
                                return Err(boxed_uncertain(std::io::Error::other(format!(
                                    "plugin cleanup remains unconfirmed: {}; exact process {} is still running or its process group still has members",
                                    block.message, process.pid
                                ))));
                            }
                            Err(error) => {
                                return Err(boxed_uncertain(std::io::Error::other(format!(
                                    "plugin cleanup remains unconfirmed: {}; could not re-check exact process {}: {error}",
                                    block.message, process.pid
                                ))));
                            }
                        }
                    } else {
                        return Err(boxed_uncertain(std::io::Error::other(format!(
                            "plugin cleanup remains blocked because the exact process instance is unavailable: {}",
                            block.message
                        ))));
                    }
                    if cleanup_confirmed
                        && cleanup.remove(&paths.workspace, &id.name)
                        && let Err(persistence) =
                            save_cleanup(&self.cleanup_path, cleanup_bytes.as_deref(), &cleanup)
                    {
                        return Err(boxed_uncertain(std::io::Error::other(format!(
                            "confirmed plugin cleanup could not be persisted: {persistence}"
                        ))));
                    }
                }
                if !cleanup_confirmed
                    && let Some(prior) = activations
                        .entries
                        .get(&id.name)
                        .and_then(|activation| activation.last_error.as_ref())
                        .filter(|error| error.outcome_unknown)
                        .cloned()
                {
                    let pending = runtime
                        .plugin_registry()
                        .pending_cleanup()
                        .into_iter()
                        .find(|instance| instance.manifest.name == id.name);
                    let Some(instance) = pending else {
                        let message = format!(
                            "plugin cleanup remains blocked because the prior outcome is unconfirmed and the exact process instance is unavailable: {}",
                            prior.message
                        );
                        if cleanup.insert(&paths.workspace, &id.name, message.clone(), None, None)
                            && let Err(persistence) =
                                save_cleanup(&self.cleanup_path, cleanup_bytes.as_deref(), &cleanup)
                        {
                            return Err(boxed_uncertain(std::io::Error::other(format!(
                                "{message}; cleanup block was not persisted: {persistence}"
                            ))));
                        }
                        return Err(boxed_uncertain(std::io::Error::other(message)));
                    };
                    if let Err(error) = runtime.retry_plugin_cleanup_exact(&instance).await {
                        let message =
                            format!("plugin cleanup remains unconfirmed after retry: {error}");
                        if cleanup.insert(
                            &paths.workspace,
                            &id.name,
                            message.clone(),
                            Some(instance),
                            cleanup_process(&error),
                        ) && let Err(persistence) =
                            save_cleanup(&self.cleanup_path, cleanup_bytes.as_deref(), &cleanup)
                        {
                            return Err(boxed_uncertain(std::io::Error::other(format!(
                                "{message}; cleanup block was not persisted: {persistence}"
                            ))));
                        }
                        return Err(error.into());
                    }
                    cleanup_confirmed = true;
                }
                let live = runtime.plugin_registry().describe(&id.name).ok();
                if !activations.entries.contains_key(&id.name) && live.is_none() {
                    if cleanup_confirmed {
                        return self.status(paths, runtime, inventory, activations, &cleanup);
                    }
                    return Err("plugin is not enabled for this workspace".into());
                }
                let activation = activations.entries.entry(id.name.clone()).or_default();
                if !activation.enabled
                    && live.is_none()
                    && !activation
                        .last_error
                        .as_ref()
                        .is_some_and(|error| error.outcome_unknown)
                {
                    return Err("plugin is not enabled for this workspace".into());
                }
                activation.enabled = false;
                activation.last_error = None;
                save_activations(&activation_path, activation_bytes.as_deref(), &activations)?;
                let (disabled_bytes, mut persisted, _) = read_activations(&activation_path)?;
                if let Some(description) = live
                    && let Err(error) = runtime.unload_plugin_exact(&description).await
                {
                    let recorded = ActivationError {
                        message: error.to_string(),
                        outcome_unknown: error.outcome_unknown(),
                    };
                    let activation = persisted.entries.entry(id.name.clone()).or_default();
                    activation.enabled = false;
                    activation.last_error = Some(recorded.clone());
                    let cleanup_changed = if recorded.outcome_unknown {
                        cleanup.insert(
                            &paths.workspace,
                            &id.name,
                            format!("plugin disable cleanup was not confirmed: {error}"),
                            Some(description),
                            cleanup_process(&error),
                        )
                    } else {
                        false
                    };
                    let mut persistence = Vec::new();
                    if cleanup_changed
                        && let Err(cleanup_error) =
                            save_cleanup(&self.cleanup_path, cleanup_bytes.as_deref(), &cleanup)
                    {
                        persistence.push(format!(
                            "cleanup block update was not persisted: {cleanup_error}"
                        ));
                    }
                    if let Err(activation_error) =
                        save_activations(&activation_path, disabled_bytes.as_deref(), &persisted)
                    {
                        persistence.push(format!(
                            "plugin disable failure was not persisted: {activation_error}"
                        ));
                    }
                    if persistence.is_empty() {
                        return Err(error.into());
                    }
                    return Err(boxed_uncertain(std::io::Error::other(format!(
                        "{error}; {}",
                        persistence.join("; ")
                    ))));
                }
                persisted.entries.remove(&id.name);
                save_activations(&activation_path, disabled_bytes.as_deref(), &persisted)?;
                return self.status(paths, runtime, inventory, persisted, &cleanup);
            }
            "settings.plugins.remove" => {
                let id: Name = serde_json::from_value(params)?;
                if !inventory.entries.contains_key(&id.name) {
                    return Err("plugin is not installed".into());
                }
                if let Some(workspace) = cleanup.blocked_workspace(&id.name) {
                    return Err(boxed_uncertain(std::io::Error::other(format!(
                        "plugin {} has unconfirmed cleanup in {}; resolve that exact instance first (disable only clears it after confirmed cleanup)",
                        id.name,
                        workspace.display()
                    ))));
                }
                if let Some(workspace) = blocked_workspace(paths, &id.name)? {
                    return Err(format!(
                        "disable and reconcile the plugin in every workspace before removing; still enabled or unconfirmed in {}",
                        workspace.display()
                    )
                    .into());
                }
                if runtime.plugin_registry().describe(&id.name).is_ok() {
                    return Err("plugin process is still loaded; disable it before removing".into());
                }
                inventory.entries.remove(&id.name);
                save_inventory(&self.inventory_path, inventory_bytes.as_deref(), &inventory)?;
            }
            _ => return Err("unsupported plugin settings method".into()),
        }
        self.status(paths, runtime, inventory, activations, &cleanup)
    }

    fn status(
        &self,
        paths: &LocalPaths,
        runtime: &Runtime,
        inventory: Inventory,
        activations: Activations,
        cleanup: &CleanupLedger,
    ) -> Result<Value> {
        let entries: Vec<Value> = inventory
            .entries
            .into_iter()
            .map(|(name, entry)| {
                let activation = activations.entries.get(&name);
                let live = runtime.plugin_registry().check_available(&name);
                let executable = executable_available(&entry);
                let cleanup_block = cleanup.get(&paths.workspace, &name);
                let last_error = cleanup_block
                    .map(|record| ActivationError {
                        message: record.message.clone(),
                        outcome_unknown: true,
                    })
                    .or_else(|| activation.and_then(|activation| activation.last_error.clone()))
                    .or_else(|| {
                        activation
                            .is_some_and(|activation| activation.enabled)
                            .then(|| {
                                live.as_ref().err().map(|error| ActivationError {
                                    message: error.to_string(),
                                    outcome_unknown: error.outcome_unknown(),
                                })
                            })
                            .flatten()
                    });
                json!({
                    "name":name,
                    "descriptor":entry.descriptor,
                    "executable_sha256":entry.executable_sha256,
                    "enabled":activation.is_some_and(|activation| activation.enabled),
                    "available":live.is_ok(),
                    "executable_available":executable.is_ok(),
                    "executable_error":executable.err(),
                    "cleanup_blocked":cleanup_block.is_some(),
                    "cleanup_process":cleanup_block.and_then(|record| record.process.clone()),
                    "last_error":last_error
                })
            })
            .collect();
        Ok(json!({
            "entries":entries,
            "loaded":runtime.list_plugins(),
            "inventory_scope":"client",
            "activation_scope":"workspace",
            "supported_role":"workspace_architecture",
            "supported_source":"local_external",
            "protocol_version":cayw_plugins::PLUGIN_PROTOCOL_VERSION
        }))
    }
}

fn blocked_workspace(paths: &LocalPaths, name: &str) -> Result<Option<PathBuf>> {
    for workspace in paths.known_workspaces()? {
        let workspace_paths = LocalPaths::from_environment(&workspace)?;
        let (_, activations, _) = read_activations(&workspace_paths.state_file(ACTIVATION_FILE))?;
        if activations.entries.get(name).is_some_and(|activation| {
            activation.enabled
                || activation
                    .last_error
                    .as_ref()
                    .is_some_and(|error| error.outcome_unknown)
        }) {
            return Ok(Some(workspace));
        }
    }
    Ok(None)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Put {
    name: Option<String>,
    descriptor: Descriptor,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Name {
    name: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Enable {
    name: String,
    approve_process: bool,
}

fn validate_structure(descriptor: Descriptor) -> Result<Descriptor> {
    descriptor.manifest.validate()?;
    if descriptor.manifest.role != PluginRole::WorkspaceArchitecture
        || descriptor.manifest.kind != PluginKind::External
        || !descriptor.executable.is_absolute()
    {
        return Err("only local External workspace architecture plugins with an absolute executable path are supported; Client plugins, npm, URLs and Native dynamic installation are unsupported".into());
    }
    if descriptor.argv.len() > 128
        || descriptor
            .argv
            .iter()
            .any(|arg| arg.len() > 4096 || arg.contains('\0'))
    {
        return Err("invalid plugin arguments".into());
    }
    Ok(descriptor)
}

/// Strict form used only when the executable is about to be registered or run.
fn validate(mut descriptor: Descriptor) -> Result<Descriptor> {
    descriptor = validate_structure(descriptor)?;
    descriptor.executable = descriptor.executable.canonicalize()?;
    Ok(descriptor)
}

/// Authorization-free listing state: a missing or replaced executable marks one
/// entry unavailable without making the whole inventory unreadable.
fn executable_available(entry: &Installed) -> std::result::Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let descriptor =
        validate_structure(entry.descriptor.clone()).map_err(|error| error.to_string())?;
    let meta = std::fs::metadata(&descriptor.executable).map_err(|error| error.to_string())?;
    if !meta.is_file() || meta.permissions().mode() & 0o111 == 0 {
        return Err("plugin executable must be an executable regular file".into());
    }
    Ok(())
}

fn executable_digest(descriptor: &Descriptor) -> Result<String> {
    use std::os::unix::fs::PermissionsExt;
    let mut file = std::fs::File::open(&descriptor.executable)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.permissions().mode() & 0o111 == 0 {
        return Err("plugin executable must be an executable regular file".into());
    }
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let size = file.read(&mut buffer)?;
        if size == 0 {
            break;
        }
        digest.update(&buffer[..size]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

async fn load(
    runtime: &Runtime,
    entry: &Installed,
    cancel: CancellationToken,
) -> Result<PluginDescription> {
    let descriptor = validate(entry.descriptor.clone())?;
    if executable_digest(&descriptor)? != entry.executable_sha256 {
        return Err(
            "plugin executable changed since registration; explicitly update it before enabling"
                .into(),
        );
    }
    let capabilities = descriptor
        .manifest
        .capabilities
        .iter()
        .map(|capability| capability.name.clone())
        .collect::<Vec<_>>();
    let permissions = descriptor
        .manifest
        .permissions
        .values()
        .flatten()
        .cloned()
        .collect::<Vec<_>>();
    let config = ExternalConfig::new(descriptor.executable, descriptor.argv, descriptor.manifest);
    Ok(runtime
        .load_external(
            config,
            PluginContext::new(capabilities, permissions),
            cancel,
        )
        .await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        ffi::OsString,
        os::unix::fs::PermissionsExt,
        sync::{
            Mutex,
            atomic::{AtomicUsize, Ordering},
        },
    };

    static ENVIRONMENT: Mutex<()> = Mutex::new(());

    struct RestoreCaywHome(Option<OsString>);

    impl Drop for RestoreCaywHome {
        fn drop(&mut self) {
            unsafe {
                match self.0.take() {
                    Some(value) => std::env::set_var("CAYW_HOME", value),
                    None => std::env::remove_var("CAYW_HOME"),
                }
            }
        }
    }

    struct RetryCleanup {
        manifest: PluginManifest,
        waits: AtomicUsize,
    }

    #[cayw_plugins::async_trait]
    impl cayw_plugins::Plugin for RetryCleanup {
        fn manifest(&self) -> &PluginManifest {
            &self.manifest
        }

        async fn call(
            &self,
            _method: &str,
            arguments: Value,
            _cancel: CancellationToken,
        ) -> std::result::Result<Value, cayw_plugins::PluginError> {
            Ok(arguments)
        }

        async fn wait_shutdown(&self) -> std::result::Result<(), cayw_plugins::PluginError> {
            if self.waits.fetch_add(1, Ordering::SeqCst) < 2 {
                Err(cayw_plugins::PluginError::OutcomeUnknown(
                    "synthetic cleanup is still pending".into(),
                ))
            } else {
                Ok(())
            }
        }
    }

    fn manifest(kind: PluginKind) -> PluginManifest {
        PluginManifest {
            name: "fixture".into(),
            version: "1.0.0".into(),
            protocol_version: cayw_plugins::PLUGIN_PROTOCOL_VERSION,
            role: PluginRole::WorkspaceArchitecture,
            kind,
            capabilities: Vec::new(),
            permissions: BTreeMap::new(),
        }
    }

    fn local_paths(data_dir: &Path, workspace: &Path) -> LocalPaths {
        let _environment = ENVIRONMENT.lock().unwrap();
        let restore = RestoreCaywHome(std::env::var_os("CAYW_HOME"));
        unsafe { std::env::set_var("CAYW_HOME", data_dir) };
        let paths = LocalPaths::from_environment(workspace);
        drop(restore);
        paths.unwrap()
    }

    fn runtime(paths: &LocalPaths) -> Runtime {
        Runtime::open(
            &paths.database,
            &paths.workspace,
            Arc::new(cayw_runtime::fixture::OfflineProvider::default()),
            cayw_ai::Model::new("fixture", "offline-runtime"),
            cayw_runtime::RuntimeConfig::default(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn persisted_cleanup_blocks_restore_and_blind_disable() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let paths = local_paths(&directory.path().join("state"), &workspace);
        paths.initialize().unwrap();
        let runtime = runtime(&paths);
        let plugins = Plugins::new(paths.data_dir.join("plugins.json"));
        let descriptor = validate(Descriptor {
            executable: std::env::current_exe().unwrap(),
            argv: Vec::new(),
            manifest: manifest(PluginKind::External),
        })
        .unwrap();
        let mut inventory = Inventory::default();
        inventory.entries.insert(
            "fixture".into(),
            Installed {
                executable_sha256: executable_digest(&descriptor).unwrap(),
                descriptor,
            },
        );
        save_inventory(&plugins.inventory_path, None, &inventory).unwrap();
        let mut activations = Activations::default();
        activations.entries.insert(
            "fixture".into(),
            Activation {
                enabled: true,
                last_error: None,
            },
        );
        save_activations(&paths.state_file(ACTIVATION_FILE), None, &activations).unwrap();
        let mut cleanup = CleanupLedger::default();
        cleanup.insert(
            &paths.workspace,
            "fixture",
            "prior cleanup is unconfirmed".into(),
            None,
            None,
        );
        save_cleanup(&plugins.cleanup_path, None, &cleanup).unwrap();

        let restore = plugins.restore(&paths, &runtime).await.unwrap_err();
        assert!(crate::cli_settings::error_outcome_unknown(restore.as_ref()));
        assert!(runtime.plugin_registry().describe("fixture").is_err());
        let disable = plugins
            .call(
                &paths,
                &runtime,
                "settings.plugins.disable",
                json!({"name":"fixture"}),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(crate::cli_settings::error_outcome_unknown(disable.as_ref()));
        assert!(
            read_cleanup(&plugins.cleanup_path)
                .unwrap()
                .1
                .get(&paths.workspace, "fixture")
                .is_some()
        );
    }

    #[tokio::test]
    async fn disable_cannot_clear_unknown_activation_without_exact_instance() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let paths = local_paths(&directory.path().join("state"), &workspace);
        paths.initialize().unwrap();
        let runtime = runtime(&paths);
        let plugins = Plugins::new(paths.data_dir.join("plugins.json"));
        let descriptor = validate(Descriptor {
            executable: std::env::current_exe().unwrap(),
            argv: Vec::new(),
            manifest: manifest(PluginKind::External),
        })
        .unwrap();
        let mut inventory = Inventory::default();
        inventory.entries.insert(
            "fixture".into(),
            Installed {
                executable_sha256: executable_digest(&descriptor).unwrap(),
                descriptor,
            },
        );
        save_inventory(&plugins.inventory_path, None, &inventory).unwrap();
        let mut activations = Activations::default();
        activations.entries.insert(
            "fixture".into(),
            Activation {
                enabled: false,
                last_error: Some(ActivationError {
                    message: "prior cleanup record could not be persisted".into(),
                    outcome_unknown: true,
                }),
            },
        );
        save_activations(&paths.state_file(ACTIVATION_FILE), None, &activations).unwrap();

        let error = plugins
            .call(
                &paths,
                &runtime,
                "settings.plugins.disable",
                json!({"name":"fixture"}),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(crate::cli_settings::error_outcome_unknown(error.as_ref()));
        let (_, persisted, _) = read_activations(&paths.state_file(ACTIVATION_FILE)).unwrap();
        assert!(
            persisted.entries["fixture"]
                .last_error
                .as_ref()
                .unwrap()
                .outcome_unknown
        );
        assert!(
            read_cleanup(&plugins.cleanup_path)
                .unwrap()
                .1
                .get(&paths.workspace, "fixture")
                .is_some_and(|record| record.instance.is_none())
        );
    }

    #[tokio::test]
    async fn explicit_disable_keeps_unknown_until_exact_cleanup_confirms() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let paths = local_paths(&directory.path().join("state"), &workspace);
        paths.initialize().unwrap();
        let runtime = runtime(&paths);
        let plugins = Plugins::new(paths.data_dir.join("plugins.json"));
        let executable = std::env::current_exe().unwrap();
        let descriptor = Descriptor {
            executable,
            argv: Vec::new(),
            manifest: manifest(PluginKind::External),
        };
        plugins
            .call(
                &paths,
                &runtime,
                "settings.plugins.install",
                json!({"name":null,"descriptor":descriptor}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        runtime
            .load_native(
                Arc::new(RetryCleanup {
                    manifest: manifest(PluginKind::Native),
                    waits: AtomicUsize::new(0),
                }),
                PluginContext::default(),
            )
            .unwrap();

        let first = plugins
            .call(
                &paths,
                &runtime,
                "settings.plugins.disable",
                json!({"name":"fixture"}),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(crate::cli_settings::error_outcome_unknown(first.as_ref()));
        assert_eq!(runtime.plugin_registry().pending_cleanup().len(), 1);
        assert!(
            read_cleanup(&paths.data_dir.join(CLEANUP_FILE))
                .unwrap()
                .1
                .get(&paths.workspace, "fixture")
                .and_then(|record| record.instance.as_ref())
                .is_some()
        );

        let second = plugins
            .call(
                &paths,
                &runtime,
                "settings.plugins.disable",
                json!({"name":"fixture"}),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(crate::cli_settings::error_outcome_unknown(second.as_ref()));
        assert_eq!(runtime.plugin_registry().pending_cleanup().len(), 1);
        assert!(
            read_cleanup(&paths.data_dir.join(CLEANUP_FILE))
                .unwrap()
                .1
                .get(&paths.workspace, "fixture")
                .is_some()
        );
        assert!(
            runtime
                .load_native(
                    Arc::new(RetryCleanup {
                        manifest: manifest(PluginKind::Native),
                        waits: AtomicUsize::new(0),
                    }),
                    PluginContext::default(),
                )
                .is_err()
        );

        let disabled = plugins
            .call(
                &paths,
                &runtime,
                "settings.plugins.disable",
                json!({"name":"fixture"}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(disabled["entries"][0]["enabled"], false);
        assert!(runtime.plugin_registry().pending_cleanup().is_empty());
        assert!(
            read_cleanup(&paths.data_dir.join(CLEANUP_FILE))
                .unwrap()
                .1
                .get(&paths.workspace, "fixture")
                .is_none()
        );
        runtime
            .load_native(
                Arc::new(RetryCleanup {
                    manifest: manifest(PluginKind::Native),
                    waits: AtomicUsize::new(0),
                }),
                PluginContext::default(),
            )
            .unwrap();
    }

    #[test]
    fn cleanup_unknown_without_exact_instance_remains_blocked() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let path = directory.path().join(CLEANUP_FILE);
        let mut cleanup = CleanupLedger::default();
        assert!(cleanup.insert(
            &workspace,
            "fixture",
            "synthetic cleanup outcome is unknown".into(),
            None,
            None,
        ));
        save_cleanup(&path, None, &cleanup).unwrap();

        let (_, reloaded) = read_cleanup(&path).unwrap();
        let blocked = reloaded.get(&workspace, "fixture").unwrap();
        assert_eq!(blocked.message, "synthetic cleanup outcome is unknown");
        assert!(blocked.instance.is_none());
        assert_eq!(reloaded.block_count(), 1);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        // Read-only startup inspection cannot convert an unavailable process
        // identity into cleanup evidence or remove the durable block.
        assert!(
            read_cleanup(&path)
                .unwrap()
                .1
                .get(&workspace, "fixture")
                .is_some()
        );
    }

    #[cfg(target_os = "macos")]
    fn save_fixture_inventory(plugins: &Plugins, manifest: PluginManifest) {
        let descriptor = validate(Descriptor {
            executable: std::env::current_exe().unwrap(),
            argv: Vec::new(),
            manifest,
        })
        .unwrap();
        let mut inventory = Inventory::default();
        inventory.entries.insert(
            "fixture".into(),
            Installed {
                executable_sha256: executable_digest(&descriptor).unwrap(),
                descriptor,
            },
        );
        save_inventory(&plugins.inventory_path, None, &inventory).unwrap();
    }

    #[tokio::test]
    async fn missing_executable_marks_one_entry_unavailable_and_keeps_management_usable() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let paths = local_paths(&directory.path().join("state"), &workspace);
        paths.initialize().unwrap();
        let runtime = runtime(&paths);
        let plugins = Plugins::new(paths.data_dir.join("plugins.json"));
        let executable = directory.path().join("missing-plugin");
        std::fs::write(&executable, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut fixture = manifest(PluginKind::External);
        fixture.name = "missing".into();
        let descriptor = validate(Descriptor {
            executable: executable.clone(),
            argv: Vec::new(),
            manifest: fixture,
        })
        .unwrap();
        plugins
            .call(
                &paths,
                &runtime,
                "settings.plugins.install",
                json!({"name":null,"descriptor":descriptor}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        std::fs::remove_file(&executable).unwrap();

        let listed = plugins
            .call(
                &paths,
                &runtime,
                "settings.plugins.list",
                json!({}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(listed["entries"][0]["name"], "missing");
        assert_eq!(listed["entries"][0]["executable_available"], false);
        assert!(listed["entries"][0]["executable_error"].is_string());
        assert_eq!(doctor(&paths).unwrap()["unavailable"][0]["name"], "missing");

        let enable = plugins
            .call(
                &paths,
                &runtime,
                "settings.plugins.enable",
                json!({"name":"missing","approve_process":true}),
                CancellationToken::new(),
            )
            .await;
        assert!(enable.is_err());
        assert!(runtime.plugin_registry().describe("missing").is_err());

        plugins
            .call(
                &paths,
                &runtime,
                "settings.plugins.remove",
                json!({"name":"missing"}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let listed = plugins
            .call(
                &paths,
                &runtime,
                "settings.plugins.list",
                json!({}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(listed["entries"].as_array().unwrap().is_empty());
        assert!(plugins.restore(&paths, &runtime).await.is_ok());
    }

    #[tokio::test]
    async fn missing_executable_records_activation_error_without_failing_client_startup() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let paths = local_paths(&directory.path().join("state"), &workspace);
        paths.initialize().unwrap();
        let runtime = runtime(&paths);
        let plugins = Plugins::new(paths.data_dir.join("plugins.json"));
        let mut fixture = manifest(PluginKind::External);
        fixture.name = "missing".into();
        let mut inventory = Inventory::default();
        inventory.entries.insert(
            "missing".into(),
            Installed {
                descriptor: Descriptor {
                    executable: workspace.join("missing-plugin"),
                    argv: Vec::new(),
                    manifest: fixture,
                },
                executable_sha256: "0".repeat(64),
            },
        );
        save_inventory(&plugins.inventory_path, None, &inventory).unwrap();
        let mut activations = Activations::default();
        activations.entries.insert(
            "missing".into(),
            Activation {
                enabled: true,
                last_error: None,
            },
        );
        save_activations(&paths.state_file(ACTIVATION_FILE), None, &activations).unwrap();

        plugins.restore(&paths, &runtime).await.unwrap();
        assert!(runtime.plugin_registry().describe("missing").is_err());
        let (_, persisted, _) = read_activations(&paths.state_file(ACTIVATION_FILE)).unwrap();
        let activation = &persisted.entries["missing"];
        assert!(activation.enabled);
        let error = activation.last_error.as_ref().unwrap();
        assert!(!error.outcome_unknown);
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn cleanup_block_stays_while_the_exact_process_identity_is_alive() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let paths = local_paths(&directory.path().join("state"), &workspace);
        paths.initialize().unwrap();
        let runtime = runtime(&paths);
        let plugins = Plugins::new(paths.data_dir.join("plugins.json"));
        save_fixture_inventory(&plugins, manifest(PluginKind::External));
        let command = std::env::current_exe().unwrap();
        let identity = PluginProcessIdentity::observe(
            std::process::id(),
            command.to_string_lossy().into_owned(),
        )
        .unwrap();
        let mut cleanup = CleanupLedger::default();
        cleanup.insert(
            &paths.workspace,
            "fixture",
            "cleanup is still unconfirmed".into(),
            None,
            Some(identity),
        );
        save_cleanup(&plugins.cleanup_path, None, &cleanup).unwrap();

        let error = plugins
            .call(
                &paths,
                &runtime,
                "settings.plugins.disable",
                json!({"name":"fixture"}),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(crate::cli_settings::error_outcome_unknown(error.as_ref()));
        let message = error.to_string();
        assert!(message.contains("still running"), "{message}");
        assert!(
            read_cleanup(&plugins.cleanup_path)
                .unwrap()
                .1
                .get(&paths.workspace, "fixture")
                .is_some_and(|record| record.process.is_some())
        );
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn cleanup_block_clears_only_after_the_exact_process_and_group_stop() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let paths = local_paths(&directory.path().join("state"), &workspace);
        paths.initialize().unwrap();
        let runtime = runtime(&paths);
        let plugins = Plugins::new(paths.data_dir.join("plugins.json"));
        save_fixture_inventory(&plugins, manifest(PluginKind::External));
        let identity = {
            use std::os::unix::process::CommandExt;
            let mut child = std::process::Command::new("/bin/sleep")
                .arg("60")
                .process_group(0)
                .spawn()
                .unwrap();
            let identity = PluginProcessIdentity::observe(child.id(), "/bin/sleep").unwrap();
            child.kill().unwrap();
            child.wait().unwrap();
            identity
        };
        let mut cleanup = CleanupLedger::default();
        cleanup.insert(
            &paths.workspace,
            "fixture",
            "cleanup is still unconfirmed".into(),
            None,
            Some(identity),
        );
        save_cleanup(&plugins.cleanup_path, None, &cleanup).unwrap();

        let listed = plugins
            .call(
                &paths,
                &runtime,
                "settings.plugins.disable",
                json!({"name":"fixture"}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(listed["entries"][0]["cleanup_blocked"], false);
        assert!(
            read_cleanup(&plugins.cleanup_path)
                .unwrap()
                .1
                .get(&paths.workspace, "fixture")
                .is_none()
        );
    }

    #[test]
    fn legacy_activation_errors_migrate_once_to_typed_unknown_state() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(ACTIVATION_FILE);
        let legacy = json!({
            "schema":LEGACY_ACTIVATION_SCHEMA,
            "entries":{
                "fixture":{
                    "enabled":true,
                    "last_error":"legacy startup failure"
                }
            }
        });
        std::fs::write(&path, serde_json::to_vec_pretty(&legacy).unwrap()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

        let (original, activations, migrated) = read_activations(&path).unwrap();
        assert!(migrated);
        assert_eq!(activations.schema, ACTIVATION_SCHEMA);
        let error = activations.entries["fixture"].last_error.as_ref().unwrap();
        assert_eq!(error.message, "legacy startup failure");
        assert!(error.outcome_unknown);

        save_activations(&path, original.as_deref(), &activations).unwrap();
        let (_, reloaded, migrated_again) = read_activations(&path).unwrap();
        assert!(!migrated_again);
        assert_eq!(reloaded.schema, ACTIVATION_SCHEMA);
        assert!(
            reloaded.entries["fixture"]
                .last_error
                .as_ref()
                .unwrap()
                .outcome_unknown
        );
    }
}
