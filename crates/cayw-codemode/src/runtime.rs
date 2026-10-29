use crate::{
    CallStatus, CapabilityCall, CapabilityError, CapabilityHost, CapabilityStop, CodeMode,
    CodeModeError, ErrorKind, ExecutionReport, Limits, VmOperation, VmServices, json,
};
use cayw_ai::CancellationToken;
use mlua::{Lua, LuaOptions, LuaSerdeExt, MultiValue, StdLib, Value as LuaValue, VmState};
use serde::Serialize;
use serde_json::{Value, json as json_value};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Instant,
};

#[derive(Clone, Debug, thiserror::Error)]
#[error("{message}")]
pub(crate) struct Fault {
    pub(crate) kind: ErrorKind,
    pub(crate) message: String,
}
impl Fault {
    pub(crate) fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        let message = message.into();
        Self {
            kind,
            message: json::bounded_message(&message).to_owned(),
        }
    }
    pub(crate) fn from_lua(error: mlua::Error) -> Self {
        if let Some(fault) = error.downcast_ref::<Self>() {
            return fault.clone();
        }
        let memory = error.chain().any(|cause| {
            matches!(
                cause.downcast_ref::<mlua::Error>(),
                Some(mlua::Error::MemoryError(_))
            )
        });
        Self::new(
            if memory {
                ErrorKind::MemoryLimit
            } else {
                ErrorKind::Script
            },
            error.to_string(),
        )
    }
}

