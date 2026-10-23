//! Host-owned interactive terminal sessions.
//!
//! A terminal is started only from an application-selected terminal profile.
//! The model supplies a profile name and, after the host returns a session ID,
//! operations addressed to that exact session. It never supplies a program,
//! argument vector, environment, working directory, or shell command line.
//!
//! Sessions live in one task's Host. The manager owns each session until the
//! process supervisor has joined; dropping a future only requests a stop and is
//! not cleanup proof. `drain` is the proof boundary used by the Runtime before a
//! task may complete. Unknown outcomes are retained and are never converted to a
//! successful terminal state by waiting, reading, or closing.

use crate::{Profile, check, failure, io_error, lock, process};
use cayw_ai::{CancellationToken, ToolDefinition};
use cayw_codemode::CapabilityError;
use cayw_device::{
    DeviceError, DirectoryProcessSpec, InteractiveLimits, ManagedStdin, ManagedStdout,
    ProcessControl, ProcessExit, ProcessIo, ProcessRunner, ResourceLimits, ResourceRequest,
    ResourceScheduler, SessionId, TerminationReason,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::Notify;

const MAX_ID_BYTES: usize = 128;
const MAX_SESSIONS: usize = 1024;
const MAX_TOTAL_SESSIONS: usize = 65_536;
const MAX_OUTPUT_BYTES: usize = 64 * 1024 * 1024;
const MAX_INPUT_BYTES: usize = 64 * 1024 * 1024;
const MAX_READ_BYTES: usize = 1024 * 1024;
const MAX_WAIT: Duration = Duration::from_secs(300);
const MAX_REQUESTS_PER_SESSION: usize = 4096;

static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);

/// Bounds applying to one Host's terminal manager.
///
/// Counts are lifetime budgets for this manager. They are not reset by a wait,
/// read, close, or cancel. The per-session output budget is also capped by the
/// selected profile's `max_output_bytes`.
#[derive(Clone, Copy, Debug)]
pub struct TerminalLimits {
    pub max_sessions: usize,
    pub max_sessions_total: usize,
    pub max_input_bytes_per_write: usize,
    pub max_input_bytes_per_session: usize,
    pub max_output_bytes_per_session: usize,
    pub max_read_bytes: usize,
    pub max_wait: Duration,
}

impl Default for TerminalLimits {
    fn default() -> Self {
        Self {
            max_sessions: 4,
            max_sessions_total: 64,
            max_input_bytes_per_write: 64 * 1024,
            max_input_bytes_per_session: 1024 * 1024,
            max_output_bytes_per_session: 1024 * 1024,
            max_read_bytes: 64 * 1024,
            max_wait: Duration::from_secs(30),
        }
    }
}

/// Explicit host opt-in. Every terminal process must select one of these
/// profiles; the profile's kind and filter fields are not used for terminals.
#[derive(Clone, Debug)]
pub struct TerminalConfig {
    pub profiles: BTreeMap<String, Profile>,
    pub limits: TerminalLimits,
}

pub(crate) fn validate_config(config: &TerminalConfig) -> Result<(), CapabilityError> {
    if config.profiles.is_empty() {
        return Err(failure(
            "terminal_handoffs requires at least one terminal profile",
        ));
    }
    let limits = config.limits;
    if limits.max_sessions == 0
        || limits.max_sessions > MAX_SESSIONS
        || limits.max_sessions_total < limits.max_sessions
        || limits.max_sessions_total > MAX_TOTAL_SESSIONS
        || limits.max_input_bytes_per_write == 0
        || limits.max_input_bytes_per_write > MAX_INPUT_BYTES
        || limits.max_input_bytes_per_session < limits.max_input_bytes_per_write
        || limits.max_input_bytes_per_session > MAX_INPUT_BYTES
        || limits.max_output_bytes_per_session == 0
        || limits.max_output_bytes_per_session > MAX_OUTPUT_BYTES
        || limits.max_read_bytes == 0
        || limits.max_read_bytes > MAX_READ_BYTES
        || limits.max_read_bytes > limits.max_output_bytes_per_session
        || limits.max_wait.is_zero()
        || limits.max_wait > MAX_WAIT
        || Instant::now().checked_add(limits.max_wait).is_none()
    {
        return Err(failure("invalid terminal limits"));
    }
    for (name, profile) in &config.profiles {
        if !process::valid_name(name) {
            return Err(failure("invalid terminal profile name"));
        }
        process::validate_program(&profile.program)?;
        if profile.allow_filter {
            return Err(failure("terminal profiles cannot allow a filter"));
        }
        if profile.timeout.is_zero()
            || Instant::now().checked_add(profile.timeout).is_none()
            || profile.max_output_bytes == 0
            || profile.args.len() > 256
            || profile
                .args
                .iter()
                .any(|argument| argument.contains('\0') || argument.len() > 8192)
            || profile.env.len() > 256
            || profile.env.iter().any(|(key, value)| {
                key.is_empty()
                    || key.contains(['=', '\0'])
                    || value.contains('\0')
                    || key.len() > 1024
                    || value.len() > 32768
            })
        {
            return Err(failure("invalid terminal profile"));
        }
        match profile.io {
            ProcessIo::Pipes => {}
            ProcessIo::Pty { columns, rows } => {
                if columns == 0 || rows == 0 {
                    return Err(failure("terminal dimensions must be positive"));
                }
                #[cfg(not(target_os = "macos"))]
                return Err(failure("terminal PTY is only supported on macOS"));
            }
        }
    }
    Ok(())
}

