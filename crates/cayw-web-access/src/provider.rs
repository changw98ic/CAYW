//! Unified Provider surface for search / fetch / extract.

use std::collections::HashMap;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
use std::pin::Pin;
use std::time::Duration;

use url::Url;

use crate::error::WebAccessError;
use crate::types::{FailureClass, SearchEmptyReason, SearchEvidence, SearchHit, SearchResponse};

/// Maximum number of redirects followed for one fetch.
const MAX_REDIRECTS: usize = 5;
const USER_AGENT: &str = "cayw-web-access/0.1 (F10 first cut)";

/// Async result alias used by provider futures.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Raw HTTP response returned by a transport.
#[derive(Debug, Clone)]
pub struct RawResponse {
    pub status: u16,
    pub final_url: String,
    pub mime_type: Option<String>,
    pub body: Vec<u8>,
}

/// Pluggable HTTP transport. Offline fixtures implement this without sockets.
pub trait HttpTransport: Send + Sync {
    fn get<'a>(
        &'a self,
        url: &'a str,
        max_bytes: usize,
    ) -> BoxFuture<'a, Result<RawResponse, WebAccessError>>;
}

/// Pluggable web search backend.
pub trait SearchProvider: Send + Sync {
    fn name(&self) -> &str;
    fn search<'a>(
        &'a self,
        query: &'a str,
        limit: u32,
    ) -> BoxFuture<'a, Result<SearchResponse, WebAccessError>>;
}

/// In-memory fixture transport: URL → canned body. Missing URLs fail as Offline.
#[derive(Debug, Clone)]
pub struct FixtureTransport {
    pages: HashMap<String, RawResponse>,
    pub offline: bool,
}

impl Default for FixtureTransport {
    fn default() -> Self {
        let mut transport = Self {
            pages: HashMap::new(),
            offline: false,
        };
        transport.pages.insert(
            "https://example.com/".into(),
            RawResponse {
                status: 200,
                final_url: "https://example.com/".into(),
                mime_type: Some("text/html; charset=utf-8".into()),
                body: b"<!doctype html><html><head><title>Example Domain</title></head><body><h1>Example Domain</h1><p>This domain is for use in documentation examples.</p></body></html>".to_vec(),
            },
        );
        transport
    }
}

impl FixtureTransport {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert_html(mut self, url: impl Into<String>, html: impl Into<String>) -> Self {
        let url = url.into();
        let body = html.into().into_bytes();
        self.pages.insert(
            url.clone(),
            RawResponse {
                status: 200,
                final_url: url,
                mime_type: Some("text/html; charset=utf-8".into()),
                body,
            },
        );
        self
    }

    pub fn insert_bytes(
        mut self,
        url: impl Into<String>,
        mime: impl Into<String>,
        body: Vec<u8>,
    ) -> Self {
        let url = url.into();
        self.pages.insert(
            url.clone(),
            RawResponse {
                status: 200,
                final_url: url,
                mime_type: Some(mime.into()),
                body,
            },
        );
        self
    }

    pub fn offline(mut self) -> Self {
        self.offline = true;
        self
    }
}

impl HttpTransport for FixtureTransport {
    fn get<'a>(
        &'a self,
        url: &'a str,
        max_bytes: usize,
    ) -> BoxFuture<'a, Result<RawResponse, WebAccessError>> {
        Box::pin(async move {
            if self.offline {
                return Err(
                    WebAccessError::offline("fixture transport is in offline mode")
                        .with_url(url)
                        .with_provider("fixture"),
                );
            }
            let Some(page) = self.pages.get(url) else {
                return Err(WebAccessError::new(
                    FailureClass::NotFound,
                    format!("no fixture for URL {url}"),
                )
                .with_url(url)
                .with_provider("fixture"));
            };
            if page.body.len() > max_bytes {
                return Err(WebAccessError::new(
                    FailureClass::BodyTooLarge,
                    format!("body {} exceeds max_bytes {max_bytes}", page.body.len()),
                )
                .with_url(url));
            }
            Ok(page.clone())
        })
    }
}

/// Fixture search provider with a fixed hit table keyed by query substring.
#[derive(Debug, Default, Clone)]
pub struct FixtureSearch {
    hits: HashMap<String, Vec<SearchHit>>,
    pub offline: bool,
}

impl FixtureSearch {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(mut self, query: impl Into<String>, hits: Vec<SearchHit>) -> Self {
        self.hits.insert(query.into(), hits);
        self
    }

