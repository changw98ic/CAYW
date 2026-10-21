//! Per-call Bridge scopes and their canonical, lease-fenced dispatch evidence.
use crate::{Runtime, RuntimeConfig, RuntimeError, cells::CallScope, lock};
use async_trait::async_trait;
use cayw_ai::CancellationToken;
use cayw_bridge::{
    BindingSnapshot, DispatchDropState, DispatchJournal, DispatchOutcome, DispatchRecord,
    InvocationContext, Operation,
};
use cayw_codemode::CapabilityError;
use cayw_core::{
    AttemptLease, CapabilityAuthorization, CapabilityCallKey, CapabilityCallRecord,
    CapabilityOutcome, CapabilityReconciliation, CapabilityStage, CoreError, CoreEvent,
    EventRecord, ExecutionRecord, SessionSnapshot, SessionStore,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

const PREPARED: &str = "runtime_bridge_prepared";
const SETTLED: &str = "runtime_bridge_settled";

pub(crate) fn is_external(name: &str) -> bool {
    matches!(
        name,
        "mcp.discover"
            | "mcp.describe"
            | "mcp.call"
            | "skills.list"
            | "skills.read"
            | "search.query"
    )
}

struct CatalogOnly;
#[async_trait]
impl cayw_host::ExternalHost for CatalogOnly {
    async fn call(
        &self,
        _: &str,
        _: Value,
        _: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        Err(CapabilityError::new(
            "Bridge execution requires its original per-invocation scope",
        ))
    }
}

pub(crate) fn effective_host_config(config: &RuntimeConfig) -> cayw_host::HostConfig {
    let mut host = config.host.clone();
    if config.bridge.is_some() {
        host.external = Some(Arc::new(CatalogOnly));
    }
    host
}

pub(crate) fn validate_config(config: &RuntimeConfig) -> Result<(), RuntimeError> {
    let Some(bridge) = &config.bridge else {
        return Ok(());
    };
    if config.host.external.is_some() {
        return Err(RuntimeError::InvalidInput("configure either bridge or host.external; their execution authority is ambiguous together".into()));
    }
    // Leave room for the exact call authorization and dispatch envelope. A large
    // catalog must be reduced by the trusted host before accepting any task.
    let bindings = serde_json::to_value(bridge.binding_snapshot())?;
    if serde_json::to_vec(&bindings)?.len() > cayw_core::MAX_METADATA_BYTES / 4 {
        return Err(RuntimeError::InvalidInput(
            "Bridge bindings exceed the durable Core metadata budget".into(),
        ));
    }
    metadata_bounds(&bindings).map_err(|error| RuntimeError::InvalidInput(error.to_string()))
}

pub(crate) fn permission_policy_id(
    config: &RuntimeConfig,
    policy: &str,
) -> Result<String, RuntimeError> {
    if config.bridge.is_none() {
        return Ok(policy.into());
    }
    // Preserve RequestStore's policy identifier contract before composing pins.
    // Hashing must not accidentally make an invalid host identifier acceptable.
    cayw_requests::HostActor::new(policy)?;
    let bindings = config
        .bridge
        .as_ref()
        .expect("configured Bridge")
        .binding_snapshot();
    let identity = serde_json::to_string(&(policy, bindings))?;
    Ok(format!(
        "runtime-bridge-{}",
        cayw_bridge::content_sha256(&identity)
    ))
}

/// Original immutable Core origin, including the worker fence and full catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BridgeDispatchOrigin {
    pub lease: AttemptLease,
    pub task_id: String,
    pub key: CapabilityCallKey,
    pub parent_call_id: String,
    pub source_hash: String,
    pub bindings: BindingSnapshot,
}
impl BridgeDispatchOrigin {
    pub(crate) fn new(call: &CapabilityCallRecord, config: &RuntimeConfig) -> Option<Self> {
        let bridge = config.bridge.as_ref().filter(|_| is_external(&call.name))?;
        Some(Self {
            lease: call.lease.clone(),
            task_id: call.task_id.clone(),
            key: call.key.clone(),
            parent_call_id: call.parent_call_id.clone(),
            source_hash: call.source_hash.clone(),
            bindings: bridge.binding_snapshot(),
        })
    }
    fn context(&self) -> Result<InvocationContext, CapabilityError> {
        Ok(InvocationContext {
            session_id: self.lease.session_id.clone(),
            task_id: self.task_id.clone(),
            attempt_id: self.lease.attempt_id.clone(),
            execution_id: self.key.execution_id.clone(),
            sequence: self
                .key
                .sequence
                .checked_add(1)
                .ok_or_else(|| CapabilityError::new("Bridge sequence exhausted"))?,
        })
    }
}

