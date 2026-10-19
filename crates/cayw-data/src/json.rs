use crate::{
    budget::{Budget, Result, Sink},
    error,
};
use serde_json::{Map, Number, Value};
use std::str::FromStr;

pub(crate) fn parse(text: &str, budget: &mut Budget<'_>) -> Result<Value> {
    let mut parser = Parser {
        text,
        offset: 0,
        budget,
    };
    let value = parser.value(1)?;
    parser.space();
    if parser.offset != text.len() {
        return Err(error("syntax", "trailing JSON content"));
    }
    Ok(value)
}

pub(crate) fn serialize(value: &Value, budget: &Budget<'_>) -> Result<String> {
    let mut sink = Sink::new(budget.limits.output_bytes, budget.cancel);
    sink.json(value)?;
    sink.finish()
}

struct Parser<'a, 'b, 'c> {
    text: &'a str,
    offset: usize,
    budget: &'b mut Budget<'c>,
}

impl Parser<'_, '_, '_> {
    fn space(&mut self) {
        while self
            .text
            .as_bytes()
            .get(self.offset)
            .is_some_and(|b| matches!(b, b' ' | b'\n' | b'\r' | b'\t'))
        {
            self.offset += 1;
        }
    }
    fn take(&mut self, byte: u8) -> bool {
        self.space();
        if self.text.as_bytes().get(self.offset) == Some(&byte) {
            self.offset += 1;
            true
        } else {
            false
        }
    }
    fn string(&mut self) -> Result<String> {
        self.space();
        let start = self.offset;
        if !self.take(b'"') {
            return Err(error("syntax", "expected JSON string"));
        }
        let mut escaped = false;
        while let Some(&byte) = self.text.as_bytes().get(self.offset) {
            self.offset += 1;
            if !escaped && byte == b'"' {
                return serde_json::from_str(&self.text[start..self.offset])
                    .map_err(|e| error("syntax", e.to_string()));
            }
            escaped = !escaped && byte == b'\\';
            if self.offset.is_multiple_of(4096) {
                self.budget.checkpoint()?;
            }
        }
        Err(error("syntax", "unterminated JSON string"))
    }
    fn value(&mut self, depth: usize) -> Result<Value> {
        self.budget.node(depth)?;
        self.space();
        match self.text.as_bytes().get(self.offset).copied() {
            Some(b'"') => self.string().map(Value::String),
            Some(b'[') => {
                self.offset += 1;
                let mut values = Vec::new();
                if self.take(b']') {
                    return Ok(Value::Array(values));
                }
                loop {
                    self.budget.rows(values.len() + 1)?;
                    values.push(self.value(depth + 1)?);
                    if self.take(b']') {
                        break;
                    }
                    if !self.take(b',') {
                        return Err(error("syntax", "expected comma or closing array"));
                    }
                }
                Ok(Value::Array(values))
            }
            Some(b'{') => {
                self.offset += 1;
                let mut values = Map::new();
                if self.take(b'}') {
                    return Ok(Value::Object(values));
                }
                loop {
                    self.budget.node(depth + 1)?;
                    let key = self.string()?;
                    if values.contains_key(&key) {
                        return Err(error("syntax", "duplicate JSON object key"));
                    }
                    if !self.take(b':') {
                        return Err(error("syntax", "expected colon after JSON key"));
                    }
                    values.insert(key, self.value(depth + 1)?);
                    if self.take(b'}') {
                        break;
                    }
                    if !self.take(b',') {
                        return Err(error("syntax", "expected comma or closing object"));
                    }
                }
                Ok(Value::Object(values))
            }
            Some(_) => {
                let start = self.offset;
                while self.text.as_bytes().get(self.offset).is_some_and(|b| {
                    !matches!(b, b',' | b']' | b'}' | b' ' | b'\n' | b'\r' | b'\t')
                }) {
                    self.offset += 1;
                }
                let token = &self.text[start..self.offset];
                match token {
                    "null" => Ok(Value::Null),
                    "true" => Ok(Value::Bool(true)),
                    "false" => Ok(Value::Bool(false)),
                    _ => number(token).map(Value::Number),
                }
            }
            None => Err(error("syntax", "unexpected end of JSON")),
        }
    }
}

pub(crate) fn number(token: &str) -> Result<Number> {
    // serde_json otherwise rounds overflowing integer tokens into an f64.
    if !token.contains(['.', 'e', 'E']) {
        if token.starts_with('-') {
            token
                .parse::<i64>()
                .map_err(|_| error("syntax", "integer is outside signed 64-bit range"))?;
        } else {
            token
                .parse::<u64>()
                .map_err(|_| error("syntax", "integer is outside unsigned 64-bit range"))?;
        }
    }
    Number::from_str(token).map_err(|_| error("syntax", "invalid or nonfinite JSON number"))
}
