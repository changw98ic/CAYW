//! Reproducible persistence measurement for the Core [`SessionStore`].
//!
//! This example measures only the public synchronous `SessionStore` API against
//! a real SQLite file in WAL mode with `synchronous=FULL` (both set by
//! `SessionStore::open`). It never contacts a model provider and never reads a
//! model credential.
//!
//! Run with:
//!
//! ```sh
//! cargo run -p cayw-core --example refactor_persistence_bench -- --size short
//! cargo run -p cayw-core --example refactor_persistence_bench -- --size medium --category mixed --sessions 8
//! ```
//!
//! Every event is produced through the documented public API, so the store's own
//! validation decides whether the workload is legal. An unexpected rejection
//! aborts the run instead of being skipped, and no internal state is fabricated.
//!
//! `normal` appends complete non-streaming turns whose persisted message history
//! grows, followed by host events, so host append latency includes a growing
//! projection. `mixed` alternates streamed updates and host events and adds a
//! representative subset of the store-level tool lifecycle (including the public
//! `mark_tool_started` boundary) without executing any tool body.
//! `message_update` measures streamed updates.
//!
//! All sessions share one `SessionStore` and therefore one SQLite connection.
//! The workload rotates round-robin across sessions, so those appends are
//! serialized, not concurrent. The projection cache is private and its
//! implementation may change, so this example reports no cache hit rate and
//! never labels a run hot or cold. The checkpoint at the end is the final state
//! after the store is closed; active-checkpoint cost is not measured.
//!
//! What this example deliberately does not claim: the projection cache, the
//! process RSS, and the number of fsync calls per commit are not exposed by the
//! public API or an existing dependency. They are reported as unavailable rather
//! than guessed. Timing a call includes its durable commit wait, so the numbers
//! are end-to-end append latencies, not pure CPU cost.

use cayw_agent::{AgentEvent, ToolResult};
use cayw_ai::{
    AssistantEvent, AssistantMessage, Content, Message, Model, StopReason, ToolCall,
    ToolResultMessage,
};
use cayw_core::{AttemptLease, SessionState, SessionStore};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    error::Error,
    fs,
    path::{Path, PathBuf},
    process,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

type BenchResult<T = ()> = Result<T, Box<dyn Error>>;

/// Text appended by each streamed update. Kept small and deterministic.
const DELTA: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
/// Streaming updates recorded before an assistant message is finalized.
const UPDATES_PER_MESSAGE: usize = 8;
/// Workload operations performed inside one mixed-category turn.
const MIXED_OPS_PER_TURN: usize = 8;
/// Hard bound on workload operations per session.
const MAX_OPS_PER_SESSION: usize = 8_192;
/// Hard bound on total workload operations across sessions.
const MAX_TOTAL_OPS: usize = 262_144;

// These two statements are copied byte-for-byte from the production read paths
// (`store::EVENTS_BY_TYPE` and the wait-log validation in `store/waits.rs`).
// They exist here only to run `EXPLAIN QUERY PLAN` on an independent connection;
// they must be kept in sync with those sources when the production SQL changes.
const EVENTS_BY_TYPE: &str = "SELECT cursor, record FROM core_events
    WHERE session_id=?1 AND json_extract(record, '$.event.type')=?2 AND cursor>?3
    ORDER BY cursor LIMIT ?4";
const WAIT_LOG: &str = "SELECT cursor, record FROM core_events WHERE session_id=?1 AND json_extract(record, '$.event.type')='execution_wait_registered' ORDER BY cursor";

#[derive(Clone, Copy)]
enum Size {
    Short,
    Medium,
    Long,
}

impl Size {
    fn parse(value: &str) -> BenchResult<Self> {
        match value {
            "short" => Ok(Self::Short),
            "medium" => Ok(Self::Medium),
            "long" => Ok(Self::Long),
            other => Err(format!("unknown size {other}; expected short, medium, or long").into()),
        }
    }

    fn ops(self) -> usize {
        match self {
            Self::Short => 64,
            Self::Medium => 512,
            Self::Long => 4_096,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Short => "short",
            Self::Medium => "medium",
            Self::Long => "long",
        }
    }
}

#[derive(Clone, Copy)]
enum Category {
    MessageUpdate,
    Normal,
    Mixed,
}

impl Category {
    fn parse(value: &str) -> BenchResult<Self> {
        match value {
            "message_update" => Ok(Self::MessageUpdate),
            "normal" => Ok(Self::Normal),
            "mixed" => Ok(Self::Mixed),
            other => Err(format!(
                "unknown category {other}; expected message_update, normal, or mixed"
            )
            .into()),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::MessageUpdate => "message_update",
            Self::Normal => "normal",
            Self::Mixed => "mixed",
        }
    }

    /// Documented shape of the final persisted history for `workload` ops in one
    /// session. The plan also computes this count from the real appends; `run`
    /// checks the two agree so the formulas cannot drift silently.
    fn expected_messages(self, workload: usize) -> usize {
        match self {
            Self::MessageUpdate => workload.div_ceil(UPDATES_PER_MESSAGE),
            Self::Normal => workload.saturating_mul(2),
            Self::Mixed => workload.div_ceil(MIXED_OPS_PER_TURN).saturating_mul(2),
        }
    }
}

