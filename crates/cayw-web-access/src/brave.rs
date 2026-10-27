//! Optional Brave Search API backend (env-gated key).

use std::sync::Arc;

use crate::error::WebAccessError;
use crate::provider::{BoxFuture, SearchProvider};
use crate::types::{FailureClass, SearchHit};

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
            max_fetch_bytes: 2 * 1024 * 1024,
            endpoint: ENDPOINT.to_string(),
        })
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
    ) -> BoxFuture<'a, Result<Vec<SearchHit>, WebAccessError>> {
        Box::pin(async move {
            let mut url = url::Url::parse(&self.endpoint).map_err(|e| {
                WebAccessError::new(FailureClass::InvalidUrl, format!("brave endpoint: {e}"))
                    .with_provider(PROVIDER)
            })?;
            url.query_pairs_mut()
                .append_pair("q", query)
                .append_pair("count", &limit.clamp(1, 20).to_string());
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
            let (status, body) =
                crate::provider::read_bounded_body(response, self.max_fetch_bytes, None, PROVIDER)
                    .await?;
            if !(200..300).contains(&status) {
                return Err(WebAccessError::new(
                    FailureClass::HttpStatus,
                    format!("brave HTTP {status}"),
                )
                .with_provider(PROVIDER)
                .with_status(status));
            }
            parse_brave_json(&body, limit)
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
    ) -> BoxFuture<'a, Result<Vec<SearchHit>, WebAccessError>> {
        Box::pin(async move {
            if let Some(brave) = &self.brave {
                match brave.search(query, limit).await {
                    Ok(hits) if !hits.is_empty() => return Ok(hits),
                    Ok(_) => { /* empty → fallback */ }
                    Err(_) => { /* soft-fallback to DDG/fixture */ }
                }
            }
            self.fallback.search(query, limit).await
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
}
