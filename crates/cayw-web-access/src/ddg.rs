//! DuckDuckGo HTML search backend (no API key).

use std::sync::Arc;

use crate::error::WebAccessError;
use crate::provider::{BoxFuture, HttpTransport, SearchProvider};
use crate::types::{FailureClass, SearchHit};

const PROVIDER: &str = "duckduckgo_html";
const ENDPOINT: &str = "https://html.duckduckgo.com/html/";

/// Web search via DuckDuckGo's HTML results page. Needs no paid key.
///
/// Uses the shared [`HttpTransport`] so tests can fixture the results HTML.
#[derive(Clone)]
pub struct DuckDuckGoHtmlSearch {
    transport: Arc<dyn HttpTransport>,
    max_fetch_bytes: usize,
}

impl DuckDuckGoHtmlSearch {
    pub fn new(transport: Arc<dyn HttpTransport>) -> Self {
        Self {
            transport,
            max_fetch_bytes: 2 * 1024 * 1024,
        }
    }

    pub fn with_max_fetch_bytes(mut self, max: usize) -> Self {
        self.max_fetch_bytes = max;
        self
    }

    fn search_url(query: &str) -> Result<String, WebAccessError> {
        let mut url = url::Url::parse(ENDPOINT).map_err(|e| {
            WebAccessError::new(
                FailureClass::InvalidUrl,
                format!("duckduckgo endpoint: {e}"),
            )
            .with_provider(PROVIDER)
        })?;
        url.query_pairs_mut().append_pair("q", query);
        Ok(url.into())
    }
}

impl SearchProvider for DuckDuckGoHtmlSearch {
    fn name(&self) -> &str {
        PROVIDER
    }

    fn search<'a>(
        &'a self,
        query: &'a str,
        limit: u32,
    ) -> BoxFuture<'a, Result<Vec<SearchHit>, WebAccessError>> {
        Box::pin(async move {
            let url = Self::search_url(query)?;
            let raw = self
                .transport
                .get(&url, self.max_fetch_bytes)
                .await
                .map_err(|e| {
                    let mut err = e;
                    if err.diagnostic.provider.is_none() {
                        err = err.with_provider(PROVIDER);
                    }
                    err
                })?;
            let html = String::from_utf8_lossy(&raw.body);
            let mut hits = parse_duckduckgo_html(&html);
            if hits.len() as u32 > limit {
                hits.truncate(limit as usize);
            }
            for (i, hit) in hits.iter_mut().enumerate() {
                hit.rank = (i as u32) + 1;
            }
            Ok(hits)
        })
    }
}

/// Parse DuckDuckGo HTML result anchors (`result__a`) into hits.
///
/// Resolves `uddg=` redirect targets when present. Intentionally small and
/// tolerant — DDG markup drifts; empty results are a soft miss, not a panic.
pub fn parse_duckduckgo_html(html: &str) -> Vec<SearchHit> {
    let mut hits: Vec<SearchHit> = Vec::new();
    let mut rest = html;
    while let Some(idx) = rest.find("result__a") {
        let window = &rest[idx..];
        let Some(tag_start) = window.rfind('<') else {
            rest = &rest[idx + 9..];
            continue;
        };
        // Prefer scanning forward from result__a for href and title text.
        let after = &window[9..];
        let href = extract_attr(window, "href")
            .or_else(|| extract_attr(after, "href"))
            .or_else(|| {
                // class may appear before href on the same tag starting earlier
                let tag = &window[tag_start..];
                let end = tag.find('>').unwrap_or(tag.len().min(400));
                extract_attr(&tag[..end], "href")
            });
        let Some(href) = href else {
            rest = &rest[idx + 9..];
            continue;
        };
        let title = anchor_inner_text(window).unwrap_or_default();
        let url = resolve_ddg_href(&href);
        if url.starts_with("http://") || url.starts_with("https://") {
            // Dedup by URL.
            if !hits.iter().any(|h| h.url == url) {
                let snippet = nearby_snippet(rest, idx);
                hits.push(SearchHit {
                    url,
                    title: nonempty(title),
                    snippet,
                    rank: (hits.len() as u32) + 1,
                });
            }
        }
        rest = &rest[idx + 9..];
    }
    hits
}