struct State {
    report: ExecutionReport,
    fault: Option<Fault>,
}
// Keep the started Host or cell service outside the suspended Luau coroutine.
// Luau can run an interrupt before polling an async callback again; retaining
// it here lets Rust settle that exact invocation even when the interrupt ends
// the coroutine. `started` records the first actual poll: an unstarted future
// is dropped, never dispatched during shutdown grace.
#[derive(Clone, Copy)]
enum PendingTarget {
    Host(usize),
    Service,
}
struct PendingCall {
    target: PendingTarget,
    started: bool,
    cancel: CancellationToken,
    shutdown_deadline: Option<tokio::time::Instant>,
    future:
        std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value, CapabilityError>> + Send>>,
}
impl Drop for PendingCall {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}
struct Clock {
    deadline: Instant,
    paused: Option<Instant>,
}
struct Run {
    state: Mutex<State>,
    limits: Limits,
    clock: Mutex<Clock>,
    cancel: CancellationToken,
    pending: Mutex<Option<PendingCall>>,
}
struct Pause(Arc<Run>);
impl Drop for Pause {
    fn drop(&mut self) {
        let mut clock = self.0.clock.lock().expect("VM clock poisoned");
        if let Some(started) = clock.paused.take() {
            clock.deadline += started.elapsed();
        }
    }
}
impl Run {
    fn poll_pending(
        &self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<Value, CapabilityError>> {
        // A suspended mlua callback can outlive its parent async thread. Keep
        // ownership here, but never keep the guard across an asynchronous wait.
        let mut pending = self.pending.lock().expect("pending call poisoned");
        let pending = pending.as_mut().expect("registered pending call");
        pending.started = true;
        pending.future.as_mut().poll(cx)
    }
    fn check(&self) -> Result<(), Fault> {
        if let Some(fault) = &self.state.lock().expect("execution state poisoned").fault {
            return Err(fault.clone());
        }
        if self.cancel.is_cancelled() {
            return Err(self.fail(Fault::new(
                ErrorKind::Cancelled,
                "CodeMode execution cancelled",
            )));
        }
        let expired = {
            let clock = self.clock.lock().expect("VM clock poisoned");
            clock.paused.is_none() && Instant::now() >= clock.deadline
        };
        if expired {
            return Err(self.fail(Fault::new(
                ErrorKind::Timeout,
                "CodeMode time limit reached",
            )));
        }
        Ok(())
    }
    fn fail(&self, fault: Fault) -> Fault {
        self.state
            .lock()
            .expect("execution state poisoned")
            .fault
            .get_or_insert(fault)
            .clone()
    }
    /// Returns the latched stop reason when it must not be handled as an
    /// ordinary recoverable script error.
    fn latched_fatal(&self) -> Option<Fault> {
        self.state
            .lock()
            .expect("execution state poisoned")
            .fault
            .clone()
            .filter(|fault| fault.is_fatal())
    }
    fn remaining(&self) -> usize {
        self.limits.output_bytes.saturating_sub(
            self.state
                .lock()
                .expect("execution state poisoned")
                .report
                .output_bytes,
        )
    }
    fn charge(&self, value: &(impl Serialize + ?Sized)) -> Result<(), Fault> {
        let mut state = self.state.lock().expect("execution state poisoned");
        let remaining = self
            .limits
            .output_bytes
            .saturating_sub(state.report.output_bytes);
        let size = json::size(value, remaining)?;
        state.report.output_bytes += size;
        Ok(())
    }
    fn interrupt(&self) -> Result<VmState, Fault> {
        self.check()?;
        let exhausted = {
            let mut state = self.state.lock().expect("execution state poisoned");
            state.report.instruction_checkpoints =
                state.report.instruction_checkpoints.saturating_add(1);
            state.report.instruction_checkpoints > self.limits.instruction_budget
        };
        if exhausted {
            return Err(self.fail(Fault::new(
                ErrorKind::InstructionLimit,
                "Luau interrupt checkpoint budget exhausted",
            )));
        }
        // AsyncThread returns Pending after a hook yield, giving the executor a
        // chance to run cancellation and timers even on a single worker thread.
        let checkpoints = self
            .state
            .lock()
            .expect("execution state poisoned")
            .report
            .instruction_checkpoints;
        Ok(if checkpoints.is_multiple_of(256) {
            VmState::Yield
        } else {
            VmState::Continue
        })
    }
    fn error(&self, fault: Fault) -> CodeModeError {
        let mut state = self.state.lock().expect("execution state poisoned");
        // An interrupt can stop a suspended Luau coroutine before the host future
        // is polled again. Any call without a settled response remains unknown.
        state.report.outcome_unknown |= state
            .report
            .calls
            .iter()
            .any(|call| call.status == CallStatus::OutcomeUnknown);
        let fault = state.fault.get_or_insert(fault).clone();
        let remaining = self
            .limits
            .output_bytes
            .saturating_sub(state.report.output_bytes);
        let (message, bytes) = json::fit_message(&fault.message, remaining);
        state.report.output_bytes += bytes;
        CodeModeError {
            kind: fault.kind,
            message,
            report: Box::new(state.report.clone()),
        }
    }
}
impl Fault {
    /// Cancellation, exhausted budgets and unconfirmed effects stop the whole
    /// execution even when a script wraps the call in `pcall` or a coroutine.
    fn is_fatal(&self) -> bool {
        self.kind.stop().is_some() || self.kind == ErrorKind::OutcomeUnknown
    }
}
fn lua_result<T>(run: &Run, result: Result<T, Fault>) -> mlua::Result<T> {
    result.map_err(|fault| {
        if fault.is_fatal() {
            // Latch first: a later `run.check` re-raises even if the script
            // caught this error object.
            mlua::Error::external(run.fail(fault))
        } else {
            // Known business failures are ordinary script errors. They stay
            // catchable and do not permanently fault the execution.
            mlua::Error::external(fault)
        }
    })
}

/// Tracks that the script is synchronously inside `coroutine.resume`. Native
/// `resume` cannot drive an async Host call: a pending Rust future would yield
/// the user thread and leave the invocation without an owner while the script
/// keeps running. Host entry points therefore reject this context before any
/// dispatch.
struct CoroutineDepth(Arc<AtomicUsize>);
impl CoroutineDepth {
    fn enter(depth: Arc<AtomicUsize>) -> Self {
        depth.fetch_add(1, Ordering::SeqCst);
        Self(depth)
    }
}
impl Drop for CoroutineDepth {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Builds the coroutine library from the native implementation. Pure
/// computation, yields and generators stay available; every native entry that
/// resumes a thread goes through one guarded function, and a latched fatal
/// fault cannot be returned as an ordinary `false, error` pair.
fn restricted_coroutine(
    lua: &Lua,
    run: &Arc<Run>,
) -> Result<(mlua::Table, Arc<AtomicUsize>), Fault> {
    let library: mlua::Table = lua.globals().get("coroutine").map_err(Fault::from_lua)?;
    let table = lua.create_table().map_err(Fault::from_lua)?;
    for name in [
        "create",
        "status",
        "running",
        "isyieldable",
        "yield",
        "close",
    ] {
        let function: mlua::Function = library.get(name).map_err(Fault::from_lua)?;
        table.raw_set(name, function).map_err(Fault::from_lua)?;
    }
    let depth = Arc::new(AtomicUsize::new(0));
    let resume_run = run.clone();
    let resume_depth = depth.clone();
    let resume = lua
        .create_function(
            move |lua, (thread, arguments): (mlua::Thread, MultiValue)| {
                let _depth = CoroutineDepth::enter(resume_depth.clone());
                // Inspect the original VM error before native coroutine.resume can
                // turn allocator failure into an ordinary false/message pair.
                let outcome = thread.resume::<MultiValue>(arguments);
                if let Some(fault) = resume_run.latched_fatal() {
                    return Err(mlua::Error::external(fault));
                }
                let mut result = MultiValue::new();
                match outcome {
                    Ok(values) => {
                        result.push_back(LuaValue::Boolean(true));
                        result.extend(values);
                    }
                    Err(error) => {
                        let fault = Fault::from_lua(error.clone());
                        if fault.is_fatal() {
                            return Err(mlua::Error::external(resume_run.fail(fault)));
                        }
                        result.push_back(LuaValue::Boolean(false));
                        result.push_back(LuaValue::String(lua.create_string(error.to_string())?));
                    }
                }
                Ok(result)
            },
        )
        .map_err(Fault::from_lua)?;
    table
        .raw_set("resume", resume.clone())
        .map_err(Fault::from_lua)?;
    let create: mlua::Function = table.get("create").map_err(Fault::from_lua)?;
    let unpack: mlua::Function = lua
        .globals()
        .get::<mlua::Table>("table")
        .map_err(Fault::from_lua)?
        .get("unpack")
        .map_err(Fault::from_lua)?;
    let select: mlua::Function = lua.globals().get("select").map_err(Fault::from_lua)?;
    // `wrap` must reuse the guarded `resume`, so it is built here rather than
    // taken from the native library.
    let wrap: mlua::Function = lua
        .load(
            r##"
            local create, resume, unpack, select = ...
            local function pack(...)
                return select("#", ...), {...}
            end
            return function(f)
                local co = create(f)
                return function(...)
                    local count, result = pack(resume(co, ...))
                    if not result[1] then error(result[2], 0) end
                    return unpack(result, 2, count)
                end
            end
            "##,
        )
        .set_name("trusted-coroutine-wrap")
        .call((create, resume, unpack, select))
        .map_err(Fault::from_lua)?;
    table.raw_set("wrap", wrap).map_err(Fault::from_lua)?;
    // The script may call these functions but cannot replace the guard.
    table.set_readonly(true);
    Ok((table, depth))
}

/// Keeps `pcall`/`xpcall` for ordinary known failures while preventing any
/// protected call from turning a latched cancellation, budget exhaustion or
/// unknown effect into a normal return value.
///
/// Native `pcall` is a C boundary, so a Host call that suspends cannot yield
/// through it. This implementation runs the protected function in an mlua
/// async thread, which lets ordinary Host calls suspend and resume normally.
fn guarded_protected_call(lua: &Lua, run: &Arc<Run>, name: &str) -> Result<mlua::Function, Fault> {
    let xpcall = name == "xpcall";
    let label = if xpcall { "xpcall" } else { "pcall" };
    let run = run.clone();
    lua.create_async_function(move |lua, arguments: MultiValue| {
        let run = run.clone();
        async move {
            if let Some(fault) = run.latched_fatal() {
                return Err(mlua::Error::external(fault));
            }
            let mut values = arguments.into_iter();
            let function = match values.next() {
                Some(LuaValue::Function(function)) => function,
                _ => {
                    return Err(mlua::Error::external(Fault::new(
                        ErrorKind::Arguments,
                        format!("{label} requires a function as its first argument"),
                    )));
                }
            };
            let handler = if xpcall {
                match values.next() {
                    Some(LuaValue::Function(handler)) => Some(handler),
                    _ => {
                        return Err(mlua::Error::external(Fault::new(
                            ErrorKind::Arguments,
                            "xpcall requires a message handler",
                        )));
                    }
                }
            } else {
                None
            };
            let protected_arguments: MultiValue = values.collect();
            let outcome = function.call_async::<MultiValue>(protected_arguments).await;
            let mut result = MultiValue::new();
            match outcome {
                Ok(values) => {
                    result.push_back(LuaValue::Boolean(true));
                    result.extend(values);
                }
                Err(error) => {
                    let fault = Fault::from_lua(error.clone());
                    if fault.is_fatal() {
                        return Err(mlua::Error::external(run.fail(fault)));
                    }
                    if let Some(fault) = run.latched_fatal() {
                        return Err(mlua::Error::external(fault));
                    }
                    let message = LuaValue::String(lua.create_string(error.to_string())?);
                    let message = match handler {
                        Some(handler) => match handler.call_async::<LuaValue>(message).await {
                            Ok(value) => value,
                            Err(handler_error) => {
                                let fault = Fault::from_lua(handler_error.clone());
                                if fault.is_fatal() {
                                    return Err(mlua::Error::external(run.fail(fault)));
                                }
                                LuaValue::String(lua.create_string(handler_error.to_string())?)
                            }
                        },
                        None => message,
                    };
                    run.check().map_err(mlua::Error::external)?;
                    result.push_back(LuaValue::Boolean(false));
                    result.push_back(message);
                }
            }
            if let Some(fault) = run.latched_fatal() {
                return Err(mlua::Error::external(fault));
            }
            Ok(result)
        }
    })
    .map_err(Fault::from_lua)
}

fn coroutine_rejection() -> Fault {
    Fault::new(
        ErrorKind::Capability,
        "Host capabilities cannot be called inside a script coroutine; call them from the main script",
    )
}

pub(crate) async fn execute(
    mode: &CodeMode,
    script: &str,
    cancel: CancellationToken,
    services: Option<Arc<dyn VmServices>>,
) -> Result<ExecutionReport, CodeModeError> {
    let child = cancel.child_token();
    let _cancel_on_drop = child.clone().drop_guard();
    let run = Arc::new(Run {
        state: Mutex::new(State {
            report: ExecutionReport::default(),
            fault: None,
        }),
        limits: mode.limits.clone(),
        clock: Mutex::new(Clock {
            deadline: Instant::now() + mode.limits.timeout,
            paused: None,
        }),
        cancel: child,
        pending: Mutex::new(None),
    });
    let result = execute_vm(mode, script, run.clone(), services).await;
    // The coroutine may have ended on its interrupt while a Host or cell service
    // was suspended. Preserve that original fault before settlement can
    // encounter payload errors.
    if let Err(fault) = &result {
        run.fail(fault.clone());
    }
    let pending = run.pending.lock().expect("pending call poisoned").take();
    if let Some(mut pending) = pending {
        match pending.target {
            PendingTarget::Host(index) => {
                if pending.started {
                    let settled = settle_pending(&run, &mut pending).await;
                    let _ = record_host(&run, index, settled);
                } else {
                    // The callback ended before its first poll: no call started,
                    // so remove the audit placeholder instead of inventing an
                    // unknown outcome.
                    let mut state = run.state.lock().expect("execution state poisoned");
                    if state.report.calls.len() == index + 1 {
                        state.report.calls.pop();
                    }
                }
            }
            PendingTarget::Service => {
                if pending.started {
                    let settled = settle_pending(&run, &mut pending).await;
                    record_service(&run, settled);
                }
            }
        }
    }
    match result {
        Ok(()) => Ok(run
            .state
            .lock()
            .expect("execution state poisoned")
            .report
            .clone()),
        Err(fault) => Err(run.error(fault)),
    }
}

async fn settle_pending(
    run: &Run,
    pending: &mut PendingCall,
) -> Option<Result<Value, CapabilityError>> {
    pending.cancel.cancel();
    let deadline = *pending
        .shutdown_deadline
        .get_or_insert_with(|| tokio::time::Instant::now() + run.limits.shutdown_grace);
    // Prefer an already-ready terminal result even at the grace boundary. Only
    // this same started future is polled; a Pending poll lets the timer end grace.
    tokio::select! {
        biased;
        result = &mut pending.future => Some(result),
        _ = tokio::time::sleep_until(deadline) => None,
    }
}

async fn settle_registered(run: &Run) -> Option<Result<Value, CapabilityError>> {
    let deadline = {
        let mut registered = run.pending.lock().expect("pending call poisoned");
        let pending = registered
            .as_mut()
            .expect("started call remains registered");
        pending.cancel.cancel();
        *pending
            .shutdown_deadline
            .get_or_insert_with(|| tokio::time::Instant::now() + run.limits.shutdown_grace)
    };
    tokio::select! {
        biased;
        result = std::future::poll_fn(|cx| run.poll_pending(cx)) => Some(result),
        _ = tokio::time::sleep_until(deadline) => None,
    }
}

fn record_host(
    run: &Run,
    index: usize,
    result: Option<Result<Value, CapabilityError>>,
) -> Result<Value, Fault> {
    match result {
        Some(Ok(value)) => {
            // Commit status before conversion, cancellation and payload checks.
            {
                let mut state = run.state.lock().expect("execution state poisoned");
                state.report.calls[index].status = CallStatus::Succeeded;
                state.report.calls[index].result_omitted = true;
            }
            // Reject/drain unchecked deep values without recursive Clone/Drop.
            let value = json::from_host(value, run.remaining())?;
            run.charge(&value)?;
            {
                let mut state = run.state.lock().expect("execution state poisoned");
                state.report.calls[index].result = Some(value.clone());
                state.report.calls[index].result_omitted = false;
            }
            Ok(value)
        }
        Some(Err(CapabilityError {
            message,
            outcome_unknown,
            stop,
        })) => {
            {
                let mut state = run.state.lock().expect("execution state poisoned");
                state.report.calls[index].status = if outcome_unknown {
                    CallStatus::OutcomeUnknown
                } else {
                    CallStatus::Failed
                };
                state.report.outcome_unknown |= outcome_unknown;
                state.report.calls[index].result_omitted = true;
            }
            // The original error is charged before truncation, as on normal exit.
            run.charge(message.as_str())?;
            let message = json::bounded_message(&message);
            {
                let mut state = run.state.lock().expect("execution state poisoned");
                state.report.calls[index].error = Some(message.to_owned());
                state.report.calls[index].result_omitted = false;
            }
            run.check()?;
            Err(Fault::new(
                capability_error_kind(outcome_unknown, stop),
                message,
            ))
        }
        None => {
            let mut state = run.state.lock().expect("execution state poisoned");
            state.report.outcome_unknown = true;
            let remaining = run
                .limits
                .output_bytes
                .saturating_sub(state.report.output_bytes);
            let (message, bytes) = json::fit_message(
                "Capability cleanup grace expired; reconcile its effects before retrying",
                remaining,
            );
            state.report.output_bytes += bytes;
            state.report.calls[index].result_omitted = message.is_empty();
            state.report.calls[index].error = (!message.is_empty()).then_some(message);
            Err(Fault::new(
                ErrorKind::OutcomeUnknown,
                "Capability did not settle within shutdown grace",
            ))
        }
    }
}

fn record_service(run: &Run, result: Option<Result<Value, CapabilityError>>) {
    match result {
        Some(Ok(value)) => json::discard(value),
        Some(Err(error)) if error.outcome_unknown => {
            run.state
                .lock()
                .expect("execution state poisoned")
                .report
                .outcome_unknown = true;
        }
        Some(Err(_)) => {}
        None => {
            run.state
                .lock()
                .expect("execution state poisoned")
                .report
                .outcome_unknown = true;
        }
    }
}

async fn call_host(
    run: &Arc<Run>,
    host: Arc<dyn CapabilityHost>,
    name: String,
    arguments: Value,
    audit_arguments: Value,
) -> Result<Value, Fault> {
    run.check()?;
    let index = {
        let mut state = run.state.lock().expect("execution state poisoned");
        let index = state.report.calls.len();
        if index >= run.limits.host_calls {
            return Err(Fault::new(
                ErrorKind::HostCallLimit,
                "Host call budget exhausted",
            ));
        }
        state.report.calls.push(CapabilityCall {
            sequence: index + 1,
            name: name.clone(),
            arguments: Some(audit_arguments),
            status: CallStatus::OutcomeUnknown,
            result: None,
            result_omitted: false,
            error: None,
        });
        index
    };
    if let Err(fault) = run.check() {
        run.state
            .lock()
            .expect("execution state poisoned")
            .report
            .calls
            .pop();
        return Err(fault);
    }
    let cancel = run.cancel.child_token();
    let host_cancel = cancel.clone();
    *run.pending.lock().expect("pending call poisoned") = Some(PendingCall {
        target: PendingTarget::Host(index),
        started: false,
        cancel,
        shutdown_deadline: None,
        future: Box::pin(async move { host.call(&name, arguments, host_cancel).await }),
    });
    run.clock.lock().expect("VM clock poisoned").paused = Some(Instant::now());
    let pause = Pause(run.clone());
    let deadline = tokio::time::Instant::now() + run.limits.host_wait;
    let result = tokio::select! {
        biased;
        _ = run.cancel.cancelled() => Err(run.fail(Fault::new(ErrorKind::Cancelled, "CodeMode execution cancelled"))),
        _ = tokio::time::sleep_until(deadline) => Err(run.fail(Fault::new(ErrorKind::Timeout, "CodeMode Host wait limit reached"))),
        result = std::future::poll_fn(|cx| {
            if let Err(fault) = run.check() {
                return std::task::Poll::Ready(Err(fault));
            }
            run.poll_pending(cx).map(Ok)
        }) => result,
    };
    let started = run
        .pending
        .lock()
        .expect("pending call poisoned")
        .as_ref()
        .expect("registered pending Host")
        .started;
    if !started {
        run.pending.lock().expect("pending call poisoned").take();
        run.state
            .lock()
            .expect("execution state poisoned")
            .report
            .calls
            .pop();
        return Err(result.expect_err("a Host result requires a first poll"));
    }
    let result = match result {
        Ok(result) => Some(result),
        Err(_) => settle_registered(run).await,
    };
    let invocation = run
        .pending
        .lock()
        .expect("pending call poisoned")
        .take()
        .expect("Host settlement finished");
    drop(invocation);
    drop(pause);
    let value = record_host(run, index, result)?;
    run.check()?;
    Ok(value)
}

fn capability_error_kind(unknown: bool, stop: Option<CapabilityStop>) -> ErrorKind {
    if unknown {
        return ErrorKind::OutcomeUnknown;
    }
    match stop {
        Some(CapabilityStop::Cancelled) => ErrorKind::Cancelled,
        Some(CapabilityStop::Timeout) => ErrorKind::Timeout,
        Some(CapabilityStop::Budget) => ErrorKind::Budget,
        Some(CapabilityStop::Exited) => ErrorKind::Exited,
        None => ErrorKind::Capability,
    }
}

fn service_fault(error: CapabilityError) -> Fault {
    Fault::new(
        capability_error_kind(error.outcome_unknown, error.stop),
        error.message,
    )
}

async fn call_service(
    run: &Arc<Run>,
    services: Arc<dyn VmServices>,
    operation: VmOperation,
    arguments: Value,
) -> Result<Value, Fault> {
    // Store/Load wait on host-owned state and Yield waits for an explicit
    // resume, so neither consumes the VM calculation clock. Store/Load are
    // bounded by host_wait; Yield has no deadline of its own. Local helpers
    // stay synchronous and keep the ordinary VM deadline.
    let paused = matches!(
        operation,
        VmOperation::Store | VmOperation::Load | VmOperation::Yield
    );
    let pause = if paused {
        run.clock.lock().expect("VM clock poisoned").paused = Some(Instant::now());
        Some(Pause(run.clone()))
    } else {
        None
    };
    let host_wait = matches!(operation, VmOperation::Store | VmOperation::Load);
    let deadline = if host_wait {
        Some(tokio::time::Instant::now() + run.limits.host_wait)
    } else if operation == VmOperation::Yield {
        None
    } else {
        Some(run.clock.lock().expect("VM clock poisoned").deadline.into())
    };
    let cancel = run.cancel.child_token();
    let service_cancel = cancel.clone();
    *run.pending.lock().expect("pending call poisoned") = Some(PendingCall {
        target: PendingTarget::Service,
        started: false,
        cancel,
        shutdown_deadline: None,
        future: Box::pin(
            async move { services.invoke(operation, arguments, service_cancel).await },
        ),
    });
    let driven = tokio::select! {
        biased;
        _ = run.cancel.cancelled() => Err(run.fail(Fault::new(
            ErrorKind::Cancelled,
            "CodeMode execution cancelled",
        ))),
        _ = async {
            match deadline {
                Some(deadline) => tokio::time::sleep_until(deadline).await,
                None => std::future::pending::<()>().await,
            }
        } => Err(run.fail(Fault::new(
            ErrorKind::Timeout,
            if host_wait {
                "CodeMode Host wait limit reached"
            } else {
                "CodeMode time limit reached"
            },
        ))),
        result = std::future::poll_fn(|cx| {
            if let Err(fault) = run.check() {
                return std::task::Poll::Ready(Err(fault));
            }
            run.poll_pending(cx).map(Ok)
        }) => result,
    };
    let started = run
        .pending
        .lock()
        .expect("pending call poisoned")
        .as_ref()
        .expect("registered pending service")
        .started;
    if !started {
        // No original service poll happened, so cleanup must not dispatch it.
        run.pending.lock().expect("pending call poisoned").take();
        drop(pause);
        return Err(driven.expect_err("a service result requires a first poll"));
    }
    match driven {
        Ok(Ok(value)) => {
            let invocation = run
                .pending
                .lock()
                .expect("pending call poisoned")
                .take()
                .expect("settled service remains registered");
            drop(invocation);
            drop(pause);
            let value = json::from_host(value, run.remaining())?;
            run.charge(&value)?;
            run.check()?;
            Ok(value)
        }
        Ok(Err(error)) => {
            let invocation = run
                .pending
                .lock()
                .expect("pending call poisoned")
                .take()
                .expect("settled service remains registered");
            drop(invocation);
            drop(pause);
            if error.outcome_unknown {
                run.state
                    .lock()
                    .expect("execution state poisoned")
                    .report
                    .outcome_unknown = true;
            }
            run.charge(error.message.as_str())?;
            Err(service_fault(error))
        }
        Err(stop) => {
            // The selected branch already latched its original stop reason.
            // Poll this exact started future for the shutdown grace; a Pending
            // poll lets the timer end grace. Never dispatch or replay a new
            // service call during this interval.
            let settled = settle_registered(run).await;
            record_service(run, settled);
            let invocation = run
                .pending
                .lock()
                .expect("pending call poisoned")
                .take()
                .expect("service settlement finished");
            drop(invocation);
            drop(pause);
            Err(stop)
        }
    }
}

fn install_services(
    lua: &Lua,
    environment: &mlua::Table,
    mode: &CodeMode,
    run: &Arc<Run>,
    services: Arc<dyn VmServices>,
    coroutine_depth: Arc<AtomicUsize>,
) -> Result<(), Fault> {
    let service_run = run.clone();
    let dispatcher = lua
        .create_async_function(move |lua, (name, arguments): (String, LuaValue)| {
            let run = service_run.clone();
            let services = services.clone();
            let inside_coroutine = coroutine_depth.load(Ordering::SeqCst) > 0;
            async move {
                let action = async {
                    if inside_coroutine {
                        return Err(coroutine_rejection());
                    }
                    run.check()?;
                    let operation = match name.as_str() {
                        "store" => VmOperation::Store,
                        "load" => VmOperation::Load,
                        "output" => VmOperation::Output,
                        "exit" => VmOperation::Exit,
                        "yield" => VmOperation::Yield,
                        "definition" => VmOperation::Definition,
                        "search" => VmOperation::Search,
                        _ => {
                            return Err(Fault::new(
                                ErrorKind::Script,
                                "Unknown trusted VM service",
                            ));
                        }
                    };
                    let arguments = json::from_lua(&lua, arguments, run.remaining())?;
                    run.charge(&arguments)?;
                    run.check()?;
                    // Only the private dispatcher can choose a wait policy; a Host
                    // capability name or user argument never opts into pausing it.
                    // Store/Load and Yield pause the calculation clock; only
                    // Store/Load use host_wait. Local helpers keep the VM deadline.
                    let value = call_service(&run, services.clone(), operation, arguments).await?;
                    json::to_lua(&lua, &value)
                }
                .await;
                lua_result(&run, action)
            }
        })
        .map_err(Fault::from_lua)?;
    // This separately compiled bootstrap returns closures; its private local
    // dispatcher is not in the user source's lexical or global environment.
    let factory = lua.load(r#"
        return function(service)
            return {
                store=function(key,value,expected_version) return service('store',{key=key,value=value,expected_version=expected_version}) end,
                load=function(key) return service('load',{key=key}) end,
                text=function(value) return service('output',{type='text',text=value}) end,
                image=function(mime_type,data) return service('output',{type='image',mime_type=mime_type,data=data}) end,
                audio=function(mime_type,data) return service('output',{type='audio',mime_type=mime_type,data=data}) end,
                notify=function(message,completed,total) return service('output',{type='notify',message=message,completed=completed,total=total}) end,
                exit=function(code) if code==nil then code=0 end; return service('exit',{code=code}) end,
                yield_control=function() return service('yield',{}) end,
                toolDefinition=function(name) return service('definition',{name=name}) end,
                tool_search=function(query,limit) if query==nil then query='' end; if limit==nil then limit=16 end; return service('search',{query=query,limit=limit}) end,
            }
        end
    "#).set_name("trusted-cell-bootstrap").eval::<mlua::Function>().map_err(Fault::from_lua)?;
    let helpers: mlua::Table = factory.call(dispatcher).map_err(Fault::from_lua)?;
    for pair in helpers.pairs::<String, LuaValue>() {
        let (name, function) = pair.map_err(Fault::from_lua)?;
        environment
            .raw_set(name, function)
            .map_err(Fault::from_lua)?;
    }
    let names = lua
        .create_sequence_from(mode.catalog.keys().cloned())
        .map_err(Fault::from_lua)?;
    names.set_readonly(true);
    environment
        .raw_set("ALL_TOOLS", names)
        .map_err(Fault::from_lua)?;
    // The public API is the exact same restricted table through cayw and _G.cayw.
    // There is no second raw capability API to bypass the live observer.
    Ok(())
}

async fn execute_vm(
    mode: &CodeMode,
    script: &str,
    run: Arc<Run>,
    services: Option<Arc<dyn VmServices>>,
) -> Result<(), Fault> {
    run.check()?;
    // The compiler is outside the VM allocator. Bound source before compiling it.
    if script.len() > run.limits.memory_bytes / 4 {
        return Err(Fault::new(
            ErrorKind::MemoryLimit,
            "Script source exceeds one quarter of the VM memory budget",
        ));
    }
    let lua = Lua::new_with(
        StdLib::TABLE | StdLib::STRING | StdLib::MATH | StdLib::UTF8 | StdLib::COROUTINE,
        LuaOptions::new(),
    )
    .map_err(Fault::from_lua)?;
    lua.set_memory_limit(run.limits.memory_bytes)
        .map_err(Fault::from_lua)?;
    let interrupt = run.clone();
    lua.set_interrupt(move |_| lua_result(&interrupt, interrupt.interrupt()));

    // Pure-computation coroutines are language features. The library exposed
    // below keeps them, but the resumer records synchronous execution so Host
    // calls cannot start a future that native `resume` cannot drive.
    let (coroutine, coroutine_depth) = restricted_coroutine(&lua, &run)?;
    let pcall = guarded_protected_call(&lua, &run, "pcall")?;
    let xpcall = guarded_protected_call(&lua, &run, "xpcall")?;

    // Environment access, arbitrary loading and debug facilities stay out.
    // Protected calls and coroutines are installed above with their guards.
    for name in [
        "os",
        "io",
        "package",
        "require",
        "load",
        "loadstring",
        "loadfile",
        "dofile",
        "process",
        "debug",
        "getfenv",
        "setfenv",
        "print",
        "warn",
        "collectgarbage",
        "newproxy",
        "gcinfo",
        "_VERSION",
    ] {
        lua.globals()
            .raw_set(name, LuaValue::Nil)
            .map_err(Fault::from_lua)?;
    }
    lua.globals()
        .raw_set("coroutine", coroutine)
        .map_err(Fault::from_lua)?;
    lua.globals()
        .raw_set("pcall", pcall)
        .map_err(Fault::from_lua)?;
    lua.globals()
        .raw_set("xpcall", xpcall)
        .map_err(Fault::from_lua)?;
    let api = lua.create_table().map_err(Fault::from_lua)?;
    api.raw_set("null", lua.null()).map_err(Fault::from_lua)?;
    api.raw_set(
        "array",
        lua.create_function(|lua, value: Option<mlua::Table>| {
            let table = value.map_or_else(|| lua.create_table(), Ok)?;
            if table.metatable().is_some() {
                return Err(mlua::Error::external(Fault::new(
                    ErrorKind::Conversion,
                    "cayw.array requires a plain table",
                )));
            }
            table.set_metatable(Some(lua.array_metatable()))?;
            Ok(table)
        })
        .map_err(Fault::from_lua)?,
    )
    .map_err(Fault::from_lua)?;

    let emit_run = run.clone();
    let observer = services.clone();
    api.raw_set(
        "emit",
        lua.create_function(move |lua, values: MultiValue| {
            let action = || {
                emit_run.check()?;
                if values.len() != 1 {
                    return Err(Fault::new(
                        ErrorKind::Conversion,
                        "cayw.emit requires exactly one JSON value",
                    ));
                }
                let value = json::from_lua(
                    lua,
                    values.into_iter().next().expect("one value"),
                    emit_run.remaining(),
                )?;
                emit_run.charge(&value)?;
                if let Some(observer) = &observer {
                    observer.emit(value).map_err(service_fault)?;
                } else {
                    emit_run
                        .state
                        .lock()
                        .expect("execution state poisoned")
                        .report
                        .outputs
                        .push(value);
                }
                emit_run.check()
            };
            lua_result(&emit_run, action())
        })
        .map_err(Fault::from_lua)?,
    )
    .map_err(Fault::from_lua)?;

    let discover_run = run.clone();
    let catalog = mode.catalog.clone();
    api.raw_set(
        "discover",
        lua.create_function(move |lua, ()| {
            let action = || {
                discover_run.check()?;
                let names = Value::Array(catalog.keys().cloned().map(Value::String).collect());
                discover_run.charge(&names)?;
                json::to_lua(lua, &names)
            };
            lua_result(&discover_run, action())
        })
        .map_err(Fault::from_lua)?,
    )
    .map_err(Fault::from_lua)?;

    let describe_run = run.clone();
    let catalog = mode.catalog.clone();
    api.raw_set("describe", lua.create_function(move |lua, name: String| {
        let action = || {
            describe_run.check()?;
            let capability = catalog.get(&name).ok_or_else(||Fault::new(ErrorKind::UnknownCapability, format!("Unknown capability: {name}")))?;
            let definition = &capability.definition;
            let value = json_value!({"name":definition.name,"description":definition.description,"parameters":definition.parameters});
            describe_run.charge(&value)?;
            json::to_lua(lua, &value)
        };
        lua_result(&describe_run, action())
    }).map_err(Fault::from_lua)?).map_err(Fault::from_lua)?;

    let host = mode.host.clone();
    let catalog = mode.catalog.clone();
    let call_run = run.clone();
    let call_depth = coroutine_depth.clone();
    api.raw_set(
        "call",
        lua.create_async_function(move |lua, values: MultiValue| {
            let run = call_run.clone();
            let host = host.clone();
            let catalog = catalog.clone();
            // Read the resumer state synchronously: mlua polls this future once
            // before returning control to `coroutine.resume`, so the rejection
            // must already be part of that first poll.
            let inside_coroutine = call_depth.load(Ordering::SeqCst) > 0;
            async move {
                let execute = async {
                    if inside_coroutine {
                        return Err(coroutine_rejection());
                    }
                    run.check()?;
                    let mut values = values.into_iter();
                    let name = match values.next() {
                        Some(LuaValue::String(name)) => name
                            .to_str()
                            .map_err(|_| {
                                Fault::new(ErrorKind::Arguments, "Capability name must be UTF-8")
                            })?
                            .to_owned(),
                        _ => {
                            return Err(Fault::new(
                                ErrorKind::Arguments,
                                "cayw.call requires a string capability name",
                            ));
                        }
                    };
                    let arguments = values.next().ok_or_else(|| {
                        Fault::new(
                            ErrorKind::Arguments,
                            "cayw.call requires an arguments object",
                        )
                    })?;
                    if values.next().is_some() {
                        return Err(Fault::new(
                            ErrorKind::Arguments,
                            "cayw.call accepts exactly two arguments",
                        ));
                    }
                    let capability = catalog.get(&name).ok_or_else(|| {
                        Fault::new(
                            ErrorKind::UnknownCapability,
                            format!("Unknown capability: {name}"),
                        )
                    })?;
                    let arguments = json::from_lua(&lua, arguments, run.remaining())?;
                    capability
                        .validator
                        .validate(&arguments)
                        .map_err(|error| Fault::new(ErrorKind::Arguments, error.to_string()))?;
                    run.charge(&arguments)?;
                    // Conversion, validation, accounting and the audit copy are all
                    // synchronous work; any of them can consume the remaining time.
                    let audit_arguments = arguments.clone();
                    let value = call_host(&run, host, name, arguments, audit_arguments).await?;
                    json::to_lua(&lua, &value)
                }
                .await;
                lua_result(&run, execute)
            }
        })
        .map_err(Fault::from_lua)?,
    )
    .map_err(Fault::from_lua)?;

    let environment = lua.create_table().map_err(Fault::from_lua)?;
    if let Some(services) = services {
        install_services(
            &lua,
            &environment,
            mode,
            &run,
            services,
            coroutine_depth.clone(),
        )?;
    }
    api.set_readonly(true);
    environment.raw_set("cayw", api).map_err(Fault::from_lua)?;
    lua.array_metatable().set_readonly(true);
    for pair in lua.globals().pairs::<String, LuaValue>() {
        let (name, value) = pair.map_err(Fault::from_lua)?;
        if name != "_G" && name != "cayw" {
            environment.raw_set(name, value).map_err(Fault::from_lua)?;
        }
    }
    environment
        .raw_set("_G", environment.clone())
        .map_err(Fault::from_lua)?;
    lua.sandbox(true).map_err(Fault::from_lua)?;
    // The environment belongs to this one execution and is writable: scripts may
    // use globals and global functions. Host tables copied here (`cayw`,
    // `ALL_TOOLS`, builtin libraries) were made read-only before the copy.
    run.check()?;
    let values = lua
        .load(script)
        .set_environment(environment)
        .set_name("codemode")
        .eval_async::<MultiValue>()
        .await
        .map_err(Fault::from_lua)?;
    run.check()?;
    if values.len() > 1 {
        return Err(Fault::new(
            ErrorKind::Conversion,
            "CodeMode scripts may return at most one JSON value",
        ));
    }
    if let Some(value) = values.into_iter().next() {
        let value = json::from_lua(&lua, value, run.remaining())?;
        run.charge(&value)?;
        run.state
            .lock()
            .expect("execution state poisoned")
            .report
            .return_value = value;
    }
    run.check()
}
