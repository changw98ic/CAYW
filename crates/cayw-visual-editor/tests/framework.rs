//! Framework project tests.
//!
//! These tests drive the public editor API with the real companion process.
//! The companion maps JSX/TSX and computes bounded patches. Every read and write
//! passes through the real Host permission gate and compare-and-swap.
//!
//! A narrow test proxy handles the deterministic failure modes:
//! - a Host write failure after one successful file produces `partial_save`;
//! - a companion process exit must not become a Host disk-unknown outcome;
//! - cross-file patch rejection and sync failure injection.

use cayw_visual_editor::{EditorConfig, EditorHandle, start};
use reqwest::{StatusCode, header};
use serde_json::{Value, json};
use std::{
    env, fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};
use tempfile::TempDir;

const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789";

const INDEX: &str = "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\"><title>Framework fixture</title></head><body><div id=\"root\"></div><script type=\"module\" src=\"/src/main.tsx\"></script></body></html>\n";
const MAIN: &str = "import { App } from './App';\nexport const main = App;\n";
const APP: &str = "export function App() {\n  return <div id=\"style-me\">Hello</div>;\n}\n";
const OTHER: &str =
    "export const Other = () => <div id=\"other\">Other<script>console.log(1);</script></div>;\n";
const ONE: &str = "export const One = () => <div id=\"one\">One</div>;\n";
const TWO: &str = "export const Two = () => <div id=\"two\">Two</div>;\n";
const NEW_FILE: &str = "export const New = () => <div id=\"new\">New</div>;\n";

struct Fixture {
    project: TempDir,
    state: TempDir,
    _tool: TempDir,
    handle: EditorHandle,
    client: reqwest::Client,
}

#[derive(Clone, Copy)]
enum Shape {
    Default,
    /// Two files in separate directories for multi-file save tests.
    Pairs,
}

#[derive(Clone, Copy)]
enum CompanionKind {
    Real,
    FaultProxy,
}

impl Fixture {
    async fn open() -> Self {
        Self::open_with(Shape::Default, CompanionKind::Real).await
    }

    async fn open_pairs() -> Self {
        Self::open_with(Shape::Pairs, CompanionKind::Real).await
    }

    async fn open_fault() -> Self {
        Self::open_with(Shape::Default, CompanionKind::FaultProxy).await
    }

