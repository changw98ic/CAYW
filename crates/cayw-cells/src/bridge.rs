use crate::{manager::Run, *};
use async_trait::async_trait;
use cayw_ai::{CancellationToken, ToolDefinition};
use cayw_codemode::{CapabilityError, CapabilityHost, VmOperation, VmServices};
use serde_json::{Value, json};
use std::sync::{Arc, atomic::Ordering};

pub(crate) struct Bridge {
    backend: Arc<dyn CellBackend>,
    run: Arc<Run>,
    catalog: Vec<ToolDefinition>,
}
impl Bridge {
    pub(crate) fn new(backend: Arc<dyn CellBackend>, run: Arc<Run>) -> Result<Self, CellError> {
        let host_catalog = backend.capabilities();
        // Catalog definitions are captured host data, not model output items.
        // Validate their structure here; item_bytes applies when Search or
        // Definition actually returns a result to Lua.
        for definition in &host_catalog {
            if CellValueOperation::from_name(&definition.name).is_some() {
                // Canonical value definitions replace any host definition with
                // these reserved names below.
                continue;
            }
            validate_json(
                &serde_json::to_value(definition)?,
                run.limits.json_depth,
                usize::MAX,
            )?;
        }
        // The canonical schemas add a fixed ToolDefinition/properties envelope.
        // They are static and trusted, so validate them with a minimum depth
        // that does not let a small user json_depth reject store/load. Their
        // bytes are catalog data, not a model output item: item_bytes still
        // applies when Definition/Search returns a result to Lua.
        let catalog = crate::values::with_definitions(host_catalog);
        for definition in crate::values::definitions() {
            validate_json(
                &serde_json::to_value(definition)?,
                run.limits.json_depth.max(8),
                usize::MAX,
            )?;
        }
        Ok(Self {
            backend,
            run,
            catalog,
        })
    }
    async fn internal(
        &self,
        operation: VmOperation,
        args: Value,
        cancel: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        match operation {
            VmOperation::Store => self.real_call(CELL_STORE, args, cancel).await,
            VmOperation::Load => self.real_call(CELL_LOAD, args, cancel).await,
            _ => self
                .local(operation, args, cancel)
                .await
                .map_err(CellError::capability_error),
        }
    }
    async fn local(
        &self,
        operation: VmOperation,
        args: Value,
        cancel: CancellationToken,
    ) -> Result<Value, CellError> {
        self.run.check()?;
        match operation {
            VmOperation::Store | VmOperation::Load => Err(CellError::Invalid(
                "value capabilities require trusted backend dispatch".into(),
            )),
            VmOperation::Output => {
                let output: CellOutput =
                    serde_json::from_value(args).map_err(|e| CellError::Invalid(e.to_string()))?;
                if matches!(output, CellOutput::Exit { .. }) {
                    return Err(CellError::Invalid("use exit() to stop the cell".into()));
                }
                self.run.emit(output)?;
                Ok(Value::Null)
            }
            VmOperation::Exit => {
                let code = args["code"]
                    .as_i64()
                    .and_then(|v| i32::try_from(v).ok())
                    .ok_or_else(|| CellError::Invalid("exit code requires an i32".into()))?;
                self.run.emit(CellOutput::Exit { code })?;
                self.run.exited.store(true, Ordering::Release);
                // The typed exit passes through protected calls. The manager
                // recognizes its own flag, never error text, as normal exit.
                Err(CellError::Exited)
            }
            VmOperation::Yield => {
                self.run.update(|record| {
                    record.status = CellStatus::Yielded;
                    Ok(())
                })?;
                let mut receiver = self.run.resume_rx.lock().await;
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => Err(CellError::Closed),
                    signal = receiver.recv() => { signal.ok_or(CellError::Closed)?; self.run.check()?; Ok(Value::Null) },
                }
            }
            VmOperation::Search => {
                let query = args["query"]
                    .as_str()
                    .ok_or_else(|| CellError::Invalid("query requires a string".into()))?
                    .to_lowercase();
                let limit = args["limit"]
                    .as_u64()
                    .filter(|v| (1..=64).contains(v))
                    .ok_or_else(|| {
                        CellError::Invalid("limit requires an integer in 1..=64".into())
                    })? as usize;
                let names: Vec<_> = self
                    .catalog
                    .iter()
                    .filter(|d| {
                        d.name.to_lowercase().contains(&query)
                            || d.description.to_lowercase().contains(&query)
                    })
                    .take(limit)
                    .map(|d| json!({"name":d.name,"description":d.description}))
                    .collect();
                let value = Value::Array(names);
                validate_json(
                    &value,
                    self.run.limits.json_depth,
                    self.run.limits.item_bytes,
                )?;
                Ok(value)
            }
            VmOperation::Definition => {
                let name = args["name"]
                    .as_str()
                    .ok_or_else(|| CellError::Invalid("name requires a string".into()))?;
                let definition = self
                    .catalog
                    .iter()
                    .find(|d| d.name == name)
                    .ok_or(CellError::NotFound)?;
                let value = serde_json::to_value(definition)?;
                validate_json(
                    &value,
                    self.run.limits.json_depth.max(8),
                    self.run.limits.item_bytes,
                )?;
                Ok(value)
            }
        }
    }
    fn argument_limits(&self, name: &str) -> (usize, usize) {
        if name == CELL_STORE {
            (
                self.run.limits.json_depth.saturating_add(1),
                self.run.limits.store_bytes.saturating_add(256),
            )
        } else {
            (self.run.limits.json_depth, self.run.limits.item_bytes)
        }
    }
    fn result_limits(&self, name: &str) -> (usize, usize) {
        if name == CELL_LOAD {
            (
                self.run.limits.json_depth.saturating_add(1),
                self.run.limits.store_bytes.saturating_add(64),
            )
        } else {
            (self.run.limits.json_depth, self.run.limits.item_bytes)
        }
    }
    async fn real_call(
        &self,
        name: &str,
        args: Value,
        cancel: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        let failure = CellError::capability_error;
        self.run.check().map_err(failure)?;
        let (argument_depth, argument_bytes) = self.argument_limits(name);
        validate_json(&args, argument_depth, argument_bytes).map_err(failure)?;
        let identity = self.run.identity();
        if !self
            .catalog
            .iter()
            .any(|definition| definition.name == name)
        {
            return Err(CapabilityError::new(
                "capability absent from the captured host catalog",
            ));
        }
        #[cfg(feature = "fault-injection")]
        cayw_ai::faults::hit(
            "cells.intent.before",
            json!({"execution":self.run.execution,"name":name,"arguments":args}),
        );
        let mut sequence = 0;
        self.run
            .update(|record| {
                if record.calls.len() >= 4096 {
                    return Err(CellError::capacity(
                        CapacityKind::HostCalls,
                        Some(record.cell_id.clone()),
                        Some(record.status),
                        record.calls.len(),
                        4096,
                    ));
                }
                sequence = record.calls.len() as u64 + 1;
                record.calls.push(CapabilityIntent {
                    sequence,
                    name: name.into(),
                    arguments: args.clone(),
                    dependencies: None,
                    status: IntentStatus::Pending,
                    result: None,
                    result_omitted: false,
                    result_hash: None,
                    error: None,
                });
                Ok(())
            })
            .map_err(failure)?;
        #[cfg(feature = "fault-injection")]
        cayw_ai::faults::hit(
            "cells.intent.committed",
            json!({"execution":self.run.execution,"sequence":sequence,"name":name,"arguments":args}),
        );
        // This drop guard records unknown synchronously, even when CodeMode
        // drops a host future on timeout or when the manager aborts its VM.
        // The intent also brackets independently awaited permission checks.
        let mut pending = Pending {
            run: self.run.clone(),
            sequence,
            settled: false,
        };
        // Every capability, including ready shared reads, traverses the original
        // backend once with its own full origin and checked original sequence.
        let response = match self
            .backend
            .call(&self.run.execution, sequence, name, args, cancel)
            .await
        {
            Ok(response) => response,
            Err(error) => BackendResponse::new(Err(error)),
        };
        if let Some(dependencies) = response.dependencies() {
            dependencies
                .validate(&identity.permission_context, &self.run.limits)
                .map_err(|error| {
                    self.run.reads.freeze(&identity.session_id);
                    CapabilityError::unknown(error.to_string())
                })?;
        }
        let (result_depth, result_bytes) = self.result_limits(name);
        #[cfg(feature = "fault-injection")]
        cayw_ai::faults::hit(
            "cells.call_terminal.before",
            json!({"execution":self.run.execution,"sequence":sequence,"name":name}),
        );
        let persist = self.run.update(|record| {
            let call = &mut record.calls[sequence as usize - 1];
            call.dependencies = response.dependencies().cloned();
            match response.result() {
                Ok(value) => {
                    call.status = IntentStatus::Succeeded;
                    call.result_hash = Some(HostResult::value_hash(value));
                    if validate_json(value, result_depth, result_bytes).is_ok() {
                        call.result = Some(value.clone());
                    } else {
                        call.result_omitted = true;
                    }
                }
                Err(error) => {
                    call.status = if error.outcome_unknown {
                        IntentStatus::OutcomeUnknown
                    } else {
                        IntentStatus::Failed
                    };
                    call.error = Some(bounded_error(&error.message));
                    if error.outcome_unknown {
                        record.status = CellStatus::OutcomeUnknown;
                        self.run.reads.freeze(&identity.session_id);
                    }
                }
            }
            Ok(())
        });
        pending.settled = persist.is_ok();
        if let Err(error) = persist {
            return Err(CapabilityError::unknown(error.to_string()));
        }
        #[cfg(feature = "fault-injection")]
        cayw_ai::faults::hit(
            "cells.call_terminal.committed",
            json!({"execution":self.run.execution,"sequence":sequence,"name":name}),
        );
        #[cfg(feature = "fault-injection")]
        cayw_ai::faults::hit(
            "cells.response_ack.before",
            json!({"execution":self.run.execution,"sequence":sequence,"name":name}),
        );
        // All dependencies and the original terminal are now durable. Only this
        // infallible handoff may publish a leader and release its read fence.
        let result = response.acknowledge();
        match result {
            Ok(value) => {
                if let Err(error) = validate_json(&value, result_depth, result_bytes) {
                    discard_json(value);
                    return Err(failure(error));
                }
                Ok(value)
            }
            Err(error) => Err(error),
        }
    }
}
#[async_trait]
impl CapabilityHost for Bridge {
    fn capabilities(&self) -> Vec<ToolDefinition> {
        self.catalog.clone()
    }
    async fn call(
        &self,
        name: &str,
        arguments: Value,
        cancel: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        self.real_call(name, arguments, cancel).await
    }
}
struct Pending {
    run: Arc<Run>,
    sequence: u64,
    settled: bool,
}
impl Drop for Pending {
    fn drop(&mut self) {
        if !self.settled {
            self.run.reads.freeze(&self.run.identity().session_id);
            let _ = self.run.update(|record| {
                let call = &mut record.calls[self.sequence as usize - 1];
                call.status = IntentStatus::OutcomeUnknown;
                call.error = Some(
                    "Call stopped before a durable response; reconcile before continuing".into(),
                );
                record.status = CellStatus::OutcomeUnknown;
                Ok(())
            });
        }
    }
}

#[async_trait]
impl VmServices for Bridge {
    fn emit(&self, value: Value) -> Result<(), CapabilityError> {
        self.run
            .check()
            .and_then(|()| self.run.emit(CellOutput::Json { value }))
            .map_err(CellError::capability_error)
    }
    async fn invoke(
        &self,
        operation: VmOperation,
        arguments: Value,
        cancel: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        self.internal(operation, arguments, cancel).await
    }
}
