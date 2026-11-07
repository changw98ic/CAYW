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

#[cfg(target_os = "macos")]
async fn running_process_tree(db: &std::path::Path, id: &str) -> Vec<cayw_host::ProcessIdentity> {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let connection = rusqlite::Connection::open(db).unwrap();
            let mut statement = connection
                .prepare(
                    "SELECT pid,parent_pid,process_group,started_seconds,started_microseconds
                 FROM runtime_background_processes WHERE task_id=?1 AND state!='stopped'",
                )
                .unwrap();
            let identities = statement
                .query_map([id], |row| {
                    Ok(cayw_host::ProcessIdentity {
                        pid: row.get::<_, u32>(0)?,
                        parent_pid: row.get::<_, u32>(1)?,
                        process_group: row.get::<_, u32>(2)?,
                        started_seconds: u64::try_from(row.get::<_, i64>(3)?)
                            .expect("valid original start time"),
                        started_microseconds: row.get::<_, u32>(4)?,
                    })
                })
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            if identities.len() >= 2
                && identities.iter().any(|child| {
                    identities
                        .iter()
                        .any(|parent| child.parent_pid == parent.pid)
                })
            {
                for identity in &identities {
                    assert!(
                        identity
                            .is_running()
                            .expect("observe original process incarnation")
                    );
                }
                return identities;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("Device must observe both original shell and real child")
}

#[cfg(target_os = "macos")]
fn assert_stopped_tree(db: &std::path::Path, id: &str, identities: &[cayw_host::ProcessIdentity]) {
    let connection = rusqlite::Connection::open(db).unwrap();
    for identity in identities {
        assert!(
            !identity
                .is_running()
                .expect("observe original process after cleanup"),
            "original incarnation remains alive: {identity:?}"
        );
        let stopped: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM runtime_background_processes WHERE task_id=?1
             AND pid=?2 AND started_seconds=?3 AND started_microseconds=?4 AND state='stopped')",
                rusqlite::params![
                    id,
                    identity.pid,
                    i64::try_from(identity.started_seconds).expect("start time fits journal"),
                    identity.started_microseconds
                ],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            stopped,
            "Device must durably confirm this original incarnation stopped"
        );
    }
    let state: String = connection
        .query_row(
            "SELECT cleanup_state FROM runtime_background_executors WHERE task_id=?1",
            [id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(state, "device_confirmed");
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
    assert_eq!(
        view.status,
        BackgroundStatus::Completed,
        "view was: {:?}",
        view
    );
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

#[cfg(unix)]
#[tokio::test]
async fn parent_cleanup_does_not_reconcile_an_orphaned_background_task() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("workspace")).unwrap();
    let db = root.join("sessions.sqlite");
    let cfg = base_config();
    let rt = Runtime::open(
        &db,
        root.join("workspace"),
        Arc::new(Script("return 1".into())),
        Model::new("fixture", "background"),
        cfg.clone(),
    )
    .unwrap();
    let session = rt.create(None).unwrap().id;
    drop(rt);

    // Parent Host cleanup cannot confirm a separately owned background process.
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute(
            "INSERT INTO runtime_background_tasks(
                id,name,profile,session_id,parent_task_id,status,exit_code,log_bytes,log_path,error,created_ms,finished_ms,pid)
             VALUES('orphan-unconfirmed','unconfirmed','bg-fixture',?1,NULL,'running',NULL,0,'/tmp/unconfirmed.log',NULL,1000,NULL,NULL)",
            [&session],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO runtime_process_scopes(task, session, attempt, worker, fence, state)
             VALUES('scope-task-1', 'sess', 'att', 'w', 1, 'drained');",
            [],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO runtime_background_tasks(
                id,name,profile,session_id,parent_task_id,status,exit_code,log_bytes,log_path,error,created_ms,finished_ms,pid)
             VALUES('orphan-confirmed','confirmed','bg-fixture',?1,'scope-task-1','running',NULL,0,'/tmp/confirmed.log',NULL,1000,NULL,NULL)",
            [&session],
        )
        .unwrap();
    }

    // Reopen runtime: mark_orphans will execute
    let rt = Runtime::open(
        &db,
        root.join("workspace"),
        Arc::new(Script("return 1".into())),
        Model::new("fixture", "background"),
        cfg,
    )
    .unwrap();

    let unconfirmed = rt
        .background_inspect("orphan-unconfirmed")
        .unwrap()
        .unwrap();
    assert_eq!(
        unconfirmed.status,
        BackgroundStatus::OutcomeUnknown,
        "interrupted task without confirmed cleanup must remain OutcomeUnknown"
    );
    assert!(
        matches!(
            unconfirmed.error.as_deref(),
            Some("unreconciled_runtime_restart" | "unconfirmed_runtime_restart")
        ),
        "error was: {:?}",
        unconfirmed.error
    );

    let confirmed = rt.background_inspect("orphan-confirmed").unwrap().unwrap();
    assert_eq!(
        confirmed.status,
        BackgroundStatus::OutcomeUnknown,
        "a drained parent must not settle its independently running background task"
    );
    assert_eq!(
        confirmed.error.as_deref(),
        Some("unreconciled_runtime_restart")
    );
    assert!(
        rt.background_recover(
            "orphan-confirmed",
            BackgroundStatus::Failed,
            Some("parent drained")
        )
        .is_err()
    );
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn descendant_cancellation_terminates_process_group_and_reaps() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("workspace")).unwrap();
    let db = root.join("sessions.sqlite");
    let mut cfg = base_config();
    cfg.host.profiles.insert(
        "bg-sleep".into(),
        Profile {
            kind: ProfileKind::Check,
            program: "/bin/sh".into(),
            args: vec!["-c".into(), "/bin/sleep 60 & wait".into()],
            env: BTreeMap::new(),
            timeout: Duration::from_secs(60),
            max_output_bytes: 4096,
            allow_filter: false,
            io: cayw_host::ProcessIo::Pipes,
        },
    );
    cfg.permissions.push(ScopedPermission::exact(
        "bg.run",
        json!({"profile":"bg-sleep","name":"sleeper"}),
    ));
    let rt = Runtime::open(
        &db,
        root.join("workspace"),
        Arc::new(Script(
            "return cayw.call('bg.run', {profile='bg-sleep', name='sleeper'})".into(),
        )),
        Model::new("fixture", "background"),
        cfg,
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
        .expect("sleeper started");
    assert_eq!(started.name, "sleeper");

    let identities = running_process_tree(&db, &started.id).await;

    let killed_view = rt.background_kill(&started.id).unwrap();
    assert!(
        killed_view.status == BackgroundStatus::Running
            || killed_view.status == BackgroundStatus::Killed
    );

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let view = loop {
        let view = rt.background_inspect(&started.id).unwrap().unwrap();
        if view.status != BackgroundStatus::Running {
            break view;
        }
        if tokio::time::Instant::now() > deadline {
            panic!("task was not killed in time: {view:?}");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(view.status, BackgroundStatus::Killed);
    let result = rt.background_result(&started.id).unwrap();
    assert_eq!(result.status, BackgroundStatus::Killed);
    assert_stopped_tree(&db, &started.id, &identities);
}

#[cfg(unix)]
#[tokio::test]
async fn ipc_rpc_routes_expose_background_operations() {
    use cayw_ipc::RpcHandler;

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
        cfg,
    )
    .unwrap();
    let session = rt.create(None).unwrap().id;
    rt.prompt(&session, "start", CancellationToken::new())
        .await
        .unwrap();

    // Old aliases must be rejected!
    let old_list = rt
        .call(
            "background.list",
            json!({"session_id": session}),
            CancellationToken::new(),
        )
        .await;
    assert!(old_list.is_err());
    let old_status = rt
        .call(
            "background.status",
            json!({"id": "foo"}),
            CancellationToken::new(),
        )
        .await;
    assert!(old_status.is_err());

    // Only runtime.background.* routes are allowed
    let list_val = rt
        .call(
            "runtime.background.list",
            json!({"session_id": session}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let arr = list_val.as_array().expect("array of tasks");
    assert_eq!(arr.len(), 1);
    let task_id = arr[0]["id"].as_str().unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let inspect_val = rt
            .call(
                "runtime.background.inspect",
                json!({"id": task_id}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        if inspect_val["status"].as_str() == Some("completed") {
            break;
        }
        if tokio::time::Instant::now() > deadline {
            panic!("task did not complete in time");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let logs_val = rt
        .call(
            "runtime.background.logs",
            json!({"id": task_id, "offset": 0, "limit": 100}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(logs_val["text"].as_str().unwrap().contains("bg-hello-f13"));

    let res_val = rt
        .call(
            "runtime.background.result",
            json!({"id": task_id}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(res_val["status"].as_str(), Some("completed"));
}

#[cfg(unix)]
#[tokio::test]
async fn concurrent_runtime_instances_do_not_falsely_orphan_active_tasks() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("workspace")).unwrap();
    let db = root.join("sessions.sqlite");
    let mut cfg = base_config();
    cfg.host.profiles.insert(
        "bg-sleep".into(),
        Profile {
            kind: ProfileKind::Check,
            program: "/bin/sleep".into(),
            args: vec!["10".into()],
            env: BTreeMap::new(),
            timeout: Duration::from_secs(20),
            max_output_bytes: 4096,
            allow_filter: false,
            io: cayw_host::ProcessIo::Pipes,
        },
    );
    cfg.permissions.push(ScopedPermission::exact(
        "bg.run",
        json!({"profile":"bg-sleep","name":"long-running"}),
    ));

    let rt1 = Runtime::open(
        &db,
        root.join("workspace"),
        Arc::new(Script(
            "return cayw.call('bg.run', {profile='bg-sleep', name='long-running'})".into(),
        )),
        Model::new("fixture", "background"),
        cfg.clone(),
    )
    .unwrap();
    let session = rt1.create(None).unwrap().id;
    rt1.prompt(&session, "start", CancellationToken::new())
        .await
        .unwrap();

    let started = rt1
        .background_list_session(&session)
        .unwrap()
        .into_iter()
        .next()
        .expect("task started");
    assert_eq!(started.status, BackgroundStatus::Running);

    // Open second runtime instance concurrently on the same store_path!
    let rt2 = Runtime::open(
        &db,
        root.join("workspace"),
        Arc::new(Script("return 1".into())),
        Model::new("fixture", "background"),
        cfg.clone(),
    )
    .unwrap();

    // rt2 must NOT mark rt1's active task as orphan/failed/outcome_unknown!
    let inspect_from_rt2 = rt2.background_inspect(&started.id).unwrap().unwrap();
    assert_eq!(
        inspect_from_rt2.status,
        BackgroundStatus::Running,
        "peer runtime must not orphan active background task"
    );

    // Kill it cleanly across runtime instances (rt2 kills rt1's task)
    rt2.background_kill(&started.id).unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let view = rt1.background_inspect(&started.id).unwrap().unwrap();
        if view.status != BackgroundStatus::Running {
            assert_eq!(view.status, BackgroundStatus::Killed);
            break;
        }
        if tokio::time::Instant::now() > deadline {
            panic!("task was not killed in time");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn descendant_process_identities_terminate_and_reap() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("workspace")).unwrap();
    let db = root.join("sessions.sqlite");
    let mut cfg = base_config();
    cfg.host.profiles.insert(
        "bg-spawn".into(),
        Profile {
            kind: ProfileKind::Check,
            program: "/bin/sh".into(),
            args: vec!["-c".into(), "/bin/sleep 30 & wait".into()],
            env: BTreeMap::new(),
            timeout: Duration::from_secs(60),
            max_output_bytes: 4096,
            allow_filter: false,
            io: cayw_host::ProcessIo::Pipes,
        },
    );
    cfg.permissions.push(ScopedPermission::exact(
        "bg.run",
        json!({"profile":"bg-spawn","name":"spawner"}),
    ));

    let rt = Runtime::open(
        &db,
        root.join("workspace"),
        Arc::new(Script(
            "return cayw.call('bg.run', {profile='bg-spawn', name='spawner'})".into(),
        )),
        Model::new("fixture", "background"),
        cfg,
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
        .expect("spawner started");

    let identities = running_process_tree(&db, &started.id).await;

    // Kill background task
    rt.background_kill(&started.id).unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let view = rt.background_inspect(&started.id).unwrap().unwrap();
        if view.status != BackgroundStatus::Running {
            assert_eq!(view.status, BackgroundStatus::Killed);
            break;
        }
        if tokio::time::Instant::now() > deadline {
            panic!("task was not killed in time");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    assert_stopped_tree(&db, &started.id, &identities);
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn host_reconciles_lost_original_executor_and_process_before_recovering() {
    struct OwnedProcess(std::process::Child);
    impl Drop for OwnedProcess {
        fn drop(&mut self) {
            let _ = self.0.kill();
            self.0.wait().expect("reap test-owned original process");
        }
    }
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("workspace")).unwrap();
    let db = root.join("sessions.sqlite");
    let cfg = base_config();
    let rt = Runtime::open(
        &db,
        root.join("workspace"),
        Arc::new(Script("return 1".into())),
        Model::new("fixture", "background"),
        cfg.clone(),
    )
    .unwrap();
    let session = rt.create(None).unwrap().id;
    // These real OS processes model a separately lost executor and its child.
    // The command outcome is deliberately unknown; no production command replays.
    let mut executor = OwnedProcess(
        std::process::Command::new("/bin/sleep")
            .arg("60")
            .spawn()
            .unwrap(),
    );
    let mut child = OwnedProcess(
        std::process::Command::new("/bin/sleep")
            .arg("60")
            .spawn()
            .unwrap(),
    );
    let supervisor = cayw_host::ProcessIdentity::observe(executor.0.id()).unwrap();
    let process = cayw_host::ProcessIdentity::observe(child.0.id()).unwrap();
    let connection = rusqlite::Connection::open(&db).unwrap();
    connection.execute(
        "INSERT INTO runtime_background_tasks(id,name,profile,session_id,status,log_bytes,log_path,created_ms,
         pid,parent_pid,process_group,started_seconds,started_microseconds)
         VALUES('lost-owner','lost','bg-fixture',?1,'outcome_unknown',0,'/tmp/lost-owner.log',1,?2,?3,?4,?5,?6)",
        rusqlite::params![session, process.pid, process.parent_pid, process.process_group,
            i64::try_from(process.started_seconds).unwrap(), process.started_microseconds],
    ).unwrap();
    connection.execute(
        "INSERT INTO runtime_background_executors(task_id,pid,parent_pid,process_group,started_seconds,started_microseconds,cleanup_state)
         VALUES('lost-owner',?1,?2,?3,?4,?5,'open')",
        rusqlite::params![supervisor.pid, supervisor.parent_pid, supervisor.process_group,
            i64::try_from(supervisor.started_seconds).unwrap(), supervisor.started_microseconds],
    ).unwrap();
    connection.execute(
        "INSERT INTO runtime_background_processes(task_id,pid,parent_pid,process_group,started_seconds,started_microseconds,state)
         VALUES('lost-owner',?1,?2,?3,?4,?5,'started')",
        rusqlite::params![process.pid, process.parent_pid, process.process_group,
            i64::try_from(process.started_seconds).unwrap(), process.started_microseconds],
    ).unwrap();
    drop(connection);
    assert!(
        rt.background_confirm_cleanup("lost-owner", "owner still running", |_| panic!(
            "live executor cannot be confirmed"
        ))
        .is_err()
    );
    executor.0.kill().unwrap();
    executor.0.wait().unwrap();
    assert!(
        rt.background_confirm_cleanup("lost-owner", "child still running", |_| panic!(
            "live child cannot be confirmed"
        ))
        .is_err()
    );
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    assert!(!supervisor.is_running().unwrap());
    assert!(!process.is_running().unwrap());
    assert!(
        rt.background_recover("lost-owner", BackgroundStatus::Failed, Some("PIDs stopped"))
            .is_err()
    );
    rt.background_confirm_cleanup(
        "lost-owner",
        "host reaped both original process owners",
        |inspection| {
            assert_eq!(inspection.task.id, "lost-owner");
            assert_eq!(inspection.task.session_id, session);
            assert_eq!(inspection.supervisor, Some(supervisor));
            assert_eq!(inspection.processes, vec![process]);
            assert!(!inspection.incomplete_identities);
            supervisor
                .is_running()
                .map_err(|error| error.to_string())
                .and_then(|alive| {
                    if alive {
                        Err("original owner is alive".into())
                    } else {
                        Ok(())
                    }
                })?;
            process
                .is_running()
                .map_err(|error| error.to_string())
                .and_then(|alive| {
                    if alive {
                        Err("original process is alive".into())
                    } else {
                        Ok(())
                    }
                })
        },
    )
    .unwrap();
    drop(rt);
    let rt = Runtime::open(
        &db,
        root.join("workspace"),
        Arc::new(Script("return 1".into())),
        Model::new("fixture", "background"),
        cfg,
    )
    .unwrap();
    let recovered = rt
        .background_recover(
            "lost-owner",
            BackgroundStatus::Failed,
            Some("host checked the command failure independently"),
        )
        .unwrap();
    assert_eq!(recovered.status, BackgroundStatus::Failed);
}

#[cfg(unix)]
#[tokio::test]
async fn recovering_unknown_outcome_requires_explicit_evidence_and_stopped_processes() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("workspace")).unwrap();
    let db = root.join("sessions.sqlite");
    let cfg = base_config();
    let rt = Runtime::open(
        &db,
        root.join("workspace"),
        Arc::new(Script("return 1".into())),
        Model::new("fixture", "background"),
        cfg.clone(),
    )
    .unwrap();
    let session = rt.create(None).unwrap().id;

    // Insert an unknown outcome task
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute(
            "INSERT INTO runtime_background_tasks(
                id,name,profile,session_id,parent_task_id,status,exit_code,log_bytes,log_path,error,created_ms,finished_ms,pid)
             VALUES('task-unknown','unknown-task','bg-fixture',?1,NULL,'outcome_unknown',NULL,0,'/tmp/unknown.log',NULL,1000,NULL,NULL)",
            [&session],
        )
        .unwrap();
    }

    // Attempting to recover to Running must fail
    assert!(
        rt.background_recover("task-unknown", BackgroundStatus::Running, Some("evidence"))
            .is_err()
    );

    // Attempting to recover without non-empty evidence must fail
    assert!(
        rt.background_recover("task-unknown", BackgroundStatus::Failed, None)
            .is_err()
    );
    assert!(
        rt.background_recover("task-unknown", BackgroundStatus::Failed, Some("   "))
            .is_err()
    );

    // A reason cannot replace the missing original execution records.
    assert!(
        rt.background_recover(
            "task-unknown",
            BackgroundStatus::Failed,
            Some("verified external cleanup"),
        )
        .is_err()
    );
    assert!(
        rt.background_confirm_cleanup("task-unknown", "external inspection failed", |_| {
            Err("host has not checked the missing records".into())
        })
        .is_err()
    );
    assert!(
        rt.background_recover(
            "task-unknown",
            BackgroundStatus::Failed,
            Some("still unverified"),
        )
        .is_err()
    );

    // This synthetic fixture stands for independent host records for the exact
    // Session and task, including the gap in the original process identities.
    rt.background_confirm_cleanup("task-unknown", "host receipt fixture-1", |inspection| {
        assert_eq!(inspection.task.id, "task-unknown");
        assert_eq!(inspection.task.session_id, session);
        assert!(inspection.incomplete_identities);
        assert!(inspection.supervisor.is_none());
        assert!(inspection.processes.is_empty());
        Ok(())
    })
    .unwrap();
    assert_eq!(
        rt.background_inspect("task-unknown")
            .unwrap()
            .unwrap()
            .status,
        BackgroundStatus::OutcomeUnknown
    );
    drop(rt);
    let rt = Runtime::open(
        &db,
        root.join("workspace"),
        Arc::new(Script("return 1".into())),
        Model::new("fixture", "background"),
        cfg,
    )
    .unwrap();
    let recovered = rt
        .background_recover(
            "task-unknown",
            BackgroundStatus::Failed,
            Some("host reconciled the failed command"),
        )
        .unwrap();
    assert_eq!(recovered.status, BackgroundStatus::Failed);
    assert_eq!(
        recovered.error.as_deref(),
        Some("host reconciled the failed command")
    );
    assert!(
        rt.background_recover("task-unknown", BackgroundStatus::Completed, Some("rewrite"))
            .is_err()
    );
    assert_eq!(
        rt.background_inspect("task-unknown").unwrap().unwrap(),
        recovered
    );
}
