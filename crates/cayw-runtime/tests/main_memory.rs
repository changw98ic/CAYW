//! Offline acceptance for main-session memory, query and forget.
//!
//! Every test uses a new temporary workspace. None reads or writes a real user
//! directory, a model or a network service.

use cayw_agent::AgentEvent;
use cayw_ai::{AssistantMessage, Content, Message, Model, ToolResultMessage};
use cayw_context::{
    CursorRange, MainSessionInput, SourceRange, SourceUnit, SummaryKind, SummaryRecord, UnitKind,
    input_budget, plan_main_session,
};
use cayw_core::{CoreEvent, EventRecord, SessionStore};
use cayw_runtime::{
    MainMemory, MemoryCandidate, MemoryOperation, MemoryOrigin, MemoryQuery, SourceType,
    user_evidence,
};
use std::{
    fs,
    path::{Path, PathBuf},
};
use tempfile::TempDir;

struct Fixture {
    _temporary: TempDir,
    workspace: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let workspace = temporary.path().join("main-session");
        fs::create_dir_all(&workspace).expect("workspace");
        Self {
            _temporary: temporary,
            workspace,
        }
    }

    fn memory(&self) -> MainMemory {
        MainMemory::open(&self.workspace)
    }

    fn write(&self, relative: &str, content: &str) -> PathBuf {
        let path = self.workspace.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("parent");
        }
        fs::write(&path, content).expect("write");
        path
    }
}

fn record(session: &str, cursor: u64, event: CoreEvent) -> EventRecord {
    EventRecord {
        protocol_version: cayw_core::PROTOCOL_VERSION,
        session_id: session.into(),
        cursor,
        timestamp: cursor,
        attempt: None,
        event,
    }
}

fn user_event(cursor: u64, text: &str) -> EventRecord {
    record(
        "s",
        cursor,
        CoreEvent::Agent {
            event: Box::new(AgentEvent::MessageStart {
                message: Message::user(text),
            }),
        },
    )
}

fn tool_event(cursor: u64, text: &str) -> EventRecord {
    record(
        "s",
        cursor,
        CoreEvent::Agent {
            event: Box::new(AgentEvent::MessageStart {
                message: Message::ToolResult(ToolResultMessage {
                    tool_call_id: "call-1".into(),
                    tool_name: "fs.read".into(),
                    content: vec![Content::text(text)],
                    details: None,
                    structured_content: None,
                    usage: None,
                    is_error: false,
                    outcome_unknown: false,
                    timestamp: cursor,
                    duration_ms: Some(1),
                }),
            }),
        },
    )
}

fn assistant_event(cursor: u64, text: &str) -> EventRecord {
    let mut message = AssistantMessage::new(&Model::new("fixture", "model"));
    message.content.push(Content::text(text));
    record(
        "s",
        cursor,
        CoreEvent::Agent {
            event: Box::new(AgentEvent::MessageStart {
                message: Message::Assistant(message),
            }),
        },
    )
}

/// Byte-for-byte snapshot of the memory directory, to prove a read did not
/// rewrite an entry file or append an event.
fn snapshot(directory: &Path) -> Vec<(String, Vec<u8>)> {
    let mut files = Vec::new();
    for entry in fs::read_dir(directory).expect("memory directory") {
        let entry = entry.expect("memory file");
        let path = entry.path();
        if path.is_file() {
            files.push((
                path.file_name().unwrap().to_string_lossy().into_owned(),
                fs::read(&path).expect("read memory file"),
            ));
        }
    }
    files.sort();
    files
}

fn candidate(kind: &str, body: &str, source_type: SourceType, sources: &[&str]) -> MemoryCandidate {
    MemoryCandidate {
        operation: MemoryOperation::Upsert,
        entry_id: None,
        kind: kind.into(),
        body: body.into(),
        source_type,
        source_log_ids: sources.iter().map(|id| (*id).to_owned()).collect(),
    }
}

