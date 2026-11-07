#![cfg(target_os = "macos")]
//! Synthetic provider inputs exercise the real Luau, Host, SDK and authenticated IPC.
use cayw_ai::{
    AssistantEvent, AssistantStream, CancellationToken, Message, Model, ModelRequest, Provider,
};
use cayw_content::{Error as ContentError, Limits};
use cayw_ipc::{Client, ServerConfig, serve};
use cayw_runtime::{
    ApprovalChoice, ContentConfig, ContentScope, HeadlessConfig, Runtime, RuntimeConfig,
    RuntimeError, ScopedPermission, TaskState, fixture::script_response,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Read, Write},
    os::unix::fs::{MetadataExt, PermissionsExt, symlink},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
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
                &format!("boundary-{}", request.messages.len()),
            ),
        }]))
    }
}

fn limits() -> Limits {
    Limits {
        max_content_bytes: 16_384,
        ..Limits::default()
    }
}
fn config(root: &Path, limits: Limits, operations: &[(&str, Value)]) -> RuntimeConfig {
    RuntimeConfig {
        content: Some(ContentConfig {
            directory: root.join("content"),
            device_id: "boundary-device".into(),
            limits,
        }),
        permissions: operations
            .iter()
            .map(|(name, arguments)| ScopedPermission::exact(*name, arguments.clone()))
            .collect(),
        ..RuntimeConfig::default()
    }
}
fn open(root: &Path, provider: Arc<Script>, cfg: RuntimeConfig) -> Result<Runtime, RuntimeError> {
    Runtime::open(
        root.join("sessions.sqlite"),
        root.join("workspace"),
        provider,
        Model::new("fixture", "content-boundaries"),
        cfg,
    )
}
struct Harness {
    _temp: tempfile::TempDir,
    root: PathBuf,
    provider: Arc<Script>,
    rt: Runtime,
    cfg: RuntimeConfig,
}
impl Harness {
    fn new(limits: Limits, operations: &[(&str, Value)]) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("workspace")).unwrap();
        let provider = Arc::new(Script::default());
        let cfg = config(&root, limits, operations);
        let rt = open(&root, provider.clone(), cfg.clone()).unwrap();
        Self {
            _temp: temp,
            root,
            provider,
            rt,
            cfg,
        }
    }
    async fn run(&self, session: &str, script: String) {
        self.provider.set(script);
        tokio::time::timeout(
            Duration::from_secs(20),
            self.rt.prompt(
                session,
                "exercise content boundary",
                CancellationToken::new(),
            ),
        )
        .await
        .expect("content fixture did not finish")
        .unwrap();
    }
    async fn call(&self, session: &str, name: &str, arguments: &Value) {
        self.run(session, call(name, arguments)).await;
    }
    fn authority(&self) -> Vec<u8> {
        std::fs::read(self.root.join("content/state.json")).unwrap()
    }
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
        Value::Array(_) | Value::Null => {
            panic!("boundary fixture accepts only explicit object scalars")
        }
        _ => value.to_string(),
    }
}
fn call(name: &str, args: &Value) -> String {
    format!(
        "return cayw.call({}, {})",
        serde_json::to_string(name).unwrap(),
        lua(args)
    )
}
fn put(name: &str, text: &str) -> Value {
    json!({"name": name, "text": text, "expected_version": 0})
}
fn receipts(rt: &Runtime, session: &str, name: &str) -> Vec<Value> {
    rt.events(session, 0, 1000)
        .unwrap()
        .into_iter()
        .filter_map(|record| {
            let value = serde_json::to_value(record).unwrap();
            let event = &value["event"];
            (event["type"] == "capability_settled" && event["call"]["name"] == name)
                .then(|| event["call"].clone())
        })
        .collect()
}
fn latest_receipt(rt: &Runtime, session: &str, name: &str) -> Value {
    receipts(rt, session, name)
        .pop()
        .expect("expected a paired capability receipt")
}
fn completed_result(rt: &Runtime, session: &str, name: &str) -> Value {
    let receipt = latest_receipt(rt, session, name);
    assert_eq!(receipt["outcome"]["type"], "completed", "{receipt:#}");
    receipt["outcome"]["result"].clone()
}
fn assert_failed(rt: &Runtime, session: &str, name: &str, detail: &str) {
    let receipt = latest_receipt(rt, session, name);
    assert_eq!(receipt["outcome"]["type"], "failed", "{receipt:#}");
    assert!(
        receipt["outcome"]["error"]
            .as_str()
            .unwrap()
            .contains(detail),
        "{receipt:#}"
    );
}
fn assert_cell_failed(rt: &Runtime, session: &str) {
    let snapshot = rt.get(session).unwrap();
    let page = snapshot
        .messages
        .iter()
        .rev()
        .find_map(|message| match message {
            Message::ToolResult(result) => result.structured_content.as_ref(),
            _ => None,
        })
        .expect("real execute_code result");
    assert_eq!(page["status"], "failed", "{page:#}");
    assert!(snapshot.pending_approvals.is_empty());
}
fn started_after(rt: &Runtime, session: &str, after: u64, name: &str) -> bool {
    rt.events(session, after, 1000)
        .unwrap()
        .into_iter()
        .any(|record| {
            let value = serde_json::to_value(record).unwrap();
            value["event"]["type"] == "capability_started" && value["event"]["call"]["name"] == name
        })
}