struct Args {
    size: Size,
    ops: Option<usize>,
    category: Category,
    sessions: usize,
    db: Option<PathBuf>,
    out: Option<PathBuf>,
}

fn main() -> BenchResult {
    let Some(args) = parse_args()? else {
        return Ok(());
    };
    let output = run(&args)?;
    if let Some(out) = &args.out {
        if let Some(parent) = out.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }
        fs::write(out, serde_json::to_string_pretty(&output)? + "\n")?;
    }
    println!("{}", serde_json::to_string(&output)?);
    Ok(())
}

fn parse_args() -> BenchResult<Option<Args>> {
    let mut args = Args {
        size: Size::Medium,
        ops: None,
        category: Category::Mixed,
        sessions: 1,
        db: None,
        out: None,
    };
    let mut iter = std::env::args().skip(1);
    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "--help" | "-h" => {
                print_help();
                return Ok(None);
            }
            "--size" => args.size = Size::parse(&value(&mut iter, "--size")?)?,
            "--category" => args.category = Category::parse(&value(&mut iter, "--category")?)?,
            "--sessions" => {
                let raw = value(&mut iter, "--sessions")?;
                args.sessions = raw
                    .parse()
                    .map_err(|_| format!("--sessions requires an integer, got {raw}"))?;
            }
            "--ops" => {
                let raw = value(&mut iter, "--ops")?;
                args.ops = Some(
                    raw.parse()
                        .map_err(|_| format!("--ops requires an integer, got {raw}"))?,
                );
            }
            "--db" => args.db = Some(PathBuf::from(value(&mut iter, "--db")?)),
            "--out" => args.out = Some(PathBuf::from(value(&mut iter, "--out")?)),
            other => return Err(format!("unknown argument: {other}").into()),
        }
    }

    let ops = args.ops.unwrap_or(args.size.ops());
    if !(1..=MAX_OPS_PER_SESSION).contains(&ops) {
        return Err(format!("--ops must be between 1 and {MAX_OPS_PER_SESSION}").into());
    }
    if !matches!(args.sessions, 1 | 2 | 8 | 32) {
        return Err("--sessions must be one of 1, 2, 8, 32".into());
    }
    if ops.saturating_mul(args.sessions) > MAX_TOTAL_OPS {
        return Err(format!(
            "workload exceeds the {MAX_TOTAL_OPS} appends bound; reduce --size, --ops or --sessions"
        )
        .into());
    }
    args.ops = Some(ops);
    Ok(Some(args))
}

fn value(iter: &mut impl Iterator<Item = String>, flag: &str) -> BenchResult<String> {
    match iter.next() {
        Some(value) => Ok(value),
        None => Err(format!("{flag} requires a value").into()),
    }
}

fn print_help() {
    println!("refactor_persistence_bench [options]");
    println!("  --size short|medium|long       workload preset (default medium)");
    println!("  --category message_update|normal|mixed  event shape (default mixed)");
    println!("  --sessions 1|2|8|32            round-robin sessions sharing one store (default 1)");
    println!(
        "  --ops N                        override workload ops per session (max {MAX_OPS_PER_SESSION})"
    );
    println!("  --db PATH                      database file (default a fresh temp directory)");
    println!("  --out PATH                     also write pretty JSON here");
    println!("  --help                         show this help");
    println!();
    println!("Writes synthetic but legal events through the public SessionStore API to a");
    println!("real SQLite WAL database with synchronous=FULL. No model provider is used.");
    println!(
        "All sessions share one store connection; appends rotate round-robin and are serialized, not concurrent."
    );
    println!(
        "normal: complete non-streaming turns with growing history, each followed by a host event."
    );
    println!(
        "mixed: streamed updates alternating with host events, plus a representative subset of the store-level tool lifecycle."
    );
    println!(
        "raw_samples_context pairs each raw sample with its session index, the preceding successful store operation, the expected projected message count before the append, and the pre-append cursor."
    );
}

enum Op {
    Agent(Box<AgentEvent>),
    Host(Value),
    /// Crosses the public `mark_tool_started` execution boundary. This is a
    /// store call, not an `AgentEvent`, and it persists exactly one event.
    MarkToolStarted {
        call_id: String,
    },
}

impl Op {
    fn agent(event: AgentEvent) -> Self {
        Self::Agent(Box::new(event))
    }

    fn operation_name(&self) -> &'static str {
        match self {
            Self::Agent(event) if matches!(event.as_ref(), AgentEvent::MessageUpdate { .. }) => {
                "message_update"
            }
            Self::Agent(_) => "agent_lifecycle",
            Self::Host(_) => "host_event",
            Self::MarkToolStarted { .. } => "tool_started",
        }
    }

    /// Number of messages this append adds to the session projection.
    ///
    /// A user or assistant `MessageEnd` adds its message. An assistant terminal
    /// with tool calls also creates one `ToolSlot`, but that slot yields no
    /// message until its result is recorded; `ToolExecutionEnd` is where the
    /// slot resolves. A tool-result `MessageEnd` only acknowledges that existing
    /// slot, so it adds nothing and must not be counted twice.
    fn message_increment(&self) -> usize {
        match self {
            Self::Agent(event) => match event.as_ref() {
                AgentEvent::MessageEnd {
                    message: Message::User { .. } | Message::Assistant(_),
                } => 1,
                AgentEvent::ToolExecutionEnd { .. } => 1,
                _ => 0,
            },
            Self::Host(_) | Self::MarkToolStarted { .. } => 0,
        }
    }
}

