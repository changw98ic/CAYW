use crate::common::{self, Result};
use cayw_ai::CancellationToken;
use serde_json::{Map, Value};

// The HTTP path builds one JSON tree with bounded depth/node count. It yields
// while scanning strings/whitespace and between structural tokens, so even an
// array of thousands of empty values cannot monopolize a runtime thread. Each
// scalar still uses serde_json's syntax/number/string validation.
const MAX_JSON_NODES: usize = 65_536;
const MAX_JSON_DEPTH: usize = 128;
const SCAN_BATCH: usize = 4096;

pub(crate) async fn parse_response(input: &str, cancel: &CancellationToken) -> Result<Value> {
    common::check_cancel(cancel)?;
    let mut parser = ResponseParser {
        input,
        cursor: 0,
        nodes: 0,
        steps: 0,
    };
    let mut stack = Vec::<Container>::new();
    let mut root = None;
    loop {
        parser.steps += 1;
        if parser.steps.is_multiple_of(64) {
            common::checkpoint(cancel).await?;
        }
        common::check_cancel(cancel)?;
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
                    return Err("Trailing data in provider JSON".into());
                }
                common::checkpoint(cancel).await?;
                return Ok(root.expect("complete root"));
            }
            _ => {}
        }
        parser.nodes += 1;
        if parser.nodes > MAX_JSON_NODES {
            return Err("Provider JSON exceeds the node limit".into());
        }
        match byte {
            Some(b'{') | Some(b'[') => {
                if stack.len() >= MAX_JSON_DEPTH {
                    return Err("Provider JSON exceeds the depth limit".into());
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

fn invalid_message() -> String {
    "Invalid, incomplete or duplicate-key JSON".into()
}

fn invalid<T>() -> Result<T> {
    Err(invalid_message())
}

struct ResponseParser<'a> {
    input: &'a str,
    cursor: usize,
    nodes: usize,
    steps: usize,
}

impl ResponseParser<'_> {
    async fn scan_checkpoint(&self, cancel: &CancellationToken) -> Result<()> {
        if self.cursor.is_multiple_of(SCAN_BATCH) {
            common::checkpoint(cancel).await?;
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
        let start = self.cursor;
        self.cursor += 1;
        let mut escaped = false;
        while let Some(&byte) = self.input.as_bytes().get(self.cursor) {
            self.cursor += 1;
            self.scan_checkpoint(cancel).await?;
            if byte == b'"' && !escaped {
                return serde_json::from_str(&self.input[start..self.cursor])
                    .map_err(|_| invalid_message());
            }
            escaped = byte == b'\\' && !escaped;
        }
        invalid()
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
                r#"{"a\\":"\\\"","unicode":"\u4f60\u597d"}"#,
                json!({"a\\":"\\\"","unicode":"你好"}),
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
            assert_eq!(parser.await.unwrap_err(), "Request cancelled");
        }
    }

    #[tokio::test]
    async fn json_nodes_and_depth_are_bounded_before_tree_expansion() {
        let nodes = format!("[{}null]", "null,".repeat(65_536));
        let depth = format!("{}null{}", "[".repeat(129), "]".repeat(129));
        for input in [nodes, depth] {
            let error = parse_response(&input, &CancellationToken::new())
                .await
                .unwrap_err();
            assert!(error.contains("limit"), "{error}");
        }
    }
}
