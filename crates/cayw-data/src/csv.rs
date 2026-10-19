use crate::{
    budget::{Budget, Result, Sink},
    error,
};
use serde_json::{Value, json};
use std::collections::BTreeSet;

pub(crate) fn parse(text: &str, budget: &mut Budget<'_>) -> Result<Value> {
    strict_syntax(text, budget)?;
    budget.node(1)?;
    for _ in 0..4 {
        budget.node(2)?;
    }
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(false)
        .from_reader(text.as_bytes());
    let mut record = csv::StringRecord::new();
    if !reader
        .read_record(&mut record)
        .map_err(|e| error("syntax", e.to_string()))?
    {
        return Err(error("syntax", "CSV requires a header"));
    }
    budget.rows(record.len())?;
    let header: Vec<_> = record.iter().map(str::to_owned).collect();
    check_header(&header)?;
    for _ in &header {
        budget.node(3)?;
    }
    let mut rows = Vec::new();
    while reader
        .read_record(&mut record)
        .map_err(|e| error("syntax", e.to_string()))?
    {
        budget.rows(rows.len() + 1)?;
        budget.rows(record.len())?;
        budget.node(3)?;
        let mut row = Vec::new();
        for field in &record {
            budget.node(4)?;
            row.push(Value::String(field.to_owned()));
        }
        rows.push(Value::Array(row));
    }
    Ok(json!({"header":header,"rows":rows}))
}

fn check_header(header: &[String]) -> Result<()> {
    let mut seen = BTreeSet::new();
    if header.is_empty() {
        return Err(error("arguments", "CSV requires at least one column"));
    }
    for field in header {
        if !seen.insert(field) {
            return Err(error("syntax", "CSV header names must be unique"));
        }
    }
    Ok(())
}

// csv's permissive reader accepts unclosed quotes/stray quotes and skips blank rows.
// Reject those cases explicitly so malformed input cannot silently lose content.
fn strict_syntax(text: &str, budget: &Budget<'_>) -> Result<()> {
    #[derive(Clone, Copy)]
    enum State {
        Start,
        Bare,
        Quoted,
        Closed,
    }
    let mut state = State::Start;
    let mut row_bytes = 0;
    let mut records = 0_usize;
    let mut cells = 1_usize;
    let mut columns = 1_usize;
    // Match the reader's stream marker handling without stripping a literal BOM
    // inside a quoted field or a later record.
    let mut bytes = text
        .strip_prefix('\u{feff}')
        .unwrap_or(text)
        .bytes()
        .peekable();
    let mut offset = 0_usize;
    while let Some(byte) = bytes.next() {
        offset += 1;
        if offset.is_multiple_of(1024) {
            budget.checkpoint()?;
        }
        if !matches!(state, State::Quoted) && matches!(byte, b'\n' | b'\r') {
            if row_bytes == 0 {
                return Err(error(
                    "syntax",
                    "blank CSV records must use a quoted empty field",
                ));
            }
            if byte == b'\r' && bytes.next_if_eq(&b'\n').is_none() {
                return Err(error("syntax", "CSV line endings must be LF or CRLF"));
            }
            records += 1;
            budget.rows(records.saturating_sub(1))?;
            columns = 1;
            state = State::Start;
            row_bytes = 0;
            continue;
        }
        row_bytes += 1;
        state = match (state, byte) {
            (State::Start, b'"') => State::Quoted,
            (State::Start | State::Bare | State::Closed, b',') => {
                cells += 1;
                columns += 1;
                budget.rows(columns)?;
                State::Start
            }
            (State::Bare, b'"') => return Err(error("syntax", "stray quote in CSV field")),
            (State::Start | State::Bare, _) => State::Bare,
            (State::Quoted, b'"') => State::Closed,
            (State::Quoted, _) => State::Quoted,
            (State::Closed, b'"') => State::Quoted,
            (State::Closed, _) => return Err(error("syntax", "content after CSV closing quote")),
        };
        if cells > budget.limits.max_nodes {
            return Err(error("node_limit", "CSV cell count exceeds max_nodes"));
        }
    }
    if matches!(state, State::Quoted) {
        return Err(error("syntax", "unclosed CSV quoted field"));
    }
    if row_bytes > 0 {
        budget.rows(records)?;
    }
    Ok(())
}

pub(crate) fn serialize(value: &Value, budget: &Budget<'_>) -> Result<String> {
    let object = value
        .as_object()
        .filter(|v| v.len() == 2)
        .ok_or_else(|| error("arguments", "CSV value requires exactly header and rows"))?;
    let header = object
        .get("header")
        .and_then(Value::as_array)
        .ok_or_else(|| error("arguments", "CSV header must be an array of strings"))?;
    let header = header
        .iter()
        .map(|v| {
            v.as_str()
                .map(str::to_owned)
                .ok_or_else(|| error("arguments", "CSV headers must be strings"))
        })
        .collect::<Result<Vec<_>>>()?;
    check_header(&header)?;
    let rows = object
        .get("rows")
        .and_then(Value::as_array)
        .ok_or_else(|| error("arguments", "CSV rows must be arrays of strings"))?;
    budget.rows(rows.len())?;
    let sink = Sink::new(budget.limits.output_bytes, budget.cancel);
    // The reader strips a BOM only at the start of the stream. Quoting every
    // field puts an opening quote before a literal U+FEFF in the first header.
    let mut writer = csv::WriterBuilder::new()
        .quote_style(csv::QuoteStyle::Always)
        .from_writer(sink);
    writer
        .write_record(&header)
        .map_err(|e| error("output_limit", e.to_string()))?;
    for row in rows {
        budget.checkpoint()?;
        let row = row
            .as_array()
            .filter(|row| row.len() == header.len())
            .ok_or_else(|| error("arguments", "CSV row width must match header"))?;
        for field in row {
            let text = field.as_str().ok_or_else(|| {
                error(
                    "arguments",
                    "CSV cells must be strings; automatic conversion is forbidden",
                )
            })?;
            writer
                .write_field(text)
                .map_err(|e| error("output_limit", e.to_string()))?;
        }
        writer
            .write_record(None::<&[u8]>)
            .map_err(|e| error("output_limit", e.to_string()))?;
    }
    writer
        .into_inner()
        .map_err(|e| error("output_limit", e.to_string()))?
        .finish()
}
