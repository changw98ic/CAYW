//! Standalone offline protocol fixture. Deliberately misbehaving modes are used
//! only by integration tests; no network or real provider is contacted.

use cayw_modules::*;
use serde_json::{Value, json};
use std::io::{BufRead, Write};

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "echo".into());
    if mode == "descendant" {
        std::thread::sleep(std::time::Duration::from_secs(300));
        return;
    }
    let input = std::io::stdin();
    let mut lines = input.lock().lines();
    let handshake: HostMessage =
        serde_json::from_str(&lines.next().expect("handshake").expect("input")).expect("host JSON");
    assert!(matches!(
        handshake,
        HostMessage::Handshake {
            protocol_version: MODULE_PROTOCOL_VERSION
        }
    ));
    if mode == "handshake_hang" {
        std::thread::sleep(std::time::Duration::from_secs(300));
    }
    if mode == "handshake_large" {
        println!("{}", "x".repeat(80_000));
        return;
    }
    let mut manifest = fixture_manifest();
    #[cfg(feature = "strongflow")]
    if matches!(
        mode.as_str(),
        "strongflow_idle_exit" | "strongflow_call_exit" | "strongflow_call_exit_tree"
    ) {
        manifest = strongflow::StrongFlow::new().manifest().clone();
        manifest.kind = ModuleKind::External;
    }
    if mode == "version" {
        manifest.protocol_version += 1;
    }
    if mode == "capabilities" {
        manifest.capabilities[0].name = "escalate".into();
    }
    emit(&ExternalMessage::Handshake { manifest });
    if matches!(mode.as_str(), "idle_exit" | "strongflow_idle_exit") {
        wait_exit_gate();
        return;
    }
    for line in lines {
        let message: HostMessage = serde_json::from_str(&line.expect("input")).expect("host JSON");
        let HostMessage::Call {
            request_id,
            method,
            arguments,
        } = message
        else {
            panic!("call expected")
        };
        if matches!(
            mode.as_str(),
            "strongflow_call_exit" | "strongflow_call_exit_tree"
        ) {
            assert_eq!(method, "strongflow.describe");
            if mode == "strongflow_call_exit_tree" {
                let mut child =
                    std::process::Command::new(std::env::current_exe().expect("executable"))
                        .arg("descendant")
                        .spawn()
                        .expect("descendant holding response pipe open");
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
            }
            std::process::exit(7);
        }
        assert_eq!(method, "fixture.echo");
        match mode.as_str() {
            "hang" => {
                if let Some(path) = arguments.get("started").and_then(Value::as_str) {
                    std::fs::write(path, std::process::id().to_string()).expect("started marker");
                }
                std::thread::sleep(std::time::Duration::from_secs(300));
            }
            "tree" => {
                let mut child =
                    std::process::Command::new(std::env::current_exe().expect("executable"))
                        .arg("descendant")
                        .spawn()
                        .expect("descendant process");
                emit(&ExternalMessage::Response {
                    request_id,
                    result: ExternalResult::Ok {
                        data: json!({"parent":std::process::id(),"descendant":child.id()}),
                    },
                });
                // The host kills this process group during the test. If it does
                // not, still reap the descendant when its bounded sleep ends.
                let _ = child.wait();
            }
            "gated_call_exit" => {
                wait_exit_gate();
                std::process::exit(7);
            }
            "crash" => std::process::exit(7),
            "wrong_id" => emit(&ExternalMessage::Response {
                request_id: request_id + 1,
                result: ExternalResult::Ok { data: arguments },
            }),
            "oversize" => {
                print!("{}", "x".repeat(80_000));
                std::io::stdout().flush().expect("flush");
            }
            "stderr" => {
                let _ = std::io::stderr().write_all(&vec![b'x'; 80_000]);
                std::thread::sleep(std::time::Duration::from_secs(300));
            }
            "approval" => {
                println!(
                    "{{\"type\":\"response\",\"request_id\":{request_id},\"result\":{{\"status\":\"ok\",\"data\":{{}}}},\"approved\":true}}"
                );
            }
            "failure" => emit(&ExternalMessage::Response {
                request_id,
                result: ExternalResult::Error {
                    message: "known fixture failure".into(),
                    outcome_unknown: false,
                },
            }),
            "unknown" => emit(&ExternalMessage::Response {
                request_id,
                result: ExternalResult::Error {
                    message: "fixture side effect unknown".into(),
                    outcome_unknown: true,
                },
            }),
            _ => emit(&ExternalMessage::Response {
                request_id,
                result: ExternalResult::Ok { data: arguments },
            }),
        }
    }
}

fn wait_exit_gate() {
    if let Some(marker) = std::env::args().nth(3) {
        let mut child = std::process::Command::new(std::env::current_exe().expect("executable"))
            .arg("descendant")
            .spawn()
            .expect("descendant holding stdout open");
        std::fs::write(marker, child.id().to_string()).expect("descendant marker");
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
    if let Some(marker) = std::env::args().nth(4) {
        std::fs::write(marker, std::process::id().to_string()).expect("parent marker");
    }
    let gate = std::env::args().nth(2).expect("exit gate");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !std::path::Path::new(&gate).exists() {
        assert!(std::time::Instant::now() < deadline, "exit gate not opened");
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

fn fixture_manifest() -> ModuleManifest {
    ModuleManifest {
        name: "fixture".into(),
        version: "1.0.0".into(),
        protocol_version: MODULE_PROTOCOL_VERSION,
        kind: ModuleKind::External,
        capabilities: vec![ToolDefinition {
            name: "fixture.echo".into(),
            description: "Echo offline JSON".into(),
            parameters: json!({"type":"object"}),
        }],
        permissions: Default::default(),
    }
}

fn emit(message: &ExternalMessage) {
    println!("{}", serde_json::to_string(message).expect("JSON"));
    std::io::stdout().flush().expect("flush");
}
