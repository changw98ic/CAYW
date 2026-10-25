use async_trait::async_trait;
use cayw_ai::{CancellationToken, ToolDefinition};
use cayw_cells::*;
use cayw_codemode::CapabilityError;
use serde_json::Value;
use std::sync::{Arc, Mutex};

struct Observer {
    journal: Arc<SqliteJournal>,
    seen: Mutex<Vec<OutputEntry>>,
}
#[async_trait]
impl CellBackend for Observer {
    fn capabilities(&self) -> Vec<ToolDefinition> {
        vec![]
    }
    fn created(&self, _: &CellExecution) -> Result<(), CellError> {
        Ok(())
    }
    async fn call(
        &self,
        _: &CellExecution,
        _: u64,
        _: &str,
        _: Value,
        _: CancellationToken,
    ) -> Result<BackendResponse, CapabilityError> {
        Err(CapabilityError::new(
            "no host execution in observer fixture",
        ))
    }
    fn finished(&self, _: &CellExecution, _: &CellTerminal) -> Result<(), CellError> {
        Ok(())
    }
    fn output(&self, execution: &CellExecution, entry: &OutputEntry) -> Result<(), CellError> {
        // Inspect the journal independently, before returning control to Luau.
        let records = self
            .journal
            .records(&execution.identity, &CellLimits::default())?;
        let record = records
            .iter()
            .find(|record| record.cell_id == execution.cell_id)
            .unwrap();
        assert_eq!(
            record.outputs.last().map(|output| output.cursor),
            Some(entry.cursor)
        );
        self.seen.lock().unwrap().push(entry.clone());
        Ok(())
    }
}

#[tokio::test]
async fn output_observer_receives_committed_entries_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(SqliteJournal::open(dir.path().join("cells.sqlite")).unwrap());
    let observer = Arc::new(Observer {
        journal: journal.clone(),
        seen: Mutex::new(vec![]),
    });
    let limits = CellLimits::default();
    let reads = ReadCoordinator::new(vec![], limits.clone()).unwrap();
    let manager = CellManager::new(
        CellIdentity {
            session_id: "output-session".into(),
            task_id: "task".into(),
            attempt_id: "attempt".into(),
            permission_context: "fixture".into(),
        },
        journal,
        observer.clone(),
        reads,
        limits,
        cayw_codemode::Limits::default(),
    )
    .unwrap();
    let page = manager
        .exec(
            ExecRequest {
                script: "cayw.emit({value=1}); cayw.emit({value=2}); return 3".into(),
                yield_time_ms: 1000,
                max_output_tokens: 16384,
            },
            "fixture-parent",
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(page.status, CellStatus::Completed);
    let seen = observer.seen.lock().unwrap();
    assert!(seen.len() >= 2);
    assert_eq!(seen[0].cursor, 1);
    assert_eq!(seen[1].cursor, 2);
    assert!(matches!(&seen[0].output,CellOutput::Json{value} if value["value"]==1));
}
