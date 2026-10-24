//! Issue 31 real-runtime demo.
//!
//! This example does **not** start the trace plugin. It starts one real
//! `cayw_runtime::Runtime` with a fixed offline `Provider`, the real Host and
//! CodeMode, and an authenticated loopback IPC server. A separate control
//! client (see `tests/demo_runtime.rs`) explicitly submits `task.enqueue` and
//! `task.resume`; the demo never runs a task on its own.
//!
//! The provider only chooses which Luau source the synthetic model asks to
//! run. The first code round performs one actual `fs.write` and one actual
//! `fs.read` through the Host. The second round builds a `GraphPlan` from the
//! real `fs.read` tool result (never from assistant text), renders the complete
//! graph program with `engine::script_for_plan`, and executes it in a second
//! bounded CodeMode cell of the same task. Every trace output is a normal
//! Cells output, so the real Runtime publishes the `trace_node` Host events.
//!
//! Usage: `demo --root NEW_ABS_DIR --listen 127.0.0.1:PORT --token-env ENV`.

use cayw_ai::{
    AssistantEvent, AssistantMessage, AssistantStream, CancellationToken, Content, Message, Model,
    ModelRequest, Provider, StopReason, ToolCall, Usage, UsageStatus,
};
use cayw_cells::{CellOutput, CellPage};
use cayw_ipc::{ServerConfig, serve};
use cayw_runtime::{Runtime, RuntimeConfig, ScopedPermission};
use cayw_trace_debug::engine::script_for_plan;
use cayw_trace_debug::{Edge, GraphPlan, Node};
use futures::stream;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    error::Error,
    fs::OpenOptions,
    io::Write,
    net::SocketAddr,
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken as StopToken;

type DemoResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

/// Exact bytes committed by the one real `fs.write`.
pub const SIDE_EFFECT_CONTENT: &str = "issue31 demo side effect written once by the real Host";
/// Pre-existing workspace input. It deliberately has no `amount` field.
pub const INPUT_FILE: &str = "{\"label\":\"issue31 demo input\",\"value\":7}\n";

const CAPTURE_SCHEMA: &str = "cayw.demo.capture.v1";
const TRACE_SCHEMA: &str = "cayw.trace.v1";
const EXEC_YIELD_MS: u64 = 30_000;
const EXEC_OUTPUT_TOKENS: usize = 128 * 1024;

/// Which condition source and threshold the fixed offline provider asks to run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Variant {
    /// Reads the absent `input.amount`; the actual Luau run must fail.
    InitialBug,
    /// Reads `input.value` with a low threshold, selecting the `high` edge.
    High,
    /// Reads `input.value` with a high threshold, selecting the `low` edge.
    Low,
    /// Waits for an explicit read approval inside the input node, then fails.
    Waiting,
}

impl Variant {
    /// The prompt text is the only variation channel in this offline fixture.
    /// It is not used as data: plan input always comes from the real tool result.
    pub fn from_prompt(prompt: &str) -> Self {
        let prompt = prompt.to_ascii_lowercase();
        if prompt.contains("wait") {
            Self::Waiting
        } else if prompt.contains("amount") || prompt.contains("initial") {
            Self::InitialBug
        } else if prompt.contains("low") {
            Self::Low
        } else {
            Self::High
        }
    }
}

#[derive(Clone, Debug)]
pub struct Options {
    pub root: PathBuf,
    pub listen: SocketAddr,
    pub token_env: String,
}

pub fn parse_options(args: impl IntoIterator<Item = String>) -> Result<Options, String> {
    let mut root = None;
    let mut listen = None;
    let mut token_env = None;
    let mut args = args.into_iter();
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--root" if root.is_none() => root = Some(args.next().ok_or("--root needs a path")?),
            "--listen" if listen.is_none() => {
                listen = Some(
                    args.next()
                        .ok_or("--listen needs IP:PORT")?
                        .parse::<SocketAddr>()
                        .map_err(|_| "--listen must be a numeric IP:PORT")?,
                )
            }
            "--token-env" if token_env.is_none() => {
                token_env = Some(args.next().ok_or("--token-env needs a name")?)
            }
            "--help" | "-h" => {
                return Err(
                    "usage: demo --root NEW_ABS_DIR --listen 127.0.0.1:PORT --token-env ENV".into(),
                );
            }
            _ => return Err("unknown or duplicate argument; use --help".into()),
        }
    }
    let root = PathBuf::from(root.ok_or("missing --root")?);
    let listen = listen.ok_or("missing --listen")?;
    if !listen.ip().is_loopback() {
        return Err("--listen must be a loopback address".into());
    }
    let token_env = token_env.ok_or("missing --token-env")?;
    if token_env.is_empty()
        || !token_env
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err("--token-env must be an environment variable name".into());
    }
    Ok(Options {
        root,
        listen,
        token_env,
    })
}

