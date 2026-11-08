//! Offline child-process fixture for Runtime lifecycle integration tests.
use cayw_plugins::{
    ExternalMessage, ExternalResult, HostMessage, PluginKind, PluginManifest, PluginRole,
    ToolDefinition,
};
use serde_json::json;
use std::io::{BufRead, Write};

fn emit(message: ExternalMessage) {
    println!("{}", serde_json::to_string(&message).unwrap());
    std::io::stdout().flush().unwrap();
}
fn main() {
    if std::env::var_os("CAYW_TEST_TUI_FIXTURE").is_some() {
        if std::env::args_os().nth(1).is_some() {
            std::process::exit(91);
        }
        for (actual, expected, failure) in [
            ("CAYW_IPC_ADDR", "EXPECTED_ADDR", 92),
            ("CAYW_IPC_TOKEN", "EXPECTED_TOKEN", 93),
            ("CAYW_WORKSPACE_ID", "EXPECTED_WORKSPACE_ID", 94),
        ] {
            if std::env::var_os(actual) != std::env::var_os(expected) {
                std::process::exit(failure);
            }
        }
        println!("workspace connection received");
        std::io::stdout().flush().unwrap();
        std::thread::sleep(std::time::Duration::from_secs(1));
        return;
    }
    let mode = std::env::args().nth(1).expect("mode");
    let input = std::io::stdin();
    let mut lines = input.lock().lines();
    let hello: HostMessage = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
    assert!(matches!(
        hello,
        HostMessage::Handshake {
            protocol_version: 1
        }
    ));
    emit(ExternalMessage::Handshake {
        manifest: PluginManifest {
            name: "process".into(),
            version: "1.0.0".into(),
            protocol_version: 1,
            role: PluginRole::WorkspaceArchitecture,
            kind: PluginKind::External,
            capabilities: vec![ToolDefinition {
                name: "process.echo".into(),
                description: "Offline process lifecycle fixture".into(),
                parameters: json!({"type":"object","additionalProperties":false}),
            }],
            permissions: Default::default(),
        },
    });
    if mode == "idle-exit" {
        let gate = std::env::args().nth(2).expect("exit gate");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !std::path::Path::new(&gate).exists() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        std::process::exit(7);
    }
    for line in lines {
        let HostMessage::Call { request_id, .. } = serde_json::from_str(&line.unwrap()).unwrap()
        else {
            panic!("call")
        };
        if mode == "call-exit" {
            std::process::exit(7)
        }
        emit(ExternalMessage::Response {
            request_id,
            result: ExternalResult::Ok {
                data: json!({"ok":true}),
            },
        });
    }
}
