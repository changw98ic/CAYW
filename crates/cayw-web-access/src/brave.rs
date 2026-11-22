//! Optional Brave Search API backend (env-gated key).

use std::sync::Arc;

use crate::error::WebAccessError;
use crate::provider::{BoxFuture, SearchProvider};
use crate::types::{FailureClass, SearchEmptyReason, SearchEvidence, SearchHit, SearchResponse};

const PROVIDER: &str = "brave";
const ENDPOINT: &str = "https://api.search.brave.com/res/v1/web/search";
/// Environment variable for the Brave Search subscription token.
pub const BRAVE_API_KEY_ENV: &str = "BRAVE_SEARCH_API_KEY";

/// Brave web search when a subscription token is available.
///
/// The HTTP transport must attach the `X-Subscription-Token` header. Prefer
/// [`BraveSearch::from_env`] which builds a dedicated reqwest client, or use
/// [`crate::host::build_search`] which selects Brave only when the env key is set.
pub struct BraveSearch {
    client: reqwest::Client,
    api_key: String,
    max_fetch_bytes: usize,
    endpoint: String,
}

impl BraveSearch {
    pub fn new(api_key: impl Into<String>) -> Result<Self, WebAccessError> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .user_agent("cayw-web-access/0.1 (F10)")
            .build()
            .map_err(|e| {
                WebAccessError::new(
                    FailureClass::ProviderUnavailable,
                    format!("brave client: {e}"),
                )
                .with_provider(PROVIDER)
            })?;
        Ok(Self {
            client,
            api_key: api_key.into(),
            max_fetch_bytes: usize::MAX,
            endpoint: ENDPOINT.to_string(),
        })
    }

    pub fn with_max_fetch_bytes(mut self, max: usize) -> Self {
        self.max_fetch_bytes = max;
        self
    }

    /// Returns `Ok(None)` when the env key is unset/empty (caller should fall back).
    pub fn from_env() -> Result<Option<Self>, WebAccessError> {
        match std::env::var(BRAVE_API_KEY_ENV) {
            Ok(key) if !key.trim().is_empty() => Ok(Some(Self::new(key.trim())?)),
            _ => Ok(None),
        }
    }
}

impl SearchProvider for BraveSearch {
    fn name(&self) -> &str {
        PROVIDER
    }

    fn search<'a>(
        &'a self,
        query: &'a str,
        limit: u32,
    ) -> BoxFuture<'a, Result<SearchResponse, WebAccessError>> {
        Box::pin(async move {
            if limit == 0 {
                return Err(WebAccessError::new(
                    FailureClass::InvalidRequest,
                    "Brave search limit must be positive",
                ));
            }
            // Brave offsets are pages, so keep count fixed across every request.
            let count = limit.min(20);
            let mut hits = Vec::new();
            let mut seen = std::collections::HashSet::new();
            for offset in 0..=9 {
                let mut url = url::Url::parse(&self.endpoint).map_err(|e| {
                    WebAccessError::new(FailureClass::InvalidUrl, format!("brave endpoint: {e}"))
                        .with_provider(PROVIDER)
                })?;
                url.query_pairs_mut()
                    .append_pair("q", query)
                    .append_pair("count", &count.to_string())
                    .append_pair("offset", &offset.to_string());
                let response = self
                    .client
                    .get(url)
                    .header("X-Subscription-Token", &self.api_key)
                    .header("Accept", "application/json")
                    .send()
                    .await
                    .map_err(|e| {
                        WebAccessError::new(FailureClass::Offline, format!("brave GET failed: {e}"))
                            .with_provider(PROVIDER)
                    })?;
                let (status, body) = crate::provider::read_bounded_body(
                    response,
                    self.max_fetch_bytes,
                    None,
                    PROVIDER,
                )
                .await?;
                if !(200..300).contains(&status) {
                    return Err(WebAccessError::new(
                        FailureClass::HttpStatus,
                        format!("brave HTTP {status}"),
                    )
                    .with_provider(PROVIDER)
                    .with_status(status));
                }
                let page = parse_brave_json(&body, count)?;
                let value: serde_json::Value = serde_json::from_slice(&body).map_err(|e| {
                    WebAccessError::new(FailureClass::ExtractFailed, format!("brave JSON: {e}"))
                        .with_provider(PROVIDER)
                })?;
                let more = value
                    .pointer("/query/more_results_available")
                    .and_then(|v| v.as_bool());
                let mut omitted = false;
                for mut hit in page {
                    if !seen.insert(hit.url.clone()) {
                        continue;
                    }
                    if hits.len() >= limit as usize {
                        omitted = true;
                        continue;
                    }
                    hit.rank = hits.len() as u32 + 1;
                    hits.push(hit);
                }
                if hits.len() >= limit as usize || more != Some(true) || offset == 9 {
                    let truncated = omitted || more != Some(false);
                    let mut result = if hits.is_empty() {
                        SearchResponse::empty(
                            PROVIDER,
                            if more == Some(false) {
                                SearchEmptyReason::TrueEmpty
                            } else {
                                SearchEmptyReason::Unattributed
                            },
                            None,
                        )
                    } else {
                        SearchResponse::new(PROVIDER, hits)
                    };
                    result.truncated = truncated;
                    if truncated {
                        result.evidence = Some(SearchEvidence {
                            http_status: Some(status),
                            source_summary: Some(if offset == 9 && more == Some(true) {
                                "Brave pagination reached its final supported offset (9); more results are available".into()
                            } else if more.is_none() {
                                "Brave did not report whether more results are available; pagination stopped".into()
                            } else {
                                "Requested result count reached; additional Brave results are available".into()
                            }),
                            response_snippet: None,
                        });
                    }
                    return Ok(result);
                }
            }
            unreachable!("the final supported page always returns a response")
        })
    }
}