/// Catalog entries for one explicitly enabled terminal manager. The Host maps
/// both names to `Executor::Terminal`; this function does not decide execution.
pub(crate) fn definitions(config: &TerminalConfig) -> Vec<ToolDefinition> {
    let profiles: Vec<String> = config.profiles.keys().cloned().collect();
    let limits = config.limits;
    vec![
        ToolDefinition {
            name: "exec_command".into(),
            description: "Start one application-selected interactive terminal profile. The host fixes the program, arguments, environment, working directory and I/O mode; the caller selects only a configured profile. Returns a host-generated session_id.".into(),
            parameters: json!({
                "type":"object",
                "properties":{"profile":{"type":"string","enum":profiles}},
                "required":["profile"],
                "additionalProperties":false
            }),
        },
        ToolDefinition {
            name: "write_stdin".into(),
            description: "Operate one host-owned terminal session. write sends raw UTF-8 text and is idempotent for a request_id; read pages raw bytes by absolute cursor; wait observes without extending the lifetime; close and cancel stop and join without claiming business rollback.".into(),
            parameters: json!({
                "type":"object",
                "properties":{
                    "session_id":{"type":"string","minLength":1,"maxLength":MAX_ID_BYTES},
                    "action":{"type":"string","enum":["write","read","wait","close","cancel"]},
                    "request_id":{"type":"string","minLength":1,"maxLength":MAX_ID_BYTES},
                    "data":{"type":"string","maxLength":limits.max_input_bytes_per_write},
                    "cursor":{"type":"integer","minimum":0},
                    "expected_cursor":{"type":"integer","minimum":0},
                    "max_bytes":{"type":"integer","minimum":1,"maximum":limits.max_read_bytes},
                    "wait_ms":{"type":"integer","minimum":0,"maximum":limits.max_wait.as_millis().min(u64::MAX as u128) as u64}
                },
                "required":["session_id","action","request_id"],
                "additionalProperties":false
            }),
        },
    ]
}

pub(crate) fn validate_arguments(
    name: &str,
    arguments: &Value,
    config: &TerminalConfig,
) -> Result<(), CapabilityError> {
    match name {
        "exec_command" => {
            let profile = arguments
                .get("profile")
                .and_then(Value::as_str)
                .ok_or_else(|| failure("terminal profile is required"))?;
            if !process::valid_name(profile) || !config.profiles.contains_key(profile) {
                return Err(failure("terminal profile is not configured"));
            }
        }
        "write_stdin" => validate_write_stdin(arguments, config)?,
        _ => {}
    }
    Ok(())
}

fn validate_write_stdin(arguments: &Value, config: &TerminalConfig) -> Result<(), CapabilityError> {
    identifier(arguments, "session_id")?;
    identifier(arguments, "request_id")?;
    let action = arguments
        .get("action")
        .and_then(Value::as_str)
        .ok_or_else(|| failure("terminal action is required"))?;
    let cursor = optional_u64(arguments, "cursor")?;
    let expected_cursor = optional_u64(arguments, "expected_cursor")?;
    let max_bytes = optional_u64(arguments, "max_bytes")?;
    let wait_ms = optional_u64(arguments, "wait_ms")?;
    let data = arguments.get("data").and_then(Value::as_str);
    match action {
        "write" => {
            let data = data.ok_or_else(|| failure("terminal write requires data"))?;
            if data.len() > config.limits.max_input_bytes_per_write {
                return Err(failure("terminal write exceeds max_input_bytes_per_write"));
            }
            if cursor.is_some() || max_bytes.is_some() || wait_ms.is_some() {
                return Err(failure(
                    "terminal write does not accept read or wait fields",
                ));
            }
        }
        "read" => {
            if cursor.is_none() {
                return Err(failure("terminal read requires cursor"));
            }
            if expected_cursor.is_some() || data.is_some() || wait_ms.is_some() {
                return Err(failure(
                    "terminal read does not accept write or wait fields",
                ));
            }
            if let Some(max_bytes) = max_bytes
                && (max_bytes == 0 || max_bytes > config.limits.max_read_bytes as u64)
            {
                return Err(failure("terminal read max_bytes is out of range"));
            }
        }
        "wait" => {
            if cursor.is_some()
                || expected_cursor.is_some()
                || max_bytes.is_some()
                || data.is_some()
            {
                return Err(failure("terminal wait only accepts wait_ms"));
            }
            if wait_ms.is_some_and(|value| {
                value > config.limits.max_wait.as_millis().min(u64::MAX as u128) as u64
            }) {
                return Err(failure("terminal wait_ms is out of range"));
            }
        }
        "close" | "cancel" => {
            if cursor.is_some() || max_bytes.is_some() || wait_ms.is_some() || data.is_some() {
                return Err(failure(
                    "terminal close or cancel does not accept data fields",
                ));
            }
        }
        _ => return Err(failure("invalid terminal action")),
    }
    Ok(())
}