struct Plan {
    ops: VecDeque<Op>,
    workload_ops: usize,
    message_updates: usize,
    host_events: usize,
    lifecycle_events: usize,
    tool_starts: usize,
    /// History entries the planned appends are expected to persist.
    expected_messages: usize,
}

fn build_plan(category: Category, workload: usize) -> Plan {
    let ops = match category {
        Category::MessageUpdate => build_message_update_plan(workload),
        Category::Normal => build_normal_plan(workload),
        Category::Mixed => build_mixed_plan(workload),
    };
    let mut message_updates = 0;
    let mut host_events = 0;
    let mut lifecycle_events = 0;
    let mut tool_starts = 0;
    let mut expected_messages = 0;
    for op in &ops {
        expected_messages += op.message_increment();
        match op {
            Op::Host(_) => host_events += 1,
            Op::MarkToolStarted { .. } => tool_starts += 1,
            Op::Agent(event) if matches!(event.as_ref(), AgentEvent::MessageUpdate { .. }) => {
                message_updates += 1;
            }
            Op::Agent(_) => lifecycle_events += 1,
        }
    }
    Plan {
        ops,
        workload_ops: message_updates + host_events,
        message_updates,
        host_events,
        lifecycle_events,
        tool_starts,
        expected_messages,
    }
}

fn build_message_update_plan(workload: usize) -> VecDeque<Op> {
    let model = Model::new("fixture", "persistence-bench");
    let mut ops = VecDeque::new();
    ops.push_back(Op::agent(AgentEvent::AgentStart));
    let mut remaining = workload;
    while remaining > 0 {
        let take = remaining.min(UPDATES_PER_MESSAGE);
        ops.push_back(Op::agent(AgentEvent::TurnStart));
        let mut text = String::new();
        let mut message = AssistantMessage::new(&model);
        ops.push_back(Op::agent(AgentEvent::MessageStart {
            message: Message::Assistant(message.clone()),
        }));
        for _ in 0..take {
            text.push_str(DELTA);
            message.content = vec![Content::text(text.clone())];
            ops.push_back(Op::agent(AgentEvent::MessageUpdate {
                message: message.clone(),
                event: Box::new(AssistantEvent::TextDelta {
                    index: 0,
                    delta: DELTA.into(),
                    partial: message.clone(),
                }),
            }));
        }
        message.stop_reason = StopReason::Stop;
        let terminal = Message::Assistant(message.clone());
        ops.push_back(Op::agent(AgentEvent::MessageEnd { message: terminal }));
        ops.push_back(Op::agent(AgentEvent::TurnEnd {
            message,
            tool_results: Vec::new(),
        }));
        remaining -= take;
    }
    // AgentEnd's message list is informational; the store does not compare it
    // with persisted history. A full history would exceed the public 64 KiB
    // single-event metadata limit at larger sizes, so history growth is carried
    // by the MessageEnd appends above.
    ops.push_back(Op::agent(AgentEvent::AgentEnd {
        messages: Vec::new(),
    }));
    ops
}

/// Each workload unit is one complete non-streaming turn followed by one host
/// event. The order matches the real agent loop: pending user input, then the
/// assistant terminal, then TurnEnd. Because the host append happens after the
/// turn, its timed projection already includes the growing message history.
fn build_normal_plan(workload: usize) -> VecDeque<Op> {
    let model = Model::new("fixture", "persistence-bench");
    let mut ops = VecDeque::new();
    ops.push_back(Op::agent(AgentEvent::AgentStart));
    for turn in 0..workload {
        ops.push_back(Op::agent(AgentEvent::TurnStart));
        let user = Message::user(format!("normal turn {turn}"));
        ops.push_back(Op::agent(AgentEvent::MessageStart {
            message: user.clone(),
        }));
        ops.push_back(Op::agent(AgentEvent::MessageEnd { message: user }));
        let mut message = AssistantMessage::new(&model);
        message.content = vec![Content::text(format!("normal reply {turn}"))];
        message.stop_reason = StopReason::Stop;
        let terminal = Message::Assistant(message.clone());
        ops.push_back(Op::agent(AgentEvent::MessageStart {
            message: terminal.clone(),
        }));
        ops.push_back(Op::agent(AgentEvent::MessageEnd { message: terminal }));
        ops.push_back(Op::agent(AgentEvent::TurnEnd {
            message,
            tool_results: Vec::new(),
        }));
        ops.push_back(Op::Host(json!({
            "seq": turn,
            "phase": "after_normal_turn",
            "note": "refactor-persistence-bench",
        })));
    }
    ops.push_back(Op::agent(AgentEvent::AgentEnd {
        messages: Vec::new(),
    }));
    ops
}

