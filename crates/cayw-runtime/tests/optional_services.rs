//! Optional services degrade on their own and never stop Runtime startup.

use cayw_ai::{AssistantEvent, AssistantStream, CancellationToken, Model, ModelRequest, Provider};
use cayw_core::{CapabilityOutcome, CoreEvent};
use cayw_host::HostConfig;
use cayw_requests::OperationScope;
use cayw_runtime::{
    BackgroundConfig, RequestPolicy, Runtime, RuntimeConfig, fixture::OfflineProvider,
};
use serde_json::json;
use std::{path::Path, sync::Arc};

struct ScriptProvider(&'static str);

impl Provider for ScriptProvider {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let message = cayw_runtime::fixture::script_response(&request, self.0, "optional-request");
        Box::pin(futures::stream::iter([AssistantEvent::Done { message }]))
    }
}

/// Run one script and return the open Runtime plus the settled error for
/// `capability.request`.
async fn request_error(
    temp: &tempfile::TempDir,
    config: RuntimeConfig,
    script: &'static str,
) -> (Runtime, String) {
    let runtime = Runtime::open(
        temp.path().join("sessions.sqlite"),
        temp.path().join("workspace"),
        Arc::new(ScriptProvider(script)),
        Model::new("fixture", "optional-services"),
        config,
    )
    .expect("optional service problems must not stop Runtime open");
    assert!(
        names(&runtime)
            .iter()
            .any(|name| name == "capability.request"),
        "request tools must stay available"
    );
    let session = runtime.create(None).unwrap().id;
    runtime
        .prompt(
            &session,
            "use the disabled operation",
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let error = runtime
        .events(&session, 0, 1000)
        .unwrap()
        .into_iter()
        .find_map(|event| match event.event {
            CoreEvent::CapabilitySettled { call } if call.name == "capability.request" => {
                match call.outcome {
                    Some(CapabilityOutcome::Failed { error }) => Some(error),
                    other => panic!("unexpected capability outcome: {other:?}"),
                }
            }
            _ => None,
        })
        .expect("capability.request must settle");
    (runtime, error)
}

fn root() -> tempfile::TempDir {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("workspace")).unwrap();
    temp
}

fn open(root: &Path, config: RuntimeConfig) -> cayw_runtime::Runtime {
    Runtime::open(
        root.join("sessions.sqlite"),
        root.join("workspace"),
        Arc::new(OfflineProvider::default()),
        Model::new("fixture", "optional-services"),
        config,
    )
    .expect("optional service problems must not stop Runtime open")
}

fn names(runtime: &Runtime) -> Vec<String> {
    runtime
        .capabilities()
        .unwrap()
        .into_iter()
        .map(|definition| definition.name)
        .collect()
}

#[tokio::test]
async fn invalid_web_and_vision_config_disable_only_those_services() {
    let temp = root();
    let config = RuntimeConfig {
        host: HostConfig {
            web_access: Some(cayw_web_access::WebAccessOptions {
                limits: cayw_web_access::Limits {
                    max_search_limit: 0,
                    ..Default::default()
                },
                search: cayw_web_access::SearchBackend::Fixture,
            }),
            vision: Some(cayw_vision::Limits {
                input_bytes: 0,
                ..Default::default()
            }),
            ..Default::default()
        },
        ..Default::default()
    };
    let runtime = open(temp.path(), config);
    let disabled = runtime.disabled_services();
    for service in ["web_access", "vision"] {
        assert!(
            disabled
                .iter()
                .any(|entry| entry.name == service && !entry.reason.is_empty()),
            "missing disabled reason for {service}: {disabled:?}"
        );
    }
    let names = names(&runtime);
    assert!(!names.iter().any(|name| name.starts_with("web.")));
    assert!(!names.iter().any(|name| name.starts_with("vision.")));
    assert!(names.iter().any(|name| name == "fs.read"));
}

#[tokio::test]
async fn background_open_failure_disables_feature_only() {
    let temp = root();
    let config = RuntimeConfig {
        background: Some(BackgroundConfig {
            max_tasks: 0,
            ..Default::default()
        }),
        ..Default::default()
    };
    let runtime = open(temp.path(), config);
    let disabled = runtime.disabled_services();
    assert!(
        disabled.iter().any(|entry| entry.name == "background"),
        "background must be reported: {disabled:?}"
    );
    assert!(!names(&runtime).iter().any(|name| name.starts_with("bg.")));
    // Ordinary sessions and file capabilities still work.
    runtime.create(Some("ordinary".into())).unwrap();
    assert!(names(&runtime).iter().any(|name| name == "fs.read"));
}