fn identifier<'a>(arguments: &'a Value, field: &str) -> Result<&'a str, CapabilityError> {
    let value = arguments
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| failure(format!("terminal {field} is required")))?;
    if value.is_empty() || value.len() > MAX_ID_BYTES || value.chars().any(char::is_control) {
        return Err(failure(format!("terminal {field} is invalid")));
    }
    Ok(value)
}

fn optional_u64(arguments: &Value, field: &str) -> Result<Option<u64>, CapabilityError> {
    match arguments.get(field) {
        None => Ok(None),
        Some(Value::Number(number)) => number
            .as_u64()
            .map(Some)
            .ok_or_else(|| failure(format!("terminal {field} must be an unsigned integer"))),
        Some(_) => Err(failure(format!(
            "terminal {field} must be an unsigned integer"
        ))),
    }
}

#[derive(Clone, Debug)]
enum State {
    Running,
    Closing,
    Exited {
        exit_code: Option<i32>,
        termination: &'static str,
        output_complete: bool,
    },
    Unknown {
        reason: String,
    },
}

#[derive(Debug, Default)]
struct Output {
    bytes: Vec<u8>,
    /// Highest contiguous byte offset already delivered through `read`.
    delivered: usize,
    eof: bool,
    truncated: bool,
}

#[derive(Clone)]
struct Request {
    action: &'static str,
    result: Value,
}

struct TerminalSession {
    id: String,
    profile: String,
    started: Instant,
    deadline: Instant,
    max_output: usize,
    limits: TerminalLimits,
    stdin: tokio::sync::Mutex<Option<ManagedStdin>>,
    control: ProcessControl,
    state: Mutex<State>,
    output: Mutex<Output>,
    input_bytes: Mutex<usize>,
    requests: Mutex<BTreeMap<String, Request>>,
    write_lock: tokio::sync::Mutex<()>,
    shutdown_lock: tokio::sync::Mutex<()>,
    reader: Mutex<Option<tokio::task::JoinHandle<()>>>,
    waiter: Mutex<Option<tokio::task::JoinHandle<()>>>,
    joined: AtomicBool,
    changed: Notify,
}

#[derive(Default)]
struct Totals {
    created: usize,
}

/// Shared per-task terminal ledger. It deliberately holds no `ProcessRunner`
/// or directory handle: each launch receives the current Host scope's observed
/// runner and pinned root, so a CallScope's process observer is never dropped.
pub(crate) struct TerminalManager {
    config: TerminalConfig,
    scheduler: ResourceScheduler,
    sessions: Mutex<BTreeMap<String, Arc<TerminalSession>>>,
    totals: Mutex<Totals>,
    closed: AtomicBool,
    unknown: Mutex<Option<String>>,
    /// Serializes startup/insertion with drain's session snapshot. A process
    /// launched while close was requested is shut down and retained as unknown
    /// instead of being inserted after the snapshot.
    start_gate: tokio::sync::Mutex<()>,
    /// Completion facts survive `drain`, which clears the session map after the
    /// original supervisors and reader tasks have joined.
    retained_completion: Mutex<Option<String>>,
}

impl TerminalManager {
    pub(crate) fn new(config: TerminalConfig) -> Result<Self, CapabilityError> {
        validate_config(&config)?;
        let scheduler = ResourceScheduler::new(ResourceLimits {
            max_concurrent_tasks: config.limits.max_sessions,
            max_declared_memory_bytes: 0,
        })
        .map_err(|error| failure(error.to_string()))?;
        Ok(Self {
            config,
            scheduler,
            sessions: Mutex::default(),
            totals: Mutex::default(),
            closed: AtomicBool::new(false),
            unknown: Mutex::default(),
            start_gate: tokio::sync::Mutex::new(()),
            retained_completion: Mutex::default(),
        })
    }

    pub(crate) fn close(&self) {
        let sessions = lock(&self.sessions);
        self.closed.store(true, Ordering::Release);
        for session in sessions.values() {
            session.signal_close();
        }
    }