pub struct Demo {
    pub address: SocketAddr,
    pub session_id: String,
    pub workspace: PathBuf,
    pub runtime: Runtime,
    pub cancel: StopToken,
    server: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
}

impl Demo {
    pub async fn shutdown(mut self) {
        self.cancel.cancel();
        if let Some(server) = self.server.take() {
            let _ = server.await;
        }
    }
}

/// Read the token only from the named environment variable and start serving.
pub async fn start(options: &Options) -> DemoResult<Demo> {
    let token = std::env::var(&options.token_env)
        .map_err(|_| format!("environment variable {} is not set", options.token_env))?;
    start_with_token(options, token).await
}

/// Starts the demo with an explicitly supplied IPC token. Tests use this so
/// they never mutate process-wide environment variables.
pub async fn start_with_token(options: &Options, token: String) -> DemoResult<Demo> {
    if token.trim().is_empty() || token.chars().any(char::is_control) {
        return Err("IPC token must be nonempty and contain no control characters".into());
    }
    // The root must be new. This prevents a demo from reusing stale hosts,
    // sessions, or a previously written side-effect file.
    std::fs::create_dir(&options.root)?;
    let workspace = options.root.join("workspace");
    std::fs::create_dir(&workspace)?;
    let mut input = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(workspace.join("input.json"))?;
    input.write_all(INPUT_FILE.as_bytes())?;
    input.sync_all()?;
    let mut approval_input = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(workspace.join("approval-input.json"))?;
    approval_input.write_all(INPUT_FILE.as_bytes())?;
    approval_input.sync_all()?;

    let permissions = vec![
        ScopedPermission::exact("fs.read", json!({"path":"input.json"})),
        ScopedPermission::exact(
            "fs.write",
            json!({"path":"side-effect.txt","content":SIDE_EFFECT_CONTENT}),
        ),
    ];
    let runtime = Runtime::open(
        options.root.join("runtime.db"),
        &workspace,
        std::sync::Arc::new(DemoProvider {
            workspace: workspace.clone(),
            calls: AtomicUsize::new(0),
        }),
        Model::new("offline-demo", "issue31-fixed-script"),
        RuntimeConfig {
            permissions,
            codemode: cayw_codemode::Limits {
                host_wait: Duration::from_secs(180),
                ..Default::default()
            },
            ..Default::default()
        },
    )?;
    let session = runtime.create(Some("Issue31 real trace demo".into()))?;
    let listener = TcpListener::bind(options.listen).await?;
    let address = listener.local_addr()?;
    if !address.ip().is_loopback() {
        return Err("IPC listener is not loopback".into());
    }
    let cancel = StopToken::new();
    let server = tokio::spawn(serve(
        listener,
        std::sync::Arc::new(runtime.clone()),
        // Leave enough bounded time to inspect an approval wait in the browser.
        ServerConfig {
            request_timeout: Duration::from_secs(180),
            frame_timeout: Duration::from_secs(240),
            ..ServerConfig::new(token)
        },
        cancel.clone(),
    ));
    // Readiness contains no credential. The control client must authenticate
    // with the token that the operator put in the named environment variable.
    println!(
        "{}",
        serde_json::to_string(&json!({
            "schema":"cayw.issue31.demo.ready.v1",
            "address":address.to_string(),
            "session_id":session.id,
            "workspace":workspace,
            "plugin_started":false,
        }))?
    );
    std::io::stdout().flush()?;
    Ok(Demo {
        address,
        session_id: session.id,
        workspace,
        runtime,
        cancel,
        server: Some(server),
    })
}

/// Fixed offline provider modeled on `cayw_runtime::fixture::script_response`.
/// It reads every page and waits for VM completion, but it chooses the next
/// script from the *structured* real tool result rather than assistant text.
struct DemoProvider {
    workspace: PathBuf,
    calls: AtomicUsize,
}

