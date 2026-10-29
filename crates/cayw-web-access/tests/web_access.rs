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
    let extracted = access
        .fetch_and_extract("https://example.com/")
        .await
        .unwrap();
    assert_eq!(extracted.title.as_deref(), Some("Example Domain"));
    assert!(extracted.text.contains("documentation examples"));
    assert!(!extracted.text.contains("ignored"));
    let cref = extracted
        .citation
        .content_ref
        .as_deref()
        .expect("content_ref");
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

    let err = access
        .search("anything", None)
        .await
        .expect_err("offline search");
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
        .call(
            capabilities::WEB_SEARCH,
            json!({"query": "example", "limit": 2}),
        )
        .await
        .unwrap();
    assert_eq!(search["hits"][0]["url"], "https://example.com/");
    let fetched = access
        .call(
            capabilities::WEB_FETCH,
            json!({"url": "https://example.com/"}),
        )
        .await
        .unwrap();
    assert_eq!(fetched["status"], 200);
    let extracted = access
        .call(
            capabilities::WEB_EXTRACT,
            json!({"url": "https://example.com/"}),
        )
        .await
        .unwrap();
    assert!(
        extracted["text"]
            .as_str()
            .unwrap()
            .contains("Example Domain")
    );
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

#[tokio::test]
async fn duckduckgo_html_search_via_fixture_transport() {
    let results_html = r#"<!doctype html><html><body>
<a class="result__a" href="https://duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2F&amp;rut=1">Example</a>
<a class="result__snippet">Example snippet</a>
</body></html>"#;
    let search_url = {
        let mut u = url::Url::parse("https://html.duckduckgo.com/html/").unwrap();
        u.query_pairs_mut().append_pair("q", "example domain");
        u.to_string()
    };
    let transport = FixtureTransport::new()
        .insert_html(&search_url, results_html)
        .insert_html("https://example.com/", demo_html());
    let search = DuckDuckGoHtmlSearch::new(std::sync::Arc::new(transport.clone()));
    // DuckDuckGoHtmlSearch needs the same transport the WebAccess fetch uses.
    let access = WebAccess::new(
        std::sync::Arc::new(transport),
        std::sync::Arc::new(search),
        ContentStore::new(),
        Limits::default(),
    );
    let found = access.search("example domain", Some(5)).await.unwrap();
    assert_eq!(found.provider, "duckduckgo_html");
    assert_eq!(found.hits.len(), 1);
    assert_eq!(found.hits[0].url, "https://example.com/");
    assert_eq!(found.hits[0].title.as_deref(), Some("Example"));
}

#[test]
fn definitions_register_web_tools_not_search_query() {
    let defs = definitions();
    let names: Vec<_> = defs.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(names, ["web.search", "web.fetch", "web.extract"]);
    assert!(!names.contains(&"search.query"));
}

#[test]
fn options_fixture_builds_without_network_client_path() {
    let access = WebAccessOptions::fixture().build().unwrap();
    assert!(access.store().is_empty());
}

#[test]
fn pdf_scope_is_literal_only_not_full_engine() {
    // Compressed / binary streams without parenthesis literals yield extract failure —
    // documenting that we do not claim full PDF support.
    let access = fixture_stack();
    let pdf =
        b"%PDF-1.4\n1 0 obj<< /Length 4 >>stream\n\x00\x01\x02\x03\nendstream\nendobj\n%%EOF\n";
    let err = access
        .extract_bytes(
            "https://example.com/bin.pdf",
            "https://example.com/bin.pdf",
            Some("application/pdf"),
            pdf,
        )
        .expect_err("no literals");
    assert_eq!(err.diagnostic().class, FailureClass::ExtractFailed);
}