fn build_mixed_plan(workload: usize) -> VecDeque<Op> {
    let model = Model::new("fixture", "persistence-bench");
    let mut ops = VecDeque::new();
    ops.push_back(Op::agent(AgentEvent::AgentStart));
    let mut remaining = workload;
    let mut host_seq = 0;
    let mut turn = 0;
    while remaining > 0 {
        ops.push_back(Op::agent(AgentEvent::TurnStart));
        let mut text = String::new();
        let mut message = AssistantMessage::new(&model);
        ops.push_back(Op::agent(AgentEvent::MessageStart {
            message: Message::Assistant(message.clone()),
        }));
        let mut performed = 0;
        while remaining > 0 && performed < MIXED_OPS_PER_TURN {
            text.push_str(DELTA);
            message.content = vec![Content::text(text.clone())];
            ops.push_back(Op::agent(AgentEvent::MessageUpdate {
                message: message.clone(),
                event: Box::new(AssistantEvent::TextDelta {
                    index: 0,
                    delta: DELTA.into(),
                    partial: message.clone(),
                }),
            }));
            remaining -= 1;
            performed += 1;
            if remaining == 0 {
                break;
            }
            ops.push_back(Op::Host(json!({
                "seq": host_seq,
                "note": "refactor-persistence-bench",
            })));
            host_seq += 1;
            remaining -= 1;
            performed += 1;
        }
        // Close the streamed assistant with one declared tool call, then record a
        // representative subset of the store-level tool lifecycle: preflight, the
        // public mark_tool_started boundary, the terminal, and the paired
        // tool-result message before TurnEnd. This is not the full Runtime
        // sequence: there is no origin binding, no execution registration, and
        // no Host execution. No tool body runs and no host executor exists.
        let call = ToolCall::new(
            format!("mixed-tool-{turn}"),
            "refactor_fixture_tool",
            json!({"turn": turn}),
        );
        message.content = vec![Content::text(text), Content::ToolCall(call.clone())];
        message.stop_reason = StopReason::ToolUse;
        let terminal = Message::Assistant(message.clone());
        ops.push_back(Op::agent(AgentEvent::MessageEnd { message: terminal }));
        ops.push_back(Op::agent(AgentEvent::ToolExecutionStart {
            call: call.clone(),
        }));
        ops.push_back(Op::MarkToolStarted {
            call_id: call.id.clone(),
        });
        let result = ToolResult::text("refactor fixture tool result");
        ops.push_back(Op::agent(AgentEvent::ToolExecutionEnd {
            call: call.clone(),
            result: result.clone(),
            duration_ms: None,
        }));
        let result_message = ToolResultMessage {
            tool_call_id: call.id.clone(),
            tool_name: call.name.clone(),
            content: result.content,
            details: None,
            structured_content: None,
            usage: None,
            is_error: false,
            outcome_unknown: false,
            timestamp: 0,
            duration_ms: None,
        };
        let result_terminal = Message::ToolResult(result_message.clone());
        ops.push_back(Op::agent(AgentEvent::MessageStart {
            message: result_terminal.clone(),
        }));
        ops.push_back(Op::agent(AgentEvent::MessageEnd {
            message: result_terminal,
        }));
        ops.push_back(Op::agent(AgentEvent::TurnEnd {
            message,
            tool_results: vec![result_message],
        }));
        turn += 1;
    }
    ops.push_back(Op::agent(AgentEvent::AgentEnd {
        messages: Vec::new(),
    }));
    ops
}

#[derive(Clone, Copy)]
struct Sample {
    nanos: u64,
    /// Session whose round-robin slot produced this sample.
    session_index: usize,
    /// Durable cursor in that session immediately before the timed append.
    cursor_before: u64,
    /// Store operation that last succeeded on this session before the sample.
    preceding_operation: &'static str,
    /// Expected projected message count in that session before the append.
    messages_before: usize,
}

#[derive(Default)]
struct Timings {
    by_operation: BTreeMap<&'static str, Vec<Sample>>,
}

impl Timings {
    fn record(
        &mut self,
        operation: &'static str,
        start: Instant,
        session_index: usize,
        cursor_before: u64,
        preceding_operation: &'static str,
        messages_before: usize,
    ) {
        let nanos = u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX);
        self.by_operation
            .entry(operation)
            .or_default()
            .push(Sample {
                nanos,
                session_index,
                cursor_before,
                preceding_operation,
                messages_before,
            });
    }

    fn to_json(&self) -> Value {
        let mut operations = BTreeMap::new();
        let mut samples_total = 0usize;
        let mut total_ns = 0u64;
        for (operation, samples) in &self.by_operation {
            samples_total += samples.len();
            total_ns =
                total_ns.saturating_add(samples.iter().map(|sample| sample.nanos).sum::<u64>());
            operations.insert((*operation).to_string(), summarize(samples));
        }
        json!({
            "samples_total": samples_total,
            "total_ns": total_ns,
            "by_operation": operations,
        })
    }
}

