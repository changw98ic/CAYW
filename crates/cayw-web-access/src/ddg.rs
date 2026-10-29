//! DuckDuckGo HTML search backend (no API key).

use std::sync::Arc;

use crate::error::WebAccessError;
use crate::provider::{BoxFuture, HttpTransport, SearchProvider};
use crate::types::{FailureClass, SearchEmptyReason, SearchEvidence, SearchHit, SearchResponse};

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
    ) -> BoxFuture<'a, Result<SearchResponse, WebAccessError>> {
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
            let mut response = parse_duckduckgo_response(Some(&html), Some(raw.status));
            if response.hits.len() as u32 > limit {
                response.hits.truncate(limit as usize);
            }
            for (i, hit) in response.hits.iter_mut().enumerate() {
                hit.rank = (i as u32) + 1;
            }
            Ok(response)
        })
    }
}

/// Canonical entry point: parse DuckDuckGo HTML results page into a [`SearchResponse`].
///
/// Distinguishes normal hits, explicit no-results pages, challenges, and markup drift.
/// Missing response text leaves the empty result unattributed.
pub fn parse_duckduckgo_html(html: &str) -> SearchResponse {
    parse_duckduckgo_response(Some(html), Some(200))
}

/// Parse DuckDuckGo response with optional status and body into a structured [`SearchResponse`].
pub fn parse_duckduckgo_response(html: Option<&str>, status: Option<u16>) -> SearchResponse {
    let status_code = status.unwrap_or(200);

    // If HTTP status is an explicit error (non-2xx)
    if !(200..300).contains(&status_code) {
        let (reason, summary, snippet) = match html {
            Some(body) if !body.trim().is_empty() => {
                let lower = body.to_lowercase();
                if is_ddg_challenge(&lower) {
                    (
                        SearchEmptyReason::Challenge,
                        extract_challenge_summary(body),
                        Some(prefix_at_char_boundary(body, 1024).to_string()),
                    )
                } else {
                    (
                        SearchEmptyReason::HttpError,
                        Some(format!("HTTP status {status_code}")),
                        Some(prefix_at_char_boundary(body, 1024).to_string()),
                    )
                }
            }
            _ => (
                SearchEmptyReason::HttpError,
                Some(format!("HTTP status {status_code}")),
                None,
            ),
        };
        return SearchResponse::empty(
            reason,
            Some(SearchEvidence {
                http_status: Some(status_code),
                source_summary: summary,
                response_snippet: snippet,
            }),
        );
    }

    // HTTP 200 path: check whether HTTP original text exists
    let Some(raw_html) = html else {
        return SearchResponse::empty(
            SearchEmptyReason::Unattributed,
            Some(SearchEvidence {
                http_status: Some(status_code),
                source_summary: Some(
                    "missing HTTP response body; cannot attribute empty result".into(),
                ),
                response_snippet: None,
            }),
        );
    };

    if raw_html.trim().is_empty() {
        return SearchResponse::empty(
            SearchEmptyReason::Unattributed,
            Some(SearchEvidence {
                http_status: Some(status_code),
                source_summary: Some(
                    "empty HTTP response body; cannot attribute empty result".into(),
                ),
                response_snippet: None,
            }),
        );
    }

    // Parse result anchors (`result__a`)
    let hits = extract_ddg_hits(raw_html);
    if !hits.is_empty() {
        return SearchResponse::new(hits);
    }

    // Hits are empty: classify into 真空 / 挑战 / 结构失配
    let lower = raw_html.to_lowercase();
    let response_snippet = Some(prefix_at_char_boundary(raw_html, 1024).to_string());

    if is_ddg_challenge(&lower) {
        let summary = extract_challenge_summary(raw_html);
        SearchResponse::empty(
            SearchEmptyReason::Challenge,
            Some(SearchEvidence {
                http_status: Some(status_code),
                source_summary: summary,
                response_snippet,
            }),
        )
    } else if is_ddg_true_empty(&lower) {
        let summary = extract_no_results_summary(raw_html);
        SearchResponse::empty(
            SearchEmptyReason::TrueEmpty,
            Some(SearchEvidence {
                http_status: Some(status_code),
                source_summary: summary,
                response_snippet,
            }),
        )
    } else {
        let summary = extract_structural_mismatch_summary(raw_html);
        SearchResponse::empty(
            SearchEmptyReason::StructuralMismatch,
            Some(SearchEvidence {
                http_status: Some(status_code),
                source_summary: summary,
                response_snippet,
            }),
        )
    }
}

