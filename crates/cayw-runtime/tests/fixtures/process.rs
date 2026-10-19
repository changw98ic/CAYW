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
        _ => panic!("unknown process fixture mode"),
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