fn summarize(samples: &[Sample]) -> Value {
    if samples.is_empty() {
        return json!({"samples": 0});
    }
    let raw_samples_ns: Vec<u64> = samples.iter().map(|sample| sample.nanos).collect();
    let raw_samples_context: Vec<Value> = samples
        .iter()
        .map(|sample| {
            json!({
                "session_index": sample.session_index,
                "preceding_operation": sample.preceding_operation,
                "messages_before": sample.messages_before,
                "cursor_before": sample.cursor_before,
            })
        })
        .collect();
    let mut sorted = raw_samples_ns.clone();
    sorted.sort_unstable();
    let count = sorted.len();
    let pick = |quantile: f64| -> u64 {
        let rank = (quantile * count as f64).ceil() as usize;
        sorted[rank.saturating_sub(1).min(count - 1)]
    };
    let total: u64 = sorted.iter().sum();
    let divisor = u64::try_from(count).unwrap_or(1);
    json!({
        "samples": count,
        "raw_samples_ns": raw_samples_ns,
        "raw_samples_context": raw_samples_context,
        "min_ns": sorted[0],
        "p50_ns": pick(0.50),
        "p90_ns": pick(0.90),
        "p99_ns": pick(0.99),
        "max_ns": sorted[count - 1],
        "mean_ns": total / divisor,
        "total_ns": total,
    })
}

struct SessionRun {
    index: usize,
    id: String,
    lease: AttemptLease,
    plan: VecDeque<Op>,
    ops_len: usize,
    expected_events: u64,
    expected_messages: usize,
    /// Cursor immediately before the next planned append.
    cursor: u64,
    /// Last successful store operation on this session.
    last_operation: &'static str,
    /// Expected projected message count before the next planned append.
    messages_before: usize,
    workload_ops: usize,
    message_updates: usize,
    host_events: usize,
    lifecycle_events: usize,
    tool_starts: usize,
}