/// One event from the canonical Core cursor. None means preparation committed;
/// it is conservatively unknown after a crash until an exact terminal is present.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BridgeDispatchEvent {
    pub cursor: u64,
    pub origin: BridgeDispatchOrigin,
    pub record: DispatchRecord,
    pub outcome: Option<DispatchOutcome>,
}

/// Cursor advances over all scanned Core events, including non-Bridge events.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BridgeDispatchPage {
    pub events: Vec<BridgeDispatchEvent>,
    pub next_cursor: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Evidence {
    origin: BridgeDispatchOrigin,
    record: DispatchRecord,
    outcome: Option<DispatchOutcome>,
}

pub(crate) fn scoped_host(
    base: &cayw_host::Host,
    scope: &Arc<CallScope>,
) -> Result<cayw_host::Host, CapabilityError> {
    let gate = Arc::new(crate::permissions::Gate {
        scope: scope.clone(),
    });
    let Some(origin) = &scope.bridge_origin else {
        return base
            .scoped(gate, scope.clone())
            .map(|host| host.with_process_observer(scope.clone()));
    };
    let bridge = scope
        .context
        .inner
        .config
        .bridge
        .as_ref()
        .expect("origin requires Bridge");
    let journal = Arc::new(Journal {
        scope: scope.clone(),
        origin: origin.clone(),
        state: Mutex::default(),
    });
    let external = bridge.scoped(&scope.name, origin.context()?, journal)?;
    base.scoped_external(gate, scope.clone(), external)
        .map(|host| host.with_process_observer(scope.clone()))
}

#[derive(Default)]
struct JournalState {
    record: Option<Value>,
    terminal: Option<Value>,
}
struct Journal {
    scope: Arc<CallScope>,
    origin: BridgeDispatchOrigin,
    state: Mutex<JournalState>,
}
impl Journal {
    fn write(
        &self,
        record: &DispatchRecord,
        outcome: Option<&DispatchOutcome>,
    ) -> Result<(), CapabilityError> {
        let context = &self.scope.context;
        let result = (|| {
            // Check even exact duplicate writes; recovery permanently fences the
            // old owner. append_host_event repeats this inside its transaction.
            context.inner.store.validate_lease(&self.origin.lease)?;
            if !self.scope.has_started() {
                return Err(corrupt("Bridge dispatch precedes Host start"));
            }
            verify_record(
                &self.origin,
                &self.scope.name,
                &self.scope.arguments,
                record,
            )?;
            verify_outcome(record, outcome)?;
            let encoded = serde_json::to_value(record)?;
            let terminal = outcome.map(serde_json::to_value).transpose()?;
            let mut state = lock(&self.state);
            if state.record.as_ref().is_some_and(|old| old != &encoded) {
                return Err(corrupt(
                    "Bridge scope cannot change its immutable dispatch record",
                ));
            }
            if let Some(prior) = &state.terminal {
                if terminal.as_ref() == Some(prior) || terminal.is_none() {
                    // A repeated preparation cannot erase an already committed
                    // terminal. It confirms the same immutable record only.
                    return Ok(());
                }
                return Err(corrupt(
                    "Bridge dispatch already has a different durable terminal",
                ));
            }
            if state.record.is_none()
                && outcome
                    .is_some_and(|outcome| !matches!(outcome, DispatchOutcome::NotExecuted { .. }))
            {
                return Err(corrupt("Bridge terminal requires committed preparation"));
            }
            if state.record.is_some() && outcome.is_none() {
                return Ok(());
            }
            let data = serde_json::to_value(Evidence {
                origin: self.origin.clone(),
                record: record.clone(),
                outcome: outcome.cloned(),
            })?;
            metadata_bounds(&data)?;
            context.inner.store.append_host_event(
                &self.origin.lease,
                if outcome.is_some() { SETTLED } else { PREPARED },
                data,
            )?;
            // Memory only enforces this scope's transitions. It is changed after
            // the synchronous FULL commit; the Core log is the sole journal.
            state.record = Some(encoded);
            state.terminal = terminal;
            context.inner.changed.notify_waiters();
            Ok(())
        })();
        result.map_err(|error: RuntimeError| context.persistence_failed(error))
    }
}
#[async_trait]
impl DispatchJournal for Journal {
    async fn started(&self, record: &DispatchRecord) -> Result<(), CapabilityError> {
        self.write(record, None)
    }
    async fn settled(
        &self,
        record: &DispatchRecord,
        outcome: &DispatchOutcome,
    ) -> Result<(), CapabilityError> {
        self.write(record, Some(outcome))
    }
    fn dropped(
        &self,
        record: &DispatchRecord,
        state: DispatchDropState,
    ) -> Result<(), CapabilityError> {
        let outcome = match state {
            DispatchDropState::NotExecuted => DispatchOutcome::NotExecuted {
                message: "Bridge future stopped before source dispatch".into(),
            },
            DispatchDropState::Unknown => DispatchOutcome::Unknown {
                message: "Bridge future stopped after source dispatch; inspect original effects"
                    .into(),
            },
        };
        self.write(record, Some(&outcome))
    }
}