fn parse_brave_json(bytes: &[u8], limit: u32) -> Result<Vec<SearchHit>, WebAccessError> {
    let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|e| {
        WebAccessError::new(FailureClass::ExtractFailed, format!("brave JSON: {e}"))
            .with_provider(PROVIDER)
    })?;
    let mut hits = Vec::new();
    let results = value
        .pointer("/web/results")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    for (i, item) in results.into_iter().enumerate() {
        if hits.len() as u32 >= limit {
            break;
        }
        let url = item
            .get("url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            continue;
        }
        hits.push(SearchHit {
            url,
            title: item
                .get("title")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            snippet: item
                .get("description")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            rank: (i as u32) + 1,
        });
    }
    Ok(hits)
}

/// Prefer Brave when configured; otherwise use the provided fallback (typically DDG HTML).
pub struct BraveOrFallback {
    brave: Option<BraveSearch>,
    fallback: Arc<dyn SearchProvider>,
}

impl BraveOrFallback {
    pub fn new(brave: Option<BraveSearch>, fallback: Arc<dyn SearchProvider>) -> Self {
        Self { brave, fallback }
    }
}

impl SearchProvider for BraveOrFallback {
    fn name(&self) -> &str {
        if self.brave.is_some() {
            PROVIDER
        } else {
            self.fallback.name()
        }
    }