#[test]
fn tool_output_can_never_become_a_memory_source() {
    let fixture = Fixture::new();
    let memory = fixture.memory();
    let events = vec![
        user_event(1, "I prefer short answers"),
        tool_event(2, "secret"),
    ];
    let evidence = user_evidence(&events);
    assert_eq!(evidence.sources().count(), 1);
    assert_eq!(evidence.sources().next().unwrap().log_id(), "s:1");

    let report = memory
        .apply(
            &evidence,
            &[candidate(
                "preference",
                "User prefers short answers",
                SourceType::Stated,
                &["s:2"],
            )],
        )
        .expect("apply");
    assert!(report.written.is_empty());
    assert_eq!(report.rejected.len(), 1);
    assert!(report.rejected[0].reason.contains("not user input"));

    let report = memory
        .apply(
            &evidence,
            &[candidate(
                "preference",
                "User prefers short answers",
                SourceType::Stated,
                &["s:1"],
            )],
        )
        .expect("apply");
    assert_eq!(report.written.len(), 1);
    assert!(report.rejected.is_empty());

    let entries = memory.entries().expect("entries");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].kind, "preference");
    assert_eq!(entries[0].body, "User prefers short answers");
    assert_eq!(entries[0].source_type, SourceType::Stated);
    let path = fixture
        .workspace
        .join("memory")
        .join(format!("{}.md", entries[0].entry_id));
    let raw = fs::read_to_string(path).expect("entry file");
    for header in [
        "entry_id:",
        "kind:",
        "source_type:",
        "source_log_ids:",
        "status:",
        "updated_at:",
    ] {
        assert!(raw.contains(header), "memory header lost {header}");
    }
    assert!(raw.contains("source_log_ids: s:1"));
}

#[test]
fn a_user_edit_marks_user_set_and_background_writes_do_not_overwrite_it() {
    let fixture = Fixture::new();
    let memory = fixture.memory();
    let evidence = user_evidence(&[user_event(1, "call me by my chosen name")]);
    let report = memory
        .apply(
            &evidence,
            &[candidate("base", "Name: old", SourceType::Stated, &["s:1"])],
        )
        .expect("apply");
    let entry_id = report.written[0].clone();
    let path = fixture
        .workspace
        .join("memory")
        .join(format!("{entry_id}.md"));
    let original = fs::read_to_string(&path).expect("entry");
    let (header, _) = original.split_once("\n\n").expect("header");
    fs::write(&path, format!("{header}\n\nName: user edited this")).expect("user edit");

    let scanned = memory.scan().expect("scan");
    assert_eq!(scanned.user_edits, vec![entry_id.clone()]);
    let entry = memory
        .entries()
        .expect("entries")
        .into_iter()
        .find(|entry| entry.entry_id == entry_id)
        .expect("entry");
    assert_eq!(entry.source_type, SourceType::UserSet);
    assert_eq!(entry.body, "Name: user edited this");
    assert!(
        fs::read_to_string(&path)
            .expect("entry")
            .contains("source_type: user_set")
    );

    let mut update = candidate(
        "base",
        "Name: background overwrite",
        SourceType::Stated,
        &["s:1"],
    );
    update.entry_id = Some(entry_id.clone());
    let report = memory.apply(&evidence, &[update]).expect("apply");
    assert!(report.written.is_empty());
    assert!(report.rejected[0].reason.contains("user_set"));
    let unchanged = memory
        .entries()
        .expect("entries")
        .into_iter()
        .find(|entry| entry.entry_id == entry_id)
        .expect("entry");
    assert_eq!(unchanged.body, "Name: user edited this");
    assert_eq!(unchanged.source_type, SourceType::UserSet);
}

