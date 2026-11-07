#![cfg(target_os = "macos")]
//! Integration tests for review items BR-02, RT-04, RT-05:
//! - BR-02: Clean up auxiliary records (executors, processes, cleanup_confirmations, cancels)
//!   during retention pruning of known terminal tasks, preserving running and outcome_unknown tasks.
//! - RT-04: Cross-runtime cancellation verifies executor identity, persists cancel intent,
//!   achieves genuine Device termination and cleanup, and reports cancel_requested=false on terminal tasks.
//! - RT-05: Remove unread SQLite newline row index generation in write_logs while preserving log file
//!   fsync and log_bytes atomic updating.

use cayw_ai::{AssistantEvent, AssistantStream, CancellationToken, Model, ModelRequest, Provider};
use cayw_host::{Profile, ProfileKind};
use cayw_runtime::{
    BackgroundConfig, BackgroundStatus, Runtime, RuntimeConfig, ScopedPermission,
    fixture::script_response,
};
use rusqlite::Connection;
use serde_json::json;
use std::{collections::BTreeMap, path::Path, sync::Arc, time::Duration};

struct Script(String);
impl Provider for Script {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let code = if self.0.is_empty() {
            request
                .messages
                .iter()
                .rev()
                .find_map(|m| match m {
                    cayw_ai::Message::User { content, .. } => {
                        content.iter().find_map(|c| match c {
                            cayw_ai::Content::Text { text } => Some(text.clone()),
                            _ => None,
                        })
                    }
                    _ => None,
                })
                .unwrap_or_else(|| "return 1".into())
        } else {
            self.0.clone()
        };
        Box::pin(futures::stream::iter([AssistantEvent::Done {
            message: script_response(
                &request,
                &code,
                &format!("review-bg-{}", request.messages.len()),
            ),
        }]))
    }
}

fn base_config(max_retained_finished: usize) -> RuntimeConfig {
    let mut config = RuntimeConfig::default();
    config.codemode.timeout = Duration::from_secs(20);
    config.max_run_time = Duration::from_secs(30);
    config.background = Some(BackgroundConfig {
        max_retained_finished,
        ..Default::default()
    });
    config.host.profiles = BTreeMap::from([
        (
            "bg-echo".into(),
            Profile {
                kind: ProfileKind::Check,
                program: "/bin/sh".into(),
                args: vec!["-c".into(), "printf 'line1\\nline2\\nline3\\n'".into()],
                env: BTreeMap::new(),
                timeout: Duration::from_secs(5),
                max_output_bytes: 4096,
                allow_filter: false,
                io: cayw_host::ProcessIo::Pipes,
            },
        ),
        (
            "bg-sleep".into(),
            Profile {
                kind: ProfileKind::Check,
                program: "/bin/sh".into(),
                args: vec!["-c".into(), "exec /bin/sleep 30".into()],
                env: BTreeMap::new(),
                timeout: Duration::from_secs(60),
                max_output_bytes: 4096,
                allow_filter: false,
                io: cayw_host::ProcessIo::Pipes,
            },
        ),
    ]);
    config.permissions = vec![
        ScopedPermission::exact("bg.run", json!({"profile":"bg-echo","name":"echoer"})),
        ScopedPermission::exact("bg.run", json!({"profile":"bg-sleep","name":"sleeper"})),
        ScopedPermission::exact("bg.kill", json!({})),
    ];
    config
}

fn open_runtime(db: &Path, workspace: &Path, script: &str, config: RuntimeConfig) -> Runtime {
    Runtime::open(
        db,
        workspace,
        Arc::new(Script(script.into())),
        Model::new("fixture", "review_bg"),
        config,
    )
    .unwrap()
}

// ---------------------------------------------------------------------------
// BR-02: Retention pruning cleans auxiliary records, preserves running & unknown
// ---------------------------------------------------------------------------

