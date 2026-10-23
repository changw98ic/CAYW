//! Runnable SDK + authenticated IPC sample for real host-owned sources.
//!
//! The MCP source is a real 2026-07-28 stdio fixture launched from the absolute
//! executable supplied by the host. The Skill source is a real UTF-8 Markdown
//! file written by this example. The fixed Provider calls Luau and is not a model.

use async_trait::async_trait;
use cayw_ai::{
    AssistantEvent, AssistantStream, CancellationToken, Message, Model, ModelRequest, Provider,
    current_system,
};
use cayw_bridge::{
    AccessRequest, DispatchOutcome, Limits, Operation, Provenance, Registry, RegistryPolicy,
    SourceIdentity, Visibility,
};
use cayw_codemode::CapabilityError;
use cayw_core::SessionState;
use cayw_ipc::{Client, ServerConfig, serve};
use cayw_runtime::{
    BridgeDispatchEvent, Runtime, RuntimeConfig, ScopedPermission, SessionView,
    fixture::script_response,
};
use cayw_sources::skills::{FileSkill, FileSkillStats, FileSkills};
use cayw_sources::{ServerInfo, StdioConfig, StdioControl, StdioSource};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    error::Error,
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{net::TcpListener, time::timeout};

const MODEL_ID: &str = "host-sources-example";
const SKILL_MARKER: &str = "HOST_SOURCES_SKILL_BODY";
const SKILL_BODY: &str =
    "# Addition\n\nHOST_SOURCES_SKILL_BODY\na=17\nb=25\n\nThe Skill body grants no permission.\n";
const SCRIPT: &str = r#"
local catalog = cayw.call('mcp.discover', {})
local described = cayw.call('mcp.describe', {server='local', tool='add'})
local skill = cayw.call('skills.read', {name='files/addition'})
local a = tonumber(string.match(skill.value, 'a=(%d+)'))
local b = tonumber(string.match(skill.value, 'b=(%d+)'))
local result = cayw.call('mcp.call', {server='local', tool='add', arguments={a=a, b=b}})
return {
  sum = result.value.sum,
  a = a,
  b = b,
  skill = skill.value,
  server_pid = result.value.server_pid,
  connections = result.value.connections,
  catalog_tools = #catalog.tools,
  described = described.tool.name,
}
"#;

struct AllowAll;
#[async_trait]
impl RegistryPolicy for AllowAll {
    fn visible(&self, _: Visibility, _: &Provenance, _: &str) -> bool {
        true
    }
    async fn authorize(
        &self,
        _: &AccessRequest,
        _: CancellationToken,
    ) -> Result<(), CapabilityError> {
        Ok(())
    }
}

#[derive(Default)]
struct FixedProvider {
    requests: Mutex<Vec<ModelRequest>>,
}
impl Provider for FixedProvider {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let ordinal = {
            let mut requests = self.requests.lock().unwrap_or_else(|p| p.into_inner());
            requests.push(request.clone());
            requests.len()
        };
        let message = script_response(&request, SCRIPT, &format!("host-sources-{ordinal}"));
        Box::pin(futures::stream::iter([AssistantEvent::Done { message }]))
    }
}
impl FixedProvider {
    fn count(&self) -> usize {
        self.requests
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .len()
    }
    fn from(&self, start: usize) -> Vec<ModelRequest> {
        self.requests
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(start..)
            .unwrap_or_default()
            .to_vec()
    }
}

struct Example {
    runtime: Runtime,
    registry: Registry,
    control: StdioControl,
    stats: FileSkillStats,
    server_info: ServerInfo,
    pins: cayw_bridge::BindingSnapshot,
    provider: Arc<FixedProvider>,
    artifacts: PathBuf,
    pid: u32,
}