#[test]
fn forget_skips_later_memory_and_keeps_cursor_reads_and_log_files() {
    let fixture = Fixture::new();
    let memory = fixture.memory();
    let evidence = user_evidence(&[user_event(1, "this fact should be forgotten")]);
    memory
        .apply(
            &evidence,
            &[candidate("base", "A fact", SourceType::Stated, &["s:1"])],
        )
        .expect("apply");
    assert!(memory.context_block().expect("block").is_some());

    let raw_log = fixture.write("raw-events.log", "session s cursor 1 original event\n");
    let raw_before = fs::read(&raw_log).expect("raw log");
    let store = SessionStore::open(fixture.workspace.join("core.sqlite")).expect("store");
    let session = store.create_session("main").expect("session");
    let events_before = store.events(&session.id, 0, 100).expect("events");

    memory
        .forget(
            SourceRange::new("s", 1, 1).expect("range"),
            "user asked to forget",
            10,
        )
        .expect("forget");

    assert!(memory.visible_entries().expect("visible").is_empty());
    assert_eq!(memory.context_block().expect("block"), None);
    assert_eq!(
        memory.forgotten_cursor_ranges("s").expect("ranges"),
        vec![CursorRange::new(1, 1).unwrap()]
    );
    let events_after = store.events(&session.id, 0, 100).expect("events");
    assert_eq!(events_before, events_after, "cursor reads changed");
    assert_eq!(fs::read(&raw_log).expect("raw log"), raw_before);
    assert!(raw_log.exists(), "raw log was deleted");
    let event_log =
        fs::read_to_string(fixture.workspace.join("memory/events.jsonl")).expect("memory events");
    assert!(event_log.contains("\"type\":\"forget\""));
    assert!(event_log.contains("\"type\":\"memory_updated\""));
}

#[test]
fn main_session_evicts_old_turns_but_keeps_memory_request_and_active_work() {
    let units = vec![
        SourceUnit {
            start_cursor: 1,
            end_cursor: 1,
            kind: UnitKind::Fixed,
            text: "rules".into(),
            outcome_unknown: false,
            pin_original: false,
        },
        SourceUnit {
            start_cursor: 2,
            end_cursor: 3,
            kind: UnitKind::CompletedTurn,
            text: "a".repeat(400),
            outcome_unknown: false,
            pin_original: false,
        },
        SourceUnit {
            start_cursor: 4,
            end_cursor: 5,
            kind: UnitKind::CompletedTurn,
            text: "b".repeat(400),
            outcome_unknown: false,
            pin_original: false,
        },
        SourceUnit {
            start_cursor: 6,
            end_cursor: 6,
            kind: UnitKind::Active,
            text: "current request".into(),
            outcome_unknown: false,
            pin_original: true,
        },
    ];
    let budget = input_budget(Some(1_400), 100, 100).expect("budget");
    let plan = plan_main_session(&MainSessionInput {
        units: &units,
        forgotten: &[],
        // Durable memory, preferences and active work stay outside eviction.
        pinned_tokens: 500,
        budget,
    })
    .expect("plan");
    assert_eq!(plan.model_calls, 0);
    assert_eq!(plan.compression_requests, 0);
    assert_eq!(plan.evicted_ranges.len(), 2);
    assert_eq!(
        plan.retained_ranges.last().unwrap(),
        &CursorRange::new(6, 6).unwrap()
    );
    assert!(
        plan.retained_ranges
            .iter()
            .any(|range| range.start_cursor == 1)
    );
    assert!(
        plan.retained_ranges
            .iter()
            .all(|range| range.start_cursor == 1 || range.start_cursor == 6)
    );

    let unknown = vec![
        SourceUnit {
            start_cursor: 1,
            end_cursor: 2,
            kind: UnitKind::ToolExchange,
            text: "u".repeat(2_000),
            outcome_unknown: true,
            pin_original: true,
        },
        SourceUnit {
            start_cursor: 3,
            end_cursor: 4,
            kind: UnitKind::ToolExchange,
            text: "p".repeat(2_000),
            outcome_unknown: false,
            pin_original: true,
        },
        SourceUnit {
            start_cursor: 5,
            end_cursor: 5,
            kind: UnitKind::Active,
            text: "now".into(),
            outcome_unknown: false,
            pin_original: true,
        },
    ];
    let tight = input_budget(Some(600), 10, 10).expect("budget");
    let plan = plan_main_session(&MainSessionInput {
        units: &unknown,
        forgotten: &[],
        pinned_tokens: 0,
        budget: tight,
    })
    .expect("plan");
    assert!(plan.evicted_ranges.is_empty());
    assert_eq!(plan.retained_ranges.len(), 3);
    assert!(!plan.sendable);
}

