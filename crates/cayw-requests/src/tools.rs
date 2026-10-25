use crate::{
    OperationCatalog, RequestError, RequestInput, RequestStore, Result, TaskIdentity, types::*,
};
use cayw_ai::{ArgumentValidator, CancellationToken, ToolDefinition};
use cayw_codemode::{CapabilityError, CapabilityHost};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

/// Tools bound to a host execution lease. Identity, actors, explicit user intent,
/// evaluation, and grant APIs are deliberately absent from model parameters.
pub struct RequestTools {
    store: Arc<RequestStore>,
    identity: TaskIdentity,
    catalog: OperationCatalog,
    definitions: Vec<ToolDefinition>,
    validators: Vec<ArgumentValidator>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StatusInput {
    request_id: String,
}
impl RequestTools {
    pub fn new(
        store: Arc<RequestStore>,
        identity: TaskIdentity,
        catalog: OperationCatalog,
    ) -> Result<Self> {
        store.check_task(&identity, &catalog)?;
        let definitions = Self::tool_definitions();
        let validators = definitions
            .iter()
            .map(|definition| {
                ArgumentValidator::new(&definition.parameters)
                    .map_err(|_| RequestError::InvalidInput("request tool schema failed".into()))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            store,
            identity,
            catalog,
            definitions,
            validators,
        })
    }
    /// Static request/status schemas for a runtime catalog. No task, store, or
    /// fabricated execution lease is needed to advertise these definitions.
    pub fn tool_definitions() -> Vec<ToolDefinition> {
        vec![
            ToolDefinition {
                name: "capability.request".into(),
                description: "Record a task-scoped capability gap for host evaluation. The first valid request is evaluable; repetition only changes the informational state. This never grants permission or runs work.".into(),
                parameters: json!({
                    "type":"object","additionalProperties":false,
                    "required":["purpose","gap","operation_id","capability","arguments"],
                    "properties":{
                        "purpose":{"type":"string","minLength":1,"maxLength":2048},
                        "gap":{"type":"string","minLength":1,"maxLength":2048},
                        "operation_id":{"type":"string","minLength":1,"maxLength":128,"pattern":"^[A-Za-z0-9_.:-]+$"},
                        "capability":{"type":"string","minLength":1,"maxLength":128,"pattern":"^[A-Za-z0-9_.:-]+$"},
                        "arguments":{"type":"object"}
                    }
                }),
            },
            ToolDefinition {
                name: "capability.status".into(),
                description: "Inspect one request owned by this task/session/attempt. This tool cannot approve, revoke, or execute a request.".into(),
                parameters: json!({
                    "type":"object","additionalProperties":false,"required":["request_id"],
                    "properties":{"request_id":{"type":"string","minLength":1,"maxLength":128,"pattern":"^[A-Za-z0-9_.:-]+$"}}
                }),
            },
        ]
    }
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.definitions.clone()
    }
    pub fn invoke(&self, name: &str, arguments: Value) -> Result<Value> {
        let index = self
            .definitions
            .iter()
            .position(|definition| definition.name == name)
            .ok_or_else(|| {
                RequestError::InvalidInput(
                    "only capability.request and capability.status are exposed".into(),
                )
            })?;
        argument_bounds(&arguments)?;
        bounded_json(&arguments, ARG_BYTES + 8192)?;
        self.validators[index].validate(&arguments).map_err(|_| {
            RequestError::InvalidInput(
                "request tool arguments do not match the strict schema".into(),
            )
        })?;
        let record = if index == 0 {
            let input: RequestInput = serde_json::from_value(arguments)?;
            self.store.request(&self.identity, &self.catalog, input)?
        } else {
            let input: StatusInput = serde_json::from_value(arguments)?;
            self.store.status(&self.identity, &input.request_id)?
        };
        bounded_json(&record, self.store.config().max_result_bytes)?;
        Ok(serde_json::to_value(record)?)
    }
}
#[async_trait::async_trait]
impl CapabilityHost for RequestTools {
    fn capabilities(&self) -> Vec<ToolDefinition> {
        self.definitions()
    }
    async fn call(
        &self,
        name: &str,
        arguments: Value,
        cancel: CancellationToken,
    ) -> std::result::Result<Value, CapabilityError> {
        if cancel.is_cancelled() {
            return Err(CapabilityError::new("request cancelled before admission"));
        }
        let result = self.invoke(name, arguments).map_err(|error| {
            let message = match error {
                RequestError::InvalidInput(reason) => format!("request input rejected: {reason}"),
                RequestError::Stale => {
                    "request belongs to a stale attempt or a terminal task".into()
                }
                RequestError::NotFound => "request was not found for this task".into(),
                RequestError::Denied(reason) => format!("request denied: {reason}"),
                RequestError::Budget => {
                    "request count or storage budget exhausted; ask the host to inspect the task"
                        .into()
                }
                RequestError::PolicyMismatch => {
                    "host catalog or permission policy changed; host must replace the attempt"
                        .into()
                }
                RequestError::Clock => "host request clock is outside the supported range".into(),
                _ => "request persistence is unavailable; no capability work was dispatched".into(),
            };
            CapabilityError::new(message)
        });
        if cancel.is_cancelled() {
            return Err(CapabilityError::new(
                "request cancelled; no capability work was dispatched",
            ));
        }
        result
    }
}