fn run(args: &Args) -> BenchResult<Value> {
    let ops = args.ops.unwrap_or(args.size.ops());
    let db_path = match &args.db {
        Some(path) => path.clone(),
        None => {
            let directory = std::env::temp_dir().join(format!(
                "cayw-refactor-bench-{}-{}",
                process::id(),
                now_ms()
            ));
            fs::create_dir_all(&directory)?;
            directory.join("sessions.db")
        }
    };
    if let Some(parent) = db_path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    if fs::metadata(&db_path)
        .map(|metadata| metadata.len() > 0)
        .unwrap_or(false)
    {
        return Err(format!(
            "database already exists and is not empty: {}",
            db_path.display()
        )
        .into());
    }

    let store = SessionStore::open(&db_path)?;
    let mut timings = Timings::default();
    let mut runs: Vec<SessionRun> = Vec::with_capacity(args.sessions);

    for index in 0..args.sessions {
        let start = Instant::now();
        let snapshot = store.create_session(format!("refactor persistence bench {index}"))?;
        timings.record("create_session", start, index, 0, "none", 0);

        let start = Instant::now();
        let lease = store.begin_attempt(&snapshot.id, "bench-worker")?;
        timings.record("begin_attempt", start, index, 1, "create_session", 0);

        let Plan {
            ops: planned,
            workload_ops,
            message_updates,
            host_events,
            lifecycle_events,
            tool_starts,
            expected_messages,
        } = build_plan(args.category, ops);
        // The plan counts the appends it actually schedules; the documented
        // formula must describe the same history or the workload changed shape.
        let documented = args.category.expected_messages(ops);
        if expected_messages != documented {
            return Err(format!(
                "plan persists {expected_messages} messages but the documented formula for category {} with {ops} ops predicts {documented}",
                args.category.name()
            )
            .into());
        }
        // SessionCreated (cursor 1), AttemptStarted (cursor 2), every planned
        // op, then AttemptFinished. Every planned op persists exactly one event.
        let ops_len = planned.len();
        let expected_events = u64::try_from(ops_len).unwrap_or(u64::MAX).saturating_add(3);
        runs.push(SessionRun {
            index,
            id: snapshot.id,
            lease,
            plan: planned,
            ops_len,
            expected_events,
            expected_messages,
            cursor: 2,
            last_operation: "begin_attempt",
            messages_before: 0,
            workload_ops,
            message_updates,
            host_events,
            lifecycle_events,
            tool_starts,
        });
    }

    // Round-robin the sessions so each store call observes interleaved traffic.
    // The sessions share one connection, so these appends are serialized.
    loop {
        let mut progressed = false;
        for run in &mut runs {
            let Some(op) = run.plan.pop_front() else {
                continue;
            };
            progressed = true;
            let cursor_before = run.cursor;
            run.cursor += 1;
            let operation = op.operation_name();
            let preceding_operation = run.last_operation;
            let messages_before = run.messages_before;
            let messages_after = messages_before + op.message_increment();
            match op {
                Op::Agent(event) => {
                    let start = Instant::now();
                    store.append_agent_event(&run.lease, &event)?;
                    timings.record(
                        operation,
                        start,
                        run.index,
                        cursor_before,
                        preceding_operation,
                        messages_before,
                    );
                }
                Op::Host(data) => {
                    let start = Instant::now();
                    store.append_host_event(&run.lease, "refactor_bench", data)?;
                    timings.record(
                        operation,
                        start,
                        run.index,
                        cursor_before,
                        preceding_operation,
                        messages_before,
                    );
                }
                Op::MarkToolStarted { call_id } => {
                    let start = Instant::now();
                    store.mark_tool_started(&run.lease, &call_id)?;
                    timings.record(
                        operation,
                        start,
                        run.index,
                        cursor_before,
                        preceding_operation,
                        messages_before,
                    );
                }
            }
            // Tracked outside the timed window; it performs no database work.
            run.last_operation = operation;
            run.messages_before = messages_after;
        }
        if !progressed {
            break;
        }
    }

    for run in &runs {
        let start = Instant::now();
        let cursor_before = run.cursor;
        store.finish_attempt(&run.lease)?;
        timings.record(
            "finish_attempt",
            start,
            run.index,
            cursor_before,
            run.last_operation,
            run.messages_before,
        );
    }

    let mut verification = Vec::with_capacity(runs.len());
    let mut verified = true;
    for run in &runs {
        let start = Instant::now();
        let snapshot = store.snapshot(&run.id)?;
        timings.record(
            "snapshot_verify",
            start,
            run.index,
            run.expected_events,
            "finish_attempt",
            run.expected_messages,
        );
        let messages_ok = snapshot.messages.len() == run.expected_messages;
        let ok = snapshot.state == SessionState::Idle
            && snapshot.cursor == run.expected_events
            && messages_ok;
        verified &= ok;
        verification.push(json!({
            "index": run.index,
            "session_id": run.id.clone(),
            "state": snapshot.state,
            "cursor": snapshot.cursor,
            "expected_cursor": run.expected_events,
            "messages": snapshot.messages.len(),
            "expected_messages": run.expected_messages,
            "messages_ok": messages_ok,
            "ok": ok,
        }));
    }
    if !verified {
        return Err(
            "persisted session state, cursor, or message count did not match the planned workload"
                .into(),
        );
    }

    let ids: Vec<String> = runs.iter().map(|run| run.id.clone()).collect();
    let live_files = file_sizes(&db_path);
    drop(store);
    let checkpoint = checkpoint_full(&db_path);
    let files_after_checkpoint = file_sizes(&db_path);
    let inspection = inspect_database(&db_path, &ids)?;

    let workload_total: usize = runs.iter().map(|run| run.workload_ops).sum();
    let updates_total: usize = runs.iter().map(|run| run.message_updates).sum();
    let host_total: usize = runs.iter().map(|run| run.host_events).sum();
    let lifecycle_total: usize = runs.iter().map(|run| run.lifecycle_events).sum();
    let tool_start_total: usize = runs.iter().map(|run| run.tool_starts).sum();
    let ops_total: usize = runs.iter().map(|run| run.ops_len).sum();
    let appended_total: u64 = runs.iter().map(|run| run.expected_events).sum();
    let messages_total: usize = runs.iter().map(|run| run.expected_messages).sum();

    if inspection.event_rows != i64::try_from(appended_total).unwrap_or(i64::MAX) {
        return Err(format!(
            "core_events has {} rows but the workload appended {appended_total} events",
            inspection.event_rows
        )
        .into());
    }
    if inspection.session_rows != i64::try_from(args.sessions).unwrap_or(i64::MAX) {
        return Err(format!(
            "core_sessions has {} rows but {} sessions were requested",
            inspection.session_rows, args.sessions
        )
        .into());
    }

    let per_session: Vec<Value> = runs
        .iter()
        .map(|run| {
            json!({
                "index": run.index,
                "session_id": run.id.clone(),
                "workload_ops": run.workload_ops,
                "message_updates": run.message_updates,
                "host_events": run.host_events,
                "lifecycle_events": run.lifecycle_events,
                "tool_started_events": run.tool_starts,
                "planned_appends": run.ops_len,
                "expected_events_including_setup_and_finish": run.expected_events,
                "expected_messages": run.expected_messages,
            })
        })
        .collect();

    Ok(json!({
        "benchmark": "cayw-core/refactor_persistence_bench",
        "generated_at_ms": now_ms(),
        "environment": {
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
        },
        "config": {
            "size": args.size.name(),
            "category": args.category.name(),
            "sessions": args.sessions,
            "workload_ops_per_session": ops,
            "total_workload_ops": ops.saturating_mul(args.sessions),
        },
        "input": {
            "workload_ops": workload_total,
            "message_updates": updates_total,
            "host_events": host_total,
            "lifecycle_events": lifecycle_total,
            "tool_started_events": tool_start_total,
            "planned_appends": ops_total,
            "expected_messages": messages_total,
            "appended_events_including_setup_and_finish": appended_total,
            "per_session": per_session,
        },
        "workload_representation": {
            "normal": "Each workload unit appends one complete non-streaming turn (TurnStart, user MessageStart/End, assistant MessageStart/End, TurnEnd) and then one host event. The host append therefore commits a projection that already contains the growing message history. Events are synthetic fixtures built before timing; no agent loop or model provider runs.",
            "mixed": "Streamed MessageUpdate events alternate with host events. Each turn also records a representative subset of the store-level tool lifecycle through the public SessionStore API: ToolExecutionStart, the mark_tool_started execution boundary, ToolExecutionEnd, and the paired tool-result MessageStart/End before TurnEnd. This is not the full Runtime sequence: it has no origin binding, no execution registration, and no Host execution. No tool body executes and no Host executor is involved; these are persisted lifecycle records, not evidence of real tool or Host execution.",
            "message_update": "Only streamed MessageUpdate events and their turn terminals are appended; no host or tool events are mixed in."
        },
        "timing_ns": timings.to_json(),
        "verification": verification,
        "storage": {
            "db_path": db_path.display().to_string(),
            "live_files_while_open": live_files,
            "checkpoint": checkpoint,
            "files_after_checkpoint": files_after_checkpoint,
            "inspection": inspection.stats,
        },
        "explain": inspection.explain,
        "model": {
            "provider": "none",
            "real_model_calls": 0,
        },
        "unavailable": {
            "projection_cache_hit_rate": {
                "available": false,
                "reason": "SessionStore keeps its projection cache private and its implementation may change. The public API exposes neither hits nor misses, so this example reports no hit rate and does not label a run hot or cold."
            },
            "per_component_fsync_count": {
                "available": false,
                "reason": "SQLite does not expose per-commit fsync counts through the public API, so the fsync composition of each timed append is unknown. Timed appends include the commit durability wait."
            },
            "memory_rss": {
                "available": false,
                "reason": "The example has no existing dependency that reports process RSS on macOS, and adding one would change the crate manifest. Process memory is not measured or estimated."
            },
            "store_connection_synchronous_pragma": {
                "available": false,
                "reason": "synchronous is connection-scoped and the store connection is closed before measurement. SessionStore::open sets FULL on its own connection."
            }
        },
        "notes": [
            "Every append went through the public SessionStore API and passed its validation; a rejected event aborts the run.",
            "No model provider or credential was used.",
            "All sessions share one SessionStore connection. Workload appends rotate round-robin across sessions and are serialized, not concurrent.",
            "The projection cache is private and may change; no cache hit rate or hot/cold distinction is claimed.",
            "normal appends a complete non-streaming turn before each host event, so host_event latency covers a projection that already includes the growing message history.",
            "normal and mixed may append several events per requested workload op; planned_appends and appended_events_including_setup_and_finish report the real counts.",
            "mixed records only a representative subset of the store-level tool lifecycle: no origin binding, no execution registration, and no Host execution are recorded. It does not claim that any tool body ran or that the full Runtime sequence was reproduced.",
            "expected_messages is accumulated from the planned appends as the projected message count: a user or assistant terminal adds one message, a tool terminal adds one when its slot resolves, and a tool-result terminal adds nothing because it fills that existing slot. The verification checks snapshot message counts, and run also checks that this plan-derived count matches the documented formula.",
            "After inspection, core_events row count must equal the planned appended events and core_sessions row count must equal the requested sessions; a mismatch aborts the run with a clear error.",
            "raw_samples_context[k] records session_index, preceding_operation, messages_before, and cursor_before for raw_samples_ns[k]. preceding_operation is the same session's previous successful store operation, and messages_before is its expected projected message count before the append. Both are updated outside the timed window with no extra snapshot or database call.",
            "AgentEnd carries an empty message list: the store does not compare it with persisted history, and a full history would exceed the public 64 KiB single-event metadata limit at larger sizes. Message history is persisted by MessageEnd events and reported by snapshot message counts and projection bytes.",
            "Timings are end-to-end call latencies including the durable commit; event construction happens before the timer starts.",
            "The checkpoint is taken after the store is dropped and reports the final state of the database files. It does not measure the cost of an active checkpoint.",
            "EXPLAIN QUERY PLAN ran on an independent connection using the production SQL text. uses_index is true only when the plan uses an index and has no unindexed full scan of core_events; no specific index is required for the wait log."
        ],
    }))
}

