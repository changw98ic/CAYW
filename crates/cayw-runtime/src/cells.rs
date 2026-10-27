use crate::{
    RuntimeError,
    execution::{RunContext, capability_error},
    lock,
};
use async_trait::async_trait;
use cayw_ai::{CancellationToken, ToolDefinition};
use cayw_cells::{CellBackend, CellError, CellExecution, CellIdentity, CellStatus, CellTerminal};
use cayw_codemode::{CapabilityError, CapabilityHost};
use cayw_core::{
    ApprovalDecision, CapabilityAuthorization, CapabilityCallKey, CapabilityInput,
    CapabilityOutcome, ExecutionInput, ExecutionOutcome,
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex, atomic::Ordering};

pub(crate) fn identity(context: &RunContext) -> CellIdentity {
    CellIdentity {
        session_id: context.lease.session_id.clone(),
        task_id: context.task.id.clone(),
        attempt_id: context.lease.attempt_id.clone(),
        permission_context: format!("{}:{}", context.lease.worker_id, context.lease.fence),
    }
}

/// Each scope owns exactly one planned capability. Permission waiting, audit and
/// drop all retain this origin even after the original model tool has replied.
pub(crate) struct CallScope {
    pub context: Arc<RunContext>,
    pub key: CapabilityCallKey,
    pub name: String,
    pub arguments: Value,
    pub request: crate::requests::Admission,
    pub bridge_origin: Option<crate::bridge::BridgeDispatchOrigin>,
    #[cfg(feature = "fault-injection")]
    fault_origin: Value,
    state: Mutex<CallState>,
}
#[derive(Default)]
struct CallState {
    authorization: Option<Authorization>,
    started: bool,
    settled: bool,
}
pub(crate) struct Authorization {
    pub proof: CapabilityAuthorization,
    pub grant: Option<crate::GrantRecord>,
}
impl CallScope {
    pub(crate) fn authorize(&self, authorization: Authorization) -> Result<(), RuntimeError> {
        let mut state = lock(&self.state);
        if state.authorization.is_some() || state.started || state.settled {
            return Err(RuntimeError::InvalidInput(
                "capability was already authorized or settled".into(),
            ));
        }
        state.authorization = Some(authorization);
        Ok(())
    }
    pub(crate) fn proof(
        &self,
        actor: String,
        approval_id: Option<String>,
    ) -> CapabilityAuthorization {
        CapabilityAuthorization {
            actor,
            target: self.key.clone(),
            task_id: self.context.task.id.clone(),
            capability: self.name.clone(),
            scope: crate::permissions::canonical_scope(&self.arguments),
            decision: ApprovalDecision::Approve,
            approval_id,
            external_provenance: self.bridge_origin.as_ref().map(|origin| {
                serde_json::to_value(&origin.bindings).expect("immutable Bridge bindings serialize")
            }),
        }
    }
    pub(crate) fn check_authority(
        &self,
        cancel: &CancellationToken,
    ) -> Result<(), CapabilityError> {
        self.context.check(cancel).map_err(capability_error)?;
        self.request.check(&self.context)?;
        if let Some(authorization) = &lock(&self.state).authorization
            && let Some(grant) = &authorization.grant
        {
            self.context
                .inner
                .journal
                .check_grant(grant)
                .map_err(known)?;
        }
        Ok(())
    }
    pub(crate) fn authorization_evidence(
        &self,
    ) -> Result<(CapabilityAuthorization, Option<crate::GrantRecord>), CapabilityError> {
        let state = lock(&self.state);
        let authorization = state
            .authorization
            .as_ref()
            .ok_or_else(|| known("read authorization is absent"))?;
        Ok((authorization.proof.clone(), authorization.grant.clone()))
    }
    pub(crate) fn has_started(&self) -> bool {
        lock(&self.state).started
    }
    pub(crate) fn start(&self) -> Result<(), CapabilityError> {
        self.context
            .check(&self.context.cancel)
            .map_err(capability_error)?;
        let mut state = lock(&self.state);
        if state.started || state.settled {
            return Err(CapabilityError::new(
                "capability execution boundary already crossed",
            ));
        }
        let authorization = state
            .authorization
            .as_ref()
            .ok_or_else(|| CapabilityError::new("capability has no fixed authorization"))?;
        if let Some(grant) = &authorization.grant {
            self.context
                .inner
                .journal
                .check_grant(grant)
                .map_err(known)?;
        }
        self.request.check(&self.context)?;
        if crate::processes::is_process(&self.name) {
            self.context
                .inner
                .process_journal
                .starting(&self.context.task.id, &self.context.lease)
                .map_err(|error| self.context.persistence_failed(error))?;
        }
        let proof = serde_json::to_value(&authorization.proof)
            .map_err(|error| self.context.persistence_failed(error))?;
        if let Some(evidence) = self.request.evidence() {
            self.context.host_event(
                "task_request_admitted",
                json!({
                    "key":self.key, "task_id":self.context.task.id,
                    "name":self.name, "receipt":evidence,
                }),
            )?;
        }
        self.request.check(&self.context)?;
        #[cfg(feature = "fault-injection")]
        cayw_ai::faults::hit("core.start.before", self.fault_origin.clone());
        self.context
            .inner
            .store
            .start_capability(&self.context.lease, &self.key, proof)
            .map_err(|error| self.context.persistence_failed(error))?;
        state.started = true;
        #[cfg(feature = "fault-injection")]
        cayw_ai::faults::hit("core.start.committed", self.fault_origin.clone());
        self.context.inner.changed.notify_waiters();
        Ok(())
    }
    pub(crate) fn settle(
        &self,
        result: &Result<Value, CapabilityError>,
    ) -> Result<(), CapabilityError> {
        let mut state = lock(&self.state);
        if state.settled {
            return Ok(());
        }
        let outcome = if !state.started {
            CapabilityOutcome::NotExecuted {
                reason: result.as_ref().err().map_or_else(
                    || "capability returned without crossing its execution boundary".into(),
                    |error| bounded(&error.message),
                ),
            }
        } else {
            match result {
                Ok(result) => CapabilityOutcome::Completed {
                    result: result.clone(),
                },
                Err(error) if error.outcome_unknown => CapabilityOutcome::OutcomeUnknown {
                    error: bounded(&error.message),
                },
                Err(error) => CapabilityOutcome::Failed {
                    error: bounded(&error.message),
                },
            }
        };
        #[cfg(feature = "fault-injection")]
        cayw_ai::faults::hit("core.capability_terminal.before", self.fault_origin.clone());
        self.context
            .inner
            .store
            .settle_capability(&self.context.lease, &self.key, outcome)
            .map_err(|error| self.context.persistence_failed(error))?;
        state.settled = true;
        #[cfg(feature = "fault-injection")]
        cayw_ai::faults::hit(
            "core.capability_terminal.committed",
            self.fault_origin.clone(),
        );
        self.context.inner.changed.notify_waiters();
        if let Err(error) = result {
            self.context.capability_stopped(error);
        }
        if result.as_ref().is_err_and(|error| error.outcome_unknown) && state.started {
            self.context.unknown.store(true, Ordering::Release);
            self.context.cancel.cancel();
        }
        if self.context.charge_bytes(&match result {
            Ok(result) => json!({"ok":true,"value":result}),
            Err(error) => json!({"ok":false,"error":error.message,"outcome_unknown":error.outcome_unknown}),
        }).is_err() {
            self.context.cancel.cancel();
        }
        Ok(())
    }
    fn dropped(&self) -> Result<(), CapabilityError> {
        let started = lock(&self.state).started;
        let error = if started {
            CapabilityError::unknown(
                "capability stopped after its execution boundary without a durable response",
            )
        } else {
            CapabilityError::new("capability stopped before execution")
        };
        self.settle(&Err(error))
    }
    fn audit_id(&self) -> String {
        format!("{}:{}", self.key.execution_id, self.key.sequence)
    }
}
pub(crate) fn bounded(message: &str) -> String {
    let mut end = message.len().min(4096);
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    if end == 0 {
        "capability failed".into()
    } else {
        message[..end].into()
    }
}
fn known(error: impl std::fmt::Display) -> CapabilityError {
    CapabilityError::new(error.to_string())
}
struct Pending(Arc<CallScope>);
impl Drop for Pending {
    fn drop(&mut self) {
        let _ = self.0.dropped();
    }
}
#[async_trait]
impl cayw_host::AuditSink for CallScope {
    async fn started(&self, name: &str, arguments: &Value) -> Result<String, CapabilityError> {
        if name != self.name || arguments != &self.arguments {
            return Err(CapabilityError::new(
                "audit origin does not match the planned capability",
            ));
        }
        self.start()?;
        Ok(self.audit_id())
    }
    async fn settled(
        &self,
        id: &str,
        result: &Result<Value, CapabilityError>,
    ) -> Result<(), CapabilityError> {
        if id != self.audit_id() {
            return Err(self
                .context
                .persistence_failed("capability terminal has a different audit identifier"));
        }
        self.settle(result)
    }
    fn dropped(&self, id: &str) -> Result<(), CapabilityError> {
        if id != self.audit_id() {
            return Err(self
                .context
                .persistence_failed("dropped capability has a different audit identifier"));
        }
        self.dropped()
    }
}