fn is_ddg_true_empty(lower: &str) -> bool {
    lower.contains("result--no-result")
        || lower.contains("no-results__message")
        || lower.contains("no-results__container")
}

fn is_ddg_challenge(lower: &str) -> bool {
    lower.contains("bots use duckduckgo")
        || lower.contains("bots@duckduckgo.com")
        || lower.contains("unusual traffic")
        || lower.contains("ddg-captcha")
        || lower.contains("cf-turnstile")
        || lower.contains("cf-challenge")
        || lower.contains("challenge-form")
        || lower.contains("cf-browser-verification")
        || lower.contains("please verify you are a human")
        || lower.contains("verify that you are a human")
        || lower.contains("verify you are not a robot")
        || lower.contains("security check")
        || lower.contains("access denied")
        || lower.contains("automated requests")
        || (lower.contains("captcha")
            && (lower.contains("challenge") || lower.contains("robot") || lower.contains("human")))
}

fn extract_no_results_summary(html: &str) -> Option<String> {
    let clean_text = decode_basic_entities(&strip_tags(html));
    let lower = clean_text.to_ascii_lowercase();
    for marker in [
        "no results found",
        "not found any results",
        "no search results",
        "未找到结果",
        "没有找到任何结果",
    ] {
        if let Some(pos) = lower.find(marker) {
            let start = clean_text.floor_char_boundary(pos.saturating_sub(30));
            let end = clean_text.floor_char_boundary(pos.saturating_add(270).min(clean_text.len()));
            let snippet = clean_text[start..end].trim();
            if !snippet.is_empty() {
                return Some(prefix_at_char_boundary(snippet, 300).to_string());
            }
        }
    }
    Some("No results found".to_string())
}

fn extract_challenge_summary(html: &str) -> Option<String> {
    let clean_text = decode_basic_entities(&strip_tags(html));
    let lower = clean_text.to_ascii_lowercase();
    for marker in [
        "bots use duckduckgo",
        "unusual traffic",
        "verify you are not a robot",
        "verify you are a human",
        "ddg-captcha",
        "access denied",
        "security check",
        "challenge",
    ] {
        if let Some(pos) = lower.find(marker) {
            let start = clean_text.floor_char_boundary(pos.saturating_sub(40));
            let end = clean_text.floor_char_boundary(pos.saturating_add(260).min(clean_text.len()));
            let snippet = clean_text[start..end].trim();
            if !snippet.is_empty() {
                return Some(prefix_at_char_boundary(snippet, 300).to_string());
            }
        }
    }
    Some("DuckDuckGo challenge / bot check detected".to_string())
}

fn extract_structural_mismatch_summary(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    if let Some(pos) = lower.find("<title>") {
        let after = &html[pos + 7..];
        if let Some(end) = after.to_ascii_lowercase().find("</title>") {
            let title = decode_basic_entities(strip_tags(&after[..end]).trim());
            if !title.is_empty() {
                return Some(format!(
                    "structural mismatch: page title {:?}, no results markup found",
                    prefix_at_char_boundary(&title, 100)
                ));
            }
        }
    }
    Some("structural mismatch: unexpected page layout without search result anchors".to_string())
}

