//! Source-engine tests for the visual editor.
//!
//! Every test opens a real project through `Project::open`, so reads and writes
//! pass through the public `cayw-host` SDK and its page permission gate. Only
//! local temporary pages are used; no network or model provider is contacted.

use cayw_visual_editor::project::{
    COMPONENT_NAMES, NODE_ATTRIBUTE, Project, ProjectError, READONLY_ATTRIBUTE,
    READONLY_REASON_ATTRIBUTE, Snapshot,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
};
use tempfile::TempDir;

const BASE_PAGE: &str = r#"<!doctype html>
<html lang="en">
<head><meta charset="utf-8"><title>Engine fixture</title></head>
<body>
<main id="app">
  <p class="lead" style="display: grid; color: red; --x: 1">Hello</p>
  <h2>Section</h2>
  <img src="/logo.svg" alt="Logo" width="64" height="64">
  <button type="button" onclick="alert(1)">Run</button>
  <div class="cards"><article><h3>A</h3><p>A body</p></article></div>
  <input name="name" placeholder="Name">
</main>
<script src="/script.js"></script>
</body>
</html>
"#;

struct Fixture {
    root: TempDir,
    state: TempDir,
}

impl Fixture {
    fn new(page: &str) -> Self {
        let root = tempfile::tempdir().expect("project tempdir");
        let state = tempfile::tempdir().expect("state tempdir");
        fs::write(root.path().join("index.html"), page).expect("write fixture page");
        Self { root, state }
    }

    async fn open(&self) -> Project {
        Project::open(self.root.path(), "index.html", self.state.path())
            .await
            .expect("open project")
    }

    fn page_path(&self) -> PathBuf {
        self.root.path().join("index.html")
    }

    fn read(&self) -> String {
        fs::read_to_string(self.page_path()).expect("read fixture page")
    }

    fn write(&self, content: &str) {
        fs::write(self.page_path(), content).expect("write fixture page");
    }
}

fn sha256(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

fn first<'a>(snapshot: &'a Snapshot, tag: &str) -> &'a cayw_visual_editor::SourceNode {
    snapshot
        .nodes
        .iter()
        .find(|node| node.tag == tag)
        .unwrap_or_else(|| panic!("no <{tag}> node"))
}

fn editable<'a>(snapshot: &'a Snapshot, tag: &str) -> &'a cayw_visual_editor::SourceNode {
    snapshot
        .nodes
        .iter()
        .find(|node| node.tag == tag && node.editable)
        .unwrap_or_else(|| panic!("no editable <{tag}> node"))
}

fn tagged<'a>(snapshot: &'a Snapshot, tag: &str, nth: usize) -> &'a cayw_visual_editor::SourceNode {
    snapshot
        .nodes
        .iter()
        .filter(|node| node.tag == tag)
        .nth(nth)
        .unwrap_or_else(|| panic!("no <{tag}> node at index {nth}"))
}

fn node<'a>(snapshot: &'a Snapshot, id: &str) -> &'a cayw_visual_editor::SourceNode {
    snapshot
        .nodes
        .iter()
        .find(|node| node.id == id)
        .unwrap_or_else(|| panic!("no node {id}"))
}

fn code(error: &ProjectError) -> &str {
    &error.code
}

async fn operation_error(project: &Project, value: Value) -> ProjectError {
    project.apply(value).await.expect_err("operation must fail")
}

#[tokio::test]
async fn snapshot_maps_exact_elements_and_injects_only_the_response() {
    let fixture = Fixture::new(BASE_PAGE);
    let project = fixture.open().await;
    let snapshot = project.snapshot().await.expect("snapshot");

    assert_eq!(NODE_ATTRIBUTE, "data-cayw-node");
    assert_eq!(snapshot.path, "index.html");
    assert_eq!(snapshot.sha256, sha256(BASE_PAGE));
    assert_eq!(
        fixture.read(),
        BASE_PAGE,
        "opening must not rewrite the page"
    );
    assert!(!fixture.read().contains(NODE_ATTRIBUTE));
    assert!(snapshot.html.contains(NODE_ATTRIBUTE));
    assert_eq!(
        snapshot.source, BASE_PAGE,
        "read-only viewers get the uninstrumented draft"
    );

    let lead = editable(&snapshot, "p");
    assert!(lead.line >= 6);
    assert_eq!(
        node(&snapshot, lead.parent.as_deref().expect("parent")).tag,
        "main"
    );
    assert_eq!(lead.reason, None);
    assert!(
        snapshot
            .html
            .contains(&format!("{NODE_ATTRIBUTE}=\"{}\"", lead.id))
    );

    let script = first(&snapshot, "script");
    assert!(!script.editable);
    assert!(script.reason.is_some());
    assert!(
        !snapshot
            .html
            .contains(&format!("{NODE_ATTRIBUTE}=\"{}\"", script.id))
    );
    assert!(snapshot.html.contains("src=\"/script.js\""));

    let input = editable(&snapshot, "input");
    assert!(
        input.editable,
        "void elements are mapped but cannot hold text"
    );
}

#[tokio::test]
async fn read_only_nodes_carry_response_only_markers_and_spoofing_is_removed() {
    let page = concat!(
        "<!doctype html><html><body><main>",
        "<div>{{ item }}</div>",
        "<p>editable</p>",
        "<p data-cayw-readonly=\"true\" data-cayw-reason=\"fake\">spoof</p>",
        "</main></body></html>"
    );
    let fixture = Fixture::new(page);
    let project = fixture.open().await;
    let snapshot = project.snapshot().await.expect("snapshot");

    let dynamic = tagged(&snapshot, "div", 0);
    assert!(!dynamic.editable);
    let marker = format!("{NODE_ATTRIBUTE}=\"{}\"", dynamic.id);
    let at = snapshot.html.find(&marker).expect("dynamic mapping");
    let start = snapshot.html[..at].rfind('<').expect("tag start");
    let end = snapshot.html[at..].find('>').expect("tag end") + at;
    let tag = &snapshot.html[start..=end];
    assert!(
        tag.contains(&format!("{READONLY_ATTRIBUTE}=\"true\"")),
        "{tag}"
    );
    assert!(
        tag.contains(&format!("{READONLY_REASON_ATTRIBUTE}=\"dynamic_region\"")),
        "{tag}"
    );

    let paragraph = tagged(&snapshot, "p", 0);
    let marker = format!("{NODE_ATTRIBUTE}=\"{}\"", paragraph.id);
    let at = snapshot.html.find(&marker).expect("paragraph mapping");
    let start = snapshot.html[..at].rfind('<').expect("tag start");
    let end = snapshot.html[at..].find('>').expect("tag end") + at;
    let tag = &snapshot.html[start..=end];
    assert!(
        !tag.contains(READONLY_ATTRIBUTE),
        "editable nodes must not be marked read-only: {tag}"
    );

    assert!(
        !snapshot.html.contains("fake"),
        "a page-supplied reason must be removed: {}",
        snapshot.html
    );
    assert_eq!(
        fixture.read(),
        page,
        "instrumentation must not change the source on disk"
    );
    assert_eq!(
        snapshot.html.matches(READONLY_ATTRIBUTE).count(),
        1,
        "only the dynamic node is marked read-only after the page-supplied copy is removed"
    );
    assert_eq!(snapshot.selected_id, None);
}