impl Example {
    async fn build(
        fixture: &Path,
        workspace: PathBuf,
        state: &Path,
        artifacts: PathBuf,
    ) -> Result<Self, Box<dyn Error>> {
        let skill_path = workspace.join("addition.md");
        std::fs::write(skill_path.as_path(), SKILL_BODY)?;
        let skills = FileSkills::load(
            vec![FileSkill {
                name: "addition".into(),
                description: "Host-owned Markdown addition Skill".into(),
                path: skill_path,
            }],
            &Limits::default(),
        )?;
        let skill_manifest = skills.manifest().clone();
        let stats = skills.stats();
        let skill_handle = skills.into_handle();

        let stdio = StdioSource::connect(
            StdioConfig::new(fixture, workspace.as_path()),
            CancellationToken::new(),
        )
        .await?;
        let server_info = stdio.server_info().clone();
        let control = stdio.control();
        let pid = control.snapshot().pid;
        let (mcp_manifest, mcp_handle) = stdio.into_parts();

        let registry = Registry::new(Limits::default(), Arc::new(AllowAll))?;
        registry.register(
            SourceIdentity {
                id: "local".into(),
                version: server_info.version.clone(),
                origin: "real local MCP 2026-07-28 stdio process".into(),
            },
            mcp_manifest,
            mcp_handle,
        )?;
        registry.register(
            SourceIdentity {
                id: "files".into(),
                version: "1".into(),
                origin: "explicit host Markdown file".into(),
            },
            skill_manifest,
            skill_handle,
        )?;

        let consumer = registry.consumer();
        let pins = consumer.binding_snapshot();
        let provider = Arc::new(FixedProvider::default());
        let permissions = vec![
            ScopedPermission::exact("mcp.discover", json!({})),
            ScopedPermission::exact("mcp.describe", json!({"server":"local","tool":"add"})),
            ScopedPermission::exact("skills.read", json!({"name":"files/addition"})),
            ScopedPermission::exact(
                "mcp.call",
                json!({"server":"local","tool":"add","arguments":{"a":17,"b":25}}),
            ),
        ];
        let config = RuntimeConfig {
            bridge: Some(consumer),
            permissions,
            max_run_time: Duration::from_secs(30),
            ..Default::default()
        };
        write_json(
            &artifacts,
            "config.json",
            &json!({
                "fixture": fixture, "workspace": workspace, "state": state,
                "skill_path": workspace.join("addition.md"),
                "skill_content_sha256": cayw_bridge::content_sha256(SKILL_BODY),
                "permissions": config.permissions, "bindings": pins,
                "runtime": {"max_run_time_ms": config.max_run_time.as_millis(),
                    "max_capability_calls": config.max_capability_calls,
                    "max_capability_bytes": config.max_capability_bytes,
                    "max_turns": config.max_turns, "max_total_tokens": config.max_total_tokens},
                "host": {"max_calls": config.host.max_calls, "max_argument_bytes": config.host.max_argument_bytes,
                    "max_result_bytes": config.host.max_result_bytes, "max_total_bytes": config.host.max_total_bytes,
                    "call_timeout_ms": config.host.call_timeout.as_millis()},
                "stdio": {"configuration": "StdioConfig::new defaults", "explicit_environment": {}},
                "registry": "Limits::default", "real_model": false
            }),
        )?;
        let runtime = Runtime::open(
            state.join("sessions.db"),
            workspace.as_path(),
            provider.clone(),
            Model::new("fixture", MODEL_ID),
            config,
        )?;
        Ok(Self {
            runtime,
            registry,
            control,
            stats,
            server_info,
            pins,
            provider,
            artifacts,
            pid,
        })
    }