    /// Requests every session to stop and proves process cleanup by awaiting the
    /// original supervisor and the session tasks. It does not call the shared
    /// runner's drain; the Host calls that after this method.
    pub(crate) async fn drain(&self) -> Result<(), DeviceError> {
        self.close();
        let sessions: Vec<Arc<TerminalSession>> = {
            let _start = self.start_gate.lock().await;
            lock(&self.sessions).values().cloned().collect()
        };
        let mut tasks = tokio::task::JoinSet::new();
        for session in sessions {
            tasks.spawn(async move {
                let id = session.id.clone();
                (id, session.shutdown().await)
            });
        }
        let mut failures = Vec::new();
        while let Some(joined) = tasks.join_next().await {
            match joined {
                Ok((_, Ok(()))) => {}
                Ok((id, Err(reason))) => {
                    self.mark_unknown(format!("{id}: {reason}"));
                    failures.push(format!("{id}: {reason}"));
                }
                Err(error) => {
                    self.mark_unknown(format!("terminal cleanup task failed: {error}"));
                    failures.push(format!("terminal cleanup task failed: {error}"));
                }
            }
        }
        if let Some(reason) = self.completion_error() {
            lock(&self.retained_completion).get_or_insert(reason);
        }
        lock(&self.sessions).clear();
        if let Some(reason) = self.unknown() {
            failures.push(reason);
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(DeviceError::CleanupUnconfirmed(failures.join("; ")))
        }
    }

    pub(crate) fn unknown(&self) -> Option<String> {
        if let Some(reason) = lock(&self.unknown).clone() {
            return Some(reason);
        }
        lock(&self.sessions)
            .values()
            .find_map(|session| session.unknown_reason())
    }

    /// Sessions whose original supervisor has not joined yet.
    pub(crate) fn active_sessions(&self) -> usize {
        lock(&self.sessions)
            .values()
            .filter(|session| !session.joined.load(Ordering::Acquire))
            .count()
    }

    /// Why this task must not be reported complete yet. A live session, an
    /// unknown outcome, a non-zero exit, or output not fully delivered to the
    /// caller all keep the task open. Reads advance one contiguous delivered
    /// offset, so a caller cannot skip a cursor gap and hide unread bytes.
    pub(crate) fn completion_error(&self) -> Option<String> {
        if let Some(reason) = lock(&self.retained_completion).clone() {
            return Some(reason);
        }
        if let Some(reason) = lock(&self.unknown).clone() {
            return Some(format!("terminal manager outcome is unknown: {reason}"));
        }
        for (id, session) in lock(&self.sessions).iter() {
            let state = lock(&session.state).clone();
            match &state {
                State::Unknown { reason } => {
                    return Some(format!(
                        "terminal session {id} outcome is unknown: {reason}"
                    ));
                }
                State::Running | State::Closing => {
                    return Some(format!("terminal session {id} is still running"));
                }
                State::Exited { .. } => {}
            }
            if !session.joined.load(Ordering::Acquire) {
                return Some(format!("terminal session {id} supervisor has not joined"));
            }
            if let State::Exited { exit_code, .. } = state {
                match exit_code {
                    Some(0) => {}
                    Some(code) => {
                        return Some(format!("terminal session {id} exited with code {code}"));
                    }
                    None => {
                        return Some(format!("terminal session {id} did not exit normally"));
                    }
                }
            }
            let output = lock(&session.output);
            if output.truncated {
                return Some(format!("terminal session {id} output was truncated"));
            }
            if !output.eof {
                return Some(format!("terminal session {id} output has not reached EOF"));
            }
            if output.delivered < output.bytes.len() {
                return Some(format!(
                    "terminal session {id} has {} unread output bytes",
                    output.bytes.len() - output.delivered
                ));
            }
        }
        None
    }

    fn mark_unknown(&self, reason: impl Into<String>) {
        lock(&self.unknown).get_or_insert_with(|| reason.into());
    }

    fn session(&self, id: &str) -> Result<Arc<TerminalSession>, CapabilityError> {
        lock(&self.sessions)
            .get(id)
            .cloned()
            .ok_or_else(|| failure("terminal session is not owned by this task"))
    }

