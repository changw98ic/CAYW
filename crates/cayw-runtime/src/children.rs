//! Local sub-agents (Issue 5) plus F12 FleetView / delegation first cut.
//!
//! One `agent.run` capability call starts exactly one real child Runtime session
//! and task in the same database. Foreground mode waits for that task's durable
//! terminal (Issue 5). Background mode returns a handle immediately and keeps the
//! child running as a detached fleet member: parent cancel / drain must not stop
//! it. `agent.fleet` / `agent.stop` / `agent.steer` expose FleetView and
//! intervention; budgets, depth and `max_concurrent_runs` still apply.
//!
//! The child gets its own Session, Task, attempt, approvals and Request identity.
//! Its permissions come only from the trusted host's `LocalAgents.permissions`;
//! model arguments cannot name, inherit or upgrade a permission. Token, capability
//! call and byte budgets are shared across the whole tree, so a child can never
//! reset its parent's spend. A child never nests beyond `max_depth`, and every
//! wait is bounded by the parent's remaining run deadline (background still shares
//! the tree absolute deadline and its own `max_time`).

use crate::{
    Inner, Runtime, RuntimeError, ScopedPermission, SessionView, TaskState,
    cells::CallScope,
    execution::RunContext,
    fleet::{AgentRunMode, FleetEntry, FleetMemberState},
    queue::{TaskLease, TaskTerminal},
};
use async_trait::async_trait;
use cayw_ai::{CancellationToken, Message, ToolDefinition};
use cayw_codemode::CapabilityError;
use cayw_host::{ExternalHost, Host};
use cayw_requests::TaskIntent;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::watch;

pub(crate) const CAPABILITY: &str = "agent.run";
pub(crate) const CAPABILITY_FLEET: &str = "agent.fleet";
pub(crate) const CAPABILITY_STOP: &str = "agent.stop";
pub(crate) const CAPABILITY_STEER: &str = "agent.steer";
const DEFAULT_LABEL: &str = "Sub-agent";
const RESULT_LABEL_BYTES: usize = 256;
const RESULT_ERROR_BYTES: usize = 256;
const CLEANUP_CAP: Duration = Duration::from_secs(60);

/// Trusted host opt-in for local sub-agents. `None` removes `agent.run` from the
/// catalog entirely; this is the only way to enable it.
#[derive(Clone, Debug)]
pub struct LocalAgents {
    /// Maximum live children in one root tree, across all depths.
    pub max_children: usize,
    /// 1 permits one child level. A child at this depth cannot start another.
    pub max_depth: usize,
    /// Cumulative provider tokens for the whole tree. Children never reset it.
    pub shared_total_tokens: u64,
    /// Total `agent.run` calls in one root tree.
    pub max_calls: usize,
    /// Serialized byte bound for one result returned to the parent's Luau cell.
    pub max_output_bytes: usize,
    /// One child's wall time, additionally capped by the parent's remaining time.
    pub max_time: Duration,
    /// The child's complete permission set. Parent grants and approvals are never
    /// consulted for the child, and model arguments cannot extend this list.
    pub permissions: Vec<ScopedPermission>,
}

impl Default for LocalAgents {
    fn default() -> Self {
        Self {
            max_children: 4,
            max_depth: 1,
            shared_total_tokens: 32_768,
            max_calls: 16,
            max_output_bytes: 256 * 1024,
            max_time: Duration::from_secs(30),
            permissions: vec![],
        }
    }
}

pub(crate) fn validate(config: Option<&LocalAgents>) -> Result<(), RuntimeError> {
    let Some(config) = config else {
        return Ok(());
    };
    if config.max_children == 0
        || config.max_children > 1024
        || config.max_depth == 0
        || config.max_depth > 32
        || config.shared_total_tokens == 0
        || config.max_calls == 0
        || config.max_calls > 100_000
        || config.max_output_bytes < 1024
        || config.max_output_bytes > 16 * 1024 * 1024
        || config.max_time < Duration::from_millis(100)
        || config.max_time > Duration::from_secs(3600)
        || std::time::Instant::now()
            .checked_add(config.max_time)
            .is_none()
    {
        return Err(RuntimeError::InvalidInput(
            "local agent limits must be positive, bounded and leave a representable deadline"
                .into(),
        ));
    }
    Ok(())
}

fn definition() -> ToolDefinition {
    ToolDefinition {
        name: CAPABILITY.into(),
        description: "Start one real local sub-agent session in this runtime. Foreground (default) waits for its durable terminal. Background returns a fleet handle immediately; the child keeps running and parent cancel does not stop it. The child has its own session, task and host-configured permissions. It cannot inherit or upgrade parent permissions, and nesting beyond the host limit is refused.".into(),
        parameters: json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["prompt"],
            "properties": {
                "prompt": {"type": "string", "minLength": 1, "maxLength": 256 * 1024},
                "label": {"type": "string", "minLength": 1, "maxLength": 1024},
                "mode": {"type": "string", "enum": ["foreground", "background"]}
            }
        }),
    }
}

fn fleet_definition() -> ToolDefinition {
    ToolDefinition {
        name: CAPABILITY_FLEET.into(),
        description: "List local sub-agents in the FleetView. Optional parent_session_id scopes the page; after/limit paginate by child_task_id.".into(),
        parameters: json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "parent_session_id": {"type": "string", "minLength": 1, "maxLength": 128},
                "after": {"type": "string", "minLength": 1, "maxLength": 128},
                "limit": {"type": "integer", "minimum": 1, "maximum": 1000}
            }
        }),
    }
}

fn stop_definition() -> ToolDefinition {
    ToolDefinition {
        name: CAPABILITY_STOP.into(),
        description:
            "Request stop for one fleet member by child_task_id. Idempotent when already terminal."
                .into(),
        parameters: json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["child_task_id"],
            "properties": {
                "child_task_id": {"type": "string", "minLength": 1, "maxLength": 128}
            }
        }),
    }
}