#[tokio::test]
async fn model_owner_and_path_injection_fail_before_host_start_without_touching_authority() {
    let original = put("notes", "private original");
    let h = Harness::new(limits(), &[("content.put", original.clone())]);
    let session = h.rt.create(None).unwrap().id;
    h.call(&session, "content.put", &original).await;
    let saved =
        h.rt.content_inspect(&session, None, "notes")
            .unwrap()
            .unwrap();
    let authority = h.authority();
    for capability in ["content.read", "content.put"] {
        for (field, injected) in [
            ("session_id", json!("another-session")),
            ("task_id", json!("another-task")),
            ("path", json!(h.root.join("content/state.json"))),
        ] {
            let mut args = if capability == "content.read" {
                json!({"name": "notes"})
            } else {
                put("notes", "injected")
            };
            args[field] = injected;
            let before = h.rt.get(&session).unwrap().cursor;
            h.call(&session, capability, &args).await;
            assert_cell_failed(&h.rt, &session);
            assert!(!started_after(&h.rt, &session, before, capability));
            assert_eq!(h.authority(), authority);
            assert_eq!(
                h.rt.content_inspect(&session, None, "notes")
                    .unwrap()
                    .unwrap(),
                saved
            );
        }
        for name in ["../notes", "objects/notes", "/tmp/notes", "notes\\escape"] {
            let args = if capability == "content.read" {
                json!({"name": name})
            } else {
                put(name, "injected")
            };
            let before = h.rt.get(&session).unwrap().cursor;
            h.call(&session, capability, &args).await;
            assert_cell_failed(&h.rt, &session);
            assert!(!started_after(&h.rt, &session, before, capability));
            assert_eq!(h.authority(), authority);
        }
    }
}

