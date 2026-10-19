use crate::{
    budget::{Budget, Result, check_value},
    error,
};
use serde_json::{Map, Value, json};
use std::collections::BTreeSet;

fn pointer<'a>(mut value: &'a Value, pointer: &str, budget: &Budget<'_>) -> Result<&'a Value> {
    if pointer.is_empty() {
        return Ok(value);
    }
    if !pointer.starts_with('/') {
        return Err(error(
            "selection",
            "JSON pointer must be empty or start with /",
        ));
    }
    for component in pointer[1..].split('/') {
        budget.checkpoint()?;
        let mut key = String::new();
        let mut chars = component.chars();
        while let Some(ch) = chars.next() {
            if ch == '~' {
                key.push(match chars.next() {
                    Some('0') => '~',
                    Some('1') => '/',
                    _ => return Err(error("selection", "invalid JSON pointer escape")),
                });
            } else {
                key.push(ch);
            }
        }
        value = match value {
            Value::Object(object) => object.get(&key),
            Value::Array(array) => {
                if key.is_empty()
                    || (key.len() > 1 && key.starts_with('0'))
                    || !key.bytes().all(|b| b.is_ascii_digit())
                {
                    return Err(error(
                        "selection",
                        "array indices must be canonical nonnegative decimal integers",
                    ));
                }
                key.parse::<usize>().ok().and_then(|i| array.get(i))
            }
            _ => None,
        }
        .ok_or_else(|| error("selection", "JSON pointer does not exist"))?;
    }
    Ok(value)
}

pub(crate) fn query(args: &Value, budget: &mut Budget<'_>) -> Result<Value> {
    check_value(&args["value"], budget, 1)?;
    let value = pointer(
        &args["value"],
        args["pointer"].as_str().unwrap_or_default(),
        budget,
    )?;
    Ok(json!({"value":value}))
}

pub(crate) fn transform(args: &Value, budget: &mut Budget<'_>) -> Result<Value> {
    check_value(&args["value"], budget, 1)?;
    let value = pointer(
        &args["value"],
        args.get("pointer")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        budget,
    )?;
    let mut seen = BTreeSet::new();
    let value = match args["operation"].as_str() {
        Some("select_fields") => {
            let object = value
                .as_object()
                .ok_or_else(|| error("selection", "select_fields requires an object"))?;
            let mut selected = Map::new();
            for field in args["fields"].as_array().expect("validated fields") {
                budget.checkpoint()?;
                let field = field.as_str().expect("validated field string");
                if !seen.insert(field.to_owned()) {
                    return Err(error("arguments", "field selection must be unique"));
                }
                let value = object
                    .get(field)
                    .ok_or_else(|| error("selection", "selected field does not exist"))?;
                selected.insert(field.to_owned(), value.clone());
            }
            Value::Object(selected)
        }
        Some("select_rows") => {
            let rows = value
                .as_array()
                .ok_or_else(|| error("selection", "select_rows requires an array"))?;
            let mut selected = Vec::new();
            let mut indices = BTreeSet::new();
            for index in args["indices"].as_array().expect("validated indices") {
                budget.checkpoint()?;
                let index = index
                    .as_u64()
                    .and_then(|i| usize::try_from(i).ok())
                    .ok_or_else(|| {
                        error("arguments", "row indices must be unsigned integer tokens")
                    })?;
                if !indices.insert(index) {
                    return Err(error("arguments", "row selection must be unique"));
                }
                selected.push(
                    rows.get(index)
                        .ok_or_else(|| error("selection", "selected row does not exist"))?
                        .clone(),
                );
            }
            Value::Array(selected)
        }
        _ => return Err(error("arguments", "unsupported transform operation")),
    };
    Ok(json!({"value":value}))
}
