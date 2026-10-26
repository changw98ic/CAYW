//! Minimal unmodified-shape sample extension for F15 Pi compat host.
//!
//! Speaks the CAYW external plugin JSON-lines protocol (stdin/stdout). Invoked
//! via `PiCompatHost::install_sample_extension` / `install_external` when the
//! executable is present on disk. Not an npm Pi ExtensionAPI package.

use cayw_plugins::{
    ExternalMessage, ExternalResult, HostMessage, PLUGIN_PROTOCOL_VERSION, PluginManifest,
};
use cayw_pi_compat::PiSampleExtension;
use serde_json::{Value, json};
use std::io::{BufRead, Write};

fn main() {
    let input = std::io::stdin();
    let mut lines = input.lock().lines();
    let handshake: HostMessage =
        serde_json::from_str(&lines.next().expect("handshake").expect("input")).expect("host JSON");
    assert!(matches!(
        handshake,
        HostMessage::Handshake {
            protocol_version: PLUGIN_PROTOCOL_VERSION
        }
    ));

    let manifest: PluginManifest = PiSampleExtension::expected_manifest();
    emit(&ExternalMessage::Handshake { manifest });

    for line in lines {
        let message: HostMessage = serde_json::from_str(&line.expect("input")).expect("host JSON");
        let HostMessage::Call {
            request_id,
            method,
            arguments,
        } = message
        else {
            panic!("call expected");
        };
        let result = handle_call(&method, arguments);
        emit(&ExternalMessage::Response { request_id, result });
    }
}

fn handle_call(method: &str, arguments: Value) -> ExternalResult {
    if method != PiSampleExtension::TOOL {
        return ExternalResult::Error {
            message: format!("unknown method {method}"),
            outcome_unknown: false,
        };
    }
    let note = arguments
        .get("note")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    ExternalResult::Ok {
        data: json!({
            "pong": true,
            "note": note,
            "package": PiSampleExtension::PACKAGE_NAME,
            "version": PiSampleExtension::PACKAGE_VERSION,
            "path": "external-subprocess",
            "surface": "tools.invoke"
        }),
    }
}

fn emit(message: &ExternalMessage) {
    println!("{}", serde_json::to_string(message).expect("JSON"));
    std::io::stdout().flush().expect("flush");
}
