//! Session-bound model access to the existing artifact and review services.
use crate::{Runtime, cells::CallScope};
use async_trait::async_trait;
use cayw_ai::{CancellationToken, ToolDefinition};
use cayw_codemode::CapabilityError;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

pub(crate) fn definitions() -> Vec<ToolDefinition> {
    let empty = json!({"type":"object","additionalProperties":false,"properties":{}});
    vec![
        ToolDefinition { name:"artifacts.put".into(), description:"Create a native HTML artifact in this session. A workspace file alone is not a native artifact. This does not certify task completion.".into(), parameters:json!({"type":"object","additionalProperties":false,"required":["artifact_id","kind","title","body"],"properties":{"artifact_id":{"type":"string","minLength":1},"kind":{"enum":["html_card","chart","standalone_preview"]},"title":{"type":"string"},"body":{"type":"string"}}}) },
        ToolDefinition { name:"artifacts.list".into(), description:"Read native artifacts belonging to this session.".into(), parameters:empty.clone() },
        ToolDefinition { name:"plannotator.list".into(), description:"Read version-bound host feedback for this session. Feedback is separate from operation approval and task success.".into(), parameters:empty.clone() },
        ToolDefinition { name:"session.run_inspector".into(), description:"Inspect this session's recorded execution outcomes and timing. A complete record does not imply successful task acceptance.".into(), parameters:empty },
    ]
}

pub(crate) fn is_tool(name: &str) -> bool {
    matches!(
        name,
        "artifacts.put" | "artifacts.list" | "plannotator.list" | "session.run_inspector"
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Put {
    artifact_id: String,
    kind: crate::ArtifactKind,
    title: String,
    body: String,
}

pub(crate) async fn invoke(
    scope: &Arc<CallScope>,
    base: &cayw_host::Host,
    arguments: Value,
    cancel: CancellationToken,
) -> Result<Value, CapabilityError> {
    let definition = definitions()
        .into_iter()
        .find(|d| d.name == scope.name)
        .expect("visual tool dispatch matched");
    crate::bridge::scoped_host(base, scope)?
        .call_service(&definition, arguments, cancel, &Service(scope))
        .await
}

struct Service<'a>(&'a Arc<CallScope>);
#[async_trait]
impl cayw_host::ExternalHost for Service<'_> {
    async fn call(
        &self,
        name: &str,
        arguments: Value,
        cancel: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        self.0.check_authority(&cancel)?;
        let runtime = Runtime {
            inner: self.0.context.inner.clone(),
        };
        let session = &self.0.context.lease.session_id;
        let value = match name {
            "artifacts.put" => {
                let input: Put = serde_json::from_value(arguments).map_err(error)?;
                let at_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_err(error)?
                    .as_millis();
                let record = runtime
                    .put_artifact(crate::PutArtifactRequest {
                        artifact_id: input.artifact_id,
                        session_id: session.clone(),
                        kind: input.kind,
                        title: input.title,
                        body: input.body,
                        at_ms: u64::try_from(at_ms).map_err(error)?,
                        result_handle: None,
                    })
                    .map_err(error)?;
                serde_json::to_value(record)
            }
            "artifacts.list" => {
                serde_json::to_value(runtime.list_artifacts(session).map_err(error)?)
            }
            "plannotator.list" => {
                serde_json::to_value(runtime.plannotator_feedback(session).map_err(error)?)
            }
            "session.run_inspector" => {
                serde_json::to_value(runtime.run_inspector(session).map_err(error)?)
            }
            _ => unreachable!("visual tool dispatch matched"),
        };
        value.map_err(error)
    }
}

fn error(error: impl std::fmt::Display) -> CapabilityError {
    CapabilityError::new(error.to_string())
}