    pub fn offline(mut self) -> Self {
        self.offline = true;
        self
    }
}

impl SearchProvider for FixtureSearch {
    fn name(&self) -> &str {
        "fixture"
    }

    fn search<'a>(
        &'a self,
        query: &'a str,
        limit: u32,
    ) -> BoxFuture<'a, Result<SearchResponse, WebAccessError>> {
        Box::pin(async move {
            if self.offline {
                return Err(
                    WebAccessError::offline("fixture search provider is offline")
                        .with_provider("fixture"),
                );
            }
            let mut hits = self
                .hits
                .get(query)
                .cloned()
                .or_else(|| {
                    self.hits.iter().find_map(|(k, v)| {
                        if query.contains(k) || k.contains(query) {
                            Some(v.clone())
                        } else {
                            None
                        }
                    })
                })
                .unwrap_or_default();
            if hits.len() as u32 > limit {
                hits.truncate(limit as usize);
            }
            if hits.is_empty() {
                // Without HTTP original text, must not attribute to TrueEmpty/Challenge/StructuralMismatch.
                Ok(SearchResponse::empty(
                    SearchEmptyReason::Unattributed,
                    Some(SearchEvidence {
                        http_status: Some(200),
                        source_summary: Some("fixture search miss without HTTP response".into()),
                        response_snippet: None,
                    }),
                ))
            } else {
                Ok(SearchResponse::new(hits))
            }
        })
    }
}

/// Live `reqwest` transport (used when the host opts into network).
///
/// Target policy: the initial URL and every redirect must resolve to a public
/// address. Hostnames are resolved before the request and pinned into the
/// per-request client so a later DNS answer cannot redirect the connection to
/// a private address. Redirects are followed manually, at most
/// [`MAX_REDIRECTS`] times. A single 30-second budget covers DNS, redirects,
/// and reading the final body.
#[derive(Debug, Clone)]
pub struct ReqwestTransport {
    timeout: Duration,
    strict_targets: bool,
}

impl ReqwestTransport {
    pub fn new() -> Result<Self, WebAccessError> {
        Ok(Self {
            timeout: Duration::from_secs(30),
            strict_targets: true,
        })
    }

    /// Local-fixture constructor for integration tests that must connect to a
    /// loopback HTTP server. It skips public-target checks and must never be
    /// used by the Host; production code uses [`ReqwestTransport::new`]. It is
    /// absent from release builds (`debug_assertions` off).
    #[cfg(any(test, debug_assertions))]
    #[doc(hidden)]
    pub fn for_local_tests() -> Self {
        Self {
            timeout: Duration::from_secs(30),
            strict_targets: false,
        }
    }

    fn build_client(
        &self,
        pin: Option<(&str, &[SocketAddr])>,
    ) -> Result<reqwest::Client, WebAccessError> {
        let mut builder = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            // A system proxy would resolve the hostname outside this process and
            // bypass the public-address pin that enforces the SSRF boundary.
            .no_proxy()
            .timeout(self.timeout)
            .user_agent(USER_AGENT);
        if let Some((host, addrs)) = pin {
            builder = builder.resolve_to_addrs(host, addrs);
        }
        builder.build().map_err(|e| {
            WebAccessError::new(
                FailureClass::ProviderUnavailable,
                format!("failed to build HTTP client: {e}"),
            )
            .with_provider("reqwest")
        })
    }
}

impl Default for ReqwestTransport {
    fn default() -> Self {
        Self::new().expect("reqwest client")
    }
}

impl HttpTransport for ReqwestTransport {
    fn get<'a>(
        &'a self,
        url: &'a str,
        max_bytes: usize,
    ) -> BoxFuture<'a, Result<RawResponse, WebAccessError>> {
        self.get_with_resolver(url, max_bytes, |url| {
            Box::pin(async move { resolve_public_addrs(&url).await })
        })
    }
}

