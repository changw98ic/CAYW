//! Loopback HTTP and preview-origin validation for the visual editor.
//!
//! The tests open a real project through the source engine's public
//! `Project::open` path, bind real sockets, and use the same HTTP surface a
//! browser uses. They do not contact any model provider.

use cayw_visual_editor::{EditorConfig, EditorHandle, NODE_ATTRIBUTE, start};
use reqwest::{StatusCode, header};
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader},
    net::SocketAddr,
    os::unix::fs::symlink,
    path::Path,
    process::{Command, Stdio},
};
use tempfile::TempDir;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789";

const PAGE: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>Editor fixture</title>
<link rel="stylesheet" href="/styles.css">
</head>
<body>
<header class="site-header"><h1 id="title">Original fixture</h1><nav><a href="/about.html" id="about-link">About</a></nav></header>
<main id="app">
  <section class="hero"><p class="lead">Welcome to the fixture.</p><button id="action" type="button">Run</button></section>
  <img id="logo" src="/logo.svg" alt="Logo" width="64" height="64">
  <div class="cards"><article class="card" id="card-1"><h2>First</h2><p>First body</p></article><article class="card" id="card-2"><h2>Second</h2><p>Second body</p></article></div>
  <input id="name" name="name" placeholder="Name">
</main>
<script src="/script.js"></script>
</body>
</html>
"#;

const STYLES: &str = "body { margin: 0; font-family: system-ui, sans-serif; }\n.cards { display: flex; gap: 1rem; }\n";
const SCRIPT: &str = "document.getElementById('action').addEventListener('click', () => { document.body.dataset.ran = '1'; });\n";
const LOGO: &str = "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"64\" height=\"64\"><rect width=\"64\" height=\"64\" fill=\"#2563eb\"/></svg>\n";

struct Fixture {
    project: TempDir,
    _outside: TempDir,
    state: TempDir,
    handle: EditorHandle,
    client: reqwest::Client,
}

impl Fixture {
    async fn open() -> Self {
        let project = tempfile::tempdir().expect("project tempdir");
        let state = tempfile::tempdir().expect("state tempdir");
        let outside = write_fixture(project.path());
        let config = EditorConfig {
            project_root: project.path().to_path_buf(),
            page: "index.html".to_owned(),
            state_dir: state.path().to_path_buf(),
            editor_addr: "127.0.0.1:0".parse().expect("editor address"),
            preview_addr: "127.0.0.1:0".parse().expect("preview address"),
            token: TOKEN.to_owned(),
            framework: None,
            node: "node".into(),
            companion: cayw_visual_editor::default_companion(),
            codeintel: None,
        };
        let handle = start(config).await.expect("editor starts");
        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("client");
        Self {
            project,
            _outside: outside,
            state,
            handle,
            client,
        }
    }

    fn editor(&self, path: &str) -> String {
        format!("{}{}", self.handle.editor_url.trim_end_matches('/'), path)
    }

    fn preview(&self, path: &str) -> String {
        format!(
            "{}{}",
            self.handle.preview_origin.trim_end_matches('/'),
            path
        )
    }

    fn preview_addr(&self) -> SocketAddr {
        self.handle
            .preview_origin
            .trim_start_matches("http://")
            .parse()
            .expect("preview authority")
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

    fn source(&self) -> String {
        fs::read_to_string(self.project.path().join("index.html")).expect("source")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.handle.cancel();
    }
}

/// Writes the project fixture and returns a second temporary directory that
/// holds the external secret. A fixed name under the shared system temporary
/// directory would collide across parallel tests.
fn write_fixture(root: &Path) -> TempDir {
    fs::write(root.join("index.html"), PAGE).expect("index");
    fs::write(root.join("styles.css"), STYLES).expect("styles");
    fs::write(root.join("script.js"), SCRIPT).expect("script");
    fs::write(root.join("logo.svg"), LOGO).expect("logo");
    fs::write(root.join("notes.ts"), "not served\n").expect("disallowed asset");
    fs::create_dir(root.join("subdir")).expect("directory");
    fs::write(root.join(".env"), "SECRET=1\n").expect("dot file");
    let outside = tempfile::tempdir().expect("outside tempdir");
    let external = outside.path().join("secret.css");
    fs::write(&external, "outside\n").expect("outside file");
    symlink(&external, root.join("leak.css")).expect("symlink");
    outside
}

fn find_node<'a>(document: &'a Value, tag: &str) -> &'a Value {
    document["snapshot"]["nodes"]
        .as_array()
        .expect("nodes")
        .iter()
        .find(|node| node["tag"] == tag && node["editable"] == true)
        .unwrap_or_else(|| panic!("no editable {tag} node in document"))
}