impl Provider for DemoProvider {
    fn stream(&self, request: ModelRequest, cancel: CancellationToken) -> AssistantStream {
        if cancel.is_cancelled() {
            let message = AssistantMessage::new(&request.model)
                .failure(StopReason::Aborted, "offline demo provider cancelled");
            return Box::pin(stream::iter(vec![AssistantEvent::Error { message }]));
        }
        let message = self.respond(&request);
        Box::pin(stream::iter(vec![AssistantEvent::Done { message }]))
    }
}

impl DemoProvider {
    fn respond(&self, request: &ModelRequest) -> AssistantMessage {
        let prompt = request
            .messages
            .iter()
            .rev()
            .find(|message| matches!(message, Message::User { .. }))
            .map(Message::text)
            .unwrap_or_default();
        match request.messages.last() {
            Some(Message::ToolResult(result)) => {
                let page = result
                    .structured_content
                    .as_ref()
                    .and_then(|value| serde_json::from_value::<CellPage>(value.clone()).ok());
                let Some(page) = page else {
                    return stop_message(
                        request,
                        "Issue31 offline demo: tool result has no CellPage".into(),
                    );
                };
                if !result.outcome_unknown
                    && !matches!(
                        page.status,
                        cayw_cells::CellStatus::OutcomeUnknown | cayw_cells::CellStatus::Cancelled
                    )
                    && (!page.status.is_terminal() || page.has_more)
                {
                    return tool_message(
                        request,
                        self.next_id(request),
                        json!({
                            "action":"wait", "cell_id":page.cell_id,
                            "cursor":page.next_cursor,
                            "yield_time_ms":EXEC_YIELD_MS,
                            "max_output_tokens":EXEC_OUTPUT_TOKENS,
                        }),
                    );
                }
                let values: Vec<&Value> = page
                    .outputs
                    .iter()
                    .filter_map(|entry| match &entry.output {
                        CellOutput::Json { value } => Some(value),
                        _ => None,
                    })
                    .collect();
                if let Some(capture) = values
                    .iter()
                    .find(|value| value["schema"] == json!(CAPTURE_SCHEMA))
                {
                    return match self.plan_from_capture(capture, &prompt) {
                        Ok(plan) => match script_for_plan(&plan) {
                            Ok(script) => tool_message(
                                request,
                                self.next_id(request),
                                json!({
                                    "action":"exec", "script":script,
                                    "yield_time_ms":EXEC_YIELD_MS,
                                    "max_output_tokens":EXEC_OUTPUT_TOKENS,
                                }),
                            ),
                            Err(error) => stop_message(
                                request,
                                format!("Issue31 demo cannot render the plan: {error}"),
                            ),
                        },
                        Err(error) => stop_message(
                            request,
                            format!("Issue31 demo rejected the captured fs.read result: {error}"),
                        ),
                    };
                }
                let completed = values
                    .iter()
                    .filter(|value| value["schema"] == json!(TRACE_SCHEMA))
                    .count();
                let detail = page
                    .error
                    .as_deref()
                    .unwrap_or("the graph script ended without another code request");
                stop_message(
                    request,
                    format!(
                        "Issue31 offline demo finished with {completed} trace outputs; {detail}"
                    ),
                )
            }
            _ => tool_message(
                request,
                self.next_id(request),
                json!({
                    "action":"exec", "script":self.bootstrap_script(),
                    "yield_time_ms":EXEC_YIELD_MS,
                    "max_output_tokens":EXEC_OUTPUT_TOKENS,
                }),
            ),
        }
    }

    fn next_id(&self, request: &ModelRequest) -> String {
        format!(
            "issue31-{}-{}",
            self.calls.fetch_add(1, Ordering::AcqRel),
            request.messages.len()
        )
    }

    /// Exactly one write is attempted, and only while the side-effect file is
    /// absent. Every task still performs a real read of the pre-seeded input.
    fn bootstrap_script(&self) -> String {
        let mut script = String::new();
        if self.workspace.join("side-effect.txt").is_file() {
            script.push_str("local __written = cayw.null\n");
        } else {
            script.push_str(&format!(
                "local __written = cayw.call('fs.write', {{path = 'side-effect.txt', content = '{}'}})\n",
                SIDE_EFFECT_CONTENT
            ));
        }
        script.push_str(
            "local __read = cayw.call('fs.read', {path = 'input.json'})\n\
             cayw.emit({schema = 'cayw.demo.capture.v1', write = __written, read = __read})\n\
             return __read\n",
        );
        script
    }