#[tokio::test]
async fn start_tag_insert_does_not_treat_unquoted_trailing_slashes_as_self_closing() {
    let page = "<!doctype html><html><body><main><a href=/docs/>Docs</a><img src=\"/logo.svg\"/></main></body></html>";
    let fixture = Fixture::new(page);
    let project = fixture.open().await;
    let snapshot = project.snapshot().await.expect("snapshot");

    assert!(
        snapshot.html.contains("href=/docs/ data-cayw-node=\""),
        "unquoted href must keep its trailing slash: {}",
        snapshot.html
    );
    assert!(
        !snapshot.html.contains("href=/docs data-cayw-node="),
        "the trailing slash was moved out of the attribute value: {}",
        snapshot.html
    );
    assert!(
        snapshot
            .html
            .contains("src=\"/logo.svg\" data-cayw-node=\""),
        "a real self-closing tag keeps its mapping before the slash: {}",
        snapshot.html
    );
    assert_eq!(fixture.read(), page, "instrumentation must not touch disk");
}

#[tokio::test]
async fn text_edits_escape_markup_and_support_undo_and_redo() {
    let fixture = Fixture::new(BASE_PAGE);
    let project = fixture.open().await;
    let before = project.snapshot().await.expect("snapshot");
    let id = editable(&before, "p").id.clone();

    let edited = project
        .apply(json!({"kind": "text", "id": id, "text": "New <text> & more"}))
        .await
        .expect("text edit");
    assert!(edited.html.contains("New &lt;text&gt; &amp; more"));
    assert_eq!(edited.sha256, before.sha256, "sha is the source revision");
    assert!(project.dirty().await);
    assert_eq!(project.history_flags().await, (true, false));

    let undone = project.apply(json!({"kind": "undo"})).await.expect("undo");
    assert!(undone.html.contains(">Hello</p>"));
    assert_eq!(undone.sha256, before.sha256);
    assert!(!project.dirty().await);
    assert_eq!(project.history_flags().await, (false, true));

    let redone = project.apply(json!({"kind": "redo"})).await.expect("redo");
    assert!(redone.html.contains("New &lt;text&gt; &amp; more"));
    assert_eq!(project.history_flags().await, (true, false));

    project.apply(json!({"kind": "undo"})).await.expect("undo");
    let error = operation_error(&project, json!({"kind": "undo"})).await;
    assert_eq!(code(&error), "history_empty");
}

#[tokio::test]
async fn style_edits_merge_declarations_and_reject_unsafe_values() {
    let fixture = Fixture::new(BASE_PAGE);
    let project = fixture.open().await;
    let snapshot = project.snapshot().await.expect("snapshot");
    let id = editable(&snapshot, "p").id.clone();

    let styled = project
        .apply(json!({
            "kind": "style",
            "id": id,
            "properties": {"color": "blue", "gap": "8px"}
        }))
        .await
        .expect("style edit");
    assert!(
        styled
            .html
            .contains("display: grid; color: blue; --x: 1; gap: 8px")
    );

    let original = project.snapshot().await.expect("snapshot");
    let id = editable(&original, "p").id.clone();
    for (properties, expected) in [
        (
            json!({"background-color": "url(http://example.test/x)"}),
            "invalid_style",
        ),
        (json!({"animation": "spin 1s"}), "invalid_style"),
        (json!({"color": "red; position: fixed"}), "invalid_style"),
        (json!({}), "invalid_style"),
        (json!({"color": 12}), "invalid_style"),
    ] {
        let error = operation_error(
            &project,
            json!({"kind": "style", "id": id, "properties": properties}),
        )
        .await;
        assert_eq!(code(&error), expected, "{properties}");
    }

    let error = operation_error(
        &project,
        json!({"kind": "style", "id": "missing", "properties": {"color": "red"}}),
    )
    .await;
    assert_eq!(code(&error), "not_found");
}

#[tokio::test]
async fn style_edits_preserve_custom_property_case_and_var_references() {
    let page = "<!doctype html><html><body><main><p style=\"--BrandColor: #123456; background-color: var(--BrandColor); color: red\">x</p></main></body></html>";
    let fixture = Fixture::new(page);
    let project = fixture.open().await;
    let snapshot = project.snapshot().await.expect("snapshot");
    let id = editable(&snapshot, "p").id.clone();

    let styled = project
        .apply(json!({"kind": "style", "id": id, "properties": {"color": "blue"}}))
        .await
        .expect("standard property update");
    assert!(
        styled.html.contains("--BrandColor: #123456"),
        "{}",
        styled.html
    );
    assert!(styled.html.contains("var(--BrandColor)"), "{}", styled.html);
    assert!(!styled.html.contains("--brandcolor"), "{}", styled.html);
    let selected = styled.selected_id.clone().expect("style selection");
    assert_eq!(node(&styled, &selected).tag, "p");

    let id = editable(&styled, "p").id.clone();
    let styled = project
        .apply(json!({"kind": "style", "id": id, "properties": {"--BrandColor": "#abcdef"}}))
        .await
        .expect("custom property update");
    assert!(
        styled.html.contains("--BrandColor: #abcdef"),
        "{}",
        styled.html
    );
    assert!(styled.html.contains("var(--BrandColor)"), "{}", styled.html);
    let selected = styled.selected_id.clone().expect("custom selection");
    assert_eq!(node(&styled, &selected).tag, "p");
}