#[tokio::test]
async fn br02_prune_finished_cleans_auxiliary_records_and_preserves_running_and_unknown() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let workspace = root.join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let db = root.join("sessions.sqlite");

    // Configure max_retained_finished = 2
    let cfg = base_config(2);
    let rt = open_runtime(
        &db,
        &workspace,
        "return cayw.call('bg.run', {profile='bg-echo', name='echoer'})",
        cfg,
    );
    let session = rt.create(None).unwrap().id;

    let conn = Connection::open(&db).unwrap();

    // Helper to insert a full task with auxiliary records
    let insert_task = |id: &str, status: &str, finished_offset_ms: i64| {
        let log_path = root.join(format!("{id}.log"));
        std::fs::write(&log_path, b"test log").unwrap();

        conn.execute(
            "INSERT INTO runtime_background_tasks(
                id, name, profile, session_id, parent_task_id, status, exit_code,
                log_bytes, log_path, created_ms, finished_ms, pid, parent_pid,
                process_group, started_seconds, started_microseconds
            ) VALUES(?1, 'test', 'bg-echo', ?2, 'parent-1', ?3, 0, 8, ?4, ?5, ?6, 100, 1, 100, 1000, 0)",
            rusqlite::params![
                id,
                session,
                status,
                log_path.to_str().unwrap(),
                1000 + finished_offset_ms,
                if status == "running" { None } else { Some(2000 + finished_offset_ms) }
            ],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO runtime_background_executors(
                task_id, pid, parent_pid, process_group, started_seconds, started_microseconds, cleanup_state
            ) VALUES(?1, 100, 1, 100, 1000, 0, 'device_confirmed')",
            rusqlite::params![id],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO runtime_background_processes(
                task_id, pid, parent_pid, process_group, started_seconds, started_microseconds, state
            ) VALUES(?1, 100, 1, 100, 1000, 0, 'stopped')",
            rusqlite::params![id],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO runtime_background_cleanup_confirmations(
                task_id, identity_snapshot, evidence, confirmed_ms
            ) VALUES(?1, 'snapshot-data', 'evidence-data', 2000)",
            rusqlite::params![id],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO runtime_background_cancels(task_id, requested_ms) VALUES(?1, 1500)",
            rusqlite::params![id],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO runtime_background_log_index(id, offset, line) VALUES(?1, 0, 1)",
            rusqlite::params![id],
        )
        .unwrap();

        log_path
    };

    // Insert 2 completed tasks: task-1 (older) and task-2 (newer)
    let log1 = insert_task("task-1", "completed", 10);
    let log2 = insert_task("task-2", "completed", 20);

    // Insert 1 running task and 1 outcome_unknown task
    let log_running = insert_task("task-running", "running", 15);
    let log_unknown = insert_task("task-unknown", "outcome_unknown", 12);

    // Now execute a real task through rt that completes and calls finish(), triggering pruning
    // Since max_retained_finished = 2, after this task finishes there will be 3 finished tasks,
    // so task-1 (the oldest finished task) must be pruned.
    rt.prompt(&session, "start", CancellationToken::new())
        .await
        .unwrap();

    let started3 = rt
        .background_list_session(&session)
        .unwrap()
        .into_iter()
        .find(|t| t.name == "echoer")
        .expect("echoer task started");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let view = rt.background_inspect(&started3.id).unwrap().unwrap();
        if view.status.is_terminal() {
            assert_eq!(view.status, BackgroundStatus::Completed);
            break;
        }
        if tokio::time::Instant::now() > deadline {
            panic!("task-3 did not complete in time");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Verify task-1 was pruned
    let task1_exists: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM runtime_background_tasks WHERE id='task-1')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(!task1_exists, "oldest completed task must be pruned");

    // Verify task-1 log was removed
    assert!(!log1.exists(), "pruned task log file must be removed");

    // Verify auxiliary rows for task-1 were completely purged (BR-02)
    for (table, col) in [
        ("runtime_background_executors", "task_id"),
        ("runtime_background_processes", "task_id"),
        ("runtime_background_cleanup_confirmations", "task_id"),
        ("runtime_background_cancels", "task_id"),
        ("runtime_background_log_index", "id"),
    ] {
        let count: i64 = conn
            .query_row(
                &format!("SELECT count(*) FROM {table} WHERE {col}='task-1'"),
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            count, 0,
            "auxiliary records in {table} must be cleaned up on prune"
        );
    }

    // Verify auxiliary records for retained tasks are preserved
    for preserved_id in ["task-2", "task-running", "task-unknown"] {
        for (table, col) in [
            ("runtime_background_executors", "task_id"),
            ("runtime_background_processes", "task_id"),
            ("runtime_background_cleanup_confirmations", "task_id"),
            ("runtime_background_cancels", "task_id"),
            ("runtime_background_log_index", "id"),
        ] {
            let count: i64 = conn
                .query_row(
                    &format!("SELECT count(*) FROM {table} WHERE {col}=?1"),
                    [preserved_id],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(
                count, 1,
                "auxiliary records in {table} for preserved task {preserved_id} must be preserved"
            );
        }
    }

    // Verify task-2 and started3 are retained
    for kept_id in ["task-2", &started3.id] {
        let exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM runtime_background_tasks WHERE id=?1)",
                [kept_id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(exists, "{kept_id} must be retained");
    }
    assert!(log2.exists());

    // Verify running and outcome_unknown tasks are NEVER pruned
    let running_exists: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM runtime_background_tasks WHERE id='task-running')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(running_exists, "running task must never be pruned");
    assert!(log_running.exists());

    let unknown_exists: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM runtime_background_tasks WHERE id='task-unknown')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(unknown_exists, "outcome_unknown task must never be pruned");
    assert!(log_unknown.exists());
}

