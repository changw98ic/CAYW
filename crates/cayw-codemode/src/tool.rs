use crate::{CapabilityStop, CodeMode};
use async_trait::async_trait;
use cayw_agent::{AgentTool, ExecutionMode, ToolContext, ToolResult};
use cayw_ai::ToolDefinition;
use serde_json::json;
use std::time::Duration;

/// Register this tool alone to expose a single orchestration entry to a model.
/// Capability discovery is performed inside Luau, rather than adding model tools.
pub struct CodeModeTool {
    code: CodeMode,
}
impl CodeModeTool {
    pub fn new(code: CodeMode) -> Self {
        Self { code }
    }
}

#[async_trait]
impl AgentTool for CodeModeTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "execute_code".into(),
            description: "Execute bounded Luau. Use cayw.discover() for capability names, cayw.describe(name) for a schema, cayw.call(name, arguments) to await an authorized host capability, and cayw.emit(value) for JSON output. Use cayw.null and cayw.array() for JSON null and an empty array. No filesystem, process, shell or network APIs are available in the VM. Every capability remains subject to host permission checks.".into(),
            parameters: json!({"type":"object","properties":{"script":{"type":"string"}},"required":["script"],"additionalProperties":false}),
        }
    }
    fn execution_mode(&self) -> Option<ExecutionMode> {
        Some(ExecutionMode::Sequential)
    }
    /// Preserve time for the inner Host grace and VM/report settlement. The
    /// constructor has validated this sum; callers configure only Limits.
    fn shutdown_grace(&self) -> Duration {
        self.code.limits.shutdown_grace + Duration::from_secs(1)
    }
    async fn execute(&self, context: ToolContext) -> Result<ToolResult, String> {
        let script = match context.arguments.as_object() {
            Some(arguments) if arguments.len() == 1 => {
                arguments.get("script").and_then(|script| script.as_str())
            }
            _ => None,
        };
        let Some(script) = script else {
            return Ok(ToolResult::error(
                "execute_code requires exactly one string field: script",
            ));
        };
        let (report, error, stop) = match self.code.execute(script, context.cancel).await {
            Ok(report) => (report, None, None),
            Err(error) => (
                *error.report,
                Some(json!({"kind":error.kind,"message":error.message})),
                error.kind.stop(),
            ),
        };
        let is_error = error.is_some();
        let outcome_unknown = report.outcome_unknown;
        let content = json!({"report":report,"error":error});
        let text = serde_json::to_string(&content).map_err(|error| error.to_string())?;
        Ok(ToolResult {
            is_error,
            outcome_unknown,
            terminate: outcome_unknown || stop.is_some_and(|stop| stop != CapabilityStop::Exited),
            structured_content: Some(content),
            ..ToolResult::text(text)
        })
    }
}
