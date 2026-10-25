use crate::*;
use async_trait::async_trait;
use cayw_agent::{AgentTool, ExecutionMode, ToolContext, ToolResult};
use cayw_ai::ToolDefinition;
use cayw_codemode::CapabilityStop;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

/// The single model-visible entry. Runtime must register this instead of
/// `cayw_codemode::CodeModeTool`; registering both is an integration error.
/// Session/task/attempt are fixed by the manager, never model-controlled.
pub struct CellTool {
    manager: Arc<CellManager>,
}
impl CellTool {
    pub fn new(manager: Arc<CellManager>) -> Self {
        Self { manager }
    }
    pub fn manager(&self) -> &Arc<CellManager> {
        &self.manager
    }
}
#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum Action {
    Exec {
        script: String,
        yield_time_ms: u64,
        max_output_tokens: usize,
    },
    Wait {
        cell_id: String,
        cursor: u64,
        yield_time_ms: u64,
        max_output_tokens: usize,
        #[serde(default = "continue_default")]
        continue_execution: bool,
    },
}
fn continue_default() -> bool {
    true
}
#[async_trait]
impl AgentTool for CellTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "execute_code".into(),
            description: "Execute or wait for one bounded Luau cell. Arguments are a JSON object. cursor, yield_time_ms and max_output_tokens use JSON integers; exact nonnegative decimal strings (including integral decimal or exponent forms) are converted before the same range checks. Example exec arguments: {\"action\":\"exec\",\"script\":\"return 1 + 1\",\"yield_time_ms\":0,\"max_output_tokens\":1024}. exec creates a globally unique cell_id; wait reads incremental output after cursor without replay. yield_time_ms returns early without cancelling the managed VM. wait resumes yield_control unless continue_execution=false. Helpers inside the Luau script: store(key,value,expected_version), load(key), text(string), image(mime,base64), audio(mime,base64), notify(message,completed,total), exit(code), yield_control(), toolDefinition(name), tool_search(query,limit). ALL_TOOLS is a read-only array of tool names. toolDefinition and tool_search are Luau functions, not external tool names. Use return or cayw.emit(value) for JSON values, including tables. pcall/xpcall handle ordinary known errors; cancellation, exhausted budgets, unknown effects and exit still stop the cell. Pure computation coroutines and per-cell globals are available. Await host tools from the main script. print, io, os, package, require and math.tointeger are unavailable. Use ordinary Luau operators and bounded string/table/math/utf8 functions. Invoke host tools with cayw.call(name,args); each invocation is authorized. Do not register any other execute_code tool. max_output_tokens is a conservative serialized UTF-8 BYTE budget (minimum 1024), not provider token usage. A page too small for the next item returns required_budget with unchanged cursor. Restart recovers durable records, never a Lua stack. No automatic replay.".into(),
            parameters: json!({"type":"object","oneOf":[
                {"type":"object","properties":{"action":{"const":"exec"},"script":{"type":"string","maxLength":self.manager.limits().script_bytes},"yield_time_ms":{"type":"integer","minimum":0,"maximum":self.manager.limits().max_yield_time.as_millis() as u64},"max_output_tokens":{"type":"integer","minimum":1024,"maximum":self.manager.limits().max_output_tokens}},"required":["action","script","yield_time_ms","max_output_tokens"],"additionalProperties":false},
                {"type":"object","properties":{"action":{"const":"wait"},"cell_id":{"type":"string","minLength":1,"maxLength":128},"cursor":{"type":"integer","minimum":0},"yield_time_ms":{"type":"integer","minimum":0,"maximum":self.manager.limits().max_yield_time.as_millis() as u64},"max_output_tokens":{"type":"integer","minimum":1024,"maximum":self.manager.limits().max_output_tokens},"continue_execution":{"type":"boolean"}},"required":["action","cell_id","cursor","yield_time_ms","max_output_tokens"],"additionalProperties":false}
            ]}),
        }
    }
    fn prepare_arguments(
        &self,
        mut arguments: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let fields: &[&str] = match arguments.get("action").and_then(serde_json::Value::as_str) {
            Some("exec") => &["yield_time_ms", "max_output_tokens"],
            Some("wait") => &["cursor", "yield_time_ms", "max_output_tokens"],
            _ => return Ok(arguments),
        };
        for field in fields {
            if let Some(value) = arguments.get_mut(*field)
                && (value.is_string() || value.is_f64())
            {
                let number = cayw_ai::unsigned_tool_integer(value)
                    .map_err(|error| format!("/{field}: {error}"))?;
                *value = number.into();
            }
        }
        Ok(arguments)
    }
    fn execution_mode(&self) -> Option<ExecutionMode> {
        Some(ExecutionMode::Sequential)
    }
    fn shutdown_grace(&self) -> std::time::Duration {
        self.manager.shutdown_grace() + std::time::Duration::from_secs(2)
    }
    async fn execute(&self, context: ToolContext) -> Result<ToolResult, String> {
        let action = match serde_json::from_value::<Action>(context.arguments) {
            Ok(action) => action,
            Err(error) => return Ok(ToolResult::error(bounded_error(&error.to_string()))),
        };
        let page = match action {
            Action::Exec {
                script,
                yield_time_ms,
                max_output_tokens,
            } => {
                self.manager
                    .exec(
                        ExecRequest {
                            script,
                            yield_time_ms,
                            max_output_tokens,
                        },
                        &context.call_id,
                        context.cancel,
                    )
                    .await
            }
            Action::Wait {
                cell_id,
                cursor,
                yield_time_ms,
                max_output_tokens,
                continue_execution,
            } => {
                self.manager
                    .wait(
                        WaitRequest {
                            cell_id,
                            cursor,
                            yield_time_ms,
                            max_output_tokens,
                            continue_execution,
                        },
                        context.cancel,
                    )
                    .await
            }
        };
        match page {
            Ok(page) => {
                let outcome_unknown = page.outcome_unknown;
                let terminate = outcome_unknown
                    || page.status == CellStatus::Cancelled
                    || page.stop.is_some_and(|stop| stop != CapabilityStop::Exited);
                let is_error = outcome_unknown
                    || matches!(
                        page.status,
                        CellStatus::Failed | CellStatus::Cancelled | CellStatus::Interrupted
                    );
                // Images/audio remain typed in structured_content. In particular
                // cayw-ai has no audio content variant; never pretend audio was
                // delivered to a provider as native audio.
                let structured = serde_json::to_value(&page).map_err(|e| e.to_string())?;
                let text = serde_json::to_string(&page).map_err(|e| e.to_string())?;
                let result = ToolResult {
                    is_error,
                    terminate,
                    outcome_unknown,
                    structured_content: Some(structured),
                    ..ToolResult::text(text)
                };
                Ok(result)
            }
            Err(error) => {
                let frozen = matches!(error, CellError::Frozen);
                Ok(ToolResult {
                    outcome_unknown: frozen,
                    terminate: frozen,
                    ..ToolResult::error(bounded_error(&error.to_string()))
                })
            }
        }
    }
}