impl std::fmt::Debug for CallScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CallScope")
            .field("name", &self.name)
            .field("execution_id", &self.key.execution_id)
            .field("sequence", &self.key.sequence)
            .finish_non_exhaustive()
    }
}

impl CallScope {
    fn process_call_identity(&self) -> crate::processes::ProcessCallIdentity<'_> {
        crate::processes::ProcessCallIdentity {
            task: &self.context.task.id,
            lease: &self.context.lease,
            execution_id: &self.key.execution_id,
            sequence: self.key.sequence,
        }
    }

    fn record_process_identity(
        &self,
        write: impl FnOnce(
            &crate::processes::ProcessJournal,
            &crate::processes::ProcessCallIdentity<'_>,
        ) -> Result<(), RuntimeError>,
    ) -> std::io::Result<()> {
        let journal = &self.context.inner.process_journal;
        write(journal, &self.process_call_identity()).map_err(|error| {
            std::io::Error::other(format!("process identity record is not durable: {error}"))
        })
    }
}

/// Durable OS process identity for exactly this planned call. A failed record
/// stops the same supervisor and returns an error; it never releases an unknown
/// scope, skips capacity accounting, or avoids the true supervisor join.
impl cayw_host::ProcessObserver for CallScope {
    fn started(&self, identity: &cayw_host::ProcessIdentity) -> std::io::Result<()> {
        self.record_process_identity(|journal, call| journal.identity_started(call, identity))
    }
    fn observed(&self, identities: &[cayw_host::ProcessIdentity]) -> std::io::Result<()> {
        self.record_process_identity(|journal, call| journal.identity_observed(call, identities))
    }
    fn stopped(&self, identity: &cayw_host::ProcessIdentity) -> std::io::Result<()> {
        self.record_process_identity(|journal, call| journal.identity_stopped(call, identity))
    }
}

