use crate::{bounds::*, model::*};
use cayw_ai::{ArgumentValidator, CancellationToken};
use cayw_codemode::CapabilityError;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, hash_map::RandomState},
    hash::{BuildHasher, Hasher},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Instant,
};

struct Tool {
    spec: ToolSpec,
    input: ArgumentValidator,
    output: Option<ArgumentValidator>,
}
struct Metadata {
    provenance: Provenance,
    tools: BTreeMap<String, Tool>,
    skills: BTreeMap<String, SkillSpec>,
    search: bool,
    bytes: usize,
}
struct Owner {
    handle: Box<dyn SourceHandle>,
    closed: AtomicBool,
}
impl Owner {
    fn close(&self) {
        if !self.closed.swap(true, Ordering::AcqRel) {
            self.handle.disconnect();
        }
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        self.close();
    }
}
struct Source {
    metadata: Arc<Metadata>,
    generation: u64,
    status: SourceStatus,
    owner: Option<Arc<Owner>>,
    cancel: CancellationToken,
    operations: usize,
    bytes: usize,
    in_flight: usize,
    unknown: bool,
    unresolved: BTreeMap<u64, UnresolvedCall>,
}
#[derive(Default)]
struct State {
    sources: BTreeMap<String, Source>,
    owner_keys: BTreeSet<String>,
    metadata_bytes: usize,
    next_id: u64,
}
struct Inner {
    incarnation: String,
    journal_failure: Mutex<Option<String>>,
    limits: Limits,
    policy: Arc<dyn RegistryPolicy>,
    state: Mutex<State>,
}

/// One registry belongs to the application, shared by all its consumers. It
/// owns injected source handles; it never connects, loads, retries, or replays.
#[derive(Clone)]
pub struct Registry(Arc<Inner>);

struct Capture {
    metadata: Arc<Metadata>,
    owner: Arc<Owner>,
    cancel: CancellationToken,
}

