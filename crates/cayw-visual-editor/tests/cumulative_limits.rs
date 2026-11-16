use cayw_visual_editor::audit::AuditStore;
#[test]
fn settled_audit_over_64_mib_can_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let mut file = std::fs::File::create(dir.path().join("calls.jsonl")).unwrap();
    use std::io::Write;
    let padding = " ".repeat(1023) + "\n";
    for _ in 0..65537 {
        file.write_all(padding.as_bytes()).unwrap();
    }
    file.write_all(b"{\"event\":\"started\",\"call_id\":\"done\"}\n{\"event\":\"settled\",\"call_id\":\"done\"}\n").unwrap();
    drop(file);
    let audit = AuditStore::open(dir.path()).unwrap();
    assert!(!audit.unknown());
    assert!(!dir.path().join("unknown.json").exists());
}

#[tokio::test]
async fn html_history_retains_over_100_edits_and_32_mib() {
    use cayw_visual_editor::project::Project;
    use serde_json::json;
    let root = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let source = format!("<!--{}--><p>initial</p>", "x".repeat(330 * 1024));
    std::fs::write(root.path().join("index.html"), &source).unwrap();
    let project = Project::open(root.path(), "index.html", state.path())
        .await
        .unwrap();
    for n in 0..105 {
        let current = project.snapshot().await.unwrap();
        let id = &current
            .nodes
            .iter()
            .find(|node| node.tag == "p")
            .unwrap()
            .id;
        project
            .apply(json!({"kind":"text", "id":id,"text":n.to_string()}))
            .await
            .unwrap();
    }
    for _ in 0..105 {
        project.apply(json!({"kind":"undo"})).await.unwrap();
    }
    assert!(!project.dirty().await);
    assert_eq!(project.history_flags().await, (false, true));
    for _ in 0..105 {
        project.apply(json!({"kind":"redo"})).await.unwrap();
    }
    let current = project.snapshot().await.unwrap();
    assert!(current.html.contains(">104</p>"));
    project.save(&current.sha256).await.unwrap();
    assert!(
        std::fs::read_to_string(root.path().join("index.html"))
            .unwrap()
            .contains(">104</p>")
    );
}

fn config(
    root: &std::path::Path,
    state: &std::path::Path,
    framework: Option<&str>,
) -> cayw_visual_editor::EditorConfig {
    cayw_visual_editor::EditorConfig {
        project_root: root.into(),
        page: "index.html".into(),
        state_dir: state.into(),
        editor_addr: "127.0.0.1:0".parse().unwrap(),
        preview_addr: "127.0.0.1:0".parse().unwrap(),
        token: "limits-test-token-01234567890123456789".into(),
        framework: framework.map(str::to_owned),
        node: "node".into(),
        companion: std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("framework/companion.mjs"),
        codeintel: None,
    }
}

#[tokio::test]
async fn html_over_8_mib_opens_and_serves_the_complete_preview() {
    let root = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let source = format!("<!--{}--><p>large</p>", "x".repeat(9 * 1024 * 1024));
    std::fs::write(root.path().join("index.html"), &source).unwrap();
    let project =
        cayw_visual_editor::project::Project::open(root.path(), "index.html", state.path())
            .await
            .unwrap();
    let snapshot = project.snapshot().await.unwrap();
    let id = &snapshot
        .nodes
        .iter()
        .find(|node| node.tag == "p")
        .unwrap()
        .id;
    let edited = project
        .apply(serde_json::json!({"kind":"text", "id":id, "text":"saved large page"}))
        .await
        .unwrap();
    project.save(&edited.sha256).await.unwrap();
    drop(project);
    let source = std::fs::read_to_string(root.path().join("index.html")).unwrap();
    assert!(source.ends_with("<p>saved large page</p>"));
    let handle = cayw_visual_editor::start(config(root.path(), state.path(), None))
        .await
        .unwrap();
    let response = reqwest::get(&handle.normal_preview_url).await.unwrap();
    assert!(response.status().is_success());
    assert_eq!(response.text().await.unwrap(), source);
    handle.shutdown().await;
}

#[tokio::test]
async fn framework_over_256_files_16_mib_total_and_8_mib_per_file_opens() {
    let root = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("index.html"),
        "<html><body><p>large project</p></body></html>",
    )
    .unwrap();
    for n in 0..257 {
        std::fs::write(
            root.path().join(format!("part{n}.jsx")),
            format!("export const part{n} = {n};"),
        )
        .unwrap();
    }
    let source = format!(
        "/*{}*/\nexport const large = 1;",
        "x".repeat(9 * 1024 * 1024)
    );
    for name in ["large-a.jsx", "large-b.jsx"] {
        std::fs::write(root.path().join(name), &source).unwrap();
    }
    let conf = config(root.path(), state.path(), Some("react"));
    let token = conf.token.clone();
    let handle = cayw_visual_editor::start(conf).await.unwrap();
    let response = reqwest::Client::new()
        .get(format!("{}api/document", handle.editor_url))
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["snapshot"]["files"].as_array().unwrap().len(), 260);
    assert!(
        body["snapshot"]["files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|file| file["path"] == "large-b.jsx")
    );
    handle.shutdown().await;
}

fn legacy_marker(dir: &std::path::Path) {
    std::fs::write(dir.join("unknown.json"), serde_json::json!({
        "code":"reconciliation_required", "message":"audit file exceeds the bounded size; reconcile it before editing", "at_ms":1
    }).to_string()).unwrap();
}