    async fn open_with(shape: Shape, kind: CompanionKind) -> Self {
        let project = tempfile::tempdir().expect("project tempdir");
        write_project(project.path(), shape);
        let state = tempfile::tempdir().expect("state tempdir");
        let tool = tempfile::tempdir().expect("tooling tempdir");
        let real_companion =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("framework/companion.mjs");
        let companion_script = match kind {
            CompanionKind::Real => real_companion,
            CompanionKind::FaultProxy => {
                let proxy_path = tool.path().join("fault_proxy.mjs");
                let proxy_code = format!(
                    r#"import {{ spawn }} from 'node:child_process';
import readline from 'node:readline';

const real = {};
const child = spawn(process.execPath, [real], {{
  cwd: process.cwd(),
  stdio: ['pipe', 'pipe', 'inherit'],
}});

let failNextSync = false;

const rlChild = readline.createInterface({{ input: child.stdout, terminal: false }});
rlChild.on('line', (line) => process.stdout.write(line + '\n'));
child.on('exit', (code) => process.exit(code ?? 0));

const rl = readline.createInterface({{ input: process.stdin, terminal: false }});
rl.on('line', (line) => {{
  let req;
  try {{
    req = JSON.parse(line);
  }} catch {{
    return child.stdin.write(line + '\n');
  }}
  if (req.method === 'edit') {{
    const op = req.params?.operation;
    if (op?.name === 'cayw_crash') {{
      child.kill();
      process.exit(3);
    }}
    if (op?.name === 'cayw_cross_file') {{
      const res = {{
        id: req.id,
        ok: true,
        result: {{
          patches: [
            {{ path: 'src/App.tsx', start: 0, end: 6, before: 'export', replacement: 'export' }},
            {{ path: 'src/other.tsx', start: 0, end: 6, before: 'export', replacement: 'export' }},
          ],
          selection: null,
        }},
      }};
      process.stdout.write(JSON.stringify(res) + '\n');
      return;
    }}
    if (op?.kind === 'move') {{
      failNextSync = true;
      const res = {{
        id: req.id,
        ok: true,
        result: {{
          patches: [
            {{ path: 'src/App.tsx', start: 0, end: 6, before: 'export', replacement: 'export' }},
          ],
          selection: null,
        }},
      }};
      process.stdout.write(JSON.stringify(res) + '\n');
      return;
    }}
  }} else if (req.method === 'sync' && failNextSync) {{
    failNextSync = false;
    const res = {{
      id: req.id,
      ok: false,
      error: {{ code: 'sync_failed', message: 'test proxy injected sync failure' }},
    }};
    process.stdout.write(JSON.stringify(res) + '\n');
    return;
  }}
  child.stdin.write(line + '\n');
}});
rl.on('close', () => {{
  child.kill();
  process.exit(0);
}});
"#,
                    serde_json::to_string(&real_companion.to_string_lossy())
                        .expect("serialize path")
                );
                fs::write(&proxy_path, proxy_code).expect("write fault proxy");
                proxy_path
            }
        };
        let config = EditorConfig {
            project_root: project.path().to_path_buf(),
            page: "index.html".to_owned(),
            state_dir: state.path().to_path_buf(),
            editor_addr: "127.0.0.1:0".parse().expect("editor address"),
            preview_addr: "127.0.0.1:0".parse().expect("preview address"),
            token: TOKEN.to_owned(),
            framework: Some("react".to_owned()),
            node: node_program(),
            companion: companion_script,
            codeintel: None,
        };
        let handle = start(config).await.expect("framework editor starts");
        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("client");
        Self {
            project,
            state,
            _tool: tool,
            handle,
            client,
        }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.project.path().join(relative)
    }

    fn read(&self, relative: &str) -> String {
        fs::read_to_string(self.path(relative)).expect("read source")
    }

    fn write(&self, relative: &str, content: &str) {
        fs::write(self.path(relative), content).expect("write source");
    }

    fn editor(&self, path: &str) -> String {
        format!("{}{}", self.handle.editor_url.trim_end_matches('/'), path)
    }

    async fn document(&self) -> Value {
        let response = self
            .client
            .get(self.editor("/api/document"))
            .bearer_auth(TOKEN)
            .send()
            .await
            .expect("document request");
        assert_eq!(response.status(), StatusCode::OK);
        response.json().await.expect("document json")
    }

    async fn post(&self, path: &str, body: Value) -> (StatusCode, Value) {
        let response = self
            .client
            .post(self.editor(path))
            .bearer_auth(TOKEN)
            .header(header::ORIGIN, &self.handle.editor_origin)
            .json(&body)
            .send()
            .await
            .expect("post request");
        let status = response.status();
        (status, response.json().await.unwrap_or(Value::Null))
    }

    async fn operate(&self, operation: Value) -> Value {
        let (status, body) = self.post("/api/operation", operation).await;
        assert_eq!(status, StatusCode::OK, "operation failed: {body}");
        body
    }

    async fn save(&self, expected: &str) -> (StatusCode, Value) {
        self.post("/api/save", json!({ "expected_sha256": expected }))
            .await
    }

    async fn reload(&self) -> Value {
        let (status, body) = self.post("/api/reload", json!({})).await;
        assert_eq!(status, StatusCode::OK, "reload failed: {body}");
        body
    }

    fn journal(&self) -> String {
        fs::read_to_string(self.state.path().join("server-events.jsonl")).unwrap_or_default()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.handle.cancel();
    }
}

