//! F13 / SLICE P: persistent background commands; Session ownership across reopen.
use cayw_ai::{AssistantEvent, AssistantStream, CancellationToken, Model, ModelRequest, Provider};
use cayw_core::CoreEvent;
use cayw_host::{Profile, ProfileKind};
use cayw_runtime::{
    BackgroundConfig, BackgroundStatus, Runtime, RuntimeConfig, ScopedPermission,
    fixture::script_response,
};
use serde_json::json;
use std::{collections::BTreeMap, sync::Arc, time::Duration};

struct Script(String);
impl Provider for Script {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        Box::pin(futures::stream::iter([AssistantEvent::Done {
            message: script_response(&request, &self.0, &format!("bg-{}", request.messages.len())),
        }]))
    }
}

fn base_config() -> RuntimeConfig {
    let mut config = RuntimeConfig::default();
    config.codemode.timeout = Duration::from_secs(20);
    config.max_run_time = Duration::from_secs(30);
    config.background = Some(BackgroundConfig::default());
    config.host.profiles = BTreeMap::from([(
        "bg-fixture".into(),
        Profile {
            kind: ProfileKind::Check,
            program: "/bin/echo".into(),
            args: vec!["bg-hello-f13".into()],
            env: BTreeMap::new(),
            timeout: Duration::from_secs(5),
            max_output_bytes: 4096,
            allow_filter: false,
            io: cayw_host::ProcessIo::Pipes,
        },
    )]);
    config.permissions = vec![ScopedPermission::exact(
        "bg.run",
        json!({"profile":"bg-fixture","name":"quick"}),
    )];
    config
}

#[cfg(unix)]
#[tokio::test]
async fn background_run_notifies_and_survives_reopen_on_same_session() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("workspace")).unwrap();
    let db = root.join("sessions.sqlite");
    let cfg = base_config();
    let rt = Runtime::open(
        &db,
        root.join("workspace"),
        Arc::new(Script(
            "return cayw.call('bg.run', {profile='bg-fixture', name='quick'})".into(),
        )),
        Model::new("fixture", "background"),
        cfg.clone(),
    )
    .unwrap();
    let session = rt.create(None).unwrap().id;
    rt.prompt(&session, "start", CancellationToken::new())
        .await
        .unwrap();
    let started = rt
        .background_list_session(&session)
        .unwrap()
        .into_iter()
        .next()
        .expect("background task listed for session");
    assert_eq!(started.session_id, session);
    assert_eq!(started.name, "quick");
    assert_eq!(started.profile, "bg-fixture");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let view = loop {
        let view = rt.background_inspect(&started.id).unwrap().unwrap();
        if view.status != BackgroundStatus::Running {
            break view;
        }
        if tokio::time::Instant::now() > deadline {
            panic!("background task did not finish: {view:?}");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(view.status, BackgroundStatus::Completed);
    assert_eq!(view.exit_code, Some(0));
    assert_eq!(view.session_id, session);
    let logs = rt.background_logs(&started.id, 0, 4096).unwrap();
    assert!(
        logs.text.contains("bg-hello-f13"),
        "log page missing command output: {:?}",
        logs.text
    );

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let notified = loop {
        let hit = rt
            .events(&session, 0, 1000)
            .unwrap()
            .into_iter()
            .any(|record| match &record.event {
                CoreEvent::Host { kind, data } if kind == "background_task_completed" => {
                    let decision = data.get("decision").unwrap_or(data);
                    decision.get("background_task_id").and_then(|v| v.as_str())
                        == Some(started.id.as_str())
                        && decision.get("session_id").and_then(|v| v.as_str())
                            == Some(session.as_str())
                }
                _ => false,
            });
        if hit {
            break true;
        }
        if tokio::time::Instant::now() > deadline {
            break false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert!(notified, "missing background_task_completed host event");

    let _task = rt
        .tasks(&session)
        .unwrap()
        .into_iter()
        .next()
        .expect("session task");

    drop(rt);
    let rt = Runtime::open(
        &db,
        root.join("workspace"),
        Arc::new(Script("return 1".into())),
        Model::new("fixture", "background"),
        cfg,
    )
    .unwrap();
    let restored = rt.background_inspect(&started.id).unwrap().unwrap();
    assert_eq!(restored.session_id, session);
    assert_eq!(restored.status, BackgroundStatus::Completed);
    assert_eq!(restored.exit_code, Some(0));
    let listed = rt.background_list_session(&session).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, started.id);
    let logs = rt.background_logs(&started.id, 0, 4096).unwrap();
    assert!(logs.text.contains("bg-hello-f13"));
}