#[tokio::test]
async fn attribute_edits_are_allowlisted_and_keep_event_attributes() {
    let fixture = Fixture::new(BASE_PAGE);
    let project = fixture.open().await;
    let snapshot = project.snapshot().await.expect("snapshot");
    let image = editable(&snapshot, "img").id.clone();

    let changed = project
        .apply(json!({
            "kind": "attribute",
            "id": image,
            "name": "src",
            "value": "data:image/png;base64,AAAA"
        }))
        .await
        .expect("image src edit");
    assert!(changed.html.contains("src=\"data:image/png;base64,AAAA\""));

    let button = editable(&changed, "button").id.clone();
    let changed = project
        .apply(json!({
            "kind": "attribute",
            "id": button,
            "name": "aria-label",
            "value": "Run \"safely\" & fast <now>"
        }))
        .await
        .expect("aria label edit");
    assert!(
        changed
            .html
            .contains("aria-label=\"Run &quot;safely&quot; &amp; fast &lt;now&gt;\"")
    );

    let button = editable(&changed, "button").id.clone();
    for (name, value, expected) in [
        ("onclick", "alert(2)", "invalid_attribute"),
        ("style", "color:red", "invalid_attribute"),
        ("data-anything", "1", "invalid_attribute"),
        ("href", "javascript:alert(1)", "invalid_value"),
        ("for", "not a valid id", "invalid_value"),
    ] {
        let error = operation_error(
            &project,
            json!({"kind": "attribute", "id": button, "name": name, "value": value}),
        )
        .await;
        assert_eq!(code(&error), expected, "{name}");
    }

    let text = project
        .apply(json!({"kind": "text", "id": button, "text": "Go"}))
        .await
        .expect("button text");
    assert!(text.html.contains(">Go</button>"));
    assert!(
        text.html.contains("onclick=\"alert(1)\""),
        "editing text must not drop event attributes"
    );
}

#[tokio::test]
async fn every_component_can_be_inserted_and_image_uses_local_data() {
    let fixture = Fixture::new(BASE_PAGE);
    let project = fixture.open().await;

    for name in COMPONENT_NAMES {
        let current = project.snapshot().await.expect("snapshot");
        let main = editable(&current, "main").id.clone();
        let snapshot = project
            .apply(json!({
                "kind": "insert",
                "target": main,
                "placement": "inside",
                "component": name
            }))
            .await
            .unwrap_or_else(|error| panic!("insert {name}: {error}"));
        assert!(
            snapshot
                .html
                .contains(&format!("data-cayw-component=\"{name}\"")),
            "missing component {name}"
        );
    }

    let snapshot = project.snapshot().await.expect("snapshot");
    assert!(
        snapshot.html.contains("data:image/svg+xml"),
        "inserted images must use the local component asset"
    );
    assert!(snapshot.html.contains("data-cayw-motion"));

    let main = editable(&snapshot, "main").id.clone();
    let error = operation_error(
        &project,
        json!({"kind": "insert", "target": main, "placement": "inside", "component": "iframe"}),
    )
    .await;
    assert_eq!(code(&error), "invalid_operation");
}

#[tokio::test]
async fn insert_and_move_respect_source_order_and_container_rules() {
    let fixture = Fixture::new(
        "<!doctype html><html><body><main><p id=\"a\">A</p><h2>B</h2></main></body></html>",
    );
    let project = fixture.open().await;
    let snapshot = project.snapshot().await.expect("snapshot");
    let paragraph = editable(&snapshot, "p").id.clone();
    let heading = editable(&snapshot, "h2").id.clone();

    let moved = project
        .apply(json!({
            "kind": "move",
            "id": paragraph,
            "target": heading,
            "placement": "after"
        }))
        .await
        .expect("move");
    assert!(
        moved.html.find("<h2").expect("heading")
            < moved.html.find("<p id=\"a\"").expect("paragraph")
    );

    // Reparsing assigns ids by source order, so take fresh ids after the move.
    let heading = editable(&moved, "h2").id.clone();
    let before = project
        .apply(json!({
            "kind": "insert",
            "target": heading,
            "placement": "before",
            "component": "text"
        }))
        .await
        .expect("insert before");
    assert!(
        before
            .html
            .find("data-cayw-component=\"text\"")
            .expect("component")
            < before.html.find("<h2").expect("heading")
    );

    let heading = editable(&before, "h2").id.clone();
    let error = operation_error(
        &project,
        json!({"kind": "insert", "target": heading, "placement": "around", "component": "text"}),
    )
    .await;
    assert_eq!(code(&error), "invalid_operation");

    let paragraph = editable(&project.snapshot().await.expect("snapshot"), "p")
        .id
        .clone();
    let error = operation_error(
        &project,
        json!({"kind": "insert", "target": paragraph, "placement": "inside", "component": "text"}),
    )
    .await;
    assert_eq!(code(&error), "unsupported_target");
}

#[tokio::test]
async fn inside_placement_rejects_an_implicitly_closed_descendant() {
    let page =
        "<!doctype html><html><body><main><div><span>x</div><p>after</p></main></body></html>";
    let fixture = Fixture::new(page);
    let project = fixture.open().await;
    let snapshot = project.snapshot().await.expect("snapshot");
    let container = editable(&snapshot, "div").id.clone();
    let paragraph = editable(&snapshot, "p").id.clone();

    let error = operation_error(
        &project,
        json!({"kind": "insert", "target": container, "placement": "inside", "component": "text"}),
    )
    .await;
    assert_eq!(code(&error), "ambiguous_target");

    let error = operation_error(
        &project,
        json!({"kind": "move", "id": paragraph, "target": container, "placement": "inside"}),
    )
    .await;
    assert_eq!(code(&error), "ambiguous_target");
    assert_eq!(fixture.read(), page);
}

