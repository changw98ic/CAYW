//! A known refusal from `CellBackend::created` is not an unknown outcome.
//! No external effect exists to reconcile, so the closed cell stays known.

use async_trait::async_trait;
use cayw_ai::{CancellationToken, ToolDefinition};
use cayw_cells::{
    BackendResponse, CellBackend, CellError, CellExecution, CellIdentity, CellLimits, CellManager,
    CellStatus, CellTerminal, ExecRequest, ReadCoordinator, SqliteJournal,
};
use cayw_codemode::{CapabilityError, Limits};
use serde_json::Value;
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

struct Refusing {
    finished: AtomicUsize,
}
#[async_trait]
impl CellBackend for Refusing {
    fn capabilities(&self) -> Vec<ToolDefinition> {
        Vec::new()
    }
    fn created(&self, _: &CellExecution) -> Result<(), CellError> {
        Err(CellError::Invalid("known creation refusal".into()))
    }
    async fn call(
        &self,
        _: &CellExecution,
        _: u64,
        _: &str,
        _: Value,
        _: CancellationToken,
    ) -> Result<BackendResponse, CapabilityError> {
        Err(CapabilityError::new("a refused cell cannot call"))
    }
    fn finished(&self, _: &CellExecution, _: &CellTerminal) -> Result<(), CellError> {
        self.finished.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test]
async fn refused_creation_reports_a_known_terminal() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(SqliteJournal::open(dir.path().join("cells.db")).unwrap());
    let limits = CellLimits::default();
    let reads = ReadCoordinator::new(vec![], limits.clone()).unwrap();
    let backend = Arc::new(Refusing {
        finished: AtomicUsize::new(0),
    });
    let manager = CellManager::new(
        CellIdentity {
            session_id: "session".into(),
            task_id: "task".into(),
            attempt_id: "attempt".into(),
            permission_context: "principal/policy-v1".into(),
        },
        journal,
        backend.clone(),
        reads,
        limits,
        Limits {
            timeout: Some(Duration::from_secs(30)),
            host_calls: Some(256),
            ..Limits::default()
        },
    )
    .unwrap();
    let error = manager
        .exec(
            ExecRequest {
                script: "return 1".into(),
                yield_time_ms: 0,
                max_output_tokens: 4096,
            },
            "parent-call",
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, CellError::Invalid(_)), "{error:?}");
    assert_eq!(
        backend.finished.load(Ordering::SeqCst),
        0,
        "a refused creation has no backend execution terminal"
    );
    let report = manager.close().await;
    assert!(!report.outcome_unknown, "{report:#?}");
    assert_eq!(report.cells.len(), 1);
    assert!(!report.cells[0].terminal.outcome_unknown);
    assert_eq!(report.cells[0].terminal.status, CellStatus::Cancelled);
    let record = manager.record(&report.cells[0].execution.cell_id).unwrap();
    assert_ne!(record.status, CellStatus::OutcomeUnknown);
    assert!(record.backend_error.is_none());
}
