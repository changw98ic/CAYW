//! Optional External-process Fusion plugin (plugin protocol v1).
//!
//! The process only adapts the wire protocol and provider configuration. Fusion
//! semantics (blind identical input, schema validation, digest, disagreements
//! without a vote) come unchanged from `cayw_plugins::fusion::collect_fusion`.
//! A child process is not a security sandbox.

use async_trait::async_trait;
use cayw_ai::{CancellationToken, ToolDefinition, Usage};
use cayw_plugins::fusion::{CandidateResponse, FusionCandidateSource, FusionInput, collect_fusion};
use cayw_plugins::{
    ExternalMessage, ExternalResult, HostMessage, PLUGIN_PROTOCOL_VERSION, PluginKind,
    PluginManifest,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fmt;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

/// Plugin name declared in the handshake manifest.
pub const PLUGIN_NAME: &str = "fusion";
/// The only capability exposed by this plugin.
pub const COLLECT_METHOD: &str = "fusion.collect";
/// Upper bound for `per_provider_timeout_ms`. The host's call timeout must be
/// larger than the timeout chosen for a call, or the host kills the process.
pub const MAX_PER_PROVIDER_TIMEOUT_MS: u64 = 600_000;
/// Matches `ExternalConfig::new`'s default `max_line_bytes`.
pub const DEFAULT_MAX_LINE_BYTES: usize = 64 * 1024;
/// Memory bound for one request line. The host enforces its own, usually
/// smaller, `max_line_bytes` on what it sends.
pub const MAX_REQUEST_LINE_BYTES: usize = 16 * 1024 * 1024;

/// The exact manifest this binary sends during the handshake. Hosts declare
/// it as `ExternalConfig::expected_manifest`; the binary prints it with
/// `--print-manifest`.
pub fn manifest() -> PluginManifest {
    PluginManifest {
        name: PLUGIN_NAME.into(),
        version: env!("CARGO_PKG_VERSION").into(),
        protocol_version: PLUGIN_PROTOCOL_VERSION,
        kind: PluginKind::External,
        capabilities: vec![ToolDefinition {
            name: COLLECT_METHOD.into(),
            description: "Collect blind, independent answers to one identical Fusion input from 3-16 distinct providers. Distinct valid answers are reported as disagreements, never decided by vote.".into(),
            parameters: collect_schema(),
        }],
        permissions: Default::default(),
    }
}

fn collect_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["input", "providers", "per_provider_timeout_ms"],
        "properties": {
            "input": {
                "type": "object",
                "additionalProperties": false,
                "required": ["question", "canonical_context", "constraints", "answer_schema"],
                "properties": {
                    "question": {"type": "string", "minLength": 1},
                    "canonical_context": {},
                    "constraints": {"type": "array", "items": {"type": "string"}},
                    "answer_schema": {"type": "object"}
                }
            },
            "providers": {
                "type": "array",
                "minItems": 3,
                "maxItems": 16,
                "items": {
                    "type": "object",
                    "required": ["provider_id", "kind"],
                    "properties": {
                        "provider_id": {"type": "string", "minLength": 1, "maxLength": 128},
                        "kind": {"type": "string"}
                    }
                }
            },
            "per_provider_timeout_ms": {
                "type": "integer",
                "minimum": 1,
                "maximum": MAX_PER_PROVIDER_TIMEOUT_MS
            }
        }
    })
}

/// Arguments of `fusion.collect`. The result is a `cayw_plugins::fusion::FusionReport`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CollectArguments {
    pub input: FusionInput,
    /// Host registration order is kept in the report.
    pub providers: Vec<ProviderSpec>,
    pub per_provider_timeout_ms: u64,
}

/// Provider configuration supplied per call. This is the extension point for
/// real model providers: add a variant and build a `FusionCandidateSource`
/// for it in [`ProviderSpec::build`]. Only offline providers ship today, so
/// this process needs no credential, network access or inherited environment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProviderSpec {
    /// Deterministic local provider for fixtures and tests. No network.
    Offline {
        provider_id: String,
        behavior: OfflineBehavior,
    },
}

/// What an offline provider does with the identical input it receives.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum OfflineBehavior {
    /// Return this value; it is still validated against `answer_schema`.
    Answer {
        value: Value,
        #[serde(default)]
        usage: Option<Usage>,
    },
    /// Report a known provider failure.
    Fail { message: String },
    /// Never answer; the per-provider timeout or cancellation ends it.
    Hang,
    /// Fault injection: panic inside the provider task. Requires
    /// `--allow-fault-injection`.
    Panic,
    /// Fault injection: terminate the whole plugin process mid-call. Requires
    /// `--allow-fault-injection`.
    Crash,
}

impl ProviderSpec {
    pub fn provider_id(&self) -> &str {
        match self {
            Self::Offline { provider_id, .. } => provider_id,
        }
    }