// ---------------------------------------------------------------------------
// RT-04: Cross-runtime cancellation with genuine executor verification and cleanup
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rt04_cross_runtime_kill_active_task_terminates_via_device_and_records_cleanup() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let workspace = root.join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let db = root.join("sessions.sqlite");

    let cfg = base_config(10);
    // rt1 runs the sleep task
    let rt1 = open_runtime(
        &db,
        &workspace,
        "return cayw.call('bg.run', {profile='bg-sleep', name='sleeper'})",
        cfg.clone(),
    );
    let session = rt1.create(None).unwrap().id;
    rt1.prompt(&session, "start", CancellationToken::new())
        .await
        .unwrap();

    let started = rt1
        .background_list_session(&session)
        .unwrap()
        .into_iter()
        .next()
        .expect("sleeper started");
    assert_eq!(started.status, BackgroundStatus::Running);

    // Open rt2 on the same db concurrently
    let rt2 = open_runtime(&db, &workspace, "return 1", cfg);

    // rt2 issues kill on rt1's active task
    let view = rt2.background_kill(&started.id).unwrap();
    assert_eq!(view.id, started.id);

    // Verify persistent cancel intent was written to runtime_background_cancels
    let conn = Connection::open(&db).unwrap();
    let cancel_recorded: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM runtime_background_cancels WHERE task_id=?1)",
            [&started.id],
            |r| r.get(0),
        )
        .unwrap();
    assert!(cancel_recorded, "cancel intent must be persisted");

    // Wait for rt1 to observe cancellation, terminate the child, and confirm cleanup
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let view = rt1.background_inspect(&started.id).unwrap().unwrap();
        if view.status != BackgroundStatus::Running {
            assert_eq!(view.status, BackgroundStatus::Killed);
            break;
        }
        if tokio::time::Instant::now() > deadline {
            panic!("task was not killed in time by cross-runtime cancel");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Verify cleanup_state is confirmed
    let cleanup_state: String = conn
        .query_row(
            "SELECT cleanup_state FROM runtime_background_executors WHERE task_id=?1",
            [&started.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(cleanup_state, "device_confirmed");

    // Second kill on already killed task succeeds without re-killing
    let terminal_view = rt2.background_kill(&started.id).unwrap();
    assert_eq!(terminal_view.status, BackgroundStatus::Killed);
}

#[tokio::test]
async fn rt04_cross_runtime_kill_refuses_when_executor_closed_or_absent() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let workspace = root.join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let db = root.join("sessions.sqlite");

    let cfg = base_config(10);
    let rt = open_runtime(&db, &workspace, "return 1", cfg.clone());
    let session = rt.create(None).unwrap().id;

    let conn = Connection::open(&db).unwrap();

    // 1. Task with no executor record
    let log_no_exec = root.join("no-exec.log");
    std::fs::write(&log_no_exec, b"").unwrap();
    conn.execute(
        "INSERT INTO runtime_background_tasks(
            id, name, profile, session_id, parent_task_id, status, log_bytes, log_path, created_ms
        ) VALUES('no-exec-task', 'test', 'bg-sleep', ?1, 'p1', 'running', 0, ?2, 1000)",
        rusqlite::params![session, log_no_exec.to_str().unwrap()],
    )
    .unwrap();

    let err_no_exec = rt.background_kill("no-exec-task").unwrap_err();
    assert!(
        err_no_exec
            .to_string()
            .contains("no original executor record"),
        "expected no executor record error, got: {err_no_exec}"
    );

    // 2. Task with executor cleanup_state = 'device_confirmed' (already closed)
    let log_closed = root.join("closed.log");
    std::fs::write(&log_closed, b"").unwrap();
    conn.execute(
        "INSERT INTO runtime_background_tasks(
            id, name, profile, session_id, parent_task_id, status, log_bytes, log_path, created_ms
        ) VALUES('closed-task', 'test', 'bg-sleep', ?1, 'p1', 'running', 0, ?2, 1000)",
        rusqlite::params![session, log_closed.to_str().unwrap()],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO runtime_background_executors(
            task_id, pid, parent_pid, process_group, started_seconds, started_microseconds, cleanup_state
        ) VALUES('closed-task', 1, 1, 1, 1, 1, 'device_confirmed')",
        [],
    )
    .unwrap();

    let err_closed = rt.background_kill("closed-task").unwrap_err();
    assert!(
        err_closed.to_string().contains("already closed"),
        "expected already closed error, got: {err_closed}"
    );

    // 3. Task with executor cleanup_state = 'open' but OS process has exited (is_running() == false)
    let log_dead = root.join("dead.log");
    std::fs::write(&log_dead, b"").unwrap();
    conn.execute(
        "INSERT INTO runtime_background_tasks(
            id, name, profile, session_id, parent_task_id, status, log_bytes, log_path, created_ms
        ) VALUES('dead-exec-task', 'test', 'bg-sleep', ?1, 'p1', 'running', 0, ?2, 1000)",
        rusqlite::params![session, log_dead.to_str().unwrap()],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO runtime_background_executors(
            task_id, pid, parent_pid, process_group, started_seconds, started_microseconds, cleanup_state
        ) VALUES('dead-exec-task', 99999, 1, 99999, 1, 1, 'open')",
        [],
    )
    .unwrap();

    let err_dead = rt.background_kill("dead-exec-task").unwrap_err();
    assert!(
        err_dead.to_string().contains("not running"),
        "expected not running error, got: {err_dead}"
    );
}