struct Inspection {
    stats: Value,
    explain: Value,
    event_rows: i64,
    session_rows: i64,
}

fn inspect_database(path: &Path, ids: &[String]) -> BenchResult<Inspection> {
    let (connection, read_only, open_error) =
        match Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
            Ok(connection) => (connection, true, Value::Null),
            Err(error) => {
                let text = error.to_string();
                (Connection::open(path)?, false, Value::String(text))
            }
        };

    let sqlite_version: String =
        connection.query_row("SELECT sqlite_version()", [], |row| row.get(0))?;
    let journal_mode: String =
        connection.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
    let page_size: i64 = connection.pragma_query_value(None, "page_size", |row| row.get(0))?;
    let page_count: i64 = connection.pragma_query_value(None, "page_count", |row| row.get(0))?;
    let freelist_count: i64 =
        connection.pragma_query_value(None, "freelist_count", |row| row.get(0))?;
    let user_version: i64 =
        connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let storage_format: Option<String> = connection
        .query_row("SELECT format FROM core_metadata LIMIT 1", [], |row| {
            row.get(0)
        })
        .optional()?;
    let session_rows: i64 =
        connection.query_row("SELECT count(*) FROM core_sessions", [], |row| row.get(0))?;
    let event_rows: i64 =
        connection.query_row("SELECT count(*) FROM core_events", [], |row| row.get(0))?;
    let projection_bytes: i64 = connection.query_row(
        "SELECT COALESCE(SUM(length(CAST(projection AS BLOB))),0) FROM core_sessions",
        [],
        |row| row.get(0),
    )?;
    let event_bytes: i64 = connection.query_row(
        "SELECT COALESCE(SUM(length(CAST(record AS BLOB))),0) FROM core_events",
        [],
        |row| row.get(0),
    )?;

    let mut projection_by_id = BTreeMap::new();
    {
        let mut query =
            connection.prepare("SELECT id, length(CAST(projection AS BLOB)) FROM core_sessions")?;
        let mut rows = query.query([])?;
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            let bytes: i64 = row.get(1)?;
            projection_by_id.insert(id, bytes);
        }
    }
    let projection_by_session: Vec<Value> = ids
        .iter()
        .map(|id| {
            json!({
                "session_id": id,
                "projection_bytes": projection_by_id.get(id).copied().unwrap_or(0),
            })
        })
        .collect();

    let first_id = ids.first().map(String::as_str).unwrap_or_default();
    let explain = json!({
        "wait_log": explain_plan(&connection, WAIT_LOG, params![first_id], None)?,
        "events_by_type": explain_plan(
            &connection,
            EVENTS_BY_TYPE,
            params![first_id, "host", 0i64, 1_000i64],
            Some("core_events_by_type"),
        )?,
    });

    let stats = json!({
        "sqlite_version": sqlite_version,
        "read_only_connection": read_only,
        "read_only_open_error": open_error,
        "journal_mode": journal_mode,
        "schema_version": user_version,
        "storage_format": storage_format,
        "page_size": page_size,
        "page_count": page_count,
        "page_bytes": page_size.saturating_mul(page_count),
        "freelist_count": freelist_count,
        "session_rows": session_rows,
        "event_rows": event_rows,
        "projection_bytes_total": projection_bytes,
        "event_record_bytes_total": event_bytes,
        "projection_by_session": projection_by_session,
    });

    Ok(Inspection {
        stats,
        explain,
        event_rows,
        session_rows,
    })
}

