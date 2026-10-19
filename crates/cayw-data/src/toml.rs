use crate::{
    budget::{Budget, Result, Sink},
    error,
};
use serde_json::{Map, Value};

pub(crate) fn parse(text: &str, budget: &mut Budget<'_>) -> Result<Value> {
    preflight(text, budget)?;
    let value: toml::Value = toml::from_str(text).map_err(|e| error("syntax", e.to_string()))?;
    convert(value, budget, 1)
}

// Bound lexical nesting/dotted paths before the TOML library allocates a value tree.
// The library's own recursion guard stays enabled. Counts are conservative.
fn preflight(text: &str, budget: &Budget<'_>) -> Result<()> {
    let bytes = text.as_bytes();
    let mut i = 0;
    let mut containers = Vec::new();
    let mut path = 1_usize;
    let mut tokens = 0_usize;
    let mut in_token = false;
    let mut key_mode = true;
    while i < bytes.len() {
        if i.is_multiple_of(1024) {
            budget.checkpoint()?;
        }
        match bytes[i] {
            b'"' | b'\'' => {
                let quote = bytes[i];
                let triple = bytes.get(i..i + 3).is_some_and(|s| s == [quote; 3]);
                i += if triple { 3 } else { 1 };
                while i < bytes.len() {
                    if i.is_multiple_of(1024) {
                        budget.checkpoint()?;
                    }
                    if quote == b'"' && bytes[i] == b'\\' {
                        i += 2;
                        continue;
                    }
                    if bytes[i] == quote
                        && (!triple || bytes.get(i..i + 3).is_some_and(|s| s == [quote; 3]))
                    {
                        i += if triple { 3 } else { 1 };
                        break;
                    }
                    i += 1;
                }
                tokens += 1;
                in_token = false;
                continue;
            }
            b'#' => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'[' | b'{' => {
                containers.push(bytes[i]);
                if bytes[i] == b'{' {
                    key_mode = true;
                }
                tokens += 1;
                in_token = false;
            }
            b']' | b'}' => {
                containers.pop();
                in_token = false;
            }
            b'.' if key_mode => {
                path += 1;
                tokens += 1;
                in_token = false;
            }
            b'=' => {
                key_mode = false;
                path = 1;
                in_token = false;
            }
            b',' => {
                if containers.last() == Some(&b'{') {
                    key_mode = true;
                }
                path = 1;
                in_token = false;
            }
            b'\n' => {
                if containers.is_empty() {
                    key_mode = true;
                    path = 1;
                }
                in_token = false;
            }
            b' ' | b'\t' | b'\r' => {
                in_token = false;
            }
            _ => {
                if !in_token {
                    tokens += 1;
                    in_token = true;
                }
            }
        }
        if containers.len() + path > budget.limits.max_depth {
            return Err(error(
                "depth_limit",
                "TOML lexical nesting or dotted path exceeds max_depth",
            ));
        }
        if tokens > budget.limits.max_nodes {
            return Err(error("node_limit", "TOML token count exceeds max_nodes"));
        }
        i += 1;
    }
    Ok(())
}

fn convert(value: toml::Value, budget: &mut Budget<'_>, depth: usize) -> Result<Value> {
    budget.node(depth)?;
    match value {
        toml::Value::String(s) => Ok(Value::String(s)),
        toml::Value::Integer(i) => Ok(Value::from(i)),
        toml::Value::Float(f) => serde_json::Number::from_f64(f)
            .map(Value::Number)
            .ok_or_else(|| error("unsupported", "TOML nonfinite floats cannot be represented")),
        toml::Value::Boolean(b) => Ok(Value::Bool(b)),
        toml::Value::Datetime(_) => Err(error(
            "unsupported",
            "TOML datetime has no JSON type; quote it explicitly",
        )),
        toml::Value::Array(values) => {
            budget.rows(values.len())?;
            values
                .into_iter()
                .map(|v| convert(v, budget, depth + 1))
                .collect::<Result<Vec<_>>>()
                .map(Value::Array)
        }
        toml::Value::Table(values) => {
            let mut object = Map::new();
            for (key, value) in values {
                budget.node(depth + 1)?;
                object.insert(key, convert(value, budget, depth + 1)?);
            }
            Ok(Value::Object(object))
        }
    }
}

// A bounded writer emits quoted keys and inline tables. No intermediate output tree.
pub(crate) fn serialize(value: &Value, budget: &Budget<'_>) -> Result<String> {
    let object = value
        .as_object()
        .ok_or_else(|| error("unsupported", "a TOML document requires an object root"))?;
    let mut sink = Sink::new(budget.limits.output_bytes, budget.cancel);
    for (key, value) in object {
        budget.checkpoint()?;
        string(key, &mut sink)?;
        sink.push(" = ")?;
        write(value, &mut sink, budget)?;
        sink.push("\n")?;
    }
    sink.finish()
}

fn string(text: &str, sink: &mut Sink<'_>) -> Result<()> {
    sink.push("\"")?;
    for ch in text.chars() {
        match ch {
            '\\' => sink.push("\\\\")?,
            '"' => sink.push("\\\"")?,
            '\u{0}'..='\u{1f}' | '\u{7f}' => sink.push(&format!("\\u{:04x}", u32::from(ch)))?,
            _ => sink.push(ch.encode_utf8(&mut [0; 4]))?,
        }
    }
    sink.push("\"")
}

fn write(value: &Value, sink: &mut Sink<'_>, budget: &Budget<'_>) -> Result<()> {
    budget.checkpoint()?;
    match value {
        Value::Null => return Err(error("unsupported", "TOML has no null value")),
        Value::Bool(_) => sink.json(value)?,
        Value::String(text) => string(text, sink)?,
        Value::Number(number) => {
            if !number.is_f64() && number.as_i64().is_none() {
                return Err(error("unsupported", "TOML integers must fit signed 64-bit"));
            }
            sink.json(number)?;
        }
        Value::Array(values) => {
            sink.push("[")?;
            for (i, value) in values.iter().enumerate() {
                if i > 0 {
                    sink.push(", ")?;
                }
                write(value, sink, budget)?;
            }
            sink.push("]")?;
        }
        Value::Object(values) => {
            sink.push("{")?;
            for (i, (key, value)) in values.iter().enumerate() {
                if i > 0 {
                    sink.push(", ")?;
                }
                string(key, sink)?;
                sink.push(" = ")?;
                write(value, sink, budget)?;
            }
            sink.push("}")?;
        }
    }
    Ok(())
}