fn node_program() -> PathBuf {
    env::var_os("CAYW_NODE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("node"))
}

fn write_project(root: &Path, shape: Shape) {
    fs::write(root.join("index.html"), INDEX).expect("index");
    fs::create_dir_all(root.join("src")).expect("src directory");
    fs::write(root.join("src/main.tsx"), MAIN).expect("main");
    fs::write(root.join("src/App.tsx"), APP).expect("app");
    fs::write(root.join("src/other.tsx"), OTHER).expect("other");
    match shape {
        Shape::Default => {}
        Shape::Pairs => {
            fs::create_dir_all(root.join("a")).expect("a directory");
            fs::create_dir_all(root.join("b")).expect("b directory");
            fs::write(root.join("a/one.tsx"), ONE).expect("one");
            fs::write(root.join("b/two.tsx"), TWO).expect("two");
        }
    }
}

fn node_at<'a>(document: &'a Value, file: &str, tag: &str) -> &'a Value {
    document["snapshot"]["nodes"]
        .as_array()
        .expect("nodes")
        .iter()
        .find(|node| node["file"] == file && node["tag"] == tag && node["editable"] == true)
        .unwrap_or_else(|| panic!("no editable <{tag}> in {file}"))
}

fn error_code(body: &Value) -> &str {
    body["error"]["code"].as_str().unwrap_or("")
}

