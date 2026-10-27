//! Workspace file producer: read bytes, extract a lightweight outline, publish.
//!
//! Closed-loop path that does **not** wait for a host-provided digest/outline
//! snapshot. Avoids a live LSP: Rust outlines are a bounded syntax scan (not a
//! full `syn` / `code.outline` reimplementation). Host refresh prefers Workspace
//! `code.outline` when available and falls back here via [`publish_bytes`].

use crate::bridge::{SymbolInput, SymbolVersionBridge, content_sha256};
use crate::types::{CodeIntelError, FileVersion};
use serde_json::Value;

/// Publish one file from its bytes into the shared bridge.
pub fn publish_bytes(
    bridge: &SymbolVersionBridge,
    path: &str,
    bytes: &[u8],
    language: &str,
) -> Result<FileVersion, CodeIntelError> {
    let digest = content_sha256(bytes);
    let language = language.trim().to_ascii_lowercase();
    let symbols = if language == "rust" {
        let text = std::str::from_utf8(bytes)
            .map_err(|_| CodeIntelError::Invalid("Rust source must be UTF-8".into()))?;
        lightweight_rust_outline(text)
    } else {
        Vec::new()
    };
    bridge.publish_file(path, &digest, &language, symbols)
}

/// Read `path` from the filesystem and publish under that path key.
pub fn publish_path(
    bridge: &SymbolVersionBridge,
    path: &str,
    language: &str,
) -> Result<FileVersion, CodeIntelError> {
    let bytes = std::fs::read(path)
        .map_err(|error| CodeIntelError::Invalid(format!("failed to read {path}: {error}")))?;
    publish_bytes(bridge, path, &bytes, language)
}

/// Resolve `relative` under `workspace` and publish under the relative key.
///
/// The bridge path key stays project-relative (`examples/demo.rs`) so Trace
/// `implementation` strings match without rewriting absolute paths.
pub fn publish_under_workspace(
    bridge: &SymbolVersionBridge,
    workspace: &std::path::Path,
    relative: &str,
    language: &str,
) -> Result<FileVersion, CodeIntelError> {
    let relative = relative.trim_start_matches("./");
    if relative.is_empty()
        || relative.contains('\\')
        || std::path::Path::new(relative).components().any(|c| {
            matches!(
                c,
                std::path::Component::ParentDir | std::path::Component::RootDir
            )
        })
    {
        return Err(CodeIntelError::Invalid(format!(
            "refusing unsafe workspace-relative path: {relative}"
        )));
    }
    let abs = workspace.join(relative);
    let bytes = std::fs::read(&abs).map_err(|error| {
        CodeIntelError::Invalid(format!("failed to read {}: {error}", abs.display()))
    })?;
    publish_bytes(bridge, relative, &bytes, language)
}

/// Guess outline language from a path extension (`rs` → `rust`).
pub fn language_for_path(path: &str) -> &'static str {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".rs") {
        "rust"
    } else if lower.ends_with(".luau") || lower.ends_with(".lua") {
        "luau"
    } else if lower.ends_with(".ts") || lower.ends_with(".tsx") {
        "typescript"
    } else if lower.ends_with(".js") || lower.ends_with(".jsx") {
        "javascript"
    } else if lower.ends_with(".py") {
        "python"
    } else {
        "text"
    }
}
/// Map a Workspace `code.outline` `symbols` array into bridge inputs.
///
/// Accepts either `parent` (outline wire) or `parent_index` (bridge wire).
pub fn symbol_inputs_from_outline(symbols: &[Value]) -> Result<Vec<SymbolInput>, CodeIntelError> {
    let mut out = Vec::with_capacity(symbols.len());
    for (index, value) in symbols.iter().enumerate() {
        let name = value
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| CodeIntelError::Invalid(format!("outline symbol {index} missing name")))?
            .to_owned();
        let kind = value
            .get("kind")
            .and_then(Value::as_str)
            .ok_or_else(|| CodeIntelError::Invalid(format!("outline symbol {index} missing kind")))?
            .to_owned();
        let byte_start = value
            .get("byte_start")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                CodeIntelError::Invalid(format!("outline symbol {index} missing byte_start"))
            })?;
        let byte_end = value
            .get("byte_end")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                CodeIntelError::Invalid(format!("outline symbol {index} missing byte_end"))
            })?;
        let start_line = value
            .get("start_line")
            .and_then(Value::as_u64)
            .map(|n| n as u32);
        let end_line = value
            .get("end_line")
            .and_then(Value::as_u64)
            .map(|n| n as u32);
        let parent_index = value
            .get("parent_index")
            .or_else(|| value.get("parent"))
            .and_then(Value::as_u64)
            .map(|n| n as u32);
        out.push(SymbolInput {
            name,
            kind,
            byte_start,
            byte_end,
            start_line,
            end_line,
            parent_index,
        });
    }
    Ok(out)
}