fn node_id(node: &Value) -> &str {
    node["id"].as_str().expect("node id")
}

fn error_code(body: &Value) -> &str {
    body["error"]["code"].as_str().unwrap_or("")
}

/// Fetches the editable preview of the fixture page and removes the injected
/// bridge script, so the remaining bytes must equal the current snapshot HTML.
async fn preview_without_bridge(fixture: &Fixture) -> String {
    let preview = fixture
        .client
        .get(fixture.preview("/index.html?cayw_edit=1"))
        .send()
        .await
        .expect("preview")
        .text()
        .await
        .expect("preview body");
    let bridge_start = preview
        .find("<script src=\"/__cayw_bridge.js?")
        .expect("bridge script");
    let bridge_end = preview[bridge_start..]
        .find("\"></script>\n")
        .map(|offset| bridge_start + offset + "\"></script>\n".len())
        .expect("bridge script end");
    let mut without_bridge = String::with_capacity(preview.len());
    without_bridge.push_str(&preview[..bridge_start]);
    without_bridge.push_str(&preview[bridge_end..]);
    without_bridge
}

async fn raw(addr: SocketAddr, request: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).await.expect("preview connect");
    stream.write_all(request.as_bytes()).await.expect("write");
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).await.expect("read");
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, body)| body.to_owned())
        .unwrap_or_default();
    (status, body)
}

#[tokio::test(flavor = "multi_thread")]
async fn health_is_public_and_minimal() {
    let fixture = Fixture::open().await;
    let response = fixture
        .client
        .get(fixture.editor("/api/health"))
        .send()
        .await
        .expect("health");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.text().await.expect("body"), r#"{"status":"ok"}"#);
}

#[tokio::test(flavor = "multi_thread")]
async fn api_requires_bearer_token_and_strict_host_and_origin() {
    let fixture = Fixture::open().await;

    let response = fixture
        .client
        .get(fixture.editor("/api/document"))
        .send()
        .await
        .expect("unauthenticated");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body: Value = response.json().await.expect("error json");
    assert_eq!(error_code(&body), "unauthorized");

    let response = fixture
        .client
        .get(fixture.editor("/api/document"))
        .bearer_auth("not-the-editor-token")
        .send()
        .await
        .expect("bad token");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let response = fixture
        .client
        .get(fixture.editor("/api/document"))
        .bearer_auth(TOKEN)
        .header(header::HOST, "evil.example")
        .send()
        .await
        .expect("bad host");
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let body: Value = response.json().await.expect("error json");
    assert_eq!(error_code(&body), "forbidden_host");

    let response = fixture
        .client
        .get(fixture.editor("/api/document"))
        .bearer_auth(TOKEN)
        .header(header::ORIGIN, "http://evil.example")
        .send()
        .await
        .expect("bad origin");
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let body: Value = response.json().await.expect("error json");
    assert_eq!(error_code(&body), "forbidden_origin");
}

#[tokio::test(flavor = "multi_thread")]
async fn state_changing_requests_require_the_editor_origin() {
    let fixture = Fixture::open().await;

    let response = fixture
        .client
        .post(fixture.editor("/api/operation"))
        .bearer_auth(TOKEN)
        .json(&json!({"kind": "undo"}))
        .send()
        .await
        .expect("no origin");
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let body: Value = response.json().await.expect("error json");
    assert_eq!(error_code(&body), "forbidden_origin");

    let response = fixture
        .client
        .post(fixture.editor("/api/operation"))
        .bearer_auth(TOKEN)
        .header(header::ORIGIN, "http://evil.example")
        .json(&json!({"kind": "undo"}))
        .send()
        .await
        .expect("bad origin");
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    // The correct origin passes the gate and reaches the source engine, which
    // reports that this new project has no history yet.
    let (status, body) = fixture
        .post("/api/operation", json!({"kind": "undo"}))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error_code(&body), "history_empty");
}

