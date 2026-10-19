use crate::{
    budget::{Budget, Result},
    error, json,
};
use serde_json::{Map, Value};
use yaml_rust2::{
    parser::{Event, Parser},
    scanner::TScalarStyle,
};

pub(crate) fn parse(text: &str, budget: &mut Budget<'_>) -> Result<Value> {
    let mut parser = Yaml {
        parser: Parser::new_from_str(text),
        budget,
    };
    if parser.next()? != Event::StreamStart || parser.next()? != Event::DocumentStart {
        return Err(error("syntax", "expected one YAML document"));
    }
    let event = parser.next()?;
    let value = parser.value(event, 1)?;
    if parser.next()? != Event::DocumentEnd || parser.next()? != Event::StreamEnd {
        return Err(error(
            "unsupported",
            "exactly one YAML document is supported",
        ));
    }
    Ok(value)
}

struct Yaml<'a, 'b, 'c> {
    parser: Parser<std::str::Chars<'a>>,
    budget: &'b mut Budget<'c>,
}

impl Yaml<'_, '_, '_> {
    fn next(&mut self) -> Result<Event> {
        self.budget.checkpoint()?;
        self.parser
            .next_token()
            .map(|(event, _)| event)
            .map_err(|e| error("syntax", e.to_string()))
    }
    fn value(&mut self, event: Event, depth: usize) -> Result<Value> {
        self.budget.node(depth)?;
        match event {
            Event::Scalar(text, style, 0, None) => scalar(text, style),
            Event::SequenceStart(0, None) => {
                let mut array = Vec::new();
                loop {
                    let event = self.next()?;
                    if event == Event::SequenceEnd {
                        break;
                    }
                    self.budget.rows(array.len() + 1)?;
                    array.push(self.value(event, depth + 1)?);
                }
                Ok(Value::Array(array))
            }
            Event::MappingStart(0, None) => {
                let mut object = Map::new();
                loop {
                    let event = self.next()?;
                    if event == Event::MappingEnd {
                        break;
                    }
                    // Only the unquoted plain scalar has merge semantics.
                    // Quoted (including escaped) spellings remain ordinary keys.
                    let merge = matches!(
                        &event,
                        Event::Scalar(text, TScalarStyle::Plain, 0, None) if text == "<<"
                    );
                    let Value::String(key) = self.value(event, depth + 1)? else {
                        return Err(error("unsupported", "YAML mapping keys must be strings"));
                    };
                    if merge {
                        return Err(error("unsupported", "YAML merge keys are unsupported"));
                    }
                    if object.contains_key(&key) {
                        return Err(error("syntax", "duplicate YAML mapping key"));
                    }
                    let event = self.next()?;
                    object.insert(key, self.value(event, depth + 1)?);
                }
                Ok(Value::Object(object))
            }
            Event::Alias(_)
            | Event::Scalar(_, _, _, _)
            | Event::SequenceStart(_, _)
            | Event::MappingStart(_, _) => Err(error(
                "unsupported",
                "YAML anchors, aliases and explicit tags are unsupported",
            )),
            _ => Err(error("syntax", "unexpected YAML event")),
        }
    }
}

fn scalar(text: String, style: TScalarStyle) -> Result<Value> {
    if style != TScalarStyle::Plain {
        return Ok(Value::String(text));
    }
    match text.as_str() {
        "" | "~" | "null" | "Null" | "NULL" => return Ok(Value::Null),
        "true" | "True" | "TRUE" => return Ok(Value::Bool(true)),
        "false" | "False" | "FALSE" => return Ok(Value::Bool(false)),
        _ => {}
    }
    let lower = text.to_ascii_lowercase();
    if matches!(lower.as_str(), ".nan" | ".inf" | "+.inf" | "-.inf") {
        return Err(error(
            "unsupported",
            "YAML nonfinite numbers cannot be represented",
        ));
    }
    // Numeric-looking unsupported forms must be quoted, rather than silently becoming strings.
    let numeric = text.as_bytes().first().is_some_and(|b| b.is_ascii_digit())
        || text
            .strip_prefix(['+', '-'])
            .is_some_and(|s| s.starts_with(|c: char| c.is_ascii_digit() || c == '.'))
        || text
            .starts_with('.')
            .then(|| &text[1..])
            .is_some_and(|s| s.starts_with(|c: char| c.is_ascii_digit()));
    if numeric {
        return json::number(&text).map(Value::Number).map_err(|_| error("unsupported", "YAML numbers must use finite, exactly representable JSON number syntax; quote other scalars"));
    }
    Ok(Value::String(text))
}

// JSON flow syntax is valid YAML 1.2 and quotes every string/key unambiguously.
pub(crate) fn serialize(value: &Value, budget: &Budget<'_>) -> Result<String> {
    json::serialize(value, budget)
}
