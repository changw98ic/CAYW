use crate::ErrorKind;
use crate::runtime::Fault;
use mlua::{Lua, LuaSerdeExt, Value as LuaValue};
use serde::Serialize;
use serde_json::{Map, Number, Value};
use std::{
    collections::{BTreeMap, HashSet},
    io::{self, Write},
};

const MAX_DEPTH: usize = 64;
const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

fn conversion(message: impl Into<String>) -> Fault {
    Fault::new(ErrorKind::Conversion, message)
}

fn output_limit() -> Fault {
    Fault::new(ErrorKind::OutputLimit, "JSON payload budget exhausted")
}

pub(crate) fn size(value: &(impl Serialize + ?Sized), limit: usize) -> Result<usize, Fault> {
    struct Counter {
        bytes: usize,
        limit: usize,
    }
    impl Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.bytes = self.bytes.saturating_add(bytes.len());
            if self.bytes > self.limit {
                return Err(io::Error::other("JSON payload budget exhausted"));
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter { bytes: 0, limit };
    serde_json::to_writer(&mut counter, value).map_err(|_| output_limit())?;
    Ok(counter.bytes)
}

struct Budget {
    remaining: usize,
}

impl Budget {
    fn bytes(&mut self, count: usize) -> Result<(), Fault> {
        self.remaining = self.remaining.checked_sub(count).ok_or_else(output_limit)?;
        Ok(())
    }

    fn value(&mut self, value: &(impl Serialize + ?Sized)) -> Result<(), Fault> {
        self.bytes(size(value, self.remaining)?)
    }
}

pub(crate) fn from_lua(lua: &Lua, value: LuaValue, limit: usize) -> Result<Value, Fault> {
    convert(
        lua,
        value,
        &mut HashSet::new(),
        0,
        &mut Budget { remaining: limit },
    )
}

fn convert(
    lua: &Lua,
    value: LuaValue,
    visited: &mut HashSet<usize>,
    depth: usize,
    budget: &mut Budget,
) -> Result<Value, Fault> {
    if depth > MAX_DEPTH {
        return Err(conversion("JSON nesting exceeds 64 levels"));
    }
    match value {
        LuaValue::Nil => {
            budget.bytes(4)?;
            Ok(Value::Null)
        }
        LuaValue::LightUserData(pointer) if pointer.0.is_null() => {
            budget.bytes(4)?;
            Ok(Value::Null)
        }
        LuaValue::Boolean(boolean) => {
            budget.bytes(if boolean { 4 } else { 5 })?;
            Ok(Value::Bool(boolean))
        }
        LuaValue::Integer(integer) => {
            let value = number(integer as f64)?;
            budget.value(&value)?;
            Ok(value)
        }
        LuaValue::Number(value) => {
            let value = number(value)?;
            budget.value(&value)?;
            Ok(value)
        }
        LuaValue::String(string) => {
            let text = string
                .to_str()
                .map_err(|_| conversion("JSON strings must contain valid UTF-8"))?;
            // Charge the escaped UTF-8 representation before allocating a Rust copy.
            budget.value(text.as_ref())?;
            Ok(Value::String(text.to_owned()))
        }
        LuaValue::Table(table) => {
            let pointer = table.to_pointer() as usize;
            if !visited.insert(pointer) {
                return Err(conversion("Cyclic tables cannot be represented as JSON"));
            }
            let is_array = if let Some(metatable) = table.metatable() {
                if metatable.to_pointer() != lua.array_metatable().to_pointer() {
                    return Err(conversion("JSON tables cannot have custom metatables"));
                }
                true
            } else {
                false
            };
            budget.bytes(2)?; // Array brackets or object braces, including empty tables.
            let mut integers = BTreeMap::new();
            let mut strings = Map::new();
            for (entry, pair) in table.pairs::<LuaValue, LuaValue>().enumerate() {
                let (key, value) = pair.map_err(Fault::from_lua)?;
                if entry > 0 {
                    budget.bytes(1)?;
                }
                match key {
                    LuaValue::String(key) => {
                        if is_array || !integers.is_empty() {
                            return Err(conversion("JSON tables cannot mix array and object keys"));
                        }
                        let key = key
                            .to_str()
                            .map_err(|_| conversion("JSON object keys must contain valid UTF-8"))?;
                        budget.value(key.as_ref())?;
                        budget.bytes(1)?; // Colon between object key and value.
                        strings.insert(
                            key.to_owned(),
                            convert(lua, value, visited, depth + 1, budget)?,
                        );
                    }
                    LuaValue::Integer(index) if index > 0 => {
                        if !strings.is_empty() {
                            return Err(conversion("JSON tables cannot mix array and object keys"));
                        }
                        integers.insert(
                            index as usize,
                            convert(lua, value, visited, depth + 1, budget)?,
                        );
                    }
                    LuaValue::Number(index)
                        if index > 0.0
                            && index.fract() == 0.0
                            && index <= MAX_SAFE_INTEGER
                            && index < usize::MAX as f64 =>
                    {
                        if !strings.is_empty() {
                            return Err(conversion("JSON tables cannot mix array and object keys"));
                        }
                        integers.insert(
                            index as usize,
                            convert(lua, value, visited, depth + 1, budget)?,
                        );
                    }
                    _ => {
                        return Err(conversion(
                            "JSON table keys must be strings or positive contiguous array indices",
                        ));
                    }
                }
            }
            visited.remove(&pointer);
            if !integers.is_empty() {
                if integers.last_key_value().map(|(index, _)| *index) != Some(integers.len()) {
                    return Err(conversion(
                        "Sparse tables cannot be represented as JSON arrays",
                    ));
                }
                Ok(Value::Array(integers.into_values().collect()))
            } else if is_array {
                Ok(Value::Array(vec![]))
            } else {
                Ok(Value::Object(strings))
            }
        }
        other => Err(conversion(format!(
            "{} values cannot be represented as JSON",
            other.type_name()
        ))),
    }
}

fn number(value: f64) -> Result<Value, Fault> {
    if !value.is_finite() {
        return Err(conversion("JSON numbers must be finite"));
    }
    if value.fract() == 0.0 {
        if value.abs() > MAX_SAFE_INTEGER {
            return Err(conversion(
                "Integer exceeds Luau's exact JSON integer range",
            ));
        }
        return Ok(Value::Number(Number::from(value as i64)));
    }
    Number::from_f64(value)
        .map(Value::Number)
        .ok_or_else(|| conversion("Invalid JSON number"))
}

pub(crate) fn to_lua(lua: &Lua, value: &Value) -> Result<LuaValue, Fault> {
    validate_host_value(value, usize::MAX)?;
    lua.to_value(value).map_err(Fault::from_lua)
}

/// Own the unchecked value until both depth and payload checks have succeeded.
/// Even rejecting an early sibling must drain every other unchecked descendant.
pub(crate) fn from_host(value: Value, limit: usize) -> Result<Value, Fault> {
    match validate_host_value(&value, limit) {
        Ok(()) => Ok(value),
        Err(fault) => {
            discard(value);
            Err(fault)
        }
    }
}

pub(crate) fn discard(value: Value) {
    let mut pending = vec![value];
    while let Some(value) = pending.pop() {
        match value {
            Value::Array(mut values) => pending.append(&mut values),
            Value::Object(values) => pending.extend(values.into_values()),
            _ => {}
        }
    }
}

/// A depth-first walk keeps at most one iterator per permitted nesting level.
/// Only scalar values reach Serialize; unchecked containers never do.
pub(crate) fn validate_host_value(value: &Value, limit: usize) -> Result<(), Fault> {
    enum Children<'a> {
        Array(std::slice::Iter<'a, Value>),
        Object(serde_json::map::Iter<'a>),
    }
    impl<'a> Children<'a> {
        fn next(&mut self) -> Option<(Option<&'a str>, &'a Value)> {
            match self {
                Self::Array(values) => values.next().map(|value| (None, value)),
                Self::Object(values) => values
                    .next()
                    .map(|(key, value)| (Some(key.as_str()), value)),
            }
        }
    }
    let mut budget = Budget { remaining: limit };
    let mut pending = vec![Children::Array(std::slice::from_ref(value).iter())];
    while let Some(children) = pending.last_mut() {
        let Some((key, value)) = children.next() else {
            pending.pop();
            continue;
        };
        if pending.len() - 1 > MAX_DEPTH {
            return Err(conversion("Host JSON nesting exceeds 64 levels"));
        }
        if let Some(key) = key {
            budget.value(key)?;
            budget.bytes(1)?;
        }
        match value {
            Value::Array(values) => {
                budget.bytes(2)?;
                budget.bytes(values.len().saturating_sub(1))?;
                pending.push(Children::Array(values.iter()));
            }
            Value::Object(values) => {
                budget.bytes(2)?;
                budget.bytes(values.len().saturating_sub(1))?;
                pending.push(Children::Object(values.iter()));
            }
            Value::Number(number) => {
                validate_host_number(number)?;
                budget.value(number)?;
            }
            scalar => budget.value(scalar)?,
        }
    }
    Ok(())
}

fn validate_host_number(number: &Number) -> Result<(), Fault> {
    if let Some(integer) = number.as_i64() {
        if integer.unsigned_abs() > MAX_SAFE_INTEGER as u64 {
            return Err(conversion(
                "Host integer cannot be represented exactly by Luau",
            ));
        }
    } else if let Some(integer) = number.as_u64() {
        if integer > MAX_SAFE_INTEGER as u64 {
            return Err(conversion(
                "Host integer cannot be represented exactly by Luau",
            ));
        }
    } else if number.as_f64().is_none_or(|value| !value.is_finite()) {
        return Err(conversion("Host number cannot be represented by Luau"));
    }
    Ok(())
}

pub(crate) fn bounded_message(message: &str) -> &str {
    const MAX_ERROR_BYTES: usize = 2048;
    let mut end = message.len().min(MAX_ERROR_BYTES);
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    &message[..end]
}

/// Empty means omitted: even JSON string quotes may exceed a tiny budget.
/// Use the actual JSON writer so escaping counts exactly as other payloads do.
pub(crate) fn fit_message(message: &str, limit: usize) -> (String, usize) {
    if limit < 3 || message.is_empty() {
        return (String::new(), 0);
    }
    let message = bounded_message(message);
    let boundaries: Vec<_> = message
        .char_indices()
        .map(|(index, _)| index)
        .chain(std::iter::once(message.len()))
        .collect();
    let (mut low, mut high) = (0, boundaries.len() - 1);
    while low < high {
        let middle = low + (high - low).div_ceil(2);
        if size(&message[..boundaries[middle]], limit).is_ok() {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    if low == 0 {
        return (String::new(), 0);
    }
    let message = &message[..boundaries[low]];
    let bytes = size(message, limit).expect("prefix fits the JSON budget");
    (message.to_owned(), bytes)
}