impl Registry {
    pub fn new(limits: Limits, policy: Arc<dyn RegistryPolicy>) -> Result<Self, CapabilityError> {
        let positive = [
            limits.max_sources,
            limits.max_tools_per_source,
            limits.max_skills_per_source,
            limits.max_metadata_bytes,
            limits.max_all_metadata_bytes,
            limits.max_argument_bytes,
            limits.max_result_bytes,
            limits.max_skill_bytes,
            limits.max_source_operations,
            limits.max_source_bytes,
            limits.max_in_flight,
        ];
        if positive.contains(&0)
            || limits.max_sources > 1024
            || limits.max_metadata_bytes > 16 * 1024 * 1024
            || limits.max_all_metadata_bytes > 64 * 1024 * 1024
            || limits.max_result_bytes > 16 * 1024 * 1024
            || limits.operation_timeout.is_zero()
            || Instant::now()
                .checked_add(limits.operation_timeout)
                .is_none()
        {
            return Err(failure("bridge budgets must be positive and bounded"));
        }
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let sequence = NEXT
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_add(1))
            .map_err(|_| failure("registry incarnations exhausted"))?;
        // Two OS-seeded independent hash states plus a checked process counter.
        // This is opaque identity, not an authentication secret or a trust proof.
        let random_a = RandomState::new().build_hasher().finish();
        let random_b = RandomState::new().build_hasher().finish();
        let incarnation = format!(
            "{:x}-{:016x}-{:016x}-{:016x}",
            std::process::id(),
            sequence,
            random_a,
            random_b
        );
        Ok(Self(Arc::new(Inner {
            incarnation,
            journal_failure: Mutex::new(None),
            limits,
            policy,
            state: Mutex::new(State::default()),
        })))
    }

    /// Moves a previously created handle into a new, non-duplicate source slot.
    pub fn register(
        &self,
        identity: SourceIdentity,
        manifest: Manifest,
        handle: Box<dyn SourceHandle>,
    ) -> Result<Provenance, CapabilityError> {
        self.install(identity, manifest, handle, false)
    }
    /// Only explicitly disconnected sources can reconnect. Existing consumers
    /// remain fenced; obtain a new `consumer()` to acknowledge the new manifest.
    /// Unknown effects survive a reconnect and block further work permanently.
    pub fn reconnect(
        &self,
        identity: SourceIdentity,
        manifest: Manifest,
        handle: Box<dyn SourceHandle>,
    ) -> Result<Provenance, CapabilityError> {
        self.install(identity, manifest, handle, true)
    }
    fn install(
        &self,
        identity: SourceIdentity,
        manifest: Manifest,
        handle: Box<dyn SourceHandle>,
        reconnect: bool,
    ) -> Result<Provenance, CapabilityError> {
        let owner = Arc::new(Owner {
            handle,
            closed: AtomicBool::new(false),
        });
        let key = owner.handle.ownership_key().to_owned();
        if key.is_empty() || key.len() > 256 {
            return Err(failure("invalid ownership key"));
        }
        // Claim before metadata validation. No second installer can accept the
        // same physical owner while this installer validates or rejects it.
        let claimed = { lock(&self.0.state).owner_keys.insert(key) };
        if !claimed {
            owner.closed.store(true, Ordering::Release);
            return Err(failure(
                "connection ownership key has already been consumed",
            ));
        }
        let mut metadata = self.metadata(identity, manifest)?;
        let id = metadata.provenance.source.id.clone();
        let mut state = lock(&self.0.state);
        let generation = match (state.sources.get(&id), reconnect) {
            (None, false) if state.sources.len() < self.0.limits.max_sources => 1,
            (Some(source), true) if source.status == SourceStatus::Disconnected => {
                if source.operations >= self.0.limits.max_source_operations {
                    return Err(failure("source operation budget exhausted"));
                }
                source
                    .generation
                    .checked_add(1)
                    .ok_or_else(|| failure("generation exhausted"))?
            }
            _ => {
                return Err(failure(
                    "duplicate source, source limit, or invalid reconnect",
                ));
            }
        };
        if metadata.search
            && state
                .sources
                .iter()
                .any(|(other_id, source)| other_id != &id && source.metadata.search)
        {
            return Err(failure("search.query already has an owner"));
        }
        let old_bytes = state.sources.get(&id).map_or(0, |s| s.metadata.bytes);
        let total = state
            .metadata_bytes
            .saturating_sub(old_bytes)
            .checked_add(metadata.bytes)
            .ok_or_else(|| failure("metadata budget overflow"))?;
        if total > self.0.limits.max_all_metadata_bytes {
            return Err(failure("total metadata budget exceeded"));
        }
        metadata.provenance.generation = generation;
        let provenance = metadata.provenance.clone();
        let metadata = Arc::new(metadata);
        if let Some(source) = state.sources.get_mut(&id) {
            source.metadata = metadata;
            source.generation = generation;
            source.status = SourceStatus::Connected;
            source.owner = Some(owner);
            source.cancel = CancellationToken::new();
            source.operations += 1;
        } else {
            state.sources.insert(
                id,
                Source {
                    metadata,
                    generation,
                    status: SourceStatus::Connected,
                    owner: Some(owner),
                    cancel: CancellationToken::new(),
                    operations: 0,
                    bytes: 0,
                    in_flight: 0,
                    unknown: false,
                    unresolved: BTreeMap::new(),
                },
            );
        }
        state.metadata_bytes = total;
        Ok(provenance)
    }
    fn metadata(
        &self,
        identity: SourceIdentity,
        manifest: Manifest,
    ) -> Result<Metadata, CapabilityError> {
        if !name(&identity.id)
            || identity.version.is_empty()
            || identity.version.len() > 256
            || identity.origin.is_empty()
            || identity.origin.len() > 2048
            || identity.version.chars().any(char::is_control)
            || identity.origin.chars().any(char::is_control)
            || manifest.tools.len() > self.0.limits.max_tools_per_source
            || manifest.skills.len() > self.0.limits.max_skills_per_source
        {
            return Err(failure("invalid source identity or manifest count"));
        }
        for tool in &manifest.tools {
            value_size(&tool.input_schema, self.0.limits.max_metadata_bytes)?;
            if let Some(schema) = &tool.output_schema {
                value_size(schema, self.0.limits.max_metadata_bytes)?;
            }
        }
        let bytes = size(&(&identity, &manifest), self.0.limits.max_metadata_bytes)?;
        let manifest_sha256 = digest(
            &serde_json::to_vec(&manifest).map_err(|_| failure("manifest serialization failed"))?,
        );
        let mut tools = BTreeMap::new();
        for spec in manifest.tools {
            if !name(&spec.name) || spec.description.len() > 8192 || tools.contains_key(&spec.name)
            {
                return Err(failure("invalid or duplicate tool metadata"));
            }
            strict_schema(&spec.input_schema)?;
            let input =
                ArgumentValidator::new(&spec.input_schema).map_err(|e| failure(e.to_string()))?;
            let output = spec
                .output_schema
                .as_ref()
                .map(|schema| {
                    strict_schema(schema)?;
                    ArgumentValidator::new(schema).map_err(|e| failure(e.to_string()))
                })
                .transpose()?;
            tools.insert(
                spec.name.clone(),
                Tool {
                    spec,
                    input,
                    output,
                },
            );
        }
        let mut skills = BTreeMap::new();
        for spec in manifest.skills {
            if !name(&spec.name)
                || identity.id.len() + 1 + spec.name.len() > 256
                || spec.description.len() > 8192
                || !sha256(&spec.content_sha256)
                || skills.insert(spec.name.clone(), spec).is_some()
            {
                return Err(failure("invalid or duplicate skill metadata"));
            }
        }
        Ok(Metadata {
            provenance: Provenance {
                registry_incarnation: self.0.incarnation.clone(),
                source: identity,
                generation: 0,
                manifest_sha256,
            },
            tools,
            skills,
            search: manifest.search,
            bytes,
        })
    }

    pub fn disconnect(&self, id: &str) -> Result<(), CapabilityError> {
        self.invalidate(id, false)
    }
    /// Revocation is permanent for this registry. A reconnect cannot restore it.
    pub fn revoke(&self, id: &str) -> Result<(), CapabilityError> {
        self.invalidate(id, true)
    }
    fn invalidate(&self, id: &str, revoke: bool) -> Result<(), CapabilityError> {
        let (owner, cancel) = {
            let mut state = lock(&self.0.state);
            let source = state
                .sources
                .get_mut(id)
                .ok_or_else(|| failure("source unavailable"))?;
            if source.status == SourceStatus::Revoked
                || (!revoke && source.status == SourceStatus::Disconnected)
            {
                return Ok(());
            }
            source.generation = source
                .generation
                .checked_add(1)
                .ok_or_else(|| failure("generation exhausted"))?;
            source.status = if revoke {
                SourceStatus::Revoked
            } else {
                SourceStatus::Disconnected
            };
            source.unknown |= !source.unresolved.is_empty();
            (source.owner.take(), source.cancel.clone())
        };
        cancel.cancel();
        if let Some(owner) = owner {
            owner.close();
        }
        Ok(())
    }

    pub fn snapshot(&self, id: &str) -> Option<SourceSnapshot> {
        lock(&self.0.state)
            .sources
            .get(id)
            .map(|source| SourceSnapshot {
                provenance: Provenance {
                    generation: source.generation,
                    ..source.metadata.provenance.clone()
                },
                status: source.status,
                operations: source.operations,
                bytes: source.bytes,
                outcome_unknown: source.unknown,
                unresolved: source.unresolved.values().cloned().collect(),
            })
    }

    pub(crate) fn bound_arguments(&self, value: &Value) -> Result<(), CapabilityError> {
        value_size(value, self.0.limits.max_argument_bytes).map(|_| ())
    }
    pub(crate) fn bound_result(&self, value: &Value) -> Result<(), CapabilityError> {
        value_size(value, self.0.limits.max_result_bytes).map(|_| ())
    }
    pub(crate) fn validate_pins<'a>(
        &self,
        pins: impl IntoIterator<Item = (&'a str, u64)>,
    ) -> Result<(), CapabilityError> {
        let state = lock(&self.0.state);
        for (id, generation) in pins {
            let source = state
                .sources
                .get(id)
                .ok_or_else(|| failure("source unavailable"))?;
            check_source(source, generation)?;
        }
        Ok(())
    }

    pub(crate) fn search_owner(
        &self,
        pins: &BTreeMap<String, u64>,
    ) -> Result<String, CapabilityError> {
        let state = lock(&self.0.state);
        state
            .sources
            .iter()
            .find(|(id, source)| source.metadata.search && pins.contains_key(*id))
            .map(|(id, _)| id.clone())
            .ok_or_else(|| failure("search is not configured"))
    }

    /// Captures current generations only. Cloning a consumer shares ownership;
    /// it does not initialize another connection or refresh its generation pins.
    pub fn consumer(&self) -> crate::Bridge {
        let sources = lock(&self.0.state)
            .sources
            .iter()
            .filter(|(_, source)| source.status == SourceStatus::Connected)
            .map(|(id, source)| (id.clone(), source.metadata.provenance.clone()))
            .collect();
        crate::Bridge {
            registry: self.clone(),
            bindings: BindingSnapshot {
                registry_incarnation: self.0.incarnation.clone(),
                sources,
            },
        }
    }

    fn capture(&self, id: &str, generation: u64) -> Result<Capture, CapabilityError> {
        let state = lock(&self.0.state);
        let source = state
            .sources
            .get(id)
            .ok_or_else(|| failure("source unavailable"))?;
        check_source(source, generation)?;
        Ok(Capture {
            metadata: source.metadata.clone(),
            owner: source.owner.as_ref().expect("connected owner").clone(),
            cancel: source.cancel.clone(),
        })
    }

    pub(crate) fn operation_timeout(&self) -> std::time::Duration {
        self.0.limits.operation_timeout
    }

    /// A durable hook failure fences all later registry operations.
    pub fn journal_failure(&self) -> Option<String> {
        lock(&self.0.journal_failure).clone()
    }
    fn check_journal(&self) -> Result<(), CapabilityError> {
        if self.journal_failure().is_some() {
            Err(failure(
                "bridge dispatch journal failed; runtime reconciliation required",
            ))
        } else {
            Ok(())
        }
    }
    fn journal_failed(&self, message: &str) {
        let bounded = failure(message).message;
        let mut error = lock(&self.0.journal_failure);
        if error.is_none() {
            *error = Some(bounded);
        }
    }
    fn before_dispatch(
        &self,
        id: &str,
        generation: u64,
        cancel: &CancellationToken,
        deadline: tokio::time::Instant,
    ) -> Result<(), CapabilityError> {
        self.check_journal()?;
        if cancel.is_cancelled() || tokio::time::Instant::now() >= deadline {
            return Err(failure(
                "source operation cancelled or timed out before dispatch",
            ));
        }
        self.capture(id, generation).map(|_| ())
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn operation(
        &self,
        id: &str,
        generation: u64,
        operation: Operation,
        target: &str,
        arguments: Value,
        cancel: CancellationToken,
        deadline: tokio::time::Instant,
        scope: &crate::ScopedBridge,
    ) -> Result<Value, CapabilityError> {
        self.before_dispatch(id, generation, &cancel, deadline)?;
        let capture = self.capture(id, generation)?;
        let bytes = value_size(&arguments, self.0.limits.max_argument_bytes)?;
        let child = cancel.child_token();
        let _drop_cancel = child.clone().drop_guard();
        let request = AccessRequest {
            source: capture.metadata.provenance.clone(),
            operation,
            target: target.into(),
            arguments: arguments.clone(),
        };
        let mut reservation = self.reserve(&request, bytes)?;
        let mut journal_guard: Option<JournalGuard> = None;
        let result = async {
            tokio::select! {
                biased;
                _ = child.cancelled() => return Err(failure("bridge permission cancelled")),
                _ = capture.cancel.cancelled() => return Err(failure("source invalidated before permission")),
                _ = tokio::time::sleep_until(deadline) => return Err(failure("bridge permission deadline")),
                result = self.0.policy.authorize(&request, child.clone()) => result?,
            }
            self.capture(id, generation)?;
            let visible = |kind, name: &str| self.0.policy.visible(kind, &capture.metadata.provenance, name);
            match operation {
                Operation::Discover => {
                    let mut value = json!({"tools":[]});
                    for tool in capture.metadata.tools.values().filter(|tool| visible(Visibility::ToolName, &tool.spec.name)) {
                        value["tools"].as_array_mut().expect("tool list").push(json!({"name":tool.spec.name, "source":capture.metadata.provenance}));
                        value_size(&value, self.0.limits.max_result_bytes)?;
                    }
                    Ok(value)
                }
                Operation::Describe => {
                    let tool = capture.metadata.tools.get(target).ok_or_else(|| failure("tool unavailable"))?;
                    if !visible(Visibility::ToolName, target) || !visible(Visibility::ToolSchema, target) {
                        return Err(failure("tool schema access denied"));
                    }
                    Ok(json!({"tool":tool.spec, "source":capture.metadata.provenance}))
                }
                Operation::ListSkills => {
                    let mut value = json!({"skills":[]});
                    for skill in capture.metadata.skills.values().filter(|skill| visible(Visibility::SkillName, &skill.name)) {
                        value["skills"].as_array_mut().expect("skill list").push(json!({"name":format!("{id}/{}", skill.name), "description":skill.description,
                            "content_sha256":skill.content_sha256, "source":capture.metadata.provenance}));
                        value_size(&value, self.0.limits.max_result_bytes)?;
                    }
                    Ok(value)
                }
                Operation::Invoke | Operation::ReadSkill | Operation::Search => {
                    let source_operation = match operation {
                        Operation::Invoke => {
                            let tool = capture.metadata.tools.get(target).ok_or_else(|| failure("tool unavailable"))?;
                            if !visible(Visibility::ToolName, target) || !visible(Visibility::ToolSchema, target) {
                                return Err(failure("tool access denied"));
                            }
                            tool.input.validate(&arguments).map_err(|e| failure(e.to_string()))?;
                            SourceOperation::InvokeTool { name: target.into(), arguments: arguments.clone() }
                        }
                        Operation::ReadSkill => {
                            if !capture.metadata.skills.contains_key(target)
                                || !visible(Visibility::SkillName, target) || !visible(Visibility::SkillContent, target)
                            { return Err(failure("skill content access denied")); }
                            SourceOperation::ReadSkill { name: target.into() }
                        }
                        Operation::Search => {
                            if !capture.metadata.search || !visible(Visibility::Search, "search.query") {
                                return Err(failure("search access denied"));
                            }
                            SourceOperation::Search { arguments: arguments.clone() }
                        }
                        _ => unreachable!(),
                    };
                    self.before_dispatch(id,generation,&child,deadline)?;
                    let record = DispatchRecord {
                        context:scope.context.clone(), capability:scope.capability.clone(), key:reservation.key.clone(),
                        arguments:arguments.clone(), arguments_sha256:digest(&serde_json::to_vec(&arguments).map_err(|_|failure("argument encoding failed"))?),
                    };
                    journal_guard = Some(JournalGuard { registry:self.clone(), journal:scope.journal.clone(), record,
                        state:DispatchDropState::NotExecuted, done:false });
                    let journal = journal_guard.as_mut().expect("prepared journal");
                    tokio::select! {
                        biased;
                        _ = child.cancelled() => return Err(failure("dispatch journal preparation cancelled")),
                        _ = capture.cancel.cancelled() => return Err(failure("source invalidated during dispatch journal preparation")),
                        _ = tokio::time::sleep_until(deadline) => return Err(failure("dispatch journal preparation deadline")),
                        result = journal.journal.started(&journal.record) => {
                            result.map_err(|e|failure(format!("dispatch journal preparation failed: {}",failure(e.message).message)))?;
                        }
                    }
                    // started() may synchronously consume the remaining deadline,
                    // cancel its token or invalidate/reconnect the source.
                    self.before_dispatch(id,generation,&child,deadline)?;
                    let effect = matches!(operation, Operation::Invoke | Operation::Search);
                    let key = reservation.key.clone();
                    let mut dispatched=false;
                    let reply = tokio::select! {
                        biased;
                        _ = child.cancelled() => Err(interrupted(dispatched && effect, "source operation cancelled")),
                        _ = capture.cancel.cancelled() => Err(interrupted(dispatched && effect, "source disconnected or revoked")),
                        _ = tokio::time::sleep_until(deadline) => Err(interrupted(dispatched && effect, "source operation deadline")),
                        reply = async {
                            // This branch is polled only after ready cancellation
                            // branches. No await separates this check from request().
                            self.before_dispatch(id,generation,&child,deadline)?;
                            let request=SourceRequest { key:key.clone(),operation:source_operation,
                                max_result_bytes:self.0.limits.max_result_bytes };
                            reservation.begin(effect,&journal.record.arguments_sha256)?;
                            if let Err(error)=self.before_dispatch(id,generation,&child,deadline) {
                                reservation.known();
                                return Err(failure(error.message));
                            }
                            dispatched=true;
                            journal.state=DispatchDropState::Unknown;
                            capture.owner.handle.request(request,child.clone()).await
                        } => reply,
                    };
                    let mut reply = match reply {
                        Ok(reply) if reply.key == key => reply,
                        Ok(_) => return Err(interrupted(effect, "source response does not match request ID, generation, or operation")),
                        Err(error) => {
                            return Err(error);
                        }
                    };
                    if child.is_cancelled() {
                        return Err(interrupted(effect, "source result arrived after cancellation"));
                    }
                    self.capture(id, generation).map_err(|_| interrupted(effect, "late result from invalidated source"))?;
                    value_size(&reply.value, self.0.limits.max_result_bytes).map_err(|_| interrupted(effect, "source result byte budget exceeded"))?;
                    if operation == Operation::Invoke {
                        if let Some(validator) = &capture.metadata.tools[target].output {
                            validator.validate(&reply.value).map_err(|_| interrupted(effect, "source result violates output schema"))?;
                        }
                    } else if operation == Operation::ReadSkill {
                        let content = reply.value.as_str().ok_or_else(|| failure("skill result must be UTF-8 text"))?;
                        if content.len() > self.0.limits.max_skill_bytes
                            || content_sha256(content) != capture.metadata.skills[target].content_sha256
                        { return Err(failure("skill content size or digest mismatch")); }
                    }
                    // Keep unknown retention until the fully bounded envelope is charged.
                    Ok(json!({"request":key, "source":capture.metadata.provenance, "value":reply.value.take()}))
                }
            }
        }.await;
        let mut result = result.and_then(|value| {
            let bytes = value_size(&value, self.0.limits.max_result_bytes).map_err(|_| {
                interrupted(
                    reservation.effect_pending,
                    "bridge result envelope exceeds byte budget",
                )
            })?;
            let mut state = lock(&self.0.state);
            if child.is_cancelled() || tokio::time::Instant::now() >= deadline {
                return Err(interrupted(
                    reservation.effect_pending,
                    "result acceptance cancelled or timed out",
                ));
            }
            let source = state.sources.get_mut(id).expect("reserved source");
            check_source(source, generation).map_err(|_| {
                interrupted(
                    reservation.effect_pending,
                    "source changed before result acceptance",
                )
            })?;
            if bytes > self.0.limits.max_source_bytes.saturating_sub(source.bytes) {
                return Err(interrupted(
                    reservation.effect_pending,
                    "source total byte budget exceeded",
                ));
            }
            source.bytes += bytes;
            Ok(value)
        });
        if let Some(journal) = &mut journal_guard {
            if journal.state == DispatchDropState::NotExecuted
                && let Err(error) = &mut result
            {
                // A different concurrent request can freeze the source while
                // this journal is preparing. This invocation still executed zero
                // source calls; do not copy the other request's uncertainty.
                error.outcome_unknown = false;
            }
            // A known source failure is accepted only while its original source
            // is still valid. Once accepted, persisting it must not reclassify it
            // after writing a different terminal record.
            if journal.state == DispatchDropState::Unknown
                && result.as_ref().is_err_and(|error| !error.outcome_unknown)
                && let Err(error) = self.before_dispatch(id, generation, &child, deadline)
            {
                result = Err(interrupted(reservation.effect_pending, &error.message));
            }
            if let Err(error) = &mut result {
                error.message = failure(std::mem::take(&mut error.message)).message;
            }
            let outcome = match &result {
                Ok(value) => DispatchOutcome::Succeeded {
                    value: value.clone(),
                },
                Err(error) if journal.state == DispatchDropState::NotExecuted => {
                    DispatchOutcome::NotExecuted {
                        message: error.message.clone(),
                    }
                }
                Err(error) if error.outcome_unknown => DispatchOutcome::Unknown {
                    message: error.message.clone(),
                },
                Err(error) => DispatchOutcome::Failed {
                    message: error.message.clone(),
                },
            };
            // Settlement is awaited after cancellation too, within the original
            // absolute budget. A synchronous fallback records unknown/zero execution.
            let persisted = tokio::select! {
                biased;
                _ = tokio::time::sleep_until(deadline) => Err(failure("dispatch journal terminal deadline")),
                result = journal.journal.settled(&journal.record,&outcome) => result,
            };
            match persisted {
                Ok(()) => journal.done = true,
                Err(error) => {
                    self.journal_failed(&error.message);
                    let _ = journal.fallback();
                    result = Err(interrupted(
                        journal.state == DispatchDropState::Unknown,
                        "dispatch journal terminal write failed; reconciliation required",
                    ));
                }
            }
            if result.is_ok() || result.as_ref().is_err_and(|error| !error.outcome_unknown) {
                reservation.known();
            }
            if journal.done
                && result.is_ok()
                && (child.is_cancelled() || tokio::time::Instant::now() >= deadline)
            {
                // The exact successful outcome is already durable. Return known
                // interruption and never replay; the journal retains the result.
                result = Err(failure(
                    "bridge result persisted after invocation cancellation or deadline; inspect dispatch journal",
                ));
            }
        }
        match result {
            Err(mut error) => {
                if error.outcome_unknown {
                    reservation.mark_unknown();
                }
                // Never retain a source's unbounded error string in the registry.
                if error.message.len() > 2048 {
                    error.message = "source error exceeds diagnostic bound".into();
                }
                Err(error)
            }
            success => success,
        }
    }

    fn reserve(
        &self,
        request: &AccessRequest,
        bytes: usize,
    ) -> Result<Reservation, CapabilityError> {
        let mut state = lock(&self.0.state);
        let next_id = state
            .next_id
            .checked_add(1)
            .ok_or_else(|| failure("request IDs exhausted"))?;
        let source = state
            .sources
            .get_mut(&request.source.source.id)
            .ok_or_else(|| failure("source unavailable"))?;
        check_source(source, request.source.generation)?;
        if source.operations >= self.0.limits.max_source_operations
            || source.in_flight >= self.0.limits.max_in_flight
            || bytes > self.0.limits.max_source_bytes.saturating_sub(source.bytes)
        {
            return Err(failure(
                "source operation, concurrency, or byte budget exhausted",
            ));
        }
        source.operations += 1;
        source.bytes += bytes;
        source.in_flight += 1;
        state.next_id = next_id;
        Ok(Reservation {
            registry: self.clone(),
            key: RequestKey {
                id: next_id,
                source: request.source.clone(),
                operation: request.operation,
                target: request.target.clone(),
            },
            effect_pending: false,
        })
    }
}

