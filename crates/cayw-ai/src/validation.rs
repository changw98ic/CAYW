use serde::{Deserialize, de};
use serde_json::{Map, Value};
use thiserror::Error;

#[derive(Debug, Clone, Error, PartialEq, Eq)]
#[error("{0}")]
pub struct ValidationError(pub String);

pub fn parse_tool_arguments(input: &str) -> Result<Value, ValidationError> {
    let value = if input.trim().is_empty() {
        serde_json::json!({})
    } else {
        serde_json::from_str::<UnambiguousValue>(input)
            .map_err(|error| ValidationError(format!("Invalid tool JSON: {error}")))?
            .0
    };
    if !value.is_object() {
        return Err(ValidationError(
            "Tool arguments must be a JSON object".into(),
        ));
    }
    Ok(value)
}

struct UnambiguousValue(Value);

impl<'de> Deserialize<'de> for UnambiguousValue {
    fn deserialize<D: de::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ValueVisitor;

        impl<'de> de::Visitor<'de> for ValueVisitor {
            type Value = UnambiguousValue;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("JSON without duplicate object fields")
            }

            fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(UnambiguousValue(Value::Null))
            }

            fn visit_bool<E: de::Error>(self, value: bool) -> Result<Self::Value, E> {
                Ok(UnambiguousValue(Value::Bool(value)))
            }

            fn visit_i64<E: de::Error>(self, value: i64) -> Result<Self::Value, E> {
                Ok(UnambiguousValue(value.into()))
            }

            fn visit_u64<E: de::Error>(self, value: u64) -> Result<Self::Value, E> {
                Ok(UnambiguousValue(value.into()))
            }

            fn visit_f64<E: de::Error>(self, value: f64) -> Result<Self::Value, E> {
                serde_json::Number::from_f64(value)
                    .map(|number| UnambiguousValue(Value::Number(number)))
                    .ok_or_else(|| E::custom("Invalid JSON number"))
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Ok(UnambiguousValue(Value::String(value.to_owned())))
            }

            fn visit_seq<A: de::SeqAccess<'de>>(
                self,
                mut sequence: A,
            ) -> Result<Self::Value, A::Error> {
                let mut values = Vec::new();
                while let Some(UnambiguousValue(value)) = sequence.next_element()? {
                    values.push(value);
                }
                Ok(UnambiguousValue(Value::Array(values)))
            }

            fn visit_map<A: de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut fields = Map::new();
                while let Some(key) = map.next_key::<String>()? {
                    if fields.contains_key(&key) {
                        return Err(de::Error::custom("duplicate JSON object field"));
                    }
                    let UnambiguousValue(value) = map.next_value()?;
                    fields.insert(key, value);
                }
                Ok(UnambiguousValue(Value::Object(fields)))
            }
        }

        deserializer.deserialize_any(ValueVisitor)
    }
}

/// Explicit conversion for tool-owned unsigned integer fields. Decimal strings
/// are parsed without floating-point rounding. Already parsed JSON floats must
/// be integral and within the safe integer range. Callers enforce their own bounds.
pub fn unsigned_tool_integer(value: &Value) -> Result<u64, ValidationError> {
    let invalid =
        || ValidationError("expected an exact nonnegative integer within u64 range".into());
    if let Some(number) = value.as_u64() {
        return Ok(number);
    }
    if let Some(number) = value.as_f64() {
        const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;
        if number.is_finite() && (0.0..=MAX_SAFE_INTEGER).contains(&number) && number.fract() == 0.0
        {
            return Ok(number as u64);
        }
        return Err(invalid());
    }
    let text = value.as_str().ok_or_else(invalid)?.trim();
    let (mantissa, exponent) = match text.split_once(['e', 'E']) {
        Some((mantissa, exponent)) => (mantissa, exponent.parse::<i64>().map_err(|_| invalid())?),
        None => (text, 0),
    };
    let (whole, fraction) = match mantissa.split_once('.') {
        Some((whole, fraction)) if !fraction.is_empty() => (whole, fraction),
        Some(_) => return Err(invalid()),
        None => (mantissa, ""),
    };
    if whole.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(invalid());
    }
    let mut digits = format!("{whole}{fraction}");
    if digits.bytes().all(|byte| byte == b'0') {
        return Ok(0);
    }
    let scale = exponent
        .checked_sub(i64::try_from(fraction.len()).map_err(|_| invalid())?)
        .ok_or_else(invalid)?;
    if scale < 0 {
        let removed = usize::try_from(scale.unsigned_abs()).map_err(|_| invalid())?;
        let keep = digits.len().checked_sub(removed).ok_or_else(invalid)?;
        if !digits.as_bytes()[keep..].iter().all(|byte| *byte == b'0') {
            return Err(invalid());
        }
        digits.truncate(keep);
    }
    let significant = digits.trim_start_matches('0');
    let zeros = usize::try_from(scale.max(0)).map_err(|_| invalid())?;
    if significant.len().saturating_add(zeros) > 20 {
        return Err(invalid());
    }
    let mut number = significant.parse::<u64>().map_err(|_| invalid())?;
    for _ in 0..zeros {
        number = number.checked_mul(10).ok_or_else(invalid)?;
    }
    Ok(number)
}

pub struct ArgumentValidator(jsonschema::Validator);

impl ArgumentValidator {
    pub fn new(schema: &Value) -> Result<Self, ValidationError> {
        reject_external_refs(schema)?;
        jsonschema::validator_for(schema)
            .map(Self)
            .map_err(|error| ValidationError(format!("Invalid tool schema: {error}")))
    }
    pub fn validate(&self, arguments: &Value) -> Result<(), ValidationError> {
        if !arguments.is_object() {
            return Err(ValidationError(
                "Tool arguments must be a JSON object".into(),
            ));
        }
        let errors = self
            .0
            .iter_errors(arguments)
            .map(|error| format_validation_error(&error))
            .collect::<Vec<_>>();
        if errors.is_empty() {
            Ok(())
        } else {
            Err(ValidationError(errors.join("; ")))
        }
    }
}

fn format_validation_error(error: &jsonschema::ValidationError<'_>) -> String {
    use jsonschema::error::ValidationErrorKind;
    match error.kind() {
        ValidationErrorKind::OneOfNotValid { context } | ValidationErrorKind::AnyOf { context }
            if !context.is_empty() =>
        {
            context
                .iter()
                .enumerate()
                .map(|(index, errors)| {
                    let details = errors
                        .iter()
                        .map(format_validation_error)
                        .collect::<Vec<_>>()
                        .join("; ");
                    format!(
                        "{} alternative {}: {details}",
                        error.kind().keyword(),
                        index + 1
                    )
                })
                .collect::<Vec<_>>()
                .join("; ")
        }
        _ => format!("{}: {error}", error.instance_path()),
    }
}

// Tool declarations cannot cause file reads or network access while compiling schemas.
fn reject_external_refs(value: &Value) -> Result<(), ValidationError> {
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                if matches!(key.as_str(), "$ref" | "$dynamicRef" | "$recursiveRef")
                    && let Some(reference) = value.as_str()
                    && !reference.starts_with('#')
                {
                    return Err(ValidationError(
                        "External schema references are not allowed".into(),
                    ));
                }
                reject_external_refs(value)?;
            }
        }
        Value::Array(array) => {
            for value in array {
                reject_external_refs(value)?;
            }
        }
        _ => {}
    }
    Ok(())
}