impl ReqwestTransport {
    // Keep the resolver injectable for deterministic deadline tests without DNS
    // or external network access. The production entry point always validates
    // and pins public addresses using resolve_public_addrs.
    fn get_with_resolver<'a, R>(
        &'a self,
        url: &'a str,
        max_bytes: usize,
        resolve: R,
    ) -> BoxFuture<'a, Result<RawResponse, WebAccessError>>
    where
        R: Fn(Url) -> BoxFuture<'static, Result<ResolvedTarget, WebAccessError>> + Send + 'a,
    {
        Box::pin(with_request_deadline(self.timeout, url, async move {
            let mut current =
                Url::parse(url).map_err(|e| invalid_url(url, format!("invalid URL: {e}")))?;
            let mut redirects = 0usize;
            loop {
                if !matches!(current.scheme(), "http" | "https") {
                    return Err(unsupported_scheme(&current));
                }
                let pin = if self.strict_targets {
                    validate_public_url(&current)?;
                    resolve(current.clone()).await?.pin
                } else {
                    None
                };
                let client = self.build_client(
                    pin.as_ref()
                        .map(|(host, addrs)| (host.as_str(), addrs.as_slice())),
                )?;
                let response = client.get(current.clone()).send().await.map_err(|e| {
                    let class = if e.is_timeout() {
                        FailureClass::Timeout
                    } else if e.is_connect() {
                        FailureClass::Dns
                    } else if e.is_request() {
                        FailureClass::InvalidUrl
                    } else {
                        FailureClass::Offline
                    };
                    WebAccessError::new(class, format!("HTTP GET failed: {e}"))
                        .with_url(display_url(&current))
                        .with_provider("reqwest")
                })?;
                let status = response.status().as_u16();
                if is_redirect(status) {
                    if redirects >= MAX_REDIRECTS {
                        return Err(WebAccessError::new(
                            FailureClass::InvalidUrl,
                            format!("too many redirects (maximum {MAX_REDIRECTS})"),
                        )
                        .with_url(display_url(&current))
                        .with_provider("reqwest"));
                    }
                    redirects += 1;
                    let location = response
                        .headers()
                        .get(reqwest::header::LOCATION)
                        .and_then(|value| value.to_str().ok())
                        .filter(|value| !value.is_empty())
                        .ok_or_else(|| {
                            WebAccessError::new(
                                FailureClass::InvalidUrl,
                                "redirect response is missing a valid Location header",
                            )
                            .with_url(display_url(&current))
                            .with_provider("reqwest")
                        })?;
                    current = redirect_target(&current, location, self.strict_targets)?;
                    continue;
                }
                let mime_type = response
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string);
                // With redirects disabled, the response URL is the URL that was
                // requested. Re-resolve happened above for this exact hop.
                let final_url = current.to_string();
                let (status, body) =
                    read_bounded_body(response, max_bytes, Some(&final_url), "reqwest").await?;
                if !(200..300).contains(&status) {
                    return Err(WebAccessError::new(
                        FailureClass::HttpStatus,
                        format!("HTTP status {status}"),
                    )
                    .with_url(final_url)
                    .with_status(status)
                    .with_provider("reqwest"));
                }
                return Ok(RawResponse {
                    status,
                    final_url,
                    mime_type,
                    body,
                });
            }
        }))
    }
}

/// One budget for DNS, all redirect hops, and the final streamed body. Per-hop
/// reqwest timeouts remain a backstop, not a fresh budget for each redirect.
async fn with_request_deadline<T>(
    timeout: Duration,
    url: &str,
    request: impl Future<Output = Result<T, WebAccessError>>,
) -> Result<T, WebAccessError> {
    tokio::time::timeout(timeout, request).await.map_err(|_| {
        let safe_url = Url::parse(url)
            .map(|url| display_url(&url))
            .unwrap_or_else(|_| "<invalid URL>".to_string());
        WebAccessError::new(FailureClass::Timeout, "HTTP GET total deadline exceeded")
            .with_url(safe_url)
            .with_provider("reqwest")
    })?
}

fn is_redirect(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

/// A URL that passed host/scheme checks and, for hostnames, has pinned
/// addresses. IP literals need no pin because the URL already names the IP.
#[derive(Debug)]
struct ResolvedTarget {
    pin: Option<(String, Vec<SocketAddr>)>,
}

fn validate_public_url(url: &Url) -> Result<(), WebAccessError> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(unsupported_scheme(url));
    }
    match url.host() {
        Some(url::Host::Ipv4(ip)) if !ipv4_is_public(ip) => {
            Err(blocked_target(url, ip.to_string()))
        }
        Some(url::Host::Ipv6(ip)) if !ipv6_is_public(ip) => {
            Err(blocked_target(url, ip.to_string()))
        }
        Some(url::Host::Domain(domain)) if is_obviously_local_domain(domain) => {
            Err(blocked_target(url, domain.to_string()))
        }
        Some(_) => Ok(()),
        None => Err(
            WebAccessError::new(FailureClass::InvalidUrl, "URL has no host")
                .with_url(display_url(url)),
        ),
    }
}