fn is_hex64(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[tokio::test]
async fn framework_open_maps_every_file_and_selection_is_a_source_file() {
    let fixture = Fixture::open().await;
    let document = fixture.document().await;
    let snapshot = &document["snapshot"];

    assert_eq!(snapshot["adapter"], "react");
    assert_eq!(snapshot["html"], "");
    assert!(is_hex64(snapshot["revision"].as_str().expect("revision")));
    assert!(is_hex64(snapshot["sha256"].as_str().expect("baseline")));
    assert_eq!(snapshot["path"], "src/App.tsx");
    assert_eq!(snapshot["source"], APP);

    let files = snapshot["files"].as_array().expect("files");
    let paths: Vec<&str> = files
        .iter()
        .map(|file| file["path"].as_str().expect("path"))
        .collect();
    assert_eq!(
        paths,
        vec!["index.html", "src/App.tsx", "src/main.tsx", "src/other.tsx",]
    );
    for file in files {
        assert!(is_hex64(file["sha256"].as_str().expect("file digest")));
        assert!(file["source"].is_string());
    }

    let nodes = snapshot["nodes"].as_array().expect("nodes");
    assert!(!nodes.is_empty());
    let mut ids = std::collections::BTreeSet::new();
    for node in nodes {
        let id = node["id"].as_str().expect("node id");
        assert!(id.len() <= 128);
        assert!(id.starts_with('n'));
        assert!(ids.insert(id), "node ids must be unique");
        let file = node["file"].as_str().expect("node file");
        let start = node["start"].as_u64().expect("node start") as usize;
        let source = &snapshot["files"]
            .as_array()
            .expect("files")
            .iter()
            .find(|candidate| candidate["path"] == file)
            .expect("node file in manifest")["source"]
            .as_str()
            .expect("source");
        assert_eq!(source.as_bytes()[start], b'<');
        assert!(source.is_char_boundary(start));
        if let Some(parent) = node["parent"].as_str() {
            let parent = nodes
                .iter()
                .find(|candidate| candidate["id"] == parent)
                .expect("parent node exists");
            assert_eq!(parent["file"], file, "a parent must be in the same file");
            assert!(
                (parent["start"].as_u64().expect("parent start") as usize) < start,
                "a parent must start before its child"
            );
        }
    }
    assert!(snapshot["selected_id"].is_null());
}

#[tokio::test]
async fn style_text_insert_and_history_round_trip() {
    let fixture = Fixture::open().await;
    let document = fixture.document().await;
    let id = node_at(&document, "src/App.tsx", "div")["id"]
        .as_str()
        .expect("id")
        .to_owned();

    let styled = fixture
        .operate(json!({ "kind": "style", "id": id, "properties": { "color": "red" } }))
        .await;
    assert!(
        styled["snapshot"]["source"]
            .as_str()
            .expect("source")
            .contains("style={{ color: \"red\" }}")
    );
    assert_eq!(styled["dirty"], true);
    assert_eq!(styled["can_undo"], true);
    assert_eq!(fixture.read("src/App.tsx"), APP, "draft never reaches disk");

    let undone = fixture.operate(json!({ "kind": "undo" })).await;
    assert_eq!(undone["snapshot"]["source"], APP);
    assert_eq!(undone["dirty"], false);
    assert_eq!(undone["can_redo"], true);

    let redone = fixture.operate(json!({ "kind": "redo" })).await;
    assert!(
        redone["snapshot"]["source"]
            .as_str()
            .expect("source")
            .contains("style={{ color: \"red\" }}")
    );

    let current = redone["snapshot"]["nodes"]
        .as_array()
        .expect("nodes")
        .iter()
        .find(|node| node["file"] == "src/App.tsx" && node["tag"] == "div")
        .expect("app div")["id"]
        .as_str()
        .expect("id")
        .to_owned();
    let inserted = fixture
        .operate(json!({
            "kind": "insert",
            "target": current,
            "placement": "inside",
            "component": "text"
        }))
        .await;
    let inserted_id = inserted["snapshot"]["selected_id"]
        .as_str()
        .expect("insert keeps a selection")
        .to_owned();
    let inserted_node = inserted["snapshot"]["nodes"]
        .as_array()
        .expect("nodes")
        .iter()
        .find(|node| node["id"] == inserted_id)
        .expect("selected node");
    assert_eq!(inserted_node["file"], "src/App.tsx");

    let deleted = fixture
        .operate(json!({ "kind": "delete", "id": inserted_id }))
        .await;
    assert!(deleted["snapshot"]["selected_id"].is_null());
}

#[tokio::test]
async fn stale_node_id_and_cross_file_patch_are_rejected_without_state_change() {
    let fixture = Fixture::open_fault().await;
    let document = fixture.document().await;
    let stale_id = node_at(&document, "src/App.tsx", "div")["id"]
        .as_str()
        .expect("id")
        .to_owned();

    fixture
        .operate(json!({ "kind": "style", "id": stale_id, "properties": { "color": "red" } }))
        .await;
    let after_style = fixture.document().await;
    let styled_source = after_style["snapshot"]["source"]
        .as_str()
        .expect("source")
        .to_owned();

    let (status, body) = fixture
        .post(
            "/api/operation",
            json!({ "kind": "text", "id": stale_id, "text": "Old" }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(error_code(&body), "stale_node");
    let unchanged = fixture.document().await;
    assert_eq!(unchanged["snapshot"]["source"], styled_source);

    let current_id = node_at(&unchanged, "src/App.tsx", "div")["id"]
        .as_str()
        .expect("id")
        .to_owned();
    let (status, body) = fixture
        .post(
            "/api/operation",
            json!({
                "kind": "attribute",
                "id": current_id,
                "name": "cayw_cross_file",
                "value": "x"
            }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(error_code(&body), "invalid_patch");
    assert_eq!(
        fixture.document().await["snapshot"]["source"],
        styled_source
    );
}

#[tokio::test]
async fn cross_file_move_is_rejected_before_the_companion() {
    let fixture = Fixture::open_pairs().await;
    let document = fixture.document().await;
    let one = node_at(&document, "a/one.tsx", "div")["id"]
        .as_str()
        .expect("one")
        .to_owned();
    let two = node_at(&document, "b/two.tsx", "div")["id"]
        .as_str()
        .expect("two")
        .to_owned();

    let (status, body) = fixture
        .post(
            "/api/operation",
            json!({ "kind": "move", "id": one, "target": two, "placement": "after" }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(error_code(&body), "cross_file_move");
    let after = fixture.document().await;
    assert_eq!(after["dirty"], false);
    assert_eq!(fixture.read("a/one.tsx"), ONE);
    assert_eq!(fixture.read("b/two.tsx"), TWO);
}

#[tokio::test]
async fn sync_failure_does_not_commit_the_candidate_draft() {
    let fixture = Fixture::open_fault().await;
    let document = fixture.document().await;
    let id = node_at(&document, "src/App.tsx", "div")["id"]
        .as_str()
        .expect("id")
        .to_owned();

    let (status, body) = fixture
        .post(
            "/api/operation",
            json!({ "kind": "move", "id": id, "target": id, "placement": "after" }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(error_code(&body), "sync_failed");
    let after = fixture.document().await;
    assert_eq!(after["snapshot"]["source"], APP);
    assert_eq!(after["dirty"], false);
    assert_eq!(after["can_undo"], false);
}

#[tokio::test]
async fn reload_rescans_added_renamed_and_removed_files() {
    let fixture = Fixture::open().await;
    let document = fixture.document().await;
    let id = node_at(&document, "src/App.tsx", "div")["id"]
        .as_str()
        .expect("id")
        .to_owned();
    fixture
        .operate(json!({ "kind": "style", "id": id, "properties": { "color": "red" } }))
        .await;

    fixture.write("src/New.tsx", NEW_FILE);
    fs::rename(
        fixture.path("src/other.tsx"),
        fixture.path("src/Renamed.tsx"),
    )
    .expect("rename other");
    fs::remove_file(fixture.path("src/App.tsx")).expect("remove app");

    let reloaded = fixture.reload().await;
    assert_eq!(reloaded["dirty"], false);
    assert_eq!(reloaded["can_undo"], false);
    let paths: Vec<&str> = reloaded["snapshot"]["files"]
        .as_array()
        .expect("files")
        .iter()
        .map(|file| file["path"].as_str().expect("path"))
        .collect();
    assert_eq!(
        paths,
        vec![
            "index.html",
            "src/New.tsx",
            "src/Renamed.tsx",
            "src/main.tsx"
        ]
    );

    // The refreshed Host gate must admit the added file, not the removed one.
    let new_id = node_at(&reloaded, "src/New.tsx", "div")["id"]
        .as_str()
        .expect("new id")
        .to_owned();
    let styled = fixture
        .operate(json!({ "kind": "style", "id": new_id, "properties": { "color": "green" } }))
        .await;
    let baseline = styled["snapshot"]["sha256"]
        .as_str()
        .expect("baseline")
        .to_owned();
    let (status, saved) = fixture.save(&baseline).await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert!(
        fixture
            .read("src/New.tsx")
            .contains("style={{ color: \"green\" }}")
    );
    assert_eq!(fixture.read("src/Renamed.tsx"), OTHER);
    assert!(!fixture.path("src/App.tsx").exists());
}

#[tokio::test]
async fn reload_failure_keeps_the_existing_draft() {
    let fixture = Fixture::open().await;
    let document = fixture.document().await;
    let id = node_at(&document, "src/App.tsx", "div")["id"]
        .as_str()
        .expect("id")
        .to_owned();
    let styled = fixture
        .operate(json!({ "kind": "style", "id": id, "properties": { "color": "red" } }))
        .await;
    let draft = styled["snapshot"]["source"]
        .as_str()
        .expect("draft")
        .to_owned();
    let files: Vec<String> = styled["snapshot"]["files"]
        .as_array()
        .expect("files")
        .iter()
        .map(|file| file["path"].as_str().expect("path").to_owned())
        .collect();

    fs::write(fixture.path("src/broken.tsx"), [0xff, 0xfe, 0xfd]).expect("broken utf8");

    let (status, body) = fixture.post("/api/reload", json!({})).await;
    assert!(!status.is_success(), "{body}");
    assert_eq!(error_code(&body), "read_failed");

    let after = fixture.document().await;
    assert_eq!(
        after["snapshot"]["source"], draft,
        "the draft survives a failed reload"
    );
    assert_eq!(after["dirty"], true);
    let after_files: Vec<&str> = after["snapshot"]["files"]
        .as_array()
        .expect("files")
        .iter()
        .map(|file| file["path"].as_str().expect("path"))
        .collect();
    assert_eq!(
        after_files,
        files.iter().map(String::as_str).collect::<Vec<_>>(),
        "a failed reload does not install the unread manifest"
    );

    fs::remove_file(fixture.path("src/broken.tsx")).expect("remove broken");
    let reloaded = fixture.reload().await;
    assert_eq!(reloaded["dirty"], false);
    assert_eq!(reloaded["snapshot"]["source"], APP);
}

#[tokio::test]
async fn readonly_companion_nodes_are_rejected() {
    let fixture = Fixture::open().await;
    let document = fixture.document().await;
    let script = document["snapshot"]["nodes"]
        .as_array()
        .expect("nodes")
        .iter()
        .find(|node| node["file"] == "src/other.tsx" && node["tag"] == "script")
        .expect("script node");
    assert_eq!(script["editable"], false);
    let id = script["id"].as_str().expect("id").to_owned();

    for operation in [
        json!({ "kind": "style", "id": id, "properties": { "color": "red" } }),
        json!({ "kind": "delete", "id": id }),
        json!({ "kind": "insert", "target": id, "placement": "inside", "component": "text" }),
    ] {
        let (status, body) = fixture.post("/api/operation", operation).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(error_code(&body), "not_editable");
    }
    let after = fixture.document().await;
    assert_eq!(after["dirty"], false);
    assert_eq!(after["snapshot"]["source"], APP);
}

#[tokio::test]
async fn save_writes_dirty_files_and_updates_the_baseline() {
    let fixture = Fixture::open().await;
    let document = fixture.document().await;
    let baseline = document["snapshot"]["sha256"]
        .as_str()
        .expect("baseline")
        .to_owned();
    let id = node_at(&document, "src/App.tsx", "div")["id"]
        .as_str()
        .expect("id")
        .to_owned();
    let style = fixture
        .operate(json!({ "kind": "style", "id": id, "properties": { "color": "red" } }))
        .await;
    let draft_revision = style["snapshot"]["revision"]
        .as_str()
        .expect("revision")
        .to_owned();

    let (status, saved) = fixture.save(&baseline).await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["dirty"], false);
    assert_eq!(saved["snapshot"]["revision"], draft_revision);
    assert!(
        fixture
            .read("src/App.tsx")
            .contains("style={{ color: \"red\" }}")
    );
    assert_eq!(fixture.read("src/other.tsx"), OTHER);
    assert_ne!(saved["snapshot"]["sha256"], baseline);

    let new_baseline = saved["snapshot"]["sha256"]
        .as_str()
        .expect("baseline")
        .to_owned();
    let (status, body) = fixture.save(&new_baseline).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["dirty"], false);
}

#[tokio::test]
async fn external_conflict_blocks_all_writes_until_reload() {
    let fixture = Fixture::open_pairs().await;
    let document = fixture.document().await;
    let baseline = document["snapshot"]["sha256"]
        .as_str()
        .expect("baseline")
        .to_owned();
    let one = node_at(&document, "a/one.tsx", "div")["id"]
        .as_str()
        .expect("one")
        .to_owned();
    fixture
        .operate(json!({ "kind": "style", "id": one, "properties": { "color": "red" } }))
        .await;
    let after_one = fixture.document().await;
    let two = node_at(&after_one, "b/two.tsx", "div")["id"]
        .as_str()
        .expect("two")
        .to_owned();
    fixture
        .operate(json!({ "kind": "style", "id": two, "properties": { "color": "blue" } }))
        .await;

    fixture.write("b/two.tsx", &TWO.replace("Two", "External"));

    let (status, body) = fixture.save(&baseline).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(error_code(&body), "stale_source");
    assert_eq!(
        fixture.read("a/one.tsx"),
        ONE,
        "a conflicting save writes no file"
    );

    let (status, body) = fixture
        .post(
            "/api/operation",
            json!({ "kind": "style", "id": one, "properties": { "color": "green" } }),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(error_code(&body), "stale_source");

    let reloaded = fixture.reload().await;
    assert_eq!(reloaded["dirty"], false);
    assert_eq!(reloaded["can_undo"], false);
    assert!(
        reloaded["snapshot"]["files"]
            .as_array()
            .expect("files")
            .iter()
            .any(|file| file["path"] == "b/two.tsx"
                && file["source"]
                    .as_str()
                    .is_some_and(|source| source.contains("External")))
    );

    let one_now = node_at(&reloaded, "a/one.tsx", "div")["id"]
        .as_str()
        .expect("one")
        .to_owned();
    fixture
        .operate(json!({ "kind": "style", "id": one_now, "properties": { "color": "green" } }))
        .await;
}

#[tokio::test]
async fn partial_save_is_a_durable_unknown_and_stops_the_editor() {
    let fixture = Fixture::open_pairs().await;
    let document = fixture.document().await;
    let baseline = document["snapshot"]["sha256"]
        .as_str()
        .expect("baseline")
        .to_owned();
    let one = node_at(&document, "a/one.tsx", "div")["id"]
        .as_str()
        .expect("one")
        .to_owned();
    fixture
        .operate(json!({ "kind": "style", "id": one, "properties": { "color": "red" } }))
        .await;
    let after_one = fixture.document().await;
    let two = node_at(&after_one, "b/two.tsx", "div")["id"]
        .as_str()
        .expect("two")
        .to_owned();
    fixture
        .operate(json!({ "kind": "style", "id": two, "properties": { "color": "blue" } }))
        .await;

    // Sorted save order writes a/one.tsx first, then cannot create a temporary
    // file in the read-only b directory. That is a real partial write.
    let readonly = fixture.path("b");
    let restore = PermissionsRestore(readonly.clone());
    fs::set_permissions(&readonly, fs::Permissions::from_mode(0o555)).expect("read-only b");

    let (status, body) = fixture.save(&baseline).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(error_code(&body), "partial_save");
    assert_eq!(body["error"]["outcome_unknown"], true);
    assert!(
        fixture
            .read("a/one.tsx")
            .contains("style={{ color: \"red\" }}")
    );
    assert_eq!(fixture.read("b/two.tsx"), TWO);
    assert!(fixture.journal().contains("\"outcome_unknown\":true"));

    let (status, body) = fixture
        .post("/api/operation", json!({ "kind": "undo" }))
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(error_code(&body), "outcome_unknown");
    restore.restore();
}

struct PermissionsRestore(PathBuf);

impl PermissionsRestore {
    fn restore(&self) {
        let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
    }
}

impl Drop for PermissionsRestore {
    fn drop(&mut self) {
        self.restore();
    }
}

#[tokio::test]
async fn companion_exit_is_not_a_host_disk_unknown() {
    let fixture = Fixture::open_fault().await;
    let document = fixture.document().await;
    let id = node_at(&document, "src/App.tsx", "div")["id"]
        .as_str()
        .expect("id")
        .to_owned();

    let (status, body) = fixture
        .post(
            "/api/operation",
            json!({
                "kind": "attribute",
                "id": id,
                "name": "cayw_crash",
                "value": "x"
            }),
        )
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"]["outcome_unknown"], false);
    assert!(
        !fixture.state.path().join("unknown.json").exists(),
        "a compiler exit must not leave a Host disk-unknown marker"
    );

    let after = fixture.document().await;
    assert_eq!(after["snapshot"]["source"], APP);
    assert!(!fixture.journal().contains("\"outcome_unknown\":true"));

    let (status, body) = fixture
        .post(
            "/api/operation",
            json!({ "kind": "style", "id": id, "properties": { "color": "red" } }),
        )
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"]["outcome_unknown"], false);
}