fn metadata_bounds(value: &Value) -> Result<(), RuntimeError> {
    let mut pending = vec![(value, 0usize)];
    let mut nodes = 0usize;
    while let Some((value, depth)) = pending.pop() {
        nodes += 1;
        if depth > cayw_core::MAX_METADATA_DEPTH || nodes > cayw_core::MAX_METADATA_NODES {
            return Err(corrupt(
                "Bridge evidence exceeds Core metadata shape bounds",
            ));
        }
        match value {
            Value::Array(values) => pending.extend(values.iter().map(|value| (value, depth + 1))),
            Value::Object(values) => {
                pending.extend(values.values().map(|value| (value, depth + 1)))
            }
            _ => {}
        }
    }
    if serde_json::to_vec(value)?.len() > cayw_core::MAX_METADATA_BYTES {
        return Err(corrupt("Bridge evidence exceeds Core metadata byte bound"));
    }
    Ok(())
}
fn corrupt(message: &str) -> RuntimeError {
    CoreError::CorruptStore(message.into()).into()
}

fn verify_record(
    origin: &BridgeDispatchOrigin,
    name: &str,
    arguments: &Value,
    record: &DispatchRecord,
) -> Result<(), RuntimeError> {
    let expected = origin
        .context()
        .map_err(|error| corrupt(&error.to_string()))?;
    let source = &record.key.source;
    if record.context != expected
        || record.capability != name
        || record.key.id == 0
        || source.registry_incarnation != origin.bindings.registry_incarnation
        || origin.bindings.sources.get(&source.source.id) != Some(source)
        || record.arguments_sha256
            != cayw_bridge::content_sha256(&serde_json::to_string(&record.arguments)?)
    {
        return Err(corrupt(
            "Bridge dispatch differs from its immutable origin, pins or argument digest",
        ));
    }
    let matches = match name {
        "mcp.call" => {
            record.key.operation == Operation::Invoke
                && arguments["server"].as_str() == Some(source.source.id.as_str())
                && arguments["tool"].as_str() == Some(record.key.target.as_str())
                && arguments.get("arguments") == Some(&record.arguments)
        }
        "skills.read" => {
            record.key.operation == Operation::ReadSkill
                && arguments["name"].as_str()
                    == Some(format!("{}/{}", source.source.id, record.key.target).as_str())
                && record.arguments == serde_json::json!({})
        }
        "search.query" => {
            record.key.operation == Operation::Search
                && record.key.target == "search.query"
                && &record.arguments == arguments
        }
        _ => false,
    };
    if !matches {
        return Err(corrupt(
            "Bridge source operation differs from original Core capability arguments",
        ));
    }
    Ok(())
}

