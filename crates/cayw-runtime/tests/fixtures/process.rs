//! Local deterministic process trees for Runtime cleanup integration tests.
use std::{collections::BTreeMap, path::PathBuf, sync::Arc, time::Duration};

fn wait_for(path: &std::path::Path) {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while !path.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "fixture gate timed out"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}
fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("leaf") => {
            let root = PathBuf::from(&args[1]);
            std::fs::write(root.join("leaf.pid"), std::process::id().to_string()).unwrap();
            wait_for(&root.join("release"));
        }
        Some("chain") => {
            // One process per generation, each spawning the next, so the parent /
            // child / grandchild topology is real and not just a recorded claim.
            let root = PathBuf::from(&args[1]);
            let depth: u32 = args[2].parse().unwrap();
            // Only the leader emits, and only when the test opens `output-release`.
            // The test writes that file only after all three generations are
            // durably recorded, so an output-budget stop can never outrun the
            // recorder of any generation. Readiness markers are not that gate:
            // `p2.pid` only proves the grandchild started.
            let emit = args.get(3).map(String::as_str) == Some("emit");
            std::fs::write(
                root.join(format!("p{depth}.pid")),
                std::process::id().to_string(),
            )
            .unwrap();
            if depth < 2 {
                let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                    .arg("chain")
                    .arg(&root)
                    .arg((depth + 1).to_string())
                    .spawn()
                    .unwrap();
                if emit {
                    wait_for(&root.join("output-release"));
                    use std::io::Write;
                    let mut out = std::io::stdout();
                    let chunk = vec![b'x'; 256];
                    for _ in 0..4096 {
                        if out.write_all(&chunk).is_err() || out.flush().is_err() {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(50));
                    }
                }
                wait_for(&root.join("release"));
                child.wait().unwrap();
            } else {
                wait_for(&root.join("release"));
            }
        }
        Some("tree") => {
            let root = PathBuf::from(&args[1]);
            std::fs::write(root.join("leader.pid"), std::process::id().to_string()).unwrap();
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .arg("leaf")
                .arg(&root)
                .spawn()
                .unwrap();
            wait_for(&root.join("release"));
            child.wait().unwrap();
        }
        Some("host") => {
            let root = PathBuf::from(&args[1]);
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let mut config = cayw_runtime::RuntimeConfig {
                    max_concurrent_runs: 1,
                    max_queued_runs: 1,
                    max_run_time: Duration::from_secs(90),
                    ..Default::default()
                };
                config.codemode.timeout = Duration::from_secs(60);
                config.host.call_timeout = Duration::from_secs(60);
                config.host.profiles.insert(
                    "tree".into(),
                    cayw_host::Profile {
                        kind: cayw_host::ProfileKind::Test,
                        program: std::env::current_exe().unwrap(),
                        args: vec!["tree".into(), root.to_string_lossy().into_owned()],
                        env: BTreeMap::new(),
                        timeout: Duration::from_secs(60),
                        max_output_bytes: 4096,
                        allow_filter: false,
                        io: cayw_host::ProcessIo::Pipes,
                    },
                );
                config.permissions = vec![cayw_runtime::ScopedPermission::exact(
                    "test.run",
                    serde_json::json!({"profile":"tree"}),
                )];
                let runtime = cayw_runtime::Runtime::open(
                    root.join("session.db"),
                    root.join("workspace"),
                    Arc::new(ProfileProvider),
                    cayw_ai::Model::new("fixture", "process-owner"),
                    config,
                )
                .unwrap();
                let id = runtime.create(None).unwrap().id;
                std::fs::write(root.join("session.id"), &id).unwrap();
                runtime
                    .prompt(
                        &id,
                        "run a host-selected process tree",
                        cayw_ai::CancellationToken::new(),
                    )
                    .await
                    .unwrap();
            });
        }
        Some("chainhost") => {
            // A real, independent host process. It owns a Runtime, starts the real
            // parent/child/grandchild chain, and either (a) waits to be killed while
            // its work is still running, or (b) performs the original graceful
            // shutdown — public cancel, then wait for the owned cleanup to reach its
            // terminal — and only then exits normally.
            let root = PathBuf::from(&args[1]);
            let pty = args.get(2).map(String::as_str) == Some("pty");
            let shutdown = args.get(3).map(String::as_str) == Some("shutdown");
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let mut config = cayw_runtime::RuntimeConfig {
                    max_concurrent_runs: 1,
                    max_queued_runs: 1,
                    max_run_time: Duration::from_secs(90),
                    ..Default::default()
                };
                config.codemode.timeout = Duration::from_secs(60);
                config.host.call_timeout = Duration::from_secs(60);
                config.host.profiles.insert(
                    "chain".into(),
                    cayw_host::Profile {
                        kind: cayw_host::ProfileKind::Test,
                        program: std::env::current_exe().unwrap(),
                        args: vec![
                            "chain".into(),
                            root.to_string_lossy().into_owned(),
                            "0".into(),
                        ],
                        env: BTreeMap::new(),
                        timeout: Duration::from_secs(60),
                        max_output_bytes: 4096,
                        allow_filter: false,
                        io: if pty {
                            cayw_host::ProcessIo::Pty {
                                columns: 80,
                                rows: 24,
                            }
                        } else {
                            cayw_host::ProcessIo::Pipes
                        },
                    },
                );
                config.permissions = vec![cayw_runtime::ScopedPermission::exact(
                    "test.run",
                    serde_json::json!({"profile":"chain"}),
                )];
                let runtime = cayw_runtime::Runtime::open(
                    root.join("session.db"),
                    root.join("workspace"),
                    Arc::new(ChainProfileProvider),
                    cayw_ai::Model::new("fixture", "process-owner"),
                    config,
                )
                .unwrap();
                let id = runtime.create(None).unwrap().id;
                std::fs::write(root.join("session.id"), &id).unwrap();
                let prompt = {
                    let runtime = runtime.clone();
                    let id = id.clone();
                    tokio::spawn(async move {
                        runtime
                            .prompt(
                                &id,
                                "run a process chain",
                                cayw_ai::CancellationToken::new(),
                            )
                            .await
                    })
                };
                std::fs::write(root.join("host.ready"), "ready").unwrap();
                if shutdown {
                    wait_for(&root.join("shutdown"));
                    runtime.cancel(&id).unwrap();
                    // The original graceful path: cancel, then wait until the owned
                    // worker has finished its own process cleanup and left the
                    // active set. Only then does this real host process exit.
                    runtime.wait_idle(&id).await;
                    let task = runtime.tasks(&id).unwrap().remove(0);
                    let drained = runtime.drain_task_processes(&task.id).await;
                    std::fs::write(
                        root.join("host.exited"),
                        format!("cancel + wait_idle + drain_task_processes={drained:?}"),
                    )
                    .unwrap();
                }
                let _ = prompt.await;
            });
        }
        _ => panic!("unknown process fixture mode"),
    }
}
struct ChainProfileProvider;
impl cayw_ai::Provider for ChainProfileProvider {
    fn stream(
        &self,
        request: cayw_ai::ModelRequest,
        _: cayw_ai::CancellationToken,
    ) -> cayw_ai::AssistantStream {
        let message = cayw_runtime::fixture::script_response(
            &request,
            "return cayw.call('test.run',{profile='chain'})",
            &format!("chain-{}", request.messages.len()),
        );
        Box::pin(futures::stream::iter([cayw_ai::AssistantEvent::Done {
            message,
        }]))
    }
}
struct ProfileProvider;
impl cayw_ai::Provider for ProfileProvider {
    fn stream(
        &self,
        request: cayw_ai::ModelRequest,
        _: cayw_ai::CancellationToken,
    ) -> cayw_ai::AssistantStream {
        let message = cayw_runtime::fixture::script_response(
            &request,
            "return cayw.call('test.run',{profile='tree'})",
            &format!("process-{}", request.messages.len()),
        );
        Box::pin(futures::stream::iter([cayw_ai::AssistantEvent::Done {
            message,
        }]))
    }
}