/// Publish from a full `code.outline` JSON object.
pub fn publish_outline_json(
    bridge: &SymbolVersionBridge,
    outline: &Value,
) -> Result<FileVersion, CodeIntelError> {
    let path = outline
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| CodeIntelError::Invalid("outline missing path".into()))?;
    let digest = outline
        .get("sha256")
        .and_then(Value::as_str)
        .ok_or_else(|| CodeIntelError::Invalid("outline missing sha256".into()))?;
    let language = outline
        .get("language")
        .and_then(Value::as_str)
        .unwrap_or("rust");
    let symbols = outline
        .get("symbols")
        .and_then(Value::as_array)
        .ok_or_else(|| CodeIntelError::Invalid("outline missing symbols".into()))?;
    let inputs = symbol_inputs_from_outline(symbols)?;
    bridge.publish_file(path, digest, language, inputs)
}

/// Bounded Rust outline without `syn`: top-level item declarations only.
pub fn lightweight_rust_outline(source: &str) -> Vec<SymbolInput> {
    let mut symbols = Vec::new();
    let mut byte_offset = 0usize;
    let mut block_comment = false;
    let mut line_no: u32 = 1;

    for line in source.split_inclusive('\n') {
        let line_start = byte_offset;
        byte_offset += line.len();
        let trimmed = line.trim_start();
        let leading = line.len() - trimmed.len();

        if block_comment {
            if trimmed.contains("*/") {
                block_comment = false;
            }
            line_no = line_no.saturating_add(1);
            continue;
        }
        if trimmed.starts_with("/*") {
            if !trimmed.contains("*/") {
                block_comment = true;
            }
            line_no = line_no.saturating_add(1);
            continue;
        }
        if trimmed.starts_with("//") {
            line_no = line_no.saturating_add(1);
            continue;
        }

        if let Some(sym) = match_decl(trimmed) {
            let name_rel = trimmed.find(&sym.name).unwrap_or(0);
            let abs = line_start + leading + name_rel;
            let end = abs + sym.name.len();
            symbols.push(SymbolInput {
                name: sym.name,
                kind: sym.kind.into(),
                byte_start: abs as u64,
                byte_end: end as u64,
                start_line: Some(line_no),
                end_line: Some(line_no),
                parent_index: None,
            });
        }
        line_no = line_no.saturating_add(1);
    }
    symbols
}

struct Decl {
    name: String,
    kind: &'static str,
}

fn match_decl(trimmed: &str) -> Option<Decl> {
    let s = strip_item_prefixes(trimmed);

    for (kw, kind) in [
        ("fn ", "function"),
        ("struct ", "struct"),
        ("enum ", "enum"),
        ("trait ", "trait"),
        ("mod ", "module"),
        ("type ", "type"),
        ("const ", "constant"),
        ("static ", "static"),
    ] {
        if let Some(rest) = s.strip_prefix(kw) {
            let name = take_ident(rest)?;
            return Some(Decl { name, kind });
        }
    }
    if let Some(rest) = s.strip_prefix("impl") {
        let rest = rest.trim_start();
        let rest = if rest.starts_with('<') {
            skip_balanced(rest, '<', '>')?.trim_start()
        } else {
            rest
        };
        let target = if let Some(idx) = find_for(rest) {
            rest[idx + 3..].trim_start()
        } else {
            rest
        };
        let name = take_impl_name(target)?;
        return Some(Decl { name, kind: "impl" });
    }
    None
}

fn strip_item_prefixes(mut s: &str) -> &str {
    loop {
        let before = s;
        s = strip_vis(s);
        for prefix in ["async", "unsafe", "default"] {
            if let Some(rest) = s.strip_prefix(prefix) {
                s = rest.trim_start();
            }
        }
        // `const fn` → keep stripping; bare `const NAME` stops so const-item match works.
        if let Some(rest) = s.strip_prefix("const") {
            let rest = rest.trim_start();
            if rest.starts_with("fn") || rest.starts_with("async") || rest.starts_with("unsafe") {
                s = rest;
            }
        }
        if s == before {
            return s;
        }
    }
}

