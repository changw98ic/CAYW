#![cfg(target_os = "macos")]
//! Trusted host policy changes after a safe stop. Scheduler capacity/fairness,
//! the declared-resource total and content enablement/quotas are one durable
//! configuration group: they are shared by concurrent connections, but they are
//! not forever immutable once every executor has stopped.
use cayw_ai::{AssistantEvent, AssistantStream, CancellationToken, Model, ModelRequest, Provider};
use cayw_content::Limits;
use cayw_runtime::{
    ContentConfig, ResourcePolicy, Runtime, RuntimeConfig, RuntimeError, ScopedPermission,
    TaskState, fixture::script_response,
};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Default)]
struct Script(Mutex<String>);
impl Script {
    fn set(&self, script: String) {
        *self.0.lock().unwrap() = script;
    }
}
impl Provider for Script {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let script = self.0.lock().unwrap().clone();
        Box::pin(futures::stream::iter([AssistantEvent::Done {
            message: script_response(
                &request,
                &script,
                &format!("config-{}", request.messages.len()),
            ),
        }]))
    }
}

#[derive(Clone, Copy)]
struct Policy {
    concurrency: usize,
    capacity: usize,
    fairness: usize,
    resources: u64,
}
const POLICY: Policy = Policy {
    concurrency: 1,
    capacity: 4,
    fairness: 8,
    resources: 100,
};

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    provider: Arc<Script>,
    rt: Option<Runtime>,
}
impl Fixture {
    fn new(policy: Policy, content: Option<ContentConfig>) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("workspace")).unwrap();
        let provider = Arc::new(Script::default());
        let mut fixture = Self {
            _temp: temp,
            root,
            provider,
            rt: None,
        };
        let rt = fixture.open(policy, content).unwrap();
        fixture.rt = Some(rt);
        fixture
    }
    fn config(&self, policy: Policy, content: Option<ContentConfig>) -> RuntimeConfig {
        RuntimeConfig {
            max_concurrent_runs: policy.concurrency,
            max_queued_runs: policy.capacity,
            fairness_after_dispatches: policy.fairness,
            resources: ResourcePolicy {
                max_declared_memory_bytes: policy.resources,
            },
            content,
            ..RuntimeConfig::default()
        }
    }
    fn open(
        &self,
        policy: Policy,
        content: Option<ContentConfig>,
    ) -> Result<Runtime, RuntimeError> {
        self.open_config(self.config(policy, content))
    }
    fn open_config(&self, config: RuntimeConfig) -> Result<Runtime, RuntimeError> {
        Runtime::open(
            self.root.join("sessions.sqlite"),
            self.root.join("workspace"),
            self.provider.clone(),
            Model::new("fixture", "config-policy"),
            config,
        )
    }
    fn runtime(&self) -> &Runtime {
        self.rt.as_ref().expect("fixture runtime is open")
    }
    fn close(&mut self) {
        self.rt = None;
    }
}

