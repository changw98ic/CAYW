//! F04 symbol/version bridge tools: refresh from workspace files, query definitions.
use crate::{Host, failure, filesystem};
use cayw_ai::CancellationToken;
use cayw_codeintel::{DefinitionQuery, SymbolVersionBridge, publish_bytes, publish_outline_json};
use cayw_codemode::CapabilityError;
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Instant;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RefreshArgs {
    path: String,
    #[serde(default = "rust_lang")]
    language: String,
}

fn rust_lang() -> String {
    "rust".into()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DefinitionsArgs {
    name: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    file_version: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PathArgs {
    path: String,
}

pub(crate) async fn execute(
    host: &Host,
    name: &str,
    args: &Value,
    cancel: &CancellationToken,
    deadline: Instant,
) -> Result<Value, CapabilityError> {
    let bridge = host
        .codeintel_bridge()
        .ok_or_else(|| failure("codeintel bridge is disabled"))?;
    match name {
        "codeintel.refresh" => refresh(host, &bridge, args, cancel, deadline).await,
        "codeintel.definitions" => definitions(&bridge, args),
        "codeintel.file_version" => file_version(&bridge, args),
        "codeintel.symbols" => symbols(&bridge, args),
        _ => Err(failure("unknown codeintel capability")),
    }
}

async fn refresh(
    host: &Host,
    bridge: &SymbolVersionBridge,
    args: &Value,
    cancel: &CancellationToken,
    deadline: Instant,
) -> Result<Value, CapabilityError> {
    let args: RefreshArgs = serde_json::from_value(args.clone())
        .map_err(|error| failure(format!("invalid codeintel.refresh arguments: {error}")))?;

    // Prefer Workspace `code.outline` when enabled (authoritative Rust outline).
    if host.config.workspace.is_some() && args.language == "rust" {
        let (workspace, workspace_deadline) = host.open_workspace(cancel, deadline).await?;
        match workspace
            .execute_until(
                "code.outline",
                &json!({"path": &args.path, "language": "rust"}),
                cancel.clone(),
                workspace_deadline,
            )
            .await
        {
            Ok(outline) => {
                let file = publish_outline_json(bridge, &outline).map_err(map_error)?;
                return file_result("code.outline", &file, bridge);
            }
            Err(error) if error.outcome_unknown => return Err(error),
            Err(_) => {
                // Fall through to lightweight producer.
            }
        }
    }

    // Closed loop without outline: read workspace file bytes, lightweight extract.
    let read = filesystem::execute(
        host,
        "fs.read",
        &json!({"path": &args.path}),
        cancel,
        deadline,
    )
    .await?;
    let content = read
        .get("content")
        .and_then(Value::as_str)
        .ok_or_else(|| failure("fs.read missing content"))?;
    let file =
        publish_bytes(bridge, &args.path, content.as_bytes(), &args.language).map_err(map_error)?;
    file_result("lightweight", &file, bridge)
}

fn file_result(
    source: &str,
    file: &cayw_codeintel::FileVersion,
    bridge: &SymbolVersionBridge,
) -> Result<Value, CapabilityError> {
    Ok(json!({
        "protocol_version": cayw_codeintel::CODEINTEL_PROTOCOL_VERSION,
        "source": source,
        "file": file,
        "symbol_count": bridge.symbols_for(&file.path).map_err(map_error)?.len(),
    }))
}

fn definitions(bridge: &SymbolVersionBridge, args: &Value) -> Result<Value, CapabilityError> {
    let args: DefinitionsArgs = serde_json::from_value(args.clone())
        .map_err(|error| failure(format!("invalid codeintel.definitions arguments: {error}")))?;
    let hits = bridge
        .definitions(DefinitionQuery {
            name: args.name,
            path: args.path,
            kind: args.kind,
            file_version: args.file_version,
        })
        .map_err(map_error)?;
    Ok(json!({
        "protocol_version": cayw_codeintel::CODEINTEL_PROTOCOL_VERSION,
        "definitions": hits,
    }))
}

fn file_version(bridge: &SymbolVersionBridge, args: &Value) -> Result<Value, CapabilityError> {
    let args: PathArgs = serde_json::from_value(args.clone())
        .map_err(|error| failure(format!("invalid codeintel.file_version arguments: {error}")))?;
    let version = bridge.file_version(&args.path).map_err(map_error)?;
    Ok(json!({
        "protocol_version": cayw_codeintel::CODEINTEL_PROTOCOL_VERSION,
        "file": version,
    }))
}

fn symbols(bridge: &SymbolVersionBridge, args: &Value) -> Result<Value, CapabilityError> {
    let args: PathArgs = serde_json::from_value(args.clone())
        .map_err(|error| failure(format!("invalid codeintel.symbols arguments: {error}")))?;
    let symbols = bridge.symbols_for(&args.path).map_err(map_error)?;
    Ok(json!({
        "protocol_version": cayw_codeintel::CODEINTEL_PROTOCOL_VERSION,
        "symbols": symbols,
    }))
}

fn map_error(error: cayw_codeintel::CodeIntelError) -> CapabilityError {
    failure(error.to_string())
}