#[tokio::test]
async fn table_structures_reject_generic_components_but_cells_accept_them() {
    let page = concat!(
        "<!doctype html><html><body><main><p>outside</p>",
        "<table><thead><tr><th>H</th></tr></thead>",
        "<tbody><tr><td>A</td><td>B</td></tr></tbody></table></main></body></html>"
    );
    let fixture = Fixture::new(page);
    let project = fixture.open().await;
    let snapshot = project.snapshot().await.expect("snapshot");

    for tag in ["table", "thead", "tbody", "tr"] {
        let target = first(&snapshot, tag).id.clone();
        let error = operation_error(
            &project,
            json!({"kind": "insert", "target": target, "placement": "inside", "component": "card"}),
        )
        .await;
        assert_eq!(code(&error), "unsupported_target", "{tag}");
    }

    let cell = editable(&snapshot, "td").id.clone();
    let inserted = project
        .apply(
            json!({"kind": "insert", "target": cell, "placement": "inside", "component": "card"}),
        )
        .await
        .expect("a table cell accepts flow content");
    assert!(inserted.html.contains("data-cayw-component=\"card\""));

    let cell = first(&inserted, "td").id.clone();
    let error = operation_error(
        &project,
        json!({"kind": "insert", "target": cell, "placement": "before", "component": "card"}),
    )
    .await;
    assert_eq!(code(&error), "unsupported_target");

    let paragraph = editable(&inserted, "p").id.clone();
    let row = first(&inserted, "tr").id.clone();
    let error = operation_error(
        &project,
        json!({"kind": "move", "id": paragraph, "target": row, "placement": "inside"}),
    )
    .await;
    assert_eq!(code(&error), "unsupported_target");
}

#[tokio::test]
async fn move_accepts_protected_inside_targets_but_protects_the_source() {
    let page = concat!(
        "<!doctype html><html><body><div id=\"outside\">x</div>",
        "<main><p>y</p></main></body></html>"
    );
    let fixture = Fixture::new(page);
    let project = fixture.open().await;
    let snapshot = project.snapshot().await.expect("snapshot");
    let outside = editable(&snapshot, "div").id.clone();
    let main = editable(&snapshot, "main").id.clone();

    let moved = project
        .apply(json!({"kind": "move", "id": outside, "target": main, "placement": "inside"}))
        .await
        .expect("main accepts inside placement");
    let selected = moved.selected_id.as_deref().expect("moved selection");
    assert_eq!(node(&moved, selected).tag, "div");
    assert!(
        moved.html.contains("id=\"outside\""),
        "the moved element keeps its content: {}",
        moved.html
    );
    assert!(
        moved.html.find("<main").expect("main")
            < moved.html.find("id=\"outside\"").expect("moved div"),
        "{}",
        moved.html
    );

    let snapshot = project.snapshot().await.expect("snapshot");
    let main = editable(&snapshot, "main").id.clone();
    let body = editable(&snapshot, "body").id.clone();
    let error = operation_error(
        &project,
        json!({"kind": "move", "id": main, "target": body, "placement": "inside"}),
    )
    .await;
    assert_eq!(code(&error), "unsupported_target");

    let paragraph = editable(&snapshot, "p").id.clone();
    let moved = project
        .apply(json!({"kind": "move", "id": paragraph, "target": body, "placement": "inside"}))
        .await
        .expect("body accepts inside placement");
    assert_eq!(
        node(&moved, moved.selected_id.as_deref().expect("selection")).tag,
        "p"
    );
}

#[tokio::test]
async fn duplicate_delete_and_protected_structures_are_checked() {
    let fixture = Fixture::new(
        "<!doctype html><html><body><main><p id=\"a\">A</p><h2>B</h2></main></body></html>",
    );
    let project = fixture.open().await;
    let snapshot = project.snapshot().await.expect("snapshot");
    let heading = editable(&snapshot, "h2").id.clone();

    let duplicated = project
        .apply(json!({"kind": "duplicate", "id": heading}))
        .await
        .expect("duplicate heading");
    assert_eq!(duplicated.html.matches(">B</h2>").count(), 2);

    let paragraph = editable(&duplicated, "p").id.clone();
    let error = operation_error(&project, json!({"kind": "duplicate", "id": paragraph})).await;
    assert_eq!(code(&error), "unsupported_target");

    let deleted = project
        .apply(json!({"kind": "delete", "id": paragraph}))
        .await
        .expect("delete paragraph");
    assert!(!deleted.html.contains(">A</p>"));

    let main = editable(&deleted, "main").id.clone();
    let error = operation_error(&project, json!({"kind": "delete", "id": main})).await;
    assert_eq!(code(&error), "unsupported_target");
    let error = operation_error(&project, json!({"kind": "duplicate", "id": main})).await;
    assert_eq!(code(&error), "unsupported_target");
}

#[tokio::test]
async fn an_element_cannot_move_inside_its_own_subtree() {
    let fixture = Fixture::new(
        "<!doctype html><html><body><main><div id=\"outer\"><p id=\"inner\">x</p></div><h2>y</h2></main></body></html>",
    );
    let project = fixture.open().await;
    let snapshot = project.snapshot().await.expect("snapshot");
    let outer = editable(&snapshot, "div").id.clone();
    let inner = editable(&snapshot, "p").id.clone();

    let error = operation_error(
        &project,
        json!({"kind": "move", "id": outer, "target": inner, "placement": "after"}),
    )
    .await;
    assert_eq!(code(&error), "unsupported_target");

    let error = operation_error(
        &project,
        json!({"kind": "move", "id": outer, "target": outer, "placement": "before"}),
    )
    .await;
    assert_eq!(code(&error), "unsupported_target");
}

#[tokio::test]
async fn dynamic_ambiguous_and_internal_regions_are_read_only() {
    let page = concat!(
        "<!doctype html><html><head><title>T</title><script>var x = \"<p>\";</script>",
        "<style>.a{color:red}</style></head><body><main>",
        "<div>{{ item }}</div>",
        "<p data-bind=\"name\">Bound</p>",
        "<p data-cayw-node=\"manual\">Reserved</p>",
        "<div><span>nested</span></div>",
        "</main></body></html>"
    );
    let fixture = Fixture::new(page);
    let project = fixture.open().await;
    let snapshot = project.snapshot().await.expect("snapshot");

    let dynamic = tagged(&snapshot, "div", 0);
    assert!(!dynamic.editable);
    assert_eq!(dynamic.reason.as_deref(), Some("dynamic_region"));
    let bound = tagged(&snapshot, "p", 0);
    assert_eq!(bound.reason.as_deref(), Some("dynamic_region"));
    let reserved = tagged(&snapshot, "p", 1);
    assert_eq!(reserved.reason.as_deref(), Some("reserved_attribute"));
    assert!(!snapshot.html.contains("data-cayw-node=\"manual\""));

    for tag in ["script", "style", "title"] {
        let node = first(&snapshot, tag);
        assert!(!node.editable, "{tag} must not be editable");
        assert!(
            !snapshot
                .html
                .contains(&format!("{NODE_ATTRIBUTE}=\"{}\"", node.id))
        );
    }

    for id in [dynamic.id.clone(), bound.id.clone()] {
        let error = operation_error(
            &project,
            json!({"kind": "text", "id": id, "text": "changed"}),
        )
        .await;
        assert_eq!(code(&error), "not_editable");
    }

    let wrapper = tagged(&snapshot, "div", 1);
    assert!(wrapper.editable);
    let error = operation_error(
        &project,
        json!({"kind": "text", "id": wrapper.id, "text": "changed"}),
    )
    .await;
    assert_eq!(code(&error), "unsupported_target");

    let leaf = tagged(&snapshot, "span", 0);
    let error = operation_error(
        &project,
        json!({"kind": "text", "id": leaf.id, "text": "bad\u{7}text"}),
    )
    .await;
    assert_eq!(code(&error), "invalid_value");
}

