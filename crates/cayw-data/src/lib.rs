//! Bounded pure data capabilities for the host's existing permission/audit gate.
//!
//! This crate registers no Agent tools, opens no resources, and owns no permissions.
//! Call [`definitions`] from the host catalog and [`execute`] after the host gate.

mod budget;
mod catalog;
mod codec;
mod csv;
mod diff;
mod json;
mod query;
mod toml;
mod xml;
mod yaml;

use budget::{Budget, Result, check_envelope, check_value};
use cayw_ai::{CancellationToken, ToolDefinition};
use cayw_codemode::CapabilityError;
use serde_json::{Value, json};
use std::{future::Future, sync::OnceLock};

/// Host-selected optional limits. `None` means this crate imposes no artificial
/// bound for that dimension. Depth counts the logical root as one; object keys
/// count as nodes. Rows limit each array/CSV table/XML child list/diff side
/// separately.
#[derive(Debug, Clone, Copy, Default)]
pub struct Limits {
    /// Complete JSON-encoded argument envelope, including all explicit text.
    pub input_bytes: Option<usize>,
    /// Complete JSON-encoded result envelope, including escaping and metadata.
    pub output_bytes: Option<usize>,
    pub max_depth: Option<usize>,
    pub max_nodes: Option<usize>,
    pub max_rows: Option<usize>,
    /// Maximum matrix cells plus charged byte comparisons for line diff.
    pub max_diff_work: Option<usize>,
}

impl Limits {
    /// Checks the configured budgets without executing a capability. Only an
    /// explicitly configured limit is validated: every `Some` value must be
    /// positive. No artificial upper ceiling is applied.
    pub fn validate(self) -> std::result::Result<(), CapabilityError> {
        let values = [
            self.input_bytes,
            self.output_bytes,
            self.max_depth,
            self.max_nodes,
            self.max_rows,
            self.max_diff_work,
        ];
        if values.into_iter().flatten().any(|value| value == 0) {
            return Err(error("invalid_limits", "configured limits must be nonzero"));
        }
        Ok(())
    }
}

/// The only canonical capability names; schemas are strict and local-only.
pub fn definitions() -> Vec<ToolDefinition> {
    catalog::definitions()
}

struct Entry {
    name: String,
    validator: jsonschema::Validator,
}
static CATALOG: OnceLock<std::result::Result<Vec<Entry>, String>> = OnceLock::new();

fn validator(name: &str) -> Result<&'static jsonschema::Validator> {
    let catalog = CATALOG
        .get_or_init(|| {
            definitions()
                .into_iter()
                .map(|d| {
                    jsonschema::validator_for(&d.parameters)
                        .map(|validator| Entry {
                            name: d.name,
                            validator,
                        })
                        .map_err(|e| e.to_string())
                })
                .collect()
        })
        .as_ref()
        .map_err(|e| error("catalog", e))?;
    catalog
        .iter()
        .find(|e| e.name == name)
        .map(|e| &e.validator)
        .ok_or_else(|| error("unknown_capability", "unknown data capability"))
}

/// Validate arguments before dispatch. All failures are known, side-effect-free
/// failures (`outcome_unknown == false`). Work uses a bounded blocking worker so
/// the executor can continue delivering cancellation while CPU parsing runs.
/// Arguments gain iterative disposal before the future is returned, including
/// when it is never polled. No worker starts until polling; dropping a polled
/// future cancels that worker at its next checkpoint.
pub fn execute(
    name: &str,
    args: Value,
    cancel: CancellationToken,
    limits: Limits,
) -> impl Future<Output = std::result::Result<Value, CapabilityError>> + Send {
    let arguments = Arguments(args);
    let name = name.to_owned();
    async move {
        limits.validate()?;
        let token = cancel.child_token();
        let guard = token.clone().drop_guard();
        if token.is_cancelled() {
            return Err(error("cancelled", "data operation cancelled"));
        }
        let result = tokio::task::spawn_blocking(move || {
            let result = (|| {
                let args = &arguments.0;
                check_envelope(args, limits, &token, false)?;
                if !validator(&name)?.is_valid(args) {
                    return Err(error(
                        "arguments",
                        "arguments do not match the capability schema",
                    ));
                }
                let mut budget = Budget::new(limits, &token);
                budget.checkpoint()?;
                let result = dispatch(&name, args, &mut budget)?;
                budget.checkpoint()?;
                check_envelope(&result, limits, &token, true)?;
                Ok(result)
            })();
            if token.is_cancelled() {
                Err(error("cancelled", "data operation cancelled"))
            } else {
                result
            }
        })
        .await
        .map_err(|_| error("worker", "data worker failed"))?;
        drop(guard);
        result
    }
}

struct Arguments(Value);
impl Drop for Arguments {
    fn drop(&mut self) {
        let mut pending = vec![std::mem::take(&mut self.0)];
        while let Some(value) = pending.pop() {
            match value {
                Value::Array(values) => pending.extend(values),
                Value::Object(values) => pending.extend(values.into_values()),
                _ => {}
            }
        }
    }
}

fn dispatch(name: &str, args: &Value, budget: &mut Budget<'_>) -> Result<Value> {
    match name {
        "data.parse" => {
            let text = args["text"].as_str().expect("validated text");
            let value = match args["format"].as_str() {
                Some("json") => json::parse(text, budget)?,
                Some("toml") => toml::parse(text, budget)?,
                Some("yaml") => yaml::parse(text, budget)?,
                Some("xml") => xml::parse(text, budget)?,
                Some("csv") => csv::parse(text, budget)?,
                _ => return Err(error("arguments", "unsupported format")),
            };
            check_value(&value, &mut Budget::new(budget.limits, budget.cancel), 1)?;
            Ok(json!({"value":value}))
        }
        "data.serialize" => {
            check_value(&args["value"], budget, 1)?;
            let text = match args["format"].as_str() {
                Some("json") => json::serialize(&args["value"], budget)?,
                Some("toml") => toml::serialize(&args["value"], budget)?,
                Some("yaml") => yaml::serialize(&args["value"], budget)?,
                Some("xml") => xml::serialize(&args["value"], budget)?,
                Some("csv") => csv::serialize(&args["value"], budget)?,
                _ => return Err(error("arguments", "unsupported format")),
            };
            Ok(json!({"text":text}))
        }
        "data.query" => query::query(args, budget),
        "data.transform" => query::transform(args, budget),
        "text.diff" => diff::diff(args, budget),
        "hash.digest" => codec::digest(args, budget),
        "codec.encode" => codec::encode(args, budget),
        "codec.decode" => codec::decode_result(args, budget),
        _ => Err(error("unknown_capability", "unknown data capability")),
    }
}

pub(crate) fn error(kind: &str, message: impl std::fmt::Display) -> CapabilityError {
    CapabilityError::new(format!("{kind}: {message}"))
}