/// Parse DuckDuckGo HTML result anchors (`result__a`) into hits.
///
/// Resolves `uddg=` redirect targets when present. Intentionally small and
/// tolerant — DDG markup drifts; empty results are a soft miss, not a panic.
pub fn extract_ddg_hits(html: &str) -> Vec<SearchHit> {
    let mut hits: Vec<SearchHit> = Vec::new();
    let mut rest = html;
    while let Some(idx) = rest.find("result__a") {
        // Tag opening `<` must be before `idx` in the current slice, with no closing `>` in between.
        let Some(tag_start) = rest[..idx].rfind('<') else {
            rest = &rest[idx + 9..];
            continue;
        };
        if rest[tag_start..idx].contains('>') {
            rest = &rest[idx + 9..];
            continue;
        }
        // Must be an `<a>` tag.
        let tag_head = &rest[tag_start..idx];
        let tag_name = tag_head.trim_start_matches('<');
        let name_end = tag_name
            .find(|c: char| c.is_ascii_whitespace() || c == '>')
            .unwrap_or(tag_name.len());
        if !tag_name[..name_end].eq_ignore_ascii_case("a") {
            rest = &rest[idx + 9..];
            continue;
        }

        // Tag must close with `>` after `idx`.
        let Some(close_rel) = rest[idx..].find('>') else {
            rest = &rest[idx + 9..];
            continue;
        };
        let tag_end = idx + close_rel;
        let opening_tag = &rest[tag_start..=tag_end];

        // Ensure this tag really has `result__a` as a class.
        let is_result_a = extract_attr(opening_tag, "class")
            .as_deref()
            .unwrap_or("")
            .split_whitespace()
            .any(|c| c == "result__a" || c.contains("result__a"));
        if !is_result_a {
            rest = &rest[tag_end + 1..];
            continue;
        }

        // Extract href strictly from this anchor's opening tag.
        // Never look ahead to subsequent elements or anchors.
        let href = extract_attr(opening_tag, "href");
        let Some(href) = href else {
            rest = &rest[tag_end + 1..];
            continue;
        };

        // Extract title between the opening `>` and closing `</a>`. Match the
        // closing tag and the next anchor case-insensitively (`</A>`, `<A ...>`),
        // and never cross into a following anchor or split a UTF-8 scalar.
        let after_tag = &rest[tag_end + 1..];
        let close = find_ascii_ci(after_tag, "</a>");
        let next_anchor = find_ascii_ci(after_tag, "<a");
        let content = match (close, next_anchor) {
            (Some(end_a), Some(next_a)) => &after_tag[..end_a.min(next_a)],
            (Some(end_a), None) => &after_tag[..end_a],
            (None, Some(next_a)) => &after_tag[..next_a],
            (None, None) => prefix_at_char_boundary(after_tag, 400),
        };
        let title = strip_tags(content);

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
        rest = &rest[tag_end + 1..];
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

fn extract_attr(tag: &str, attr_name: &str) -> Option<String> {
    let tag_bytes = tag.as_bytes();
    let mut i = 0;
    // Skip tag name (e.g. "<a")
    if i < tag_bytes.len() && tag_bytes[i] == b'<' {
        i += 1;
        while i < tag_bytes.len() && !tag_bytes[i].is_ascii_whitespace() && tag_bytes[i] != b'>' {
            i += 1;
        }
    }
    while i < tag_bytes.len() {
        while i < tag_bytes.len() && tag_bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= tag_bytes.len() || tag_bytes[i] == b'>' || tag_bytes[i] == b'/' {
            break;
        }
        let key_start = i;
        while i < tag_bytes.len()
            && !tag_bytes[i].is_ascii_whitespace()
            && tag_bytes[i] != b'='
            && tag_bytes[i] != b'>'
            && tag_bytes[i] != b'/'
        {
            i += 1;
        }
        let key = &tag[key_start..i];
        while i < tag_bytes.len() && tag_bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let mut val = None;
        if i < tag_bytes.len() && tag_bytes[i] == b'=' {
            i += 1;
            while i < tag_bytes.len() && tag_bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < tag_bytes.len() {
                if tag_bytes[i] == b'"' || tag_bytes[i] == b'\'' {
                    let quote = tag_bytes[i];
                    i += 1;
                    let val_start = i;
                    while i < tag_bytes.len() && tag_bytes[i] != quote {
                        i += 1;
                    }
                    val = Some(tag[val_start..i].to_string());
                    if i < tag_bytes.len() && tag_bytes[i] == quote {
                        i += 1;
                    }
                } else {
                    let val_start = i;
                    while i < tag_bytes.len()
                        && !tag_bytes[i].is_ascii_whitespace()
                        && tag_bytes[i] != b'>'
                    {
                        i += 1;
                    }
                    val = Some(tag[val_start..i].to_string());
                }
            }
        }
        if key.eq_ignore_ascii_case(attr_name) {
            return val;
        }
    }
    None
}

/// Cap `s` to at most `max_bytes` without splitting a UTF-8 scalar.
fn prefix_at_char_boundary(s: &str, max_bytes: usize) -> &str {
    &s[..s.floor_char_boundary(max_bytes.min(s.len()))]
}

/// Byte offset of the first ASCII-case-insensitive occurrence of `needle`.
///
/// `needle` must be ASCII, so any matched offset is a UTF-8 char boundary.
fn find_ascii_ci(haystack: &str, needle: &str) -> Option<usize> {
    let needle = needle.as_bytes();
    let hay = haystack.as_bytes();
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|window| {
        window
            .iter()
            .zip(needle)
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
    })
}

fn nearby_snippet(full: &str, result_idx: usize) -> Option<String> {
    // `result_idx + 1200` is a raw byte offset and can land mid Chinese / emoji
    // scalar; slicing `&full[result_idx..window_end]` then panics. Floor both ends.
    let start = full.floor_char_boundary(result_idx.min(full.len()));
    let window_end = full.floor_char_boundary(start.saturating_add(1200).min(full.len()));
    let window = &full[start..window_end];
    for marker in ["result__snippet", "result__body"] {
        if let Some(m) = window.find(marker) {
            let slice = &window[m..];
            if let Some(gt) = slice.find('>') {
                let after = &slice[gt + 1..];
                // Byte-cap fallback used to use `after.len().min(280)` which can land
                // mid multi-byte Chinese / emoji scalar and panic on `&after[..end]`.
                let raw = match after.find("</") {
                    Some(i) => &after[..i],
                    None => prefix_at_char_boundary(after, 280),
                };
                let text = strip_tags(raw);
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

    /// Truncated DDG HTML with a long Chinese (+ emoji) snippet and no closing
    /// `</…>` used to panic: `after.len().min(280)` is not always a char boundary.
    #[test]
    fn chinese_snippet_byte_cap_does_not_panic() {
        let zh = "中文搜索结果摘要这是一段很长的中文摘要内容用于测试字节截断".repeat(10);
        assert!(zh.len() > 280);
        assert!(!zh.is_char_boundary(280));
        // No `</` after the snippet opener — same shape as a mid-page body cut.
        let html = format!(
            r#"<div class="result"><a class="result__a" href="https://example.cn/page">中文标题🚀✨</a><a class="result__snippet">{zh}"#
        );
        let hits = parse_duckduckgo_html(&html);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].url, "https://example.cn/page");
        assert_eq!(hits[0].title.as_deref(), Some("中文标题🚀✨"));
        let snippet = hits[0].snippet.as_deref().expect("snippet");
        assert!(snippet.starts_with("中文"));
        assert!(snippet.len() <= 280);
        assert!(snippet.is_char_boundary(snippet.len()));
        // Mixed emoji title path also stayed intact.
        assert!(hits[0].title.as_deref().unwrap().contains('🚀'));
    }

    #[test]
    fn prefix_at_char_boundary_floors_mid_scalar() {
        let s = "中文🚀摘要";
        // '中' is bytes 0..3; index 2 is inside it.
        assert!(!s.is_char_boundary(2));
        assert_eq!(prefix_at_char_boundary(s, 2), "");
        assert_eq!(prefix_at_char_boundary(s, 3), "中");
        assert_eq!(prefix_at_char_boundary(s, 100), s);
    }

    /// Window cut at `result_idx + 1200` used to panic when that byte offset
    /// landed mid multi-byte Chinese / emoji scalar (separate from the 280 cap).
    #[test]
    fn nearby_snippet_window_cut_mid_scalar_does_not_panic() {
        let base =
            r#"<a class="result__a" href="https://x.com/">T</a><span class="result__snippet">"#;
        let idx = base.find("result__a").expect("marker");
        let after_marker_bytes = base[idx..].len();
        // Pad ASCII so result_idx+1200 falls inside '中' (continuation byte).
        let pad = 1199 - after_marker_bytes;
        assert!(pad > 0, "fixture pad must be positive");
        let zh = "中文emoji🚀".repeat(50);
        let html = format!("<div>{base}{}{zh}</span></div>", "x".repeat(pad));
        let result_idx = html.find("result__a").expect("result__a");
        let raw_end = result_idx + 1200;
        assert!(raw_end < html.len());
        assert!(
            !html.is_char_boundary(raw_end),
            "fixture must place window_end mid-scalar"
        );
        // Direct call — previously panicked on `&full[result_idx..raw_end]`.
        let snip = nearby_snippet(&html, result_idx);
        assert!(snip.is_some());
        let snip = snip.unwrap();
        assert!(snip.contains('x') || snip.contains('中') || snip.contains('🚀'));
        assert!(snip.is_char_boundary(snip.len()));

        let hits = parse_duckduckgo_html(&html);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].url, "https://x.com/");
        assert!(hits[0].snippet.is_some());
    }

    /// `result__body` marker path must also survive a mid-scalar window end.
    #[test]
    fn nearby_snippet_result_body_window_mid_scalar() {
        let base = r#"<a class="result__a" href="https://example.cn/z">标题🚀</a><div class="result__body">"#;
        let idx = base.find("result__a").expect("marker");
        let after_marker_bytes = base[idx..].len();
        let pad = 1199 - after_marker_bytes;
        assert!(pad > 0);
        let zh = "摘要内容✨".repeat(80);
        let html = format!("<div>{base}{}{zh}</div></div>", "y".repeat(pad));
        let result_idx = html.find("result__a").expect("result__a");
        assert!(!html.is_char_boundary(result_idx + 1200));
        let snip = nearby_snippet(&html, result_idx).expect("snippet");
        assert!(snip.is_char_boundary(snip.len()));
        let hits = parse_duckduckgo_html(&html);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].title.as_deref().unwrap_or("").contains('🚀'));
    }

    #[test]
    fn nearby_snippet_accepts_result_idx_past_end() {
        assert_eq!(nearby_snippet("short", 999), None);
        assert_eq!(nearby_snippet("", 0), None);
    }

    #[test]
    fn ddg_challenge_takes_priority_over_no_results_markup() {
        let html = r#"<div class="no-results__message">No results found</div>
            <form id="challenge-form">Verify you are a human</form>"#;
        let response = parse_duckduckgo_html(html);
        assert_eq!(response.empty_reason, Some(SearchEmptyReason::Challenge));
    }

    #[test]
    fn ddg_plain_no_results_text_does_not_prove_an_empty_search() {
        let response = parse_duckduckgo_html("<p>A guide about no search results</p>");
        assert_eq!(
            response.empty_reason,
            Some(SearchEmptyReason::StructuralMismatch)
        );
    }

    #[test]
    fn ddg_diagnostic_offsets_preserve_unicode_boundaries() {
        let prefix = "İ".repeat(32);
        let challenge =
            format!("{prefix}<p>Verify you are a human</p><form id=\"challenge-form\"></form>");
        let response = parse_duckduckgo_html(&challenge);
        assert_eq!(response.empty_reason, Some(SearchEmptyReason::Challenge));
        assert!(
            response
                .evidence
                .unwrap()
                .source_summary
                .unwrap()
                .contains("Verify")
        );

        let empty = format!("{prefix}<div class=\"no-results__message\">No results found</div>");
        let response = parse_duckduckgo_html(&empty);
        assert_eq!(response.empty_reason, Some(SearchEmptyReason::TrueEmpty));
        assert!(
            response
                .evidence
                .unwrap()
                .source_summary
                .unwrap()
                .contains("No results found")
        );

        let drift = format!("{prefix}<TITLE>İ Search Results</TITLE>");
        let response = parse_duckduckgo_html(&drift);
        assert_eq!(
            response.empty_reason,
            Some(SearchEmptyReason::StructuralMismatch)
        );
        assert!(
            response
                .evidence
                .unwrap()
                .source_summary
                .unwrap()
                .contains("İ Search Results")
        );
    }

    #[test]
    fn ddg_classifies_true_empty_inline_fixture() {
        let html = r#"<!doctype html><html><body>
        <div class="result result--no-result">
            <div class="no-results__message">
                <h1>No results found for <strong>"unmatched_query_string"</strong></h1>
                <p>Suggestions: Check spelling</p>
            </div>
        </div></body></html>"#;
        let resp = parse_duckduckgo_html(html);
        assert!(resp.hits.is_empty());
        assert_eq!(resp.empty_reason, Some(SearchEmptyReason::TrueEmpty));
        let ev = resp.evidence.unwrap();
        assert!(ev.source_summary.unwrap().contains("No results found"));
        assert!(ev.response_snippet.unwrap().contains("no-results__message"));
    }

    #[test]
    fn ddg_classifies_challenge() {
        let html = r#"<!doctype html><html><body>
        <h1>Unfortunately, bots use DuckDuckGo too</h1>
        <p>Please complete the following challenge to verify you are a human</p>
        <div class="challenge-form"></div>
        </body></html>"#;
        let resp = parse_duckduckgo_html(html);
        assert!(resp.hits.is_empty());
        assert_eq!(resp.empty_reason, Some(SearchEmptyReason::Challenge));
        let ev = resp.evidence.unwrap();
        assert!(
            ev.source_summary
                .unwrap()
                .to_lowercase()
                .contains("bots use duckduckgo")
        );
    }

    #[test]
    fn ddg_classifies_structural_mismatch() {
        let html = r#"<!doctype html><html><head><title>DuckDuckGo Drifted Layout</title></head>
        <body><div class="unknown_feed">Some unexpected content without results</div></body></html>"#;
        let resp = parse_duckduckgo_html(html);
        assert!(resp.hits.is_empty());
        assert_eq!(
            resp.empty_reason,
            Some(SearchEmptyReason::StructuralMismatch)
        );
        let ev = resp.evidence.unwrap();
        assert!(
            ev.source_summary
                .unwrap()
                .contains("DuckDuckGo Drifted Layout")
        );
    }

    #[test]
    fn ddg_without_http_response_text_cannot_attribute() {
        let resp_none = parse_duckduckgo_response(None, Some(200));
        assert!(resp_none.hits.is_empty());
        assert_eq!(
            resp_none.empty_reason,
            Some(SearchEmptyReason::Unattributed)
        );
        assert!(
            resp_none
                .evidence
                .as_ref()
                .unwrap()
                .response_snippet
                .is_none()
        );

        let resp_empty = parse_duckduckgo_response(Some("   "), Some(200));
        assert!(resp_empty.hits.is_empty());
        assert_eq!(
            resp_empty.empty_reason,
            Some(SearchEmptyReason::Unattributed)
        );
        assert!(
            resp_empty
                .evidence
                .as_ref()
                .unwrap()
                .response_snippet
                .is_none()
        );
    }

    #[test]
    fn ddg_classifies_http_status_error() {
        let resp = parse_duckduckgo_response(Some("Internal Server Error"), Some(500));
        assert!(resp.hits.is_empty());
        assert_eq!(resp.empty_reason, Some(SearchEmptyReason::HttpError));
        assert_eq!(resp.evidence.as_ref().unwrap().http_status, Some(500));
    }

    #[test]
    fn parses_anchors_with_various_attribute_orders_and_avoids_href_bleeding() {
        // First anchor has href before class="result__a".
        // Second anchor is an unrelated anchor without class="result__a".
        // Third anchor has class="result__a" before href.
        // Fourth anchor has class="result__a" but NO href — it must be ignored, NOT borrow href from subsequent anchor!
        // Fifth anchor has href before class.
        let html = r#"
        <div class="result">
          <a href="https://example.com/first" class="result__a">First Title</a>
        </div>
        <div class="unrelated">
          <a href="https://example.com/unrelated">Unrelated</a>
        </div>
        <div class="result">
          <a class="result__a" href="https://example.com/third">Third Title</a>
        </div>
        <div class="result">
          <a class="result__a">Anchor Without Href</a>
        </div>
        <div class="result">
          <a id="foo" href="https://example.com/fifth" class="result__a" target="_blank">Fifth Title</a>
        </div>
        "#;
        let hits = extract_ddg_hits(html);
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].url, "https://example.com/first");
        assert_eq!(hits[0].title.as_deref(), Some("First Title"));
        assert_eq!(hits[0].rank, 1);

        assert_eq!(hits[1].url, "https://example.com/third");
        assert_eq!(hits[1].title.as_deref(), Some("Third Title"));
        assert_eq!(hits[1].rank, 2);

        assert_eq!(hits[2].url, "https://example.com/fifth");
        assert_eq!(hits[2].title.as_deref(), Some("Fifth Title"));
        assert_eq!(hits[2].rank, 3);
    }

    #[test]
    fn parses_anchors_with_single_quotes_and_spaces_in_attributes() {
        let html = r#"
        <div class="result">
          <a class = 'result__a' href = 'https://example.com/single-quotes'>Single Quotes Title</a>
        </div>
        "#;
        let hits = extract_ddg_hits(html);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].url, "https://example.com/single-quotes");
        assert_eq!(hits[0].title.as_deref(), Some("Single Quotes Title"));
    }

    #[test]
    fn uppercase_closing_anchor_tag_is_recognized() {
        let html = r#"<a class="result__a" href="https://example.com/upper">Upper Title</A>"#;
        let hits = extract_ddg_hits(html);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].url, "https://example.com/upper");
        assert_eq!(hits[0].title.as_deref(), Some("Upper Title"));
    }

    #[test]
    fn unclosed_title_stops_at_next_uppercase_anchor() {
        let html = r#"<a class="result__a" href="https://example.com/a">First Title
            <A class="result__a" href="https://example.com/b">Second</A>"#;
        let hits = extract_ddg_hits(html);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].url, "https://example.com/a");
        assert_eq!(hits[0].title.as_deref(), Some("First Title"));
        assert_eq!(hits[1].url, "https://example.com/b");
        assert_eq!(hits[1].title.as_deref(), Some("Second"));
    }

    #[test]
    fn unclosed_multibyte_title_cap_does_not_panic_or_cross_scalar() {
        // No `</a>` and no following anchor: the 400-byte fallback used to slice
        // at a raw offset, panicking when it landed inside a 3-byte CJK scalar.
        let long = "中".repeat(200); // 600 bytes; 400 is not a char boundary
        assert!(!long.is_char_boundary(400));
        let html = format!(r#"<a class="result__a" href="https://example.cn/long">{long}"#);
        let hits = extract_ddg_hits(&html);
        assert_eq!(hits.len(), 1);
        let title = hits[0].title.as_deref().expect("title");
        assert!(title.len() <= 400);
        assert!(title.is_char_boundary(title.len()));
        assert_eq!(title.len(), 399);
        assert!(title.chars().all(|c| c == '中'));
    }
}
