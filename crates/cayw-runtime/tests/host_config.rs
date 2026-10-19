use cayw_ai::{
    AssistantEvent, AssistantStream, CancellationToken, Message, Model, ModelRequest, Provider,
};
use cayw_host::{Profile, ProfileKind};
use cayw_runtime::{Runtime, RuntimeConfig, ScopedPermission};
use serde_json::json;
use std::{collections::BTreeMap, sync::Arc, time::Duration};

struct Profiles;
impl Provider for Profiles {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let message = cayw_runtime::fixture::script_response(
            &request,
            "local names = tool_search('', 64); local b = toolDefinition('build.run'); local t = toolDefinition('test.run'); local s = cayw.call('git.status', {}); local build = cayw.call('build.run',{profile='build-fixture'}); local test = cayw.call('test.run',{profile='test-fixture'}); return {build=build,test=test,status=s}",
            &format!("profiles-{}", request.messages.len()),
        );
        Box::pin(futures::stream::iter(vec![AssistantEvent::Done {
            message,
        }]))
    }
}
// These application-selected commands are real child processes. The fixture
// names prove HostConfig reaches the actual catalog and process runner.
#[cfg(unix)]
#[tokio::test]
async fn injected_profiles_and_git_execute_from_luau_with_exact_host_grants() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let mut config = RuntimeConfig::default();
    config.codemode.timeout = Duration::from_secs(20);
    config.host.git_program = Some("/usr/bin/git".into());
    config.host.profiles = BTreeMap::from([
        (
            "build-fixture".into(),
            Profile {
                kind: ProfileKind::Build,
                program: "/usr/bin/true".into(),
                args: vec![],
                env: BTreeMap::new(),
                timeout: Duration::from_secs(5),
                max_output_bytes: 4096,
                allow_filter: false,
            },
        ),
        (
            "test-fixture".into(),
            Profile {
                kind: ProfileKind::Test,
                program: "/usr/bin/true".into(),
                args: vec![],
                env: BTreeMap::new(),
                timeout: Duration::from_secs(5),
                max_output_bytes: 4096,
                allow_filter: false,
            },
        ),
    ]);
    config.permissions = vec![
        ScopedPermission::exact("git.status", json!({})),
        ScopedPermission::exact("build.run", json!({"profile":"build-fixture"})),
        ScopedPermission::exact("test.run", json!({"profile":"test-fixture"})),
    ];
    let status = std::process::Command::new("/usr/bin/git")
        .arg("init")
        .arg("--quiet")
        .arg(&workspace)
        .status()
        .unwrap();
    assert!(status.success());
    let rt = Runtime::open(
        dir.path().join("sessions.db"),
        &workspace,
        Arc::new(Profiles),
        Model::new("fixture", "profiles"),
        config,
    )
    .unwrap();
    let catalog = rt.capabilities().unwrap();
    assert!(catalog.iter().any(|cap| cap.name == "build.run"));
    assert!(catalog.iter().any(|cap| cap.name == "test.run"));
    assert!(catalog.iter().any(|cap| cap.name == "git.status"));
    let id = rt.create(None).unwrap().id;
    let result = rt
        .prompt(&id, "run named host profiles", CancellationToken::new())
        .await
        .unwrap();
    let tool = result
        .messages
        .iter()
        .find_map(|message| match message {
            Message::ToolResult(r) => Some(r),
            _ => None,
        })
        .unwrap();
    assert!(!tool.is_error, "{tool:?}");
    let starts: Vec<_> = rt
        .events(&id, 0, 1000)
        .unwrap()
        .into_iter()
        .filter_map(|record| match record.event {
            cayw_core::CoreEvent::CapabilityStarted { call } => Some(call.name),
            _ => None,
        })
        .collect();
    assert_eq!(starts, vec!["git.status", "build.run", "test.run"]);
}