    pub(crate) async fn exec(
        &self,
        root: &cap_std::fs::Dir,
        runner: Arc<ProcessRunner>,
        profile_name: &str,
        cancel: &CancellationToken,
        call_deadline: Instant,
    ) -> Result<Value, CapabilityError> {
        // Hold the startup gate across launch and registration so `drain` cannot
        // snapshot the session map while a process is being launched.
        let _start = self.start_gate.lock().await;
        if self.closed.load(Ordering::Acquire) {
            return Err(failure("terminal manager is closed"));
        }
        check(cancel, call_deadline)?;
        let profile = self
            .config
            .profiles
            .get(profile_name)
            .cloned()
            .ok_or_else(|| failure("terminal profile is not configured"))?;
        let limits = self.config.limits;
        let max_output = profile
            .max_output_bytes
            .min(limits.max_output_bytes_per_session);
        let started = Instant::now();
        let deadline = started
            .checked_add(profile.timeout)
            .ok_or_else(|| failure("terminal session lifetime is not representable"))?;
        {
            let mut totals = lock(&self.totals);
            if totals.created >= limits.max_sessions_total {
                return Err(failure("terminal session lifetime budget exhausted"));
            }
            totals.created += 1;
        }
        let session_id = self.new_session_id();
        let permit = match self.scheduler.try_acquire(
            SessionId::new(session_id.clone()).map_err(|error| failure(error.to_string()))?,
            ResourceRequest::default(),
        ) {
            Ok(permit) => permit,
            Err(error) => {
                self.rollback_total();
                return Err(failure(error.to_string()));
            }
        };
        let spec = match spec(root, &profile, max_output) {
            Ok(spec) => spec,
            Err(error) => {
                self.rollback_total();
                return Err(error);
            }
        };
        let session_cancel = CancellationToken::new();
        let interactive_limits = InteractiveLimits {
            max_write_bytes: limits.max_input_bytes_per_write,
            max_buffered_stdout_bytes: max_output,
        };
        let start = runner.start_interactive_in_directory_with_permit(
            spec,
            interactive_limits,
            session_cancel.clone(),
            permit,
        );
        tokio::pin!(start);
        let process = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                session_cancel.cancel();
                self.mark_unknown("exec_command cancelled during terminal startup");
                return Err(CapabilityError::unknown(
                    "exec_command cancelled during terminal startup; process cleanup is pending",
                ));
            }
            _ = tokio::time::sleep_until(call_deadline.into()) => {
                session_cancel.cancel();
                self.mark_unknown("terminal startup deadline reached");
                return Err(CapabilityError::unknown(
                    "terminal startup deadline reached; process cleanup is pending",
                ));
            }
            result = &mut start => match result {
                Ok(process) => process,
                Err(error) => {
                    self.rollback_total();
                    let mapped = start_error(error);
                    if mapped.outcome_unknown {
                        self.mark_unknown(mapped.message.clone());
                    }
                    return Err(mapped);
                }
            },
        };
        let session = Arc::new(TerminalSession {
            id: session_id.clone(),
            profile: profile_name.into(),
            started,
            deadline,
            max_output,
            limits,
            stdin: tokio::sync::Mutex::new(Some(process.stdin)),
            control: process.control,
            state: Mutex::new(State::Running),
            output: Mutex::default(),
            input_bytes: Mutex::default(),
            requests: Mutex::default(),
            write_lock: tokio::sync::Mutex::new(()),
            shutdown_lock: tokio::sync::Mutex::new(()),
            reader: Mutex::default(),
            waiter: Mutex::default(),
            joined: AtomicBool::new(false),
            changed: Notify::new(),
        });
        let reader = tokio::spawn(read_output(session.clone(), process.stdout));
        let waiter = tokio::spawn(wait_process(session.clone(), session.control.clone()));
        *lock(&session.reader) = Some(reader);
        *lock(&session.waiter) = Some(waiter);
        {
            let mut sessions = lock(&self.sessions);
            if self.closed.load(Ordering::Acquire) {
                drop(sessions);
                let detail = session
                    .shutdown()
                    .await
                    .err()
                    .unwrap_or_else(|| "process cleanup completed".into());
                self.mark_unknown(format!(
                    "terminal manager closed during terminal startup; the session was shut down without being registered ({detail})"
                ));
                return Err(CapabilityError::unknown(
                    "terminal manager closed during terminal startup; the process was shut down and the outcome is unknown",
                ));
            }
            sessions.insert(session_id.clone(), session.clone());
        }
        Ok(json!({
            "session_id": session_id,
            "profile": profile_name,
            "state": "running",
            "output_cursor": 0,
            "input_bytes": 0,
            "remaining_ms": deadline.saturating_duration_since(Instant::now()).as_millis().min(u64::MAX as u128) as u64,
        }))
    }

    fn new_session_id(&self) -> String {
        for _ in 0..16 {
            let id = format!(
                "term-{:x}-{:x}-{:x}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |duration| duration.as_nanos()),
                NEXT_SESSION.fetch_add(1, Ordering::Relaxed)
            );
            if !lock(&self.sessions).contains_key(&id) {
                return id;
            }
        }
        format!("term-{}", NEXT_SESSION.fetch_add(1, Ordering::Relaxed))
    }

    fn rollback_total(&self) {
        let mut totals = lock(&self.totals);
        totals.created = totals.created.saturating_sub(1);
    }

    pub(crate) async fn write_stdin(
        &self,
        arguments: &Value,
        cancel: &CancellationToken,
        deadline: Instant,
    ) -> Result<Value, CapabilityError> {
        check(cancel, deadline)?;
        let session_id = identifier(arguments, "session_id")?;
        let request_id = identifier(arguments, "request_id")?;
        let action = arguments
            .get("action")
            .and_then(Value::as_str)
            .ok_or_else(|| failure("terminal action is required"))?;
        let session = self.session(session_id)?;
        if self.closed.load(Ordering::Acquire) && action == "write" {
            return Err(failure("terminal manager is closed"));
        }
        match action {
            "write" => {
                let data = arguments
                    .get("data")
                    .and_then(Value::as_str)
                    .ok_or_else(|| failure("terminal write requires data"))?;
                let expected = optional_u64(arguments, "expected_cursor")?;
                session
                    .write(request_id, data, expected, cancel, deadline)
                    .await
            }
            "read" => {
                let cursor = optional_u64(arguments, "cursor")?
                    .ok_or_else(|| failure("terminal read requires cursor"))?;
                let max_bytes = optional_u64(arguments, "max_bytes")?
                    .map_or(session.limits.max_read_bytes, |value| value as usize);
                session.read(request_id, cursor, max_bytes)
            }
            "wait" => {
                let wait_ms = optional_u64(arguments, "wait_ms")?.unwrap_or(0);
                session.wait(request_id, wait_ms, cancel, deadline).await
            }
            "close" | "cancel" => {
                let expected = optional_u64(arguments, "expected_cursor")?;
                let action = if action == "close" { "close" } else { "cancel" };
                session.stop(request_id, action, expected).await
            }
            _ => Err(failure("invalid terminal action")),
        }
    }
}