    fn save_mode(&self, mode: &str, id: &str, start: usize) -> Result<PathBuf, Box<dyn Error>> {
        let dir = self.artifacts.join(mode);
        std::fs::create_dir(dir.as_path())?;
        let view = self.runtime.get(id)?;
        let events = self.runtime.events(id, 0, 1000)?;
        let tasks = self.runtime.tasks(id)?;
        let dispatches = dispatches(&self.runtime, id)?;
        let cells = view
            .messages
            .iter()
            .filter_map(|message| match message {
                Message::ToolResult(result) => result.structured_content.clone(),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_request_context(self.provider.as_ref(), start)?;
        assert_dispatches(&dispatches)?;
        write_json(&dir, "session.json", &view)?;
        write_json(&dir, "core-events.json", &events)?;
        write_json(&dir, "cells.json", &cells)?;
        write_json(&dir, "tasks.json", &tasks)?;
        write_json(&dir, "bridge-dispatches.json", &dispatches)?;
        write_json(
            &dir,
            "sources.json",
            &json!({
                "server_info": &self.server_info,
                "bindings": &self.pins,
                "local": self.registry.snapshot("local"),
                "files": self.registry.snapshot("files"),
            }),
        )?;
        write_json(&dir, "control.json", &self.control.snapshot())?;
        write_json(&dir, "skill-stats.json", &self.stats.snapshot())?;
        write_json(
            &dir,
            "provider.json",
            &provider_report(self.provider.as_ref(), start),
        )?;
        Ok(dir)
    }

    async fn finish(self) -> Result<Value, Box<dyn Error>> {
        self.control.close();
        timeout(Duration::from_secs(10), self.control.wait_closed()).await?;
        let stdio = self.control.snapshot();
        if !stdio.cleanup_complete || !stdio.reaped || stdio.failure.is_some() {
            return Err(format!("stdio cleanup is unconfirmed: {stdio:?}").into());
        }
        self.registry.disconnect("files")?;
        let skill = self.stats.snapshot();
        if !skill.closed {
            return Err("Skill source did not close".into());
        }
        let checks = json!({
            "stdio_spawned_once": stdio.spawned == 1,
            "stdio_cleanup_complete": stdio.cleanup_complete,
            "stdio_reaped": stdio.reaped,
            "stdio_failure_is_none": stdio.failure.is_none(),
            "skill_source_closed": skill.closed,
            "skill_load_scans": skill.load_scans,
            "skill_on_demand_reads": skill.on_demand_reads,
            "skill_rejections": skill.rejections,
        });
        Ok(json!({
            "stdio": &stdio,
            "skill_stats": &skill,
            "checks": checks,
        }))
    }
}

fn assert_answer(value: &Value, pid: u32) {
    assert_eq!(value["sum"], json!(42));
    assert_eq!(value["a"], json!(17));
    assert_eq!(value["b"], json!(25));
    assert_eq!(value["skill"], json!(SKILL_BODY));
    assert_eq!(value["catalog_tools"], json!(2));
    assert_eq!(value["described"], json!("add"));
    assert_eq!(value["server_pid"], json!(pid));
    assert_eq!(value["connections"], json!(1));
}
fn request_text(request: &ModelRequest) -> String {
    serde_json::to_string(&request.messages).expect("serialize model request")
}
fn assert_request_context(provider: &FixedProvider, start: usize) -> Result<(), Box<dyn Error>> {
    let requests = provider.from(start);
    if requests.len() < 2 {
        return Err("expected an execute turn and a tool-result turn".into());
    }
    let first_tools = current_system(&requests[0].messages)
        .tools
        .into_iter()
        .map(|tool| tool.name)
        .collect::<Vec<_>>();
    assert_eq!(first_tools, vec!["execute_code".to_owned()]);
    for request in &requests {
        let tools = current_system(&request.messages)
            .tools
            .into_iter()
            .map(|tool| tool.name)
            .collect::<Vec<_>>();
        assert!(
            tools.is_empty() || tools == vec!["execute_code".to_owned()],
            "unexpected model-visible tools: {tools:?}"
        );
    }
    assert!(!request_text(&requests[0]).contains(SKILL_MARKER));
    assert!(
        requests[1..]
            .iter()
            .any(|request| request_text(request).contains(SKILL_MARKER))
    );
    Ok(())
}
fn provider_report(provider: &FixedProvider, start: usize) -> Value {
    let requests = provider.from(start);
    let first_tools = requests
        .first()
        .map(|request| {
            current_system(&request.messages)
                .tools
                .into_iter()
                .map(|tool| tool.name)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let first_has_skill = requests
        .first()
        .is_some_and(|request| request_text(request).contains(SKILL_MARKER));
    let later_has_skill = requests
        .iter()
        .skip(1)
        .any(|request| request_text(request).contains(SKILL_MARKER));
    json!({
        "kind": "fixed scripted Provider using cayw_runtime::fixture::script_response",
        "real_model": false,
        "model": {"provider":"fixture","id":MODEL_ID},
        "request_count": requests.len(),
        "first_request_tools": first_tools,
        "first_request_has_skill_body": first_has_skill,
        "later_request_has_skill_body": later_has_skill,
        "requests": requests,
    })
}

fn cell_value(page: &Value) -> Option<Value> {
    page.get("outputs")?.as_array()?.iter().find_map(|entry| {
        let output = &entry["output"];
        (output["type"].as_str() == Some("json")).then(|| output["value"].clone())
    })
}
fn session_value(view: &SessionView) -> Value {
    view.messages
        .iter()
        .rev()
        .find_map(|message| match message {
            Message::ToolResult(result) => result.structured_content.as_ref().and_then(cell_value),
            _ => None,
        })
        .expect("no JSON Cell output in session")
}
fn ipc_value(messages: &[Value]) -> Value {
    messages
        .iter()
        .rev()
        .find_map(|message| message.get("structured_content").and_then(cell_value))
        .expect("no JSON Cell output in IPC response")
}
fn dispatches(rt: &Runtime, id: &str) -> Result<Vec<BridgeDispatchEvent>, Box<dyn Error>> {
    let (mut cursor, mut events) = (0, Vec::new());
    loop {
        let page = rt.bridge_dispatches(id, cursor, 1000)?;
        if page.next_cursor == cursor {
            break;
        }
        cursor = page.next_cursor;
        events.extend(page.events);
    }
    Ok(events)
}
fn assert_dispatches(entries: &[BridgeDispatchEvent]) -> Result<(), Box<dyn Error>> {
    let prepared = entries
        .iter()
        .filter(|entry| entry.outcome.is_none())
        .count();
    let settled = entries
        .iter()
        .filter(|entry| entry.outcome.is_some())
        .collect::<Vec<_>>();
    let len = entries.len();
    if prepared != 2 || settled.len() != 2 || len != 4 {
        return Err(format!(
            "expected two prepared and two settled source dispatches, got {len} entries"
        )
        .into());
    }
    let read = settled
        .iter()
        .find(|entry| entry.record.key.operation == Operation::ReadSkill)
        .copied()
        .ok_or("missing Skill read dispatch")?;
    assert_eq!(read.record.key.source.source.id, "files");
    assert_eq!(read.record.key.target, "addition");
    match &read.outcome {
        Some(DispatchOutcome::Succeeded { value }) => {
            assert_eq!(value["value"], json!(SKILL_BODY));
        }
        other => return Err(format!("Skill read did not succeed: {other:?}").into()),
    }
    let invoke = settled
        .iter()
        .find(|entry| entry.record.key.operation == Operation::Invoke)
        .copied()
        .ok_or("missing MCP invoke dispatch")?;
    assert_eq!(invoke.record.key.source.source.id, "local");
    assert_eq!(invoke.record.key.target, "add");
    assert_eq!(invoke.record.arguments, json!({"a":17,"b":25}));
    match &invoke.outcome {
        Some(DispatchOutcome::Succeeded { value }) => {
            assert_eq!(value["value"]["sum"], json!(42));
        }
        other => return Err(format!("MCP invoke did not succeed: {other:?}").into()),
    }
    Ok(())
}

fn write_json<T: Serialize + ?Sized>(
    dir: &Path,
    name: &str,
    value: &T,
) -> Result<(), Box<dyn Error>> {
    let path = dir.join(name);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path.as_path())?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

struct Options {
    fixture: String,
    workspace: String,
    state: String,
    artifacts: String,
}
impl Options {
    fn parse() -> Result<Self, Box<dyn Error>> {
        let (mut fixture, mut workspace, mut state, mut artifacts) = (None, None, None, None);
        let mut args = std::env::args().skip(1);
        while let Some(flag) = args.next() {
            let value = args
                .next()
                .ok_or_else(|| format!("{flag} requires a path"))?;
            let slot = match flag.as_str() {
                "--fixture" => &mut fixture,
                "--workspace" => &mut workspace,
                "--state" => &mut state,
                "--artifacts" => &mut artifacts,
                _ => return Err(format!("unknown argument: {flag}").into()),
            };
            if slot.replace(value).is_some() {
                return Err(format!("duplicate argument: {flag}").into());
            }
        }
        Ok(Self {
            fixture: fixture.ok_or("missing --fixture")?,
            workspace: workspace.ok_or("missing --workspace")?,
            state: state.ok_or("missing --state")?,
            artifacts: artifacts.ok_or("missing --artifacts")?,
        })
    }
}
fn absolute_file(value: &str, label: &str) -> Result<PathBuf, Box<dyn Error>> {
    let path = PathBuf::from(value);
    if !path.is_absolute() || !path.is_file() {
        return Err(format!("{label} must be an absolute existing file").into());
    }
    Ok(path)
}
fn fresh_dir(value: &str, label: &str) -> Result<PathBuf, Box<dyn Error>> {
    let path = PathBuf::from(value);
    if !path.is_absolute() || path.exists() {
        return Err(format!("{label} must be an absolute new directory").into());
    }
    std::fs::create_dir(path.as_path())?;
    Ok(path)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let options = Options::parse()?;
    let fixture = absolute_file(&options.fixture, "--fixture")?;
    let workspace = fresh_dir(&options.workspace, "--workspace")?;
    let state = fresh_dir(&options.state, "--state")?;
    let artifacts = fresh_dir(&options.artifacts, "--artifacts")?;
    let example = Example::build(&fixture, workspace, &state, artifacts).await?;

    let sdk_start = example.provider.count();
    let sdk_id = example
        .runtime
        .create(Some("SDK real host sources".into()))?
        .id;
    let sdk_view = example
        .runtime
        .prompt(
            &sdk_id,
            "discover, describe, read the Skill and add its inputs",
            CancellationToken::new(),
        )
        .await?;
    assert_eq!(sdk_view.state, SessionState::Idle);
    assert!(sdk_view.unresolved_capabilities.is_empty());
    let sdk_result = session_value(&sdk_view);
    assert_answer(&sdk_result, example.pid);
    let sdk_dir = example.save_mode("sdk", &sdk_id, sdk_start)?;

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let nonce = uuid::Uuid::new_v4();
    let token = format!("host-sources-{nonce}");
    let shutdown = CancellationToken::new();
    let server = tokio::spawn(serve(
        listener,
        Arc::new(example.runtime.clone()),
        ServerConfig::new(token.clone()),
        shutdown.clone(),
    ));
    if Client::connect(address, "not-the-issued-token")
        .await
        .is_ok()
    {
        return Err("loopback IPC accepted a wrong token".into());
    }
    let client = Client::connect(address, token.as_str()).await?;
    let ipc_start = example.provider.count();
    let created = client
        .request("session.create", json!({"label":"IPC real host sources"}))
        .await?;
    let ipc_id = created["id"]
        .as_str()
        .ok_or("IPC session.create returned no id")?
        .to_owned();
    let ipc_answer = client
        .request(
            "session.prompt",
            json!({"session_id":&ipc_id,"text":"authenticated IPC: discover, describe, read and add"}),
        )
        .await?;
    assert_eq!(ipc_answer["state"], "idle");
    let ipc_result = ipc_value(
        ipc_answer["messages"]
            .as_array()
            .ok_or("IPC response has no messages")?,
    );
    assert_answer(&ipc_result, example.pid);
    let ipc_dir = example.save_mode("ipc", &ipc_id, ipc_start)?;
    write_json(&ipc_dir, "ipc-response.json", &ipc_answer)?;

    client.close().await;
    shutdown.cancel();
    let stopped = timeout(Duration::from_secs(10), server).await??;
    stopped?;

    let artifacts_root = example.artifacts.clone();
    let cleanup = example.finish().await?;
    let cleanup_confirmed = cleanup["checks"]["stdio_cleanup_complete"] == json!(true)
        && cleanup["checks"]["stdio_reaped"] == json!(true)
        && cleanup["checks"]["stdio_spawned_once"] == json!(true)
        && cleanup["checks"]["stdio_failure_is_none"] == json!(true)
        && cleanup["stdio"]["tool_calls"] == json!(2)
        && cleanup["checks"]["skill_source_closed"] == json!(true)
        && cleanup["checks"]["skill_load_scans"] == json!(1)
        && cleanup["checks"]["skill_on_demand_reads"] == json!(2)
        && cleanup["checks"]["skill_rejections"] == json!(0);
    assert!(cleanup_confirmed, "{cleanup:?}");
    let summary = json!({
        "provider": {
            "kind": "fixed scripted Provider; not a real model",
            "real_model": false,
            "model": {"provider":"fixture","id":MODEL_ID},
        },
        "sdk": {"session_id": sdk_id, "result": sdk_result, "artifacts": sdk_dir},
        "ipc": {
            "session_id": ipc_id,
            "result": ipc_result,
            "artifacts": ipc_dir,
            "wrong_token_rejected": true,
        },
        "cleanup_confirmed": cleanup_confirmed,
        "processes": {
            "stdio": "reaped",
            "file_skill": "closed",
            "ipc_server": "stopped",
        },
        "cleanup": cleanup,
        "note": "Authenticated loopback IPC used the same real host sources. The token is not saved.",
    });
    write_json(&artifacts_root, "result.json", &summary)?;
    println!("{}", serde_json::to_string_pretty(&summary)?);
    Ok(())
}
