//! HTML body text and minimal PDF text extraction.

use html5gum::{Token, Tokenizer};

use crate::error::WebAccessError;
use crate::types::{ContentKind, FailureClass};

fn is_skip_tag(name: &str) -> bool {
    matches!(
        name,
        "script" | "style" | "noscript" | "template" | "svg" | "iframe"
    )
}

fn tag_name(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).to_ascii_lowercase()
}

fn token_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Extract visible text + best-effort `<title>` from HTML bytes/string.
pub fn extract_html(
    input: &str,
    max_chars: usize,
) -> Result<(Option<String>, String, bool), WebAccessError> {
    let mut title: Option<String> = None;
    let mut in_title = false;
    let mut skip_depth: usize = 0;
    let mut parts: Vec<String> = Vec::new();
    let mut truncated = false;

    for token in Tokenizer::new(input) {
        let token = match token {
            Ok(token) => token,
            Err(_) => continue,
        };
        match token {
            Token::StartTag(tag) => {
                let name = tag_name(&tag.name);
                if name == "title" {
                    in_title = true;
                }
                if is_skip_tag(&name) {
                    skip_depth += 1;
                }
            }
            Token::EndTag(tag) => {
                let name = tag_name(&tag.name);
                if name == "title" {
                    in_title = false;
                }
                if is_skip_tag(&name) && skip_depth > 0 {
                    skip_depth -= 1;
                }
            }
            Token::String(text) => {
                let s = token_text(&text);
                if in_title {
                    let t = s.trim();
                    if !t.is_empty() {
                        title = Some(match title.take() {
                            Some(prev) => format!("{prev}{t}"),
                            None => t.to_string(),
                        });
                    }
                    continue;
                }
                if skip_depth > 0 {
                    continue;
                }
                let trimmed = collapse_ws(&s);
                if trimmed.is_empty() {
                    continue;
                }
                let used: usize = parts.iter().map(String::len).sum();
                let remaining = max_chars.saturating_sub(used);
                if remaining == 0 {
                    truncated = true;
                    break;
                }
                if trimmed.chars().count() > remaining {
                    parts.push(trimmed.chars().take(remaining).collect());
                    truncated = true;
                    break;
                }
                parts.push(trimmed);
            }
            _ => {}
        }
    }

    let text = collapse_ws(&parts.join(" "));
    if text.is_empty() && title.is_none() {
        return Err(WebAccessError::new(
            FailureClass::ExtractFailed,
            "HTML produced no extractable text",
        ));
    }
    Ok((title, text, truncated))
}

/// Minimal PDF text extraction: collect printable strings from literal `(...)`
/// objects that commonly hold page text. This is a first-cut heuristic, not a
/// full PDF parser (no font decoding / ToUnicode).
pub fn extract_pdf(bytes: &[u8], max_chars: usize) -> Result<(String, bool), WebAccessError> {
    if bytes.len() < 5 || &bytes[..5] != b"%PDF-" {
        return Err(WebAccessError::new(
            FailureClass::ExtractFailed,
            "bytes are not a PDF (missing %PDF- header)",
        ));
    }
    let mut out = String::new();
    let mut truncated = false;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'('
            && let Some((s, next)) = read_pdf_literal(bytes, i + 1)
        {
            push_pdf_fragment(&mut out, &s, max_chars, &mut truncated);
            i = next;
            if truncated {
                break;
            }
            continue;
        }
        i += 1;
    }
    let text = collapse_ws(&out);
    if text.is_empty() {
        return Err(WebAccessError::new(
            FailureClass::ExtractFailed,
            "PDF produced no extractable literal strings",
        ));
    }
    Ok((text, truncated))
}

