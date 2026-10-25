//! Offline demo: fixture search → extract → print citation + content ref.

use cayw_web_access::*;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let html = r#"<html><head><title>CAYW F10</title></head>
<body><p>Public URL body for citation.</p></body></html>"#;
    let transport = FixtureTransport::new().insert_html("https://example.com/f10", html);
    let search = FixtureSearch::new().insert(
        "f10",
        vec![hit(1, "https://example.com/f10", "CAYW F10", "citation demo")],
    );
    let access = WebAccess::with_fixtures(transport, search);

    let found = access.search("f10", Some(1)).await.expect("search");
    println!("search provider={} hits={}", found.provider, found.hits.len());
    let extracted = access
        .extract_url(&found.hits[0].url)
        .await
        .expect("extract");
    println!(
        "title={:?} content_ref={} text={}",
        extracted.title,
        extracted.citation.content_ref.as_deref().unwrap_or("-"),
        extracted.text
    );
    println!(
        "protocol={} capabilities={:?}",
        WEB_ACCESS_PROTOCOL_VERSION,
        capabilities::ALL
    );
}