    /// Build a candidate source. Every source runs in its own task, so a
    /// panicking provider becomes a failed candidate instead of a crash.
    pub fn build(self, options: &ServeOptions) -> Result<Arc<dyn FusionCandidateSource>, String> {
        let source: Arc<dyn FusionCandidateSource> = match self {
            Self::Offline {
                provider_id,
                behavior,
            } => {
                if matches!(behavior, OfflineBehavior::Panic | OfflineBehavior::Crash)
                    && !options.allow_fault_injection
                {
                    return Err(format!(
                        "provider {provider_id}: fault injection is disabled for this process"
                    ));
                }
                Arc::new(OfflineSource {
                    provider_id,
                    behavior,
                })
            }
        };
        Ok(Arc::new(Isolated(source)))
    }
}

struct OfflineSource {
    provider_id: String,
    behavior: OfflineBehavior,
}

#[async_trait]
impl FusionCandidateSource for OfflineSource {
    fn provider_id(&self) -> &str {
        &self.provider_id
    }

    async fn answer(
        &self,
        _input: Arc<FusionInput>,
        cancel: CancellationToken,
    ) -> CandidateResponse {
        match &self.behavior {
            OfflineBehavior::Answer { value, usage } => CandidateResponse::Answer {
                value: value.clone(),
                usage: usage.clone(),
            },
            OfflineBehavior::Fail { message } => CandidateResponse::Failed {
                message: message.clone(),
                usage: None,
            },
            OfflineBehavior::Hang => {
                cancel.cancelled().await;
                CandidateResponse::Failed {
                    message: "cancelled".into(),
                    usage: None,
                }
            }
            OfflineBehavior::Panic => panic!("offline fault injection: provider panicked"),
            OfflineBehavior::Crash => std::process::exit(70),
        }
    }
}

/// Runs a source in its own task. Dropping the answer future aborts the task.
struct Isolated(Arc<dyn FusionCandidateSource>);

#[async_trait]
impl FusionCandidateSource for Isolated {
    fn provider_id(&self) -> &str {
        self.0.provider_id()
    }

    async fn answer(
        &self,
        input: Arc<FusionInput>,
        cancel: CancellationToken,
    ) -> CandidateResponse {
        let source = self.0.clone();
        let mut task = AbortOnDrop(tokio::spawn(
            async move { source.answer(input, cancel).await },
        ));
        match (&mut task.0).await {
            Ok(response) => response,
            Err(error) if error.is_panic() => CandidateResponse::Failed {
                message: "provider task panicked".into(),
                usage: None,
            },
            Err(_) => CandidateResponse::Failed {
                message: "provider task aborted".into(),
                usage: None,
            },
        }
    }
}

struct AbortOnDrop<T>(tokio::task::JoinHandle<T>);
impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[derive(Debug, Clone)]
pub struct ServeOptions {
    /// Limit for each response line written. Keep it equal to the host's
    /// `ExternalConfig::max_line_bytes`; a larger report becomes a known error
    /// instead of a protocol violation that would end the process.
    pub max_line_bytes: usize,
    /// Enables the `panic` and `crash` offline behaviors. Only for tests.
    pub allow_fault_injection: bool,
}

impl Default for ServeOptions {
    fn default() -> Self {
        Self {
            max_line_bytes: DEFAULT_MAX_LINE_BYTES,
            allow_fault_injection: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServeError {
    /// The host violated the protocol in a way no response can describe.
    Protocol(String),
    Io(String),
}

impl fmt::Display for ServeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Protocol(message) => write!(f, "protocol error: {message}"),
            Self::Io(message) => write!(f, "I/O error: {message}"),
        }
    }
}

impl std::error::Error for ServeError {}

/// Execute one `fusion.collect` call. Every failure is a known error result:
/// offline providers have no side effects, and a provider failure is recorded
/// in its candidate rather than failing the whole call.
pub async fn handle_call(
    method: &str,
    arguments: Value,
    cancel: CancellationToken,
    options: &ServeOptions,
) -> ExternalResult {
    match collect(method, arguments, cancel, options).await {
        Ok(data) => ExternalResult::Ok { data },
        Err(message) => ExternalResult::Error {
            message,
            outcome_unknown: false,
        },
    }
}

async fn collect(
    method: &str,
    arguments: Value,
    cancel: CancellationToken,
    options: &ServeOptions,
) -> Result<Value, String> {
    if method != COLLECT_METHOD {
        return Err(format!("unknown method {method}"));
    }
    let arguments: CollectArguments = serde_json::from_value(arguments)
        .map_err(|error| format!("invalid {COLLECT_METHOD} arguments: {error}"))?;
    if !(1..=MAX_PER_PROVIDER_TIMEOUT_MS).contains(&arguments.per_provider_timeout_ms) {
        return Err(format!(
            "per_provider_timeout_ms must be within 1..={MAX_PER_PROVIDER_TIMEOUT_MS}"
        ));
    }
    let sources = arguments
        .providers
        .into_iter()
        .map(|spec| spec.build(options))
        .collect::<Result<Vec<_>, _>>()?;
    let report = collect_fusion(
        arguments.input,
        sources,
        Duration::from_millis(arguments.per_provider_timeout_ms),
        cancel,
    )
    .await
    .map_err(|error| error.to_string())?;
    serde_json::to_value(report).map_err(|error| error.to_string())
}

enum Line {
    Text(Vec<u8>),
    TooLong,
}