fn steer_definition() -> ToolDefinition {
    ToolDefinition {
        name: CAPABILITY_STEER.into(),
        description: "Append one steering user message to a running fleet member. Requires the child Agent control to be attached.".into(),
        parameters: json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["child_task_id", "text"],
            "properties": {
                "child_task_id": {"type": "string", "minLength": 1, "maxLength": 128},
                "text": {"type": "string", "minLength": 1, "maxLength": 256 * 1024}
            }
        }),
    }
}

/// Catalog entry for the trusted host. Discovery is not permission.
pub(crate) fn definitions(inner: &Inner) -> Vec<ToolDefinition> {
    if inner.config.local_agents.is_some() {
        vec![
            definition(),
            fleet_definition(),
            stop_definition(),
            steer_definition(),
        ]
    } else {
        vec![]
    }
}

pub(crate) fn is_capability(name: &str) -> bool {
    matches!(
        name,
        CAPABILITY | CAPABILITY_FLEET | CAPABILITY_STOP | CAPABILITY_STEER
    )
}

/// Shared and per-run local-agent state attached to every RunContext.
///
/// Token/call/byte counters, the agent-run counter, the live-child counter and the
/// absolute run deadline are shared by the root task and every descendant. The
/// child map is per run: a child's drain must not join its parent's siblings.
pub(crate) struct RunScope {
    tokens: Arc<AtomicU64>,
    token_limit: u64,
    calls: Arc<AtomicUsize>,
    bytes: Arc<AtomicUsize>,
    agent_runs: Arc<AtomicUsize>,
    live_children: Arc<AtomicUsize>,
    children: Mutex<BTreeMap<String, ChildHandle>>,
    depth: usize,
    permissions: Vec<ScopedPermission>,
    config: Option<LocalAgents>,
    deadline: std::time::Instant,
    max_capability_calls: usize,
    max_capability_bytes: usize,
}

impl RunScope {
    pub(crate) fn root(inner: &Inner) -> Self {
        let config = inner.config.local_agents.clone();
        let token_limit = config
            .as_ref()
            .map_or(inner.config.max_total_tokens, |config| {
                config
                    .shared_total_tokens
                    .min(inner.config.max_total_tokens)
            });
        let deadline = std::time::Instant::now()
            .checked_add(inner.config.max_run_time)
            .unwrap_or_else(|| std::time::Instant::now() + Duration::from_secs(24 * 3600));
        Self {
            tokens: Arc::new(AtomicU64::new(0)),
            token_limit,
            calls: Arc::new(AtomicUsize::new(0)),
            bytes: Arc::new(AtomicUsize::new(0)),
            agent_runs: Arc::new(AtomicUsize::new(0)),
            live_children: Arc::new(AtomicUsize::new(0)),
            children: Mutex::new(BTreeMap::new()),
            depth: 0,
            permissions: inner.config.permissions.clone(),
            config,
            deadline,
            max_capability_calls: inner.config.max_capability_calls,
            max_capability_bytes: inner.config.max_capability_bytes,
        }
    }

    pub(crate) fn child(&self, config: &LocalAgents) -> Result<Self, RuntimeError> {
        if self.depth >= config.max_depth {
            return Err(RuntimeError::InvalidInput(
                "local agent depth limit reached".into(),
            ));
        }
        Ok(Self {
            tokens: self.tokens.clone(),
            token_limit: self.token_limit,
            calls: self.calls.clone(),
            bytes: self.bytes.clone(),
            agent_runs: self.agent_runs.clone(),
            live_children: self.live_children.clone(),
            children: Mutex::new(BTreeMap::new()),
            depth: self.depth + 1,
            permissions: config.permissions.clone(),
            config: Some(config.clone()),
            deadline: self.deadline,
            max_capability_calls: self.max_capability_calls,
            max_capability_bytes: self.max_capability_bytes,
        })
    }

    pub(crate) fn depth(&self) -> usize {
        self.depth
    }

    pub(crate) fn permissions(&self) -> &[ScopedPermission] {
        &self.permissions
    }

    pub(crate) fn tokens(&self) -> &Arc<AtomicU64> {
        &self.tokens
    }

    pub(crate) fn token_limit(&self) -> u64 {
        self.token_limit
    }

    /// Absolute deadline for the whole root tree. A child never receives a fresh
    /// parent time budget.
    pub(crate) fn deadline(&self) -> std::time::Instant {
        self.deadline
    }

    pub(crate) fn remaining_time(&self) -> Duration {
        self.deadline
            .saturating_duration_since(std::time::Instant::now())
    }

    fn cleanup_bound(&self) -> Duration {
        self.config
            .as_ref()
            .map_or(CLEANUP_CAP, |config| config.max_time.min(CLEANUP_CAP))
    }

    /// Shared capability-call admission for the whole tree.
    pub(crate) fn admit_call(&self, arguments: &Value) -> Result<(), RuntimeError> {
        let count = self.calls.fetch_add(1, Ordering::AcqRel);
        if count >= self.max_capability_calls {
            return Err(RuntimeError::Budget("capability count"));
        }
        self.charge_bytes(arguments)
    }

    /// Shared capability payload accounting for the whole tree.
    pub(crate) fn charge_bytes(&self, value: &impl Serialize) -> Result<(), RuntimeError> {
        let size = serde_json::to_vec(value)?.len();
        let previous = self.bytes.fetch_add(size, Ordering::AcqRel);
        if previous.saturating_add(size) > self.max_capability_bytes {
            return Err(RuntimeError::Budget("capability byte"));
        }
        Ok(())
    }

