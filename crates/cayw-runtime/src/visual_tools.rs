//! Session-bound model access to the existing artifact and review services.
use crate::{Runtime, cells::CallScope};
use async_trait::async_trait;
use cayw_ai::{CancellationToken, ToolDefinition};
use cayw_artifacts::ArtifactSummary;
use cayw_codemode::CapabilityError;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

pub(crate) fn definitions() -> Vec<ToolDefinition> {
    let empty = json!({"type":"object","additionalProperties":false,"properties":{}});
    vec![
        ToolDefinition { name:"artifacts.put".into(), description:"Create a native HTML artifact in this session. A workspace file alone is not a native artifact. This does not certify task completion.".into(), parameters:json!({"type":"object","additionalProperties":false,"required":["artifact_id","kind","title","body"],"properties":{"artifact_id":{"type":"string","minLength":1},"kind":{"enum":["html_card","chart","standalone_preview"]},"title":{"type":"string"},"body":{"type":"string"}}}) },
        ToolDefinition {
            name: "artifacts.update".into(),
            description: "Update an artifact in this session using the version you last read.".into(),
            parameters: json!({
                "type":"object","additionalProperties":false,
                "required":["artifact_id","expected_content_version"],
                "properties":{
                    "artifact_id":{"type":"string","minLength":1},
                    "expected_content_version":{"type":"integer","minimum":1},
                    "title":{"type":"string"},"body":{"type":"string"}
                }
            }),
        },
        ToolDefinition { name:"artifacts.list".into(), description:"List native artifacts and their current versions in this session.".into(), parameters:empty.clone() },
        ToolDefinition { name:"artifacts.preview".into(), description:"Read the HTML of an artifact in this session.".into(), parameters:json!({"type":"object","additionalProperties":false,"required":["artifact_id"],"properties":{"artifact_id":{"type":"string","minLength":1}}}) },
        ToolDefinition { name:"plannotator.list".into(), description:"Read version-bound host feedback for this session. Feedback is separate from operation approval and task success.".into(), parameters:empty.clone() },
        ToolDefinition {
            name: "session.run_inspector".into(),
            description: "Inspect execution outcomes and timing. Use after and limit to read a window, and next_after to continue.".into(),
            parameters: json!({"type":"object","additionalProperties":false,"properties":{
                "after":{"type":"integer","minimum":0},"limit":{"type":"integer","minimum":1}
            }}),
        },
    ]
}

pub(crate) fn is_tool(name: &str) -> bool {
    matches!(
        name,
        "artifacts.put"
            | "artifacts.update"
            | "artifacts.list"
            | "artifacts.preview"
            | "plannotator.list"
            | "session.run_inspector"
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Update {
    artifact_id: String,
    expected_content_version: u64,
    title: Option<String>,
    body: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Preview {
    artifact_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Inspect {
    #[serde(default)]
    after: u64,
    limit: Option<usize>,
}

fn timestamp() -> Result<u64, CapabilityError> {
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(error)?;
    u64::try_from(elapsed.as_millis()).map_err(error)
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
    fn delegates_workspace_calls(&self) -> bool {
        true
    }
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
                let record = runtime
                    .put_artifact(crate::PutArtifactRequest {
                        artifact_id: input.artifact_id,
                        session_id: session.clone(),
                        kind: input.kind,
                        title: input.title,
                        body: input.body,
                        at_ms: timestamp()?,
                        result_handle: None,
                    })
                    .map_err(error)?;
                serde_json::to_value(ArtifactSummary::from(record))
            }
            "artifacts.update" => {
                let input: Update = serde_json::from_value(arguments).map_err(error)?;
                let record = runtime
                    .update_artifact(crate::UpdateArtifactRequest {
                        artifact_id: input.artifact_id,
                        session_id: session.clone(),
                        title: input.title,
                        body: input.body,
                        at_ms: timestamp()?,
                        expected_content_version: Some(input.expected_content_version),
                    })
                    .map_err(error)?;
                serde_json::to_value(ArtifactSummary::from(record))
            }
            "artifacts.list" => {
                serde_json::to_value(runtime.summarize_artifacts(session).map_err(error)?)
            }
            "artifacts.preview" => {
                let input: Preview = serde_json::from_value(arguments).map_err(error)?;
                let preview = runtime
                    .artifact_store()
                    .preview_for_session(&input.artifact_id, session)
                    .map_err(error)?;
                serde_json::to_value(preview)
            }
            "plannotator.list" => {
                serde_json::to_value(runtime.plannotator_feedback(session).map_err(error)?)
            }
            "session.run_inspector" => {
                let page: Inspect = serde_json::from_value(arguments).map_err(error)?;
                serde_json::to_value(
                    runtime
                        .run_inspector_page(session, page.after, page.limit)
                        .map_err(error)?,
                )
            }
            _ => unreachable!("visual tool dispatch matched"),
        };
        value.map_err(error)
    }
}

fn error(error: impl std::fmt::Display) -> CapabilityError {
    CapabilityError::new(error.to_string())
}
