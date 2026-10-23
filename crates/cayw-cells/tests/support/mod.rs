use async_trait::async_trait;
use cayw_ai::{CancellationToken, ToolDefinition};
use cayw_cells::{
    CellBackend, CellError, CellExecution, CellLimits, CellManager, CellPage, CellTerminal,
    CellValueAuthorizer, CellValueExecutor, CellValueOperation, ExecRequest, SqliteJournal,
    authorized_cell_backend,
};
use cayw_codemode::{CapabilityError, CapabilityHost};
use serde_json::Value;
use std::sync::Arc;

/// An explicit test host adapter. Production uses Runtime's durable backend;
/// no blanket CapabilityHost -> CellBackend conversion exists in the library.
pub fn backend(host: Arc<dyn CapabilityHost>) -> Arc<dyn CellBackend> {
    Arc::new(Backend { host })
}
struct Backend {
    host: Arc<dyn CapabilityHost>,
}
#[async_trait]
impl CellBackend for Backend {
    fn capabilities(&self) -> Vec<ToolDefinition> {
        self.host.capabilities()
    }
    fn created(&self, _: &CellExecution) -> Result<(), CellError> {
        Ok(())
    }
    async fn call(
        &self,
        _: &CellExecution,
        _: u64,
        name: &str,
        args: Value,
        cancel: CancellationToken,
    ) -> Result<cayw_cells::BackendResponse, CapabilityError> {
        Ok(cayw_cells::BackendResponse::new(
            self.host.call(name, args, cancel).await,
        ))
    }
    fn finished(&self, _: &CellExecution, _: &CellTerminal) -> Result<(), CellError> {
        Ok(())
    }
}
/// Explicit test-only value policy. Production must supply its real host
/// authorization; the library does not default-allow store/load.
#[allow(dead_code)]
struct AllowFixtureValues;
#[async_trait]
impl CellValueAuthorizer for AllowFixtureValues {
    async fn authorize(
        &self,
        _: &CellExecution,
        _: CellValueOperation,
        _: &Value,
        _: CancellationToken,
    ) -> Result<(), CapabilityError> {
        Ok(())
    }
}
#[allow(dead_code)]
pub fn values_wrap(
    inner: Arc<dyn CellBackend>,
    journal: Arc<SqliteJournal>,
    limits: CellLimits,
) -> Arc<dyn CellBackend> {
    authorized_cell_backend(
        inner,
        CellValueExecutor::new(journal, limits),
        Arc::new(AllowFixtureValues),
    )
}
#[allow(dead_code)]
pub fn backend_with_values(
    host: Arc<dyn CapabilityHost>,
    journal: Arc<SqliteJournal>,
    limits: CellLimits,
) -> Arc<dyn CellBackend> {
    values_wrap(backend(host), journal, limits)
}
/// Explicit test fixture for independently admitted reads. The adapter below is
/// a real CellBackend; Bridge never has a registered-read bypass.
#[allow(dead_code)]
#[async_trait]
pub trait FixtureRead: Send + Sync {
    async fn admit(
        &self,
        identity: &cayw_cells::CellIdentity,
        arguments: &Value,
        cancel: CancellationToken,
    ) -> Result<cayw_cells::ReadStamp, CapabilityError>;
    async fn capture(
        &self,
        stamp: &cayw_cells::ReadStamp,
        cancel: CancellationToken,
    ) -> Result<Value, CapabilityError>;
}
#[allow(dead_code)]
pub fn shared_backend(
    host: Arc<dyn CapabilityHost>,
    fixture: Arc<dyn FixtureRead>,
    reads: Arc<cayw_cells::ReadCoordinator>,
) -> Arc<dyn CellBackend> {
    Arc::new(Shared {
        host,
        fixture,
        reads,
    })
}
struct Shared {
    host: Arc<dyn CapabilityHost>,
    fixture: Arc<dyn FixtureRead>,
    reads: Arc<cayw_cells::ReadCoordinator>,
}
struct Source<'a> {
    fixture: &'a dyn FixtureRead,
    stamp: cayw_cells::ReadStamp,
}
#[async_trait]
impl cayw_cells::ReadSource for Source<'_> {
    async fn capture(&mut self, cancel: CancellationToken) -> Result<Value, CapabilityError> {
        self.fixture.capture(&self.stamp, cancel).await
    }
}
struct Complete(Option<cayw_cells::ReadCandidate>);
impl cayw_cells::BackendCompletion for Complete {
    fn acknowledge(mut self: Box<Self>) {
        self.0.take().unwrap().acknowledge();
    }
}
#[async_trait]
impl CellBackend for Shared {
    fn capabilities(&self) -> Vec<ToolDefinition> {
        self.host.capabilities()
    }
    fn created(&self, _: &CellExecution) -> Result<(), CellError> {
        Ok(())
    }
    async fn call(
        &self,
        execution: &CellExecution,
        sequence: u64,
        name: &str,
        args: Value,
        cancel: CancellationToken,
    ) -> Result<cayw_cells::BackendResponse, CapabilityError> {
        if name != "read" {
            self.reads.invalidate(&execution.identity.session_id);
            return Ok(cayw_cells::BackendResponse::new(
                self.host.call(name, args, cancel).await,
            ));
        }
        let stamp = self
            .fixture
            .admit(&execution.identity, &args, cancel.clone())
            .await?;
        let mut source = Source {
            fixture: &*self.fixture,
            stamp: stamp.clone(),
        };
        let candidate = self
            .reads
            .read(
                cayw_cells::ReadOrigin {
                    execution: execution.clone(),
                    sequence,
                },
                name,
                &args,
                stamp,
                &mut source,
                cancel,
            )
            .await?;
        Ok(cayw_cells::BackendResponse::owned(
            Ok(candidate.value().clone()),
            Some(candidate.dependencies.clone()),
            Box::new(Complete(Some(candidate))),
        ))
    }
    fn finished(&self, _: &CellExecution, _: &CellTerminal) -> Result<(), CellError> {
        Ok(())
    }
}
#[allow(dead_code)]
#[async_trait]
pub trait TestCellExec {
    async fn exec_fixture(
        &self,
        request: ExecRequest,
        cancel: CancellationToken,
    ) -> Result<CellPage, CellError>;
}
#[async_trait]
impl TestCellExec for CellManager {
    async fn exec_fixture(
        &self,
        request: ExecRequest,
        cancel: CancellationToken,
    ) -> Result<CellPage, CellError> {
        self.exec(request, "fixture-parent", cancel).await
    }
}
