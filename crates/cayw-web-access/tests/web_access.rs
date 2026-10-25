//! F10 / SLICE M offline smoke: search → fetch/extract → citation; offline diagnostic;
//! never touches `search.query`.

use cayw_web_access::*;
use serde_json::json;

fn demo_html() -> &'static str {
    r#"<!doctype html>
<html><head><title>Example Domain</title></head>
<body><div><h1>Example Domain</h1>
<p>This domain is for use in documentation examples.</p>
<script>ignored()</script>
</div></body></html>"#
}

fn fixture_stack() -> WebAccess {
    let transport = FixtureTransport::new().insert_html("https://example.com/", demo_html());
    let search = FixtureSearch::new().insert(
        "example",
        vec![hit(
            1,
            "https://example.com/",
            "Example Domain",
            "documentation examples",
        )],
    );
    WebAccess::with_fixtures(transport, search)
}

#[tokio::test]
async fn public_url_extract_yields_citation_for_later_tools() {
    let access = fixture_stack();
    let extracted = access.fetch_and_extract("https://example.com/").await.unwrap();
    assert_eq!(extracted.title.as_deref(), Some("Example Domain"));
    assert!(extracted.text.contains("documentation examples"));
    assert!(!extracted.text.contains("ignored"));
    let cref = extracted.citation.content_ref.as_deref().expect("content_ref");
    let body = access.store().get_text(cref).unwrap();
    assert!(body.contains("Example Domain"));
    // Citation JSON is what subsequent tools would receive as a handle-like pointer.
    let wire = serde_json::to_value(&extracted.citation).unwrap();
    assert_eq!(wire["url"], "https://example.com/");
    assert_eq!(wire["content_ref"], cref);
}

#[tokio::test]
async fn search_then_extract_pipeline() {
    let access = fixture_stack();
    let found = access.search("example", Some(3)).await.unwrap();
    assert_eq!(found.provider, "fixture");
    assert_eq!(found.hits.len(), 1);
    assert_eq!(found.citations[0].url, "https://example.com/");
    let extracted = access.extract_url(&found.hits[0].url).await.unwrap();
    assert_eq!(extracted.kind, ContentKind::Html);
    assert!(extracted.citation.content_ref.is_some());
}

#[tokio::test]
async fn offline_failure_is_diagnosable() {
    let transport = FixtureTransport::new().offline();
    let search = FixtureSearch::new().offline();
    let access = WebAccess::with_fixtures(transport, search);
    let err = access
        .extract_url("https://example.com/")
        .await
        .expect_err("offline");
    let d = err.diagnostic();
    assert_eq!(d.class, FailureClass::Offline);
    assert!(d.offline_mode);
    assert_eq!(d.url.as_deref(), Some("https://example.com/"));
    assert!(d.message.contains("offline"));

    let err = access.search("anything", None).await.expect_err("offline search");
    assert_eq!(err.diagnostic().class, FailureClass::Offline);
}

#[tokio::test]
async fn refuses_search_query_capability_name() {
    let access = fixture_stack();
    let err = access
        .call("search.query", json!({"query": "x"}))
        .await
        .expect_err("must refuse");
    assert!(err.diagnostic().message.contains("search.query"));
    assert!(
        !capabilities::is_web_access("search.query"),
        "search.query must not be classified as web-access"
    );
    assert!(capabilities::is_local_search_query("search.query"));
}

#[tokio::test]
async fn call_dispatch_web_capabilities() {
    let access = fixture_stack();
    let search = access
        .call(capabilities::WEB_SEARCH, json!({"query": "example", "limit": 2}))
        .await
        .unwrap();
    assert_eq!(search["hits"][0]["url"], "https://example.com/");
    let fetched = access
        .call(capabilities::WEB_FETCH, json!({"url": "https://example.com/"}))
        .await
        .unwrap();
    assert_eq!(fetched["status"], 200);
    let extracted = access
        .call(capabilities::WEB_EXTRACT, json!({"url": "https://example.com/"}))
        .await
        .unwrap();
    assert!(extracted["text"].as_str().unwrap().contains("Example Domain"));
}

#[test]
fn pdf_extract_through_service() {
    let access = fixture_stack();
    let pdf = b"%PDF-1.4\nBT (Cited PDF Body) Tj ET\n%%EOF\n";
    let result = access
        .extract_bytes(
            "https://example.com/doc.pdf",
            "https://example.com/doc.pdf",
            Some("application/pdf"),
            pdf,
        )
        .unwrap();
    assert_eq!(result.kind, ContentKind::PdfText);
    assert!(result.text.contains("Cited PDF Body"));
    assert!(result.citation.content_ref.is_some());
}

#[test]
fn capability_names_are_stable() {
    assert_eq!(capabilities::WEB_SEARCH, "web.search");
    assert_eq!(capabilities::WEB_FETCH, "web.fetch");
    assert_eq!(capabilities::WEB_EXTRACT, "web.extract");
    assert_eq!(WEB_ACCESS_PROTOCOL_VERSION, 1);
}