    fn search<'a>(
        &'a self,
        query: &'a str,
        limit: u32,
    ) -> BoxFuture<'a, Result<SearchResponse, WebAccessError>> {
        Box::pin(async move {
            match &self.brave {
                Some(brave) => brave.search(query, limit).await,
                None => self.fallback.search(query, limit).await,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    struct ServerGuard {
        release_tx: Option<mpsc::Sender<()>>,
        handle: Option<thread::JoinHandle<()>>,
    }

    impl ServerGuard {
        fn new(release_tx: mpsc::Sender<()>, handle: thread::JoinHandle<()>) -> Self {
            Self {
                release_tx: Some(release_tx),
                handle: Some(handle),
            }
        }

        fn release_and_join(mut self) {
            if let Some(tx) = self.release_tx.take() {
                let _ = tx.send(());
            }
            if let Some(handle) = self.handle.take() {
                handle.join().expect("server thread panicked");
            }
        }
    }

    impl Drop for ServerGuard {
        fn drop(&mut self) {
            drop(self.release_tx.take());
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    fn read_http_request(stream: &mut std::net::TcpStream) -> std::io::Result<()> {
        let mut buf = [0u8; 4096];
        let mut n = 0;
        while n < buf.len() {
            let read_bytes = stream.read(&mut buf[n..])?;
            if read_bytes == 0 {
                break;
            }
            n += read_bytes;
            if buf[..n].windows(4).any(|w| w == b"\r\n\r\n") {
                return Ok(());
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn brave_search_chunked_exceeding_max_terminates_early() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
        let port = listener.local_addr().unwrap().port();
        let (release_tx, release_rx) = mpsc::channel();

        let server_handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept connection");
            read_http_request(&mut stream).expect("read request");

            stream
                .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n")
                .expect("write headers");
            stream.flush().expect("flush headers");

            let chunk1 = vec![b' '; 800];
            stream
                .write_all(format!("{:x}\r\n", chunk1.len()).as_bytes())
                .unwrap();
            stream.write_all(&chunk1).unwrap();
            stream.write_all(b"\r\n").unwrap();

            let chunk2 = vec![b' '; 800];
            stream
                .write_all(format!("{:x}\r\n", chunk2.len()).as_bytes())
                .unwrap();
            stream.write_all(&chunk2).unwrap();
            stream.write_all(b"\r\n").unwrap();
            stream.flush().unwrap();

            // Do not send terminal chunk or close connection until the client
            // has confirmed it aborted early upon exceeding limit.
            let _ = release_rx.recv_timeout(Duration::from_secs(10));
        });

        let guard = ServerGuard::new(release_tx, server_handle);

        let mut brave = BraveSearch::new("dummy-key").expect("brave search");
        brave.endpoint = format!("http://127.0.0.1:{port}/res/v1/web/search");
        brave.max_fetch_bytes = 1000;

        let res = tokio::time::timeout(Duration::from_secs(10), brave.search("rust", 5))
            .await
            .expect("watchdog timeout: client should not wait for server EOF")
            .expect_err("must fail with BodyTooLarge");

        assert_eq!(res.diagnostic.class, FailureClass::BodyTooLarge);
        assert_eq!(res.diagnostic.provider.as_deref(), Some("brave"));

        guard.release_and_join();
    }

    #[tokio::test]
    async fn configured_brave_preserves_empty_results_and_search_errors() {
        for body in ["{}", "invalid json"] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            listener.set_nonblocking(true).unwrap();
            let server = thread::spawn(move || {
                let deadline = std::time::Instant::now() + Duration::from_secs(10);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(
                                std::time::Instant::now() < deadline,
                                "missing Brave request"
                            );
                            thread::sleep(Duration::from_millis(1));
                        }
                        Err(error) => panic!("accept: {error}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                read_http_request(&mut stream).unwrap();
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .unwrap();
            });
            let mut brave = BraveSearch::new("synthetic").unwrap();
            brave.endpoint = format!("http://{address}/search");
            let fallback = crate::DuckDuckGoHtmlSearch::new(Arc::new(
                crate::FixtureTransport::new().insert_html(
                    "https://html.duckduckgo.com/html/?q=rust",
                    r#"<a class="result__a" href="https://example.com/rust">Rust</a>"#,
                ),
            ));
            let access = crate::WebAccess::with_fixtures(
                crate::FixtureTransport::new(),
                BraveOrFallback::new(
                    None,
                    Arc::new(BraveOrFallback::new(Some(brave), Arc::new(fallback))),
                ),
            );
            let result = access.search("rust", Some(5)).await;
            server.join().unwrap();
            if body == "{}" {
                let response = result.unwrap();
                assert_eq!(response.provider, "brave");
                assert!(response.citations.is_empty());
            } else {
                let error = result.unwrap_err();
                assert_eq!(error.diagnostic.provider.as_deref(), Some("brave"));
            }
        }
    }

    #[test]
    fn parse_brave_json_extracts_hits() {
        let json_body = br#"{
            "web": {
                "results": [
                    {
                        "title": "Rust Programming Language",
                        "url": "https://www.rust-lang.org/",
                        "description": "A language empowering everyone to build reliable and efficient software."
                    },
                    {
                        "title": "Invalid Non-HTTP",
                        "url": "ftp://example.com/file",
                        "description": "Ignored non-http url"
                    }
                ]
            }
        }"#;
        let hits = parse_brave_json(json_body, 10).expect("parse hits");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title.as_deref(), Some("Rust Programming Language"));
        assert_eq!(hits[0].url, "https://www.rust-lang.org/");
        assert_eq!(hits[0].rank, 1);
    }

    async fn paged_fixture(
        limit: u32,
        pages: Vec<(Vec<u32>, Option<bool>)>,
    ) -> crate::types::SearchResult {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let server = thread::spawn(move || {
            for (offset, (ids, more)) in pages.into_iter().enumerate() {
                let deadline = std::time::Instant::now() + Duration::from_secs(10);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(
                                std::time::Instant::now() < deadline,
                                "missing page {offset}"
                            );
                            thread::sleep(Duration::from_millis(1));
                        }
                        Err(e) => panic!("accept: {e}"),
                    }
                };
                // macOS can inherit O_NONBLOCK from the listener; this fixture
                // reads a complete request synchronously on its own thread.
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                let mut request = Vec::new();
                let mut byte = [0];
                while !request.ends_with(b"\r\n\r\n") {
                    stream.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                }
                let request = String::from_utf8(request).unwrap();
                assert!(request.contains(&format!("count={}", limit.min(20))));
                assert!(request.contains(&format!("offset={offset}")));
                let results: Vec<_> = ids.into_iter().map(|id| serde_json::json!({
                    "url": format!("https://example.com/{id}"), "title": format!("Result {id}")
                })).collect();
                let body = serde_json::json!({
                    "query": {"more_results_available": more},
                    "web": {"results": results}
                })
                .to_string();
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
            }
        });
        let mut brave = BraveSearch::new("fixture-key").unwrap();
        brave.endpoint = format!("http://{address}/search");
        let access = crate::WebAccess::new(
            Arc::new(crate::FixtureTransport::new()),
            Arc::new(brave),
            crate::ContentStore::new(),
            crate::Limits::default(),
        );
        let result = access.search("pages", Some(limit)).await;
        server.join().unwrap();
        result.unwrap()
    }

    #[tokio::test]
    async fn paginates_past_twenty_with_fixed_page_size_and_deduplicates() {
        let response = paged_fixture(
            50,
            vec![
                ((0..20).collect(), Some(true)),
                ((19..39).collect(), Some(true)),
                ((39..59).collect(), Some(false)),
            ],
        )
        .await;
        assert_eq!(response.hits.len(), 50);
        assert_eq!(response.hits.last().unwrap().url, "https://example.com/49");
        assert_eq!(response.hits.last().unwrap().rank, 50);
        assert!(response.truncated);
    }

    #[tokio::test]
    async fn stops_when_provider_reports_exhaustion() {
        let response = paged_fixture(50, vec![((0..7).collect(), Some(false))]).await;
        assert_eq!(response.provider, "brave");
        assert!(
            response
                .citations
                .iter()
                .all(|citation| citation.provider.as_deref() == Some("brave"))
        );
        assert_eq!(response.hits.len(), 7);
        assert!(!response.truncated);
    }

    #[tokio::test]
    async fn provider_page_limit_is_reported_as_incomplete() {
        let pages = (0..10)
            .map(|page| ((page * 20..page * 20 + 20).collect(), Some(true)))
            .collect();
        let response = paged_fixture(250, pages).await;
        assert_eq!(response.hits.len(), 200);
        assert!(response.truncated);
        assert!(
            response
                .evidence
                .unwrap()
                .source_summary
                .unwrap()
                .contains("offset (9)")
        );
    }

    #[tokio::test]
    async fn absent_pagination_signal_is_not_reported_as_complete() {
        let response = paged_fixture(50, vec![((0..20).collect(), None)]).await;
        assert_eq!(response.hits.len(), 20);
        assert!(response.truncated);
    }

    #[tokio::test]
    async fn exact_requested_count_with_exhaustion_is_complete_through_service() {
        let response = paged_fixture(20, vec![((0..20).collect(), Some(false))]).await;
        assert_eq!(response.hits.len(), 20);
        assert!(!response.truncated);
    }
}
