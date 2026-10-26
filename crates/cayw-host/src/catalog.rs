use crate::{Executor, HostConfig, ProfileKind, failure, filesystem, git, process, terminal};
use cayw_ai::ToolDefinition;
use cayw_codemode::CapabilityError;
use serde_json::{Value, json};

fn definition(
    name: &str,
    description: &str,
    properties: Value,
    required: &[&str],
) -> ToolDefinition {
    ToolDefinition {
        name: name.into(),
        description: description.into(),
        parameters: json!({
            "type":"object", "properties":properties, "required":required, "additionalProperties":false
        }),
    }
}
fn string(max: usize) -> Value {
    json!({"type":"string", "minLength":1, "maxLength":max})
}
fn path() -> Value {
    string(4096)
}
fn hash() -> Value {
    json!({"type":"string", "pattern":"^[0-9a-fA-F]{64}$"})
}

pub(crate) fn definitions(config: &HostConfig) -> Vec<(ToolDefinition, Executor)> {
    let mut definitions = vec![
        definition(
            "fs.read",
            "Read one bounded UTF-8 regular file and its SHA-256.",
            json!({"path":path()}),
            &["path"],
        ),
        definition(
            "fs.list",
            "List bounded direct directory entries without following symbolic links.",
            json!({"path":path()}),
            &["path"],
        ),
        definition(
            "fs.stat",
            "Inspect an entry without following symbolic links.",
            json!({"path":path()}),
            &["path"],
        ),
        definition(
            "fs.write",
            "Atomically create a UTF-8 file. Replacing an existing file requires overwrite=true and expected_sha256.",
            json!({
                "path":path(), "content":{"type":"string"}, "overwrite":{"type":"boolean"}, "expected_sha256":hash()
            }),
            &["path", "content"],
        ),
        definition(
            "fs.patch",
            "Atomically replace one unique exact text match after checking expected_sha256.",
            json!({
                "path":path(), "old":string(config.max_file_bytes), "new":{"type":"string"}, "expected_sha256":hash()
            }),
            &["path", "old", "new", "expected_sha256"],
        ),
    ].into_iter().map(|definition| (definition, Executor::Filesystem)).collect::<Vec<_>>();
    if config.data.is_some() {
        definitions.extend(
            cayw_data::definitions()
                .into_iter()
                .map(|definition| (definition, Executor::Data)),
        );
    }
    if config.workspace.is_some() {
        definitions.extend(
            cayw_workspace::definitions()
                .into_iter()
                .map(|definition| (definition, Executor::Workspace)),
        );
    }
    definitions.extend(
        git::definitions(config)
            .into_iter()
            .map(|definition| (definition, Executor::Git)),
    );
    for (kind, name) in [
        (ProfileKind::Build, "build.run"),
        (ProfileKind::Test, "test.run"),
        (ProfileKind::Check, "check.run"),
        (ProfileKind::Lint, "lint.run"),
        (ProfileKind::Format, "format.run"),
    ] {
        let profiles: Vec<_> = config
            .profiles
            .iter()
            .filter(|(_, profile)| profile.kind == kind)
            .map(|(name, _)| name)
            .collect();
        if profiles.is_empty() {
            continue;
        }
        let mut properties = json!({"profile":{"type":"string", "enum":profiles}});
        if kind == ProfileKind::Test {
            properties["filter"] = string(256);
        }
        definitions.push((definition(name, "Run an application-selected process profile with bounded time and output. No agent argv or shell.", properties, &["profile"]), Executor::Profile));
    }
    if let Some(config) = &config.terminal_handoffs {
        definitions.extend(
            terminal::definitions(config)
                .into_iter()
                .map(|definition| (definition, Executor::Terminal)),
        );
    }
    if config.web_access.is_some() {
        definitions.extend(
            cayw_web_access::definitions()
                .into_iter()
                .map(|definition| (definition, Executor::WebAccess)),
        );
    }
    if config.external.is_some() {
        definitions.extend([
            definition("search.query", "Query the application's existing bounded search service.", json!({
                "query":string(8192), "path":path(), "limit":{"type":"integer", "minimum":1, "maximum":1000}
            }), &["query"]),
            definition("mcp.discover", "Discover tools from the application's existing MCP connections.", json!({"server":string(256)}), &[]),
            definition("mcp.describe", "Describe one tool from an existing MCP connection.", json!({"server":string(256),"tool":string(256)}), &["server","tool"]),
            definition("mcp.call", "Call one tool through the application's existing MCP connection and permissions.", json!({
                "server":string(256),"tool":string(256),"arguments":{"type":"object"}
            }), &["server","tool","arguments"]),
            definition("mcp.resources.list", "List resource metadata pinned by the host's existing MCP sources.", json!({"server":string(256)}), &[]),
            definition("mcp.resources.read", "Read one manifest-pinned resource through the host's existing MCP source.", json!({"server":string(256),"uri":string(2048)}), &["server","uri"]),
            definition("skills.list", "List skills from the application's existing skill source.", json!({}), &[]),
            definition("skills.read", "Read one named skill from the application's existing skill source.", json!({"name":string(256)}), &["name"]),
        ].into_iter().map(|definition| (definition, Executor::External)));
    }
    definitions
}

pub(crate) fn validate_arguments(
    name: &str,
    args: &Value,
    config: &HostConfig,
) -> Result<(), CapabilityError> {
    git::validate_arguments(name, args, config)?;
    if let Some(config) = &config.terminal_handoffs {
        terminal::validate_arguments(name, args, config)?;
    }
    if let Some(path) = args.get("path").and_then(Value::as_str) {
        filesystem::components(
            path,
            matches!(
                name,
                "fs.list" | "fs.stat" | "fs.tree" | "fs.grep" | "fs.glob" | "search.query"
            ),
        )?;
    }
    if name == "fs.write" {
        if args["content"].as_str().expect("schema validated").len() > config.max_file_bytes {
            return Err(failure("file content exceeds byte limit"));
        }
        if args.get("expected_sha256").is_some()
            && args.get("overwrite").and_then(Value::as_bool) != Some(true)
        {
            return Err(failure("expected_sha256 requires overwrite=true"));
        }
        if args.get("overwrite").and_then(Value::as_bool) == Some(true)
            && args.get("expected_sha256").is_none()
        {
            return Err(failure("overwrite=true requires expected_sha256"));
        }
    }
    if name == "fs.patch"
        && (args["old"].as_str().expect("schema validated").len() > config.max_file_bytes
            || args["new"].as_str().expect("schema validated").len() > config.max_file_bytes)
    {
        return Err(failure("patch text exceeds file byte limit"));
    }
    if name == "test.run"
        && let Some(filter) = args.get("filter").and_then(Value::as_str)
    {
        let profile = config
            .profiles
            .get(args["profile"].as_str().expect("schema validated"))
            .ok_or_else(|| failure("profile is not configured"))?;
        if !profile.allow_filter || !process::valid_filter(filter) {
            return Err(failure("test profile does not allow this filter"));
        }
    }
    Ok(())
}

pub(crate) fn may_mutate(name: &str) -> bool {
    matches!(
        name,
        "fs.write"
            | "fs.patch"
            | "fs.mkdir"
            | "fs.move"
            | "fs.remove"
            | "fs.prepare_change_batch"
            | "fs.submit_change_batch"
            | "fs.recover_change_batch"
            | "build.run"
            | "test.run"
            | "check.run"
            | "lint.run"
            | "format.run"
            | "mcp.call"
    ) || name == "search.query" // External services may perform remote accounting/work.
        || git::may_mutate(name)
}