struct CatalogGate;
#[async_trait]
impl cayw_host::PermissionGate for CatalogGate {
    async fn authorize(
        &self,
        _: &str,
        _: &Value,
        _: CancellationToken,
    ) -> Result<(), CapabilityError> {
        Err(CapabilityError::new("catalog scope cannot execute"))
    }
}
struct CatalogAudit;
#[async_trait]
impl cayw_host::AuditSink for CatalogAudit {
    async fn started(&self, _: &str, _: &Value) -> Result<String, CapabilityError> {
        Err(CapabilityError::new("catalog scope cannot execute"))
    }
    async fn settled(
        &self,
        _: &str,
        _: &Result<Value, CapabilityError>,
    ) -> Result<(), CapabilityError> {
        Err(CapabilityError::new("catalog scope cannot execute"))
    }
    fn dropped(&self, _: &str) -> Result<(), CapabilityError> {
        Err(CapabilityError::new("catalog scope cannot execute"))
    }
}
pub(crate) fn catalog_host(
    root: &std::path::Path,
    config: cayw_host::HostConfig,
) -> Result<cayw_host::Host, RuntimeError> {
    cayw_host::Host::new(root, Arc::new(CatalogGate), Arc::new(CatalogAudit), config)
        .map_err(|error| RuntimeError::Workspace(error.to_string()))
}