#[tokio::test]
async fn optional_end_tags_mark_the_affected_region_ambiguous() {
    let fixture =
        Fixture::new("<!doctype html><html><body><main><p>one<p>two</main></body></html>");
    let project = fixture.open().await;
    let snapshot = project.snapshot().await.expect("snapshot");
    let first_paragraph = tagged(&snapshot, "p", 0);
    let second_paragraph = tagged(&snapshot, "p", 1);
    assert!(!first_paragraph.editable);
    assert!(!second_paragraph.editable);
    assert_eq!(
        first_paragraph.reason.as_deref(),
        Some("ambiguous_structure")
    );
    assert_eq!(
        second_paragraph.reason.as_deref(),
        Some("ambiguous_structure")
    );

    let error = operation_error(
        &project,
        json!({"kind": "text", "id": second_paragraph.id, "text": "changed"}),
    )
    .await;
    assert_eq!(code(&error), "not_editable");
}

#[tokio::test]
async fn nested_vector_content_is_read_only_without_affecting_following_html() {
    let page = concat!(
        "<!doctype html><html><body><main>",
        "<svg viewBox=\"0 0 10 10\"><rect width=\"10\" height=\"10\"/></svg>",
        "<p>after</p></main></body></html>"
    );
    let fixture = Fixture::new(page);
    let project = fixture.open().await;
    let snapshot = project.snapshot().await.expect("snapshot");

    let svg = first(&snapshot, "svg");
    let rect = first(&snapshot, "rect");
    assert_eq!(svg.reason.as_deref(), Some("vector_content"));
    assert_eq!(rect.reason.as_deref(), Some("foreign_content"));
    assert!(!svg.editable && !rect.editable);

    let paragraph = first(&snapshot, "p");
    assert!(paragraph.editable);
    assert_eq!(
        node(&snapshot, paragraph.parent.as_deref().expect("parent")).tag,
        "main",
        "closing a self-closed vector child must not change the HTML parent"
    );
}

#[tokio::test]
async fn nested_vector_regions_keep_their_foreign_depth() {
    let page = concat!(
        "<!doctype html><html><body><main>",
        "<svg viewBox=\"0 0 10 10\"><svg viewBox=\"0 0 4 4\"><rect width=\"4\" height=\"4\"/></svg>",
        "<path d=\"M0 0h1\"/></svg>",
        "<p>after</p></main></body></html>"
    );
    let fixture = Fixture::new(page);
    let project = fixture.open().await;
    let snapshot = project.snapshot().await.expect("snapshot");

    let path = first(&snapshot, "path");
    assert!(!path.editable);
    assert_eq!(path.reason.as_deref(), Some("foreign_content"));
    let paragraph = first(&snapshot, "p");
    assert!(paragraph.editable);
    assert_eq!(
        node(&snapshot, paragraph.parent.as_deref().expect("parent")).tag,
        "main"
    );
}

#[tokio::test]
async fn save_writes_only_the_draft_with_compare_and_swap() {
    let fixture = Fixture::new(BASE_PAGE);
    let project = fixture.open().await;
    let before = project.snapshot().await.expect("snapshot");
    let id = editable(&before, "p").id.clone();

    project
        .apply(json!({"kind": "text", "id": id, "text": "Saved draft"}))
        .await
        .expect("text edit");
    let saved = project.save(&before.sha256).await.expect("save");
    assert!(!fixture.read().contains(NODE_ATTRIBUTE));
    assert!(fixture.read().contains("Saved draft"));
    assert_eq!(saved.sha256, sha256(&fixture.read()));
    assert!(!project.dirty().await);

    let error = project
        .save(&"0".repeat(64))
        .await
        .expect_err("old revision is stale");
    assert_eq!(code(&error), "stale_source");
    assert!(!error.outcome_unknown);

    let error = project.save("not-a-digest").await.expect_err("invalid");
    assert_eq!(code(&error), "invalid_value");
}