/// Resolve a hostname and require every answer to be public. The returned
/// addresses are pinned into the request's client to prevent DNS rebinding.
async fn resolve_public_addrs(url: &Url) -> Result<ResolvedTarget, WebAccessError> {
    let port = url.port_or_known_default().ok_or_else(|| {
        WebAccessError::new(FailureClass::InvalidUrl, "URL has no usable port")
            .with_url(display_url(url))
    })?;
    match url.host() {
        Some(url::Host::Ipv4(ip)) => {
            if !ipv4_is_public(ip) {
                return Err(blocked_target(url, ip.to_string()));
            }
            Ok(ResolvedTarget { pin: None })
        }
        Some(url::Host::Ipv6(ip)) => {
            if !ipv6_is_public(ip) {
                return Err(blocked_target(url, ip.to_string()));
            }
            Ok(ResolvedTarget { pin: None })
        }
        Some(url::Host::Domain(domain)) => {
            let host = domain.to_string();
            let lookup = host.clone();
            // The outer request deadline bounds waiting for this task. A running
            // system resolver cannot be aborted; it may finish in the background,
            // but its late result cannot start an HTTP request after timeout.
            let addrs = tokio::task::spawn_blocking(move || {
                (lookup.as_str(), port)
                    .to_socket_addrs()
                    .map(|iter| iter.collect::<Vec<_>>())
            })
            .await
            .map_err(|e| {
                WebAccessError::new(
                    FailureClass::Dns,
                    format!("resolve {host}: resolver task failed: {e}"),
                )
                .with_url(display_url(url))
                .with_provider("reqwest")
            })?
            .map_err(|e| {
                WebAccessError::new(FailureClass::Dns, format!("resolve {host}: {e}"))
                    .with_url(display_url(url))
                    .with_provider("reqwest")
            })?;
            if addrs.is_empty() {
                return Err(WebAccessError::new(
                    FailureClass::Dns,
                    format!("resolve {host}: no addresses"),
                )
                .with_url(display_url(url))
                .with_provider("reqwest"));
            }
            if let Some(bad) = addrs.iter().find(|addr| !ip_is_public(addr.ip())) {
                return Err(blocked_target(
                    url,
                    format!("{host} resolves to {}", bad.ip()),
                ));
            }
            Ok(ResolvedTarget {
                pin: Some((host, addrs)),
            })
        }
        None => Err(
            WebAccessError::new(FailureClass::InvalidUrl, "URL has no host")
                .with_url(display_url(url)),
        ),
    }
}

fn redirect_target(current: &Url, location: &str, strict: bool) -> Result<Url, WebAccessError> {
    let next = current.join(location).map_err(|e| {
        WebAccessError::new(
            FailureClass::InvalidUrl,
            format!("invalid redirect location: {e}"),
        )
        .with_url(display_url(current))
        .with_provider("reqwest")
    })?;
    if !matches!(next.scheme(), "http" | "https") {
        return Err(unsupported_scheme(&next));
    }
    if strict {
        validate_public_url(&next)?;
    }
    Ok(next)
}

fn invalid_url(url: &str, message: impl Into<String>) -> WebAccessError {
    WebAccessError::new(FailureClass::InvalidUrl, message).with_url(url)
}

fn unsupported_scheme(url: &Url) -> WebAccessError {
    WebAccessError::new(
        FailureClass::UnsupportedScheme,
        format!("unsupported URL scheme {}", url.scheme()),
    )
    .with_url(display_url(url))
}

fn blocked_target(url: &Url, detail: impl std::fmt::Display) -> WebAccessError {
    WebAccessError::new(
        FailureClass::InvalidUrl,
        format!("refusing non-public web target: {detail}"),
    )
    .with_url(display_url(url))
    .with_provider("reqwest")
}

/// Strip userinfo before putting a URL into a diagnostic.
fn display_url(url: &Url) -> String {
    let mut safe = url.clone();
    let _ = safe.set_username("");
    let _ = safe.set_password(None);
    safe.to_string()
}

fn is_obviously_local_domain(domain: &str) -> bool {
    let domain = domain.strip_suffix('.').unwrap_or(domain);
    domain.eq_ignore_ascii_case("localhost")
        || domain
            .rsplit_once('.')
            .is_some_and(|(_, suffix)| suffix.eq_ignore_ascii_case("localhost"))
}