pub(crate) struct Backend {
    context: Arc<RunContext>,
    base: Arc<cayw_host::Host>,
    catalog: Vec<ToolDefinition>,
    requests: Option<Arc<crate::requests::TaskRequests>>,
    #[cfg(feature = "plugins")]
    plugins: Arc<crate::plugins::PluginCatalog>,
}
impl Backend {
    pub(crate) fn new(context: Arc<RunContext>) -> Result<Self, RuntimeError> {
        let base = Arc::new(
            context
                .inner
                .host_root
                .task_host(Arc::new(CatalogGate), Arc::new(CatalogAudit))
                .map_err(|error| RuntimeError::Workspace(error.to_string()))?,
        );
        let requests = crate::requests::start(&context, &base)?;
        let mut catalog = crate::requests::operation_definitions(&context.inner, &base);
        catalog.extend(crate::requests::definitions(&context.inner));
        #[cfg(feature = "plugins")]
        let plugins = Arc::new(crate::plugins::PluginCatalog::new(&context)?);
        #[cfg(feature = "plugins")]
        catalog.extend(plugins.definitions());
        crate::processes::register(&context, base.clone())?;
        Ok(Self {
            context,
            base,
            catalog,
            requests,
            #[cfg(feature = "plugins")]
            plugins,
        })
    }
    fn verify(&self, execution: &CellExecution) -> Result<(), CellError> {
        if execution.identity != identity(&self.context) {
            return Err(CellError::Invalid(
                "cell origin does not match the active task lease".into(),
            ));
        }
        Ok(())
    }
}
#[async_trait]
impl CellBackend for Backend {
    fn capabilities(&self) -> Vec<ToolDefinition> {
        self.catalog.clone()
    }
    fn created(&self, execution: &CellExecution) -> Result<(), CellError> {
        self.verify(execution)?;
        self.context
            .check(&self.context.cancel)
            .map_err(|error| CellError::Invalid(error.to_string()))?;
        self.context
            .inner
            .store
            .begin_execution(
                &self.context.lease,
                ExecutionInput {
                    execution_id: execution.cell_id.clone(),
                    parent_call_id: execution.parent_call_id.clone(),
                    task_id: self.context.task.id.clone(),
                    source_hash: execution.source_hash.clone(),
                },
            )
            .map_err(|error| {
                CellError::Journal(self.context.persistence_failed(error).to_string())
            })?;
        self.context
            .activity
            .cell_created(&self.context, &execution.cell_id)
            .map_err(|error| {
                CellError::Journal(self.context.persistence_failed(error).to_string())
            })?;
        self.context.inner.changed.notify_waiters();
        Ok(())
    }
    async fn call(
        &self,
        execution: &CellExecution,
        sequence: u64,
        name: &str,
        arguments: Value,
        cancel: CancellationToken,
    ) -> Result<cayw_cells::BackendResponse, CapabilityError> {
        self.verify(execution).map_err(known)?;
        self.context
            .activity
            .resume_cell(&self.context, &execution.cell_id, &cancel)
            .await
            .map_err(capability_error)?;
        let cells_sequence = sequence;
        // Cells uses one-based journal intent positions; Core uses zero-based
        // capability keys. The checked mapping is fixed for the entire lifetime.
        let sequence = sequence
            .checked_sub(1)
            .ok_or_else(|| CapabilityError::new("cell capability sequence must start at one"))?;
        let key = CapabilityCallKey {
            execution_id: execution.cell_id.clone(),
            sequence,
        };
        #[cfg(feature = "fault-injection")]
        let fault_origin = json!({
            "session_id":self.context.lease.session_id,"task_id":self.context.task.id,
            "lease":self.context.lease,"key":key,"execution":execution,
            "parent_call_id":execution.parent_call_id,"source_hash":execution.source_hash,
            "name":name,"arguments":arguments,
        });
        #[cfg(feature = "fault-injection")]
        cayw_ai::faults::hit("core.plan.before", fault_origin.clone());
        let call = self
            .context
            .inner
            .store
            .plan_capability(
                &self.context.lease,
                CapabilityInput {
                    key: key.clone(),
                    name: name.into(),
                    arguments: arguments.clone(),
                },
            )
            .map_err(|error| self.context.persistence_failed(error))?;
        #[cfg(feature = "fault-injection")]
        {
            let mut committed = fault_origin.clone();
            committed["call"] = serde_json::to_value(&call).expect("original call serializes");
            cayw_ai::faults::hit("core.plan.committed", committed);
        }
        self.context.inner.changed.notify_waiters();
        let scope = Arc::new(CallScope {
            context: self.context.clone(),
            key,
            name: name.into(),
            arguments: arguments.clone(),
            bridge_origin: crate::bridge::BridgeDispatchOrigin::new(
                &call,
                &self.context.inner.config,
            ),
            request: crate::requests::admission(
                &self.context,
                self.requests.as_ref(),
                name,
                &arguments,
            ),
            #[cfg(feature = "fault-injection")]
            fault_origin,
            state: Mutex::default(),
        });
        let _pending = Pending(scope.clone());
        let admission = (|| {
            self.context.check(&cancel)?;
            self.context.admit_call(&arguments)
        })();
        let result = match admission {
            Err(error) => Err(capability_error(error)),
            Ok(()) if crate::requests::is_tool(name) && self.requests.is_some() => {
                crate::requests::invoke_tool(
                    self.requests.as_ref().expect("request task checked"),
                    &self.base,
                    &scope,
                    arguments,
                    cancel,
                )
                .await
            }
            Ok(()) if crate::values::is_value(name) => {
                crate::values::invoke(&scope, &self.base, execution, arguments, cancel).await
            }
            Ok(()) if crate::collaboration::is_tool(name) => {
                crate::collaboration::invoke(&scope, &self.base, arguments, cancel).await
            }
            Ok(()) if crate::content::is_tool(name) => {
                crate::content::invoke(&scope, &self.base, arguments, cancel).await
            }
            Ok(()) if crate::background::is_tool(name) => {
                crate::background::invoke(&scope, &self.base, arguments, cancel).await
            }
            Ok(()) if crate::results::is_tool(name) => {
                crate::results::invoke(&scope, &self.base, arguments, cancel).await
            }
            Ok(())
                if crate::children::is_capability(name)
                    && self.context.inner.config.local_agents.is_some() =>
            {
                match crate::bridge::scoped_host(&self.base, &scope) {
                    Ok(host) => crate::children::call(host, &scope, name, arguments, cancel).await,
                    Err(error) => Err(error),
                }
            }
            Ok(()) if name == "fs.read" && self.context.inner.config.host.frozen_reads => {
                return match crate::shared_read::call(
                    &scope,
                    &self.base,
                    execution,
                    cells_sequence,
                    arguments,
                    cancel,
                )
                .await
                {
                    Ok(response) => Ok(response),
                    Err(error) => {
                        let result = Err(error);
                        scope.settle(&result)?;
                        Ok(cayw_cells::BackendResponse::owned(
                            result,
                            None,
                            Box::new(crate::shared_read::Completion::ordinary(scope.clone())),
                        ))
                    }
                };
            }
            Ok(()) => {
                // Cooperating writes advance the held-root Host generation.
                // Pure reads retain the same immutable shared capture.
                let host = crate::bridge::scoped_host(&self.base, &scope);
                match host {
                    Err(error) => Err(error),
                    Ok(host) => {
                        #[cfg(feature = "plugins")]
                        let host: Arc<dyn CapabilityHost> =
                            Arc::new(crate::plugins::PluginHost::new(
                                host,
                                scope.clone(),
                                self.plugins.clone(),
                            ));
                        #[cfg(not(feature = "plugins"))]
                        let host: Arc<dyn CapabilityHost> = Arc::new(host);
                        crate::requests::call(&scope, host, arguments, cancel).await
                    }
                }
            }
        };
        // Core requires a nonempty bounded error. Return that same canonical
        // error to Cells, including refusals before Host audit start.
        // Settle original host execution result first so display/externalization
        // failure cannot erase an already committed Host success.
        let host_result = result.map_err(|mut error| {
            error.message = bounded(&error.message);
            error
        });
        scope.settle(&host_result)?;
        let presentation = match &host_result {
            Ok(value) => crate::results::maybe_externalize(&scope, value.clone()).await,
            Err(error) => Err(error.clone()),
        };
        let storage_error = presentation
            .as_ref()
            .err()
            .filter(|error| error.outcome_unknown && host_result.is_ok())
            .map(|error| error.message.clone());
        Ok(cayw_cells::BackendResponse::owned(
            host_result,
            None,
            Box::new(
                crate::shared_read::Completion::ordinary(scope.clone())
                    .with_storage_error(storage_error),
            ),
        )
        .with_presentation(presentation))
    }
    fn output(
        &self,
        execution: &CellExecution,
        entry: &cayw_cells::OutputEntry,
    ) -> Result<(), CellError> {
        // A structured trace is ordinary bounded output, never host authority.
        // Publish after the Cells commit so observers can follow the live path.
        let cayw_cells::CellOutput::Json { value } = &entry.output else {
            return Ok(());
        };
        if value.get("schema").and_then(Value::as_str) != Some("cayw.trace.v1") {
            return Ok(());
        }
        self.verify(execution)?;
        self.context
            .host_event(
                "trace_node",
                json!({
                    "execution_id":execution.cell_id,
                    "parent_call_id":execution.parent_call_id,
                    "source_hash":execution.source_hash,
                    "task_id":self.context.task.id,
                    "entry":entry,
                }),
            )
            .map_err(|error| CellError::Journal(error.to_string()))
    }
    fn finished(
        &self,
        execution: &CellExecution,
        terminal: &CellTerminal,
    ) -> Result<(), CellError> {
        self.verify(execution)?;
        if terminal.outcome_unknown {
            self.context.unknown.store(true, Ordering::Release);
            self.context.cancel.cancel();
        }
        let outcome = match terminal.status {
            CellStatus::Completed | CellStatus::Exited | CellStatus::Reconciled => {
                ExecutionOutcome::Completed {
                    result: json!({"cell_id":execution.cell_id,"status":terminal.status}),
                }
            }
            CellStatus::Cancelled => ExecutionOutcome::Cancelled {
                reason: terminal
                    .error
                    .as_deref()
                    .map(bounded)
                    .unwrap_or_else(|| "cell cancelled".into()),
            },
            CellStatus::Failed => ExecutionOutcome::Failed {
                error: terminal
                    .error
                    .as_deref()
                    .map(bounded)
                    .unwrap_or_else(|| "cell failed".into()),
            },
            _ => ExecutionOutcome::Interrupted {
                reason: terminal.error.as_deref().map(bounded).unwrap_or_else(|| {
                    "cell stopped; inspect its exact capability outcomes".into()
                }),
            },
        };
        #[cfg(feature = "fault-injection")]
        cayw_ai::faults::hit(
            "core.execution_terminal.before",
            json!({"session_id":self.context.lease.session_id,"task_id":self.context.task.id,"lease":self.context.lease,"execution":execution,"outcome":outcome}),
        );
        self.context
            .inner
            .store
            .finish_execution(&self.context.lease, &execution.cell_id, outcome)
            .map_err(|error| {
                CellError::Journal(self.context.persistence_failed(error).to_string())
            })?;
        #[cfg(feature = "fault-injection")]
        cayw_ai::faults::hit(
            "core.execution_terminal.committed",
            json!({
                "session_id": self.context.lease.session_id,
                "task_id": self.context.task.id,
                "lease": self.context.lease,
                "execution": execution,
                "terminal": terminal,
            }),
        );
        self.context
            .activity
            .cell_finished(&self.context, &execution.cell_id)
            .map_err(|error| {
                CellError::Journal(self.context.persistence_failed(error).to_string())
            })?;
        if let Some(stop) = terminal
            .stop
            .filter(|stop| *stop != cayw_codemode::CapabilityStop::Exited)
        {
            self.context.capability_stopped(&CapabilityError::stopped(
                stop,
                terminal
                    .error
                    .clone()
                    .unwrap_or_else(|| "cell execution stopped".into()),
            ));
        }
        self.context.inner.changed.notify_waiters();
        Ok(())
    }
}
