//! Strict, cooperative JSON parsing for HTTP provider responses.
//!
//! Duplicate object fields (including escaped spellings) are rejected. Parsing
//! checks cancellation and yields while scanning strings, whitespace and tokens.
//! Callers must also bound the response bytes before invoking this parser.

use crate::CancellationToken;
use serde_json::{Map, Value};

/// Response syntax errors can be reported to the model; cancellation and
/// resource limits must still stop response processing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    #[error("Invalid, incomplete or duplicate-key JSON")]
    InvalidSyntax,
    #[error("Trailing data in provider JSON")]
    TrailingData,
    #[error("Provider JSON exceeds the node limit")]
    NodeLimit,
    #[error("Provider JSON exceeds the depth limit")]
    DepthLimit,
    #[error("Provider JSON exceeds the scalar token limit")]
    ScalarTokenLimit,
    #[error("Request cancelled")]
    Cancelled,
}

impl From<ParseError> for String {
    fn from(error: ParseError) -> Self {
        error.to_string()
    }
}

type Result<T> = std::result::Result<T, ParseError>;
const MAX_JSON_NODES: usize = 65_536;
const MAX_JSON_DEPTH: usize = 128;
const SCAN_BATCH: usize = 4096;

/// Parse one JSON value without silently replacing duplicate object fields.
/// Supports at most 65,536 values and 128 nested containers. Errors contain no
/// input data. Cancellation returns `Request cancelled`.
pub async fn parse_response(input: &str, cancel: &CancellationToken) -> Result<Value> {
    check_cancel(cancel)?;
    let mut parser = ResponseParser {
        input,
        cursor: 0,
        nodes: 0,
        steps: 0,
        next_scan: SCAN_BATCH,
    };
    let mut stack = Vec::<Container>::new();
    let mut root = None;
    loop {
        parser.steps += 1;
        if parser.steps.is_multiple_of(64) {
            checkpoint(cancel).await?;
        }
        check_cancel(cancel)?;
        parser.whitespace(cancel).await?;
        let byte = input.as_bytes().get(parser.cursor).copied();
        let expected = stack.last().map(Container::expected);
        match expected {
            Some(Expected::FirstKey | Expected::Key) => {
                if byte == Some(b'}') && expected == Some(Expected::FirstKey) {
                    parser.cursor += 1;
                    let value = stack.pop().expect("object").into_value();
                    put_value(&mut stack, &mut root, value)?;
                    continue;
                }
                if byte != Some(b'"') {
                    return invalid();
                }
                let key = parser.string(cancel).await?;
                let Some(Container::Object {
                    fields,
                    pending,
                    expected,
                }) = stack.last_mut()
                else {
                    unreachable!()
                };
                if fields.contains_key(&key) {
                    return invalid();
                }
                *pending = Some(key);
                *expected = Expected::Colon;
                continue;
            }
            Some(Expected::Colon) => {
                if byte != Some(b':') {
                    return invalid();
                }
                parser.cursor += 1;
                stack
                    .last_mut()
                    .expect("object")
                    .set_expected(Expected::Value);
                continue;
            }
            Some(Expected::Separator) => {
                let container = stack.last_mut().expect("container");
                let (closing, next) = match container {
                    Container::Array { .. } => (b']', Expected::Value),
                    Container::Object { .. } => (b'}', Expected::Key),
                };
                if byte == Some(b',') {
                    parser.cursor += 1;
                    container.set_expected(next);
                } else if byte == Some(closing) {
                    parser.cursor += 1;
                    let value = stack.pop().expect("container").into_value();
                    put_value(&mut stack, &mut root, value)?;
                } else {
                    return invalid();
                }
                continue;
            }
            Some(Expected::FirstValue) if byte == Some(b']') => {
                parser.cursor += 1;
                let value = stack.pop().expect("array").into_value();
                put_value(&mut stack, &mut root, value)?;
                continue;
            }
            None if root.is_some() => {
                if byte.is_some() {
                    return Err(ParseError::TrailingData);
                }
                checkpoint(cancel).await?;
                return Ok(root.expect("complete root"));
            }
            _ => {}
        }
        parser.nodes += 1;
        if parser.nodes > MAX_JSON_NODES {
            return Err(ParseError::NodeLimit);
        }
        match byte {
            Some(b'{') | Some(b'[') => {
                if stack.len() >= MAX_JSON_DEPTH {
                    return Err(ParseError::DepthLimit);
                }
                parser.cursor += 1;
                stack.push(if byte == Some(b'{') {
                    Container::Object {
                        fields: Map::new(),
                        pending: None,
                        expected: Expected::FirstKey,
                    }
                } else {
                    Container::Array {
                        values: Vec::new(),
                        expected: Expected::FirstValue,
                    }
                });
            }
            Some(b'"') => {
                let value = Value::String(parser.string(cancel).await?);
                put_value(&mut stack, &mut root, value)?;
            }
            Some(_) => {
                let start = parser.cursor;
                while let Some(&byte) = input.as_bytes().get(parser.cursor) {
                    if matches!(
                        byte,
                        b' ' | b'\r' | b'\n' | b'\t' | b',' | b']' | b'}' | b':'
                    ) {
                        break;
                    }
                    parser.cursor += 1;
                    parser.scan_checkpoint(cancel).await?;
                }
                if start == parser.cursor {
                    return invalid();
                }
                // JSON numbers have no useful representation here beyond one
                // bounded token. Avoid a second synchronous multi-MiB scan.
                if parser.cursor - start > SCAN_BATCH {
                    return Err(ParseError::ScalarTokenLimit);
                }
                let value = serde_json::from_str::<Value>(&input[start..parser.cursor])
                    .map_err(|_| invalid_message())?;
                if value.is_array() || value.is_object() {
                    return invalid();
                }
                put_value(&mut stack, &mut root, value)?;
            }
            None => return invalid(),
        }
    }
}

