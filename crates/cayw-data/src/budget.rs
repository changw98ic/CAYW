use crate::{Limits, error};
use cayw_ai::CancellationToken;
use cayw_codemode::CapabilityError;
use serde_json::Value;
use std::io::{self, Write};

pub(crate) type Result<T> = std::result::Result<T, CapabilityError>;

pub(crate) struct Budget<'a> {
    pub limits: Limits,
    pub cancel: &'a CancellationToken,
    nodes: usize,
}

impl<'a> Budget<'a> {
    pub fn new(limits: Limits, cancel: &'a CancellationToken) -> Self {
        Self {
            limits,
            cancel,
            nodes: 0,
        }
    }

    pub fn checkpoint(&self) -> Result<()> {
        if self.cancel.is_cancelled() {
            Err(error("cancelled", "data operation cancelled"))
        } else {
            Ok(())
        }
    }

    pub fn node(&mut self, depth: usize) -> Result<()> {
        self.checkpoint()?;
        if depth > self.limits.max_depth {
            return Err(error("depth_limit", "data nesting exceeds max_depth"));
        }
        self.nodes += 1;
        if self.nodes > self.limits.max_nodes {
            return Err(error("node_limit", "data exceeds max_nodes"));
        }
        Ok(())
    }

    pub fn rows(&self, rows: usize) -> Result<()> {
        self.checkpoint()?;
        if rows > self.limits.max_rows {
            Err(error("row_limit", "data exceeds max_rows"))
        } else {
            Ok(())
        }
    }
}

/// The argument/result wrapper has a fixed allowance; logical data is checked separately.
pub(crate) fn check_envelope(
    value: &Value,
    limits: Limits,
    cancel: &CancellationToken,
    output: bool,
) -> Result<()> {
    let allowance = Limits {
        max_depth: limits.max_depth + 8,
        max_nodes: limits.max_nodes + 128,
        ..limits
    };
    check_value(value, &mut Budget::new(allowance, cancel), 1)?;
    let maximum = if output {
        limits.output_bytes
    } else {
        limits.input_bytes
    };
    let kind = if output {
        "output_limit"
    } else {
        "input_limit"
    };
    let mut sink = Sink::counting(maximum, cancel, kind);
    serde_json::to_writer(&mut sink, value).map_err(|e| error(kind, e.to_string()))?;
    Ok(())
}

pub(crate) fn check_value(value: &Value, budget: &mut Budget<'_>, depth: usize) -> Result<()> {
    budget.node(depth)?;
    match value {
        Value::Array(items) => {
            budget.rows(items.len())?;
            for item in items {
                check_value(item, budget, depth + 1)?;
            }
        }
        Value::Object(fields) => {
            for item in fields.values() {
                budget.node(depth + 1)?; // Account for each key as well as its value.
                check_value(item, budget, depth + 1)?;
            }
        }
        _ => {}
    }
    Ok(())
}

pub(crate) struct Sink<'a> {
    bytes: Vec<u8>,
    written: usize,
    maximum: usize,
    retain: bool,
    cancel: &'a CancellationToken,
    kind: &'static str,
}

impl<'a> Sink<'a> {
    pub fn new(maximum: usize, cancel: &'a CancellationToken) -> Self {
        Self {
            bytes: Vec::new(),
            written: 0,
            maximum,
            retain: true,
            cancel,
            kind: "output_limit",
        }
    }
    fn counting(maximum: usize, cancel: &'a CancellationToken, kind: &'static str) -> Self {
        Self {
            retain: false,
            kind,
            ..Self::new(maximum, cancel)
        }
    }
    pub fn push(&mut self, text: &str) -> Result<()> {
        self.write_all(text.as_bytes())
            .map_err(|e| error(self.kind, e.to_string()))
    }
    pub fn json(&mut self, value: &impl serde::Serialize) -> Result<()> {
        serde_json::to_writer(&mut *self, value).map_err(|e| error(self.kind, e.to_string()))
    }
    pub fn finish(self) -> Result<String> {
        String::from_utf8(self.bytes).map_err(|e| error("encoding", e.to_string()))
    }
}

impl Write for Sink<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.cancel.is_cancelled() {
            return Err(io::Error::other("cancelled: data operation cancelled"));
        }
        if bytes.len() > self.maximum.saturating_sub(self.written) {
            return Err(io::Error::other(format!(
                "{}: encoded payload exceeds byte limit",
                self.kind
            )));
        }
        self.written += bytes.len();
        if self.retain {
            self.bytes.extend_from_slice(bytes);
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