#[tokio::test]
async fn health_includes_optional_service_status() {
    let temp = root();
    let config = RuntimeConfig {
        host: HostConfig {
            web_access: Some(cayw_web_access::WebAccessOptions {
                limits: cayw_web_access::Limits {
                    max_search_limit: 0,
                    ..Default::default()
                },
                search: cayw_web_access::SearchBackend::Fixture,
            }),
            ..Default::default()
        },
        ..Default::default()
    };
    let runtime = open(temp.path(), config);
    let health = serde_json::to_value(runtime.health().unwrap()).unwrap();
    assert_eq!(health["status"], "ok");
    let disabled = health["disabled_services"].as_array().unwrap();
    assert!(
        disabled
            .iter()
            .any(|entry| entry["name"] == "web_access" && entry["reason"].is_string()),
        "unexpected health: {health}"
    );
}

#[tokio::test]
async fn request_policy_for_disabled_service_does_not_fail_open() {
    let temp = root();
    let config = RuntimeConfig {
        host: HostConfig {
            web_access: Some(cayw_web_access::WebAccessOptions {
                limits: cayw_web_access::Limits {
                    max_search_limit: 0,
                    ..Default::default()
                },
                search: cayw_web_access::SearchBackend::Fixture,
            }),
            ..Default::default()
        },
        request_policy: Some(RequestPolicy {
            permission_policy_id: "optional-service-policy".into(),
            operations: vec![OperationScope::exact(
                "web-search",
                "web.search",
                json!({"query":"cays"}),
                "web-search-binding",
            )],
        }),
        ..Default::default()
    };
    let runtime = open(temp.path(), config);
    assert!(
        runtime
            .disabled_services()
            .iter()
            .any(|entry| entry.name == "web_access")
    );
}

#[tokio::test]
async fn health_reports_no_disabled_services_by_default() {
    let temp = root();
    let runtime = open(temp.path(), RuntimeConfig::default());
    let health = serde_json::to_value(runtime.health().unwrap()).unwrap();
    assert_eq!(health["disabled_services"], json!([]));
}

#[tokio::test]
async fn all_disabled_request_policy_keeps_request_tools_and_reports_reason() {
    let temp = root();
    let config = RuntimeConfig {
        host: HostConfig {
            web_access: Some(cayw_web_access::WebAccessOptions {
                limits: cayw_web_access::Limits {
                    max_search_limit: 0,
                    ..Default::default()
                },
                search: cayw_web_access::SearchBackend::Fixture,
            }),
            ..Default::default()
        },
        request_policy: Some(RequestPolicy {
            permission_policy_id: "optional-service-policy".into(),
            operations: vec![OperationScope::exact(
                "web-search",
                "web.search",
                json!({"query":"cays"}),
                "web-search-binding",
            )],
        }),
        ..Default::default()
    };
    let script = "local input={purpose='search the web',gap='web access is optional',operation_id='web-search',capability='web.search',arguments={query='cays'}}\nreturn cayw.call('capability.request',input)";
    let (runtime, error) = request_error(&temp, config, script).await;
    let reason = runtime
        .disabled_services()
        .iter()
        .find(|service| service.name == "web_access")
        .expect("web_access must be reported")
        .reason
        .clone();
    assert_eq!(error, format!("service disabled: {reason}"));
}

#[tokio::test]
async fn external_request_policy_without_installed_sources_does_not_fail_open() {
    let temp = root();
    let config = RuntimeConfig {
        request_policy: Some(RequestPolicy {
            permission_policy_id: "external-service-policy".into(),
            operations: vec![OperationScope::exact(
                "mcp-discover",
                "mcp.discover",
                json!({}),
                "mcp-discover-binding",
            )],
        }),
        ..Default::default()
    };
    let script = "local input={purpose='discover MCP tools',gap='MCP is optional',operation_id='mcp-discover',capability='mcp.discover',arguments={}}\nreturn cayw.call('capability.request',input)";
    let (_runtime, error) = request_error(&temp, config, script).await;
    assert_eq!(
        error,
        "service disabled: MCP and Pi sources are not installed"
    );
}
