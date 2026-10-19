use cayw_plugins::*;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

fn config(mode: &str) -> ExternalConfig {
    ExternalConfig::new(
        PathBuf::from(env!("CARGO_BIN_EXE_cayw-plugin-fixture")),
        vec![mode.into()],
        PluginManifest {
            name: "fixture".into(),
            version: "1.0.0".into(),
            protocol_version: PLUGIN_PROTOCOL_VERSION,
            kind: PluginKind::External,
            capabilities: vec![ToolDefinition {
                name: "fixture.echo".into(),
                description: "Echo offline JSON".into(),
                parameters: json!({"type":"object"}),
            }],
            permissions: Default::default(),
        },
    )
}
fn context() -> PluginContext {
    PluginContext::new(["fixture.echo".into()], [])
}

async fn load(mode: &str) -> PluginRegistry {
    let registry = PluginRegistry::new();
    registry
        .load_external(config(mode), context(), CancellationToken::new())
        .await
        .unwrap();
    registry
}

#[cfg(feature = "strongflow")]
#[tokio::test]
async fn idle_external_exit_freezes_session_without_invoking_a_capability() {
    use cayw_plugins::strongflow::{SessionBinding, StrongFlow};

    let registry = PluginRegistry::new();
    let exit_gate = marker();
    let mut manifest = StrongFlow::new().manifest().clone();
    manifest.kind = PluginKind::External;
    let configuration = ExternalConfig::new(
        PathBuf::from(env!("CARGO_BIN_EXE_cayw-plugin-fixture")),
        vec![
            "strongflow_idle_exit".into(),
            exit_gate.to_str().unwrap().into(),
        ],
        manifest,
    );
    let description = registry
        .load_external(
            configuration,
            PluginContext::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let mut binding =
        SessionBinding::strongflow("idle-process-session".into(), &description).unwrap();
    binding.check_available(&registry).unwrap();
    tokio::fs::write(&exit_gate, b"exit").await.unwrap();
    let frozen = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if binding.check_available(&registry).is_err() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    let _ = tokio::fs::remove_file(exit_gate).await;
    frozen.expect("idle process exit must freeze without a capability probe");
    assert!(binding.frozen_reason().is_some());
    assert_eq!(
        binding.bound_plugin().unwrap().generation,
        description.generation
    );
}

#[cfg(feature = "strongflow")]
#[tokio::test]
async fn terminal_external_call_freezes_the_original_binding() {
    use cayw_plugins::strongflow::{SessionBinding, StrongFlow};

    let modes = [
        "strongflow_call_exit",
        #[cfg(unix)]
        "strongflow_call_exit_tree",
    ];
    for mode in modes {
        let registry = PluginRegistry::new();
        let mut manifest = StrongFlow::new().manifest().clone();
        manifest.kind = PluginKind::External;
        let description = registry
            .load_external(
                ExternalConfig::new(
                    PathBuf::from(env!("CARGO_BIN_EXE_cayw-plugin-fixture")),
                    vec![mode.into()],
                    manifest,
                ),
                PluginContext::new(["strongflow.describe".into()], []),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let mut binding =
            SessionBinding::strongflow("call-process-session".into(), &description).unwrap();
        binding.check_available(&registry).unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(3),
            registry.call(
                "strongflow",
                "strongflow.describe",
                json!({"workflow_id":"fixture"}),
                CancellationToken::new(),
                Duration::from_secs(20),
            ),
        )
        .await
        .expect("parent exit must terminate active exchange despite inherited pipe");
        assert!(result.unwrap_err().outcome_unknown());
        assert!(binding.check_available(&registry).is_err());
        assert!(binding.frozen_reason().is_some());
        assert_eq!(
            binding.bound_plugin().unwrap().generation,
            description.generation
        );
    }
}

#[cfg(all(unix, feature = "strongflow"))]
#[tokio::test]
async fn idle_parent_exit_is_detected_even_when_descendant_keeps_stdout_open() {
    use cayw_plugins::strongflow::{SessionBinding, StrongFlow};

    let registry = PluginRegistry::new();
    let exit_gate = marker();
    let descendant_marker = marker();
    let mut manifest = StrongFlow::new().manifest().clone();
    manifest.kind = PluginKind::External;
    let description = registry
        .load_external(
            ExternalConfig::new(
                PathBuf::from(env!("CARGO_BIN_EXE_cayw-plugin-fixture")),
                vec![
                    "strongflow_idle_exit".into(),
                    exit_gate.to_str().unwrap().into(),
                    descendant_marker.to_str().unwrap().into(),
                ],
                manifest,
            ),
            PluginContext::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let mut binding =
        SessionBinding::strongflow("descendant-process-session".into(), &description).unwrap();
    let _descendant = wait_started(&descendant_marker).await;
    binding.check_available(&registry).unwrap();
    tokio::fs::write(&exit_gate, b"exit").await.unwrap();
    let frozen = tokio::time::timeout(Duration::from_secs(2), async {
        while binding.check_available(&registry).is_ok() {
            tokio::task::yield_now().await;
        }
    })
    .await;
    registry.unload("strongflow").unwrap();
    let _ = tokio::fs::remove_file(exit_gate).await;
    let _ = tokio::fs::remove_file(descendant_marker).await;
    frozen.expect("parent exit must freeze despite inherited stdout");
    assert!(binding.frozen_reason().is_some());
}

#[cfg(unix)]
struct ExitFixture {
    gate: PathBuf,
    descendant: PathBuf,
    parent: PathBuf,
}

#[cfg(unix)]
impl ExitFixture {
    fn new() -> Self {
        Self {
            gate: marker(),
            descendant: marker(),
            parent: marker(),
        }
    }

    fn configure(&self, configuration: &mut ExternalConfig) {
        configuration.argv.extend([
            self.gate.to_str().unwrap().into(),
            self.descendant.to_str().unwrap().into(),
            self.parent.to_str().unwrap().into(),
        ]);
    }

    /// This blocks the current-thread runtime. OS process state, rather than a
    /// timer or a plugin call, proves exit before any host availability check.
    fn exit_without_scheduling(&self) -> (u32, u32) {
        use std::time::Instant;

        let deadline = Instant::now() + Duration::from_secs(5);
        let read_pid = |path: &std::path::Path| loop {
            if let Ok(value) = std::fs::read_to_string(path)
                && let Ok(pid) = value.parse::<u32>()
            {
                break pid;
            }
            assert!(Instant::now() < deadline, "fixture did not start");
            std::thread::sleep(Duration::from_millis(1));
        };
        let parent = read_pid(&self.parent);
        let descendant = read_pid(&self.descendant);
        assert!(process_state(descendant).is_some_and(|state| !state.starts_with('Z')));
        std::fs::write(&self.gate, b"exit").unwrap();
        loop {
            if process_state(parent).is_none_or(|state| state.starts_with('Z')) {
                break;
            }
            assert!(Instant::now() < deadline, "fixture parent did not exit");
            std::thread::sleep(Duration::from_millis(1));
        }
        (parent, descendant)
    }
}

#[cfg(unix)]
impl Drop for ExitFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.gate);
        let _ = std::fs::remove_file(&self.descendant);
        let _ = std::fs::remove_file(&self.parent);
    }
}

