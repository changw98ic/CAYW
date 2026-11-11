//! Authenticated, host-only product settings. Never registered as model tools.
use cayw_ai::CancellationToken;
use cayw_ipc::{Client, RpcError};
use cayw_runtime::{
    ProviderConfig, Runtime,
    local_cli::{LocalPaths, read_private, save_config, save_config_locked},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    error::Error,
    io,
    path::{Path, PathBuf},
};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

#[derive(Debug, thiserror::Error)]
#[error(transparent)]
pub(crate) struct Uncertain(Box<dyn Error + Send + Sync>);

/// The management action crossed a process/persistence boundary. Preserve the
/// original cause and require inspection instead of implying a safe replay.
pub(crate) fn uncertain(error: impl Into<Box<dyn Error + Send + Sync>>) -> Uncertain {
    Uncertain(error.into())
}

pub(crate) fn runtime_error<'a>(
    error: &'a (dyn Error + 'static),
) -> Option<&'a cayw_runtime::RuntimeError> {
    error
        .downcast_ref::<cayw_runtime::RuntimeError>()
        .or_else(|| error.source().and_then(runtime_error))
}

pub(crate) fn error_outcome_unknown(error: &(dyn Error + 'static)) -> bool {
    if error.is::<Uncertain>() {
        return true;
    }
    #[cfg(feature = "plugins")]
    if error
        .downcast_ref::<PluginSettingsError>()
        .is_some_and(|error| error.outcome_unknown)
    {
        return true;
    }
    if let Some(error) = runtime_error(error) {
        return error.outcome_unknown();
    }
    error.source().is_some_and(error_outcome_unknown)
}

#[cfg(feature = "plugins")]
#[derive(Debug, thiserror::Error)]
#[error("{source}")]
struct PluginSettingsError {
    #[source]
    source: Box<dyn Error + Send + Sync>,
    outcome_unknown: bool,
}

#[cfg(feature = "plugins")]
impl PluginSettingsError {
    fn new(source: Box<dyn Error + Send + Sync>) -> Self {
        let outcome_unknown = error_outcome_unknown(source.as_ref());
        Self {
            source,
            outcome_unknown,
        }
    }
}

fn settings_error_code(error: &(dyn Error + 'static)) -> &'static str {
    if let Some(error) = runtime_error(error) {
        error.code()
    } else if error_outcome_unknown(error) {
        "settings_outcome_unknown"
    } else {
        "settings_error"
    }
}

/// Device catalog at `<CAYW_HOME>/models.json`. One list for every workspace.
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Models {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    default: Option<String>,
    #[serde(default)]
    models: BTreeMap<String, ProviderConfig>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SelectionFile {
    name: String,
}

enum SelectionState {
    Missing,
    Present(String),
}

/// The profile this workspace will actually use.
pub struct ModelChoice {
    pub model: Option<ProviderConfig>,
    pub fallback: Option<String>,
}

pub struct Settings {
    pub paths: LocalPaths,
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
    model: Value,
    #[serde(default)]
    credential: Option<CredentialUpdate>,
    #[serde(default)]
    clear_api_key: bool,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CredentialUpdate {
    Keep,
    Replace,
    Remove,
}

impl Put {
    fn resolve_model(&self, previous: Option<&ProviderConfig>) -> Result<ProviderConfig> {
        // Keep old callers compatible: a supplied key replaces, explicit null
        // removes, and an omitted key preserves an existing profile. Omission
        // still means no authentication when creating a new profile.
        let credential = if self.clear_api_key {
            if self.model.get("api_key").is_some_and(|key| !key.is_null()) {
                return Err("cannot set and clear api_key in the same update".into());
            }
            if matches!(
                self.credential,
                Some(CredentialUpdate::Keep | CredentialUpdate::Replace)
            ) {
                return Err("clear_api_key contradicts the credential action".into());
            }
            CredentialUpdate::Remove
        } else {
            self.credential
                .unwrap_or_else(|| match self.model.get("api_key") {
                    Some(Value::Null) => CredentialUpdate::Remove,
                    Some(_) => CredentialUpdate::Replace,
                    None => CredentialUpdate::Keep,
                })
        };
        let mut model = self.model.clone();
        match credential {
            CredentialUpdate::Keep => {
                if model.get("api_key").is_some() {
                    return Err("keep credential must not include api_key".into());
                }
                if let Some(previous) = previous {
                    if previous.has_api_key() {
                        let public = previous.public_view();
                        // Compare the full endpoint and adapter, including path
                        // and scheme. Never reuse a stored secret at a changed
                        // destination, even when the caller explicitly says keep.
                        if ["service", "api", "base_url", "api_version"]
                            .iter()
                            .any(|field| model.get(*field) != public.get(*field))
                        {
                            return Err("endpoint or adapter changed; explicitly replace or remove the credential".into());
                        }
                        model["api_key"] = serde_json::to_value(previous)?["api_key"].clone();
                    }
                } else if self.credential.is_some() {
                    return Err("cannot keep a credential for a missing model profile".into());
                }
            }
            CredentialUpdate::Replace => {
                if model.get("api_key").and_then(Value::as_str).is_none() {
                    return Err("replace credential requires a nonempty api_key".into());
                }
            }
            CredentialUpdate::Remove => {
                if model.get("api_key").is_some_and(|key| !key.is_null()) {
                    return Err("remove credential must not include an api_key value".into());
                }
            }
        }
        if let Some(previous) = previous {
            let previous_json = serde_json::to_value(previous)?;
            for field in ["context_window", "tokenizer"] {
                if model.get(field).is_none()
                    && let Some(existing) = previous_json.get(field).filter(|v| !v.is_null())
                {
                    model[field] = existing.clone();
                }
            }
        }
        let model: ProviderConfig = parse(model)?;
        model.validate()?;
        Ok(model)
    }
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

fn put_model_profile(models: &mut Models, put: Put) -> Result<ProviderConfig> {
    name(&put.name)?;
    let model = put.resolve_model(models.models.get(&put.name))?;
    if models.models.len() >= 64 && !models.models.contains_key(&put.name) {
        return Err("at most 64 model profiles are supported".into());
    }
    models.models.insert(put.name, model.clone());
    Ok(model)
}

const MODEL_FILE_LIMIT: u64 = 1024 * 1024;

fn device_models_path(paths: &LocalPaths) -> PathBuf {
    paths.data_dir.join("models.json")
}

fn read_optional(path: &Path) -> Result<Option<Vec<u8>>> {
    match read_private(path, MODEL_FILE_LIMIT) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn pretty(value: &impl Serialize) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn validate_catalog(models: &Models) -> Result<()> {
    for (id, model) in &models.models {
        name(id)?;
        model.validate()?;
    }
    if models
        .default
        .as_ref()
        .is_some_and(|id| !models.models.contains_key(id))
    {
        return Err("device default model profile is missing".into());
    }
    if models.models.len() > 64 {
        return Err("at most 64 model profiles are supported".into());
    }
    Ok(())
}

fn read_catalog_bytes(path: &Path) -> Result<(Option<Vec<u8>>, Models)> {
    let original = read_optional(path)?;
    let models: Models = original
        .as_deref()
        .map(serde_json::from_slice)
        .transpose()?
        .unwrap_or_default();
    validate_catalog(&models)?;
    Ok((original, models))
}

fn read_catalog(paths: &LocalPaths) -> Result<(Option<Vec<u8>>, Models)> {
    read_catalog_bytes(&device_models_path(paths))
}

fn selection_state(path: &Path) -> Result<SelectionState> {
    match read_optional(path)? {
        None => Ok(SelectionState::Missing),
        Some(bytes) => {
            let file: SelectionFile = serde_json::from_slice(&bytes)?;
            name(&file.name)?;
            Ok(SelectionState::Present(file.name))
        }
    }
}

fn resolve_choice(models: &Models, requested: Option<&str>) -> Result<ModelChoice> {
    match requested {
        Some(id) if models.models.contains_key(id) => Ok(ModelChoice {
            model: models.models.get(id).cloned(),
            fallback: None,
        }),
        Some(id) => {
            let fallback = match models.default.as_deref() {
                Some(default) if models.models.contains_key(default) => {
                    format!("模型方案 {id} 不存在，已改用设备默认方案 {default}")
                }
                Some(_) => return Err("device default model profile is missing".into()),
                None => format!("模型方案 {id} 不存在，设备没有默认方案"),
            };
            let model = models
                .default
                .as_ref()
                .and_then(|default| models.models.get(default).cloned());
            Ok(ModelChoice {
                model,
                fallback: Some(fallback),
            })
        }
        None => {
            if models
                .default
                .as_ref()
                .is_some_and(|id| !models.models.contains_key(id))
            {
                return Err("device default model profile is missing".into());
            }
            Ok(ModelChoice {
                model: models
                    .default
                    .as_ref()
                    .and_then(|id| models.models.get(id).cloned()),
                fallback: None,
            })
        }
    }
}

fn effective_name(models: &Models, requested: Option<&str>) -> Option<String> {
    match requested {
        Some(id) if models.models.contains_key(id) => Some(id.to_owned()),
        _ => models
            .default
            .clone()
            .filter(|id| models.models.contains_key(id)),
    }
}

/// Profile this workspace uses from the device catalog.
pub fn model_choice(paths: &LocalPaths) -> Result<ModelChoice> {
    let (_, models) = read_catalog(paths)?;
    let requested = match selection_state(&paths.state_file("model-selection.json"))? {
        SelectionState::Present(name) => Some(name),
        SelectionState::Missing => None,
    };
    resolve_choice(&models, requested.as_deref())
}

pub fn selected_model(paths: &LocalPaths) -> Result<Option<ProviderConfig>> {
    Ok(model_choice(paths)?.model)
}

fn public_models(models: &Models, selected: Option<&str>, fallback: Option<&str>) -> Value {
    let profiles = models
        .models
        .iter()
        .map(|(name, model)| (name.clone(), model.public_view()))
        .collect::<serde_json::Map<_, _>>();
    let missing_window = models
        .models
        .iter()
        .filter(|(_, model)| !model.has_context_window())
        .map(|(name, _)| name.clone())
        .collect::<Vec<_>>();
    let mut response = json!({
        "selected": selected,
        "default": models.default,
        "models": profiles,
    });
    if !missing_window.is_empty() {
        response["context_window_missing"] = json!(missing_window);
    }
    if let Some(fallback) = fallback {
        response["fallback"] = json!(fallback);
    }
    response
}

fn write_selection(paths: &LocalPaths, profile_name: &str) -> Result<()> {
    name(profile_name)?;
    let path = paths.state_file("model-selection.json");
    let original = read_optional(&path)?;
    save_config(
        &path,
        original.as_deref(),
        &pretty(&SelectionFile {
            name: profile_name.to_owned(),
        })?,
    )
    .map_err(uncertain)?;
    Ok(())
}

/// `cayw conf` writes the device catalog, sets its default, and selects that
/// profile for this workspace. An offline CLI never reads model credentials.
pub fn save_configured_model(paths: &LocalPaths, model: ProviderConfig) -> Result<()> {
    model.validate()?;
    let _lock = paths.lock_data_dir().map_err(uncertain)?;
    let (original, mut models) = read_catalog(paths)?;
    if models.models.len() >= 64 && !models.models.contains_key("configured") {
        return Err(
            "at most 64 model profiles are supported; remove an unused profile before configuring"
                .into(),
        );
    }
    models.models.insert("configured".into(), model);
    models.default = Some("configured".into());
    validate_catalog(&models)?;
    save_config_locked(
        &device_models_path(paths),
        original.as_deref(),
        &pretty(&models)?,
    )
    .map_err(uncertain)?;
    drop(_lock);
    write_selection(paths, "configured")
}

/// Remember the one main session for this workspace. A missing record adopts
/// the only existing session, or creates one. Several sessions and no record
/// is an error: this does not pick or create another.
pub fn ensure_main_session(runtime: &Runtime, paths: &LocalPaths) -> Result<String> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Record {
        session_id: String,
    }
    let path = paths.state_file("main-session.json");
    match read_optional(&path)? {
        Some(bytes) => {
            let record: Record = serde_json::from_slice(&bytes)?;
            runtime.get(&record.session_id).map_err(|_| {
                format!(
                    "主 session {} 不在这个工作区里，未另建会话",
                    record.session_id
                )
            })?;
            Ok(record.session_id)
        }
        None => {
            let sessions = runtime.list()?;
            let id = match sessions.len() {
                0 => runtime.create(Some("主 session".into()))?.id,
                1 => sessions.into_iter().next().expect("one session").id,
                _ => {
                    return Err("主 session 工作区里已有多个会话，未自动挑选或新建".into());
                }
            };
            save_config(&path, None, &pretty(&json!({"session_id": id}))?).map_err(uncertain)?;
            Ok(id)
        }
    }
}

async fn request_for_workspace(
    client: &Client,
    paths: &LocalPaths,
    method: &str,
    params: Value,
) -> Result<Value> {
    let catalog = client.request("client.workspaces.list", json!({})).await?;
    let workspace_id = catalog["workspaces"]
        .as_array()
        .and_then(|workspaces| {
            workspaces.iter().find_map(|entry| {
                (entry["path"].as_str() == paths.workspace.to_str())
                    .then(|| entry["id"].as_str().map(str::to_owned))
                    .flatten()
            })
        })
        .ok_or("workspace is not registered in the local Client")?;
    client
        .request("client.workspaces.open", json!({"id":workspace_id}))
        .await?;
    if workspace_id == "local" {
        Ok(client.request(method, params).await?)
    } else {
        Ok(client
            .request(
                "client.workspace.call",
                json!({
                    "workspace_id":workspace_id,
                    "rpc":{"method":method,"params":params}
                }),
            )
            .await?)
    }
}

/// Reload only from the persisted profile. An absent connection is offline;
/// malformed discovery, transport loss and rejected activation remain explicit.
pub async fn reload_running_model(paths: &LocalPaths) -> Value {
    let reload: Result<Option<Value>> = async {
        match paths.client_file.symlink_metadata() {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        }
        let (address, token) = paths.connect()?;
        let client = Client::connect(address, token).await?;
        let result =
            request_for_workspace(&client, paths, "settings.models.reload", json!({})).await?;
        client.close().await;
        Ok(Some(result))
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
                let code = settings_error_code(error.as_ref());
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
            return self
                .plugins
                .call(&self.paths, runtime, method, params, _cancel)
                .await
                .map_err(|error| Box::new(PluginSettingsError::new(error)) as Box<dyn Error>);
        }
        if !method.starts_with("settings.models.") {
            return Err(
                "unsupported settings method (optional plugins require the plugins build feature)"
                    .into(),
            );
        }
        let _lock = self.paths.lock_data_dir().map_err(uncertain)?;
        let (original, mut models) = read_catalog(&self.paths)?;
        let requested = match selection_state(&self.paths.state_file("model-selection.json"))? {
            SelectionState::Present(name) => Some(name),
            SelectionState::Missing => None,
        };
        let mut write_catalog = false;
        let mut next_selection = None;
        let mut hot_reload = None;
        match method {
            "settings.models.list" => {
                let _: Empty = parse(params)?;
            }
            "settings.models.reload" => {
                let _: Empty = parse(params)?;
                if self.offline {
                    return Err("offline fixture cannot select a real model".into());
                }
                let id = effective_name(&models, requested.as_deref())
                    .ok_or("no selected model profile")?;
                let model = models
                    .models
                    .get(&id)
                    .ok_or("selected model profile is missing")?;
                hot_reload = Some(model.build()?);
            }
            "settings.models.put" => {
                let put: Put = parse(params)?;
                let selected_name = put.name.clone();
                let model = put_model_profile(&mut models, put)?;
                write_catalog = true;
                if effective_name(&models, requested.as_deref()).as_deref()
                    == Some(selected_name.as_str())
                {
                    if self.offline {
                        return Err("offline fixture cannot select a real model".into());
                    }
                    hot_reload = Some(model.build()?);
                }
            }
            "settings.models.use" => {
                let selected: Name = parse(params)?;
                if self.offline {
                    return Err("offline fixture cannot select a real model".into());
                }
                let model = models
                    .models
                    .get(&selected.name)
                    .ok_or("model profile not found")?
                    .clone();
                hot_reload = Some(model.build()?);
                next_selection = Some(selected.name);
            }
            "settings.models.remove" => {
                let selected: Name = parse(params)?;
                if effective_name(&models, requested.as_deref()).as_deref()
                    == Some(selected.name.as_str())
                {
                    return Err("select another model before removing the active profile".into());
                }
                if models.default.as_deref() == Some(selected.name.as_str()) {
                    return Err("choose another device default before removing this profile".into());
                }
                if models.models.remove(&selected.name).is_none() {
                    return Err("model profile not found".into());
                }
                write_catalog = true;
            }
            _ => return Err("unsupported model settings method".into()),
        }
        if write_catalog {
            validate_catalog(&models)?;
            save_config_locked(
                &device_models_path(&self.paths),
                original.as_deref(),
                &pretty(&models)?,
            )
            .map_err(uncertain)?;
        }
        drop(_lock);
        if let Some(profile_name) = next_selection.as_deref() {
            write_selection(&self.paths, profile_name)?;
        }
        let reloaded = hot_reload.is_some();
        if let Some(selection) = hot_reload {
            runtime.select_model(selection)?;
            self.available = true;
        }
        let shown = next_selection.or(requested);
        let choice = resolve_choice(&models, shown.as_deref())?;
        let mut response = public_models(
            &models,
            effective_name(&models, shown.as_deref()).as_deref(),
            choice.fallback.as_deref(),
        );
        if let Some(warning) = choice
            .model
            .as_ref()
            .and_then(|model| model.context_window_warning(runtime.is_main_session()))
        {
            if reloaded {
                eprintln!("{warning}");
            }
            response["context_window_warning"] = json!(warning);
        }
        response["health"] = serde_json::to_value(runtime.health()?)?;
        response["offline"] = json!(self.offline);
        Ok(response)
    }
}

/// CLI consumes exactly the same authenticated management path as the WebUI.
pub async fn run(mut args: impl Iterator<Item = String>, plugin: bool) -> Result<()> {
    let action = args.next().ok_or("expected list / add / use / remove (plugins: list / install / update / enable / disable / remove / clear)")?;
    if action == "--help" {
        println!(
            "cayw models list|add NAME --file MODEL.json|use NAME|remove NAME [--workspace DIR]\nFor an existing model profile, omitting api_key preserves its credential; api_key:null explicitly removes it. Changing the endpoint or adapter requires a supplied key or explicit null. New profiles without api_key use no authentication.\ncayw plugins list|install --file LOCAL.json|update NAME --file LOCAL.json|enable NAME --approve-process|disable NAME|remove NAME|clear NAME [--workspace DIR]\nLocal External workspace architecture descriptors only; no downloads. Install/update do not execute code. Enable requires explicit process approval. Clear discards a cleanup block only after the recorded process is confirmed stopped; it leaves the enabled flag unchanged."
        );
        return Ok(());
    }
    let mut workspace = std::env::current_dir()?;
    let mut id = None;
    let mut file = None;
    let mut approve = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--workspace" => workspace = PathBuf::from(args.next().ok_or("missing workspace")?),
            "--file" => file = Some(PathBuf::from(args.next().ok_or("missing file")?)),
            "--approve-process" if plugin => approve = true,
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
    if approve {
        params.insert("approve_process".into(), json!(true));
    }
    let method = format!(
        "settings.{}.{method_action}",
        if plugin { "plugins" } else { "models" }
    );
    let response = request_for_workspace(&client, &paths, &method, Value::Object(params)).await;
    client.close().await;
    println!("{}", serde_json::to_string_pretty(&response?)?);
    Ok(())
}

#[cfg(test)]
mod model_tests {
    use super::*;

    fn profile(base_url: &str, api_key: Option<&str>) -> ProviderConfig {
        let mut value = json!({
            "service":"openai",
            "api":"chat_completions",
            "id":"example-model",
            "base_url":base_url,
            "api_version":"v1",
            "max_output_tokens":4096
        });
        if let Some(api_key) = api_key {
            value["api_key"] = json!(api_key);
        }
        serde_json::from_value(value).unwrap()
    }

    fn update(model: ProviderConfig, clear_api_key: bool) -> Put {
        let has_api_key = model.has_api_key();
        let mut model = serde_json::to_value(model).unwrap();
        if !has_api_key {
            model.as_object_mut().unwrap().remove("api_key");
        }
        Put {
            name: "local".into(),
            model,
            credential: None,
            clear_api_key,
        }
    }

    #[test]
    fn model_put_preserves_or_clears_a_saved_key_only_when_explicit() {
        let mut models = Models::default();
        models.models.insert(
            "local".into(),
            profile("https://api.example/v1", Some("saved-secret")),
        );

        put_model_profile(
            &mut models,
            update(profile("https://api.example/v1", None), false),
        )
        .unwrap();
        assert!(models.models["local"].has_api_key());
        let public = public_models(&models, None, None);
        assert_eq!(public["models"]["local"]["authentication"], "plain_file");
        assert!(!public.to_string().contains("saved-secret"));

        put_model_profile(
            &mut models,
            update(profile("https://api.example/v1", None), true),
        )
        .unwrap();
        assert!(!models.models["local"].has_api_key());
        assert_eq!(
            public_models(&models, None, None)["models"]["local"]["authentication"],
            "none"
        );
    }

    #[test]
    fn model_put_rejects_contradictory_clear_and_credential_destination_changes() {
        let mut models = Models::default();
        models.models.insert(
            "local".into(),
            profile("https://api.example/v1", Some("saved-secret")),
        );

        let error = put_model_profile(
            &mut models,
            update(
                profile("https://api.example/v1", Some("replacement-secret")),
                true,
            ),
        )
        .err()
        .expect("setting and clearing a key must be rejected");
        assert!(error.to_string().contains("set and clear"));
        assert_eq!(
            serde_json::to_value(&models.models["local"]).unwrap()["api_key"],
            "saved-secret"
        );

        let error = put_model_profile(
            &mut models,
            update(profile("https://other.example/v1", None), false),
        )
        .err()
        .expect("moving a saved key without a replacement must be rejected");
        assert!(error.to_string().contains("endpoint or adapter changed"));
        assert_eq!(
            serde_json::to_value(&models.models["local"]).unwrap()["api_key"],
            "saved-secret"
        );
    }

    #[test]
    fn clear_api_key_alias_is_compatible_but_rejects_conflicting_actions() {
        let previous = profile("https://api.example/v1", Some("saved-secret"));
        for action in [None, Some("remove")] {
            let mut params = json!({
                "name":"local",
                "model":previous.public_view(),
                "clear_api_key":true
            });
            params["model"]
                .as_object_mut()
                .unwrap()
                .remove("authentication");
            if let Some(action) = action {
                params["credential"] = json!(action);
            }
            let put: Put = parse(params).unwrap();
            assert!(!put.resolve_model(Some(&previous)).unwrap().has_api_key());
        }
        for action in ["keep", "replace"] {
            let mut put = update(profile("https://api.example/v1", None), true);
            put.credential = Some(parse(json!(action)).unwrap());
            assert!(put.resolve_model(Some(&previous)).is_err());
        }
    }

    #[test]
    fn model_put_clear_flag_defaults_to_false() {
        let put: Put = parse(json!({
            "name":"local",
            "model":{
                "service":"openai",
                "api":"chat_completions",
                "id":"example-model",
                "base_url":"https://api.example/v1",
                "api_version":"v1",
                "max_output_tokens":4096
            }
        }))
        .unwrap();
        assert!(!put.clear_api_key);
    }
}

#[cfg(all(test, feature = "plugins"))]
mod tests {
    use super::*;

    #[test]
    fn plugin_settings_error_preserves_typed_unknown_classification() {
        let source: Box<dyn Error + Send + Sync> = Box::new(cayw_runtime::RuntimeError::Plugin(
            cayw_plugins::PluginError::OutcomeUnknown("cleanup was not confirmed".into()),
        ));
        let error = PluginSettingsError::new(source);

        assert!(error_outcome_unknown(&error));
        assert_eq!(settings_error_code(&error), "plugin_outcome_unknown");
    }

    #[test]
    fn plugin_settings_error_preserves_untyped_unknown_classification() {
        let source: Box<dyn Error + Send + Sync> = Box::new(uncertain(std::io::Error::other(
            "management persistence was not confirmed",
        )));
        let error = PluginSettingsError::new(source);

        assert!(error_outcome_unknown(&error));
        assert_eq!(settings_error_code(&error), "settings_outcome_unknown");
    }
}

#[cfg(test)]
mod model_credential_tests {
    use super::*;

    fn model() -> Value {
        json!({
            "service":"openai", "api":"chat_completions", "id":"synthetic-model",
            "base_url":"https://fixture.invalid/v1", "api_version":"v1",
            "max_output_tokens":4096
        })
    }

    fn stored() -> ProviderConfig {
        let mut value = model();
        value["api_key"] = json!("SYNTHETIC_NOT_A_REAL_KEY");
        parse(value).unwrap()
    }

    fn put(model: Value, credential: Option<CredentialUpdate>) -> Put {
        Put {
            name: "daily".into(),
            model,
            credential,
            clear_api_key: false,
        }
    }

    #[test]
    fn unrelated_edit_preserves_key_for_explicit_and_legacy_callers() {
        for credential in [None, Some(CredentialUpdate::Keep)] {
            let mut changed = model();
            changed["max_output_tokens"] = json!(2048);
            changed["id"] = json!("synthetic-model-2");
            let resolved = put(changed, credential)
                .resolve_model(Some(&stored()))
                .unwrap();
            assert_eq!(
                serde_json::to_value(&resolved).unwrap()["api_key"],
                "SYNTHETIC_NOT_A_REAL_KEY"
            );
            assert_eq!(resolved.public_view()["max_output_tokens"], 2048);
            assert_eq!(resolved.public_view()["authentication"], "plain_file");
            assert!(resolved.public_view().get("api_key").is_none());
        }
    }

    #[test]
    fn endpoint_or_adapter_changes_never_reuse_stored_credentials() {
        for (field, value) in [
            ("base_url", "https://other.invalid/v1"),
            ("base_url", "http://fixture.invalid/v1"),
            ("base_url", "https://fixture.invalid/other"),
            ("base_url", "https://fixture.invalid:8443/v1"),
            ("service", "magpie"),
            ("api", "responses"),
            ("api_version", "v2"),
        ] {
            for credential in [None, Some(CredentialUpdate::Keep)] {
                let mut changed = model();
                changed[field] = json!(value);
                let result = put(changed, credential).resolve_model(Some(&stored()));
                let error = result.err().unwrap().to_string();
                assert!(error.contains("explicitly replace or remove"));
                assert!(!error.contains("SYNTHETIC_NOT_A_REAL_KEY"));
            }
        }
    }

    #[test]
    fn explicit_replacement_and_removal_allow_changed_endpoint() {
        for credential in [None, Some(CredentialUpdate::Replace)] {
            let mut changed = model();
            changed["base_url"] = json!("https://other.invalid/v1");
            changed["api_key"] = json!("SYNTHETIC_REPLACEMENT");
            let resolved = put(changed, credential)
                .resolve_model(Some(&stored()))
                .unwrap();
            assert_eq!(
                serde_json::to_value(resolved).unwrap()["api_key"],
                "SYNTHETIC_REPLACEMENT"
            );
        }
        for credential in [None, Some(CredentialUpdate::Remove)] {
            let mut changed = model();
            changed["base_url"] = json!("https://other.invalid/v1");
            changed["api_key"] = Value::Null;
            assert!(
                !put(changed, credential)
                    .resolve_model(Some(&stored()))
                    .unwrap()
                    .has_api_key()
            );
        }
        assert!(
            !put(model(), Some(CredentialUpdate::Remove))
                .resolve_model(Some(&stored()))
                .unwrap()
                .has_api_key()
        );
    }

    #[test]
    fn new_profiles_and_no_auth_edits_remain_supported() {
        for credential in [None, Some(CredentialUpdate::Remove)] {
            assert!(
                !put(model(), credential)
                    .resolve_model(None)
                    .unwrap()
                    .has_api_key()
            );
        }
        assert!(
            put(model(), Some(CredentialUpdate::Keep))
                .resolve_model(None)
                .is_err()
        );
        let unauthenticated: ProviderConfig = parse(model()).unwrap();
        let mut changed = model();
        changed["base_url"] = json!("https://other.invalid/v1");
        assert!(
            !put(changed, Some(CredentialUpdate::Keep))
                .resolve_model(Some(&unauthenticated))
                .unwrap()
                .has_api_key()
        );
        let mut authenticated = model();
        authenticated["api_key"] = json!("SYNTHETIC_NEW_KEY");
        for credential in [None, Some(CredentialUpdate::Replace)] {
            assert!(
                put(authenticated.clone(), credential)
                    .resolve_model(None)
                    .unwrap()
                    .has_api_key()
            );
        }
    }

    #[test]
    fn contradictory_and_invalid_credential_updates_fail_closed() {
        for credential in [CredentialUpdate::Keep, CredentialUpdate::Remove] {
            let mut value = model();
            value["api_key"] = json!("SYNTHETIC_NOT_A_REAL_KEY");
            assert!(
                put(value, Some(credential))
                    .resolve_model(Some(&stored()))
                    .is_err()
            );
        }
        for value in [
            Value::Null,
            json!(""),
            json!(" "),
            json!(7),
            json!("bad\nkey"),
        ] {
            let mut changed = model();
            changed["api_key"] = value;
            assert!(
                put(changed, Some(CredentialUpdate::Replace))
                    .resolve_model(Some(&stored()))
                    .is_err()
            );
        }
        assert!(
            put(model(), Some(CredentialUpdate::Replace))
                .resolve_model(None)
                .is_err()
        );
        let mut unknown_field = model();
        unknown_field["unknown"] = json!(true);
        assert!(put(unknown_field, None).resolve_model(None).is_err());
        assert!(
            parse::<Put>(json!({"name":"daily","model":model(),"credential":"invalid"})).is_err()
        );
        assert!(put(json!([]), None).resolve_model(None).is_err());
    }

    #[test]
    fn unrelated_edit_preserves_context_window_and_tokenizer_until_explicitly_changed() {
        let mut with_window = model();
        with_window["api_key"] = json!("SYNTHETIC_NOT_A_REAL_KEY");
        with_window["context_window"] = json!(128000);
        with_window["tokenizer"] = json!("cl100k_base");
        let existing: ProviderConfig = parse(with_window).unwrap();

        let preserved = put(model(), Some(CredentialUpdate::Keep))
            .resolve_model(Some(&existing))
            .unwrap();
        let view = preserved.public_view();
        assert_eq!(view["context_window"], json!(128000));
        assert_eq!(view["tokenizer"], json!("cl100k_base"));

        let mut cleared = model();
        cleared["context_window"] = Value::Null;
        cleared["tokenizer"] = Value::Null;
        let cleared = put(cleared, Some(CredentialUpdate::Keep))
            .resolve_model(Some(&existing))
            .unwrap();
        let view = cleared.public_view();
        assert!(view.get("context_window").is_none() || view["context_window"].is_null());
        assert!(view.get("tokenizer").is_none() || view["tokenizer"].is_null());
    }
}