#[tokio::test]
async fn ddg_search_distinguishes_empty_and_evidence() {
    let noresults_html =
        r#"<div class="result result--no-result"><div class="no-results__message"><h1>No results found</h1></div></div>"#.to_string();
    let normal_html =
        r#"<div class="result"><a class="result__a" href="https://example.com/rust">Rust</a></div>"#
            .to_string();

    let challenge_html = r#"<html><body><h1>Unfortunately, bots use DuckDuckGo too</h1><p>Please complete the challenge</p></body></html>"#;
    let structural_drift_html = r#"<html><head><title>Search Results</title></head><body><div class="new_layout">Nothing to see here</div></body></html>"#;

    let search_url_normal = {
        let mut u = url::Url::parse("https://html.duckduckgo.com/html/").unwrap();
        u.query_pairs_mut().append_pair("q", "Rust ownership");
        u.to_string()
    };
    let search_url_empty = {
        let mut u = url::Url::parse("https://html.duckduckgo.com/html/").unwrap();
        u.query_pairs_mut()
            .append_pair("q", "\"xqz97b81nmv02498712398471923847\"");
        u.to_string()
    };
    let search_url_challenge = {
        let mut u = url::Url::parse("https://html.duckduckgo.com/html/").unwrap();
        u.query_pairs_mut().append_pair("q", "bot trigger");
        u.to_string()
    };
    let search_url_drift = {
        let mut u = url::Url::parse("https://html.duckduckgo.com/html/").unwrap();
        u.query_pairs_mut().append_pair("q", "drift layout");
        u.to_string()
    };

    let transport = FixtureTransport::new()
        .insert_html(&search_url_normal, normal_html)
        .insert_html(&search_url_empty, noresults_html)
        .insert_html(&search_url_challenge, challenge_html)
        .insert_html(&search_url_drift, structural_drift_html);

    let search = DuckDuckGoHtmlSearch::new(std::sync::Arc::new(transport.clone()));
    let access = WebAccess::new(
        std::sync::Arc::new(transport),
        std::sync::Arc::new(search),
        ContentStore::new(),
        Limits::default(),
    );

    // 1. 正常 (Normal)
    let res_normal = access.search("Rust ownership", Some(5)).await.unwrap();
    assert!(!res_normal.hits.is_empty());
    assert_eq!(res_normal.empty_reason, None);
    assert_eq!(res_normal.evidence, None);

    // 2. 真空 (True Empty)
    let res_empty = access
        .search("\"xqz97b81nmv02498712398471923847\"", Some(5))
        .await
        .unwrap();
    assert!(res_empty.hits.is_empty());
    assert_eq!(res_empty.empty_reason, Some(SearchEmptyReason::TrueEmpty));
    let ev = res_empty.evidence.expect("evidence present for true empty");
    assert_eq!(ev.http_status, Some(200));
    assert!(
        ev.source_summary
            .as_deref()
            .unwrap_or("")
            .contains("No results found")
    );
    assert!(ev.response_snippet.is_some());

    // 3. 挑战 (Challenge)
    let res_challenge = access.search("bot trigger", Some(5)).await.unwrap();
    assert!(res_challenge.hits.is_empty());
    assert_eq!(
        res_challenge.empty_reason,
        Some(SearchEmptyReason::Challenge)
    );
    let ev = res_challenge.evidence.expect("evidence for challenge");
    assert!(
        ev.source_summary
            .as_deref()
            .unwrap_or("")
            .to_lowercase()
            .contains("bots use duckduckgo")
    );

    // 4. 结构变更 (Structural Mismatch)
    let res_drift = access.search("drift layout", Some(5)).await.unwrap();
    assert!(res_drift.hits.is_empty());
    assert_eq!(
        res_drift.empty_reason,
        Some(SearchEmptyReason::StructuralMismatch)
    );
    let ev = res_drift
        .evidence
        .expect("evidence for structural mismatch");
    assert!(
        ev.source_summary
            .as_deref()
            .unwrap_or("")
            .contains("Search Results")
    );

    // Missing response text leaves the empty reason unattributed.
    let search_fixture = FixtureSearch::new();
    let access_fixture = WebAccess::new(
        std::sync::Arc::new(FixtureTransport::new()),
        std::sync::Arc::new(search_fixture),
        ContentStore::new(),
        Limits::default(),
    );
    let res_unattributed = access_fixture
        .search("missing in fixture", Some(5))
        .await
        .unwrap();
    assert!(res_unattributed.hits.is_empty());
    assert_eq!(
        res_unattributed.empty_reason,
        Some(SearchEmptyReason::Unattributed)
    );
    assert!(
        res_unattributed
            .evidence
            .unwrap()
            .response_snippet
            .is_none()
    );
}

#[tokio::test]
async fn ddg_search_body_too_large_fails_fast() {
    let search_url = {
        let mut u = url::Url::parse("https://html.duckduckgo.com/html/").unwrap();
        u.query_pairs_mut().append_pair("q", "giant payload");
        u.to_string()
    };
    let giant_html = "x".repeat(3000);
    let transport = FixtureTransport::new().insert_html(&search_url, giant_html);
    let search = DuckDuckGoHtmlSearch::new(std::sync::Arc::new(transport.clone()))
        .with_max_fetch_bytes(1000);
    let access = WebAccess::new(
        std::sync::Arc::new(transport),
        std::sync::Arc::new(search),
        ContentStore::new(),
        Limits::default(),
    );
    let err = access
        .search("giant payload", Some(5))
        .await
        .expect_err("must exceed body limit");
    assert_eq!(err.diagnostic().class, FailureClass::BodyTooLarge);
}
