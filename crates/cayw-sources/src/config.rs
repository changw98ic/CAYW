//! Import host-owned MCP stdio definitions from common external config shapes.
//!
//! Supported input shapes (trusted host files only; never model-supplied):
//! - Claude Desktop / Cursor / VS Code / Codex style `{ "mcpServers": { ... } }`
//! - A bare `{ "<name>": { "command": "...", ... } }` object of servers
//!
//! Only local stdio entries with an absolute `command` path are accepted. Remote
//! URL/SSE/HTTP transports, package managers (`npx`, `uvx`, bare names) and
//! relative paths are reported as skipped. Lifecycle defaults to `lazy`.

use crate::lifecycle::{LifecycleMode, ServerSpec};
use crate::stdio::StdioConfig;
use cayw_codemode::CapabilityError;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};

/// One imported server definition after validation, ready for lifecycle prepare.
#[derive(Debug, Clone)]
pub struct ImportedServer {
    pub spec: ServerSpec,
    /// Absolute path of the config file that contributed this entry, when known.
    pub source_path: Option<PathBuf>,
}

/// Result of parsing an external MCP config document.
#[derive(Debug, Clone, Default)]
pub struct ImportReport {
    pub accepted: Vec<ImportedServer>,
    pub skipped: Vec<SkippedServer>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedServer {
    pub name: String,
    pub reason: String,
}

/// Parse a UTF-8 JSON document that uses a common `mcpServers` layout.
pub fn import_mcp_servers_json(
    text: &str,
    source_path: Option<&Path>,
    default_directory: &Path,
) -> Result<ImportReport, CapabilityError> {
    if text.len() > 1024 * 1024 {
        return Err(failure("MCP config exceeds 1 MiB bound"));
    }
    let value: Value =
        serde_json::from_str(text).map_err(|_| failure("MCP config is not valid JSON"))?;
    import_mcp_servers_value(&value, source_path, default_directory)
}

/// Parse an already-decoded JSON value.
pub fn import_mcp_servers_value(
    value: &Value,
    source_path: Option<&Path>,
    default_directory: &Path,
) -> Result<ImportReport, CapabilityError> {
    if !default_directory.is_absolute() || !default_directory.is_dir() {
        return Err(failure(
            "import default directory must be an absolute existing directory",
        ));
    }
    let servers = match value {
        Value::Object(map) if map.contains_key("mcpServers") => map
            .get("mcpServers")
            .and_then(Value::as_object)
            .ok_or_else(|| failure("mcpServers must be an object"))?,
        Value::Object(map) if map.contains_key("mcp-servers") => map
            .get("mcp-servers")
            .and_then(Value::as_object)
            .ok_or_else(|| failure("mcp-servers must be an object"))?,
        Value::Object(map) => map,
        _ => return Err(failure("MCP config root must be a JSON object")),
    };
    if servers.len() > 128 {
        return Err(failure("MCP config lists too many servers"));
    }
    let mut report = ImportReport::default();
    for (name, entry) in servers {
        match parse_entry(name, entry, source_path, default_directory) {
            Ok(imported) => report.accepted.push(imported),
            Err(reason) => report.skipped.push(SkippedServer {
                name: name.clone(),
                reason,
            }),
        }
    }
    Ok(report)
}

fn parse_entry(
    name: &str,
    entry: &Value,
    source_path: Option<&Path>,
    default_directory: &Path,
) -> Result<ImportedServer, String> {
    if !valid_name(name) {
        return Err("invalid server name".into());
    }
    let object = entry
        .as_object()
        .ok_or_else(|| "server entry must be an object".to_string())?;
    if object.contains_key("url")
        || object.contains_key("serverUrl")
        || object.get("type").and_then(Value::as_str) == Some("sse")
        || object.get("type").and_then(Value::as_str) == Some("http")
        || object.get("type").and_then(Value::as_str) == Some("streamable-http")
    {
        return Err("remote/URL MCP transports are outside this local stdio importer".into());
    }
    let command = object
        .get("command")
        .and_then(Value::as_str)
        .ok_or_else(|| "stdio entry requires command".to_string())?;
    if command.is_empty() || command.len() > 4096 || command.contains('\0') {
        return Err("invalid command".into());
    }
    let program = PathBuf::from(command);
    if !program.is_absolute() {
        return Err(
            "command must be an absolute executable path (npx/uvx/relative names are skipped)"
                .into(),
        );
    }
    if !program.is_file() {
        return Err("command path is not an existing regular file".into());
    }
    let arguments = match object.get("args") {
        None => Vec::new(),
        Some(Value::Array(items)) if items.len() <= 128 => items
            .iter()
            .map(|item| {
                item.as_str()
                    .filter(|text| text.len() <= 8192 && !text.contains('\0'))
                    .map(str::to_owned)
                    .ok_or_else(|| "invalid args entry".to_string())
            })
            .collect::<Result<Vec<_>, _>>()?,
        _ => return Err("args must be an array of short strings".into()),
    };
    let environment = match object.get("env") {
        None => BTreeMap::new(),
        Some(Value::Object(map)) if map.len() <= 128 => {
            let mut env = BTreeMap::new();
            for (key, value) in map {
                let value = value
                    .as_str()
                    .filter(|text| text.len() <= 8192 && !text.contains('\0'))
                    .ok_or_else(|| "env values must be short strings".to_string())?;
                if key.is_empty()
                    || key.len() > 256
                    || key.contains(['=', '\0'])
                    || env.insert(key.clone(), value.to_owned()).is_some()
                {
                    return Err("invalid env key".into());
                }
            }
            env
        }
        _ => return Err("env must be an object of short strings".into()),
    };
    let directory = match object.get("cwd").or_else(|| object.get("directory")) {
        None => default_directory.to_path_buf(),
        Some(Value::String(text)) => {
            let path = PathBuf::from(text);
            if !path.is_absolute() || !path.is_dir() {
                return Err("cwd/directory must be an absolute existing directory".into());
            }
            path
        }
        _ => return Err("cwd/directory must be a string path".into()),
    };
    let lifecycle = match object
        .get("lifecycle")
        .and_then(Value::as_str)
        .unwrap_or("lazy")
    {
        "lazy" => LifecycleMode::Lazy,
        "lazy-keep-alive" | "lazy_keep_alive" => LifecycleMode::LazyKeepAlive,
        "eager" | "keep-alive" | "keep_alive" => LifecycleMode::Eager,
        other => return Err(format!("unsupported lifecycle `{other}`")),
    };
    let idle_timeout = match object
        .get("idleTimeout")
        .or_else(|| object.get("idle_timeout"))
    {
        None => match lifecycle {
            LifecycleMode::Lazy => Duration::from_secs(10 * 60),
            LifecycleMode::LazyKeepAlive | LifecycleMode::Eager => Duration::ZERO,
        },
        Some(Value::Number(number)) => {
            let minutes = number
                .as_f64()
                .filter(|value| *value >= 0.0 && *value <= 24.0 * 60.0)
                .ok_or_else(|| "idleTimeout must be minutes in [0, 1440]".to_string())?;
            Duration::from_secs_f64(minutes * 60.0)
        }
        _ => return Err("idleTimeout must be a number of minutes".into()),
    };
    let version = object
        .get("version")
        .and_then(Value::as_str)
        .unwrap_or("imported")
        .to_owned();
    if version.is_empty() || version.len() > 256 || version.chars().any(char::is_control) {
        return Err("invalid version".into());
    }
    let mut config = StdioConfig::new(&program, &directory);
    config.arguments = arguments;
    config.environment = environment;
    Ok(ImportedServer {
        spec: ServerSpec {
            id: name.to_owned(),
            version,
            origin: source_path.map_or_else(
                || "host:imported-mcp".into(),
                |path| format!("host:imported-mcp:{}", path.display()),
            ),
            config,
            lifecycle,
            idle_timeout,
        },
        source_path: source_path.map(Path::to_path_buf),
    })
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
}

fn failure(message: &str) -> CapabilityError {
    CapabilityError::new(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn imports_claude_desktop_style_stdio_and_skips_remote() {
        let root = tempfile::tempdir().unwrap();
        let program = root.path().join("server");
        fs::write(&program, b"#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(&program).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&program, permissions).unwrap();
        }
        let json = format!(
            r#"{{
              "mcpServers": {{
                "local": {{
                  "command": "{}",
                  "args": ["--fixture"],
                  "env": {{"A":"1"}},
                  "lifecycle": "lazy",
                  "idleTimeout": 0.05
                }},
                "remote": {{"url": "https://example.invalid/mcp"}}
              }}
            }}"#,
            program.display()
        );
        let report = import_mcp_servers_json(
            &json,
            Some(root.path().join("cfg.json").as_path()),
            root.path(),
        )
        .unwrap();
        assert_eq!(report.accepted.len(), 1);
        assert_eq!(report.accepted[0].spec.id, "local");
        assert_eq!(report.accepted[0].spec.lifecycle, LifecycleMode::Lazy);
        assert_eq!(
            report.accepted[0].spec.idle_timeout,
            Duration::from_secs_f64(0.05 * 60.0)
        );
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].name, "remote");
    }

    #[test]
    fn skips_relative_and_package_manager_commands() {
        let root = tempfile::tempdir().unwrap();
        let report = import_mcp_servers_json(
            r#"{"mcpServers":{"npx":{"command":"npx","args":["-y","pkg"]}}}"#,
            None,
            root.path(),
        )
        .unwrap();
        assert!(report.accepted.is_empty());
        assert_eq!(report.skipped[0].name, "npx");
    }
}
