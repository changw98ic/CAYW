use crate::{CellError, CellExecution, CellTerminal, DependencySnapshot, discard_json};
use async_trait::async_trait;
use cayw_ai::{CancellationToken, ToolDefinition};
use cayw_codemode::CapabilityError;
use serde_json::Value;

/// A non-Clone, exactly-once handoff. Implementations perform all durable writes
/// before returning the response. Acknowledgment only publishes/releases owned
/// resources after Cells commits; Drop must retain uncertainty and freeze reuse.
pub trait BackendCompletion: Send + Sync {
    fn acknowledge(self: Box<Self>);
}

pub struct BackendResponse {
    result: Option<Result<Value, CapabilityError>>,
    presentation: Option<Result<Value, CapabilityError>>,
    dependencies: Option<DependencySnapshot>,
    completion: Option<Box<dyn BackendCompletion>>,
    publish_result: bool,
}
impl BackendResponse {
    pub fn new(result: Result<Value, CapabilityError>) -> Self {
        Self {
            result: Some(result),
            presentation: None,
            dependencies: None,
            completion: None,
            publish_result: false,
        }
    }
    pub fn owned(
        result: Result<Value, CapabilityError>,
        dependencies: Option<DependencySnapshot>,
        completion: Box<dyn BackendCompletion>,
    ) -> Self {
        Self {
            result: Some(result),
            presentation: None,
            dependencies,
            completion: Some(completion),
            publish_result: false,
        }
    }
    /// Publish the original successful result as cell output before Lua resumes.
    pub fn with_published_result(mut self) -> Self {
        self.publish_result = true;
        self
    }
    pub(crate) fn published_result(&self) -> Option<&Value> {
        if self.publish_result {
            self.result().as_ref().ok()
        } else {
            None
        }
    }
    pub fn result(&self) -> &Result<Value, CapabilityError> {
        self.result
            .as_ref()
            .expect("response result retained until acknowledgment")
    }
    pub fn dependencies(&self) -> Option<&DependencySnapshot> {
        self.dependencies.as_ref()
    }
    /// Preserve the original terminal for the journal while delivering a separate
    /// display value (or display/storage error) only after that terminal commits.
    pub fn with_presentation(mut self, result: Result<Value, CapabilityError>) -> Self {
        if let Some(Ok(value)) = self.presentation.replace(result) {
            discard_json(value);
        }
        self
    }
    pub fn acknowledge(mut self) -> Result<Value, CapabilityError> {
        if let Some(completion) = self.completion.take() {
            completion.acknowledge();
        }
        if let Some(result) = self.presentation.take() {
            return result;
        }
        self.result.take().expect("response acknowledged once")
    }
}
impl Drop for BackendResponse {
    fn drop(&mut self) {
        if let Some(Ok(value)) = self.result.take() {
            discard_json(value);
        }
        if let Some(Ok(value)) = self.presentation.take() {
            discard_json(value);
        }
    }
}

/// Every call receives the full immutable original origin after its Pending
/// intent commits. No cache or registered-read branch may bypass this boundary.
#[async_trait]
pub trait CellBackend: Send + Sync {
    fn capabilities(&self) -> Vec<ToolDefinition>;
    fn created(&self, execution: &CellExecution) -> Result<(), CellError>;
    /// A persisted cell is waiting for a VM execution permit.
    fn queued(&self, _execution: &CellExecution) -> Result<(), CellError> {
        Ok(())
    }
    /// Reacquire any enclosing task admission before starting this VM.
    async fn dispatched(
        &self,
        _execution: &CellExecution,
        _cancel: CancellationToken,
    ) -> Result<(), CellError> {
        Ok(())
    }
    async fn call(
        &self,
        execution: &CellExecution,
        sequence: u64,
        name: &str,
        arguments: Value,
        cancel: CancellationToken,
    ) -> Result<BackendResponse, CapabilityError>;
    fn finished(&self, execution: &CellExecution, terminal: &CellTerminal)
    -> Result<(), CellError>;
    /// Observes an already committed output. It cannot grant execution authority.
    fn output(
        &self,
        _execution: &CellExecution,
        _entry: &crate::OutputEntry,
    ) -> Result<(), CellError> {
        Ok(())
    }
}