#[tokio::test(flavor = "multi_thread")]
async fn editor_static_assets_are_served_with_security_headers() {
    let fixture = Fixture::open().await;
    for (path, expected_type) in [
        ("/", "text/html"),
        ("/editor.js", "text/javascript"),
        ("/editor.css", "text/css"),
    ] {
        let response = fixture
            .client
            .get(fixture.editor(path))
            .send()
            .await
            .expect("static asset");
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("");
        assert!(
            content_type.starts_with(expected_type),
            "{path}: {content_type}"
        );
        assert_eq!(
            response
                .headers()
                .get(header::CACHE_CONTROL)
                .and_then(|value| value.to_str().ok()),
            Some("no-store")
        );
        assert!(
            response
                .headers()
                .get(header::CONTENT_SECURITY_POLICY)
                .is_some(),
            "{path} must carry a CSP"
        );
        assert!(!response.text().await.expect("body").is_empty());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn preview_serves_the_original_page_and_assets_verbatim() {
    let fixture = Fixture::open().await;
    let response = fixture
        .client
        .get(fixture.preview("/index.html"))
        .send()
        .await
        .expect("preview page");
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers().get("x-cayw-edit").is_none());
    assert_eq!(response.text().await.expect("page"), PAGE);

    let response = fixture
        .client
        .get(fixture.preview("/styles.css"))
        .send()
        .await
        .expect("css");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.text().await.expect("css body"), STYLES);

    let response = fixture
        .client
        .get(fixture.preview("/script.js"))
        .send()
        .await
        .expect("script");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.text().await.expect("script body"), SCRIPT);

    let response = fixture
        .client
        .get(fixture.preview("/logo.svg"))
        .send()
        .await
        .expect("logo");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.text().await.expect("logo body"), LOGO);
}