/// Runs `EXPLAIN QUERY PLAN` and reports whether the query is index-backed.
///
/// `uses_index` is true only when the plan names an index (`USING INDEX` or
/// `USING COVERING INDEX`) and contains no unindexed full scan of
/// `core_events`. `expected_index` is informational: callers may pass `None`
/// when no specific index is required.
fn explain_plan<P: rusqlite::Params>(
    connection: &Connection,
    sql: &str,
    parameters: P,
    expected_index: Option<&str>,
) -> BenchResult<Value> {
    let mut statement = connection.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
    let plan: Vec<String> = statement
        .query_map(parameters, |row| row.get::<_, String>(3))?
        .collect::<Result<Vec<_>, _>>()?;
    let names_an_index = plan
        .iter()
        .any(|step| step.contains("USING INDEX") || step.contains("USING COVERING INDEX"));
    let full_scan = plan
        .iter()
        .any(|step| step.contains("SCAN core_events") && !step.contains("USING"));
    let uses_expected_index =
        expected_index.map(|index| plan.iter().any(|step| step.contains(index)));
    Ok(json!({
        "sql": sql,
        "plan": plan,
        "expected_index": expected_index,
        "uses_expected_index": uses_expected_index,
        "uses_index": names_an_index && !full_scan,
    }))
}

/// Checkpoints the files after the store connection has been closed and reports
/// the resulting final state. It does not time an active checkpoint.
fn checkpoint_full(path: &Path) -> Value {
    let result = Connection::open(path).and_then(|connection| {
        connection.query_row("PRAGMA wal_checkpoint(FULL)", [], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })
    });
    match result {
        Ok((busy, log, checkpointed)) => json!({
            "mode": "FULL",
            "scope": "final state after the store connection was closed",
            "measures_active_checkpoint_cost": false,
            "ok": true,
            "busy": busy,
            "log": log,
            "checkpointed": checkpointed,
        }),
        Err(error) => json!({
            "mode": "FULL",
            "scope": "final state after the store connection was closed",
            "measures_active_checkpoint_cost": false,
            "ok": false,
            "error": error.to_string(),
        }),
    }
}

fn file_sizes(path: &Path) -> Value {
    let db_bytes = file_len(path);
    let wal_bytes = file_len(&sidecar(path, "-wal"));
    let shm_bytes = file_len(&sidecar(path, "-shm"));
    json!({
        "db_bytes": db_bytes,
        "wal_bytes": wal_bytes,
        "shm_bytes": shm_bytes,
        "total_bytes": db_bytes.saturating_add(wal_bytes).saturating_add(shm_bytes),
    })
}

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

fn file_len(path: &Path) -> u64 {
    fs::metadata(path)
        .map(|metadata| metadata.len())
        .unwrap_or(0)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}
