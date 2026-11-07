//! Authenticated, host-only product settings. Never registered as model tools.
use cayw_ai::CancellationToken;
use cayw_ipc::{Client, RpcError};
use cayw_runtime::{
    ProviderConfig, Runtime,
    local_cli::{LocalPaths, read_private, save_config},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, error::Error, path::PathBuf};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

#[derive(Debug, thiserror::Error)]
#[error(transparent)]
struct Uncertain(Box<dyn Error>);

/// The management action crossed a process/persistence boundary. Preserve the
/// original cause and require inspection instead of implying a safe replay.
pub fn uncertain(error: impl Into<Box<dyn Error>>) -> Box<dyn Error> {
    Box::new(Uncertain(error.into()))
}

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Models {
    selected: Option<String>,
    models: BTreeMap<String, ProviderConfig>,
}

pub struct Settings {
    pub paths: LocalPaths,
    pub workspaces: crate::cli_workspaces::Workspaces,
    pub pi_enabled: bool,
    pub offline: bool,
    pub available: bool,
    #[cfg(feature = "plugins")]
    pub plugins: crate::cli_plugins::Plugins,
}

fn parse<T: serde::de::DeserializeOwned>(params: Value) -> Result<T> {
    Ok(serde_json::from_value(params)?)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Name {
    name: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Put {
    name: String,
    model: ProviderConfig,
}

fn name(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 80
        || !value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'))
    {
        return Err("profile name must contain 1..80 letters, digits, '.', '_' or '-'".into());
    }
    Ok(())
}

fn read_models(paths: &LocalPaths) -> Result<(Option<Vec<u8>>, Models)> {
    let original = match read_private(&paths.state_file("models.json"), 1024 * 1024) {
        Ok(bytes) => Some(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    let models: Models = original
        .as_deref()
        .map(serde_json::from_slice)
        .transpose()?
        .unwrap_or_default();
    for (id, model) in &models.models {
        name(id)?;
        model.validate()?;
    }
    if models
        .selected
        .as_ref()
        .is_some_and(|id| !models.models.contains_key(id))
    {
        return Err("selected model profile is missing".into());
    }
    Ok((original, models))
}

pub fn selected_model(paths: &LocalPaths) -> Result<Option<ProviderConfig>> {
    let (_, models) = read_models(paths)?;
    models
        .selected
        .as_ref()
        .map(|id| {
            models
                .models
                .get(id)
                .cloned()
                .ok_or_else(|| "selected model profile is missing".into())
        })
        .transpose()
}

/// configure and live management share the same persisted selected profile.
/// This stores metadata only; an offline CLI never reads model credentials.
pub fn save_configured_model(paths: &LocalPaths, model: ProviderConfig) -> Result<()> {
    model.validate()?;
    let (original, mut models) = read_models(paths)?;
    if models.models.len() >= 64 && !models.models.contains_key("configured") {
        return Err(
            "at most 64 model profiles are supported; remove an unused profile before configuring"
                .into(),
        );
    }
    models.models.insert("configured".into(), model);
    models.selected = Some("configured".into());
    save_config(
        &paths.state_file("models.json"),
        original.as_deref(),
        &serde_json::to_vec_pretty(&models)?,
    )
    .map_err(uncertain)
}

/// Reload only from the persisted profile. An absent connection is offline;
/// malformed discovery, transport loss and rejected activation remain explicit.
pub async fn reload_running_model(paths: &LocalPaths) -> Value {
    let reload: Result<Option<Value>> = async {
        match paths.connection_file.symlink_metadata() {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        }
        let (address, token) = paths.connect()?;
        let client = Client::connect(address, token).await?;
        Ok(Some(
            client.request("settings.models.reload", json!({})).await?,
        ))
    }
    .await;
    match reload {
        Ok(None) => Value::Null,
        Ok(Some(_)) => json!({"applied":true}),
        Err(error) => {
            let unknown = error
                .downcast_ref::<cayw_ipc::ClientError>()
                .is_none_or(|error| error.outcome_unknown);
            json!({"applied":if unknown { Value::Null } else { json!(false) },"outcome_unknown":unknown,"error":error.to_string()})
        }
    }
}

impl Settings {
    fn model_path(&self) -> PathBuf {
        self.paths.state_file("models.json")
    }
    pub fn selected_model(&self) -> Result<Option<ProviderConfig>> {
        selected_model(&self.paths)
    }
    fn models(&self) -> Result<(Option<Vec<u8>>, Models)> {
        read_models(&self.paths)
    }
    pub async fn call(
        &mut self,
        runtime: &Runtime,
        method: &str,
        params: Value,
        cancel: CancellationToken,
    ) -> std::result::Result<Value, RpcError> {
        self.call_inner(runtime, method, params, cancel)
            .await
            .map_err(|error| {
                let code = if error.is::<Uncertain>() {
                    "settings_outcome_unknown"
                } else if let Some(error) = error.downcast_ref::<cayw_runtime::RuntimeError>() {
                    error.code()
                } else {
                    "settings_error"
                };
                RpcError::new(code, error.to_string())
            })
    }
    async fn call_inner(
        &mut self,
        runtime: &Runtime,
        method: &str,
        params: Value,
        _cancel: CancellationToken,
    ) -> Result<Value> {
        if method.starts_with("settings.workspaces.") {
            return self.workspaces.call(method, params, _cancel).await;
        }
        if method == "settings.tools" {
            let _: Empty = parse(params)?;
            return crate::cli_workspaces::optional_tools(self.pi_enabled);
        }
        if method == "settings.workspace" {
            let _: Empty = parse(params)?;
            return Ok(
                json!({"workspace":self.paths.workspace,"database":self.paths.database,"offline":self.offline,"model_available":self.available,"known_workspaces":self.paths.known_workspaces()?}),
            );
        }
        #[cfg(feature = "plugins")]
        if method.starts_with("settings.plugins.") {
            return self.plugins.call(runtime, method, params, _cancel).await;
        }
        if !method.starts_with("settings.models.") {
            return Err(
                "unsupported settings method (optional plugins require the plugins build feature)"
                    .into(),
            );
        }
        let (original, mut models) = self.models()?;
        let mut selection = None;
        match method {
            "settings.models.list" => {
                let _: Empty = parse(params)?;
            }
            "settings.models.reload" => {
                let _: Empty = parse(params)?;
                if self.offline {
                    return Err("offline fixture cannot select a real model".into());
                }
                let id = models
                    .selected
                    .as_ref()
                    .ok_or("no selected model profile")?;
                let model = models
                    .models
                    .get(id)
                    .ok_or("selected model profile is missing")?;
                runtime.select_model(model.build()?)?;
                self.available = true;
            }
            "settings.models.put" => {
                let put: Put = parse(params)?;
                name(&put.name)?;
                put.model.validate()?;
                if models.models.len() >= 64 && !models.models.contains_key(&put.name) {
                    return Err("at most 64 model profiles are supported".into());
                }
                if models.selected.as_ref() == Some(&put.name) {
                    if self.offline {
                        return Err("offline fixture cannot select a real model".into());
                    }
                    selection = Some(put.model.build()?);
                }
                models.models.insert(put.name, put.model);
            }
            "settings.models.use" => {
                let selected: Name = parse(params)?;
                if self.offline {
                    return Err("offline fixture cannot select a real model".into());
                }
                let model = models
                    .models
                    .get(&selected.name)
                    .ok_or("model profile not found")?;
                selection = Some(model.build()?);
                models.selected = Some(selected.name);
            }
            "settings.models.remove" => {
                let selected: Name = parse(params)?;
                if models.selected.as_ref() == Some(&selected.name) {
                    return Err("select another model before removing the active profile".into());
                }
                if models.models.remove(&selected.name).is_none() {
                    return Err("model profile not found".into());
                }
            }
            _ => return Err("unsupported model settings method".into()),
        }
        if !matches!(method, "settings.models.list" | "settings.models.reload") {
            save_config(
                &self.model_path(),
                original.as_deref(),
                &serde_json::to_vec_pretty(&models)?,
            )
            .map_err(uncertain)?;
            if let Some(selection) = selection {
                runtime.select_model(selection)?;
                self.available = true;
            }
        }
        Ok(
            json!({"selected":models.selected,"models":models.models,"health":runtime.health()?,"offline":self.offline}),
        )
    }
}

/// CLI consumes exactly the same authenticated management path as the WebUI.
pub async fn run(mut args: impl Iterator<Item = String>, plugin: bool) -> Result<()> {
    let action = args.next().ok_or("expected list / add / use / remove (plugins: list / install / update / enable / disable / remove)")?;
    if action == "--help" {
        println!(
            "cayw models list|add NAME --file MODEL.json|use NAME|remove NAME [--workspace DIR]\ncayw plugins list|install --file LOCAL.json|update NAME --file LOCAL.json|enable NAME --approve-process|disable NAME|remove NAME|bind NAME --session ID [--workspace DIR]\nLocal External descriptors only; no downloads. Install/update do not execute code. Enable requires explicit process approval."
        );
        return Ok(());
    }
    let mut workspace = std::env::current_dir()?;
    let mut id = None;
    let mut file = None;
    let mut approve = false;
    let mut session = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--workspace" => workspace = PathBuf::from(args.next().ok_or("missing workspace")?),
            "--file" => file = Some(PathBuf::from(args.next().ok_or("missing file")?)),
            "--approve-process" if plugin => approve = true,
            "--session" if plugin => session = Some(args.next().ok_or("missing session ID")?),
            _ if !arg.starts_with('-') && id.is_none() => id = Some(arg),
            _ => return Err(format!("unexpected argument {arg}").into()),
        }
    }
    let paths = LocalPaths::from_environment(workspace)?;
    let (address, token) = paths.connect()?;
    let client = Client::connect(address, token).await?;
    let method_action = if action == "add" && !plugin {
        "put"
    } else {
        &action
    };
    let mut params = serde_json::Map::new();
    if let Some(id) = id {
        params.insert("name".into(), json!(id));
    }
    if let Some(path) = file {
        let bytes = std::fs::read(path)?;
        if bytes.len() > 256 * 1024 {
            return Err("descriptor exceeds 256 KiB".into());
        }
        params.insert(
            if plugin { "descriptor" } else { "model" }.into(),
            serde_json::from_slice(&bytes)?,
        );
    }
    if let Some(session) = session {
        params.insert("session_id".into(), json!(session));
    }
    if approve {
        params.insert("approve_process".into(), json!(true));
    }
    let method = format!(
        "settings.{}.{method_action}",
        if plugin { "plugins" } else { "models" }
    );
    println!(
        "{}",
        serde_json::to_string_pretty(&client.request(&method, Value::Object(params)).await?)?
    );
    Ok(())
}