#[cfg(unix)]
fn process_state(pid: u32) -> Option<String> {
    let output = std::process::Command::new("/bin/ps")
        .args(["-p", &pid.to_string(), "-o", "stat="])
        .output()
        .unwrap();
    assert!(output.status.success() || output.status.code() == Some(1));
    let state = String::from_utf8(output.stdout).unwrap().trim().to_string();
    (!state.is_empty()).then_some(state)
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn direct_availability_detects_exit_without_scheduling_and_cleans_descendant() {
    let fixture = ExitFixture::new();
    let mut configuration = config("idle_exit");
    fixture.configure(&mut configuration);
    let plugin = ExternalPlugin::spawn(configuration, CancellationToken::new())
        .await
        .unwrap();
    plugin.check_available().unwrap();
    let (parent, descendant) = fixture.exit_without_scheduling();
    let checks: Vec<_> = (0..3).map(|_| plugin.check_available()).collect();
    // No await has occurred since the parent was released. Cleanup can now run.
    plugin.shutdown();
    wait_dead(parent).await;
    wait_dead(descendant).await;
    for result in checks {
        assert!(
            matches!(result, Err(PluginError::Unavailable(_))),
            "{result:?}"
        );
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn registry_availability_detects_exit_without_scheduling_and_cleans_descendant() {
    let fixture = ExitFixture::new();
    let mut configuration = config("idle_exit");
    fixture.configure(&mut configuration);
    let registry = PluginRegistry::new();
    let loaded = registry
        .load_external(configuration, context(), CancellationToken::new())
        .await
        .unwrap();
    registry.check_available("fixture").unwrap();
    let (parent, descendant) = fixture.exit_without_scheduling();
    let checks: Vec<_> = (0..3)
        .map(|_| registry.check_available("fixture"))
        .collect();
    assert_eq!(registry.describe("fixture").unwrap(), loaded);
    registry.unload("fixture").unwrap();
    wait_dead(parent).await;
    wait_dead(descendant).await;
    for result in checks {
        assert!(
            matches!(result, Err(PluginError::Unavailable(_))),
            "{result:?}"
        );
    }
}

#[cfg(all(unix, feature = "strongflow"))]
#[tokio::test(flavor = "current_thread")]
async fn binding_availability_freezes_on_exit_without_scheduling() {
    use cayw_plugins::strongflow::{SessionBinding, SessionMode, StrongFlow};

    let fixture = ExitFixture::new();
    let mut configuration = config("strongflow_idle_exit");
    configuration.expected_manifest = StrongFlow::new().manifest().clone();
    configuration.expected_manifest.kind = PluginKind::External;
    fixture.configure(&mut configuration);
    let registry = PluginRegistry::new();
    let loaded = registry
        .load_external(
            configuration,
            PluginContext::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let mut binding = SessionBinding::strongflow("blocked-executor".into(), &loaded).unwrap();
    let original = binding.bound_plugin().cloned();
    binding.check_available(&registry).unwrap();
    let (parent, descendant) = fixture.exit_without_scheduling();
    let check = binding.check_available(&registry);
    let frozen = binding.frozen_reason().is_some();
    let second_check = registry.check_available("strongflow");
    registry.unload("strongflow").unwrap();
    wait_dead(parent).await;
    wait_dead(descendant).await;
    assert!(matches!(check, Err(PluginError::Unavailable(_))));
    assert!(matches!(second_check, Err(PluginError::Unavailable(_))));
    assert!(frozen, "first synchronous check must freeze the binding");
    assert_eq!(binding.mode(), SessionMode::StrongFlow);
    assert_eq!(binding.bound_plugin(), original.as_ref());
    assert!(binding.check_available(&registry).is_err());
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn synchronous_exit_detection_keeps_an_in_flight_call_unknown() {
    let fixture = ExitFixture::new();
    let mut configuration = config("gated_call_exit");
    configuration.call_timeout = Duration::from_secs(20);
    fixture.configure(&mut configuration);
    let plugin = Arc::new(
        ExternalPlugin::spawn(configuration, CancellationToken::new())
            .await
            .unwrap(),
    );
    let call = {
        let plugin = plugin.clone();
        tokio::spawn(async move {
            plugin
                .call("fixture.echo", json!({}), CancellationToken::new())
                .await
        })
    };
    // The marker is only written after the child has received this request.
    wait_started(&fixture.parent).await;
    let (parent, descendant) = fixture.exit_without_scheduling();
    let check = plugin.check_available();
    let result = tokio::time::timeout(Duration::from_secs(5), call)
        .await
        .expect("exit check must wake the in-flight request")
        .unwrap();
    plugin.shutdown();
    wait_dead(parent).await;
    wait_dead(descendant).await;
    assert!(matches!(check, Err(PluginError::Unavailable(_))));
    assert!(result.unwrap_err().outcome_unknown());
    assert!(matches!(
        plugin
            .call("fixture.echo", json!({}), CancellationToken::new())
            .await,
        Err(PluginError::Unavailable(_))
    ));
}

#[cfg(unix)]
#[test]
fn runtime_shutdown_releases_child_while_plugin_is_still_owned() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let started = marker();
    let (plugin, pid) = runtime.block_on(async {
        let plugin = Arc::new(
            ExternalPlugin::spawn(config("hang"), CancellationToken::new())
                .await
                .unwrap(),
        );
        let call_plugin = plugin.clone();
        let call_marker = started.clone();
        tokio::spawn(async move {
            call_plugin
                .call(
                    "fixture.echo",
                    json!({"started":call_marker}),
                    CancellationToken::new(),
                )
                .await
        });
        let pid = wait_started(&started).await;
        (plugin, pid)
    });
    drop(runtime);
    assert!(plugin.check_available().is_err());
    // A fresh runtime can reap Tokio's orphan queue after supervisor teardown.
    // Retaining the plugin must not retain an unreaped child indefinitely.
    let reaper = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let reaped = reaper.block_on(async {
        tokio::time::timeout(Duration::from_secs(2), async {
            while process_state(pid).is_some() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
    });
    drop(plugin);
    reaper.block_on(wait_dead(pid));
    let _ = std::fs::remove_file(started);
    reaped.expect("supervisor shutdown must release the child even with a live plugin owner");
}

#[tokio::test]
async fn actual_process_handshake_and_multiple_paired_requests() {
    let registry = load("echo").await;
    for n in 0..4 {
        assert_eq!(
            registry
                .call(
                    "fixture",
                    "fixture.echo",
                    json!({"n":n}),
                    CancellationToken::new(),
                    Duration::from_secs(3)
                )
                .await
                .unwrap(),
            json!({"n":n})
        );
    }
}

#[tokio::test]
async fn external_data_cannot_approve_or_expand_grants() {
    let registry = load("echo").await;
    assert_eq!(
        registry
            .call(
                "fixture",
                "fixture.echo",
                json!({"approved":true,"capabilities":["danger"]}),
                CancellationToken::new(),
                Duration::from_secs(3)
            )
            .await
            .unwrap()["approved"],
        true
    );
    assert!(matches!(
        registry
            .call(
                "fixture",
                "danger",
                json!({}),
                CancellationToken::new(),
                Duration::from_secs(1)
            )
            .await,
        Err(PluginError::Denied(_))
    ));
    let denied = PluginRegistry::new();
    denied
        .load_external(
            config("echo"),
            PluginContext::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(matches!(
        denied
            .call(
                "fixture",
                "fixture.echo",
                json!({}),
                CancellationToken::new(),
                Duration::from_secs(1)
            )
            .await,
        Err(PluginError::Denied(_))
    ));
}

#[tokio::test]
async fn handshake_mismatch_and_manifest_escalation_are_rejected() {
    for mode in ["version", "capabilities", "handshake_large"] {
        let registry = PluginRegistry::new();
        assert!(
            matches!(
                registry
                    .load_external(config(mode), context(), CancellationToken::new())
                    .await,
                Err(PluginError::Protocol(_))
            ),
            "{mode}"
        );
        assert!(registry.list().is_empty());
    }
}

#[tokio::test]
async fn handshake_timeout_and_cancel_do_not_load_plugin() {
    let registry = PluginRegistry::new();
    let mut configuration = config("handshake_hang");
    configuration.handshake_timeout = Duration::from_millis(200);
    assert!(matches!(
        registry
            .load_external(configuration, context(), CancellationToken::new())
            .await,
        Err(PluginError::Timeout)
    ));
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(matches!(
        registry
            .load_external(config("echo"), context(), cancel)
            .await,
        Err(PluginError::Cancelled)
    ));
    assert!(registry.list().is_empty());
}

#[tokio::test]
async fn wrong_request_id_crash_oversize_and_forged_envelope_are_terminal() {
    for mode in ["wrong_id", "crash", "oversize", "approval", "unknown"] {
        let registry = load(mode).await;
        let error = registry
            .call(
                "fixture",
                "fixture.echo",
                json!({}),
                CancellationToken::new(),
                Duration::from_secs(3),
            )
            .await
            .unwrap_err();
        assert!(error.outcome_unknown(), "{mode}: {error}");
        let again = registry
            .call(
                "fixture",
                "fixture.echo",
                json!({}),
                CancellationToken::new(),
                Duration::from_secs(3),
            )
            .await;
        assert!(
            matches!(again, Err(PluginError::Unavailable(_))),
            "{mode}: {again:?}"
        );
    }
}

#[tokio::test]
async fn external_known_failure_is_retained_and_process_can_continue() {
    let registry = load("failure").await;
    for _ in 0..2 {
        let error = registry
            .call(
                "fixture",
                "fixture.echo",
                json!({}),
                CancellationToken::new(),
                Duration::from_secs(3),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, PluginError::Failed(_)));
        assert!(!error.outcome_unknown());
    }
}

#[tokio::test]
async fn stderr_and_lifetime_stdout_budgets_are_enforced() {
    let registry = PluginRegistry::new();
    let mut configuration = config("stderr");
    configuration.max_line_bytes = 4096;
    configuration.max_output_bytes = 8192;
    registry
        .load_external(configuration, context(), CancellationToken::new())
        .await
        .unwrap();
    let error = registry
        .call(
            "fixture",
            "fixture.echo",
            json!({}),
            CancellationToken::new(),
            Duration::from_secs(3),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, PluginError::Protocol(_)));
    let registry = PluginRegistry::new();
    let mut configuration = config("echo");
    configuration.max_line_bytes = 1024;
    configuration.max_output_bytes = 2048;
    registry
        .load_external(configuration, context(), CancellationToken::new())
        .await
        .unwrap();
    let mut exceeded = false;
    for _ in 0..10 {
        let result = registry
            .call(
                "fixture",
                "fixture.echo",
                json!({"value":"x".repeat(500)}),
                CancellationToken::new(),
                Duration::from_secs(3),
            )
            .await;
        if result.is_err() {
            exceeded = true;
            break;
        }
    }
    assert!(exceeded);
}

#[tokio::test]
async fn own_process_timeout_is_terminal() {
    let registry = PluginRegistry::new();
    let mut configuration = config("hang");
    configuration.call_timeout = Duration::from_millis(200);
    registry
        .load_external(configuration, context(), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        registry
            .call(
                "fixture",
                "fixture.echo",
                json!({}),
                CancellationToken::new(),
                Duration::from_secs(3)
            )
            .await,
        Err(PluginError::Timeout)
    );
    assert!(matches!(
        registry
            .call(
                "fixture",
                "fixture.echo",
                json!({}),
                CancellationToken::new(),
                Duration::from_secs(1)
            )
            .await,
        Err(PluginError::Unavailable(_))
    ));
}

static MARKER_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
fn marker() -> PathBuf {
    std::env::temp_dir().join(format!(
        "cayw-plugins-{}-{}",
        std::process::id(),
        MARKER_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ))
}
async fn wait_started(path: &std::path::Path) -> u32 {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(value) = tokio::fs::read_to_string(path).await
                && let Ok(id) = value.parse()
            {
                break id;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn cancellation_unload_and_drop_kill_actual_running_process() {
    for action in ["cancel", "unload", "drop", "registry_timeout"] {
        let registry = Arc::new(load("hang").await);
        let started = marker();
        let cancel = CancellationToken::new();
        let task = {
            let registry = registry.clone();
            let started = started.clone();
            let cancel = cancel.clone();
            tokio::spawn(async move {
                registry
                    .call(
                        "fixture",
                        "fixture.echo",
                        json!({"started":started}),
                        cancel,
                        if action == "registry_timeout" {
                            Duration::from_millis(500)
                        } else {
                            Duration::from_secs(20)
                        },
                    )
                    .await
            })
        };
        let pid = wait_started(&started).await;
        match action {
            "cancel" => {
                cancel.cancel();
                assert_eq!(task.await.unwrap(), Err(PluginError::Cancelled));
            }
            "unload" => {
                registry.unload("fixture").unwrap();
                assert_eq!(task.await.unwrap(), Err(PluginError::Unloaded));
            }
            "drop" => {
                task.abort();
                let _ = task.await;
            }
            _ => assert_eq!(task.await.unwrap(), Err(PluginError::Timeout)),
        }
        #[cfg(unix)]
        wait_dead(pid).await;
        #[cfg(not(unix))]
        let _ = pid;
        let _ = tokio::fs::remove_file(started).await;
    }
}

#[cfg(unix)]
async fn wait_dead(pid: u32) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if matches!(
                nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None),
                Err(nix::errno::Errno::ESRCH)
            ) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("process must terminate and be reaped");
}

#[cfg(unix)]
#[tokio::test]
async fn unix_unload_kills_process_group_including_descendant() {
    let registry = load("tree").await;
    let data = registry
        .call(
            "fixture",
            "fixture.echo",
            json!({}),
            CancellationToken::new(),
            Duration::from_secs(3),
        )
        .await
        .unwrap();
    registry.unload("fixture").unwrap();
    wait_dead(data["parent"].as_u64().unwrap() as u32).await;
    wait_dead(data["descendant"].as_u64().unwrap() as u32).await;
}