    /// Build the plan only from the real Host result. The assistant never
    /// supplies input data, and the disk copy must still match that result.
    fn plan_from_capture(&self, capture: &Value, prompt: &str) -> Result<GraphPlan, String> {
        let read = capture
            .get("read")
            .ok_or_else(|| "capture has no read result".to_string())?;
        if read["path"] != json!("input.json") {
            return Err("captured read path is not input.json".into());
        }
        let content = read["content"]
            .as_str()
            .ok_or_else(|| "captured read has no UTF-8 content".to_string())?;
        let sha256 = read["sha256"]
            .as_str()
            .ok_or_else(|| "captured read has no sha256".to_string())?;
        let bytes = read["bytes"]
            .as_u64()
            .ok_or_else(|| "captured read has no byte count".to_string())?;
        if bytes != content.len() as u64 {
            return Err("captured read byte count disagrees with its content".into());
        }
        let digest = format!("{:x}", Sha256::digest(content.as_bytes()));
        if !digest.eq_ignore_ascii_case(sha256) {
            return Err("captured read sha256 does not match its content".into());
        }
        let on_disk = std::fs::read(self.workspace.join("input.json"))
            .map_err(|error| format!("cannot read the pre-seeded input: {error}"))?;
        if on_disk != content.as_bytes() {
            return Err("captured fs.read content differs from the pre-seeded input file".into());
        }
        let parsed: Value = serde_json::from_str(content)
            .map_err(|error| format!("captured input is not JSON: {error}"))?;
        let disk_parsed: Value = serde_json::from_slice(&on_disk)
            .map_err(|error| format!("pre-seeded input is not JSON: {error}"))?;
        if parsed != disk_parsed {
            return Err("captured input JSON differs from the pre-seeded file".into());
        }
        let variant = Variant::from_prompt(prompt);
        Ok(build_plan(variant, parsed, read.clone()))
    }
}

fn tool_message(request: &ModelRequest, call_id: String, arguments: Value) -> AssistantMessage {
    let mut message = AssistantMessage::new(&request.model);
    message.usage = Usage {
        input: 4,
        output: 4,
        total_tokens: 8,
        ..Default::default()
    };
    message.usage_status = UsageStatus::Reported;
    message.content.push(Content::ToolCall(ToolCall::new(
        call_id,
        "execute_code",
        arguments,
    )));
    message.stop_reason = StopReason::ToolUse;
    message
}

fn stop_message(request: &ModelRequest, text: String) -> AssistantMessage {
    let mut message = AssistantMessage::new(&request.model);
    message.usage = Usage {
        input: 4,
        output: 4,
        total_tokens: 8,
        ..Default::default()
    };
    message.usage_status = UsageStatus::Reported;
    message.content.push(Content::text(text));
    message.stop_reason = StopReason::Stop;
    message
}

/// Five nodes: input -> condition -> high | low -> result. The waiting variant
/// also reads a fixed file inside the input node through ordinary Host approval.
pub fn build_plan(variant: Variant, input: Value, captured_read: Value) -> GraphPlan {
    let threshold = match variant {
        Variant::Low => 20,
        Variant::InitialBug | Variant::High | Variant::Waiting => 5,
    };
    let (condition_source, id, label) = match variant {
        Variant::InitialBug => (
            CONDITION_INITIAL,
            "issue31-initial",
            "Issue31 initial condition reads a missing amount",
        ),
        Variant::High => (
            CONDITION_FIXED,
            "issue31-high",
            "Issue31 fixed condition with the high branch",
        ),
        Variant::Low => (
            CONDITION_FIXED,
            "issue31-low",
            "Issue31 fixed condition with the low branch",
        ),
        Variant::Waiting => (
            CONDITION_INITIAL,
            "issue31-waiting",
            "Issue31 input waits for a real read approval",
        ),
    };
    let expected_content = captured_read["content"].as_str().unwrap_or_default();
    let expected_sha256 = captured_read["sha256"].as_str().unwrap_or_default();
    let expected_bytes = captured_read["bytes"].as_u64().unwrap_or_default();
    let input_source = if variant == Variant::Waiting {
        format!(
            "local approved = cayw.call('fs.read', {{path='approval-input.json'}})\nif approved.content ~= context.captured_read.content then error('approved read mismatch') end\n{INPUT_NODE}"
        )
    } else {
        INPUT_NODE.to_owned()
    };
    GraphPlan {
        id: id.into(),
        label: label.into(),
        entry: "input".into(),
        nodes: vec![
            node(
                "input",
                "Real input",
                "captured",
                &input_source,
                json!({
                    "expected_content":expected_content,
                    "expected_sha256":expected_sha256,
                    "expected_bytes":expected_bytes,
                }),
            ),
            node(
                "condition",
                "Threshold condition",
                "branch",
                condition_source,
                json!({"threshold":threshold}),
            ),
            node("high", "High branch", "branch", HIGH_NODE, json!({})),
            node("low", "Low branch", "branch", LOW_NODE, json!({})),
            node("result", "Result", "output", RESULT_NODE, json!({})),
        ],
        edges: vec![
            edge("input", "condition", None),
            edge("condition", "high", Some("high")),
            edge("condition", "low", Some("low")),
            edge("high", "result", None),
            edge("low", "result", None),
        ],
        input,
        initial_state: json!({"visited":0}),
        context: json!({
            "captured_read":captured_read,
            "origin":"actual Host fs.read tool result, injected by the offline demo provider",
        }),
    }
}

