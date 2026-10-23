//! Human clarification and task-plan collaboration capabilities.
//!
//! The model can only ask a question or replace the current task plan. Both are
//! ordinary dispatched capabilities: [`invoke`] sends them through the original
//! scoped Host so the existing permission gate, cancellation, byte budget and
//! CallScope audit run before anything is persisted. The model has no answer
//! capability; only the host SDK/RPC method [`Runtime::answer_question`] can
//! answer, and answering never creates a permission grant.
//!
//! Core append-only Host events are the authority. Questions and plans are
//! rebuilt from those events on every read, including reads through another
//! Runtime connection. No in-memory question or plan table exists.

use crate::{Inner, Runtime, RuntimeError, TaskState};
use async_trait::async_trait;
use cayw_ai::{CancellationToken, ToolDefinition};
use cayw_codemode::CapabilityError;
use cayw_core::{
    AttemptLease, CapabilityCallKey, CapabilityCallRecord, CoreError, CoreEvent, EventRecord,
    MAX_EVENT_PAGE, SessionState,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::{Duration, Instant},
};
use uuid::Uuid;

pub const ASK: &str = "user.ask";
pub const PLAN: &str = "plan.update";
const ASKED: &str = "collaboration_question_asked";
const ANSWERED: &str = "collaboration_question_answered";
const PLAN_UPDATED: &str = "collaboration_plan_updated";
const MAX_QUESTION_BYTES: usize = 4096;
const MAX_ANSWER_BYTES: usize = 4096;
const MAX_OPTIONS: usize = 16;
const MAX_OPTION_BYTES: usize = 1024;
const MAX_STEPS: usize = 32;
const MAX_STEP_TEXT_BYTES: usize = 1024;
const MAX_STEP_ID_BYTES: usize = 64;
const MAX_STEPS_BYTES: usize = 64 * 1024;
const MAX_IDENTIFIER_BYTES: usize = 128;
const MAX_ACTOR_BYTES: usize = 1024;