#[tokio::test]
async fn selected_id_follows_the_exact_source_position_of_each_operation() {
    let page = concat!(
        "<!doctype html><html><body><main><p>one</p><h2>two</h2>",
        "<div id=\"movable\">three</div></main></body></html>"
    );
    let fixture = Fixture::new(page);
    let project = fixture.open().await;
    let snapshot = project.snapshot().await.expect("snapshot");
    assert_eq!(snapshot.selected_id, None);

    let paragraph = editable(&snapshot, "p").id.clone();
    let styled = project
        .apply(json!({"kind": "style", "id": paragraph, "properties": {"color": "blue"}}))
        .await
        .expect("style edit");
    let selected = styled.selected_id.clone().expect("style selection");
    assert_eq!(node(&styled, &selected).tag, "p");
    assert_eq!(
        project.snapshot().await.expect("snapshot").selected_id,
        Some(selected),
        "the selected id survives a document read"
    );

    let main = editable(&styled, "main").id.clone();
    let inserted = project
        .apply(json!({
            "kind": "insert",
            "target": main,
            "placement": "inside",
            "component": "hero"
        }))
        .await
        .expect("insert hero");
    let selected = inserted.selected_id.clone().expect("insert selection");
    assert_eq!(
        node(&inserted, &selected).tag,
        "section",
        "the read-only component <style> must be skipped"
    );
    assert_eq!(
        inserted
            .nodes
            .iter()
            .rfind(|node| node.tag == "section")
            .expect("hero section")
            .id,
        selected
    );

    let heading = editable(&inserted, "h2").id.clone();
    let duplicated = project
        .apply(json!({"kind": "duplicate", "id": heading}))
        .await
        .expect("duplicate heading");
    let selected = duplicated.selected_id.clone().expect("duplicate selection");
    assert_eq!(node(&duplicated, &selected).tag, "h2");
    assert_eq!(
        duplicated
            .nodes
            .iter()
            .filter(|node| node.tag == "h2")
            .nth(1)
            .expect("duplicate heading")
            .id,
        selected,
        "the duplicate, not the source element, is selected"
    );

    let movable = editable(&duplicated, "div").id.clone();
    let heading = editable(&duplicated, "h2").id.clone();
    let moved = project
        .apply(json!({"kind": "move", "id": movable, "target": heading, "placement": "before"}))
        .await
        .expect("move div before heading");
    let selected = moved.selected_id.clone().expect("move selection");
    assert_eq!(node(&moved, &selected).tag, "div");
    assert_eq!(
        selected,
        moved
            .nodes
            .iter()
            .find(|node| node.tag == "div")
            .expect("moved div")
            .id,
        "the first div in source order is the moved element"
    );
    let selected_at = moved
        .html
        .find(&format!("{NODE_ATTRIBUTE}=\"{selected}\""))
        .expect("moved mapping");
    let heading_at = moved.html.find("<h2").expect("heading");
    assert!(
        selected_at < heading_at,
        "the moved div must be selected at its new source position"
    );

    let movable = editable(&moved, "div").id.clone();
    let deleted = project
        .apply(json!({"kind": "delete", "id": movable}))
        .await
        .expect("delete div");
    assert_eq!(deleted.selected_id, None);

    let undone = project.apply(json!({"kind": "undo"})).await.expect("undo");
    assert_eq!(undone.selected_id, None);
    let redone = project.apply(json!({"kind": "redo"})).await.expect("redo");
    assert_eq!(redone.selected_id, None);

    let reloaded = project.reload().await.expect("reload");
    assert_eq!(reloaded.selected_id, None);
}

#[tokio::test]
async fn an_external_change_is_reported_and_never_overwritten() {
    let fixture = Fixture::new(BASE_PAGE);
    let project = fixture.open().await;
    let before = project.snapshot().await.expect("snapshot");
    let id = editable(&before, "p").id.clone();
    project
        .apply(json!({"kind": "text", "id": id, "text": "Draft edit"}))
        .await
        .expect("text edit");
    project.save(&before.sha256).await.expect("first save");

    let saved = project.snapshot().await.expect("snapshot");
    let id = editable(&saved, "p").id.clone();
    project
        .apply(json!({"kind": "text", "id": id, "text": "Second draft"}))
        .await
        .expect("second edit");

    let external = "<!doctype html><html><body><main><p>External</p></main></body></html>";
    fixture.write(external);

    let error = project
        .save(&saved.sha256)
        .await
        .expect_err("external change must win");
    assert_eq!(code(&error), "stale_source");
    assert!(!error.outcome_unknown);
    assert_eq!(
        fixture.read(),
        external,
        "the external bytes stay untouched"
    );

    let draft = project.snapshot().await.expect("draft snapshot");
    assert!(draft.html.contains("Second draft"));
    assert!(project.dirty().await);
    assert_eq!(
        draft.sha256,
        sha256(external),
        "the conflict reports the current external revision"
    );

    // The refreshed digest is readable from the snapshot, but it must not
    // release the stale draft. Otherwise a client could send this digest back
    // and overwrite the external source with the old mapping.
    let error = project
        .save(&draft.sha256)
        .await
        .expect_err("a conflicted project must stay locked until reload");
    assert_eq!(code(&error), "stale_source");
    assert!(!error.outcome_unknown);
    assert_eq!(
        fixture.read(),
        external,
        "the external bytes stay untouched after the retry"
    );

    let paragraph = editable(&draft, "p").id.clone();
    let error = operation_error(
        &project,
        json!({"kind": "text", "id": paragraph, "text": "After conflict"}),
    )
    .await;
    assert_eq!(
        code(&error),
        "stale_source",
        "editing stays locked until reload"
    );

    let reloaded = project.reload().await.expect("reload");
    assert_eq!(reloaded.sha256, sha256(external));
    assert!(reloaded.html.contains(">External</p>"));
    assert!(!reloaded.html.contains("Second draft"));
    assert!(reloaded.html.contains(NODE_ATTRIBUTE));
    assert!(!project.dirty().await);
    assert_eq!(project.history_flags().await, (false, false));

    // Reload is the only release: the fresh source is editable and savable
    // again with its own revision.
    let paragraph = editable(&reloaded, "p").id.clone();
    let edited = project
        .apply(json!({"kind": "text", "id": paragraph, "text": "After reload"}))
        .await
        .expect("edit after reload");
    project
        .save(&edited.sha256)
        .await
        .expect("save after reload");
    assert!(fixture.read().contains("After reload"));
}

#[tokio::test]
async fn reload_keeps_the_old_draft_when_the_new_source_cannot_be_mapped() {
    let fixture = Fixture::new(BASE_PAGE);
    let project = fixture.open().await;
    let before = project.snapshot().await.expect("snapshot");
    let paragraph = editable(&before, "p").id.clone();
    project
        .apply(json!({"kind": "text", "id": paragraph, "text": "Draft survives"}))
        .await
        .expect("text edit");
    let draft = project.snapshot().await.expect("draft snapshot");

    fixture.write(&"<div></div>".repeat(20_001));
    let error = project
        .reload()
        .await
        .expect_err("too many elements must fail mapping");
    assert_eq!(code(&error), "page_too_complex");

    let after = project
        .snapshot()
        .await
        .expect("snapshot after failed reload");
    assert_eq!(after.sha256, draft.sha256, "the old revision must survive");
    assert!(after.html.contains("Draft survives"));
    assert_eq!(after.selected_id, draft.selected_id);
}

#[tokio::test]
async fn an_unresolved_unknown_marker_refuses_startup() {
    let fixture = Fixture::new(BASE_PAGE);
    fs::write(
        fixture.state.path().join("unknown.json"),
        b"{\"code\":\"old\"}\n",
    )
    .expect("marker");
    let error = Project::open(fixture.root.path(), "index.html", fixture.state.path())
        .await
        .expect_err("unknown marker must refuse startup");
    assert_eq!(code(&error), "reconciliation_required");
    assert!(error.outcome_unknown);
}

