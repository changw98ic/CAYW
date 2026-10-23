use cayw_codemode::CapabilityError;
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    io::{self, Write},
    ops::{Deref, DerefMut},
};

/// Own untrusted JSON before constructing an async future. Rejection and
/// unpolled-future drop must not recursively destroy an attacker-owned tree.
pub(crate) struct OwnedJson(pub(crate) Value);
impl OwnedJson {
    pub(crate) fn take(&mut self) -> Value {
        self.0.take()
    }
}
impl Deref for OwnedJson {
    type Target = Value;
    fn deref(&self) -> &Value {
        &self.0
    }
}
impl DerefMut for OwnedJson {
    fn deref_mut(&mut self) -> &mut Value {
        &mut self.0
    }
}
impl Drop for OwnedJson {
    fn drop(&mut self) {
        drop_json(self.take());
    }
}
pub(crate) fn drop_json(value: Value) {
    let mut pending = vec![value];
    while let Some(value) = pending.pop() {
        match value {
            Value::Array(values) => pending.extend(values),
            Value::Object(values) => pending.extend(values.into_values()),
            _ => {}
        }
    }
}

pub(crate) fn failure(message: impl Into<String>) -> CapabilityError {
    let mut message = message.into();
    if message.len() > 2048 {
        let mut end = 2048;
        while !message.is_char_boundary(end) {
            end -= 1;
        }
        message.truncate(end);
    }
    CapabilityError::new(message)
}

pub(crate) fn digest(value: &[u8]) -> String {
    format!("{:x}", Sha256::digest(value))
}

pub fn content_sha256(content: &str) -> String {
    digest(content.as_bytes())
}

pub(crate) fn size(value: &impl Serialize, limit: usize) -> Result<usize, CapabilityError> {
    struct Counter {
        used: usize,
        limit: usize,
    }
    impl Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() > self.limit.saturating_sub(self.used) {
                return Err(io::Error::other("bridge JSON byte budget exceeded"));
            }
            self.used += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter { used: 0, limit };
    serde_json::to_writer(&mut counter, value)
        .map_err(|_| failure("bridge JSON byte budget exceeded"))?;
    Ok(counter.used)
}

/// Reject deep trees before serialization, validation, or cloning can recurse.
/// Reference-based traversal allocates no copy of untrusted source content.
pub(crate) fn value_size(value: &Value, limit: usize) -> Result<usize, CapabilityError> {
    fn check(value: &Value, depth: usize) -> Result<(), CapabilityError> {
        if depth > 48 {
            return Err(failure("bridge JSON nesting exceeds 48"));
        }
        match value {
            Value::Object(object) => {
                for value in object.values() {
                    check(value, depth + 1)?;
                }
            }
            Value::Array(array) => {
                for value in array {
                    check(value, depth + 1)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    check(value, 0)?;
    size(value, limit)
}

pub(crate) fn name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
}

pub(crate) fn sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

/// URI syntax only. The value is an opaque source key; it is never resolved,
/// decoded or opened as a filesystem path, and fragments/queries are not data.
pub(crate) fn resource_uri(value: &str) -> bool {
    if value.is_empty() || value.len() > 2048 || !value.is_ascii() {
        return false;
    }
    if value
        .bytes()
        .any(|c| c.is_ascii_control() || c.is_ascii_whitespace())
    {
        return false;
    }
    let Some((scheme, rest)) = value.split_once(':') else {
        return false;
    };
    !scheme.is_empty()
        && scheme.as_bytes()[0].is_ascii_alphabetic()
        && scheme
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'+' | b'-' | b'.'))
        && rest.bytes().any(|c| c.is_ascii_alphanumeric())
}

/// `type/subtype` tokens only, without parameters. MIME is explicit metadata,
/// not a hint that may be rewritten by the bridge.
pub(crate) fn mime_type(value: &str) -> bool {
    if value.is_empty() || value.len() > 256 || !value.is_ascii() {
        return false;
    }
    let Some((kind, subtype)) = value.split_once('/') else {
        return false;
    };
    let token = |byte: u8| {
        byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'!' | b'#'
                    | b'$'
                    | b'%'
                    | b'&'
                    | b'\''
                    | b'*'
                    | b'+'
                    | b'-'
                    | b'.'
                    | b'^'
                    | b'_'
                    | b'`'
                    | b'|'
                    | b'~'
            )
    };
    !kind.is_empty()
        && !subtype.is_empty()
        && !subtype.contains('/')
        && kind.bytes().all(token)
        && subtype.bytes().all(token)
}

/// All object schemas are closed. Local refs are accepted by the same validator
/// used by the Core; external schema resolution is rejected without IO.
pub(crate) fn strict_schema(schema: &Value) -> Result<(), CapabilityError> {
    fn walk(value: &Value, depth: usize) -> Result<(), CapabilityError> {
        if depth > 32 {
            return Err(failure("schema nesting exceeds 32"));
        }
        match value {
            Value::Object(object) => {
                let object_type = object.get("type").is_some_and(|kind| {
                    kind == "object"
                        || kind
                            .as_array()
                            .is_some_and(|types| types.iter().any(|t| t == "object"))
                });
                let object_shape = object.contains_key("properties")
                    || object.contains_key("patternProperties")
                    || object.contains_key("additionalProperties");
                if (object_type || object_shape)
                    && object.get("additionalProperties") != Some(&Value::Bool(false))
                {
                    return Err(failure("object schemas require additionalProperties=false"));
                }
                // Traverse schema positions only. Names in `properties` and
                // data in `const`/`enum` are not schema keywords themselves.
                for key in [
                    "properties",
                    "patternProperties",
                    "$defs",
                    "definitions",
                    "dependentSchemas",
                ] {
                    if let Some(Value::Object(children)) = object.get(key) {
                        for child in children.values() {
                            walk(child, depth + 1)?;
                        }
                    }
                }
                for key in ["allOf", "anyOf", "oneOf", "prefixItems"] {
                    if let Some(Value::Array(children)) = object.get(key) {
                        for child in children {
                            walk(child, depth + 1)?;
                        }
                    }
                }
                for key in [
                    "items",
                    "contains",
                    "not",
                    "if",
                    "then",
                    "else",
                    "additionalItems",
                    "unevaluatedItems",
                    "unevaluatedProperties",
                    "propertyNames",
                    "additionalProperties",
                ] {
                    if let Some(child) = object.get(key) {
                        walk(child, depth + 1)?;
                    }
                }
            }
            Value::Array(array) => {
                for child in array {
                    walk(child, depth + 1)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    if schema["type"] != "object" {
        return Err(failure("tool schemas must declare type=object"));
    }
    walk(schema, 0)
}

pub(crate) fn lock<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}
