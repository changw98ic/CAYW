//! External StrongFlow process. It wraps the library state machine and is not
//! linked into the default cayw binary.
//!
//! The durable file is a validated StrongFlowState. Its plugin manifest stays
//! the native library manifest so the host can checkpoint it. The handshake
//! manifest is that same manifest with kind External.

use cayw_plugins::strongflow::*;
use cayw_plugins::{ExternalMessage, ExternalResult, HostMessage, PLUGIN_PROTOCOL_VERSION};
use serde_json::{Value, json};
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::Arc;

fn main() {
    if let Err(error) = run() {
        let _ = writeln!(std::io::stderr(), "cayw-plugin-strongflow: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let state_path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("state file path is required")?;
    let flow = Arc::new(if state_path.exists() {
        let bytes = std::fs::read(&state_path).map_err(|error| error.to_string())?;
        let state: StrongFlowState =
            serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
        StrongFlow::restore(state).map_err(|error| error.to_string())?
    } else {
        StrongFlow::new()
    });
    let manifest = StrongFlow::external_manifest();
    let input = std::io::stdin();
    let mut lines = input.lock().lines();
    let handshake = read_host(lines.next())?;
    let HostMessage::Handshake { protocol_version } = handshake else {
        return Err("expected handshake".into());
    };
    if protocol_version != PLUGIN_PROTOCOL_VERSION {
        return Err("unsupported protocol version".into());
    }
    emit(&ExternalMessage::Handshake { manifest });
    persist(&flow, &state_path)?;
    for line in lines {
        let HostMessage::Call {
            request_id,
            method,
            arguments,
        } = read_host(Some(line))?
        else {
            return Err("expected call".into());
        };
        let result = dispatch(&flow, &method, arguments);
        if matches!(result, ExternalResult::Ok { .. }) {
            persist(&flow, &state_path)?;
        }
        emit(&ExternalMessage::Response { request_id, result });
    }
    Ok(())
}

fn dispatch(flow: &StrongFlow, method: &str, arguments: Value) -> ExternalResult {
    let outcome = (|| -> Result<Value, String> {
        match method {
            "strongflow.describe" => {
                let id = required_str(&arguments, "workflow_id")?;
                serde_json::to_value(flow.describe(&id).map_err(|error| error.to_string())?)
                    .map_err(|error| error.to_string())
            }
            "strongflow.ready" => {
                let id = required_str(&arguments, "workflow_id")?;
                serde_json::to_value(flow.ready(&id).map_err(|error| error.to_string())?)
                    .map_err(|error| error.to_string())
            }
            "strongflow.host.create_plan" => {
                let plan: WorkflowPlan = serde_json::from_value(
                    arguments.get("plan").cloned().ok_or("plan is required")?,
                )
                .map_err(|error| error.to_string())?;
                flow.create_plan(plan).map_err(|error| error.to_string())?;
                Ok(json!({"status": "created"}))
            }
            "strongflow.host.start_step" => {
                flow.start_step(
                    &required_str(&arguments, "workflow_id")?,
                    &required_str(&arguments, "step_id")?,
                )
                .map_err(|error| error.to_string())?;
                Ok(json!({"status": "running"}))
            }
            "strongflow.host.submit_evidence" => {
                let evidence: ExecutionEvidence = serde_json::from_value(
                    arguments
                        .get("evidence")
                        .cloned()
                        .ok_or("evidence is required")?,
                )
                .map_err(|error| error.to_string())?;
                let status = flow
                    .submit_evidence(
                        &required_str(&arguments, "workflow_id")?,
                        &required_str(&arguments, "step_id")?,
                        evidence,
                    )
                    .map_err(|error| error.to_string())?;
                serde_json::to_value(status).map_err(|error| error.to_string())
            }
            _ => Err(format!("denied method {method}")),
        }
    })();
    match outcome {
        Ok(data) => ExternalResult::Ok { data },
        Err(message) => ExternalResult::Error {
            message,
            outcome_unknown: false,
        },
    }
}

fn required_str(arguments: &Value, name: &str) -> Result<String, String> {
    arguments
        .get(name)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("{name} is required"))
}

/// Write the library checkpoint. Kind stays Native so StrongFlowState::validate
/// accepts it. The host reads this file; it is not an agent-facing capability.
fn persist(flow: &StrongFlow, path: &std::path::Path) -> Result<(), String> {
    let state = flow.export_state();
    state.validate().map_err(|error| error.to_string())?;
    let bytes = serde_json::to_vec(&state).map_err(|error| error.to_string())?;
    let temporary = path.with_extension("tmp");
    std::fs::write(&temporary, bytes).map_err(|error| error.to_string())?;
    std::fs::rename(temporary, path).map_err(|error| error.to_string())
}

fn read_host(line: Option<Result<String, std::io::Error>>) -> Result<HostMessage, String> {
    let line = line
        .ok_or("plugin closed stdin")?
        .map_err(|error| error.to_string())?;
    serde_json::from_str(&line).map_err(|error| error.to_string())
}

fn emit(message: &ExternalMessage) {
    println!("{}", serde_json::to_string(message).expect("JSON"));
    let _ = std::io::stdout().flush();
}