fn spec(
    root: &cap_std::fs::Dir,
    profile: &Profile,
    max_output: usize,
) -> Result<DirectoryProcessSpec, CapabilityError> {
    Ok(DirectoryProcessSpec {
        program: profile.program.clone(),
        args: profile.args.clone(),
        cwd: root.try_clone().map_err(io_error)?.into_std_file(),
        env: profile.env.clone(),
        timeout: profile.timeout,
        max_output_bytes: max_output,
        io: profile.io,
    })
}

fn start_error(error: DeviceError) -> CapabilityError {
    match error {
        DeviceError::NotStarted { .. }
        | DeviceError::InvalidSpec(_)
        | DeviceError::ProgramNotAllowed(_)
        | DeviceError::Cancelled
        | DeviceError::RunnerClosed
        | DeviceError::ResourcesBusy
        | DeviceError::ResourceRequestTooLarge
        | DeviceError::UnsupportedResource(_)
        | DeviceError::InvalidLimits(_)
        | DeviceError::InvalidSessionId
        | DeviceError::UnsupportedPlatform => failure(error.to_string()),
        _ => CapabilityError::unknown(error.to_string()),
    }
}

impl TerminalSession {
    fn is_live(&self) -> bool {
        matches!(*lock(&self.state), State::Running | State::Closing)
    }

    fn is_running(&self) -> bool {
        matches!(*lock(&self.state), State::Running)
    }

    fn unknown_reason(&self) -> Option<String> {
        match &*lock(&self.state) {
            State::Unknown { reason } => Some(reason.clone()),
            _ => None,
        }
    }

    fn terminal_error(&self) -> Option<CapabilityError> {
        self.unknown_reason().map(|reason| {
            CapabilityError::unknown(format!("terminal outcome is unknown: {reason}"))
        })
    }

    fn signal_close(&self) {
        {
            let mut state = lock(&self.state);
            if matches!(*state, State::Running) {
                *state = State::Closing;
            }
        }
        self.control.close();
        self.changed.notify_waiters();
    }

    fn mark_unknown(&self, reason: impl Into<String>) {
        let mut state = lock(&self.state);
        if !matches!(*state, State::Unknown { .. }) {
            *state = State::Unknown {
                reason: reason.into(),
            };
        }
        drop(state);
        self.changed.notify_waiters();
    }

    fn finish(&self, outcome: Result<ProcessExit, Arc<DeviceError>>) {
        self.joined.store(true, Ordering::Release);
        match outcome {
            Ok(exit) if exit.termination == TerminationReason::Exited && exit.output_complete => {
                let mut state = lock(&self.state);
                if !matches!(*state, State::Unknown { .. }) {
                    *state = State::Exited {
                        exit_code: exit.status.code(),
                        termination: "exited",
                        output_complete: true,
                    };
                }
            }
            Ok(exit) => self.mark_unknown(format!(
                "terminal process stopped with {:?}; complete={}",
                exit.termination, exit.output_complete
            )),
            Err(error) => self.mark_unknown(format!(
                "terminal process cleanup failed: {}",
                error.as_ref()
            )),
        }
        self.changed.notify_waiters();
    }

    fn snapshot(&self) -> Value {
        let state = lock(&self.state).clone();
        let output = lock(&self.output);
        let (name, exit_code, termination, output_complete, unknown) = match &state {
            State::Running => ("running", None, None, None, None),
            State::Closing => ("closing", None, None, None, None),
            State::Exited {
                exit_code,
                termination,
                output_complete,
            } => (
                "exited",
                *exit_code,
                Some(*termination),
                Some(*output_complete),
                None,
            ),
            State::Unknown { reason } => ("unknown", None, None, None, Some(reason.clone())),
        };
        let input_bytes = *lock(&self.input_bytes);
        json!({
            "session_id": self.id,
            "profile": self.profile,
            "state": name,
            "running": matches!(state, State::Running | State::Closing),
            "exit_code": exit_code,
            "termination": termination,
            "output_complete": output_complete,
            "truncated": output.truncated,
            "output_cursor": output.bytes.len(),
            "consumed_bytes": output.delivered,
            "unread_bytes": output.bytes.len().saturating_sub(output.delivered),
            "input_bytes": input_bytes,
            "elapsed_ms": self.started.elapsed().as_millis().min(u64::MAX as u128) as u64,
            "remaining_ms": self.deadline.saturating_duration_since(Instant::now()).as_millis().min(u64::MAX as u128) as u64,
            "unknown": unknown,
        })
    }