fn nonempty(s: String) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(decode_basic_entities(t))
    }
}

fn extract_attr(s: &str, name: &str) -> Option<String> {
    let patterns = [
        format!("{name}=\""),
        format!("{name}='"),
        format!("{name} = \""),
        format!("{name} = '"),
    ];
    for (i, pat) in patterns.iter().enumerate() {
        if let Some(pos) = s.find(pat) {
            let start = pos + pat.len();
            let quote = if i % 2 == 0 { '"' } else { '\'' };
            let end = s[start..].find(quote)? + start;
            return Some(s[start..end].to_string());
        }
    }
    None
}

fn anchor_inner_text(from_class: &str) -> Option<String> {
    let gt = from_class.find('>')?;
    let after = &from_class[gt + 1..];
    let close = after.find("</a>")?;
    let raw = after[..close].to_string();
    // Strip nested tags lightly.
    let mut out = String::new();
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '<' {
            while chars.peek().is_some_and(|x| *x != '>') {
                chars.next();
            }
            chars.next();
        } else {
            out.push(c);
        }
    }
    Some(out)
}

fn nearby_snippet(full: &str, result_idx: usize) -> Option<String> {
    let window_end = (result_idx + 1200).min(full.len());
    let window = &full[result_idx..window_end];
    for marker in ["result__snippet", "result__body"] {
        if let Some(m) = window.find(marker) {
            let slice = &window[m..];
            if let Some(gt) = slice.find('>') {
                let after = &slice[gt + 1..];
                let end = after.find("</").unwrap_or_else(|| after.len().min(280));
                let text = strip_tags(&after[..end]);
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    return Some(decode_basic_entities(trimmed));
                }
            }
        }
    }
    None
}

fn strip_tags(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '<' {
            while chars.peek().is_some_and(|x| *x != '>') {
                chars.next();
            }
            chars.next();
        } else {
            out.push(c);
        }
    }
    out
}

fn decode_basic_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
}

/// Unwrap DuckDuckGo redirect links (`uddg=`).
pub fn resolve_ddg_href(href: &str) -> String {
    let decoded = decode_basic_entities(href);
    if let Ok(url) = url::Url::parse(&decoded) {
        for (k, v) in url.query_pairs() {
            if k == "uddg" {
                return v.to_string();
            }
        }
    }
    if let Some(pos) = decoded.find("uddg=") {
        let rest = &decoded[pos + 5..];
        let end = rest.find('&').unwrap_or(rest.len());
        let enc = &rest[..end];
        let fake = format!("https://x.invalid/?uddg={enc}");
        if let Ok(u) = url::Url::parse(&fake) {
            for (k, v) in u.query_pairs() {
                if k == "uddg" {
                    return v.to_string();
                }
            }
        }
    }
    decoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_result_anchors_and_uddg() {
        let html = r#"
        <div class="result">
          <a rel="nofollow" class="result__a" href="https://duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fdocs&amp;rut=abc">Example Docs</a>
          <a class="result__snippet">A short snippet about docs.</a>
        </div>
        <div class="result">
          <a class="result__a" href="https://rust-lang.org/">Rust</a>
        </div>
        "#;
        let hits = parse_duckduckgo_html(html);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].url, "https://example.com/docs");
        assert_eq!(hits[0].title.as_deref(), Some("Example Docs"));
        assert!(
            hits[0]
                .snippet
                .as_deref()
                .unwrap_or("")
                .contains("short snippet")
        );
        assert_eq!(hits[1].url, "https://rust-lang.org/");
    }

    #[test]
    fn search_url_encodes_query() {
        let u = DuckDuckGoHtmlSearch::search_url("foo bar").unwrap();
        assert!(u.starts_with(ENDPOINT));
        assert!(u.contains("foo") && u.contains("bar"));
    }
}