    /// Shared budget check used by both parent and child RunContexts.
    pub(crate) fn check(&self) -> Result<(), RuntimeError> {
        if self.tokens.load(Ordering::Acquire) > self.token_limit {
            return Err(RuntimeError::Budget("token"));
        }
        if self.calls.load(Ordering::Acquire) > self.max_capability_calls {
            return Err(RuntimeError::Budget("capability count"));
        }
        if self.bytes.load(Ordering::Acquire) > self.max_capability_bytes {
            return Err(RuntimeError::Budget("capability byte"));
        }
        Ok(())
    }

    /// Counts one `agent.run` request in this tree. Rejection is immediate.
    pub(crate) fn begin_agent_run(&self, max_calls: usize) -> Result<(), RuntimeError> {
        self.agent_runs
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < max_calls).then_some(count + 1)
            })
            .map(|_| ())
            .map_err(|_| RuntimeError::InvalidInput("local agent call limit reached".into()))
    }

    fn reserve_child(&self, max_children: usize) -> Result<ChildPermit, RuntimeError> {
        self.live_children
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < max_children).then_some(count + 1)
            })
            .map(|_| ChildPermit {
                live: self.live_children.clone(),
                active: true,
            })
            .map_err(|_| RuntimeError::InvalidInput("local agent concurrency limit reached".into()))
    }

    fn insert_child(&self, task_id: String, handle: ChildHandle, mut permit: ChildPermit) {
        permit.active = false;
        let previous = self
            .children
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(task_id, handle);
        debug_assert!(previous.is_none(), "child task identity is unique");
    }

    /// Temporarily removes one child so its runner can be joined without
    /// holding the scope lock across an await. The tree-wide live slot remains
    /// charged; `ChildJoin` restores the handle unless the join is confirmed.
    fn take_child_for_join(&self, task_id: &str) -> ChildJoin<'_> {
        let handle = self
            .children
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(task_id);
        ChildJoin {
            scope: self,
            task_id: task_id.into(),
            handle,
            confirmed: false,
        }
    }

    fn restore_child(&self, task_id: String, handle: ChildHandle) {
        let previous = self
            .children
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(task_id, handle);
        debug_assert!(previous.is_none(), "child task identity is unique");
    }

    /// Releases the live slot of a child whose handle was removed for a
    /// confirmed join. The permit count is never released before that proof.
    fn release_child(&self) {
        self.live_children.fetch_sub(1, Ordering::AcqRel);
    }

    /// Shared live-child counter for FleetRegistry detached ownership.
    fn live_children_handle(&self) -> Arc<AtomicUsize> {
        self.live_children.clone()
    }

    /// Snapshot of the children still owned by this run. Used only after the
    /// run's capability futures have been dropped, so no new child can be
    /// inserted. Each child is then removed and re-inserted one at a time, so
    /// an unconfirmed handle is never consumed by a bulk take.
    fn child_ids(&self) -> Vec<String> {
        self.children
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .keys()
            .cloned()
            .collect()
    }
}

struct ChildPermit {
    live: Arc<AtomicUsize>,
    active: bool,
}

impl Drop for ChildPermit {
    fn drop(&mut self) {
        if self.active {
            self.live.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

struct ChildHandle {
    cancel: CancellationToken,
    outcome: watch::Receiver<Option<ChildOutcome>>,
    join: tokio::task::JoinHandle<()>,
    /// Tracks whether the runner `JoinHandle` may still be polled. A completed
    /// handle is never awaited twice, and a cleanly joined worker whose finish
    /// evidence was not recorded remains owned so a later drain can retry.
    join_state: JoinState,
    label: String,
    session_id: String,
    depth: usize,
    /// Background / detached children are skipped by parent drain and cancel.
    detached: bool,
}

#[derive(Clone, Debug)]
enum JoinState {
    /// The runner has not been joined; polling `join` is still valid.
    Pending,
    /// The runner joined cleanly. Its finish evidence may still be missing.
    Joined,
    /// The join ended with an error. The worker is gone but the terminal was
    /// never confirmed together with a clean join.
    Failed(String),
}

enum JoinAttempt {
    Joined,
    Failed(String),
    TimedOut,
    Missing,
}

/// Owns a temporarily removed child handle. Unless the runner join is confirmed,
/// Drop puts the handle back so the run's drain still owns the same worker and
/// its live-child permit. A cancelled or dropped wait therefore cannot detach
/// the runner silently.
struct ChildJoin<'a> {
    scope: &'a RunScope,
    task_id: String,
    handle: Option<ChildHandle>,
    confirmed: bool,
}

impl Drop for ChildJoin<'_> {
    fn drop(&mut self) {
        if !self.confirmed
            && let Some(handle) = self.handle.take()
        {
            self.scope.restore_child(self.task_id.clone(), handle);
        }
    }
}

#[derive(Clone, Debug)]
struct ChildOutcome {
    task_id: String,
    session_id: String,
    label: String,
    depth: usize,
    state: TaskState,
    error: Option<String>,
    output: String,
}

impl ChildOutcome {
    fn confirmed(&self) -> bool {
        matches!(
            self.state,
            TaskState::Completed | TaskState::Failed | TaskState::Cancelled
        )
    }