#[test]
fn dream_organizes_references_without_rewriting_summaries_or_logs() {
    let fixture = Fixture::new();
    let memory = fixture.memory();
    let summaries = vec![
        SummaryRecord {
            summary_id: "upper".into(),
            version: 1,
            session_id: "s".into(),
            kind: SummaryKind::Stage,
            start_cursor: 1,
            end_cursor: 8,
            text: "upper".into(),
            evidence: vec!["s:1".into(), "s:2".into(), "s:1".into()],
            local_reuse: false,
        },
        SummaryRecord {
            summary_id: "inner".into(),
            version: 1,
            session_id: "s".into(),
            kind: SummaryKind::Segment,
            start_cursor: 3,
            end_cursor: 5,
            text: "inner".into(),
            evidence: vec!["s:2".into(), "s:1".into()],
            local_reuse: false,
        },
    ];
    let before = summaries.clone();
    let raw = fixture.write("summary-raw.log", "original summary source\n");
    let raw_before = fs::read(&raw).expect("raw");
    let dream = memory.dream(&summaries, None).expect("dream");
    assert_eq!(dream.body, None);
    assert_eq!(dream.model_calls, 0);
    assert_eq!(summaries, before);
    assert_eq!(fs::read(&raw).expect("raw"), raw_before);
    let upper = dream
        .references
        .iter()
        .find(|reference| reference.summary_id == "upper")
        .expect("upper");
    assert_eq!(upper.child_of.as_deref(), Some("inner"));
    assert_eq!(upper.source_log_ids, vec!["s:inner".to_string()]);

    let with_model = memory
        .dream(&summaries, Some("explicit model text"))
        .expect("dream");
    assert_eq!(with_model.body.as_deref(), Some("explicit model text"));
    assert_eq!(summaries, before);
}

#[test]
fn memory_files_are_private_and_remain_under_the_main_session_workspace() {
    let fixture = Fixture::new();
    let memory = fixture.memory();
    let evidence = user_evidence(&[user_event(1, "remember this")]);
    memory
        .apply(
            &evidence,
            &[candidate(
                "base",
                "Remembered",
                SourceType::Inferred,
                &["s:1"],
            )],
        )
        .expect("apply");
    let directory = fixture.workspace.join("memory");
    assert!(directory.is_dir());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for entry in fs::read_dir(&directory).expect("directory") {
            let entry = entry.expect("entry");
            let mode = entry.metadata().expect("metadata").permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "memory file is not private");
        }
    }
    assert_eq!(memory.memory_directory(), directory.as_path());
    assert!(
        !fixture
            .workspace
            .join("memory")
            .join("..")
            .join("escape")
            .exists()
    );
}

#[test]
fn model_write_capabilities_cannot_target_the_memory_directory() {
    let fixture = Fixture::new();
    let memory = fixture.memory();
    for path in [
        "memory/entry.md",
        "./memory/entry.md",
        "memory",
        "memory/../memory/entry.md",
    ] {
        assert!(
            memory.model_write_blocked("fs.write", &serde_json::json!({"path": path})),
            "{path} was not blocked"
        );
    }
    assert!(memory.model_write_blocked(
        "fs.prepare_change_batch",
        &serde_json::json!({"operations":[{"op":"write","path":"memory/x.md"}]})
    ));
    assert!(memory.model_write_blocked(
        "fs.move",
        &serde_json::json!({"source":"notes.md","destination":"memory/x.md"})
    ));
    assert!(!memory.model_write_blocked("fs.write", &serde_json::json!({"path":"notes.md"})));
    assert!(!memory.model_write_blocked("fs.read", &serde_json::json!({"path":"memory/x.md"})));
}

