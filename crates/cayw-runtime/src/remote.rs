//! Optional outbound Device adapter; all execution stays in the common RPC API.
use crate::Runtime;
use async_trait::async_trait;
use cayw_ai::CancellationToken;
use cayw_ipc::RpcHandler;
use cayw_remote::{DeviceHandler, RemoteError};
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    version: u32,
    id: String,
    method: String,
    params: Value,
}

#[async_trait]
impl DeviceHandler for Runtime {
    async fn handle(
        &self,
        request: Value,
        cancel: CancellationToken,
    ) -> Result<Value, RemoteError> {
        let request: Request =
            serde_json::from_value(request).map_err(|_| RemoteError::InvalidRequest)?;
        if request.version != cayw_core::PROTOCOL_VERSION {
            return Err(RemoteError::IncompatibleVersion);
        }
        if request.id.trim().is_empty() || request.id.len() > 128 {
            return Err(RemoteError::InvalidRequest);
        }
        // RPC returns a structured result/error as data. Errors that can include
        // unknown effects also set the relay terminal to OutcomeUnknown.
        match RpcHandler::call(self, &request.method, request.params, cancel).await {
            Ok(result)
                if result.get("state").and_then(Value::as_str) == Some("needs_reconciliation") =>
            {
                Err(RemoteError::OutcomeUnknown)
            }
            Ok(result) => {
                Ok(serde_json::json!({"version":request.version,"id":request.id,"result":result}))
            }
            Err(error)
                if error.code == "needs_reconciliation"
                    || error.code.ends_with("outcome_unknown") =>
            {
                Err(RemoteError::OutcomeUnknown)
            }
            Err(error) => {
                Ok(serde_json::json!({"version":request.version,"id":request.id,"error":error}))
            }
        }
    }
}