#[tokio::test]
async fn legacy_size_marker_is_repaired_only_after_both_logs_are_settled() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("index.html"), "<p>page</p>").unwrap();
    let calls = "{\"event\":\"started\",\"call_id\":\"one\"}\n{\"event\":\"settled\",\"call_id\":\"one\"}\n";
    for (server_log, succeeds) in [
        (
            "{\"event\":\"started\",\"request\":\"r\"}\n{\"event\":\"settled\",\"request\":\"r\",\"outcome_unknown\":false}\n",
            true,
        ),
        ("{\"event\":\"started\",\"request\":\"r\"}\n", false),
        (
            "{\"event\":\"started\",\"request\":\"r\"}\n{\"event\":\"settled\",\"request\":\"r\",\"outcome_unknown\":true}\n",
            false,
        ),
    ] {
        let state = tempfile::tempdir().unwrap();
        std::fs::write(state.path().join("calls.jsonl"), calls).unwrap();
        std::fs::write(state.path().join("server-events.jsonl"), server_log).unwrap();
        legacy_marker(state.path());
        match cayw_visual_editor::start(config(root.path(), state.path(), None)).await {
            Ok(handle) => {
                assert!(succeeds);
                assert!(!state.path().join("unknown.json").exists());
                handle.shutdown().await;
            }
            Err(_) => {
                assert!(!succeeds);
                assert!(state.path().join("unknown.json").exists());
            }
        }
    }
}

#[test]
fn legacy_size_marker_does_not_clear_missing_corrupt_or_unknown_call_evidence() {
    for log in [
        None,
        Some("broken"),
        Some("{\"event\":\"started\",\"call_id\":\"one\"}\n"),
        Some(
            "{\"event\":\"started\",\"call_id\":\"one\"}\n{\"event\":\"settled\",\"call_id\":\"one\",\"outcome_unknown\":true}\n",
        ),
    ] {
        let state = tempfile::tempdir().unwrap();
        if let Some(log) = log {
            std::fs::write(state.path().join("calls.jsonl"), log).unwrap();
        }
        legacy_marker(state.path());
        assert!(AuditStore::open(state.path()).is_err());
        assert!(AuditStore::open(state.path()).is_err());
        if log.is_none() {
            assert!(!state.path().join("calls.jsonl").exists());
        }
        assert!(state.path().join("unknown.json").exists());
    }
}

#[tokio::test]
async fn preview_streams_large_resources_and_accepts_deep_long_paths() {
    let root = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("index.html"), "<p>resource test</p>").unwrap();
    let payload = "x".repeat(9 * 1024 * 1024);
    std::fs::write(root.path().join("big.js"), &payload).unwrap();
    let relative = (0..17)
        .map(|_| "long-directory-name-0123456789")
        .collect::<Vec<_>>()
        .join("/")
        + "/a.js";
    let path = root.path().join(&relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, "deep resource").unwrap();
    assert!(relative.len() > 512);
    let page = relative.replace("a.js", "page.html");
    std::fs::write(root.path().join(&page), "<p>deep page</p>").unwrap();
    let page_state = tempfile::tempdir().unwrap();
    let project = cayw_visual_editor::project::Project::open(root.path(), &page, page_state.path())
        .await
        .unwrap();
    assert!(
        project
            .snapshot()
            .await
            .unwrap()
            .source
            .contains("deep page")
    );

    let handle = cayw_visual_editor::start(config(root.path(), state.path(), None))
        .await
        .unwrap();
    let base = &handle.preview_origin;
    for (path, expected) in [
        ("big.js", payload.as_str()),
        (relative.as_str(), "deep resource"),
    ] {
        let response = reqwest::get(format!("{base}/{path}")).await.unwrap();
        assert!(response.status().is_success());
        let actual = response.text().await.unwrap();
        assert_eq!(actual.len(), expected.len());
        assert!(actual == expected);
    }
    handle.shutdown().await;
}

#[tokio::test]
async fn html_edits_preserve_large_text_attributes_and_style_sets() {
    use cayw_visual_editor::project::Project;
    use serde_json::json;
    let root = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("index.html"),
        "<p>text</p><img src=\"a.png\">",
    )
    .unwrap();
    let project = Project::open(root.path(), "index.html", state.path())
        .await
        .unwrap();
    let text = "字".repeat(100000);
    let snapshot = project.snapshot().await.unwrap();
    let id = &snapshot.nodes.iter().find(|n| n.tag == "p").unwrap().id;
    let snapshot = project
        .apply(json!({"kind":"text","id":id,"text":text}))
        .await
        .unwrap();
    assert!(snapshot.source.contains(&text));
    let id = &snapshot.nodes.iter().find(|n| n.tag == "img").unwrap().id;
    let data = format!("data:image/png;base64,{}", "A".repeat(70000));
    let snapshot = project
        .apply(json!({"kind":"attribute","id":id,"name":"src","value":data}))
        .await
        .unwrap();
    assert!(snapshot.source.contains(&data));
    let id = &snapshot.nodes.iter().find(|n| n.tag == "p").unwrap().id;
    let properties: serde_json::Map<String, serde_json::Value> = (0..40)
        .map(|n| (format!("--property-{n}"), json!("x".repeat(600))))
        .collect();
    let snapshot = project
        .apply(json!({"kind":"style","id":id,"properties":properties}))
        .await
        .unwrap();
    assert!(snapshot.source.contains("--property-39"));
}

#[tokio::test]
async fn legacy_marker_with_missing_server_log_stays_blocked_on_repeated_start() {
    let root = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("index.html"), "<p>page</p>").unwrap();
    std::fs::write(state.path().join("calls.jsonl"), "").unwrap();
    legacy_marker(state.path());
    for _ in 0..2 {
        assert!(
            cayw_visual_editor::start(config(root.path(), state.path(), None))
                .await
                .is_err()
        );
        assert!(state.path().join("unknown.json").exists());
        assert!(!state.path().join("server-events.jsonl").exists());
    }
}
