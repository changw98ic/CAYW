//! External Fusion program. Candidate answers arrive in the call arguments.
//! This process does not call a model and does not elect a winner.

use cayw_plugins::fusion::{CandidateResponse, FusionCandidateSource, FusionInput, collect_fusion};
use cayw_plugins::{
    CancellationToken, ExternalMessage, ExternalResult, HostMessage, PLUGIN_PROTOCOL_VERSION,
    PluginError, PluginKind, PluginManifest, ToolDefinition,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::io::{BufRead, Write};
use std::sync::Arc;
use std::time::Duration;

const METHOD: &str = "fusion.collect";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CollectArguments {
    input: FusionInput,
    candidates: Vec<SuppliedCandidate>,
    timeout_ms: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SuppliedCandidate {
    provider_id: String,
    response: CandidateResponse,
}

struct SuppliedSource {
    provider_id: String,
    response: CandidateResponse,
}

#[async_trait::async_trait]
impl FusionCandidateSource for SuppliedSource {
    fn provider_id(&self) -> &str {
        &self.provider_id
    }

    async fn answer(
        &self,
        _input: Arc<FusionInput>,
        _cancel: CancellationToken,
    ) -> CandidateResponse {
        self.response.clone()
    }
}

fn manifest() -> PluginManifest {
    PluginManifest {
        name: "fusion".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        protocol_version: PLUGIN_PROTOCOL_VERSION,
        kind: PluginKind::External,
        capabilities: vec![ToolDefinition {
            name: METHOD.into(),
            description: "Collect host-supplied candidate answers and list disagreements. Does not call a model or elect a winner.".into(),
            parameters: collect_parameters(),
        }],
        permissions: Default::default(),
    }
}

fn collect_parameters() -> Value {
    json!({"type": "object"})
}

fn main() {
    let input = std::io::stdin();
    let mut lines = input.lock().lines();
    let Some(first) = lines.next() else {
        return;
    };
    let Ok(first) = first else {
        return;
    };
    let Ok(HostMessage::Handshake { protocol_version }) = serde_json::from_str(&first) else {
        return;
    };
    if protocol_version != PLUGIN_PROTOCOL_VERSION {
        return;
    }
    emit(&ExternalMessage::Handshake {
        manifest: manifest(),
    });
    for line in lines {
        let Ok(line) = line else {
            return;
        };
        let Ok(message) = serde_json::from_str::<HostMessage>(&line) else {
            return;
        };
        let HostMessage::Call {
            request_id,
            method,
            arguments,
        } = message
        else {
            return;
        };
        emit(&ExternalMessage::Response {
            request_id,
            result: dispatch(&method, arguments),
        });
    }
}

fn dispatch(method: &str, arguments: Value) -> ExternalResult {
    if method != METHOD {
        return error(PluginError::Denied(method.into()));
    }
    match collect(arguments) {
        Ok(report) => ExternalResult::Ok { data: report },
        Err(failure) => error(failure),
    }
}

fn collect(arguments: Value) -> Result<Value, PluginError> {
    let arguments: CollectArguments = serde_json::from_value(arguments)
        .map_err(|error| PluginError::InvalidArguments(error.to_string()))?;
    let timeout = Duration::from_millis(arguments.timeout_ms);
    let sources = arguments
        .candidates
        .into_iter()
        .map(|candidate| {
            Arc::new(SuppliedSource {
                provider_id: candidate.provider_id,
                response: candidate.response,
            }) as Arc<dyn FusionCandidateSource>
        })
        .collect();
    let report = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .map_err(|error| PluginError::Failed(error.to_string()))?
        .block_on(collect_fusion(
            arguments.input,
            sources,
            timeout,
            CancellationToken::new(),
        ))?;
    serde_json::to_value(report).map_err(|error| PluginError::Failed(error.to_string()))
}

fn error(failure: PluginError) -> ExternalResult {
    ExternalResult::Error {
        message: failure.to_string(),
        outcome_unknown: failure.outcome_unknown(),
    }
}

fn emit(message: &impl Serialize) {
    let encoded = serde_json::to_string(message).expect("plugin JSON");
    let mut output = std::io::stdout().lock();
    writeln!(output, "{encoded}").expect("plugin stdout");
    output.flush().expect("plugin flush");
}