/// Serve protocol v1 until the host closes the input. Requests are handled
/// one at a time, as the host sends them. A malformed call gets an error
/// response and the process keeps serving. When the input closes mid-call
/// the call is cancelled and no response is written.
pub async fn serve<R, W>(input: R, mut output: W, options: ServeOptions) -> Result<(), ServeError>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin,
{
    if options.max_line_bytes == 0 {
        return Err(ServeError::Protocol(
            "max_line_bytes must be positive".into(),
        ));
    }
    let closed = CancellationToken::new();
    let (sender, mut lines) = mpsc::channel(4);
    let _reader = AbortOnDrop(tokio::spawn(read_lines(
        input,
        MAX_REQUEST_LINE_BYTES.max(options.max_line_bytes),
        sender,
        closed.clone(),
    )));
    let first = match lines.recv().await {
        Some(Line::Text(line)) => line,
        Some(Line::TooLong) => {
            return Err(ServeError::Protocol("handshake exceeds line limit".into()));
        }
        None => return Err(ServeError::Protocol("input closed before handshake".into())),
    };
    match serde_json::from_slice::<HostMessage>(&first) {
        Ok(HostMessage::Handshake { protocol_version })
            if protocol_version == PLUGIN_PROTOCOL_VERSION => {}
        Ok(HostMessage::Handshake { protocol_version }) => {
            return Err(ServeError::Protocol(format!(
                "unsupported protocol version {protocol_version}"
            )));
        }
        _ => return Err(ServeError::Protocol("expected handshake".into())),
    }
    write_line(
        &mut output,
        &ExternalMessage::Handshake {
            manifest: manifest(),
        },
    )
    .await?;
    while let Some(line) = lines.recv().await {
        let line = match line {
            Line::Text(line) => line,
            Line::TooLong => {
                return Err(ServeError::Protocol("request exceeds line limit".into()));
            }
        };
        let (request_id, result) = match serde_json::from_slice::<HostMessage>(&line) {
            Ok(HostMessage::Call {
                request_id,
                method,
                arguments,
            }) => {
                let cancel = CancellationToken::new();
                let result = tokio::select! {
                    biased;
                    _ = closed.cancelled() => {
                        cancel.cancel();
                        return Ok(());
                    }
                    result = handle_call(&method, arguments, cancel.clone(), &options) => result,
                };
                (request_id, result)
            }
            Ok(HostMessage::Handshake { .. }) => {
                eprintln!("cayw-fusion-plugin: ignoring repeated handshake");
                continue;
            }
            Err(error) => match call_request_id(&line) {
                Some(request_id) => (
                    request_id,
                    ExternalResult::Error {
                        message: format!("malformed call: {error}"),
                        outcome_unknown: false,
                    },
                ),
                None => {
                    eprintln!("cayw-fusion-plugin: ignoring malformed message: {error}");
                    continue;
                }
            },
        };
        let mut response = ExternalMessage::Response { request_id, result };
        if encode(&response)?.len() > options.max_line_bytes {
            response = ExternalMessage::Response {
                request_id,
                result: ExternalResult::Error {
                    message: format!(
                        "{COLLECT_METHOD} result exceeds the {}-byte protocol line limit",
                        options.max_line_bytes
                    ),
                    outcome_unknown: false,
                },
            };
        }
        write_line(&mut output, &response).await?;
    }
    Ok(())
}

fn call_request_id(line: &[u8]) -> Option<u64> {
    let value: Value = serde_json::from_slice(line).ok()?;
    if value.get("type")?.as_str()? != "call" {
        return None;
    }
    value.get("request_id")?.as_u64()
}

fn encode(message: &ExternalMessage) -> Result<Vec<u8>, ServeError> {
    serde_json::to_vec(message).map_err(|error| ServeError::Protocol(error.to_string()))
}

async fn write_line<W: AsyncWrite + Unpin>(
    output: &mut W,
    message: &ExternalMessage,
) -> Result<(), ServeError> {
    let mut bytes = encode(message)?;
    bytes.push(b'\n');
    output
        .write_all(&bytes)
        .await
        .map_err(|error| ServeError::Io(error.to_string()))?;
    output
        .flush()
        .await
        .map_err(|error| ServeError::Io(error.to_string()))
}

async fn read_lines<R: AsyncRead + Unpin>(
    input: R,
    limit: usize,
    sender: mpsc::Sender<Line>,
    closed: CancellationToken,
) {
    let mut input = BufReader::new(input);
    let mut line = Vec::new();
    loop {
        let available = match input.fill_buf().await {
            Ok([]) | Err(_) => break,
            Ok(available) => available,
        };
        let end = available.iter().position(|byte| *byte == b'\n');
        let count = end.map_or(available.len(), |index| index + 1);
        let content = count - usize::from(end.is_some());
        if line.len().saturating_add(content) > limit {
            let _ = sender.send(Line::TooLong).await;
            break;
        }
        line.extend_from_slice(&available[..content]);
        input.consume(count);
        if end.is_some()
            && sender
                .send(Line::Text(std::mem::take(&mut line)))
                .await
                .is_err()
        {
            break;
        }
    }
    closed.cancel();
}