fn check_source(source: &Source, generation: u64) -> Result<(), CapabilityError> {
    if source.unknown {
        return Err(CapabilityError::unknown(
            "source has unresolved effects; explicit application reconciliation is required",
        ));
    }
    if source.status != SourceStatus::Connected
        || source.generation != generation
        || source.cancel.is_cancelled()
    {
        return Err(failure(
            "source disconnected, revoked, or generation is stale",
        ));
    }
    Ok(())
}
fn interrupted(effect: bool, message: &str) -> CapabilityError {
    if effect {
        CapabilityError::unknown(message)
    } else {
        failure(message)
    }
}
struct JournalGuard {
    registry: Registry,
    journal: Arc<dyn DispatchJournal>,
    record: DispatchRecord,
    state: DispatchDropState,
    done: bool,
}
impl JournalGuard {
    fn fallback(&mut self) -> Result<(), CapabilityError> {
        self.done = true;
        let result = self.journal.dropped(&self.record, self.state);
        if let Err(error) = &result {
            self.registry.journal_failed(&error.message);
        }
        result
    }
}
impl Drop for JournalGuard {
    fn drop(&mut self) {
        if !self.done {
            let _ = self.fallback();
        }
    }
}

struct Reservation {
    registry: Registry,
    key: RequestKey,
    effect_pending: bool,
}
impl Reservation {
    fn begin(&mut self, effect: bool, arguments_sha256: &str) -> Result<(), CapabilityError> {
        let mut state = lock(&self.registry.0.state);
        let source = state
            .sources
            .get_mut(&self.key.source.source.id)
            .expect("reserved source");
        check_source(source, self.key.source.generation)?;
        if effect {
            source.unresolved.insert(
                self.key.id,
                UnresolvedCall {
                    key: self.key.clone(),
                    arguments_sha256: arguments_sha256.into(),
                    outcome_unknown: true,
                },
            );
            self.effect_pending = true;
        }
        Ok(())
    }
    fn known(&mut self) {
        if self.effect_pending {
            let mut state = lock(&self.registry.0.state);
            let source = state
                .sources
                .get_mut(&self.key.source.source.id)
                .expect("reserved source");
            source.unresolved.remove(&self.key.id);
            self.effect_pending = false;
        }
    }
    fn mark_unknown(&self) {
        lock(&self.registry.0.state)
            .sources
            .get_mut(&self.key.source.source.id)
            .expect("reserved source")
            .unknown = true;
    }
}
impl Drop for Reservation {
    fn drop(&mut self) {
        let mut state = lock(&self.registry.0.state);
        let source = state
            .sources
            .get_mut(&self.key.source.source.id)
            .expect("reserved source");
        source.in_flight -= 1;
        if self.effect_pending {
            source.unknown = true;
        }
    }
}