#[test]
fn range_query_returns_chat_work_and_decisions_with_original_log_ids() {
    let fixture = Fixture::new();
    let memory = fixture.memory();
    let events = vec![
        user_event(1, "chat about the weekend"),
        assistant_event(2, "that sounds fun"),
        tool_event(3, "build passed"),
        user_event(4, "decision: ship on Friday"),
        user_event(6, "later unrelated turn"),
    ];
    let evidence = user_evidence(&events);
    memory
        .apply(
            &evidence,
            &[
                candidate(
                    "preference",
                    "User likes weekend chat",
                    SourceType::Stated,
                    &["s:1"],
                ),
                candidate("decision", "Ship on Friday", SourceType::Inferred, &["s:4"]),
            ],
        )
        .expect("apply");

    let view = memory
        .query(&MemoryQuery::new("s", 1, Some(4)), &events)
        .expect("query");
    assert_eq!(view.start_cursor, 1);
    assert_eq!(view.end_cursor, 4);
    assert_eq!(
        view.events
            .iter()
            .map(|event| event.log_id.as_str())
            .collect::<Vec<_>>(),
        vec!["s:1", "s:2", "s:3", "s:4"]
    );
    assert_eq!(
        view.events
            .iter()
            .map(|event| event.origin)
            .collect::<Vec<_>>(),
        vec![
            MemoryOrigin::User,
            MemoryOrigin::Assistant,
            MemoryOrigin::Tool,
            MemoryOrigin::User
        ]
    );
    assert!(
        view.events
            .iter()
            .any(|event| event.text.contains("weekend"))
    );
    assert!(
        view.events
            .iter()
            .any(|event| event.text.contains("build passed"))
    );
    // Both chit-chat and the recorded decision are reachable by range, and the
    // entry points back at the original log id.
    let kinds = view
        .entries
        .iter()
        .map(|entry| entry.kind.as_str())
        .collect::<Vec<_>>();
    assert!(kinds.contains(&"preference"));
    assert!(kinds.contains(&"decision"));
    let decision = view
        .entries
        .iter()
        .find(|entry| entry.kind == "decision")
        .expect("decision entry");
    assert_eq!(decision.source_log_ids, vec!["s:4".to_string()]);
    assert_eq!(decision.body, "Ship on Friday");
    assert_eq!(decision.source_type, SourceType::Inferred);

    let narrow = memory
        .query(&MemoryQuery::new("s", 4, Some(4)), &events)
        .expect("query");
    assert_eq!(narrow.events.len(), 1);
    assert_eq!(narrow.events[0].log_id, "s:4");
    assert_eq!(narrow.entries.len(), 1);
    assert_eq!(narrow.entries[0].kind, "decision");

    // A window that includes only chit-chat still reaches the recorded entry
    // and the work result, with no later turn leaking in.
    let early = memory
        .query(&MemoryQuery::new("s", 1, Some(3)), &events)
        .expect("query");
    assert!(
        early
            .events
            .iter()
            .all(|event| event.cursor <= 3 && event.cursor >= 1)
    );
    assert!(early.entries.iter().all(|entry| entry.kind != "decision"));
}