fn node(id: &str, label: &str, module: &str, source: &str, params: Value) -> Node {
    Node {
        id: id.into(),
        label: label.into(),
        module: module.into(),
        source: source.into(),
        params,
        input_pointer: String::new(),
        implementation: format!("examples/demo.rs::{id}"),
    }
}

fn edge(from: &str, to: &str, branch: Option<&str>) -> Edge {
    Edge {
        from: from.into(),
        to: to.into(),
        branch: branch.map(Into::into),
    }
}

/// The first node proves that the statically injected plan input and context
/// are the same real `fs.read` result, without any Host call or model claim.
const INPUT_NODE: &str = r#"
local captured = context.captured_read
if type(captured) ~= "table" then error("captured fs.read result is absent") end
if captured.path ~= "input.json" then error("captured fs.read path mismatch") end
if captured.content ~= params.expected_content then error("captured fs.read content mismatch") end
if captured.sha256 ~= params.expected_sha256 then error("captured fs.read sha256 mismatch") end
if captured.bytes ~= params.expected_bytes then error("captured fs.read byte count mismatch") end
local value = input.value
if type(value) ~= "number" then error("plan input.value must be a number") end
local visited = (state.visited or 0) + 1
return {output = {value = value, source_sha256 = captured.sha256}, state = {visited = visited}}
"#;

/// Initial condition: `input.amount` does not exist, so the comparison with a
/// number raises a real Luau error inside the managed CodeMode cell.
const CONDITION_INITIAL: &str = r#"
local visited = (state.visited or 0) + 1
local threshold = params.threshold
local amount = input.amount
local level = "low"
if amount > threshold then level = "high" end
return {output = {value = amount, level = level, threshold = threshold}, state = {visited = visited}, branch = level}
"#;

/// User-edited condition: reads the real `input.value` instead.
pub const CONDITION_FIXED: &str = r#"
local visited = (state.visited or 0) + 1
local threshold = params.threshold
local value = input.value
if type(value) ~= "number" then error("condition requires numeric input.value") end
local level = "low"
if value > threshold then level = "high" end
return {output = {value = value, level = level, threshold = threshold}, state = {visited = visited}, branch = level}
"#;

const HIGH_NODE: &str = r#"
local visited = (state.visited or 0) + 1
return {output = {value = input.value, level = "high", threshold = input.threshold, rule = "value > threshold"}, state = {visited = visited}}
"#;

const LOW_NODE: &str = r#"
local visited = (state.visited or 0) + 1
return {output = {value = input.value, level = "low", threshold = input.threshold, rule = "value <= threshold"}, state = {visited = visited}}
"#;

const RESULT_NODE: &str = r#"
local visited = (state.visited or 0) + 1
return {output = {value = input.value, classification = input.level, threshold = input.threshold, rule = input.rule, visited_nodes = visited}, state = {visited = visited}}
"#;

#[tokio::main]
async fn main() -> DemoResult<()> {
    let options = parse_options(std::env::args().skip(1))?;
    let demo = start(&options).await?;
    println!(
        "{}",
        serde_json::to_string(&json!({
            "schema":"cayw.issue31.demo.waiting.v1",
            "message":"waiting for an authenticated control client to task.enqueue and task.resume",
        }))?
    );
    std::io::stdout().flush()?;
    tokio::signal::ctrl_c().await?;
    demo.shutdown().await;
    Ok(())
}