#[tokio::test]
async fn session_and_task_reads_are_isolated_and_physical_record_names_grant_no_access() {
    let session_put = put("shared", "session-owned");
    let task_put =
        json!({"name":"private","scope":"task","text":"task-owned","expected_version":0});
    let read_session = json!({"name":"shared"});
    let read_task = json!({"name":"private","scope":"task"});
    let h = Harness::new(
        limits(),
        &[
            ("content.put", session_put.clone()),
            ("content.put", task_put.clone()),
            ("content.read", read_session.clone()),
            ("content.read", read_task.clone()),
        ],
    );
    let a = h.rt.create(None).unwrap().id;
    let b = h.rt.create(None).unwrap().id;
    h.run(
        &a,
        format!(
            "local a=cayw.call('content.put',{}); return cayw.call('content.put',{})",
            lua(&session_put),
            lua(&task_put)
        ),
    )
    .await;
    let task = h.rt.tasks(&a).unwrap().pop().unwrap();
    let saved =
        h.rt.content_inspect(&a, Some(&task.id), "private")
            .unwrap()
            .unwrap();
    assert_eq!(saved.scope, ContentScope::Task);
    assert_eq!(saved.task_id.as_deref(), Some(task.id.as_str()));
    assert!(h.rt.content_inspect(&a, None, "private").unwrap().is_none());
    assert!(h.rt.content_inspect(&b, Some(&task.id), "private").is_err());
    h.call(&a, "content.read", &read_task).await;
    assert!(completed_result(&h.rt, &a, "content.read").is_null());
    h.call(&a, "content.read", &read_session).await;
    assert_eq!(
        completed_result(&h.rt, &a, "content.read")["text"],
        "session-owned"
    );
    for args in [&read_task, &read_session] {
        h.call(&b, "content.read", args).await;
        assert!(completed_result(&h.rt, &b, "content.read").is_null());
    }
    assert_eq!(
        h.rt.content_inspect(&a, Some(&task.id), "private")
            .unwrap()
            .unwrap(),
        saved
    );
    let physical = saved.record.name.to_string();
    let mut cfg = h.cfg.clone();
    let physical_read = json!({"name":physical});
    cfg.permissions.push(ScopedPermission::exact(
        "content.read",
        physical_read.clone(),
    ));
    drop(h.rt);
    let rt = open(&h.root, h.provider.clone(), cfg).unwrap();
    h.provider.set(call("content.read", &physical_read));
    tokio::time::timeout(
        Duration::from_secs(20),
        rt.prompt(&b, "try a known physical key", CancellationToken::new()),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(completed_result(&rt, &b, "content.read").is_null());
    assert_eq!(
        rt.content_inspect(&a, Some(&task.id), "private")
            .unwrap()
            .unwrap(),
        saved
    );
}

#[tokio::test]
async fn same_version_with_wrong_revision_cannot_overwrite_content() {
    let original = put("notes", "first");
    let wrong = json!({"name":"notes","text":"forged","expected_version":1,"expected_revision":"0".repeat(64)});
    let h = Harness::new(
        limits(),
        &[
            ("content.put", original.clone()),
            ("content.put", wrong.clone()),
        ],
    );
    let s = h.rt.create(None).unwrap().id;
    h.call(&s, "content.put", &original).await;
    let first = h.rt.content_inspect(&s, None, "notes").unwrap().unwrap();
    let authority = h.authority();
    h.call(&s, "content.put", &wrong).await;
    assert_failed(&h.rt, &s, "content.put", "revision conflict");
    assert!(
        latest_receipt(&h.rt, &s, "content.put")["outcome"]["error"]
            .as_str()
            .unwrap()
            .contains(&first.revision_hex)
    );
    assert_eq!(h.authority(), authority);
    assert_eq!(
        h.rt.content_inspect(&s, None, "notes").unwrap().unwrap(),
        first
    );
}

#[tokio::test]
async fn revision_shape_and_utf8_byte_limit_fail_without_committing() {
    let malformed = [
        json!({"name":"notes","text":"bad","expected_version":0,"expected_revision":"0".repeat(64)}),
        json!({"name":"notes","text":"bad","expected_version":1}),
        put("notes", "ééééé"),
    ];
    let grants = malformed
        .iter()
        .cloned()
        .map(|v| ("content.put", v))
        .collect::<Vec<_>>();
    let h = Harness::new(
        Limits {
            max_content_bytes: 8,
            ..limits()
        },
        &grants,
    );
    let s = h.rt.create(None).unwrap().id;
    let authority = h.authority();
    for (index, args) in malformed.iter().enumerate() {
        h.call(&s, "content.put", args).await;
        assert_failed(
            &h.rt,
            &s,
            "content.put",
            if index == 2 {
                "UTF-8 byte budget"
            } else {
                "creation requires"
            },
        );
        assert_eq!(h.authority(), authority);
        assert!(h.rt.content_inspect(&s, None, "notes").unwrap().is_none());
    }
    for args in [
        put("notes", "123456789"),
        json!({"name":"notes","text":"bad","expected_version":-1}),
        json!({"name":"notes","text":"bad","expected_version":1,"expected_revision":"A".repeat(64)}),
    ] {
        let before = h.rt.get(&s).unwrap().cursor;
        h.call(&s, "content.put", &args).await;
        assert_cell_failed(&h.rt, &s);
        assert!(!started_after(&h.rt, &s, before, "content.put"));
        assert_eq!(h.authority(), authority);
    }
}

#[tokio::test]
async fn disk_and_record_quotas_fail_without_changing_existing_records() {
    let a = put("a", "12345678");
    let b = put("b", "12345678");
    let c = put("c", "12345678");
    let h = Harness::new(
        Limits {
            max_content_bytes: 8,
            max_total_bytes: 8,
            max_records: 2,
            ..limits()
        },
        &[
            ("content.put", a.clone()),
            ("content.put", b.clone()),
            ("content.put", c.clone()),
        ],
    );
    let s = h.rt.create(None).unwrap().id;
    h.call(&s, "content.put", &a).await;
    let first = h.rt.content_inspect(&s, None, "a").unwrap().unwrap();
    h.call(&s, "content.put", &b).await;
    let authority = h.authority();
    h.call(&s, "content.put", &c).await;
    assert_failed(&h.rt, &s, "content.put", "quota exceeded");
    assert_eq!(h.authority(), authority);
    assert!(h.rt.content_inspect(&s, None, "c").unwrap().is_none());
    let update = json!({"name":"a","text":"87654321","expected_version":1,"expected_revision":first.revision_hex});
    let mut cfg = h.cfg.clone();
    cfg.permissions
        .push(ScopedPermission::exact("content.put", update.clone()));
    drop(h.rt);
    let rt = open(&h.root, h.provider.clone(), cfg).unwrap();
    h.provider.set(call("content.put", &update));
    tokio::time::timeout(
        Duration::from_secs(20),
        rt.prompt(&s, "exceed retained object quota", CancellationToken::new()),
    )
    .await
    .unwrap()
    .unwrap();
    assert_failed(&rt, &s, "content.put", "quota exceeded");
    assert_eq!(
        std::fs::read(h.root.join("content/state.json")).unwrap(),
        authority
    );
    assert_eq!(rt.content_inspect(&s, None, "a").unwrap().unwrap(), first);
}

#[tokio::test]
async fn damaged_missing_and_symlinked_store_entries_fail_closed_through_sdk_and_host() {
    for scenario in [
        "corrupt_object",
        "missing_object",
        "symlink_object",
        "corrupt_state",
        "missing_state",
        "symlink_state",
        "symlink_objects_directory",
    ] {
        let write = put("notes", "original");
        let read = json!({"name":"notes"});
        let h = Harness::new(
            limits(),
            &[
                ("content.put", write.clone()),
                ("content.read", read.clone()),
            ],
        );
        let s = h.rt.create(None).unwrap().id;
        h.call(&s, "content.put", &write).await;
        let saved = h.rt.content_inspect(&s, None, "notes").unwrap().unwrap();
        let object = h.root.join("content/objects").join(&saved.content_hash_hex);
        let state = h.root.join("content/state.json");
        let external = h.root.join("external");
        std::fs::write(&external, b"outside untouched").unwrap();
        match scenario {
            "corrupt_object" => std::fs::write(&object, b"tampered").unwrap(),
            "missing_object" => std::fs::remove_file(&object).unwrap(),
            "symlink_object" => {
                std::fs::remove_file(&object).unwrap();
                symlink(&external, &object).unwrap();
            }
            "corrupt_state" => std::fs::write(&state, b"{broken authority").unwrap(),
            "missing_state" => std::fs::remove_file(&state).unwrap(),
            "symlink_state" => {
                std::fs::remove_file(&state).unwrap();
                symlink(&external, &state).unwrap();
            }
            "symlink_objects_directory" => {
                std::fs::rename(h.root.join("content/objects"), h.root.join("saved-objects"))
                    .unwrap();
                symlink(h.root.join("saved-objects"), h.root.join("content/objects")).unwrap();
            }
            _ => unreachable!(),
        }
        assert!(
            h.rt.content_inspect(&s, None, "notes").is_err(),
            "SDK accepted {scenario}"
        );
        h.call(&s, "content.read", &read).await;
        let receipt = latest_receipt(&h.rt, &s, "content.read");
        assert_eq!(
            receipt["outcome"]["type"], "failed",
            "{scenario}: {receipt:#}"
        );
        assert!(!receipt["outcome"]["error"].as_str().unwrap().is_empty());
        assert_eq!(std::fs::read(&external).unwrap(), b"outside untouched");
        if scenario == "corrupt_state" {
            assert_eq!(std::fs::read(&state).unwrap(), b"{broken authority");
        }
        if scenario == "missing_state" {
            assert!(!state.exists());
        }
        if let Ok(reopened) = open(&h.root, h.provider.clone(), h.cfg.clone()) {
            assert!(
                matches!(
                    scenario,
                    "corrupt_object" | "missing_object" | "symlink_object"
                ),
                "reopening accepted unsafe authority: {scenario}"
            );
            assert!(
                reopened.content_inspect(&s, None, "notes").is_err(),
                "reopened SDK accepted {scenario}"
            );
        }
    }
}

#[tokio::test]
async fn reopening_same_path_with_a_new_empty_directory_rejects_its_identity() {
    let write = put("notes", "must remain in old directory");
    let h = Harness::new(limits(), &[("content.put", write.clone())]);
    let s = h.rt.create(None).unwrap().id;
    h.call(&s, "content.put", &write).await;
    let saved = h.rt.content_inspect(&s, None, "notes").unwrap().unwrap();
    let moved = h.root.join("moved-content");
    std::fs::rename(h.root.join("content"), &moved).unwrap();
    std::fs::create_dir(h.root.join("content")).unwrap();
    std::fs::set_permissions(
        h.root.join("content"),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    assert_eq!(
        h.rt.content_inspect(&s, None, "notes").unwrap().unwrap(),
        saved
    );
    drop(h.rt);
    let error = open(&h.root, h.provider.clone(), h.cfg.clone())
        .err()
        .expect("replacement identity must fail");
    assert!(error.to_string().contains("identity differs"), "{error}");
    std::fs::remove_dir_all(h.root.join("content")).unwrap();
    std::fs::rename(moved, h.root.join("content")).unwrap();
    let restored = open(&h.root, h.provider.clone(), h.cfg).unwrap();
    assert_eq!(
        restored
            .content_inspect(&s, None, "notes")
            .unwrap()
            .unwrap(),
        saved
    );
}

#[tokio::test]
async fn group_or_other_write_mode_is_refused_without_repairing_or_writing_content() {
    let write = put("notes", "original");
    let read = json!({"name":"notes"});
    let h = Harness::new(
        limits(),
        &[
            ("content.put", write.clone()),
            ("content.read", read.clone()),
        ],
    );
    let s = h.rt.create(None).unwrap().id;
    h.call(&s, "content.put", &write).await;
    let saved = h.rt.content_inspect(&s, None, "notes").unwrap().unwrap();
    let authority = h.authority();
    for mode in [0o720, 0o702] {
        std::fs::set_permissions(
            h.root.join("content"),
            std::fs::Permissions::from_mode(mode),
        )
        .unwrap();
        let error = h.rt.content_inspect(&s, None, "notes").unwrap_err();
        assert!(
            error.to_string().contains("must not be writable"),
            "{error}"
        );
        h.call(&s, "content.read", &read).await;
        assert_failed(&h.rt, &s, "content.read", "must not be writable");
        h.call(&s, "content.put", &write).await;
        assert_failed(&h.rt, &s, "content.put", "must not be writable");
        assert_eq!(h.authority(), authority);
        assert!(open(&h.root, h.provider.clone(), h.cfg.clone()).is_err());
        assert_eq!(
            std::fs::metadata(h.root.join("content"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            mode
        );
    }
    std::fs::set_permissions(
        h.root.join("content"),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    assert_eq!(
        h.rt.content_inspect(&s, None, "notes").unwrap().unwrap(),
        saved
    );
}

#[test]
fn content_and_workspace_directories_cannot_overlap_or_use_symlink_aliases() {
    for scenario in [
        "same",
        "inside_workspace",
        "workspace_inside_content",
        "symlink_alias",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let workspace = root.join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let mut cfg = config(&root, limits(), &[]);
        cfg.content.as_mut().unwrap().directory = match scenario {
            "same" => workspace.clone(),
            "inside_workspace" => workspace.join("content"),
            "workspace_inside_content" => root.clone(),
            "symlink_alias" => {
                let alias = root.join("alias");
                symlink(&workspace, &alias).unwrap();
                alias
            }
            _ => unreachable!(),
        };
        assert!(
            open(&root, Arc::new(Script::default()), cfg).is_err(),
            "accepted {scenario}"
        );
        assert!(
            !workspace.join("state.json").exists(),
            "initialized content in workspace: {scenario}"
        );
    }
}

#[tokio::test]
async fn content_configuration_binding_rejects_silent_directory_device_or_disable_changes_and_allows_a_stopped_quota_change()
 {
    let h = Harness::new(limits(), &[]);
    let mut changed = Vec::new();
    let mut directory = h.cfg.clone();
    directory.content.as_mut().unwrap().directory = h.root.join("other-content");
    changed.push(directory);
    let mut device = h.cfg.clone();
    device.content.as_mut().unwrap().device_id = "different-device".into();
    changed.push(device);
    let mut quotas = h.cfg.clone();
    quotas.content.as_mut().unwrap().limits.max_records += 1;
    let mut disabled = h.cfg.clone();
    disabled.content = None;
    changed.push(disabled);
    for cfg in changed {
        // The original Runtime is still live: these are genuine replacement or
        // disable attempts, not a policy change on a stopped database.
        assert!(open(&h.root, h.provider.clone(), cfg).is_err());
    }
    // A live store must not have its quota changed underneath it.
    assert!(open(&h.root, h.provider.clone(), quotas.clone()).is_err());
    assert!(open(&h.root, h.provider.clone(), h.cfg.clone()).is_ok());

    // After a safe stop, the same root/device quota change is accepted as the
    // one canonical policy; a later opening without live work keeps it.
    let root = h.root.clone();
    let provider = h.provider.clone();
    let original = h.cfg.clone();
    let _temp = h._temp;
    drop(h.rt);
    let rt = open(&root, provider.clone(), quotas.clone())
        .expect("a stopped same-root/device quota increase must be accepted");
    assert!(
        rt.content_inspect(&rt.create(None).unwrap().id, None, "absent-content",)
            .unwrap()
            .is_none()
    );
    drop(rt);
    let binding: Value = serde_json::from_str(&content_binding(&root)).unwrap();
    assert_eq!(
        binding["config"]["limits"]["max_records"],
        json!(limits().max_records + 1)
    );
    assert!(open(&root, provider, original).is_ok());
}

fn directory_snapshot(root: &Path) -> BTreeMap<PathBuf, Value> {
    fn collect(root: &Path, path: &Path, entries: &mut BTreeMap<PathBuf, Value>) {
        let metadata = std::fs::symlink_metadata(path).unwrap();
        let mut entry = json!({
            "device":metadata.dev(),
            "inode":metadata.ino(),
            "mode":metadata.permissions().mode(),
        });
        if metadata.is_dir() {
            entry["kind"] = json!("directory");
        } else if metadata.file_type().is_symlink() {
            entry["kind"] = json!("symlink");
            entry["target"] = json!(std::fs::read_link(path).unwrap());
        } else {
            assert!(metadata.is_file(), "unexpected fixture entry: {path:?}");
            entry["kind"] = json!("file");
            entry["bytes"] = json!(std::fs::read(path).unwrap());
        }
        entries.insert(path.strip_prefix(root).unwrap().to_path_buf(), entry);
        if metadata.is_dir() {
            for child in std::fs::read_dir(path).unwrap() {
                collect(root, &child.unwrap().path(), entries);
            }
        }
    }
    let mut entries = BTreeMap::new();
    collect(root, root, &mut entries);
    entries
}
fn content_binding(root: &Path) -> String {
    let db = rusqlite::Connection::open(root.join("sessions.sqlite")).unwrap();
    db.query_row(
        "SELECT identity_json FROM runtime_content_config WHERE singleton=1",
        [],
        |row| row.get(0),
    )
    .unwrap()
}
fn candidate_with_canary(path: &Path) {
    std::fs::create_dir(path).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(
        path.join(".state-stage"),
        b"candidate must remain unchanged",
    )
    .unwrap();
}

#[tokio::test]
async fn content_open_rejection_preserves_candidate_directory_on_configuration_change() {
    let h = Harness::new(limits(), &[]);
    let binding = content_binding(&h.root);
    let original = directory_snapshot(&h.root.join("content"));
    let candidate = h.root.join("candidate-content");
    candidate_with_canary(&candidate);
    let before = directory_snapshot(&candidate);
    let mut cfg = h.cfg.clone();
    cfg.content.as_mut().unwrap().directory = candidate.clone();
    drop(h.rt);
    let error = open(&h.root, h.provider.clone(), cfg)
        .err()
        .expect("changed content configuration must be rejected");
    let after = directory_snapshot(&candidate);
    println!(
        "CAYW_CONTENT_OPEN_REJECTION {}",
        json!({"scenario":"configuration_change","error":error.to_string(),"before":before,"after":after})
    );
    assert_eq!(content_binding(&h.root), binding);
    assert_eq!(directory_snapshot(&h.root.join("content")), original);
    assert_eq!(
        after, before,
        "rejected configuration changed the candidate directory"
    );
}

#[tokio::test]
async fn content_open_rejection_preserves_replacement_directory_on_inode_change() {
    let h = Harness::new(limits(), &[]);
    let binding = content_binding(&h.root);
    let candidate = h.root.join("content");
    let moved = h.root.join("original-content");
    std::fs::rename(&candidate, &moved).unwrap();
    candidate_with_canary(&candidate);
    let before = directory_snapshot(&candidate);
    let original = directory_snapshot(&moved);
    let old_metadata = std::fs::metadata(&moved).unwrap();
    let new_metadata = std::fs::metadata(&candidate).unwrap();
    assert_ne!(
        (old_metadata.dev(), old_metadata.ino()),
        (new_metadata.dev(), new_metadata.ino())
    );
    drop(h.rt);
    let error = open(&h.root, h.provider.clone(), h.cfg.clone())
        .err()
        .expect("replacement content identity must be rejected");
    let after = directory_snapshot(&candidate);
    println!(
        "CAYW_CONTENT_OPEN_REJECTION {}",
        json!({"scenario":"inode_change","error":error.to_string(),"before":before,"after":after})
    );
    assert_eq!(content_binding(&h.root), binding);
    assert_eq!(directory_snapshot(&moved), original);
    assert_eq!(
        after, before,
        "rejected identity changed the replacement directory"
    );
}

#[tokio::test]
async fn content_first_enablement_after_disabled_binding_creates_one_store() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("workspace")).unwrap();
    let provider = Arc::new(Script::default());
    let mut cfg = config(&root, limits(), &[]);
    cfg.content = None;
    let rt = open(&root, provider.clone(), cfg).unwrap();
    assert_eq!(content_binding(&root), "null");
    drop(rt);
    let parent = root.join("never-created");
    let candidate = parent.join("content");
    assert!(!parent.exists());
    let mut enabled = config(&root, limits(), &[]);
    enabled.content.as_mut().unwrap().directory = candidate.clone();
    let rt = open(&root, provider.clone(), enabled.clone())
        .expect("a stopped database must accept its first trusted content enablement");
    println!(
        "CAYW_CONTENT_FIRST_ENABLEMENT {}",
        json!({"binding_after":content_binding(&root),"candidate_exists":candidate.exists(),"parent_exists":parent.exists()})
    );
    assert!(
        candidate.join("state.json").is_file(),
        "accepted first enablement must create the canonical store"
    );
    let binding: Value = serde_json::from_str(&content_binding(&root)).unwrap();
    assert_eq!(
        binding["config"]["directory"],
        json!(candidate.to_str().unwrap())
    );
    assert_eq!(binding["config"]["device_id"], json!("boundary-device"));
    let session = rt.create(None).unwrap().id;
    assert!(
        rt.content_inspect(&session, None, "absent")
            .unwrap()
            .is_none()
    );
    drop(rt);
    // The one canonical enabled binding reopens unchanged.
    assert!(open(&root, provider, enabled).is_ok());
}

#[tokio::test]
async fn content_open_rejection_preserves_initial_unsafe_directory_and_canary() {
    let mut observations = Vec::new();
    for mode in [0o720, 0o702] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("workspace")).unwrap();
        let candidate = root.join("content");
        candidate_with_canary(&candidate);
        std::fs::set_permissions(&candidate, std::fs::Permissions::from_mode(mode)).unwrap();
        let before = directory_snapshot(&candidate);
        let error = open(
            &root,
            Arc::new(Script::default()),
            config(&root, limits(), &[]),
        )
        .err()
        .expect("initial group/other-writable content directory must be rejected");
        let after = directory_snapshot(&candidate);
        println!(
            "CAYW_CONTENT_OPEN_REJECTION {}",
            json!({"scenario":"initial_unsafe_mode","mode":mode,"error":error.to_string(),"before":before,"after":after})
        );
        observations.push((mode, before, after));
    }
    for (mode, before, after) in observations {
        assert_eq!(
            after, before,
            "rejected initial mode {mode:o} changed the candidate directory"
        );
    }
}

fn headless(value: Value) -> Result<RuntimeConfig, RuntimeError> {
    serde_json::from_value::<HeadlessConfig>(value)
        .map_err(RuntimeError::Json)?
        .into_runtime_config()
}
#[test]
fn headless_content_configuration_checks_limits_paths_and_exact_permission_schema_without_opening_store()
 {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let content =
        json!({"directory":root.join("content"),"device_id":"boundary-device","limits":limits()});
    let valid = put("notes", "allowed");
    let cfg=headless(json!({"content":content,"permissions":[{"capability":"content.put","arguments":valid},{"capability":"content.read","arguments":{"name":"notes","scope":"task"}}]})).unwrap();
    assert_eq!(cfg.content.unwrap().directory, root.join("content"));
    assert_eq!(
        cfg.permissions[0].scope,
        ScopedPermission::exact("content.put", valid).scope
    );
    assert!(!root.join("content").exists());
    assert!(!root.join("sessions.sqlite").exists());
    for args in [
        json!({"name":"../notes"}),
        json!({"name":"notes","session_id":"foreign"}),
        json!({"name":"notes","task_id":"foreign"}),
        json!({"name":"notes","path":"state.json"}),
        json!({"name":"notes","scope":"all"}),
    ] {
        assert!(headless(json!({"content":content,"permissions":[{"capability":"content.read","arguments":args}]})).is_err());
    }
    for args in [
        json!({"name":"notes","text":"x"}),
        json!({"name":"notes","text":"x","expected_version":"0"}),
        json!({"name":"notes","text":"x","expected_version":-1}),
        json!({"name":"notes","text":"x","expected_version":1,"expected_revision":"A".repeat(64)}),
    ] {
        assert!(headless(json!({"content":content,"permissions":[{"capability":"content.put","arguments":args}]})).is_err());
    }
    assert!(
        headless(
            json!({"permissions":[{"capability":"content.read","arguments":{"name":"notes"}}]})
        )
        .is_err()
    );
    for (field, value) in [
        ("directory", json!("relative/content")),
        ("device_id", json!("../device")),
        (
            "limits",
            json!({"max_content_bytes":0,"max_total_bytes":8,"max_records":1,"max_security_entries":1}),
        ),
        ("session_id", json!("foreign")),
    ] {
        let mut bad = content.clone();
        bad[field] = value;
        assert!(
            headless(json!({"content":bad})).is_err(),
            "accepted invalid {field}"
        );
    }
    assert!(!root.join("content").exists());
}

#[tokio::test]
async fn exact_permission_for_one_text_does_not_authorize_a_changed_text() {
    let granted = put("notes", "original");
    let changed = put("notes", "changed");
    let h = Harness::new(limits(), &[("content.put", granted.clone())]);
    let s = h.rt.create(None).unwrap().id;
    h.call(&s, "content.put", &granted).await;
    let saved = h.rt.content_inspect(&s, None, "notes").unwrap().unwrap();
    let authority = h.authority();
    let cursor = h.rt.get(&s).unwrap().cursor;
    h.provider.set(call("content.put", &changed));
    let rt = h.rt.clone();
    let session = s.clone();
    let worker = tokio::spawn(async move {
        rt.prompt(
            &session,
            "changed arguments need new permission",
            CancellationToken::new(),
        )
        .await
    });
    let pending = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Some(pending) = h.rt.get(&s).unwrap().pending_approvals.first() {
                break pending.clone();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        pending.input.scope,
        ScopedPermission::exact("content.put", changed).scope
    );
    assert!(!started_after(&h.rt, &s, cursor, "content.put"));
    assert_eq!(h.authority(), authority);
    h.rt.approve(&s, &pending.id, ApprovalChoice::Deny, "content-boundary")
        .unwrap();
    tokio::time::timeout(Duration::from_secs(20), worker)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(!started_after(&h.rt, &s, cursor, "content.put"));
    assert_eq!(
        h.rt.content_inspect(&s, None, "notes").unwrap().unwrap(),
        saved
    );
}

#[tokio::test]
async fn authenticated_ipc_inspection_validates_owner_shape_permissions_and_record_integrity() {
    let write = json!({"name":"private","scope":"task","text":"owned","expected_version":0});
    let h = Harness::new(limits(), &[("content.put", write.clone())]);
    let a = h.rt.create(None).unwrap().id;
    let b = h.rt.create(None).unwrap().id;
    h.call(&a, "content.put", &write).await;
    let task = h.rt.tasks(&a).unwrap().pop().unwrap();
    let saved =
        h.rt.content_inspect(&a, Some(&task.id), "private")
            .unwrap()
            .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let stop = CancellationToken::new();
    let server = tokio::spawn(serve(
        listener,
        Arc::new(h.rt.clone()),
        ServerConfig::new("boundary-token"),
        stop.clone(),
    ));
    assert_eq!(
        Client::connect(address, "wrong-token")
            .await
            .unwrap_err()
            .code,
        "unauthorized"
    );
    let client = Client::connect(address, "boundary-token").await.unwrap();
    let args = json!({"session_id":a,"task_id":task.id,"name":"private"});
    assert_eq!(
        client
            .request("content.inspect", args.clone())
            .await
            .unwrap(),
        serde_json::to_value(&saved).unwrap()
    );
    assert!(
        client
            .request("content.inspect", json!({"session_id":b,"name":"private"}))
            .await
            .unwrap()
            .is_null()
    );
    assert_eq!(
        client
            .request(
                "content.inspect",
                json!({"session_id":b,"task_id":task.id,"name":"private"})
            )
            .await
            .unwrap_err()
            .code,
        "invalid_params"
    );
    for field in ["path", "scope", "device_id"] {
        let mut bad = args.clone();
        bad[field] = json!("override");
        assert_eq!(
            client
                .request("content.inspect", bad)
                .await
                .unwrap_err()
                .code,
            "invalid_params"
        );
    }
    assert!(
        client
            .request(
                "content.inspect",
                json!({"session_id":a,"task_id":task.id,"name":saved.record.name})
            )
            .await
            .unwrap()
            .is_null()
    );
    std::fs::set_permissions(
        h.root.join("content"),
        std::fs::Permissions::from_mode(0o720),
    )
    .unwrap();
    assert_eq!(
        client
            .request("content.inspect", args.clone())
            .await
            .unwrap_err()
            .code,
        "invalid_params"
    );
    std::fs::set_permissions(
        h.root.join("content"),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    std::fs::write(
        h.root.join("content/objects").join(&saved.content_hash_hex),
        b"wrong",
    )
    .unwrap();
    let error = client.request("content.inspect", args).await.unwrap_err();
    assert!(error.message.contains("integrity"), "{error:?}");
    client.close().await;
    stop.cancel();
    server.await.unwrap().unwrap();
}

#[tokio::test]
async fn oversized_external_content_returns_an_explicit_ipc_frame_error_without_losing_the_connection()
 {
    let write = put("notes", "seed");
    let h = Harness::new(Limits::default(), &[("content.put", write.clone())]);
    let s = h.rt.create(None).unwrap().id;
    h.call(&s, "content.put", &write).await;
    let first = h.rt.content_inspect(&s, None, "notes").unwrap().unwrap();
    let store = cayw_content::ContentStore::open(
        h.root.join("content"),
        cayw_content::Id::new("boundary-device").unwrap(),
        Limits::default(),
    )
    .unwrap();
    let text = "x".repeat(1024 * 1024 + 1);
    let record = store
        .try_put(
            &first.record.name,
            cayw_content::ExpectedRevision::Present {
                version: first.record.version,
                revision: first.record.revision,
            },
            text.as_bytes(),
        )
        .unwrap();
    drop(store);
    let authority = h.authority();
    let cursor = h.rt.get(&s).unwrap().cursor;
    let inspected = h.rt.content_inspect(&s, None, "notes").unwrap().unwrap();
    assert_eq!(inspected.text, text);
    assert_eq!(inspected.record, record);
    let sdk_bytes = serde_json::to_vec(&inspected).unwrap().len();
    assert!(sdk_bytes > 1024 * 1024);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let stop = CancellationToken::new();
    let server = tokio::spawn(serve(
        listener,
        Arc::new(h.rt.clone()),
        ServerConfig::new("large-content-token"),
        stop.clone(),
    ));
    let client = Client::connect(address, "large-content-token")
        .await
        .unwrap();
    let error = tokio::time::timeout(
        Duration::from_secs(20),
        client.request("content.inspect", json!({"session_id":s,"name":"notes"})),
    )
    .await
    .unwrap()
    .unwrap_err();
    let healthy = client.request("runtime.health", json!({})).await.unwrap();
    client.close().await;
    stop.cancel();
    server.await.unwrap().unwrap();
    println!(
        "CAYW_CONTENT_LARGE_INSPECTION {}",
        json!({"stored_text_bytes":text.len(),"sdk_value_bytes":sdk_bytes,"ipc_error":error,"connection_status":healthy["status"]})
    );
    assert_eq!(error.code, "response_too_large_outcome_unknown");
    assert!(error.outcome_unknown);
    assert_eq!(healthy["status"], "ok");
    assert_eq!(h.authority(), authority);
    assert_eq!(h.rt.get(&s).unwrap().cursor, cursor);
}

struct LockWorker(Child);
impl Drop for LockWorker {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
#[test]
fn content_lock_worker() {
    let Some(root) = std::env::var_os("CAYW_CONTENT_BOUNDARY_LOCK_ROOT") else {
        return;
    };
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(PathBuf::from(root).join(".lock"))
        .unwrap();
    file.lock().unwrap();
    println!("CAYW_CONTENT_BOUNDARY_LOCK_HELD");
    std::io::stdout().flush().unwrap();
    std::io::stdin().read_exact(&mut [0]).unwrap();
    file.unlock().unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn external_process_content_lock_wait_is_cancellable_without_detaching_a_write() {
    let write = put("notes", "must not be written");
    let h = Harness::new(limits(), &[("content.put", write.clone())]);
    let s = h.rt.create(None).unwrap().id;
    let authority = h.authority();
    let mut worker = LockWorker(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "content_lock_worker", "--nocapture"])
            .env("CAYW_CONTENT_BOUNDARY_LOCK_ROOT", h.root.join("content"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let mut output = BufReader::new(worker.0.stdout.take().unwrap());
    loop {
        let mut line = String::new();
        assert_ne!(
            output.read_line(&mut line).unwrap(),
            0,
            "lock worker exited"
        );
        if line.trim() == "CAYW_CONTENT_BOUNDARY_LOCK_HELD" {
            break;
        }
    }
    assert!(matches!(
        h.rt.content_inspect(&s, None, "notes"),
        Err(RuntimeError::Content(ContentError::Busy))
    ));
    h.provider.set(call("content.put", &write));
    let rt = h.rt.clone();
    let session = s.clone();
    let cancel = CancellationToken::new();
    let token = cancel.clone();
    let run = tokio::spawn(async move {
        rt.prompt(&session, "wait for externally held lock", token)
            .await
    });
    tokio::time::timeout(Duration::from_secs(20), async {
        while !started_after(&h.rt, &s, 0, "content.put") {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(h.authority(), authority);
    cancel.cancel();
    let result = tokio::time::timeout(Duration::from_secs(20), run)
        .await
        .expect("cancellation must not await external unlock")
        .unwrap();
    let task = h.rt.tasks(&s).unwrap().pop().unwrap();
    assert!(result.is_err() || task.state != TaskState::Completed);
    assert!(!matches!(
        task.state,
        TaskState::Running | TaskState::WaitingApproval | TaskState::Queued
    ));
    assert_eq!(h.authority(), authority);
    let settled = receipts(&h.rt, &s, "content.put");
    assert!(
        !settled.is_empty(),
        "cancelled started call needs a terminal receipt"
    );
    assert!(
        settled
            .iter()
            .all(|call| call["outcome"]["type"] != "completed")
    );
    println!(
        "CAYW_CONTENT_BUSY_CANCEL {}",
        json!({"task_state":task.state,"settled_outcomes":settled.iter().map(|call|&call["outcome"]).collect::<Vec<_>>()})
    );
    worker.0.stdin.take().unwrap().write_all(b"x").unwrap();
    assert!(worker.0.wait().unwrap().success());
    assert!(h.rt.content_inspect(&s, None, "notes").unwrap().is_none());
    assert_eq!(h.authority(), authority);
}