fn verify_outcome(
    record: &DispatchRecord,
    outcome: Option<&DispatchOutcome>,
) -> Result<(), RuntimeError> {
    if let Some(DispatchOutcome::Succeeded { value }) = outcome {
        let key = serde_json::to_value(&record.key)?;
        let source = serde_json::to_value(&record.key.source)?;
        if value.as_object().is_none_or(|object| object.len() != 3)
            || value.get("request") != Some(&key)
            || value.get("source") != Some(&source)
            || value.get("value").is_none()
        {
            return Err(corrupt(
                "Bridge successful terminal differs from its exact source request",
            ));
        }
    }
    Ok(())
}

struct Dispatch {
    prepared: bool,
    event: BridgeDispatchEvent,
}
/// Reconstruct only from the canonical log, checking ordering and complete Core
/// origins. An arbitrary Host event kind or a changed projection is no evidence.
#[derive(Default)]
pub(crate) struct DispatchLog {
    leases: BTreeMap<String, AttemptLease>,
    executions: BTreeMap<String, ExecutionRecord>,
    calls: BTreeMap<CapabilityCallKey, CapabilityCallRecord>,
    dispatches: BTreeMap<CapabilityCallKey, Dispatch>,
    request_ids: BTreeSet<(String, u64)>,
    repairable: BTreeSet<CapabilityCallKey>,
}
impl DispatchLog {
    pub(crate) fn observe(
        &mut self,
        event: &EventRecord,
    ) -> Result<Option<BridgeDispatchEvent>, RuntimeError> {
        match &event.event {
            CoreEvent::AttemptStarted { lease } => {
                self.leases.insert(lease.attempt_id.clone(), lease.clone());
            }
            CoreEvent::ExecutionStarted { execution }
            | CoreEvent::ExecutionYielded { execution }
            | CoreEvent::ExecutionFinished { execution } => {
                self.executions
                    .insert(execution.execution_id.clone(), execution.clone());
            }
            CoreEvent::CapabilityPlanned { call } | CoreEvent::CapabilityStarted { call } => {
                self.calls.insert(call.key.clone(), call.clone());
            }
            CoreEvent::CapabilitySettled { call }
            | CoreEvent::CapabilityReconciled { call, .. } => {
                self.repairable.remove(&call.key);
                self.calls.insert(call.key.clone(), call.clone());
            }
            CoreEvent::AttemptRecovered {
                capabilities,
                executions,
                ..
            } => {
                for call in capabilities {
                    if self.calls.get(&call.key).is_some_and(|old| {
                        old.stage == CapabilityStage::Started && old.outcome.is_none()
                    }) {
                        self.repairable.insert(call.key.clone());
                    }
                    self.calls.insert(call.key.clone(), call.clone());
                }
                for execution in executions {
                    self.executions
                        .insert(execution.execution_id.clone(), execution.clone());
                }
            }
            CoreEvent::Host { kind, data } if kind == PREPARED || kind == SETTLED => {
                let evidence: Evidence = serde_json::from_value(data.clone())?;
                if evidence.outcome.is_some() != (kind == SETTLED)
                    || event.attempt.as_ref() != Some(&evidence.origin.lease)
                    || event.session_id != evidence.origin.lease.session_id
                {
                    return Err(corrupt(
                        "Bridge evidence has an invalid canonical event envelope",
                    ));
                }
                let call = self
                    .calls
                    .get(&evidence.origin.key)
                    .ok_or_else(|| corrupt("Bridge evidence has no original Core call"))?;
                if call.stage != CapabilityStage::Started || call.outcome.is_some() {
                    return Err(corrupt(
                        "Bridge evidence must precede its original Core terminal",
                    ));
                }
                self.verify_origin(&evidence.origin, call)?;
                verify_record(
                    &evidence.origin,
                    &call.name,
                    &call.arguments,
                    &evidence.record,
                )?;
                verify_outcome(&evidence.record, evidence.outcome.as_ref())?;
                let key = evidence.origin.key.clone();
                let output = BridgeDispatchEvent {
                    cursor: event.cursor,
                    origin: evidence.origin,
                    record: evidence.record,
                    outcome: evidence.outcome,
                };
                if let Some(prior) = self.dispatches.get_mut(&key) {
                    if prior.event.origin != output.origin
                        || serde_json::to_value(&prior.event.record)?
                            != serde_json::to_value(&output.record)?
                    {
                        return Err(corrupt("Bridge terminal changed the original dispatch"));
                    }
                    if prior.event.outcome.is_some() || output.outcome.is_none() || !prior.prepared
                    {
                        return Err(corrupt(
                            "Bridge journal has duplicate or reordered transitions",
                        ));
                    }
                    prior.event = output.clone();
                } else {
                    if output.outcome.as_ref().is_some_and(|outcome| {
                        !matches!(outcome, DispatchOutcome::NotExecuted { .. })
                    }) {
                        return Err(corrupt("Bridge terminal has no committed preparation"));
                    }
                    if !self.request_ids.insert((
                        output.record.key.source.registry_incarnation.clone(),
                        output.record.key.id,
                    )) {
                        return Err(corrupt(
                            "Bridge request identity was reused by another invocation",
                        ));
                    }
                    self.dispatches.insert(
                        key,
                        Dispatch {
                            prepared: output.outcome.is_none(),
                            event: output.clone(),
                        },
                    );
                }
                return Ok(Some(output));
            }
            _ => {}
        }
        Ok(None)
    }
    fn verify_origin(
        &self,
        origin: &BridgeDispatchOrigin,
        call: &CapabilityCallRecord,
    ) -> Result<(), RuntimeError> {
        let execution = self
            .executions
            .get(&origin.key.execution_id)
            .ok_or_else(|| corrupt("Bridge evidence has no original Core execution"))?;
        let proof: CapabilityAuthorization = serde_json::from_value(
            call.authorization
                .clone()
                .ok_or_else(|| corrupt("Bridge call has no authorization"))?,
        )?;
        let bindings = serde_json::to_value(&origin.bindings)?;
        if self.leases.get(&origin.lease.attempt_id) != Some(&origin.lease)
            || call.lease != origin.lease
            || execution.lease != origin.lease
            || call.task_id != origin.task_id
            || execution.task_id != origin.task_id
            || call.parent_call_id != origin.parent_call_id
            || execution.parent_call_id != origin.parent_call_id
            || call.source_hash != origin.source_hash
            || execution.source_hash != origin.source_hash
            || proof.target != origin.key
            || proof.task_id != origin.task_id
            || proof.capability != call.name
            || proof.scope != crate::permissions::canonical_scope(&call.arguments)
            || proof.decision != cayw_core::ApprovalDecision::Approve
            || proof.external_provenance.as_ref() != Some(&bindings)
            || call.external_provenance.as_ref() != Some(&bindings)
        {
            return Err(corrupt(
                "Bridge journal does not match full original Core lease, execution and authorization",
            ));
        }
        Ok(())
    }
    pub(crate) fn validate_snapshot(&self, snapshot: &SessionSnapshot) -> Result<(), RuntimeError> {
        for (key, dispatch) in &self.dispatches {
            let call = self
                .calls
                .get(key)
                .ok_or_else(|| corrupt("Bridge call disappeared from Core log"))?;
            self.verify_origin(&dispatch.event.origin, call)?;
            verify_record(
                &dispatch.event.origin,
                &call.name,
                &call.arguments,
                &dispatch.event.record,
            )?;
            let execution = self
                .executions
                .get(&key.execution_id)
                .expect("verified original execution");
            if snapshot
                .executions
                .iter()
                .find(|item| item.execution_id == key.execution_id)
                != Some(execution)
            {
                return Err(corrupt(
                    "Bridge execution projection differs from canonical Core log",
                ));
            }
            let unresolved = call.stage != CapabilityStage::Settled
                || matches!(call.outcome, Some(CapabilityOutcome::OutcomeUnknown { .. }));
            let projected = snapshot
                .unresolved_capabilities
                .iter()
                .find(|item| item.key == *key);
            if (unresolved && projected != Some(call)) || (!unresolved && projected.is_some()) {
                return Err(corrupt(
                    "Bridge capability projection differs from canonical Core log",
                ));
            }
        }
        Ok(())
    }
    pub(crate) fn tracks_call(&self, call: &CapabilityCallRecord) -> bool {
        self.dispatches.contains_key(&call.key) && self.calls.get(&call.key) == Some(call)
    }
    pub(crate) fn repair_known(&self, store: &SessionStore, id: &str) -> Result<(), RuntimeError> {
        for (key, dispatch) in &self.dispatches {
            let call = self.calls.get(key).expect("validated Bridge call");
            if !(self.repairable.contains(key)
                || call.stage == CapabilityStage::Started && call.outcome.is_none())
            {
                continue;
            }
            let outcome = match dispatch.event.outcome.as_ref() {
                Some(DispatchOutcome::Succeeded { value }) => CapabilityOutcome::Completed {
                    result: value.clone(),
                },
                Some(DispatchOutcome::Failed { message }) => CapabilityOutcome::Failed {
                    error: crate::cells::bounded(message),
                },
                Some(DispatchOutcome::NotExecuted { message }) => CapabilityOutcome::NotExecuted {
                    reason: crate::cells::bounded(message),
                },
                Some(DispatchOutcome::Unknown { .. }) | None => continue,
            };
            store.reconcile_capability(id, CapabilityReconciliation {
                key: key.clone(), actor: "runtime-bridge-recovery".into(),
                evidence: format!("Exact original Runtime Bridge terminal committed at Core cursor {}; full lease, parent, source hash, bindings and arguments verified against original Core events", dispatch.event.cursor),
                outcome, external_provenance: call.external_provenance.clone(),
            })?;
        }
        Ok(())
    }
}

