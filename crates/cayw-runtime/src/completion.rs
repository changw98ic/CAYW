//! Completion depends on actual VM state and output delivered by the tool.
use crate::{RuntimeError, execution::RunContext, lock};
use cayw_ai::ToolResultMessage;
use cayw_cells::{CellManager, CellOutput, CellPage, CellStatus};
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

#[derive(Default)]
struct Delivery {
    cursors: BTreeSet<u64>,
    vm_alive: bool,
}

pub(crate) struct Completion {
    context: Arc<RunContext>,
    manager: Arc<CellManager>,
    delivered: Mutex<BTreeMap<String, Delivery>>,
}
impl Completion {
    pub(crate) fn new(context: Arc<RunContext>, manager: Arc<CellManager>) -> Self {
        Self {
            context,
            manager,
            delivered: Mutex::default(),
        }
    }

    /// Called only after the genuine tool result has committed to Core history.
    pub(crate) fn observe(&self, result: &ToolResultMessage) -> Result<(), RuntimeError> {
        if result.tool_name != "execute_code" {
            return Ok(());
        }
        let Some(value) = &result.structured_content else {
            return Ok(());
        };
        let page: CellPage = serde_json::from_value(value.clone())?;
        let record = self
            .manager
            .record(&page.cell_id)
            .map_err(|error| RuntimeError::Worker(error.to_string()))?;
        if record.identity != crate::cells::identity(&self.context)
            || page.cursor > page.next_cursor
            || page.next_cursor > record.outputs.len() as u64
            || page.outputs.len() as u64 != page.next_cursor - page.cursor
            || page.outputs.iter().enumerate().any(|(offset, entry)| {
                entry.cursor != page.cursor + offset as u64 + 1
                    || record.outputs.get(entry.cursor as usize - 1) != Some(entry)
            })
        {
            return Err(RuntimeError::Worker(
                "cell delivery differs from its original output".into(),
            ));
        }
        self.context
            .host_event(
                "cell_output_delivered",
                json!({
                    "task_id": self.context.task.id,
                    "cell_id": page.cell_id,
                    "tool_call_id": result.tool_call_id,
                    "cursor": page.cursor,
                    "next_cursor": page.next_cursor,
                    "vm_alive": page.vm_alive,
                }),
            )
            .map_err(|error| RuntimeError::Worker(error.to_string()))?;
        let mut delivered = lock(&self.delivered);
        let delivery = delivered.entry(page.cell_id).or_default();
        delivery
            .cursors
            .extend(page.outputs.iter().map(|entry| entry.cursor));
        delivery.vm_alive = page.vm_alive;
        Ok(())
    }

    pub(crate) fn check(&self) -> Result<(), RuntimeError> {
        let context = &self.context;
        let snapshot = context.inner.store.snapshot(&context.lease.session_id)?;
        if !snapshot.unresolved_calls.is_empty() || !snapshot.unresolved_capabilities.is_empty() {
            return Err(RuntimeError::Worker(
                "task still has unsettled calls".into(),
            ));
        }
        let records = context
            .inner
            .cells
            .records(
                &crate::cells::identity(context),
                &context.inner.config.cells,
            )
            .map_err(|error| RuntimeError::Worker(error.to_string()))?;
        let delivered = lock(&self.delivered);
        for record in records {
            if record.identity != crate::cells::identity(context) || record.unresolved() {
                return Err(RuntimeError::Worker(
                    "cell still has unsettled calls".into(),
                ));
            }
            let delivery = delivered.get(&record.cell_id);
            if !record.status.is_terminal() || delivery.is_none_or(|page| page.vm_alive) {
                return Err(RuntimeError::Worker("cell still requires wait".into()));
            }
            if record.status == CellStatus::OutcomeUnknown {
                return Err(RuntimeError::NeedsReconciliation);
            }
            if record
                .outputs
                .iter()
                .any(|entry| delivery.is_none_or(|page| !page.cursors.contains(&entry.cursor)))
            {
                return Err(RuntimeError::Worker("unread cell output".into()));
            }
            if let Some(code) = record.outputs.iter().find_map(|entry| match entry.output {
                CellOutput::Exit { code } if code != 0 => Some(code),
                _ => None,
            }) {
                return Err(RuntimeError::Worker(format!(
                    "cell exited with code {code}"
                )));
            }
        }
        drop(delivered);
        if let Some(host) = lock(&context.inner.processes).get(&context.task.id)
            && let Some(reason) = host.terminal_completion_error()
        {
            return Err(RuntimeError::Worker(reason));
        }
        Ok(())
    }
}