fn strip_vis(s: &str) -> &str {
    let Some(rest) = s.strip_prefix("pub") else {
        return s;
    };
    let rest = rest.trim_start();
    if let Some(rest) = rest.strip_prefix('(') {
        if let Some(close) = rest.find(')') {
            return rest[close + 1..].trim_start();
        }
        return s;
    }
    rest
}

fn take_ident(s: &str) -> Option<String> {
    let s = s.trim_start();
    let mut chars = s.chars();
    let first = chars.next()?;
    if !(first.is_ascii_alphabetic() || first == '_') {
        return None;
    }
    let mut name = String::from(first);
    for ch in chars {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            name.push(ch);
        } else {
            break;
        }
    }
    Some(name)
}

fn take_impl_name(s: &str) -> Option<String> {
    let s = s.trim_start();
    let end = s.find(['{', ';', '\n']).unwrap_or(s.len());
    let raw = s[..end].trim();
    if raw.is_empty() {
        return None;
    }
    let name = raw
        .rsplit("::")
        .next()
        .unwrap_or(raw)
        .split_whitespace()
        .next()
        .unwrap_or(raw);
    let name = name.trim_matches(|c: char| matches!(c, '<' | '>' | ',' | '(' | ')'));
    if name.is_empty() {
        None
    } else {
        Some(name.to_owned())
    }
}

fn find_for(s: &str) -> Option<usize> {
    let mut depth = 0i32;
    let mut i = 0usize;
    while i < s.len() {
        let ch = s[i..].chars().next()?;
        match ch {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            _ => {}
        }
        if depth == 0 && s[i..].starts_with("for ") {
            let ok = i == 0
                || s.as_bytes()
                    .get(i.wrapping_sub(1))
                    .is_some_and(|b| b.is_ascii_whitespace());
            if ok {
                return Some(i);
            }
        }
        i += ch.len_utf8();
    }
    None
}

fn skip_balanced(s: &str, open: char, close: char) -> Option<&str> {
    let mut chars = s.chars();
    if chars.next()? != open {
        return None;
    }
    let mut depth = 1i32;
    let mut idx = open.len_utf8();
    for ch in chars {
        if ch == open {
            depth += 1;
        } else if ch == close {
            depth -= 1;
            if depth == 0 {
                return Some(&s[idx + close.len_utf8()..]);
            }
        }
        idx += ch.len_utf8();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DefinitionQuery;

    #[test]
    fn lightweight_outline_finds_fn_and_struct() {
        let src = "/// docs\npub fn visible() {}\nstruct Helper { x: u8 }\n";
        let symbols = lightweight_rust_outline(src);
        assert!(
            symbols
                .iter()
                .any(|s| s.name == "visible" && s.kind == "function"),
            "{symbols:?}"
        );
        assert!(
            symbols
                .iter()
                .any(|s| s.name == "Helper" && s.kind == "struct"),
            "{symbols:?}"
        );
    }

    #[test]
    fn publish_bytes_round_trips_definitions() {
        let bridge = SymbolVersionBridge::new();
        let src = b"pub fn add(a: i32, b: i32) -> i32 { a + b }\n";
        let file = publish_bytes(&bridge, "src/math.rs", src, "rust").unwrap();
        assert_eq!(file.version, 1);
        let hits = bridge
            .definitions(DefinitionQuery {
                name: "add".into(),
                path: None,
                kind: Some("function".into()),
                file_version: Some(file.version),
            })
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].file.content_sha256, file.content_sha256);
    }

    #[test]
    fn outline_json_maps_parent_field() {
        let symbols = vec![serde_json::json!({
            "name": "Child",
            "kind": "function",
            "byte_start": 10,
            "byte_end": 15,
            "start_line": 2,
            "end_line": 2,
            "parent": 0
        })];
        let inputs = symbol_inputs_from_outline(&symbols).unwrap();
        assert_eq!(inputs[0].parent_index, Some(0));
    }

    #[test]
    fn clones_share_one_index() {
        let a = SymbolVersionBridge::new();
        let b = a.clone();
        publish_bytes(&a, "a.rs", b"fn shared() {}", "rust").unwrap();
        assert_eq!(b.symbols_for("a.rs").unwrap().len(), 1);
    }
}