    fn unknown(&self) -> bool {
        matches!(
            self.state,
            TaskState::OutcomeUnknown | TaskState::Draining | TaskState::Interrupted
        )
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RunArgs {
    prompt: String,
    label: Option<String>,
    mode: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FleetArgs {
    parent_session_id: Option<String>,
    after: Option<String>,
    limit: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StopArgs {
    child_task_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SteerArgs {
    child_task_id: String,
    text: String,
}

struct ChildService {
    context: Arc<RunContext>,
    config: LocalAgents,
}

impl ChildService {
    async fn run(
        &self,
        prompt: String,
        label: String,
        cancel: CancellationToken,
        mode: AgentRunMode,
    ) -> Result<Value, CapabilityError> {
        let context = self.context.clone();
        // Background children must not observe the parent capability cancel token:
        // returning the handle would otherwise cancel them via drop_guard.
        let parent_cancel = cancel.clone();
        let child_lifetime = if mode.detached() {
            CancellationToken::new()
        } else {
            cancel.child_token()
        };
        context.check(&parent_cancel).map_err(known)?;
        if context.scope.remaining_time().is_zero() {
            return Err(known("parent run time budget is exhausted"));
        }
        let child_scope = Arc::new(context.scope.child(&self.config).map_err(known)?);
        context
            .scope
            .begin_agent_run(self.config.max_calls)
            .map_err(known)?;
        let permit = context
            .scope
            .reserve_child(self.config.max_children)
            .map_err(known)?;

        // Serialize the capacity precheck, child session creation and immediate
        // enqueue with the runtime's other admission decisions. A capacity
        // refusal must not leave an empty child session behind. No await occurs
        // while the coordination guard is held.
        let (session, lease) = {
            let _coordinate = context.inner.coordination.enter().map_err(known)?;
            capacity_precheck(&context.inner).map_err(known)?;
            let session = context
                .inner
                .store
                .create_session(label.clone())
                .map_err(|error| context.persistence_failed(error))?;
            let lease = context
                .inner
                .queue
                .try_start_child(
                    &session.id,
                    &prompt,
                    &TaskIntent::General,
                    context.inner.config.max_queued_runs,
                    context.inner.config.max_concurrent_runs,
                )
                .map_err(known)?;
            (session, lease)
        };
        let identity = json!({
            "parent_session": &context.lease.session_id,
            "parent_task": &context.task.id,
            "child_session": &session.id,
            "child_task": &lease.id,
            "depth": child_scope.depth(),
            "label": &label,
            "prompt_bytes": prompt.len(),
            "mode": mode,
            "detached": mode.detached(),
        });
        if let Err(error) = context.inner.store.append_host_event(
            &context.lease,
            "agent_child_started",
            identity.clone(),
        ) {
            abort_started_child(&context.inner, &lease, "parent identity record failed");
            return Err(context.persistence_failed(error));
        }
        context.inner.changed.notify_waiters();
        if let Err(error) = context.inner.store.append_control_event(
            &session.id,
            session.cursor,
            None,
            "runtime-local-agent",
            "agent_child_linked",
            identity,
        ) {
            abort_started_child(&context.inner, &lease, "child identity record failed");
            return Err(context.persistence_failed(error));
        }
        context.inner.changed.notify_waiters();

        let child_cancel = child_lifetime;
        let (sender, receiver) = watch::channel(None);
        let runtime = Runtime {
            inner: context.inner.clone(),
        };
        let task_id = lease.id.clone();
        let session_id = session.id.clone();
        let runner_session = session_id.clone();
        let runner_label = label.clone();
        let depth = child_scope.depth();
        let runner_task = task_id.clone();
        let runner_cancel = child_cancel.clone();
        let join = tokio::spawn(async move {
            let result =
                crate::execution::resume_child(&runtime, &runner_task, child_scope, runner_cancel)
                    .await;
            let outcome = observe(
                &runtime,
                &runner_task,
                &runner_session,
                &runner_label,
                depth,
                result,
            );
            let _ = sender.send(Some(outcome));
        });
        // Transfer the live-slot permit into fleet ownership before any await.
        let live = context.scope.live_children_handle();
        let mut permit = permit;
        permit.active = false;
        context
            .inner
            .fleet
            .register(
                task_id.clone(),
                FleetEntry {
                    parent_session_id: context.lease.session_id.clone(),
                    parent_task_id: context.task.id.clone(),
                    child_session_id: session.id.clone(),
                    label: label.clone(),
                    depth,
                    mode,
                    detached: mode.detached(),
                    cancel: child_cancel.clone(),
                    state: FleetMemberState::Running,
                    error: None,
                    live: live.clone(),
                    // Only detached members own the live-slot release via fleet.
                    // Foreground still releases through insert_child / release_child.
                    slot_held: mode.detached(),
                    control: Mutex::new(None),
                },
            )
            .map_err(known)?;
        if mode.detached() {
            // Detached background: do not park the handle in the parent drain map.
            // A finisher owns join + durable evidence + live-slot release.
            let finish_context = context.clone();
            let finish_task = task_id.clone();
            let finish_session = session_id.clone();
            let finish_label = label.clone();
            let finish_depth = depth;
            let finish_max_time = self.config.max_time;
            let mut finish_receiver = receiver;
            tokio::spawn(async move {
                finish_detached(
                    finish_context,
                    finish_task,
                    finish_session,
                    finish_label,
                    finish_depth,
                    finish_max_time,
                    join,
                    &mut finish_receiver,
                )
                .await;
            });
            return Ok(json!({
                "session_id": session.id,
                "task_id": task_id,
                "label": bounded(&label, RESULT_LABEL_BYTES),
                "depth": depth,
                "mode": mode,
                "detached": true,
                "state": "running",
            }));
        }
        context.scope.insert_child(
            task_id.clone(),
            ChildHandle {
                cancel: child_cancel.clone(),
                outcome: receiver.clone(),
                join,
                join_state: JoinState::Pending,
                label: label.clone(),
                session_id: session.id.clone(),
                depth,
                detached: false,
            },
            permit,
        );

        let mut receiver = receiver;
        let parent_deadline = tokio::time::Instant::from_std(context.scope.deadline());
        let own_deadline = tokio::time::Instant::now() + self.config.max_time;
        let deadline = parent_deadline.min(own_deadline);
        let mut timed_out = false;
        enum Stop {
            Outcome(ChildOutcome),
            Cleanup,
        }
        let stop = loop {
            let ready = { receiver.borrow().clone() };
            if let Some(outcome) = ready {
                break Stop::Outcome(outcome);
            }
            tokio::select! {
                biased;
                _ = context.cancel.cancelled() => {
                    child_cancel.cancel();
                    break Stop::Cleanup;
                }
                _ = parent_cancel.cancelled() => {
                    child_cancel.cancel();
                    break Stop::Cleanup;
                }
                _ = tokio::time::sleep_until(deadline) => {
                    timed_out = true;
                    child_cancel.cancel();
                    break Stop::Cleanup;
                }
                changed = receiver.changed() => {
                    if changed.is_err() {
                        break Stop::Cleanup;
                    }
                }
            }
        };
        let outcome = match stop {
            Stop::Outcome(outcome) => outcome,
            Stop::Cleanup => {
                wait_cleanup(
                    &context,
                    &task_id,
                    &session_id,
                    &label,
                    depth,
                    self.config.max_time,
                    &mut receiver,
                )
                .await
            }
        };
        // A terminal that is not confirmed, or that is itself unknown, is never
        // recorded as a finished child. The handle and its live slot stay in the
        // scope so this run's drain still owns exactly the same worker.
        if !outcome.confirmed() || outcome.unknown() {
            context.unknown.store(true, Ordering::Release);
            context.cancel.cancel();
            return Err(CapabilityError::unknown(format!(
                "local agent {} ended in {:?} and requires reconciliation",
                outcome.task_id, outcome.state
            )));
        }

        // Join the runner before recording its terminal. `ChildJoin` puts the
        // handle back unless the whole finish path is confirmed, so neither a
        // bounded wait nor a failed evidence write can detach the worker or
        // release its live slot. Capacity is released only after the finish
        // event is durable.
        let mut joined = context.scope.take_child_for_join(&task_id);
        let attempt = match joined.handle.as_mut() {
            None => JoinAttempt::Missing,
            Some(handle) => match &handle.join_state {
                JoinState::Joined => JoinAttempt::Joined,
                JoinState::Failed(reason) => JoinAttempt::Failed(reason.clone()),
                JoinState::Pending => {
                    match tokio::time::timeout(self.config.max_time, &mut handle.join).await {
                        Ok(Ok(())) => {
                            handle.join_state = JoinState::Joined;
                            JoinAttempt::Joined
                        }
                        Ok(Err(error)) => {
                            let reason = error.to_string();
                            handle.join_state = JoinState::Failed(reason.clone());
                            JoinAttempt::Failed(reason)
                        }
                        Err(_) => JoinAttempt::TimedOut,
                    }
                }
            },
        };
        match attempt {
            JoinAttempt::Joined => {}
            JoinAttempt::Failed(reason) => {
                drop(joined);
                context.unknown.store(true, Ordering::Release);
                context.cancel.cancel();
                return Err(CapabilityError::unknown(format!(
                    "local agent {} runner did not join cleanly: {reason}",
                    outcome.task_id
                )));
            }
            JoinAttempt::TimedOut => {
                drop(joined);
                context.unknown.store(true, Ordering::Release);
                context.cancel.cancel();
                return Err(CapabilityError::unknown(format!(
                    "local agent {} cleanup did not finish; its runner stays owned by this run",
                    outcome.task_id
                )));
            }
            JoinAttempt::Missing => {
                drop(joined);
                context.unknown.store(true, Ordering::Release);
                context.cancel.cancel();
                return Err(CapabilityError::unknown(format!(
                    "local agent {} lost its runner handle before its terminal was confirmed",
                    outcome.task_id
                )));
            }
        }

        let event = finish_event(&context, &outcome);
        if let Err(error) =
            context
                .inner
                .store
                .append_host_event(&context.lease, "agent_child_finished", event)
        {
            // The joined worker and its live slot stay owned by the scope; a
            // later drain sees `Joined` and retries this evidence write.
            drop(joined);
            return Err(context.persistence_failed(error));
        }
        joined.confirmed = true;
        drop(joined);
        context.scope.release_child();
        context.inner.fleet.mark_terminal(
            &outcome.task_id,
            FleetMemberState::from_task(outcome.state),
            outcome.error.clone(),
            false,
        );
        context.inner.changed.notify_waiters();
        if timed_out && outcome.state != TaskState::Completed {
            return Err(known("local agent exceeded its time limit"));
        }
        result_value(outcome, self.config.max_output_bytes)
    }
}

#[async_trait]
impl ExternalHost for ChildService {
    async fn call(
        &self,
        name: &str,
        arguments: Value,
        cancel: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        if name != CAPABILITY {
            return Err(known("local agent service received a different capability"));
        }
        // Host already validated the schema; parse again with deny_unknown_fields
        // so extra model fields can never become authority.
        let args: RunArgs = serde_json::from_value(arguments)
            .map_err(|error| known(format!("agent.run arguments are invalid: {error}")))?;
        if args.prompt.trim().is_empty()
            || args.prompt.len() > self.context.inner.config.max_prompt_bytes
        {
            return Err(known(
                "agent.run prompt is empty or exceeds the prompt byte budget",
            ));
        }
        if let Some(label) = &args.label
            && (label.trim().is_empty() || label.len() > 1024)
        {
            return Err(known("agent.run label must contain 1..1024 bytes"));
        }
        let label = args.label.unwrap_or_else(|| DEFAULT_LABEL.into());
        let mode = AgentRunMode::parse(args.mode.as_deref()).map_err(known)?;
        self.run(args.prompt, label, cancel, mode).await
    }
}

/// Invoked by the backend after its ordinary capability admission. The supplied
/// host is already scoped to this exact `CallScope` for permission and audit, so
/// Gate authorization, the Core capability start and settlement all wrap the
/// child's start and cleanup.
pub(crate) async fn call(
    host: Host,
    scope: &Arc<CallScope>,
    name: &str,
    arguments: Value,
    cancel: CancellationToken,
) -> Result<Value, CapabilityError> {
    let context = scope.context.clone();
    let config = context
        .inner
        .config
        .local_agents
        .clone()
        .ok_or_else(|| known("local agents are not enabled by the trusted host"))?;
    match name {
        CAPABILITY_FLEET => {
            let args: FleetArgs = serde_json::from_value(arguments)
                .map_err(|error| known(format!("agent.fleet arguments are invalid: {error}")))?;
            let view = context
                .inner
                .fleet
                .snapshot(
                    args.parent_session_id.as_deref(),
                    args.after.as_deref(),
                    args.limit.unwrap_or(100),
                )
                .map_err(known)?;
            return serde_json::to_value(view).map_err(|error| known(error.to_string()));
        }
        CAPABILITY_STOP => {
            let args: StopArgs = serde_json::from_value(arguments)
                .map_err(|error| known(format!("agent.stop arguments are invalid: {error}")))?;
            return context.inner.fleet.stop(&args.child_task_id).map_err(known);
        }
        CAPABILITY_STEER => {
            let args: SteerArgs = serde_json::from_value(arguments)
                .map_err(|error| known(format!("agent.steer arguments are invalid: {error}")))?;
            return context
                .inner
                .fleet
                .steer(&args.child_task_id, &args.text)
                .map_err(known);
        }
        CAPABILITY => {}
        other => return Err(known(format!("local agent service received {other}"))),
    }
    // Reject nesting before any permission wait. A child at max_depth must not
    // be able to sit in an approval queue for a call that can never run.
    if context.scope.depth() >= config.max_depth {
        return Err(known("local agent depth limit reached"));
    }
    let service = ChildService { context, config };
    let definition = definition();
    let child = cancel.child_token();
    let _cancel_on_drop = child.clone().drop_guard();
    let run = host.call_service(&definition, arguments, child.clone(), &service);
    crate::requests::watch(scope, child, run).await
}

fn capacity_precheck(inner: &Inner) -> Result<(), RuntimeError> {
    inner.queue.check_child_capacity(
        inner.config.max_queued_runs,
        inner.config.max_concurrent_runs,
    )
}

fn abort_started_child(inner: &Inner, lease: &TaskLease, reason: &str) {
    if inner
        .queue
        .finish(lease, TaskTerminal::Failed, Some(reason.into()), |_| Ok(()))
        .is_err()
    {
        let _ = inner.queue.block(lease, reason);
    }
}

fn observe(
    runtime: &Runtime,
    task_id: &str,
    session_id: &str,
    label: &str,
    depth: usize,
    result: Result<SessionView, RuntimeError>,
) -> ChildOutcome {
    let output = match &result {
        Ok(view) => view
            .messages
            .iter()
            .rev()
            .find_map(|message| match message {
                Message::Assistant(_) => Some(message.text()),
                _ => None,
            })
            .unwrap_or_default(),
        Err(_) => String::new(),
    };
    let record = runtime.task(task_id);
    let (state, error) = match record {
        Ok(task) => (
            task.state,
            task.error
                .or_else(|| result.as_ref().err().map(ToString::to_string)),
        ),
        Err(error) => (TaskState::OutcomeUnknown, Some(error.to_string())),
    };
    ChildOutcome {
        task_id: task_id.into(),
        session_id: session_id.into(),
        label: label.into(),
        depth,
        state,
        error,
        output,
    }
}

fn observe_task(runtime: &Runtime, task_id: &str, fallback: &ChildOutcome) -> ChildOutcome {
    let Ok(task) = runtime.task(task_id) else {
        return ChildOutcome {
            state: TaskState::OutcomeUnknown,
            error: Some("local agent task terminal is unavailable".into()),
            output: String::new(),
            ..fallback.clone()
        };
    };
    ChildOutcome {
        task_id: task.id,
        session_id: task.session_id,
        label: fallback.label.clone(),
        depth: fallback.depth,
        state: task.state,
        error: task.error.or_else(|| fallback.error.clone()),
        output: String::new(),
    }
}

/// Owns a detached background child through its durable terminal. Parent cancel
/// and drain never enter this path; only the child's own token, time budget, or
/// an explicit fleet stop can stop it.
#[allow(clippy::too_many_arguments)]
async fn finish_detached(
    context: Arc<RunContext>,
    task_id: String,
    session_id: String,
    label: String,
    depth: usize,
    max_time: Duration,
    mut join: tokio::task::JoinHandle<()>,
    receiver: &mut watch::Receiver<Option<ChildOutcome>>,
) {
    let parent_deadline = tokio::time::Instant::from_std(context.scope.deadline());
    let own_deadline = tokio::time::Instant::now() + max_time;
    let deadline = parent_deadline.min(own_deadline);
    // Register always inserts the token before this finisher is spawned; a
    // missing handle is a programming error, not a never-cancelled default.
    let fleet_cancel = context
        .inner
        .fleet
        .cancel_token(&task_id)
        .expect("detached fleet member must expose its cancel token after register");
    loop {
        if receiver.borrow().is_some() {
            break;
        }
        tokio::select! {
            biased;
            _ = fleet_cancel.cancelled() => break,
            _ = tokio::time::sleep_until(deadline) => {
                fleet_cancel.cancel();
                break;
            }
            changed = receiver.changed() => {
                if changed.is_err() {
                    break;
                }
            }
        }
    }
    let outcome = wait_cleanup(
        &context,
        &task_id,
        &session_id,
        &label,
        depth,
        max_time,
        receiver,
    )
    .await;
    // Soft join for the detached runner: keep ownership in the fleet registry,
    // but surface join failure/timeout as Unknown instead of discarding silently.
    let join_ok = match tokio::time::timeout(max_time, &mut join).await {
        Ok(Ok(())) => true,
        Ok(Err(_join_err)) => {
            context.unknown.store(true, Ordering::Release);
            false
        }
        Err(_elapsed) => {
            context.unknown.store(true, Ordering::Release);
            false
        }
    };
    if join_ok && outcome.confirmed() && !outcome.unknown() {
        let event = finish_event(&context, &outcome);
        let _ =
            context
                .inner
                .store
                .append_host_event(&context.lease, "agent_child_finished", event);
        context.inner.fleet.mark_terminal(
            &task_id,
            FleetMemberState::from_task(outcome.state),
            outcome.error.clone(),
            true,
        );
    } else {
        let error = if !join_ok {
            Some(
                outcome
                    .error
                    .unwrap_or_else(|| "detached child runner join timed out or failed".into()),
            )
        } else {
            outcome
                .error
                .or_else(|| Some("detached child requires reconciliation".into()))
        };
        context
            .inner
            .fleet
            .mark_terminal(&task_id, FleetMemberState::Unknown, error, false);
    }
    context.inner.changed.notify_waiters();
}

/// Waits for cancellation cleanup without detaching the child. If the bounded
/// cleanup cannot confirm a terminal, the parent is marked unknown so its own
/// admission slot is retained for explicit recovery.
async fn wait_cleanup(
    context: &RunContext,
    task_id: &str,
    session_id: &str,
    label: &str,
    depth: usize,
    bound: Duration,
    receiver: &mut watch::Receiver<Option<ChildOutcome>>,
) -> ChildOutcome {
    let deadline = tokio::time::Instant::now() + bound;
    let runtime = Runtime {
        inner: context.inner.clone(),
    };
    let fallback = ChildOutcome {
        task_id: task_id.into(),
        session_id: session_id.into(),
        label: label.into(),
        depth,
        state: TaskState::Draining,
        error: None,
        output: String::new(),
    };
    loop {
        if let Some(outcome) = receiver.borrow().clone() {
            return outcome;
        }
        let changed = tokio::select! {
            biased;
            _ = tokio::time::sleep_until(deadline) => None,
            changed = receiver.changed() => Some(changed),
        };
        match changed {
            Some(Ok(())) => {}
            _ => {
                let outcome = observe_task(&runtime, task_id, &fallback);
                if !outcome.confirmed() || outcome.unknown() {
                    context.unknown.store(true, Ordering::Release);
                    context.cancel.cancel();
                }
                return outcome;
            }
        }
    }
}

fn finish_event(context: &RunContext, outcome: &ChildOutcome) -> Value {
    json!({
        "parent_session": &context.lease.session_id,
        "parent_task": &context.task.id,
        "child_session": &outcome.session_id,
        "child_task": &outcome.task_id,
        "depth": outcome.depth,
        "label": bounded(&outcome.label, RESULT_LABEL_BYTES),
        "state": outcome.state,
        "error": outcome.error.as_deref().map(|error| bounded(error, RESULT_ERROR_BYTES)),
        "output_bytes": outcome.output.len(),
    })
}

fn result_value(outcome: ChildOutcome, limit: usize) -> Result<Value, CapabilityError> {
    if !outcome.confirmed() || outcome.unknown() {
        return Err(CapabilityError::unknown(format!(
            "local agent {} ended in {:?} and requires reconciliation",
            outcome.task_id, outcome.state
        )));
    }
    match outcome.state {
        TaskState::Completed => {}
        TaskState::Cancelled => {
            return Err(known("local agent was cancelled before completion"));
        }
        TaskState::Failed => {
            return Err(known(format!(
                "local agent failed: {}",
                outcome.error.as_deref().unwrap_or("known failure")
            )));
        }
        _ => {
            return Err(CapabilityError::unknown(format!(
                "local agent {} has an unknown outcome",
                outcome.task_id
            )));
        }
    }
    let mut output = bounded(&outcome.output, limit);
    loop {
        let value = json!({
            "session_id": &outcome.session_id,
            "task_id": &outcome.task_id,
            "label": bounded(&outcome.label, RESULT_LABEL_BYTES),
            "depth": outcome.depth,
            "state": outcome.state,
            "output": &output,
        });
        let size = serde_json::to_vec(&value)
            .map_err(|error| known(error.to_string()))?
            .len();
        if size <= limit {
            return Ok(value);
        }
        if output.is_empty() {
            return Err(known("local agent output budget is too small"));
        }
        let shrink = size.saturating_sub(limit).max(1);
        output = bounded(&output, output.len().saturating_sub(shrink));
    }
}

fn bounded(text: &str, limit: usize) -> String {
    let mut end = text.len().min(limit);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].into()
}

fn known(message: impl std::fmt::Display) -> CapabilityError {
    CapabilityError::new(message.to_string())
}

/// Cancels every child still owned by this run and waits for their durable
/// terminals and their worker joins. A child is recorded and releases its live
/// slot only after both the clean join and the durable finish event succeed.
/// Any unconfirmed child stays in the scope with its original handle and live
/// slot: this returns `RecoveryRequired`, never a plain known failure.
pub(crate) async fn drain(context: &RunContext) -> Result<(), RuntimeError> {
    let ids = context.scope.child_ids();
    if ids.is_empty() {
        return Ok(());
    }
    let bound = context.scope.cleanup_bound();
    let deadline = tokio::time::Instant::now() + bound;
    let mut unconfirmed = vec![];
    for task_id in ids {
        // Remove exactly one handle. `ChildJoin` restores it unless the whole
        // finish path below is confirmed, so a timed-out join or a failed
        // evidence write cannot detach the original worker.
        let mut joined = context.scope.take_child_for_join(&task_id);
        if joined.handle.is_none() {
            unconfirmed.push(format!("{task_id}: child handle disappeared"));
            drop(joined);
            continue;
        }
        if joined.handle.as_ref().is_some_and(|handle| handle.detached) {
            // Detached background members are owned by FleetRegistry, not drain.
            drop(joined);
            continue;
        }
        let (outcome, attempt) = {
            let handle = joined.handle.as_mut().expect("handle checked above");
            handle.cancel.cancel();
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let outcome = wait_cleanup(
                context,
                &task_id,
                &handle.session_id,
                &handle.label,
                handle.depth,
                remaining,
                &mut handle.outcome,
            )
            .await;
            // A completed JoinHandle is never polled twice. `Joined` and
            // `Failed` carry a previous decision from the run path.
            let attempt = match &handle.join_state {
                JoinState::Joined => JoinAttempt::Joined,
                JoinState::Failed(reason) => JoinAttempt::Failed(reason.clone()),
                JoinState::Pending => {
                    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                    match tokio::time::timeout(remaining, &mut handle.join).await {
                        Ok(Ok(())) => {
                            handle.join_state = JoinState::Joined;
                            JoinAttempt::Joined
                        }
                        Ok(Err(error)) => {
                            let reason = error.to_string();
                            handle.join_state = JoinState::Failed(reason.clone());
                            JoinAttempt::Failed(reason)
                        }
                        Err(_) => JoinAttempt::TimedOut,
                    }
                }
            };
            (outcome, attempt)
        };
        match attempt {
            JoinAttempt::Joined => {}
            JoinAttempt::Failed(reason) => {
                unconfirmed.push(format!("{task_id}: runner join failed: {reason}"));
                drop(joined);
                continue;
            }
            JoinAttempt::TimedOut => {
                unconfirmed.push(format!("{task_id}: cleanup join did not finish"));
                drop(joined);
                continue;
            }
            JoinAttempt::Missing => {
                unconfirmed.push(format!("{task_id}: child handle disappeared"));
                drop(joined);
                continue;
            }
        }
        if !outcome.confirmed() || outcome.unknown() {
            unconfirmed.push(format!("{task_id}: {:?}", outcome.state));
            drop(joined);
            continue;
        }
        // Record the terminal before releasing capacity. If this write fails,
        // the handle keeps `Joined` and its live slot stays charged so a later
        // drain can retry the evidence instead of forgetting the child.
        match context.inner.store.append_host_event(
            &context.lease,
            "agent_child_finished",
            finish_event(context, &outcome),
        ) {
            Ok(_) => {
                joined.confirmed = true;
                drop(joined);
                context.scope.release_child();
                context.inner.fleet.mark_terminal(
                    &task_id,
                    FleetMemberState::from_task(outcome.state),
                    outcome.error.clone(),
                    false,
                );
                context.inner.changed.notify_waiters();
            }
            Err(error) => {
                context.unknown.store(true, Ordering::Release);
                unconfirmed.push(format!("{task_id}: evidence: {error}"));
                drop(joined);
            }
        }
    }
    if unconfirmed.is_empty() {
        return Ok(());
    }
    context.unknown.store(true, Ordering::Release);
    context.cancel.cancel();
    Err(RuntimeError::RecoveryRequired {
        cause: Box::new(RuntimeError::Worker(format!(
            "local agent cleanup was not confirmed: {}",
            unconfirmed.join("; ")
        ))),
        cleanup: None,
        outcome_unknown: true,
    })
}

#[cfg(test)]
impl RunScope {
    fn fixture() -> Self {
        Self {
            tokens: Arc::new(AtomicU64::new(0)),
            token_limit: 1024,
            calls: Arc::new(AtomicUsize::new(0)),
            bytes: Arc::new(AtomicUsize::new(0)),
            agent_runs: Arc::new(AtomicUsize::new(0)),
            live_children: Arc::new(AtomicUsize::new(0)),
            children: Mutex::new(BTreeMap::new()),
            depth: 0,
            permissions: vec![],
            config: Some(LocalAgents::default()),
            deadline: std::time::Instant::now() + Duration::from_secs(60),
            max_capability_calls: 1024,
            max_capability_bytes: 1024,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The failure path that drain and the capability wait both rely on: a
    /// bounded join that does not finish must put the original handle back and
    /// keep its live slot charged.
    #[tokio::test(start_paused = true)]
    async fn unconfirmed_join_restores_the_original_handle_and_live_slot() {
        let scope = RunScope::fixture();
        let permit = scope.reserve_child(1).expect("first child slot");
        let (sender, outcome) = watch::channel(None);
        let join = tokio::spawn(std::future::pending::<()>());
        scope.insert_child(
            "child-task".into(),
            ChildHandle {
                cancel: CancellationToken::new(),
                outcome,
                join,
                join_state: JoinState::Pending,
                label: "Child".into(),
                session_id: "child-session".into(),
                depth: 1,
                detached: false,
            },
            permit,
        );
        assert_eq!(scope.live_children.load(Ordering::Acquire), 1);

        let mut joined = scope.take_child_for_join("child-task");
        let handle = joined.handle.as_mut().expect("handle");
        assert!(
            tokio::time::timeout(Duration::from_secs(60), &mut handle.join)
                .await
                .is_err(),
            "a pending runner must not report a clean join"
        );
        drop(joined);

        assert!(
            scope.children.lock().unwrap().contains_key("child-task"),
            "the original handle must be restored after an unconfirmed join"
        );
        assert_eq!(
            scope.live_children.load(Ordering::Acquire),
            1,
            "the live slot stays charged until the finish path is confirmed"
        );

        let mut joined = scope.take_child_for_join("child-task");
        let handle = joined.handle.take().expect("restored handle");
        handle.join.abort();
        let _ = handle.join.await;
        scope.release_child();
        assert_eq!(scope.live_children.load(Ordering::Acquire), 0);
        drop(sender);
    }
}