#[tokio::test]
async fn a_prior_unsettled_audit_call_refuses_startup_and_leaves_a_marker() {
    let fixture = Fixture::new(BASE_PAGE);
    fs::write(
        fixture.state.path().join("calls.jsonl"),
        "{\"event\":\"started\",\"call_id\":\"old-call\",\"capability\":\"fs.write\",\"at_ms\":1}\n",
    )
    .expect("prior audit");
    let error = Project::open(fixture.root.path(), "index.html", fixture.state.path())
        .await
        .expect_err("unsettled call must refuse startup");
    assert_eq!(code(&error), "reconciliation_required");
    assert!(error.outcome_unknown);
    assert!(
        fixture.state.path().join("unknown.json").exists(),
        "a refusal must persist the unknown marker"
    );

    let fixture = Fixture::new(BASE_PAGE);
    fs::write(
        fixture.state.path().join("calls.jsonl"),
        concat!(
            "{\"event\":\"started\",\"call_id\":\"old-call\",\"capability\":\"fs.write\",\"at_ms\":1}\n",
            "{\"event\":\"settled\",\"call_id\":\"old-call\",\"outcome_unknown\":true,\"at_ms\":2}\n"
        ),
    )
    .expect("prior unknown audit");
    let error = Project::open(fixture.root.path(), "index.html", fixture.state.path())
        .await
        .expect_err("unknown settlement must refuse startup");
    assert_eq!(code(&error), "reconciliation_required");
    assert!(error.outcome_unknown);
}

#[tokio::test]
async fn one_process_owns_a_state_directory() {
    let fixture = Fixture::new(BASE_PAGE);
    let first = fixture.open().await;
    let error = match Project::open(fixture.root.path(), "index.html", fixture.state.path()).await {
        Ok(_) => panic!("a second writer must be refused"),
        Err(error) => error,
    };
    assert_eq!(code(&error), "state_dir_busy");
    assert!(!error.outcome_unknown);
    drop(first);

    let reopened = fixture.open().await;
    assert_eq!(reopened.page(), "index.html");
}

#[tokio::test]
async fn state_directory_and_page_paths_are_bounded() {
    let fixture = Fixture::new(BASE_PAGE);

    let inside = fixture.root.path().join("state");
    let error = Project::open(fixture.root.path(), "index.html", &inside)
        .await
        .expect_err("state inside project");
    assert_eq!(code(&error), "state_inside_project");
    assert!(
        !inside.exists(),
        "a refused state directory must not be created inside the project"
    );

    for page in [
        "index.txt",
        "../escape.html",
        "sub/../index.html",
        ".hidden.html",
    ] {
        let error = Project::open(fixture.root.path(), page, fixture.state.path())
            .await
            .expect_err("invalid page");
        assert_eq!(code(&error), "invalid_page", "{page}");
    }

    let error = Project::open(fixture.root.path(), "missing.html", fixture.state.path())
        .await
        .expect_err("missing page");
    assert_eq!(code(&error), "read_failed");

    let missing_root = fixture.root.path().join("absent");
    let error = Project::open(&missing_root, "index.html", fixture.state.path())
        .await
        .expect_err("missing root");
    assert_eq!(code(&error), "invalid_project");
}