/// Only these two names are model-visible. There is deliberately no answer,
/// approve, evaluate or grant capability.
pub fn definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: ASK.into(),
            description: "Ask the human operator one blocking clarification question. The call waits for the host to answer; options are advisory choices. Asking never grants permission and performs no other work.".into(),
            parameters: json!({
                "type":"object","additionalProperties":false,"required":["question"],
                "properties":{
                    "question":{"type":"string","minLength":1,"maxLength":MAX_QUESTION_BYTES},
                    "options":{"type":"array","maxItems":MAX_OPTIONS,"items":{"type":"string","minLength":1,"maxLength":MAX_OPTION_BYTES}}
                }
            }),
        },
        ToolDefinition {
            name: PLAN.into(),
            description: "Replace the current task plan with compare-and-swap on expected_version. Version starts at 0 for each task; a successful update returns the previous version, the new version and source=model. At most one step may be in_progress and ids must be unique.".into(),
            parameters: json!({
                "type":"object","additionalProperties":false,"required":["expected_version","steps"],
                "properties":{
                    "expected_version":{"type":"integer","minimum":0},
                    "steps":{"type":"array","maxItems":MAX_STEPS,"items":{
                        "type":"object","additionalProperties":false,"required":["id","text","status"],
                        "properties":{
                            "id":{"type":"string","minLength":1,"maxLength":MAX_STEP_ID_BYTES,"pattern":"^[A-Za-z0-9_.:-]+$"},
                            "text":{"type":"string","minLength":1,"maxLength":MAX_STEP_TEXT_BYTES},
                            "status":{"enum":["pending","in_progress","completed"]}
                        }
                    }}
                }
            }),
        },
    ]
}
pub fn is_tool(name: &str) -> bool {
    matches!(name, ASK | PLAN)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuestionState {
    Pending,
    Answered,
    Expired,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuestionAnswer {
    pub answer: String,
    pub actor: String,
    pub answered_at: u64,
}
/// A question is bound to the complete original lease, task and capability key.
/// `state` is derived from Core events plus the current attempt, task and queue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuestionView {
    pub id: String,
    pub session_id: String,
    pub task_id: String,
    pub attempt_id: String,
    pub call: CapabilityCallKey,
    pub question: String,
    pub options: Vec<String>,
    pub state: QuestionState,
    pub answer: Option<QuestionAnswer>,
    pub asked_at: u64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStepStatus {
    Pending,
    InProgress,
    Completed,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanStep {
    pub id: String,
    pub text: String,
    pub status: PlanStepStatus,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanView {
    pub session_id: String,
    pub task_id: String,
    pub attempt_id: String,
    pub version: u64,
    pub previous_version: u64,
    pub source: String,
    pub steps: Vec<PlanStep>,
    pub updated_at: u64,
    pub cursor: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Asked {
    question_id: String,
    task_id: String,
    lease: AttemptLease,
    call: CapabilityCallKey,
    question: String,
    options: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Answered {
    question_id: String,
    task_id: String,
    attempt_id: String,
    answer: String,
    actor: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanUpdate {
    task_id: String,
    attempt_id: String,
    call: CapabilityCallKey,
    previous_version: u64,
    version: u64,
    source: String,
    steps: Vec<PlanStep>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AskInput {
    question: String,
    #[serde(default)]
    options: Vec<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanInput {
    expected_version: u64,
    steps: Vec<PlanStep>,
}

fn known(error: impl std::fmt::Display) -> CapabilityError {
    CapabilityError::new(error.to_string())
}
fn corrupt(message: impl Into<String>) -> CoreError {
    CoreError::CorruptStore(message.into())
}

/// Dispatches the two model capabilities through the original Host permission
/// gate and CallScope audit. Semantic validation runs before asking a human.
pub(crate) async fn invoke(
    scope: &Arc<crate::cells::CallScope>,
    base: &cayw_host::Host,
    arguments: Value,
    cancel: CancellationToken,
) -> Result<Value, CapabilityError> {
    let definition = definitions()
        .into_iter()
        .find(|definition| definition.name == scope.name)
        .ok_or_else(|| CapabilityError::new("collaboration capability is unknown"))?;
    match scope.name.as_str() {
        ASK => {
            let input: AskInput = serde_json::from_value(arguments.clone()).map_err(|error| {
                CapabilityError::new(format!("user.ask arguments are invalid: {error}"))
            })?;
            validate_ask(&input)?;
        }
        PLAN => {
            let input: PlanInput = serde_json::from_value(arguments.clone()).map_err(|error| {
                CapabilityError::new(format!("plan.update arguments are invalid: {error}"))
            })?;
            check_steps(&input.steps).map_err(CapabilityError::new)?;
        }
        _ => return Err(CapabilityError::new("collaboration capability is unknown")),
    }
    let host = crate::bridge::scoped_host(base, scope)?;
    // The Host's external executor maps its own cancellation and deadline to
    // `outcome_unknown`. Collaboration calls keep their caller token outside the
    // Host, observe it in the service, and stop just before the Host deadline so
    // cancellation or an unanswered question settles as a known failure while
    // the durable question/plan fact stays in Core. The stop time is enforced
    // inside the service instead of by dropping the Host future, because a
    // dropped Host call conservatively records an unknown outcome.
    let host_timeout = scope.context.inner.config.host.call_timeout;
    let stop_at = Instant::now() + host_timeout.saturating_sub(host_timeout / 4);
    let service = Service {
        scope,
        caller_cancel: cancel.clone(),
        stop_at,
    };
    host.call_service(&definition, arguments, CancellationToken::new(), &service)
        .await
}

struct Service<'a> {
    scope: &'a Arc<crate::cells::CallScope>,
    caller_cancel: CancellationToken,
    stop_at: Instant,
}
#[async_trait]
impl cayw_host::ExternalHost for Service<'_> {
    async fn call(
        &self,
        name: &str,
        arguments: Value,
        cancel: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        match name {
            ASK => {
                ask(
                    self.scope,
                    arguments,
                    &self.caller_cancel,
                    &cancel,
                    self.stop_at,
                )
                .await
            }
            PLAN => plan(self.scope, arguments, &self.caller_cancel, &cancel),
            _ => Err(CapabilityError::new("collaboration capability is unknown")),
        }
    }
}

/// Persist the question through the RunContext Host-event boundary, then poll
/// the same Core session for an answer. Cancellation or a stale permission
/// returns a known failure; the question fact stays in the append-only log.
async fn ask(
    scope: &Arc<crate::cells::CallScope>,
    arguments: Value,
    caller_cancel: &CancellationToken,
    host_cancel: &CancellationToken,
    stop_at: Instant,
) -> Result<Value, CapabilityError> {
    let input: AskInput = serde_json::from_value(arguments).map_err(|error| {
        CapabilityError::new(format!("user.ask arguments are invalid: {error}"))
    })?;
    validate_ask(&input)?;
    let context = &scope.context;
    context.check(host_cancel).map_err(known)?;
    if caller_cancel.is_cancelled() {
        return Err(CapabilityError::new(
            "user.ask was cancelled before the question was persisted",
        ));
    }
    scope.check_authority(host_cancel)?;
    if Instant::now() >= stop_at {
        return Err(CapabilityError::new(
            "user.ask reached its host deadline before the question was persisted",
        ));
    }
    // Everything at or before this cursor predates the question. Polling from
    // here is bounded by the events this call appends plus the answer.
    let after = context
        .inner
        .store
        .snapshot(&context.lease.session_id)
        .map_err(|error| CapabilityError::new(format!("question log is unavailable: {error}")))?
        .cursor;
    let question_id = Uuid::new_v4().to_string();
    let asked = Asked {
        question_id: question_id.clone(),
        task_id: context.task.id.clone(),
        lease: context.lease.clone(),
        call: scope.key.clone(),
        question: input.question,
        options: input.options,
    };
    let data = serde_json::to_value(&asked).map_err(|error| {
        known(format!(
            "collaboration question serialization failed: {error}"
        ))
    })?;
    context.host_event(ASKED, data)?;
    // The question is now durable. If the VM drops this future because its own
    // cancellation branch won the race, settle the original scope as a known
    // failure instead of the generic "started capability dropped" unknown. The
    // append-only question fact remains available to the host either way.
    let mut stopped = KnownStop {
        scope,
        reason: format!(
            "question {question_id} stopped before an answer was accepted; the question fact is retained"
        ),
        armed: true,
    };
    let result = wait_for_answer(
        scope,
        &question_id,
        after,
        caller_cancel,
        host_cancel,
        stop_at,
    )
    .await;
    stopped.armed = false;
    result
}

/// Settles the original capability as a known failure if its future is dropped
/// after the question became durable. `CallScope::settle` is idempotent, so a
/// normal return still owns its own terminal.
struct KnownStop<'a> {
    scope: &'a Arc<crate::cells::CallScope>,
    reason: String,
    armed: bool,
}
impl Drop for KnownStop<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let _ = self
            .scope
            .settle(&Err(CapabilityError::new(self.reason.clone())));
        self.scope.context.inner.changed.notify_waiters();
    }
}

async fn wait_for_answer(
    scope: &Arc<crate::cells::CallScope>,
    question_id: &str,
    after: u64,
    caller_cancel: &CancellationToken,
    host_cancel: &CancellationToken,
    stop_at: Instant,
) -> Result<Value, CapabilityError> {
    let context = &scope.context;
    let mut cursor = after;
    loop {
        if caller_cancel.is_cancelled() {
            return Err(CapabilityError::new(format!(
                "question {question_id} expired: the original capability was cancelled"
            )));
        }
        if Instant::now() >= stop_at {
            return Err(CapabilityError::new(format!(
                "question {question_id} expired: the collaboration call reached its host deadline"
            )));
        }
        scope.check_authority(host_cancel).map_err(|error| {
            CapabilityError::new(format!("question {question_id} expired: {}", error.message))
        })?;
        let (answer, next) = read_answer(
            &context.inner,
            &context.lease.session_id,
            cursor,
            question_id,
            &context.lease,
            &context.task.id,
        )?;
        cursor = next;
        if let Some((answer, answered_at)) = answer {
            // The response is only accepted while the original exact permission
            // and cancellation state remain current.
            if caller_cancel.is_cancelled() {
                return Err(CapabilityError::new(format!(
                    "question {question_id} expired: the original capability was cancelled"
                )));
            }
            scope.check_authority(host_cancel).map_err(|error| {
                CapabilityError::new(format!("question {question_id} expired: {}", error.message))
            })?;
            context.check(&context.cancel).map_err(known)?;
            return Ok(json!({
                "question_id": question_id,
                "answer": answer,
                "answered_at": answered_at,
            }));
        }
        tokio::select! { biased;
            _ = caller_cancel.cancelled() => return Err(CapabilityError::new(format!(
                "question {question_id} expired: the original capability was cancelled"
            ))),
            _ = host_cancel.cancelled() => return Err(CapabilityError::new(format!(
                "question {question_id} expired: the original capability was cancelled"
            ))),
            _ = context.cancel.cancelled() => return Err(CapabilityError::new(format!(
                "question {question_id} expired: the task was cancelled"
            ))),
            _ = tokio::time::sleep(stop_at.saturating_duration_since(Instant::now())) => return Err(CapabilityError::new(format!(
                "question {question_id} expired: the collaboration call reached its host deadline"
            ))),
            _ = context.inner.changed.notified() => {},
            _ = tokio::time::sleep(Duration::from_millis(50)) => {},
        }
    }
}

fn read_answer(
    inner: &Arc<Inner>,
    session: &str,
    after: u64,
    question_id: &str,
    lease: &AttemptLease,
    task_id: &str,
) -> Result<(Option<(String, u64)>, u64), CapabilityError> {
    let snapshot = inner
        .store
        .snapshot(session)
        .map_err(|error| CapabilityError::new(format!("question log is unavailable: {error}")))?;
    let mut cursor = after;
    let mut answer = None;
    while cursor < snapshot.cursor {
        let limit = (snapshot.cursor - cursor).min(MAX_EVENT_PAGE as u64) as usize;
        let page = inner
            .store
            .events(session, cursor, limit)
            .map_err(|error| {
                CapabilityError::new(format!("question log is unavailable: {error}"))
            })?;
        if page.is_empty() {
            break;
        }
        for record in &page {
            cursor = record.cursor;
            let CoreEvent::Host { kind, data } = &record.event else {
                continue;
            };
            if kind != ANSWERED {
                continue;
            }
            let answered: Answered = serde_json::from_value(data.clone()).map_err(|error| {
                CapabilityError::new(format!("collaboration question log is corrupt: {error}"))
            })?;
            if answered.question_id != question_id {
                continue;
            }
            if answered.task_id != task_id || answered.attempt_id != lease.attempt_id {
                return Err(CapabilityError::new(
                    "question answer does not belong to the original task attempt",
                ));
            }
            answer = Some((answered.answer, record.timestamp));
            break;
        }
        if answer.is_some() {
            break;
        }
    }
    Ok((answer, cursor))
}

/// Compare-and-swap the current task plan. Version 0 means the task has no plan
/// yet. The coordination lock spans the authoritative read and the event append,
/// so two Runtime connections cannot both accept the same expected_version.
fn plan(
    scope: &Arc<crate::cells::CallScope>,
    arguments: Value,
    caller_cancel: &CancellationToken,
    host_cancel: &CancellationToken,
) -> Result<Value, CapabilityError> {
    let input: PlanInput = serde_json::from_value(arguments).map_err(|error| {
        CapabilityError::new(format!("plan.update arguments are invalid: {error}"))
    })?;
    check_steps(&input.steps).map_err(CapabilityError::new)?;
    let context = &scope.context;
    context.check(host_cancel).map_err(known)?;
    if caller_cancel.is_cancelled() {
        return Err(CapabilityError::new(
            "plan.update was cancelled before the plan was recorded",
        ));
    }
    scope.check_authority(host_cancel)?;
    let _coordinate = context.inner.coordination.enter().map_err(known)?;
    context
        .inner
        .store
        .validate_lease(&context.lease)
        .map_err(|error| {
            CapabilityError::new(format!(
                "plan.update requires the original active attempt: {error}"
            ))
        })?;
    let events = read_log(&context.inner, &context.lease.session_id).map_err(known)?;
    let current = latest_plan(&events, &context.task.id)
        .map_err(|error| CapabilityError::new(error.to_string()))?;
    let previous = current.as_ref().map_or(0, |plan| plan.update.version);
    if previous != input.expected_version {
        return Err(CapabilityError::new(format!(
            "plan.update version conflict: expected {}, current {previous}",
            input.expected_version
        )));
    }
    let version = previous
        .checked_add(1)
        .ok_or_else(|| CapabilityError::new("plan.update version is exhausted"))?;
    let update = PlanUpdate {
        task_id: context.task.id.clone(),
        attempt_id: context.lease.attempt_id.clone(),
        call: scope.key.clone(),
        previous_version: previous,
        version,
        source: "model".into(),
        steps: input.steps,
    };
    let data = serde_json::to_value(&update)
        .map_err(|error| known(format!("plan update serialization failed: {error}")))?;
    context.host_event(PLAN_UPDATED, data)?;
    Ok(json!({
        "task_id": update.task_id,
        "attempt_id": update.attempt_id,
        "previous_version": previous,
        "version": version,
        "source": update.source,
        "steps": update.steps,
    }))
}

fn validate_ask(input: &AskInput) -> Result<(), CapabilityError> {
    check_text(&input.question, "question", MAX_QUESTION_BYTES, true)
        .map_err(CapabilityError::new)?;
    if input.options.len() > MAX_OPTIONS {
        return Err(CapabilityError::new(format!(
            "options may contain at most {MAX_OPTIONS} entries"
        )));
    }
    for option in &input.options {
        check_text(option, "option", MAX_OPTION_BYTES, false).map_err(CapabilityError::new)?;
    }
    Ok(())
}
fn check_text(value: &str, field: &str, maximum: usize, multiline: bool) -> Result<(), String> {
    if value.trim().is_empty() || value.len() > maximum {
        return Err(format!("{field} must contain 1..{maximum} bytes"));
    }
    if value
        .chars()
        .any(|c| c.is_control() && !(multiline && matches!(c, '\n' | '\t')))
    {
        return Err(format!("{field} contains unsupported control characters"));
    }
    Ok(())
}
fn check_identifier(value: &str, field: &str, maximum: usize) -> Result<(), String> {
    if value.is_empty()
        || value.len() > maximum
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':'))
    {
        return Err(format!(
            "{field} must contain 1..{maximum} ASCII identifier bytes"
        ));
    }
    Ok(())
}
fn check_steps(steps: &[PlanStep]) -> Result<(), String> {
    if steps.len() > MAX_STEPS {
        return Err(format!("plan may contain at most {MAX_STEPS} steps"));
    }
    let encoded = serde_json::to_vec(steps).map_err(|error| error.to_string())?;
    if encoded.len() > MAX_STEPS_BYTES {
        return Err(format!(
            "plan steps exceed {MAX_STEPS_BYTES} serialized bytes"
        ));
    }
    let mut ids = BTreeSet::new();
    let mut in_progress = 0usize;
    for step in steps {
        check_identifier(&step.id, "plan step id", MAX_STEP_ID_BYTES)?;
        if !ids.insert(step.id.as_str()) {
            return Err(format!("plan step id {} is duplicated", step.id));
        }
        check_text(&step.text, "plan step text", MAX_STEP_TEXT_BYTES, true)?;
        if step.status == PlanStepStatus::InProgress {
            in_progress += 1;
        }
    }
    if in_progress > 1 {
        return Err("plan may contain at most one in_progress step".into());
    }
    Ok(())
}
fn validate_actor(actor: &str) -> Result<(), RuntimeError> {
    check_text(actor, "actor", MAX_ACTOR_BYTES, false).map_err(RuntimeError::InvalidInput)
}
fn validate_answer(answer: &str) -> Result<(), RuntimeError> {
    check_text(answer, "answer", MAX_ANSWER_BYTES, true).map_err(RuntimeError::InvalidInput)
}
fn validate_request(request: &str) -> Result<(), RuntimeError> {
    check_identifier(request, "question id", MAX_IDENTIFIER_BYTES)
        .map_err(RuntimeError::InvalidInput)
}

fn read_log(inner: &Inner, session: &str) -> Result<Vec<EventRecord>, RuntimeError> {
    let snapshot = inner.store.snapshot(session)?;
    let mut cursor = 0u64;
    let mut events = Vec::new();
    while cursor < snapshot.cursor {
        let limit = (snapshot.cursor - cursor).min(MAX_EVENT_PAGE as u64) as usize;
        let page = inner.store.events(session, cursor, limit)?;
        if page.is_empty() {
            break;
        }
        let next = page.last().map_or(cursor, |record| record.cursor);
        if next <= cursor {
            break;
        }
        cursor = next;
        events.extend(page);
    }
    Ok(events)
}

struct SeenAsk {
    asked: Asked,
    timestamp: u64,
}
struct SeenAnswer {
    answered: Answered,
    timestamp: u64,
}
struct QuestionLog {
    asks: Vec<SeenAsk>,
    answers: BTreeMap<String, SeenAnswer>,
}
fn question_log(events: &[EventRecord]) -> Result<QuestionLog, RuntimeError> {
    let mut ids = BTreeSet::new();
    let mut asks = Vec::new();
    let mut answers = BTreeMap::new();
    for record in events {
        let CoreEvent::Host { kind, data } = &record.event else {
            continue;
        };
        match kind.as_str() {
            ASKED => {
                let asked: Asked = serde_json::from_value(data.clone())
                    .map_err(|error| corrupt(format!("invalid question event: {error}")))?;
                if record.attempt.as_ref() != Some(&asked.lease) {
                    return Err(corrupt("question event lease does not match its record").into());
                }
                if !ids.insert(asked.question_id.clone()) {
                    return Err(corrupt("question id is duplicated in the session log").into());
                }
                asks.push(SeenAsk {
                    asked,
                    timestamp: record.timestamp,
                });
            }
            ANSWERED => {
                let answered: Answered = serde_json::from_value(data.clone())
                    .map_err(|error| corrupt(format!("invalid question answer event: {error}")))?;
                if record
                    .attempt
                    .as_ref()
                    .is_none_or(|lease| lease.attempt_id != answered.attempt_id)
                {
                    return Err(corrupt("question answer has no matching attempt").into());
                }
                if answers
                    .insert(
                        answered.question_id.clone(),
                        SeenAnswer {
                            answered,
                            timestamp: record.timestamp,
                        },
                    )
                    .is_some()
                {
                    return Err(corrupt("question was answered more than once").into());
                }
            }
            _ => {}
        }
    }
    for (question_id, answer) in &answers {
        let Some(ask) = asks
            .iter()
            .find(|ask| &ask.asked.question_id == question_id)
        else {
            return Err(corrupt("question answer has no original question").into());
        };
        if answer.answered.task_id != ask.asked.task_id
            || answer.answered.attempt_id != ask.asked.lease.attempt_id
        {
            return Err(corrupt("question answer belongs to a different task attempt").into());
        }
    }
    Ok(QuestionLog { asks, answers })
}

fn view(
    session: &str,
    ask: &SeenAsk,
    answer: Option<&SeenAnswer>,
    state: QuestionState,
) -> QuestionView {
    QuestionView {
        id: ask.asked.question_id.clone(),
        session_id: session.into(),
        task_id: ask.asked.task_id.clone(),
        attempt_id: ask.asked.lease.attempt_id.clone(),
        call: ask.asked.call.clone(),
        question: ask.asked.question.clone(),
        options: ask.asked.options.clone(),
        state,
        answer: answer.map(|answer| QuestionAnswer {
            answer: answer.answered.answer.clone(),
            actor: answer.answered.actor.clone(),
            answered_at: answer.timestamp,
        }),
        asked_at: ask.timestamp,
    }
}

/// A question is alive only while the complete original lease is still the
/// current attempt, its task is running and not cancelled, the session is not
/// frozen or reconciling, and the Core lease fence still validates.
fn question_active(
    inner: &Inner,
    session: &str,
    attempt: Option<&AttemptLease>,
    state: SessionState,
    tasks: &[crate::TaskRecord],
    calls: &[CapabilityCallRecord],
    asked: &Asked,
) -> Result<bool, RuntimeError> {
    let Some(lease) = attempt else {
        return Ok(false);
    };
    if lease != &asked.lease || state == SessionState::NeedsReconciliation {
        return Ok(false);
    }
    // The answer is accepted only while the original capability call is still
    // waiting. After that call settles (for example at its deadline), an answer
    // would have no waiter and is late even though the task is still running.
    if !calls.iter().any(|call| {
        call.key == asked.call
            && call.name == ASK
            && call.task_id == asked.task_id
            && call.lease == asked.lease
    }) {
        return Ok(false);
    }
    if inner
        .cells
        .is_frozen(session)
        .map_err(|error| RuntimeError::Workspace(error.to_string()))?
    {
        return Ok(false);
    }
    let Some(task) = tasks.iter().find(|task| task.id == asked.task_id) else {
        return Ok(false);
    };
    if task.state != TaskState::Running
        || task.cancel_requested
        || task.worker_id.as_deref() != Some(&lease.worker_id)
        || task.attempt_id.as_deref() != Some(&lease.attempt_id)
        || task.fence != lease.fence
    {
        return Ok(false);
    }
    Ok(inner.store.validate_lease(lease).is_ok())
}

struct StoredPlan {
    update: PlanUpdate,
    cursor: u64,
    timestamp: u64,
}
fn latest_plan(events: &[EventRecord], task_id: &str) -> Result<Option<StoredPlan>, CoreError> {
    let mut version = 0u64;
    let mut latest = None;
    for record in events {
        let CoreEvent::Host { kind, data } = &record.event else {
            continue;
        };
        if kind != PLAN_UPDATED {
            continue;
        }
        let update: PlanUpdate = serde_json::from_value(data.clone())
            .map_err(|error| corrupt(format!("invalid plan event: {error}")))?;
        if update.task_id != task_id {
            continue;
        }
        let expected = version
            .checked_add(1)
            .ok_or_else(|| corrupt("plan version is exhausted"))?;
        if update.previous_version != version || update.version != expected {
            return Err(corrupt("plan version chain is broken"));
        }
        if record
            .attempt
            .as_ref()
            .is_none_or(|lease| lease.attempt_id != update.attempt_id)
        {
            return Err(corrupt("plan event has no matching attempt"));
        }
        if update.source.is_empty() || update.source.len() > 64 {
            return Err(corrupt("plan event has an invalid source"));
        }
        check_steps(&update.steps)
            .map_err(|error| corrupt(format!("stored plan is invalid: {error}")))?;
        version = update.version;
        latest = Some(StoredPlan {
            update,
            cursor: record.cursor,
            timestamp: record.timestamp,
        });
    }
    Ok(latest)
}
fn latest_any_plan(events: &[EventRecord]) -> Result<Option<StoredPlan>, CoreError> {
    let Some(record) = events.iter().rev().find(
        |record| matches!(&record.event, CoreEvent::Host { kind, .. } if kind == PLAN_UPDATED),
    ) else {
        return Ok(None);
    };
    let CoreEvent::Host { data, .. } = &record.event else {
        return Ok(None);
    };
    let update: PlanUpdate = serde_json::from_value(data.clone())
        .map_err(|error| corrupt(format!("invalid plan event: {error}")))?;
    latest_plan(events, &update.task_id)
}

impl Runtime {
    /// Host SDK and authenticated IPC only. The model has no equivalent tool.
    /// Answers are accepted only for the current original attempt; duplicate
    /// identical answers are idempotent, duplicate different answers are rejected.
    /// Answering never writes a permission grant.
    pub fn answer_question(
        &self,
        session: &str,
        request: &str,
        answer: &str,
        actor: &str,
    ) -> Result<QuestionView, RuntimeError> {
        validate_actor(actor)?;
        validate_answer(answer)?;
        validate_request(request)?;
        let _coordinate = self.inner.coordination.enter()?;
        let snapshot = self.inner.store.snapshot(session)?;
        let events = read_log(&self.inner, session)?;
        let log = question_log(&events)?;
        let ask = log
            .asks
            .iter()
            .find(|ask| ask.asked.question_id == request)
            .ok_or_else(|| {
                RuntimeError::InvalidInput(format!(
                    "question {request} does not belong to session {session}"
                ))
            })?;
        let tasks = self.inner.queue.list(session)?;
        let active = question_active(
            &self.inner,
            session,
            snapshot.attempt.as_ref(),
            snapshot.state,
            &tasks,
            &snapshot.unresolved_capabilities,
            &ask.asked,
        )?;
        if let Some(existing) = log.answers.get(request) {
            // Replaying the recorded answer is idempotent only while the
            // original request is still the live request. A late, cancelled or
            // expired request is refused even when the text matches.
            if !active {
                return Err(RuntimeError::StaleApproval);
            }
            if existing.answered.answer == answer {
                return Ok(view(session, ask, Some(existing), QuestionState::Answered));
            }
            return Err(RuntimeError::InvalidInput(
                "question was already answered with a different answer".into(),
            ));
        }
        if !active {
            return Err(RuntimeError::StaleApproval);
        }
        let answered = Answered {
            question_id: request.into(),
            task_id: ask.asked.task_id.clone(),
            attempt_id: ask.asked.lease.attempt_id.clone(),
            answer: answer.into(),
            actor: actor.into(),
        };
        let record = self
            .inner
            .store
            .append_host_event(&ask.asked.lease, ANSWERED, serde_json::to_value(&answered)?)
            .map_err(|error| match error {
                CoreError::StaleLease => RuntimeError::StaleApproval,
                other => RuntimeError::Core(other),
            })?;
        self.inner.approvals.notify_waiters();
        self.inner.changed.notify_waiters();
        let stored = SeenAnswer {
            answered,
            timestamp: record.timestamp,
        };
        Ok(view(session, ask, Some(&stored), QuestionState::Answered))
    }
    /// All questions reconstructed from the Core log, including answered and
    /// expired ones. Pending state depends on the current attempt and queue row.
    pub fn questions(&self, session: &str) -> Result<Vec<QuestionView>, RuntimeError> {
        let session_view = self.get(session)?;
        let tasks = self.inner.queue.list(session)?;
        let events = read_log(&self.inner, session)?;
        let log = question_log(&events)?;
        let mut questions = Vec::with_capacity(log.asks.len());
        for ask in &log.asks {
            let answer = log.answers.get(&ask.asked.question_id);
            let state = if answer.is_some() {
                QuestionState::Answered
            } else if question_active(
                &self.inner,
                session,
                session_view.attempt.as_ref(),
                session_view.state,
                &tasks,
                &session_view.unresolved_capabilities,
                &ask.asked,
            )? {
                QuestionState::Pending
            } else {
                QuestionState::Expired
            };
            questions.push(view(session, ask, answer, state));
        }
        Ok(questions)
    }
    pub fn pending_questions(&self, session: &str) -> Result<Vec<QuestionView>, RuntimeError> {
        Ok(self
            .questions(session)?
            .into_iter()
            .filter(|question| question.state == QuestionState::Pending)
            .collect())
    }
    /// Current task plan, or the most recent plan in the session while idle.
    /// Returns `None` when no plan event exists.
    pub fn plan(&self, session: &str) -> Result<Option<PlanView>, RuntimeError> {
        let view = self.get(session)?;
        let tasks = self.inner.queue.list(session)?;
        let events = read_log(&self.inner, session)?;
        let current = view.attempt.as_ref().and_then(|lease| {
            tasks
                .iter()
                .find(|task| task.attempt_id.as_deref() == Some(&lease.attempt_id))
                .map(|task| task.id.clone())
        });
        let stored = match current {
            Some(task_id) => latest_plan(&events, &task_id)?,
            None => latest_any_plan(&events)?,
        };
        Ok(stored.map(|stored| PlanView {
            session_id: session.into(),
            task_id: stored.update.task_id,
            attempt_id: stored.update.attempt_id,
            version: stored.update.version,
            previous_version: stored.update.previous_version,
            source: stored.update.source,
            steps: stored.update.steps,
            updated_at: stored.timestamp,
            cursor: stored.cursor,
        }))
    }
}