pub(crate) fn read_log(
    store: &SessionStore,
    id: &str,
    snapshot: &SessionSnapshot,
) -> Result<DispatchLog, RuntimeError> {
    let (log, _) = read_page(store, id, snapshot, snapshot.cursor, 1)?;
    Ok(log)
}
fn read_page(
    store: &SessionStore,
    id: &str,
    snapshot: &SessionSnapshot,
    after: u64,
    limit: usize,
) -> Result<(DispatchLog, BridgeDispatchPage), RuntimeError> {
    if limit == 0 || limit > cayw_core::MAX_EVENT_PAGE {
        return Err(CoreError::InvalidLimit {
            limit,
            max: cayw_core::MAX_EVENT_PAGE,
        }
        .into());
    }
    if after > snapshot.cursor {
        return Err(CoreError::FutureCursor {
            after,
            current: snapshot.cursor,
        }
        .into());
    }
    let end = after.saturating_add(limit as u64).min(snapshot.cursor);
    let mut log = DispatchLog::default();
    let mut cursor = 0;
    let mut events = Vec::new();
    while cursor < snapshot.cursor {
        for event in store.events(
            id,
            cursor,
            (snapshot.cursor - cursor).min(cayw_core::MAX_EVENT_PAGE as u64) as usize,
        )? {
            cursor = event.cursor;
            if let Some(entry) = log.observe(&event)?
                && entry.cursor > after
                && entry.cursor <= end
            {
                events.push(entry);
            }
        }
    }
    log.validate_snapshot(snapshot)?;
    Ok((
        log,
        BridgeDispatchPage {
            events,
            next_cursor: end,
        },
    ))
}
impl Runtime {
    /// Read the source dispatch journal using the canonical Core cursor. This
    /// never inspects current registry state or creates a replacement consumer.
    pub fn bridge_dispatches(
        &self,
        id: &str,
        after: u64,
        limit: usize,
    ) -> Result<BridgeDispatchPage, RuntimeError> {
        let snapshot = self.inner.store.snapshot(id)?;
        // Return entries from the same verified prefix. A concurrent append may
        // appear on the next page but cannot bypass this page's validation.
        let (_, page) = read_page(&self.inner.store, id, &snapshot, after, limit)?;
        Ok(page)
    }
}
