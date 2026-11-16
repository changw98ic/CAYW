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