fn content_limits() -> Limits {
    Limits {
        max_content_bytes: 1024,
        max_total_bytes: 4096,
        max_records: 8,
        ..Limits::default()
    }
}
fn content_config(root: &Path, limits: Limits) -> ContentConfig {
    ContentConfig {
        directory: root.join("content"),
        device_id: "policy-device".into(),
        limits,
    }
}
fn binding(root: &Path) -> Value {
    let db = rusqlite::Connection::open(root.join("sessions.sqlite")).unwrap();
    let encoded: String = db
        .query_row(
            "SELECT identity_json FROM runtime_content_config WHERE singleton=1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    serde_json::from_str(&encoded).unwrap()
}
fn stored_limits(root: &Path) -> Value {
    let state: Value =
        serde_json::from_slice(&std::fs::read(root.join("content/state.json")).unwrap()).unwrap();
    state["limits"].clone()
}
fn lua(value: &Value) -> String {
    match value {
        Value::Object(fields) => format!(
            "{{{}}}",
            fields
                .iter()
                .map(|(key, value)| format!(
                    "[{}]={}",
                    serde_json::to_string(key).unwrap(),
                    lua(value)
                ))
                .collect::<Vec<_>>()
                .join(",")
        ),
        Value::Array(_) | Value::Null => panic!("fixture accepts only object scalars"),
        _ => value.to_string(),
    }
}
fn put(name: &str, text: &str) -> Value {
    json!({"name": name, "text": text, "expected_version": 0})
}
fn call(name: &str, args: &Value) -> String {
    format!(
        "return cayw.call({}, {})",
        serde_json::to_string(name).unwrap(),
        lua(args)
    )
}

#[tokio::test]
async fn scheduler_and_resource_policy_changes_apply_after_safe_shutdown() {
    let mut fixture = Fixture::new(POLICY, None);
    fixture.close();
    let changed = Policy {
        concurrency: 2,
        capacity: 4,
        fairness: 6,
        resources: 150,
    };
    let rt = fixture
        .open(changed, None)
        .expect("a stopped database must accept a trusted scheduler/resource change");
    let view = rt.scheduler(10).unwrap();
    assert_eq!(view.max_concurrent_runs, 2);
    assert_eq!(view.max_accepted_tasks, 4);
    assert_eq!(view.fairness_after_dispatches, 6);
    assert_eq!(
        rt.resources().unwrap().policy.max_declared_memory_bytes,
        150
    );
    assert_eq!(rt.health().unwrap().max_concurrent_runs, 2);
    drop(rt);
    // The adopted policy is the one durable policy for every later connection.
    let reopened = fixture.open(changed, None).unwrap();
    assert_eq!(reopened.scheduler(10).unwrap().max_concurrent_runs, 2);
}

#[tokio::test]
async fn concurrent_connection_must_share_the_effective_policy() {
    let mut fixture = Fixture::new(POLICY, None);
    let changed = Policy {
        concurrency: 2,
        ..POLICY
    };
    let error = fixture
        .open(changed, None)
        .err()
        .expect("a live connection must not have its policy overwritten");
    assert!(
        error.to_string().contains("another runtime connection"),
        "{error}"
    );
    // The rejected opener did not change the effective policy.
    assert_eq!(
        fixture.runtime().scheduler(10).unwrap().max_concurrent_runs,
        1
    );
    fixture.close();
    let rt = fixture.open(changed, None).unwrap();
    assert_eq!(rt.scheduler(10).unwrap().max_concurrent_runs, 2);
}

#[tokio::test]
async fn live_task_blocks_policy_change_until_the_database_settles() {
    let mut fixture = Fixture::new(POLICY, None);
    let session = fixture.runtime().create(None).unwrap().id;
    let task = fixture
        .runtime()
        .enqueue(&session, "durable queued work")
        .unwrap();
    assert_eq!(task.state, TaskState::Queued);
    fixture.close();
    let changed = Policy {
        concurrency: 2,
        ..POLICY
    };
    fixture
        .open(changed, None)
        .err()
        .expect("a database with live work must not adopt a different policy");
    // The original policy and the durable queued task are unchanged.
    let rt = fixture.open(POLICY, None).unwrap();
    let restored = rt.task(&task.id).unwrap();
    assert_eq!(restored.state, TaskState::Queued);
    assert_eq!(rt.scheduler(10).unwrap().max_concurrent_runs, 1);
}

#[tokio::test]
async fn content_first_enablement_after_disabled_binding_creates_one_store() {
    let mut fixture = Fixture::new(POLICY, None);
    assert_eq!(binding(&fixture.root), Value::Null);
    fixture.close();
    let limits = content_limits();
    let enabled = content_config(&fixture.root, limits);
    let rt = fixture
        .open(POLICY, Some(enabled.clone()))
        .expect("a stopped, never-enabled binding must accept its first trusted enablement");
    fixture.rt = Some(rt);
    assert!(fixture.root.join("content/state.json").is_file());
    let bound = binding(&fixture.root);
    assert_eq!(
        bound["config"]["directory"],
        json!(enabled.directory.to_str().unwrap())
    );
    assert_eq!(bound["config"]["device_id"], json!("policy-device"));
    let session = fixture.runtime().create(None).unwrap().id;
    assert!(
        fixture
            .runtime()
            .content_inspect(&session, None, "absent")
            .unwrap()
            .is_none()
    );
    fixture.close();
    fixture
        .open(POLICY, Some(enabled))
        .expect("the one canonical enabled binding must reopen");
}

#[tokio::test]
async fn content_quota_increase_preserves_records_and_rejects_unsafe_changes() {
    let limits = content_limits();
    let text = "kept across quota change";
    let put = put("note", text);
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("workspace")).unwrap();
    let provider = Arc::new(Script::default());
    let base = RuntimeConfig {
        content: Some(content_config(&root, limits)),
        permissions: vec![ScopedPermission::exact("content.put", put.clone())],
        ..RuntimeConfig::default()
    };
    let open = |config: RuntimeConfig| {
        Runtime::open(
            root.join("sessions.sqlite"),
            root.join("workspace"),
            provider.clone(),
            Model::new("fixture", "content-policy"),
            config,
        )
    };
    let first = open(base.clone()).unwrap();
    let session = first.create(None).unwrap().id;
    provider.set(call("content.put", &put));
    tokio::time::timeout(
        Duration::from_secs(20),
        first.prompt(
            &session,
            "write canonical content",
            CancellationToken::new(),
        ),
    )
    .await
    .expect("content fixture did not finish")
    .unwrap();
    let saved = first
        .content_inspect(&session, None, "note")
        .unwrap()
        .unwrap();
    assert_eq!(saved.text, text);

    let increased = content_config(
        &root,
        Limits {
            max_records: limits.max_records + 1,
            max_total_bytes: limits.max_total_bytes + 1024,
            ..limits
        },
    );
    let mut increased_cfg = base.clone();
    increased_cfg.content = Some(increased.clone());
    open(increased_cfg.clone())
        .err()
        .expect("a live content store must not have its quota changed underneath it");

    drop(first);
    let reopened = open(increased_cfg.clone())
        .expect("same root/device quota increase must be accepted after a safe stop");
    assert_eq!(
        reopened
            .content_inspect(&session, None, "note")
            .unwrap()
            .unwrap()
            .text,
        text
    );
    assert_eq!(
        stored_limits(&root),
        json!({
            "max_content_bytes": increased.limits.max_content_bytes,
            "max_total_bytes": increased.limits.max_total_bytes,
            "max_records": increased.limits.max_records,
            "max_security_entries": increased.limits.max_security_entries,
        })
    );
    drop(reopened);

    // A limit that would invalidate an existing record is rejected without
    // rewriting the durable quota or the content.
    let mut too_small_cfg = base.clone();
    too_small_cfg.content = Some(content_config(
        &root,
        Limits {
            max_content_bytes: 4,
            ..increased.limits
        },
    ));
    open(too_small_cfg)
        .err()
        .expect("a quota decrease that discards existing bytes must be rejected");
    let survived =
        open(increased_cfg.clone()).expect("the rejected decrease must preserve the prior quota");
    assert_eq!(
        survived
            .content_inspect(&session, None, "note")
            .unwrap()
            .unwrap()
            .text,
        text
    );
    drop(survived);

    // The root identity is still authoritative; a quota change cannot move it.
    let mut moved_cfg = base.clone();
    moved_cfg.content = Some(ContentConfig {
        directory: root.join("other-content"),
        ..increased.clone()
    });
    open(moved_cfg)
        .err()
        .expect("an original content root must not be replaced by a quota change");
}