#[test]
fn query_omits_forgotten_ranges_and_never_rewrites_memory_files() {
    let fixture = Fixture::new();
    let memory = fixture.memory();
    let events = vec![
        user_event(1, "this fact should be forgotten"),
        user_event(2, "this fact stays"),
    ];
    let evidence = user_evidence(&events);
    memory
        .apply(
            &evidence,
            &[
                candidate("base", "Forgotten fact", SourceType::Stated, &["s:1"]),
                candidate("base", "Kept fact", SourceType::Stated, &["s:2"]),
            ],
        )
        .expect("apply");
    memory
        .forget(
            SourceRange::new("s", 1, 1).expect("range"),
            "user asked to forget",
            10,
        )
        .expect("forget");

    let directory = fixture.workspace.join("memory");
    let before = snapshot(&directory);
    let full = memory
        .query(&MemoryQuery::new("s", 0, None), &events)
        .expect("query");
    assert_eq!(
        full.events
            .iter()
            .map(|event| event.log_id.as_str())
            .collect::<Vec<_>>(),
        vec!["s:2"]
    );
    assert_eq!(full.entries.len(), 1);
    assert_eq!(full.entries[0].body, "Kept fact");

    let forgotten = memory
        .query(&MemoryQuery::new("s", 1, Some(1)), &events)
        .expect("query");
    assert!(forgotten.events.is_empty());
    assert!(forgotten.entries.is_empty());
    assert_eq!(
        snapshot(&directory),
        before,
        "query rewrote a memory file or appended an event"
    );

    // The original events are still readable through the ordinary cursor path.
    let store = SessionStore::open(fixture.workspace.join("core.sqlite")).expect("store");
    let session = store.create_session("main").expect("session");
    let events_by_cursor = store
        .events(&session.id, 0, 100)
        .expect("cursor read still works");
    assert!(events_by_cursor.is_empty() || events_by_cursor[0].cursor == 1);
}

#[test]
fn query_reports_user_edits_without_writing_and_background_still_refuses_user_set() {
    let fixture = Fixture::new();
    let memory = fixture.memory();
    let events = vec![user_event(1, "remember my name")];
    let evidence = user_evidence(&events);
    let report = memory
        .apply(
            &evidence,
            &[candidate("base", "Name: old", SourceType::Stated, &["s:1"])],
        )
        .expect("apply");
    let entry_id = report.written[0].clone();
    let path = fixture
        .workspace
        .join("memory")
        .join(format!("{entry_id}.md"));
    let original = fs::read_to_string(&path).expect("entry");
    let (header, _) = original.split_once("\n\n").expect("header");
    fs::write(&path, format!("{header}\n\nName: user edited this")).expect("user edit");

    let before = snapshot(&fixture.workspace.join("memory"));
    let view = memory
        .query(&MemoryQuery::new("s", 1, Some(1)), &events)
        .expect("query");
    assert_eq!(view.entries.len(), 1);
    assert_eq!(view.entries[0].body, "Name: user edited this");
    assert_eq!(view.entries[0].source_type, SourceType::UserSet);
    assert_eq!(
        snapshot(&fixture.workspace.join("memory")),
        before,
        "query rewrote the user-edited entry"
    );

    let mut update = candidate(
        "base",
        "Name: background overwrite",
        SourceType::Stated,
        &["s:1"],
    );
    update.entry_id = Some(entry_id.clone());
    let report = memory.apply(&evidence, &[update]).expect("apply");
    assert!(report.written.is_empty());
    assert!(report.rejected[0].reason.contains("user_set"));
    assert!(
        memory
            .entries()
            .expect("entries")
            .into_iter()
            .find(|entry| entry.entry_id == entry_id)
            .expect("entry")
            .body
            .contains("user edited this")
    );
}

#[test]
fn query_rejects_an_invalid_scope_without_touching_files() {
    let fixture = Fixture::new();
    let memory = fixture.memory();
    assert!(
        memory
            .query(&MemoryQuery::new("s:1", 0, None), &[])
            .is_err()
    );
    assert!(
        memory
            .query(&MemoryQuery::new("s", 5, Some(4)), &[])
            .is_err()
    );
    assert!(
        memory
            .query(&MemoryQuery::new("s", 0, Some(0)), &[])
            .is_err()
    );
}