fn ip_is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => ipv4_is_public(ip),
        IpAddr::V6(ip) => ipv6_is_public(ip),
    }
}

fn ipv4_is_public(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_multicast()
        || ip.is_documentation()
        || octets[0] == 0
        || (octets[0] == 100 && (64..=127).contains(&octets[1]))
        || (octets[0] == 192 && octets[1] == 0 && octets[2] == 0)
        || (octets[0] == 192 && octets[1] == 88 && octets[2] == 99)
        || (octets[0] == 198 && matches!(octets[1], 18 | 19))
        || octets[0] >= 240)
}

fn ipv6_is_public(ip: Ipv6Addr) -> bool {
    if ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_multicast()
        || ip.is_unique_local()
        || ip.is_unicast_link_local()
        || ip.to_ipv4_mapped().is_some()
        || ip.to_ipv4().is_some()
    {
        return false;
    }
    let segments = ip.segments();
    // Global unicast is 2000::/3. Everything outside it is special-purpose.
    if segments[0] & 0xe000 != 0x2000 {
        return false;
    }
    // Documentation 2001:db8::/32 and 3fff::/20.
    if (segments[0] == 0x2001 && segments[1] == 0x0db8)
        || (segments[0] == 0x3fff && (segments[1] & 0xf000) == 0)
    {
        return false;
    }
    // 6to4 2002::/16 and Teredo 2001::/32 can embed private IPv4.
    if segments[0] == 0x2002 || (segments[0] == 0x2001 && segments[1] == 0) {
        return false;
    }
    // NAT64 64:ff9b::/96 can map private IPv4.
    if segments[0] == 0x0064 && segments[1] == 0xff9b {
        return false;
    }
    true
}

