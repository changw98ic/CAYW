//! Trusted Runtime dispatch for cell value capabilities. The model can choose a
//! key and value, but never the session. Every store/load traverses the original
//! Host permission, start, budget and settlement path before touching the Cell
//! journal.
use crate::cells::CallScope;
use async_trait::async_trait;
use cayw_ai::{CancellationToken, ToolDefinition};
use cayw_cells::{CellExecution, CellValueExecutor, CellValueOperation};
use cayw_codemode::CapabilityError;
use cayw_host::ExternalHost;
use serde_json::Value;
use std::sync::Arc;

/// Whether this trusted Runtime path owns the capability name. The Cells
/// manager uses the same canonical namespace, so the model cannot invent an
/// alias or supply a session override.
pub(crate) fn is_value(name: &str) -> bool {
    CellValueOperation::from_name(name).is_some()
}

/// The only Runtime entry for canonical `cells.store` and `cells.load`.
/// `scope` supplies the exact planned Core key and immutable arguments;
/// `execution` supplies the trusted session/task origin. No session id is read
/// from model arguments or the Luau VM.
pub(crate) async fn invoke(
    scope: &Arc<CallScope>,
    base: &cayw_host::Host,
    execution: &CellExecution,
    arguments: Value,
    cancel: CancellationToken,
) -> Result<Value, CapabilityError> {
    let operation = CellValueOperation::from_name(&scope.name)
        .ok_or_else(|| CapabilityError::new("not a cell value capability"))?;
    verify_origin(scope, execution)?;
    let definition = definition(&scope.name)?;
    let executor = CellValueExecutor::new(
        scope.context.inner.cells.clone(),
        scope.context.inner.config.cells.clone(),
    );
    let service = Service {
        executor: &executor,
        execution,
        capability: &scope.name,
        operation,
    };
    // Use a scoped Host exactly like an ordinary capability. Its permission gate
    // is the Runtime Gate for this fixed CallScope, and its audit sink is the
    // CallScope itself, so Host start/settled and Core planned/started/settled
    // keep the original key pairing.
    let host = crate::bridge::scoped_host(base, scope)?;
    let child = cancel.child_token();
    let _cancel_on_drop = child.clone().drop_guard();
    let run = host.call_service(&definition, arguments, child.clone(), &service);
    // The same authority fence used by ordinary capabilities stops this call if
    // its exact grant, lease or binding is revoked while it is waiting.
    crate::requests::watch(scope, child, run).await
}

fn verify_origin(scope: &Arc<CallScope>, execution: &CellExecution) -> Result<(), CapabilityError> {
    if execution.identity != crate::cells::identity(&scope.context) {
        return Err(CapabilityError::new(
            "cell value origin does not match the active task lease",
        ));
    }
    if execution.cell_id != scope.key.execution_id {
        return Err(CapabilityError::new(
            "cell value execution does not match its planned capability key",
        ));
    }
    Ok(())
}

fn definition(name: &str) -> Result<ToolDefinition, CapabilityError> {
    crate::values::definitions()
        .into_iter()
        .find(|definition| definition.name == name)
        .ok_or_else(|| CapabilityError::new("not a cell value capability"))
}

struct Service<'a> {
    executor: &'a CellValueExecutor,
    execution: &'a CellExecution,
    capability: &'a str,
    operation: CellValueOperation,
}

#[async_trait]
impl ExternalHost for Service<'_> {
    async fn call(
        &self,
        capability: &str,
        arguments: Value,
        cancel: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        if capability != self.capability {
            return Err(CapabilityError::new(
                "cell value service invoked for a different capability",
            ));
        }
        if CellValueOperation::from_name(capability) != Some(self.operation) {
            return Err(CapabilityError::new("not a cell value capability"));
        }
        if cancel.is_cancelled() {
            return Err(CapabilityError::new(
                "cell value operation cancelled before execution",
            ));
        }
        self.executor.invoke(self.execution, capability, arguments)
    }
}

/// Canonical definitions for the Runtime backend catalog. The Cells manager
/// also merges these definitions into every Bridge catalog, so a host cannot
/// replace or weaken the schemas.
pub(crate) fn definitions() -> Vec<ToolDefinition> {
    cayw_cells::values::definitions()
}
