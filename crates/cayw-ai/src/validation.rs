use serde_json::Value;
use thiserror::Error;

#[derive(Debug, Clone, Error, PartialEq, Eq)]
#[error("{0}")]
pub struct ValidationError(pub String);

pub fn parse_tool_arguments(input: &str) -> Result<Value, ValidationError> {
    let value = if input.trim().is_empty() {
        serde_json::json!({})
    } else {
        serde_json::from_str(input)
            .map_err(|error| ValidationError(format!("Invalid tool JSON: {error}")))?
    };
    if !value.is_object() {
        return Err(ValidationError(
            "Tool arguments must be a JSON object".into(),
        ));
    }
    Ok(value)
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
            .map(|error| format!("{}: {error}", error.instance_path()))
            .collect::<Vec<_>>();
        if errors.is_empty() {
            Ok(())
        } else {
            Err(ValidationError(errors.join("; ")))
        }
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