/// Reads an HTTP response body with streaming byte accumulation bounded by `max_bytes`.
///
/// Terminating early when the accumulated byte count exceeds `max_bytes` ensures that
/// neither chunked streams without Content-Length nor responses with missing/forged
/// lengths cause unbounded memory allocations or wait indefinitely for full body completion.
pub(crate) async fn read_bounded_body(
    mut response: reqwest::Response,
    max_bytes: usize,
    url: Option<&str>,
    provider: &str,
) -> Result<(u16, Vec<u8>), WebAccessError> {
    let status = response.status().as_u16();

    // Fast-path: check declared Content-Length header, but never rely on it alone.
    if let Some(content_length) = response.content_length()
        && content_length > max_bytes as u64
    {
        let mut err = WebAccessError::new(
            FailureClass::BodyTooLarge,
            format!("body {content_length} exceeds max_bytes {max_bytes}"),
        )
        .with_provider(provider)
        .with_status(status);
        if let Some(u) = url {
            err = err.with_url(u);
        }
        return Err(err);
    }

    let mut accumulated: usize = 0;
    let mut body = Vec::new();

    while let Some(chunk) = response.chunk().await.map_err(|e| {
        let class = if e.is_timeout() {
            FailureClass::Timeout
        } else {
            FailureClass::Offline
        };
        let mut err = WebAccessError::new(class, format!("failed to read body: {e}"))
            .with_provider(provider)
            .with_status(status);
        if let Some(u) = url {
            err = err.with_url(u);
        }
        err
    })? {
        accumulated = accumulated.saturating_add(chunk.len());
        if accumulated > max_bytes {
            // Drop response immediately to terminate the underlying transport
            drop(response);
            let mut err = WebAccessError::new(
                FailureClass::BodyTooLarge,
                format!("body {accumulated} exceeds max_bytes {max_bytes}"),
            )
            .with_provider(provider)
            .with_status(status);
            if let Some(u) = url {
                err = err.with_url(u);
            }
            return Err(err);
        }
        body.extend_from_slice(&chunk);
    }

    Ok((status, body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    #[tokio::test(start_paused = true)]
    async fn total_deadline_includes_public_dns_resolution() {
        let transport = ReqwestTransport::new().unwrap();
        let started = tokio::time::Instant::now();
        let error = transport
            .get_with_resolver("https://user:secret@example.com/", 1024, |_| {
                Box::pin(std::future::pending())
            })
            .await
            .unwrap_err();
        assert_eq!(error.diagnostic().class, FailureClass::Timeout);
        assert_eq!(error.diagnostic().provider.as_deref(), Some("reqwest"));
        assert_eq!(error.diagnostic().url.as_deref(), Some("https://example.com/"));
        // This is Tokio's virtual clock, not an elapsed wall-time assertion.
        assert_eq!(started.elapsed(), transport.timeout);
    }

    #[tokio::test(start_paused = true)]
    async fn total_deadline_does_not_restart_between_request_phases() {
        use std::cell::Cell;

        // Model DNS, redirect hops and a streamed body with deterministic waits.
        // No individual timer exceeds the budget, but their sum does.
        // Exercise expiry in each phase and check that no later phase runs.
        for blocked_phase in 0..4 {
            let completed = Cell::new(0);
            let started = tokio::time::Instant::now();
            let error = with_request_deadline(
                Duration::from_secs(30),
                "https://example.com/",
                async {
                    for phase in 0..4 {
                        let duration = if phase == blocked_phase { 30 } else { 5 };
                        tokio::time::sleep(Duration::from_secs(duration)).await;
                        // Make the boundary case unambiguously pending at expiry.
                        if phase == blocked_phase {
                            std::future::pending::<()>().await;
                        }
                        completed.set(completed.get() + 1);
                    }
                    Ok(())
                },
            )
            .await
            .unwrap_err();
            assert_eq!(error.diagnostic().class, FailureClass::Timeout);
            assert_eq!(completed.get(), blocked_phase);
            assert_eq!(started.elapsed(), Duration::from_secs(30));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn total_deadline_preserves_success_and_request_errors() {
        let result = with_request_deadline(Duration::from_secs(30), "https://example.com/", async {
            tokio::time::sleep(Duration::from_secs(10)).await;
            Ok(42)
        })
        .await;
        assert_eq!(result, Ok(42));

        let expected = blocked_target(&Url::parse("http://127.0.0.1/").unwrap(), "loopback");
        let result = with_request_deadline(Duration::from_secs(30), "http://127.0.0.1/", async {
            Err::<(), _>(expected.clone())
        })
        .await;
        assert_eq!(result, Err(expected));
    }

    #[test]
    fn ip_policy_rejects_non_public_targets() {
        let v4 = [
            "127.0.0.1",
            "10.0.0.1",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "0.0.0.0",
            "224.0.0.1",
            "255.255.255.255",
            "100.64.0.1",
            "198.18.0.1",
            "192.88.99.1",
            "240.0.0.1",
            "192.0.2.1",
            "198.51.100.1",
            "203.0.113.1",
        ];
        for raw in v4 {
            let ip: Ipv4Addr = raw.parse().unwrap();
            assert!(!ip_is_public(IpAddr::V4(ip)), "{raw} must be rejected");
        }
        let v6 = [
            "::1",
            "::",
            "fc00::1",
            "fd00::1",
            "fe80::1",
            "ff02::1",
            "::ffff:127.0.0.1",
            "2001:db8::1",
            "3fff::1",
            "2002::1",
            "2001::1",
            "64:ff9b::1",
        ];
        for raw in v6 {
            let ip: Ipv6Addr = raw.parse().unwrap();
            assert!(!ip_is_public(IpAddr::V6(ip)), "{raw} must be rejected");
        }
    }

    #[test]
    fn ip_policy_allows_public_targets() {
        for raw in [
            "1.1.1.1",
            "8.8.8.8",
            "93.184.216.34",
            "2606:4700:4700::1111",
            "2001:4860:4860::8888",
            "2a00:1450:4001:81b::200e",
        ] {
            let ip: IpAddr = raw.parse().unwrap();
            assert!(ip_is_public(ip), "{raw} must be allowed");
        }
    }

    #[test]
    fn validate_public_url_rejects_local_and_special_hosts() {
        for raw in [
            "http://127.0.0.1/",
            "http://[::1]/",
            "http://169.254.169.254/",
            "http://10.0.0.1/",
            "http://192.168.1.1/",
            "http://172.16.0.1/",
            "http://0.0.0.0/",
            "http://224.0.0.1/",
            "http://[fc00::1]/",
            "http://[fe80::1]/",
            "http://[::ffff:127.0.0.1]/",
            "http://localhost/",
            "http://LOCALHOST./",
            "http://foo.localhost/",
            "file:///etc/passwd",
            "ftp://example.com/",
        ] {
            let url = Url::parse(raw).unwrap();
            assert!(validate_public_url(&url).is_err(), "{raw} must be rejected");
        }
        assert!(validate_public_url(&Url::parse("https://example.com/a?b=c").unwrap()).is_ok());
    }

    #[tokio::test]
    async fn resolve_public_addrs_rejects_localhost() {
        let url = Url::parse("http://localhost:8080/").unwrap();
        let error = resolve_public_addrs(&url)
            .await
            .expect_err("localhost must not resolve to a public target");
        assert!(
            error.diagnostic().message.contains("non-public")
                || error.diagnostic().class == FailureClass::Dns,
            "unexpected diagnostic: {error}"
        );
    }

    #[test]
    fn redirect_target_rejects_non_public_locations() {
        let base = Url::parse("https://example.com/start").unwrap();
        for location in [
            "http://127.0.0.1/",
            "http://169.254.169.254/latest/meta-data/",
            "http://10.0.0.1/",
            "http://192.168.1.1/",
            "http://[::1]/",
            "http://[::ffff:127.0.0.1]/",
            "http://localhost/",
            "ftp://example.com/file",
        ] {
            assert!(
                redirect_target(&base, location, true).is_err(),
                "{location} must be rejected as a redirect target"
            );
        }
        let allowed = redirect_target(&base, "https://example.com/next", true).unwrap();
        assert_eq!(allowed.as_str(), "https://example.com/next");
    }

    fn spawn_redirect_server(total: usize, finish: bool) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = thread::spawn(move || {
            for index in 0..total {
                let (mut stream, _) = listener.accept().unwrap();
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let response = if finish && index + 1 == total {
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 5\r\nConnection: close\r\n\r\nfinal"
                } else {
                    "HTTP/1.1 302 Found\r\nLocation: /next\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                };
                let _ = stream.write_all(response.as_bytes());
            }
        });
        (format!("http://127.0.0.1:{port}/start"), handle)
    }

    #[tokio::test]
    async fn pinned_resolution_connects_to_validated_address() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
        });

        let transport = ReqwestTransport::new().unwrap();
        let address: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let client = transport
            .build_client(Some(("pinned.example", &[address])))
            .unwrap();
        let response = client
            .get(format!("http://pinned.example:{port}/"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.text().await.unwrap(), "ok");
        handle.join().unwrap();
    }

    #[tokio::test]
    async fn pinned_resolution_ignores_system_proxy() {
        const CHILD: &str = "CAYW_WEB_ACCESS_PROXY_CHILD";
        if std::env::var(CHILD).as_deref() != Ok("child") {
            let proxy = TcpListener::bind("127.0.0.1:0").unwrap();
            let proxy_url = format!("http://{}", proxy.local_addr().unwrap());
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "provider::tests::pinned_resolution_ignores_system_proxy",
                    "--nocapture",
                ])
                .env(CHILD, "child")
                .env("HTTP_PROXY", &proxy_url)
                .env("HTTPS_PROXY", &proxy_url)
                .env("ALL_PROXY", &proxy_url)
                .env("http_proxy", &proxy_url)
                .env("https_proxy", &proxy_url)
                .env("all_proxy", &proxy_url)
                .env("NO_PROXY", "")
                .env("no_proxy", "")
                .status()
                .unwrap();
            assert!(status.success(), "system proxy bypassed the pinned target");
            return;
        }

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\ndirect",
            );
        });
        let transport = ReqwestTransport {
            timeout: Duration::from_secs(2),
            strict_targets: true,
        };
        let client = transport
            .build_client(Some(("proxy-proof.example", &[address])))
            .unwrap();
        let response = client
            .get(format!("http://proxy-proof.example:{}/", address.port()))
            .send()
            .await
            .unwrap();
        assert_eq!(response.text().await.unwrap(), "direct");
        handle.join().unwrap();
    }

    #[tokio::test]
    async fn follows_redirects_and_enforces_limit() {
        let transport = ReqwestTransport::for_local_tests();

        let (url, handle) = spawn_redirect_server(3, true);
        let response = transport.get(&url, 1024).await.unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"final");
        assert!(response.final_url.ends_with("/next"));
        handle.join().unwrap();

        let (url, handle) = spawn_redirect_server(6, false);
        let error = transport.get(&url, 1024).await.unwrap_err();
        assert!(
            error.diagnostic().message.contains("too many redirects"),
            "unexpected error: {error}"
        );
        handle.join().unwrap();
    }
}