    fn read(
        &self,
        request_id: &str,
        cursor: u64,
        max_bytes: usize,
    ) -> Result<Value, CapabilityError> {
        if let Some(error) = self.terminal_error() {
            return Err(error);
        }
        let mut output = lock(&self.output);
        let cursor = usize::try_from(cursor)
            .map_err(|_| failure("terminal output cursor is out of range"))?;
        if cursor > output.delivered {
            return Err(failure(
                "terminal output cursor skips unread bytes; read the delivered prefix first",
            ));
        }
        if cursor > output.bytes.len() {
            return Err(failure("terminal output cursor is ahead of the session"));
        }
        let end = cursor
            .saturating_add(max_bytes.min(self.limits.max_read_bytes))
            .min(output.bytes.len());
        // The canonical payload is raw bytes. `text` is display-only and may
        // contain replacement characters when a page splits a UTF-8 sequence.
        let raw = output.bytes[cursor..end].to_vec();
        let text = String::from_utf8_lossy(&raw).into_owned();
        let bytes = end - cursor;
        let eof = output.eof;
        let truncated = output.truncated;
        output.delivered = output.delivered.max(end);
        let delivered = output.delivered;
        drop(output);
        Ok(json!({
            "session_id": self.id,
            "request_id": request_id,
            "action": "read",
            "cursor": cursor,
            "next_cursor": end,
            "bytes": raw,
            "text": text,
            "data_bytes": bytes,
            "consumed_bytes": delivered,
            "eof": eof,
            "truncated": truncated,
            "status": self.snapshot(),
        }))
    }

    async fn write(
        &self,
        request_id: &str,
        data: &str,
        expected_cursor: Option<u64>,
        cancel: &CancellationToken,
        deadline: Instant,
    ) -> Result<Value, CapabilityError> {
        let _write = self.write_lock.lock().await;
        if let Some(record) = lock(&self.requests).get(request_id).cloned() {
            if record.action != "write" {
                return Err(failure(
                    "terminal request_id was already used for a different action",
                ));
            }
            return Ok(record.result);
        }
        if let Some(error) = self.terminal_error() {
            return Err(error);
        }
        if !self.is_running() {
            return Err(failure("terminal session is not running"));
        }
        if let Some(expected) = expected_cursor
            && expected != lock(&self.output).bytes.len() as u64
        {
            return Err(failure("terminal output cursor is stale"));
        }
        if data.len() > self.limits.max_input_bytes_per_write {
            return Err(failure("terminal write exceeds max_input_bytes_per_write"));
        }
        {
            let input = lock(&self.input_bytes);
            if data
                .len()
                .checked_add(*input)
                .is_none_or(|total| total > self.limits.max_input_bytes_per_session)
            {
                return Err(failure("terminal session input budget exhausted"));
            }
        }
        if lock(&self.requests).len() >= MAX_REQUESTS_PER_SESSION {
            return Err(failure("terminal request ledger is full"));
        }
        let mut stdin = self.stdin.lock().await;
        let result = {
            let Some(stdin) = stdin.as_mut() else {
                return Err(failure("terminal stdin is closed"));
            };
            let write = stdin.write_all(data.as_bytes());
            tokio::pin!(write);
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    self.mark_unknown("terminal write cancelled; a partial write may have reached the process");
                    return Err(CapabilityError::unknown(
                        "terminal write cancelled; a partial write may have reached the process",
                    ));
                }
                _ = tokio::time::sleep_until(deadline.into()) => {
                    self.mark_unknown("terminal write deadline reached; a partial write may have reached the process");
                    return Err(CapabilityError::unknown(
                        "terminal write deadline reached; a partial write may have reached the process",
                    ));
                }
                result = &mut write => result,
            }
        };
        drop(stdin);
        if let Err(error) = result {
            if self.is_running() {
                self.mark_unknown(format!("terminal write failed: {error}"));
                return Err(CapabilityError::unknown(format!(
                    "terminal write failed: {error}"
                )));
            }
            return Err(failure(format!("terminal write failed: {error}")));
        }
        let mut input = lock(&self.input_bytes);
        *input += data.len();
        let input_total = *input;
        drop(input);
        let output_cursor = lock(&self.output).bytes.len();
        let result = json!({
            "session_id": self.id,
            "request_id": request_id,
            "action": "write",
            "accepted_bytes": data.len(),
            "input_bytes": input_total,
            "output_cursor": output_cursor,
            "status": self.snapshot(),
        });
        lock(&self.requests).insert(
            request_id.into(),
            Request {
                action: "write",
                result: result.clone(),
            },
        );
        Ok(result)
    }

    async fn wait(
        &self,
        request_id: &str,
        wait_ms: u64,
        cancel: &CancellationToken,
        deadline: Instant,
    ) -> Result<Value, CapabilityError> {
        if let Some(error) = self.terminal_error() {
            return Err(error);
        }
        if self.is_live() {
            let wait = Duration::from_millis(
                wait_ms.min(self.limits.max_wait.as_millis().min(u64::MAX as u128) as u64),
            );
            let end = Instant::now()
                .checked_add(wait)
                .map_or(deadline, |value| value.min(deadline));
            let control = self.control.clone();
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    return Err(failure("cancelled while waiting for terminal"));
                }
                _ = tokio::time::sleep_until(end.into()) => {}
                outcome = control.wait() => self.finish(outcome),
            }
        }
        if let Some(error) = self.terminal_error() {
            return Err(error);
        }
        Ok(json!({
            "session_id": self.id,
            "request_id": request_id,
            "action": "wait",
            "status": self.snapshot(),
        }))
    }

    async fn stop(
        &self,
        request_id: &str,
        action: &'static str,
        expected_cursor: Option<u64>,
    ) -> Result<Value, CapabilityError> {
        let _write = self.write_lock.lock().await;
        if let Some(record) = lock(&self.requests).get(request_id).cloned() {
            if record.action != action {
                return Err(failure(
                    "terminal request_id was already used for a different action",
                ));
            }
            return Ok(record.result);
        }
        if let Some(expected) = expected_cursor
            && expected != lock(&self.output).bytes.len() as u64
        {
            return Err(failure("terminal output cursor is stale"));
        }
        let cleanup = self.shutdown().await;
        let result = json!({
            "session_id": self.id,
            "request_id": request_id,
            "action": action,
            "status": self.snapshot(),
        });
        if let Err(reason) = cleanup {
            self.mark_unknown(reason.clone());
            return Err(CapabilityError::unknown(format!(
                "terminal {action} did not confirm a known outcome: {reason}"
            )));
        }
        let mut requests = lock(&self.requests);
        if requests.len() < MAX_REQUESTS_PER_SESSION {
            requests.insert(
                request_id.into(),
                Request {
                    action,
                    result: result.clone(),
                },
            );
        }
        drop(requests);
        Ok(result)
    }

    async fn shutdown(&self) -> Result<(), String> {
        let _shutdown = self.shutdown_lock.lock().await;
        self.control.close();
        drop(self.stdin.lock().await.take());
        let outcome = self.control.wait().await;
        self.finish(outcome);
        let waiter = lock(&self.waiter).take();
        if let Some(waiter) = waiter {
            let _ = waiter.await;
        }
        let reader = lock(&self.reader).take();
        if let Some(reader) = reader {
            let _ = reader.await;
        }
        if let Some(reason) = self.unknown_reason() {
            return Err(reason);
        }
        if self.is_live() {
            let reason = "terminal process did not publish a terminal outcome".to_string();
            self.mark_unknown(reason.clone());
            return Err(reason);
        }
        Ok(())
    }
}

