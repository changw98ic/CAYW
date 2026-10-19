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
        let catalog = backend.capabilities();
        for definition in &catalog {
            validate_json(
                &serde_json::to_value(definition)?,
                run.limits.json_depth,
                run.limits.item_bytes,
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
    ) -> Result<Value, CellError> {
        self.run.check()?;
        let identity = self.run.identity();
        match operation {
            VmOperation::Store => {
                let key = args["key"]
                    .as_str()
                    .ok_or_else(|| CellError::Invalid("key requires a string".into()))?;
                let version = args["expected_version"].as_u64().ok_or_else(|| {
                    CellError::Invalid("expected_version requires an unsigned integer".into())
                })?;
                let value = args
                    .get("value")
                    .ok_or_else(|| CellError::Invalid("value required".into()))?
                    .clone();
                let snapshot = self.run.journal.store(
                    &identity.session_id,
                    key,
                    version,
                    value,
                    &self.run.limits,
                )?;
                // Return a receipt rather than echoing a value potentially as
                // large as the session store. load() retrieves the snapshot.
                Ok(json!({"version":snapshot.version}))
            }
            VmOperation::Load => {
                let key = args["key"]
                    .as_str()
                    .ok_or_else(|| CellError::Invalid("key requires a string".into()))?;
                let snapshot = self.run.journal.load(&identity.session_id, key)?;
                if let Some(snapshot) = &snapshot {
                    // json_depth applies to the logical stored value. The
                    // version/value receipt adds one fixed envelope level.
                    validate_json(
                        &snapshot.value,
                        self.run.limits.json_depth,
                        self.run.limits.store_bytes,
                    )?;
                }
                let value = serde_json::to_value(snapshot)?;
                validate_json(
                    &value,
                    self.run.limits.json_depth.saturating_add(1),
                    self.run.limits.store_bytes.saturating_add(64),
                )?;
                Ok(value)
            }
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
                // CodeMode treats a host Err as an uncatchable execution fault.
                // Its pcall/xpcall/coroutine libraries are absent. The manager
                // recognizes its own flag, never the error text, as normal exit.
                Err(CellError::Closed)
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
                Ok(serde_json::to_value(definition)?)
            }
        }
    }
    async fn real_call(
        &self,
        name: &str,
        args: Value,
        cancel: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        let failure = |e: CellError| CapabilityError::new(e.to_string());
        self.run.check().map_err(failure)?;
        validate_json(
            &args,
            self.run.limits.json_depth,
            self.run.limits.item_bytes,
        )
        .map_err(failure)?;
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
        let mut sequence = 0;
        self.run
            .update(|record| {
                if record.calls.len() >= 4096 {
                    return Err(CellError::Capacity);
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
        let persist = self.run.update(|record| {
            let call = &mut record.calls[sequence as usize - 1];
            call.dependencies = response.dependencies().cloned();
            match response.result() {
                Ok(value) => {
                    call.status = IntentStatus::Succeeded;
                    call.result_hash = Some(HostResult::value_hash(value));
                    if validate_json(
                        value,
                        self.run.limits.json_depth,
                        self.run.limits.item_bytes,
                    )
                    .is_ok()
                    {
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
        // All dependencies and the original terminal are now durable. Only this
        // infallible handoff may publish a leader and release its read fence.
        let result = response.acknowledge();
        match result {
            Ok(value) => {
                if let Err(error) = validate_json(
                    &value,
                    self.run.limits.json_depth,
                    self.run.limits.item_bytes,
                ) {
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
            .map_err(|e| CapabilityError::new(e.to_string()))
    }
    async fn invoke(
        &self,
        operation: VmOperation,
        arguments: Value,
        cancel: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        self.internal(operation, arguments, cancel)
            .await
            .map_err(|e| CapabilityError::new(e.to_string()))
    }
}