pub(crate) fn check_cancel(cancel: &CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        Err(ParseError::Cancelled)
    } else {
        Ok(())
    }
}

pub(crate) async fn checkpoint(cancel: &CancellationToken) -> Result<()> {
    check_cancel(cancel)?;
    tokio::task::yield_now().await;
    check_cancel(cancel)
}

fn invalid_message() -> ParseError {
    ParseError::InvalidSyntax
}

fn invalid<T>() -> Result<T> {
    Err(invalid_message())
}

struct ResponseParser<'a> {
    input: &'a str,
    cursor: usize,
    nodes: usize,
    steps: usize,
    next_scan: usize,
}

impl ResponseParser<'_> {
    async fn scan_checkpoint(&mut self, cancel: &CancellationToken) -> Result<()> {
        if self.cursor >= self.next_scan {
            checkpoint(cancel).await?;
            self.next_scan = self.cursor.saturating_add(SCAN_BATCH);
        }
        Ok(())
    }

    async fn whitespace(&mut self, cancel: &CancellationToken) -> Result<()> {
        while self
            .input
            .as_bytes()
            .get(self.cursor)
            .is_some_and(|byte| matches!(byte, b' ' | b'\r' | b'\n' | b'\t'))
        {
            self.cursor += 1;
            self.scan_checkpoint(cancel).await?;
        }
        Ok(())
    }

    async fn string(&mut self, cancel: &CancellationToken) -> Result<String> {
        self.cursor += 1;
        let mut value = String::new();
        while let Some(&byte) = self.input.as_bytes().get(self.cursor) {
            match byte {
                b'"' => {
                    self.cursor += 1;
                    return Ok(value);
                }
                b'\\' => {
                    self.cursor += 1;
                    let escaped = *self
                        .input
                        .as_bytes()
                        .get(self.cursor)
                        .ok_or_else(invalid_message)?;
                    self.cursor += 1;
                    value.push(match escaped {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => {
                            let first = self.hex_quad()?;
                            let code = if (0xd800..=0xdbff).contains(&first) {
                                if self.input.as_bytes().get(self.cursor..self.cursor + 2)
                                    != Some(b"\\u")
                                {
                                    return invalid();
                                }
                                self.cursor += 2;
                                let second = self.hex_quad()?;
                                if !(0xdc00..=0xdfff).contains(&second) {
                                    return invalid();
                                }
                                0x10000 + ((first - 0xd800) << 10) + second - 0xdc00
                            } else {
                                first
                            };
                            char::from_u32(code).ok_or_else(invalid_message)?
                        }
                        _ => return invalid(),
                    });
                }
                0..=0x1f => return invalid(),
                _ => {
                    let character = self.input[self.cursor..]
                        .chars()
                        .next()
                        .expect("UTF-8 character");
                    value.push(character);
                    self.cursor += character.len_utf8();
                }
            }
            self.scan_checkpoint(cancel).await?;
        }
        invalid()
    }

    fn hex_quad(&mut self) -> Result<u32> {
        let bytes = self
            .input
            .as_bytes()
            .get(self.cursor..self.cursor + 4)
            .ok_or_else(invalid_message)?;
        let mut value = 0;
        for byte in bytes {
            let digit = match byte {
                b'0'..=b'9' => byte - b'0',
                b'a'..=b'f' => byte - b'a' + 10,
                b'A'..=b'F' => byte - b'A' + 10,
                _ => return invalid(),
            };
            value = value * 16 + u32::from(digit);
        }
        self.cursor += 4;
        Ok(value)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Expected {
    FirstKey,
    Key,
    Colon,
    FirstValue,
    Value,
    Separator,
}

enum Container {
    Array {
        values: Vec<Value>,
        expected: Expected,
    },
    Object {
        fields: Map<String, Value>,
        pending: Option<String>,
        expected: Expected,
    },
}

impl Container {
    fn expected(&self) -> Expected {
        match self {
            Self::Array { expected, .. } | Self::Object { expected, .. } => *expected,
        }
    }
    fn set_expected(&mut self, next: Expected) {
        match self {
            Self::Array { expected, .. } | Self::Object { expected, .. } => *expected = next,
        }
    }
    fn into_value(self) -> Value {
        match self {
            Self::Array { values, .. } => Value::Array(values),
            Self::Object { fields, .. } => Value::Object(fields),
        }
    }
}

fn put_value(stack: &mut [Container], root: &mut Option<Value>, value: Value) -> Result<()> {
    match stack.last_mut() {
        Some(Container::Array { values, expected }) => {
            values.push(value);
            *expected = Expected::Separator;
        }
        Some(Container::Object {
            fields,
            pending,
            expected,
        }) => {
            let key = pending.take().ok_or_else(invalid_message)?;
            fields.insert(key, value);
            *expected = Expected::Separator;
        }
        None => *root = Some(value),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::task::noop_waker;
    use serde_json::json;
    use std::future::Future;
    use std::task::{Context, Poll};

    #[tokio::test]
    async fn strict_response_json_preserves_values_and_rejects_ambiguous_syntax() {
        for (input, expected) in [
            (
                r#" {"parts":[{"text":"你好\n\"end\""},null,true,false,-1,1.5,1e2,{},[]]} "#,
                json!({"parts":[{"text":"你好\n\"end\""},null,true,false,-1,1.5,100.0,{},[]]}),
            ),
            (
                r#"{"a\\":"\\\"","unicode":"\u4f60\u597d\ud83e\udd80\b\f\t\/"}"#,
                json!({"a\\":"\\\"","unicode":"你好🦀\u{8}\u{c}\t/"}),
            ),
            ("null", Value::Null),
            ("[]", json!([])),
            ("{}", json!({})),
        ] {
            assert_eq!(
                parse_response(input, &CancellationToken::new())
                    .await
                    .unwrap(),
                expected
            );
        }
        for input in [
            "",
            "[",
            "{",
            "[1,]",
            "{\"a\":1,}",
            "[1 2]",
            "{\"a\" 1}",
            "{1:2}",
            "[01]",
            "[+1]",
            "[truefalse]",
            "[NaN]",
            "{\"a\":}",
            "{\"a\":1} {}",
            "{\"x\":{\"a\":1,\"a\":2}}",
            "{\"a\":1,\"\\u0061\":2}",
            "\"unterminated",
            "\"bad\nstring\"",
            "\"\\q\"",
            "\"\\ud800\"",
            "\"\\udc00\"",
            "\"\\ud800\\u0000\"",
            "\"\\ud800\\u\"",
            "\"\\uZZZZ\"",
        ] {
            assert!(
                parse_response(input, &CancellationToken::new())
                    .await
                    .is_err(),
                "accepted {input:?}"
            );
        }
    }

    #[tokio::test]
    async fn syntax_and_trailing_data_have_distinct_error_kinds() {
        for (input, expected) in [
            ("{\"city\":", ParseError::InvalidSyntax),
            ("{\"city\":1,\"city\":2}", ParseError::InvalidSyntax),
            ("{} trailing", ParseError::TrailingData),
        ] {
            assert_eq!(
                parse_response(input, &CancellationToken::new())
                    .await
                    .unwrap_err(),
                expected,
            );
        }
    }

    #[tokio::test]
    async fn json_array_and_string_scanning_yield_before_the_whole_frame_is_parsed() {
        for input in [
            format!("[{} BROKEN]", "null,".repeat(3000)),
            format!("\"{}", "x".repeat(16_000)),
        ] {
            let cancel = CancellationToken::new();
            let mut parser = Box::pin(parse_response(&input, &cancel));
            let waker = noop_waker();
            let mut context = Context::from_waker(&waker);
            assert!(matches!(parser.as_mut().poll(&mut context), Poll::Pending));
            cancel.cancel();
            assert_eq!(parser.await.unwrap_err(), ParseError::Cancelled);
        }
    }

    #[tokio::test]
    async fn json_nodes_depth_and_scalar_tokens_are_bounded_before_tree_expansion() {
        for (input, expected) in [
            (
                format!("[{}null]", "null,".repeat(65_536)),
                ParseError::NodeLimit,
            ),
            (
                format!("{}null{}", "[".repeat(129), "]".repeat(129)),
                ParseError::DepthLimit,
            ),
            ("1".repeat(SCAN_BATCH + 1), ParseError::ScalarTokenLimit),
        ] {
            assert_eq!(
                parse_response(&input, &CancellationToken::new())
                    .await
                    .unwrap_err(),
                expected,
            );
        }
    }
}