// ---------------------------------------------------------------------------
// RT-05: write_logs omits SQLite newline index; preserves bytes & reads
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rt05_write_logs_omits_sqlite_line_index_and_preserves_atomic_bytes_and_reads() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let workspace = root.join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let db = root.join("sessions.sqlite");

    let cfg = base_config(10);
    let rt = open_runtime(
        &db,
        &workspace,
        "return cayw.call('bg.run', {profile='bg-echo', name='echoer'})",
        cfg,
    );
    let session = rt.create(None).unwrap().id;
    rt.prompt(&session, "start", CancellationToken::new())
        .await
        .unwrap();

    let started = rt
        .background_list_session(&session)
        .unwrap()
        .into_iter()
        .next()
        .expect("echoer started");

    // Wait for the task to complete
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let view = rt.background_inspect(&started.id).unwrap().unwrap();
        if view.status.is_terminal() {
            assert_eq!(view.status, BackgroundStatus::Completed);
            assert_eq!(view.exit_code, Some(0));
            break;
        }
        if tokio::time::Instant::now() > deadline {
            panic!("task did not complete in time");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Verify runtime_background_log_index has ZERO rows for this task (RT-05)
    let conn = Connection::open(&db).unwrap();
    let index_count: i64 = conn
        .query_row(
            "SELECT count(*) FROM runtime_background_log_index WHERE id=?1",
            [&started.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        index_count, 0,
        "runtime_background_log_index must not be populated by write_logs"
    );

    // Verify log_bytes was atomically updated and matches log file size
    let view = rt.background_inspect(&started.id).unwrap().unwrap();
    assert_eq!(
        view.log_bytes, 18,
        "3 lines of 'line1\\nline2\\nline3\\n' = 18 bytes"
    );

    // Verify reading logs with pagination works without SQLite newline index
    let page1 = rt.background_logs(&started.id, 0, 10).unwrap();
    assert_eq!(page1.offset, 0);
    assert_eq!(page1.bytes_returned, 10);
    assert!(page1.has_more);
    assert_eq!(page1.text, "line1\nline");

    let page2 = rt
        .background_logs(&started.id, page1.next_offset, 20)
        .unwrap();
    assert_eq!(page2.offset, 10);
    assert_eq!(page2.bytes_returned, 8);
    assert!(!page2.has_more);
    assert_eq!(page2.text, "2\nline3\n");
}

#[tokio::test]
async fn rt04_kill_task_capability_reports_cancel_requested_false_for_terminal_tasks() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let workspace = root.join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let db = root.join("sessions.sqlite");

    let terminal_id = "task-completed-test";

    let mut cfg = base_config(10);
    cfg.permissions = vec![ScopedPermission::exact(
        "bg.kill",
        json!({"id": terminal_id}),
    )];

    // Open runtime with empty Script so it takes script from prompt text
    let rt = open_runtime(&db, &workspace, "", cfg);
    let session = rt.create(None).unwrap().id;

    let conn = Connection::open(&db).unwrap();

    // Insert completed task owned by session
    let log_term = root.join("term.log");
    std::fs::write(&log_term, b"").unwrap();
    conn.execute(
        "INSERT INTO runtime_background_tasks(
            id, name, profile, session_id, parent_task_id, status, exit_code, log_bytes, log_path, created_ms, finished_ms
        ) VALUES(?1, 'term', 'bg-echo', ?2, 'p1', 'completed', 0, 0, ?3, 1000, 2000)",
        rusqlite::params![terminal_id, session, log_term.to_str().unwrap()],
    )
    .unwrap();

    // Prompt calls bg.kill on terminal task -> returns cancel_requested == false
    let term_script = format!(
        "local res = cayw.call('bg.kill', {{id='{terminal_id}'}})\n\
         assert(res.cancel_requested == false, 'cancel_requested must be false for terminal task')\n\
         assert(res.status == 'completed', 'status must be completed')\n\
         return 'ok'"
    );
    rt.prompt(&session, &term_script, CancellationToken::new())
        .await
        .unwrap();
}