#[tokio::test(flavor = "multi_thread")]
async fn edit_mode_injects_mapping_and_bridge_without_touching_disk() {
    let fixture = Fixture::open().await;
    let response = fixture
        .client
        .get(fixture.preview("/index.html?cayw_edit=1"))
        .send()
        .await
        .expect("edit page");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-cayw-edit")
            .and_then(|value| value.to_str().ok()),
        Some("1")
    );
    let body = response.text().await.expect("edit body");
    assert!(body.contains(NODE_ATTRIBUTE), "mapping attribute missing");
    assert!(body.contains("/__cayw_bridge.js?editor_origin="));
    assert!(
        body.starts_with("<!doctype html>"),
        "the real doctype must stay first"
    );
    let head = body
        .find("<html lang=\"en\">\n<head>")
        .expect("original html and head opener");
    let bridge = body
        .find("/__cayw_bridge.js?editor_origin=")
        .expect("bridge");
    let project_script = body
        .find("<script src=\"/script.js\"")
        .expect("project script");
    assert!(
        head < bridge,
        "the bridge must follow the original html/head attributes"
    );
    assert!(
        bridge < project_script,
        "the bridge must run before every project script"
    );
    let encoded = fixture
        .handle
        .editor_origin
        .replace(':', "%3A")
        .replace('/', "%2F");
    assert!(
        body.contains(&encoded),
        "editor origin missing from bridge URL"
    );
    assert_eq!(fixture.source(), PAGE, "preview must never rewrite source");

    let response = fixture
        .client
        .get(fixture.preview("/__cayw_bridge.js"))
        .send()
        .await
        .expect("bridge");
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("text/javascript"))
    );
    assert!(
        response
            .text()
            .await
            .expect("bridge body")
            .contains("cayw:ready")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn preview_rejects_traversal_dotfiles_reserved_directories_symlinks_and_bad_hosts() {
    let fixture = Fixture::open().await;
    let addr = fixture.preview_addr();
    let authority = addr.to_string();
    let cases = [
        ("/%2e%2e/cayw-outside-secret.css", 400),
        ("/.env", 404),
        ("/.git/config", 404),
        ("/notes.ts", 404),
        ("/subdir", 404),
        ("/leak.css", 404),
        ("/__cayw-other.js", 404),
    ];
    for (path, expected) in cases {
        let request =
            format!("GET {path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n");
        let (status, body) = raw(addr, &request).await;
        assert_eq!(status, expected, "{path}: {body}");
        assert!(!body.contains("outside"), "{path} leaked an outside file");
    }

    let request =
        "GET /styles.css HTTP/1.1\r\nHost: evil.example\r\nConnection: close\r\n\r\n".to_owned();
    let (status, _) = raw(addr, &request).await;
    assert_eq!(status, 403);

    // The allowed asset still works through the same raw path.
    let request =
        format!("GET /styles.css HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n");
    let (status, body) = raw(addr, &request).await;
    assert_eq!(status, 200);
    assert_eq!(body, STYLES);
}

#[tokio::test(flavor = "multi_thread")]
async fn style_edits_update_snapshot_preview_and_authoritative_history() {
    let fixture = Fixture::open().await;
    let document = fixture.document().await;
    let id = node_id(find_node(&document, "h1")).to_owned();

    let document = fixture
        .operate(json!({"kind": "style", "id": id, "properties": {"color": "#ff0000", "font-size": "2rem"}}))
        .await;
    assert!(document["can_undo"].as_bool().expect("can_undo"));
    assert!(!document["can_redo"].as_bool().expect("can_redo"));
    assert!(document["dirty"].as_bool().expect("dirty"));
    let html = document["snapshot"]["html"].as_str().expect("html");
    assert!(html.contains("#ff0000"));
    assert!(html.contains("2rem"));

    let preview = fixture
        .client
        .get(fixture.preview("/index.html?cayw_edit=1"))
        .send()
        .await
        .expect("preview")
        .text()
        .await
        .expect("preview body");
    assert!(preview.contains("#ff0000"));

    let undo = fixture.operate(json!({"kind": "undo"})).await;
    assert!(!undo["can_undo"].as_bool().expect("can_undo"));
    assert!(undo["can_redo"].as_bool().expect("can_redo"));
    assert!(
        !undo["snapshot"]["html"]
            .as_str()
            .expect("html")
            .contains("#ff0000")
    );

    let redo = fixture.operate(json!({"kind": "redo"})).await;
    assert!(redo["can_undo"].as_bool().expect("can_undo"));
    assert!(!redo["can_redo"].as_bool().expect("can_redo"));
    assert!(
        redo["snapshot"]["html"]
            .as_str()
            .expect("html")
            .contains("#ff0000")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_operations_leave_document_and_preview_at_the_latest_state() {
    let fixture = Fixture::open().await;

    // Build a known six-step history. Each step reads the current snapshot
    // again because node ids are tied to the current draft revision, so an id
    // taken before an earlier edit must not be reused for a later one.
    let document = fixture.document().await;
    let heading = node_id(find_node(&document, "h1")).to_owned();
    fixture
        .operate(json!({"kind": "text", "id": heading, "text": "Concurrent heading"}))
        .await;

    let document = fixture.document().await;
    let heading = node_id(find_node(&document, "h1")).to_owned();
    fixture
        .operate(json!({"kind": "style", "id": heading, "properties": {"font-size": "2.5rem"}}))
        .await;

    let document = fixture.document().await;
    let paragraph = node_id(find_node(&document, "p")).to_owned();
    fixture
        .operate(json!({"kind": "style", "id": paragraph, "properties": {"color": "#123456"}}))
        .await;

    let document = fixture.document().await;
    let button = node_id(find_node(&document, "button")).to_owned();
    fixture
        .operate(json!({
            "kind": "attribute",
            "id": button,
            "name": "title",
            "value": "Concurrent title"
        }))
        .await;

    let document = fixture.document().await;
    let link = node_id(find_node(&document, "a")).to_owned();
    fixture
        .operate(json!({"kind": "text", "id": link, "text": "Concurrent link"}))
        .await;

    let document = fixture.document().await;
    let main = node_id(find_node(&document, "main")).to_owned();
    fixture
        .operate(
            json!({"kind": "style", "id": main, "properties": {"background-color": "#fafafa"}}),
        )
        .await;

    // Real concurrent requests through the same HTTP surface a browser uses.
    // Four id-free undos race with two document reads, exercising the unified
    // mutation gate without depending on revision-specific node ids. Undo is
    // last-in-first-out, so four undos leave exactly the first two edits.
    let (first, second, third, fourth, live, live_again) = tokio::join!(
        fixture.post("/api/operation", json!({"kind": "undo"})),
        fixture.post("/api/operation", json!({"kind": "undo"})),
        fixture.post("/api/operation", json!({"kind": "undo"})),
        fixture.post("/api/operation", json!({"kind": "undo"})),
        fixture.document(),
        fixture.document(),
    );
    for (status, body) in [first, second, third, fourth] {
        assert_eq!(
            status,
            StatusCode::OK,
            "concurrent operation failed: {body}"
        );
    }
    assert_eq!(live["snapshot"]["path"], "index.html");
    assert_eq!(live_again["snapshot"]["path"], "index.html");

    let latest = fixture.document().await;
    let html = latest["snapshot"]["html"].as_str().expect("html");
    for marker in ["Concurrent heading", "2.5rem"] {
        assert!(
            html.contains(marker),
            "latest document is missing {marker}: {html}"
        );
    }
    for marker in ["#123456", "Concurrent title", "Concurrent link", "#fafafa"] {
        assert!(
            !html.contains(marker),
            "undo left {marker} in the latest document: {html}"
        );
    }
    assert!(latest["dirty"].as_bool().expect("dirty"));
    assert!(latest["can_undo"].as_bool().expect("can_undo"));
    assert!(latest["can_redo"].as_bool().expect("can_redo"));

    assert_eq!(
        preview_without_bridge(&fixture).await,
        html,
        "preview did not publish the latest document snapshot"
    );
    assert_eq!(fixture.source(), PAGE, "operations must not write source");
}

#[tokio::test(flavor = "multi_thread")]
async fn stale_node_ids_from_an_earlier_snapshot_are_rejected() {
    let fixture = Fixture::open().await;
    let document = fixture.document().await;
    let stale = node_id(find_node(&document, "h1")).to_owned();

    let first = fixture
        .operate(json!({"kind": "text", "id": stale, "text": "First revision"}))
        .await;
    let first_html = first["snapshot"]["html"].as_str().expect("html").to_owned();
    assert!(first_html.contains("First revision"));

    // Both edits were built from the same snapshot. The first changed the
    // draft revision, so the second id now belongs to the previous revision
    // and must never be applied to whatever element occupies that position.
    let (status, body) = fixture
        .post(
            "/api/operation",
            json!({"kind": "style", "id": stale, "properties": {"color": "#ff0000"}}),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(error_code(&body), "not_found");

    let latest = fixture.document().await;
    let html = latest["snapshot"]["html"].as_str().expect("html");
    assert_eq!(html, first_html, "the rejected edit must change nothing");
    assert!(!html.contains("#ff0000"));
    assert_eq!(
        preview_without_bridge(&fixture).await,
        html,
        "the preview must stay at the first revision"
    );

    // A fresh snapshot issues ids bound to the current draft revision, so a
    // follow-up edit from that snapshot succeeds.
    let fresh = node_id(find_node(&latest, "h1")).to_owned();
    assert_ne!(fresh, stale, "a new draft must issue new node ids");
    let second = fixture
        .operate(json!({"kind": "style", "id": fresh, "properties": {"color": "#ff0000"}}))
        .await;
    assert!(
        second["snapshot"]["html"]
            .as_str()
            .expect("html")
            .contains("#ff0000")
    );
    let latest = fixture.document().await;
    assert_eq!(
        preview_without_bridge(&fixture).await,
        latest["snapshot"]["html"].as_str().expect("html"),
        "the preview must publish the recovery edit"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn text_insert_duplicate_move_and_delete_are_forwarded() {
    let fixture = Fixture::open().await;

    let document = fixture.document().await;
    let heading = node_id(find_node(&document, "h1")).to_owned();
    let document = fixture
        .operate(json!({"kind": "text", "id": heading, "text": "Changed heading"}))
        .await;
    assert!(
        document["snapshot"]["html"]
            .as_str()
            .expect("html")
            .contains("Changed heading")
    );

    let document = fixture.document().await;
    let main = node_id(find_node(&document, "main")).to_owned();
    let document = fixture
        .operate(
            json!({"kind": "insert", "target": main, "placement": "inside", "component": "card"}),
        )
        .await;
    assert!(
        document["snapshot"]["html"]
            .as_str()
            .expect("html")
            .contains("data-testid=\"cayw-component-card\"")
    );

    let document = fixture.document().await;
    let paragraph = node_id(find_node(&document, "p")).to_owned();
    let document = fixture
        .operate(json!({"kind": "duplicate", "id": paragraph}))
        .await;
    let html = document["snapshot"]["html"].as_str().expect("html");
    assert_eq!(
        html.matches("Welcome to the fixture.").count(),
        2,
        "duplicate result: {html}"
    );

    let document = fixture.document().await;
    let articles = document["snapshot"]["nodes"]
        .as_array()
        .expect("nodes")
        .iter()
        .filter(|node| node["tag"] == "article" && node["editable"] == true)
        .collect::<Vec<_>>();
    assert!(articles.len() >= 2, "expected two fixture cards");
    let second = node_id(articles[1]).to_owned();
    let first = node_id(articles[0]).to_owned();
    let document = fixture
        .operate(json!({"kind": "move", "id": second, "target": first, "placement": "before"}))
        .await;
    let html = document["snapshot"]["html"].as_str().expect("html");
    assert!(
        html.find("id=\"card-2\"").expect("card-2") < html.find("id=\"card-1\"").expect("card-1"),
        "move did not reorder the cards"
    );

    let document = fixture.document().await;
    let paragraph = node_id(find_node(&document, "p")).to_owned();
    let document = fixture
        .operate(json!({"kind": "delete", "id": paragraph}))
        .await;
    assert_eq!(
        document["snapshot"]["html"]
            .as_str()
            .expect("html")
            .matches("Welcome to the fixture.")
            .count(),
        1
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn whole_file_and_unknown_operations_are_rejected_by_the_server_gate() {
    let fixture = Fixture::open().await;
    let document = fixture.document().await;
    let id = node_id(find_node(&document, "h1")).to_owned();

    for operation in [
        json!({"kind": "replace", "html": "<html>owned</html>"}),
        json!({"kind": "write", "path": "index.html", "content": "x"}),
        json!({"kind": "style", "id": id, "properties": {"color": "#fff"}, "html": "<b>x</b>"}),
        json!({"kind": "style", "id": id, "properties": {"color": "red; position: fixed"}}),
        json!({"kind": "text", "id": id, "text": "ok", "path": "index.html"}),
        json!({"kind": "insert", "target": id, "placement": "inside"}),
        json!({"kind": "move", "id": id, "target": id, "placement": "sideways"}),
    ] {
        let (status, body) = fixture.post("/api/operation", operation.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{operation}: {body}");
        assert_eq!(error_code(&body), "unsupported_operation", "{operation}");
    }
    assert_eq!(fixture.source(), PAGE);
}

#[tokio::test(flavor = "multi_thread")]
async fn save_uses_cas_and_refuses_to_overwrite_an_external_change() {
    let fixture = Fixture::open().await;
    let document = fixture.document().await;
    let original_sha = document["snapshot"]["sha256"]
        .as_str()
        .expect("sha")
        .to_owned();
    let heading = node_id(find_node(&document, "h1")).to_owned();
    let document = fixture
        .operate(json!({"kind": "text", "id": heading, "text": "Saved heading"}))
        .await;
    assert!(document["dirty"].as_bool().expect("dirty"));

    let (status, saved) = fixture
        .post("/api/save", json!({"expected_sha256": original_sha}))
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert!(!saved["dirty"].as_bool().expect("dirty"));
    let saved_sha = saved["snapshot"]["sha256"]
        .as_str()
        .expect("sha")
        .to_owned();
    assert_ne!(saved_sha, original_sha);
    let source = fixture.source();
    assert!(source.contains("Saved heading"));
    assert!(
        !source.contains(NODE_ATTRIBUTE),
        "mapping must not reach disk"
    );

    // A stale editor cannot save over a newer file.
    let document = fixture.document().await;
    let heading = node_id(find_node(&document, "h1")).to_owned();
    fixture
        .operate(json!({"kind": "text", "id": heading, "text": "Unsaved heading"}))
        .await;
    let external = PAGE.replace("Original fixture", "External heading");
    fs::write(fixture.project.path().join("index.html"), &external).expect("external write");
    let document = fixture.document().await;
    let stale_sha = document["snapshot"]["sha256"]
        .as_str()
        .expect("sha")
        .to_owned();
    let (status, body) = fixture
        .post("/api/save", json!({"expected_sha256": stale_sha}))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(error_code(&body), "stale_source");
    assert_eq!(fixture.source(), external, "external bytes must survive");

    // The failed save refreshes the source digest inside the project engine.
    // The next document read must publish that digest together with the
    // authoritative draft and history flags, not the previous snapshot.
    let after = fixture.document().await;
    assert_ne!(
        after["snapshot"]["sha256"].as_str().expect("sha"),
        stale_sha,
        "a failed save must publish the refreshed source digest"
    );
    assert!(
        after["snapshot"]["html"]
            .as_str()
            .expect("html")
            .contains("Unsaved heading")
    );
    assert!(after["dirty"].as_bool().expect("dirty"));
    assert!(after["can_undo"].as_bool().expect("can_undo"));
}

#[tokio::test(flavor = "multi_thread")]
async fn reload_reads_source_again_and_clears_draft_history() {
    let fixture = Fixture::open().await;
    let document = fixture.document().await;
    let heading = node_id(find_node(&document, "h1")).to_owned();
    fixture
        .operate(json!({"kind": "text", "id": heading, "text": "Draft only"}))
        .await;

    let external = PAGE.replace("Original fixture", "Reloaded source");
    fs::write(fixture.project.path().join("index.html"), &external).expect("external write");
    let (status, document) = fixture.post("/api/reload", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{document}");
    assert!(!document["can_undo"].as_bool().expect("can_undo"));
    assert!(!document["can_redo"].as_bool().expect("can_redo"));
    assert!(!document["dirty"].as_bool().expect("dirty"));
    assert!(
        document["snapshot"]["html"]
            .as_str()
            .expect("html")
            .contains("Reloaded source")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn durable_audit_is_written_outside_the_project() {
    let fixture = Fixture::open().await;
    let document = fixture.document().await;
    let id = node_id(find_node(&document, "h1")).to_owned();
    fixture
        .operate(json!({"kind": "style", "id": id, "properties": {"color": "#00ff00"}}))
        .await;
    let log = fs::read_to_string(fixture.state.path().join("server-events.jsonl"))
        .expect("server audit log");
    assert!(log.contains("\"event\":\"started\""));
    assert!(log.contains("\"event\":\"settled\""));
    assert!(log.contains("\"outcome\":\"ok\""));
    assert!(!log.contains(TOKEN), "the token must never be audited");
    assert!(!fixture.project.path().join("server-events.jsonl").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn nested_uppercase_page_redirects_the_root_alias() {
    let project = tempfile::tempdir().expect("project");
    let state = tempfile::tempdir().expect("state");
    fs::create_dir(project.path().join("site")).expect("site directory");
    fs::write(project.path().join("site/index.HTML"), PAGE).expect("nested page");
    fs::write(project.path().join("styles.css"), STYLES).expect("styles");
    let handle = start(EditorConfig {
        project_root: project.path().to_path_buf(),
        page: "site/index.HTML".to_owned(),
        state_dir: state.path().to_path_buf(),
        editor_addr: "127.0.0.1:0".parse().expect("editor address"),
        preview_addr: "127.0.0.1:0".parse().expect("preview address"),
        token: TOKEN.to_owned(),
        framework: None,
        node: "node".into(),
        companion: cayw_visual_editor::default_companion(),
        codeintel: None,
    })
    .await
    .expect("uppercase nested page starts");
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("client");
    let response = client
        .get(format!(
            "{}/?cayw_edit=1",
            handle.preview_origin.trim_end_matches('/')
        ))
        .send()
        .await
        .expect("root alias");
    assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(
        response
            .headers()
            .get(header::LOCATION)
            .and_then(|value| value.to_str().ok()),
        Some("/site/index.HTML?cayw_edit=1")
    );

    let response = client
        .get(format!(
            "{}/site/index.HTML?cayw_edit=1",
            handle.preview_origin.trim_end_matches('/')
        ))
        .send()
        .await
        .expect("nested page");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-cayw-edit")
            .and_then(|value| value.to_str().ok()),
        Some("1")
    );
    let body = response.text().await.expect("page body");
    assert!(body.contains(NODE_ATTRIBUTE));
    assert!(body.contains("/__cayw_bridge.js?editor_origin="));
}

#[tokio::test(flavor = "multi_thread")]
async fn state_directory_alias_is_refused_before_creating_inside_the_project() {
    let project = tempfile::tempdir().expect("project");
    let _outside = write_fixture(project.path());
    let holder = tempfile::tempdir().expect("holder");
    let alias = holder.path().join("project-alias");
    symlink(project.path(), &alias).expect("project alias");
    let error = start(EditorConfig {
        project_root: project.path().to_path_buf(),
        page: "index.html".to_owned(),
        state_dir: alias.join("state"),
        editor_addr: "127.0.0.1:0".parse().expect("address"),
        preview_addr: "127.0.0.1:0".parse().expect("address"),
        token: TOKEN.to_owned(),
        framework: None,
        node: "node".into(),
        companion: cayw_visual_editor::default_companion(),
        codeintel: None,
    })
    .await;
    assert!(
        error.is_err(),
        "an aliased state path inside the project must fail"
    );
    assert!(
        !project.path().join("state").exists(),
        "the refused state directory must not be created inside the project"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn state_directory_inside_the_project_is_refused() {
    let project = tempfile::tempdir().expect("project");
    let _outside = write_fixture(project.path());
    let state = project.path().join("state");
    let error = start(EditorConfig {
        project_root: project.path().to_path_buf(),
        page: "index.html".to_owned(),
        state_dir: state,
        editor_addr: "127.0.0.1:0".parse().expect("address"),
        preview_addr: "127.0.0.1:0".parse().expect("address"),
        token: TOKEN.to_owned(),
        framework: None,
        node: "node".into(),
        companion: cayw_visual_editor::default_companion(),
        codeintel: None,
    })
    .await;
    assert!(error.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn retained_unknown_outcome_refuses_startup() {
    let project = tempfile::tempdir().expect("project");
    let _outside = write_fixture(project.path());
    let state = tempfile::tempdir().expect("state");
    fs::write(state.path().join("unknown.json"), "{}").expect("marker");
    let error = start(EditorConfig {
        project_root: project.path().to_path_buf(),
        page: "index.html".to_owned(),
        state_dir: state.path().to_path_buf(),
        editor_addr: "127.0.0.1:0".parse().expect("address"),
        preview_addr: "127.0.0.1:0".parse().expect("address"),
        token: TOKEN.to_owned(),
        framework: None,
        node: "node".into(),
        companion: cayw_visual_editor::default_companion(),
        codeintel: None,
    })
    .await;
    assert!(
        error.is_err(),
        "an unknown marker must never be cleared automatically"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn cli_prints_only_the_two_urls_and_serves_health() {
    let project = tempfile::tempdir().expect("project");
    let state = tempfile::tempdir().expect("state");
    let _outside = write_fixture(project.path());
    let token_env = format!("CAYW_TEST_EDITOR_TOKEN_{}", std::process::id());
    let mut child = Command::new(env!("CARGO_BIN_EXE_cayw-visual-editor"))
        .args([
            "--project",
            project.path().to_str().expect("project path"),
            "--page",
            "index.html",
            "--state-dir",
            state.path().to_str().expect("state path"),
            "--addr",
            "127.0.0.1:0",
            "--preview-addr",
            "127.0.0.1:0",
            "--token-env",
            &token_env,
        ])
        .env(&token_env, TOKEN)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn editor binary");
    let stdout = child.stdout.take().expect("stdout");
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).expect("startup line");
    let startup: Value = serde_json::from_str(line.trim()).expect("startup JSON");
    assert!(startup["editor_url"].as_str().is_some());
    assert!(startup["preview_url"].as_str().is_some());
    assert!(!line.contains(TOKEN), "startup line leaked the token");
    assert_eq!(line.trim().lines().count(), 1);

    let editor_url = startup["editor_url"].as_str().expect("editor url");
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("client");
    let response = client
        .get(format!("{}/api/health", editor_url.trim_end_matches('/')))
        .send()
        .await
        .expect("health");
    assert_eq!(response.status(), StatusCode::OK);

    let _ = child.kill();
    let _ = child.wait();
}

#[tokio::test(flavor = "multi_thread")]
async fn save_publishes_to_codeintel_bridge_and_preserves_write_outcome() {
    let project = tempfile::tempdir().expect("project tempdir");
    let state = tempfile::tempdir().expect("state tempdir");
    let outside = write_fixture(project.path());
    let bridge = cayw_codeintel::SymbolVersionBridge::new();
    let config = EditorConfig {
        project_root: project.path().to_path_buf(),
        page: "index.html".to_owned(),
        state_dir: state.path().to_path_buf(),
        editor_addr: "127.0.0.1:0".parse().expect("editor address"),
        preview_addr: "127.0.0.1:0".parse().expect("preview address"),
        token: TOKEN.to_owned(),
        framework: None,
        node: "node".into(),
        companion: cayw_visual_editor::default_companion(),
        codeintel: Some(bridge.clone()),
    };
    let handle = start(config).await.expect("editor starts");
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("client");
    let fixture = Fixture {
        project,
        _outside: outside,
        state,
        handle,
        client,
    };

    // Initially, no version is published in the shared bridge for index.html.
    assert!(bridge.file_version("index.html").unwrap().is_none());

    // Make an edit and save.
    let document = fixture.document().await;
    let original_sha = document["snapshot"]["sha256"]
        .as_str()
        .expect("sha")
        .to_owned();
    let heading = node_id(find_node(&document, "h1")).to_owned();
    fixture
        .operate(json!({"kind": "text", "id": heading, "text": "Bridge published heading 1"}))
        .await;

    let (status, saved) = fixture
        .post("/api/save", json!({"expected_sha256": original_sha}))
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert!(!saved["dirty"].as_bool().expect("dirty"));

    // Verify bridge has updated version and sha.
    let fv1 = bridge
        .file_version("index.html")
        .unwrap()
        .expect("file version after save 1");
    assert_eq!(fv1.version, 1);
    let disk_source = fixture.source();
    assert!(disk_source.contains("Bridge published heading 1"));
    let expected_sha1 = cayw_codeintel::content_sha256(disk_source.as_bytes());
    assert_eq!(fv1.content_sha256, expected_sha1);

    // Second edit and save: monotonic version increment.
    let document2 = fixture.document().await;
    let sha2 = document2["snapshot"]["sha256"]
        .as_str()
        .expect("sha")
        .to_owned();
    let heading2 = node_id(find_node(&document2, "h1")).to_owned();
    fixture
        .operate(json!({"kind": "text", "id": heading2, "text": "Bridge published heading 2"}))
        .await;

    let (status, saved2) = fixture
        .post("/api/save", json!({"expected_sha256": sha2}))
        .await;
    assert_eq!(status, StatusCode::OK, "{saved2}");

    let fv2 = bridge
        .file_version("index.html")
        .unwrap()
        .expect("file version after save 2");
    assert_eq!(fv2.version, 2);
    let disk_source2 = fixture.source();
    assert!(disk_source2.contains("Bridge published heading 2"));
    let expected_sha2 = cayw_codeintel::content_sha256(disk_source2.as_bytes());
    assert_eq!(fv2.content_sha256, expected_sha2);
}