async fn read_output(session: Arc<TerminalSession>, mut stdout: ManagedStdout) {
    loop {
        match stdout.recv().await {
            Ok(Some(chunk)) => {
                let mut output = lock(&session.output);
                if output.bytes.len() < session.max_output {
                    let room = session.max_output - output.bytes.len();
                    let keep = chunk.len().min(room);
                    output.bytes.extend_from_slice(&chunk[..keep]);
                    if keep < chunk.len() {
                        output.truncated = true;
                    }
                } else if !chunk.is_empty() {
                    output.truncated = true;
                }
                let truncated = output.truncated;
                drop(output);
                session.changed.notify_waiters();
                if truncated {
                    session.mark_unknown("terminal output limit exceeded");
                    session.control.close();
                }
            }
            Ok(None) => {
                lock(&session.output).eof = true;
                session.changed.notify_waiters();
                // Retain the stdout endpoint until the supervisor has joined.
                // Dropping it at EOF requests a stop and can turn a normal exit
                // into a cancellation if the supervisor has not observed it yet.
                let _ = session.control.wait().await;
                break;
            }
            Err(error) => {
                session.mark_unknown(format!("terminal output read failed: {error}"));
                session.control.close();
                let _ = session.control.wait().await;
                break;
            }
        }
    }
}

async fn wait_process(session: Arc<TerminalSession>, control: ProcessControl) {
    let outcome = control.wait().await;
    session.finish(outcome);
}

pub(crate) async fn execute(
    manager: &TerminalManager,
    root: &cap_std::fs::Dir,
    runner: Arc<ProcessRunner>,
    name: &str,
    arguments: &Value,
    cancel: &CancellationToken,
    deadline: Instant,
) -> Result<Value, CapabilityError> {
    match name {
        "exec_command" => {
            let profile = arguments
                .get("profile")
                .and_then(Value::as_str)
                .ok_or_else(|| failure("terminal profile is required"))?;
            manager.exec(root, runner, profile, cancel, deadline).await
        }
        "write_stdin" => manager.write_stdin(arguments, cancel, deadline).await,
        _ => Err(failure("unknown terminal capability")),
    }
}
