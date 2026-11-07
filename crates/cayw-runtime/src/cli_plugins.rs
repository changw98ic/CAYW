//! Local External plugin inventory. Registration is separate from code execution.
use crate::cli_settings::uncertain;
use cayw_ai::CancellationToken;
use cayw_plugins::{ExternalConfig, PluginContext, PluginKind, PluginManifest};
use cayw_runtime::{
    Runtime,
    local_cli::{read_private, save_config},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, error::Error, io::Read, path::PathBuf};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

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
    enabled: bool,
    last_error: Option<String>,
}
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Inventory {
    entries: BTreeMap<String, Installed>,
}

pub struct Plugins {
    path: PathBuf,
}
impl Plugins {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
    fn read(&self) -> Result<(Option<Vec<u8>>, Inventory)> {
        let original = match read_private(&self.path, 1024 * 1024) {
            Ok(bytes) => Some(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        let inventory = original
            .as_deref()
            .map(serde_json::from_slice)
            .transpose()?
            .unwrap_or_default();
        Ok((original, inventory))
    }
    fn save(&self, original: Option<&[u8]>, inventory: &Inventory) -> Result<()> {
        save_config(&self.path, original, &serde_json::to_vec_pretty(inventory)?).map_err(uncertain)
    }
    /// Replay only an explicitly approved local executable with identical bytes.
    /// A failed handshake is retained as an error and never silently retried.
    pub async fn restore(&mut self, runtime: &Runtime) -> Result<()> {
        let (original, mut inventory) = self.read()?;
        let mut changed = false;
        for entry in inventory.entries.values_mut().filter(|entry| entry.enabled) {
            if let Err(error) = load(runtime, entry, CancellationToken::new()).await {
                entry.enabled = false;
                entry.last_error = Some(error.to_string());
                changed = true;
            }
        }
        if changed {
            self.save(original.as_deref(), &inventory)?;
        }
        Ok(())
    }
    pub async fn call(
        &mut self,
        runtime: &Runtime,
        method: &str,
        params: Value,
        cancel: CancellationToken,
    ) -> Result<Value> {
        let (original, mut inventory) = self.read()?;
        match method {
            "settings.plugins.bind" => {
                let binding: Bind = serde_json::from_value(params)?;
                return Ok(serde_json::to_value(
                    runtime.bind_plugin(&binding.session_id, &binding.name)?,
                )?);
            }
            "settings.plugins.list" => {
                let _: Empty = serde_json::from_value(params)?;
            }
            "settings.plugins.install" | "settings.plugins.update" => {
                let put: Put = serde_json::from_value(params)?;
                let descriptor = validate(put.descriptor)?;
                let id = descriptor.manifest.name.clone();
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
                    let old = inventory
                        .entries
                        .get(&id)
                        .ok_or("plugin is not installed")?;
                    if old.enabled || runtime.plugin_registry().describe(&id).is_ok() {
                        return Err(
                            "disable the plugin before updating; bound sessions stay frozen".into(),
                        );
                    }
                }
                let digest = executable_digest(&descriptor)?;
                inventory.entries.insert(
                    id,
                    Installed {
                        descriptor,
                        executable_sha256: digest,
                        enabled: false,
                        last_error: None,
                    },
                );
            }
            "settings.plugins.enable" => {
                let enable: Enable = serde_json::from_value(params)?;
                if !enable.approve_process {
                    return Err("explicit process approval is required: local code runs as you, without a security sandbox; declared plugin capabilities remain subject to Host approvals and budgets".into());
                }
                let entry = inventory
                    .entries
                    .get_mut(&enable.name)
                    .ok_or("plugin is not installed")?;
                if entry.enabled {
                    return Err("plugin already enabled; inspect live status".into());
                }
                match load(runtime, entry, cancel).await {
                    Ok(()) => {
                        entry.enabled = true;
                        entry.last_error = None;
                    }
                    Err(error) => {
                        entry.last_error = Some(error.to_string());
                        self.save(original.as_deref(), &inventory)?;
                        return Err(error);
                    }
                }
                if let Err(error) = self.save(original.as_deref(), &inventory) {
                    if let Err(cleanup) = runtime.unload_plugin(&enable.name) {
                        return Err(uncertain(std::io::Error::other(format!(
                            "{error}; plugin cleanup also failed: {cleanup}"
                        ))));
                    }
                    return Err(error);
                }
                return self.status(runtime, inventory);
            }
            "settings.plugins.disable" | "settings.plugins.remove" => {
                let id: Name = serde_json::from_value(params)?;
                let entry = inventory
                    .entries
                    .get_mut(&id.name)
                    .ok_or("plugin is not installed")?;
                // Persist the user's stop intent first: a failed checkpoint or
                // unload must not cause this executable to restart automatically.
                entry.enabled = false;
                self.save(original.as_deref(), &inventory)?;
                let (disabled_bytes, mut inventory) = self.read()?;
                if runtime.plugin_registry().describe(&id.name).is_ok() {
                    // Runtime checkpoints and freezes bound sessions before unload.
                    if let Err(error) = runtime.unload_plugin(&id.name) {
                        if let Some(entry) = inventory.entries.get_mut(&id.name) {
                            entry.last_error = Some(error.to_string());
                        }
                        self.save(disabled_bytes.as_deref(), &inventory)?;
                        return Err(error.into());
                    }
                }
                if method.ends_with("remove") {
                    inventory.entries.remove(&id.name);
                } else if let Some(entry) = inventory.entries.get_mut(&id.name) {
                    entry.last_error = None;
                }
                self.save(disabled_bytes.as_deref(), &inventory)?;
                return self.status(runtime, inventory);
            }
            _ => return Err("unsupported plugin settings method".into()),
        }
        if method != "settings.plugins.list" {
            self.save(original.as_deref(), &inventory)?;
        }
        self.status(runtime, inventory)
    }
    fn status(&self, runtime: &Runtime, inventory: Inventory) -> Result<Value> {
        let entries: Vec<Value> = inventory.entries.into_iter().map(|(name, entry)| {
            let live = runtime.plugin_registry().check_available(&name);
            json!({"name":name,"descriptor":entry.descriptor,"executable_sha256":entry.executable_sha256,"enabled":entry.enabled,"available":live.is_ok(),"last_error":entry.last_error.or_else(|| if entry.enabled { live.err().map(|e| e.to_string()) } else {None})})
        }).collect();
        Ok(
            json!({"entries":entries,"loaded":runtime.list_plugins(),"supported_source":"local_external","protocol_version":cayw_plugins::PLUGIN_PROTOCOL_VERSION}),
        )
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Bind {
    session_id: String,
    name: String,
}
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
fn validate(mut descriptor: Descriptor) -> Result<Descriptor> {
    descriptor.manifest.validate()?;
    if descriptor.manifest.kind != PluginKind::External || !descriptor.executable.is_absolute() {
        return Err("only local External plugins with an absolute executable path are supported; npm, URLs and Native dynamic installation are unsupported".into());
    }
    if descriptor.argv.len() > 128
        || descriptor
            .argv
            .iter()
            .any(|arg| arg.len() > 4096 || arg.contains('\0'))
    {
        return Err("invalid plugin arguments".into());
    }
    descriptor.executable = descriptor.executable.canonicalize()?;
    Ok(descriptor)
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
async fn load(runtime: &Runtime, entry: &Installed, cancel: CancellationToken) -> Result<()> {
    let descriptor = validate(entry.descriptor.clone())?;
    if executable_digest(&descriptor)? != entry.executable_sha256 {
        return Err("plugin executable changed since registration; disable and explicitly update it before enabling".into());
    }
    let capabilities = descriptor
        .manifest
        .capabilities
        .iter()
        .map(|c| c.name.clone())
        .collect::<Vec<_>>();
    let permissions = descriptor
        .manifest
        .permissions
        .values()
        .flatten()
        .cloned()
        .collect::<Vec<_>>();
    let config = ExternalConfig::new(descriptor.executable, descriptor.argv, descriptor.manifest);
    runtime
        .load_external(
            config,
            PluginContext::new(capabilities, permissions),
            cancel,
        )
        .await
        .map_err(uncertain)?;
    Ok(())
}
