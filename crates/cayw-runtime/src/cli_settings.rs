//! Authenticated, host-only product settings. Never registered as model tools.
use cayw_ai::CancellationToken;
use cayw_ipc::{Client, RpcError};
use cayw_runtime::{
    HeadlessConfig, ProviderConfig, Runtime,
    local_cli::{LocalPaths, read_private, save_config},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, error::Error, path::PathBuf};

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

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Models {
    selected: Option<String>,
    models: BTreeMap<String, ProviderConfig>,
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

/// Move the removed local `config.json.model` field into this workspace's one
/// canonical model profile file. A previously selected profile wins; the old
/// field is still removed so later starts have only one source of truth.
pub fn migrate_legacy_model(paths: &LocalPaths, config_path: &std::path::Path) -> Result<bool> {
    let original = read_private(config_path, 1024 * 1024)?;
    let mut configuration: Value = serde_json::from_slice(&original)?;
    let object = configuration
        .as_object_mut()
        .ok_or("configuration must be a JSON object")?;
    let Some(raw_model) = object.get("model").cloned() else {
        return Ok(false);
    };
    if selected_model(paths)?.is_none() {
        let model: ProviderConfig = serde_json::from_value(raw_model)?;
        model.validate()?;
        save_configured_model(paths, model)?;
    }
    object.remove("model");
    let checked: HeadlessConfig = serde_json::from_value(configuration.clone())?;
    checked.into_runtime_config()?;
    let mut bytes = serde_json::to_vec_pretty(&configuration)?;
    bytes.push(b'\n');
    save_config(config_path, Some(&original), &bytes).map_err(uncertain)?;
    Ok(true)
}

fn public_models(models: &Models) -> Value {
    let profiles = models
        .models
        .iter()
        .map(|(name, model)| (name.clone(), model.public_view()))
        .collect::<serde_json::Map<_, _>>();
    json!({"selected":models.selected,"models":profiles})
}

/// `cayw conf` and live management share the same persisted selected profile.
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
    Ok(save_config(
        &paths.state_file("models.json"),
        original.as_deref(),
        &serde_json::to_vec_pretty(&models)?,
    )
    .map_err(uncertain)?)
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
    fn model_path(&self) -> PathBuf {
        self.paths.state_file("models.json")
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
                let selected_name = put.name.clone();
                let model = put_model_profile(&mut models, put)?;
                if models.selected.as_ref() == Some(&selected_name) {
                    if self.offline {
                        return Err("offline fixture cannot select a real model".into());
                    }
                    selection = Some(model.build()?);
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
        let mut response = public_models(&models);
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
        let public = public_models(&models);
        assert_eq!(public["models"]["local"]["authentication"], "plain_file");
        assert!(!public.to_string().contains("saved-secret"));

        put_model_profile(
            &mut models,
            update(profile("https://api.example/v1", None), true),
        )
        .unwrap();
        assert!(!models.models["local"].has_api_key());
        assert_eq!(
            public_models(&models)["models"]["local"]["authentication"],
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
}