fn read_pdf_literal(bytes: &[u8], mut i: usize) -> Option<(String, usize)> {
    let mut raw = Vec::new();
    let mut depth = 1usize;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if i + 1 < bytes.len() => {
                let n = bytes[i + 1];
                match n {
                    b'n' => raw.push(b'\n'),
                    b'r' => raw.push(b'\r'),
                    b't' => raw.push(b'\t'),
                    b'b' => raw.push(0x08),
                    b'f' => raw.push(0x0c),
                    b'(' | b')' | b'\\' => raw.push(n),
                    b'\n' | b'\r' => {}
                    _ => raw.push(n),
                }
                i += 2;
            }
            b'(' => {
                depth += 1;
                raw.push(b'(');
                i += 1;
            }
            b')' => {
                depth -= 1;
                i += 1;
                if depth == 0 {
                    let s = String::from_utf8_lossy(&raw).into_owned();
                    return Some((s, i));
                }
                raw.push(b')');
            }
            b => {
                raw.push(b);
                i += 1;
            }
        }
        if raw.len() > 64 * 1024 {
            return None;
        }
    }
    None
}

fn push_pdf_fragment(out: &mut String, s: &str, max_chars: usize, truncated: &mut bool) {
    let cleaned: String = s
        .chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .collect();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        return;
    }
    let letters = cleaned.chars().filter(|c| c.is_alphabetic()).count();
    if letters == 0 && !cleaned.chars().any(|c| c.is_numeric()) {
        return;
    }
    if out.len() >= max_chars {
        *truncated = true;
        return;
    }
    if !out.is_empty() {
        out.push(' ');
    }
    let room = max_chars.saturating_sub(out.len());
    if cleaned.len() > room {
        out.extend(cleaned.chars().take(room));
        *truncated = true;
    } else {
        out.push_str(cleaned);
    }
}

/// Choose extractor from MIME / magic, returning kind + title + text + truncated.
pub fn extract_auto(
    mime: Option<&str>,
    bytes: &[u8],
    max_chars: usize,
) -> Result<(ContentKind, Option<String>, String, bool), WebAccessError> {
    let mime_l = mime.map(|m| m.to_ascii_lowercase()).unwrap_or_default();
    if mime_l.contains("pdf") || bytes.starts_with(b"%PDF-") {
        let (text, truncated) = extract_pdf(bytes, max_chars)?;
        return Ok((ContentKind::PdfText, None, text, truncated));
    }
    if mime_l.contains("html")
        || mime_l.contains("xml")
        || looks_like_html(bytes)
        || mime_l.starts_with("text/")
        || mime_l.is_empty()
    {
        let text = std::str::from_utf8(bytes).map_err(|_| {
            WebAccessError::new(FailureClass::ExtractFailed, "body is not valid UTF-8 text")
        })?;
        if mime_l.contains("html") || looks_like_html(bytes) || mime_l.is_empty() {
            let (title, body, truncated) = extract_html(text, max_chars)?;
            return Ok((ContentKind::Html, title, body, truncated));
        }
        let truncated = text.chars().count() > max_chars;
        let clipped: String = text.chars().take(max_chars).collect();
        return Ok((ContentKind::Text, None, collapse_ws(&clipped), truncated));
    }
    Err(WebAccessError::new(
        FailureClass::ExtractFailed,
        format!(
            "unsupported MIME for extraction: {}",
            mime.unwrap_or("<none>")
        ),
    ))
}

fn looks_like_html(bytes: &[u8]) -> bool {
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(256)]).to_ascii_lowercase();
    head.contains("<html") || head.contains("<!doctype html") || head.contains("<head")
}

fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = true;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        } else {
            out.push(ch);
            prev_space = false;
        }
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_skips_script_and_reads_title() {
        let html = r#"<html><head><title>Demo Page</title><script>evil()</script>
<style>.x{}</style></head><body><p>Hello <b>World</b></p></body></html>"#;
        let (title, text, truncated) = extract_html(html, 10_000).unwrap();
        assert_eq!(title.as_deref(), Some("Demo Page"));
        assert!(text.contains("Hello"));
        assert!(text.contains("World"));
        assert!(!text.contains("evil"));
        assert!(!truncated);
    }

    #[test]
    fn pdf_literal_strings() {
        let pdf = b"%PDF-1.4\n1 0 obj<<>>stream\nBT (Hello PDF) Tj ET\nendstream\nendobj\n%%EOF\n";
        let (text, _) = extract_pdf(pdf, 1000).unwrap();
        assert!(text.contains("Hello PDF"));
    }
}