#[tokio::test]
async fn durable_audit_records_calls_without_page_content() {
    let page = BASE_PAGE.replace("Hello", "AUDIT-SECRET-TEXT");
    let fixture = Fixture::new(&page);
    let project = fixture.open().await;
    let snapshot = project.snapshot().await.expect("snapshot");
    let id = editable(&snapshot, "p").id.clone();
    project
        .apply(json!({"kind": "text", "id": id, "text": "AUDIT-DRAFT-TEXT"}))
        .await
        .expect("edit");
    project.save(&snapshot.sha256).await.expect("save");

    let log = fs::read_to_string(fixture.state.path().join("calls.jsonl")).expect("audit log");
    assert!(log.contains("\"capability\":\"fs.read\""));
    assert!(log.contains("\"capability\":\"fs.write\""));
    assert!(log.contains("\"event\":\"settled\""));
    assert!(!log.contains("AUDIT-SECRET-TEXT"));
    assert!(!log.contains("AUDIT-DRAFT-TEXT"));
    assert!(log.contains("\"redacted\":true"));
    for line in log.lines().filter(|line| !line.trim().is_empty()) {
        let value: Value = serde_json::from_str(line).expect("audit line is JSON");
        assert!(value.get("call_id").and_then(Value::as_str).is_some());
    }
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlinked_state_alias_is_refused_before_creating_the_directory() {
    let fixture = Fixture::new(BASE_PAGE);
    let alias_parent = tempfile::tempdir().expect("alias tempdir");
    let alias = alias_parent.path().join("project-alias");
    std::os::unix::fs::symlink(fixture.root.path(), &alias).expect("symlink alias");

    let inside = alias.join("state");
    let error = Project::open(fixture.root.path(), "index.html", &inside)
        .await
        .expect_err("a symlink alias into the project must be refused");
    assert_eq!(code(&error), "state_inside_project");
    assert!(
        !fixture.root.path().join("state").exists(),
        "the alias target must not be created"
    );
}

#[tokio::test]
async fn ids_are_bound_to_the_draft_revision_and_old_ids_are_rejected() {
    let fixture =
        Fixture::new("<!doctype html><html><body><main><p>A</p><h2>B</h2></main></body></html>");
    let project = fixture.open().await;
    let snapshot = project.snapshot().await.expect("snapshot");
    let paragraph = editable(&snapshot, "p").id.clone();
    let (prefix, digest) = paragraph.rsplit_once('-').expect("id revision suffix");
    assert!(prefix.starts_with('n'), "{paragraph}");
    assert_eq!(digest.len(), 64, "{paragraph}");
    assert!(
        digest
            .chars()
            .all(|character| character.is_ascii_hexdigit())
    );

    let heading = editable(&snapshot, "h2").id.clone();
    let changed = project
        .apply(json!({
            "kind": "insert",
            "target": heading,
            "placement": "before",
            "component": "text"
        }))
        .await
        .expect("structural change");
    let error = operation_error(
        &project,
        json!({"kind": "text", "id": paragraph, "text": "WRONG ELEMENT"}),
    )
    .await;
    assert_eq!(code(&error), "not_found");
    assert!(!changed.html.contains("WRONG ELEMENT"));
    assert_ne!(
        editable(&changed, "p").id.as_str(),
        paragraph.as_str(),
        "a new draft revision must issue a new id"
    );
}

#[tokio::test]
async fn template_text_after_a_child_protects_the_wrapper() {
    let page = "<!doctype html><html><body><main><div id=\"wrap\"><span>Hi</span> {{ user }}</div><p>safe</p></main></body></html>";
    let fixture = Fixture::new(page);
    let project = fixture.open().await;
    let snapshot = project.snapshot().await.expect("snapshot");

    let wrapper = first(&snapshot, "div");
    assert!(!wrapper.editable, "the wrapper has dynamic text");
    assert_eq!(wrapper.reason.as_deref(), Some("dynamic_region"));
    let span = first(&snapshot, "span");
    assert!(
        !span.editable,
        "a descendant of a dynamic wrapper is read-only"
    );
    assert_eq!(span.parent.as_deref(), Some(wrapper.id.as_str()));
    let safe = editable(&snapshot, "p");
    assert_eq!(safe.tag, "p");

    let wrapper_id = wrapper.id.clone();
    let error = operation_error(
        &project,
        json!({"kind": "style", "id": wrapper_id, "properties": {"color": "red"}}),
    )
    .await;
    assert_eq!(code(&error), "not_editable");
}

#[tokio::test]
async fn script_containing_wrappers_cannot_be_moved_duplicated_or_deleted() {
    let page = "<!doctype html><html><body><main><div id=\"wrap\"><p>inside</p><script>console.log(1)</script></div><p>neighbor</p></main></body></html>";
    let fixture = Fixture::new(page);
    let project = fixture.open().await;
    let snapshot = project.snapshot().await.expect("snapshot");
    let wrapper = editable(&snapshot, "div").id.clone();
    let body = editable(&snapshot, "body").id.clone();
    let neighbor = tagged(&snapshot, "p", 1);
    assert!(neighbor.editable);
    let neighbor = neighbor.id.clone();

    for operation in [
        json!({"kind": "duplicate", "id": wrapper}),
        json!({"kind": "delete", "id": wrapper}),
        json!({"kind": "move", "id": wrapper, "target": body, "placement": "inside"}),
    ] {
        let error = operation_error(&project, operation.clone()).await;
        assert_eq!(code(&error), "unsupported_target", "{operation}");
    }

    let changed = project
        .apply(json!({
            "kind": "attribute",
            "id": neighbor,
            "name": "aria-label",
            "value": "static neighbor"
        }))
        .await
        .expect("a static neighboring element stays editable");
    assert!(changed.html.contains("aria-label=\"static neighbor\""));
}

#[tokio::test]
async fn raw_text_elements_and_their_descendants_are_read_only() {
    let page = concat!(
        "<!doctype html><html><body><main>",
        "<xmp><b>raw</b> {{ user }}</xmp>",
        "<noembed><i>raw</i></noembed>",
        "<noframes><div><p>nested</p></div></noframes>",
        "<p>safe</p></main></body></html>"
    );
    let fixture = Fixture::new(page);
    let project = fixture.open().await;
    let snapshot = project.snapshot().await.expect("snapshot");

    for tag in ["xmp", "noembed", "noframes"] {
        let node = first(&snapshot, tag);
        assert!(!node.editable, "<{tag}> must be read-only");
        assert!(node.reason.is_some(), "<{tag}> needs a reason");
    }
    let nested = tagged(&snapshot, "p", 0);
    assert!(!nested.editable, "a descendant of noframes is read-only");
    let safe = tagged(&snapshot, "p", 1);
    assert!(safe.editable, "unrelated content stays editable");

    let plain = Fixture::new("<!doctype html><html><body><main><plaintext><b>raw</b>");
    let project = plain.open().await;
    let snapshot = project.snapshot().await.expect("snapshot");
    assert!(!first(&snapshot, "plaintext").editable);
}

#[tokio::test]
async fn the_real_site_page_maps_and_edits_without_touching_scripts() {
    let site = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../site/index.html");
    let original = fs::read_to_string(&site).expect("real site page");
    let fixture = Fixture::new(&original);
    let project = fixture.open().await;
    let snapshot = project.snapshot().await.expect("snapshot");

    assert!(
        snapshot.nodes.len() > 50,
        "the real page must map many nodes"
    );
    assert!(snapshot.nodes.iter().filter(|node| node.editable).count() >= 20);
    let main = first(&snapshot, "main");
    assert!(main.editable);

    let svg_count = snapshot
        .nodes
        .iter()
        .filter(|node| node.tag == "svg")
        .count();
    assert!(svg_count >= 2);
    assert!(
        snapshot
            .nodes
            .iter()
            .filter(|node| node.tag == "svg")
            .all(|node| !node.editable)
    );
    assert_eq!(
        first(&snapshot, "svg").reason.as_deref(),
        Some("vector_content")
    );

    let hero_paragraph = snapshot
        .nodes
        .iter()
        .find(|node| node.tag == "p" && node.line > 70 && node.editable)
        .expect("hero paragraph");
    assert_eq!(
        node(
            &snapshot,
            hero_paragraph.parent.as_deref().expect("hero parent")
        )
        .tag,
        "div",
        "an inline SVG must not capture the following HTML children"
    );

    let hero = snapshot
        .nodes
        .iter()
        .find(|node| node.tag == "section" && node.editable)
        .expect("hero section");
    let edited = project
        .apply(json!({
            "kind": "style",
            "id": hero.id,
            "properties": {"padding-top": "1px"}
        }))
        .await
        .expect("style real page");
    assert!(edited.html.contains("padding-top: 1px"));

    let main = editable(&edited, "main").id.clone();
    let edited = project
        .apply(json!({
            "kind": "insert",
            "target": main,
            "placement": "inside",
            "component": "card"
        }))
        .await
        .expect("insert into real page");
    assert!(edited.html.contains("data-cayw-component=\"card\""));

    let saved = project
        .save(&snapshot.sha256)
        .await
        .expect("save real page");
    let written = fixture.read();
    assert_eq!(saved.sha256, sha256(&written));
    assert!(!written.contains(NODE_ATTRIBUTE));
    assert!(written.contains("<script>document.documentElement.classList.add(\"js\");</script>"));
    assert!(written.contains("src=\"/script.js\""));
    assert!(written.contains("padding-top: 1px"));
    assert!(written.contains("data-cayw-component=\"card\""));
    assert!(written.starts_with("<!DOCTYPE html>"));
}
