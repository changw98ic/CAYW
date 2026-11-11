//! Automatic post-turn memory writer for the main-session workspace.
//!
//! Reads only newly completed user-input turns after `writer_frontier`, skips
//! pure acknowledgements without calling a model, verifies every candidate
//! provenance against durable user input, and preserves `user_set` and
//! concurrently edited files as pending candidates.

use std::{
    collections::BTreeMap,
    sync::{Arc, Weak},
};

use cayw_agent::AgentEvent;
use cayw_ai::CancellationToken;
use cayw_context::{
    DEFAULT_OUTPUT_RESERVE, DEFAULT_PROTOCOL_MARGIN, SourceRange, estimate_text, log_id,
};
use cayw_core::CoreEvent;
use serde::Deserialize;

use crate::{
    Inner, Runtime,
    context_maintenance::{
        MaintenanceStreamOutcome, consume_maintenance_stream, model_is_configured,
        overlaps_forgotten, reserve_maintenance, settle_reservation,
    },
    main_memory::{MemoryCandidate, now_ms, user_evidence},
    session_context::{JobKind, ReservationSpec, effective_input_budget},
};

const MEMORY_WRITER_SYSTEM_PROMPT: &str = r#"You are the main-session memory writer. Read the new user-input turns and the existing memory entries, then emit JSON only with no extra prose:
{"candidates":[{"operation":"upsert","entry_id":null,"kind":"base","body":"...","source_type":"stated","source_log_ids":["session:cursor"]}],"forget":[{"start_cursor":1,"end_cursor":1,"reason":"..."}]}
Rules:
1. Extract only durable base facts ("base": user profile, long-term goals, stable workspace facts) or enduring working/communication preferences ("preference"). Skip ephemeral task chatter or one-off questions by returning {"candidates":[],"forget":[]}.
2. Every candidate MUST cite exact `source_log_ids` from the listed user turns. Never cite tool results, web pages, or assistant output.
3. Use `source_type` = "stated" for explicit user statements and "inferred" for cautious user-turn inferences. Never emit "user_set".
4. When a new user statement updates or supersedes an existing entry, reuse its `entry_id` with `operation`: "upsert" to update/merge it, or emit `operation`: "disable" with that `entry_id` to retire the superseded entry.
5. Emit `forget` ranges only when the user explicitly asks to forget prior turns in this session."#;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct WriterResponse {
    #[serde(default)]
    candidates: Vec<MemoryCandidate>,
    #[serde(default)]
    forget: Vec<ForgetDirective>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ForgetDirective {
    start_cursor: u64,
    end_cursor: u64,
    #[serde(default)]
    reason: String,
}

#[derive(Debug, Clone)]
struct UserTurnSource {
    cursor: u64,
    log_id: String,
    text: String,
}

pub(crate) fn schedule_post_turn(inner: Weak<Inner>, session_id: &str) {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let Some(strong) = inner.upgrade() else {
        return;
    };
    if !strong.config.main_session || strong.maintenance_cancel.is_cancelled() {
        return;
    }
    let sid = session_id.to_owned();
    let cancel = strong.maintenance_cancel.child_token();
    let weak = Arc::downgrade(&strong);
    handle.spawn(async move {
        run_memory_writer(weak, sid, cancel).await;
    });
}

async fn run_memory_writer(inner: Weak<Inner>, session_id: String, cancel: CancellationToken) {
    let Some(strong) = inner.upgrade() else {
        return;
    };
    let runtime = Runtime {
        inner: strong.clone(),
    };
    let frontier = match strong.memory.writer_frontier(&session_id) {
        Ok(frontier) => frontier,
        Err(error) => {
            strong
                .memory
                .record_writer_error(&session_id, error.to_string());
            return;
        }
    };
    let events = match runtime.session_events(&session_id) {
        Ok(events) => events,
        Err(error) => {
            strong
                .memory
                .record_writer_error(&session_id, error.to_string());
            return;
        }
    };
    let forgotten = match strong.memory.forgotten_cursor_ranges(&session_id) {
        Ok(forgotten) => forgotten,
        Err(error) => {
            strong
                .memory
                .record_writer_error(&session_id, error.to_string());
            return;
        }
    };

    let mut new_user_turns = Vec::new();
    let mut max_seen_cursor = frontier;
    for record in &events {
        if record.cursor <= frontier {
            continue;
        }
        if let CoreEvent::Agent { event } = &record.event
            && let AgentEvent::MessageEnd {
                message: user @ cayw_ai::Message::User { .. },
            } = event.as_ref()
        {
            max_seen_cursor = max_seen_cursor.max(record.cursor);
            if overlaps_forgotten(record.cursor, record.cursor, &forgotten) {
                continue;
            }
            let text = user.text().trim().to_owned();
            if text.is_empty() || is_pure_confirmation(&text) {
                continue;
            }
            let id = match log_id(&session_id, record.cursor) {
                Ok(id) => id,
                Err(error) => {
                    strong
                        .memory
                        .record_writer_error(&session_id, error.to_string());
                    return;
                }
            };
            new_user_turns.push(UserTurnSource {
                cursor: record.cursor,
                log_id: id,
                text,
            });
        }
    }

    if new_user_turns.is_empty() {
        if max_seen_cursor > frontier
            && let Err(error) = strong
                .memory
                .advance_writer_frontier(&session_id, max_seen_cursor)
        {
            strong
                .memory
                .record_writer_error(&session_id, error.to_string());
        }
        return;
    }

    let (configured, window, output_reserve) = {
        let selected = crate::lock(&strong.selection);
        (
            model_is_configured(&strong.config, &selected.model),
            selected.model.context_window,
            selected
                .model
                .max_output_tokens
                .unwrap_or(DEFAULT_OUTPUT_RESERVE),
        )
    };
    if !configured {
        return;
    }

    let start_cursor = new_user_turns
        .first()
        .map(|turn| turn.cursor)
        .unwrap_or(frontier + 1);
    let end_cursor = max_seen_cursor;

    let existing_entries = match strong.memory.visible_entries() {
        Ok(entries) => entries,
        Err(error) => {
            strong
                .memory
                .record_writer_error(&session_id, error.to_string());
            return;
        }
    };
    let expected_versions: BTreeMap<String, String> = existing_entries
        .iter()
        .map(|entry| (entry.entry_id.clone(), entry.version.clone()))
        .collect();

    let mut prompt = String::new();
    prompt.push_str("Existing active memory entries:\n");
    if existing_entries.is_empty() {
        prompt.push_str("(none)\n");
    } else {
        for entry in &existing_entries {
            prompt.push_str(&format!(
                "- entry_id={} kind={} source_type={:?}: {}\n",
                entry.entry_id,
                entry.kind,
                entry.source_type,
                entry.body.trim()
            ));
        }
    }
    prompt.push_str("\nNew user-input turns:\n");
    for turn in &new_user_turns {
        prompt.push_str(&format!("[{}] {}\n", turn.log_id, turn.text));
    }

    let margin = effective_input_budget(window, output_reserve)
        .map(|b| b.margin)
        .unwrap_or(DEFAULT_PROTOCOL_MARGIN);
    let input_bytes = estimate_text(&prompt)
        .tokens
        .saturating_add(estimate_text(MEMORY_WRITER_SYSTEM_PROMPT).tokens);

    drop(runtime);
    drop(strong);

    let Some((reservation_id, budget_root, provider, mut model)) = reserve_maintenance(
        &inner,
        &session_id,
        ReservationSpec {
            kind: JobKind::MemoryWriter,
            start_cursor,
            end_cursor,
            input_bytes,
            margin,
            output_max: output_reserve,
        },
        &cancel,
        |s| {
            s.memory
                .writer_frontier(&session_id)
                .map(|current| current < end_cursor)
                .unwrap_or(false)
        },
    )
    .await
    else {
        return;
    };

    model.max_output_tokens = Some(output_reserve);
    let outcome = consume_maintenance_stream(
        provider,
        model,
        MEMORY_WRITER_SYSTEM_PROMPT,
        &prompt,
        cancel.clone(),
    )
    .await;

    let Some(strong) = inner.upgrade() else {
        return;
    };
    settle_reservation(&strong, &budget_root, &session_id, reservation_id, &outcome).await;

    let text = match outcome {
        MaintenanceStreamOutcome::Completed { text, .. } => text,
        MaintenanceStreamOutcome::Failed { reason, .. } => {
            if !cancel.is_cancelled() {
                strong
                    .memory
                    .record_writer_error(&session_id, format!("memory writer failed: {reason}"));
            }
            return;
        }
    };
    let parsed = match parse_writer_response(&text) {
        Ok(parsed) => parsed,
        Err(error) => {
            strong
                .memory
                .record_writer_error(&session_id, format!("invalid memory writer JSON: {error}"));
            return;
        }
    };

    let runtime = Runtime {
        inner: strong.clone(),
    };
    let latest_events = match runtime.session_events(&session_id) {
        Ok(events) => events,
        Err(error) => {
            strong
                .memory
                .record_writer_error(&session_id, error.to_string());
            return;
        }
    };
    let session_max_cursor = latest_events.last().map(|e| e.cursor).unwrap_or(end_cursor);
    let mut changed = false;
    let mut forget_failed = false;

    for directive in &parsed.forget {
        if directive.start_cursor == 0
            || directive.end_cursor < directive.start_cursor
            || directive.end_cursor > session_max_cursor
        {
            strong.memory.record_writer_error(
                &session_id,
                format!(
                    "memory writer emitted out-of-bounds forget range {}..{}",
                    directive.start_cursor, directive.end_cursor
                ),
            );
            forget_failed = true;
            continue;
        }
        match SourceRange::new(&session_id, directive.start_cursor, directive.end_cursor) {
            Ok(range) => {
                let reason = if directive.reason.trim().is_empty() {
                    "user requested forget"
                } else {
                    directive.reason.trim()
                };
                match strong.memory.forget(range, reason, now_ms()) {
                    Ok(_) => changed = true,
                    Err(error) => {
                        strong
                            .memory
                            .record_writer_error(&session_id, error.to_string());
                        forget_failed = true;
                    }
                }
            }
            Err(error) => {
                strong
                    .memory
                    .record_writer_error(&session_id, error.to_string());
                forget_failed = true;
            }
        }
    }

    let evidence = user_evidence(&latest_events);
    let report = match strong.memory.apply_with_versions(
        &evidence,
        &parsed.candidates,
        Some(&expected_versions),
    ) {
        Ok(report) => report,
        Err(error) => {
            strong
                .memory
                .record_writer_error(&session_id, error.to_string());
            return;
        }
    };
    if !report.written.is_empty() {
        changed = true;
    }
    if forget_failed {
        return;
    }
    match strong
        .memory
        .advance_writer_frontier(&session_id, end_cursor)
    {
        Ok(()) => {
            if changed {
                strong.changed.notify_waiters();
            }
        }
        Err(error) => {
            strong
                .memory
                .record_writer_error(&session_id, error.to_string());
        }
    }
}

fn parse_writer_response(raw: &str) -> Result<WriterResponse, serde_json::Error> {
    let trimmed = raw.trim();
    let stripped = if let Some(rest) = trimmed.strip_prefix("```") {
        let rest = rest
            .strip_prefix("json")
            .or_else(|| rest.strip_prefix("JSON"))
            .unwrap_or(rest)
            .trim_start();
        rest.strip_suffix("```").unwrap_or(rest).trim()
    } else {
        trimmed
    };
    serde_json::from_str::<WriterResponse>(stripped)
}

pub(crate) fn is_pure_confirmation(text: &str) -> bool {
    let normalized: String = text
        .trim()
        .trim_matches(|ch: char| {
            matches!(
                ch,
                '.' | '!' | '?' | ',' | ';' | '。' | '！' | '？' | '，' | '；'
            )
        })
        .trim()
        .to_lowercase();
    matches!(
        normalized.as_str(),
        "ok" | "okay"
            | "k"
            | "yes"
            | "yep"
            | "yeah"
            | "sure"
            | "thanks"
            | "thank you"
            | "thx"
            | "got it"
            | "continue"
            | "go ahead"
            | "好"
            | "好的"
            | "收到"
            | "明白"
            | "明白了"
            | "可以"
            | "行"
            | "嗯"
            | "嗯嗯"
            | "谢谢"
            | "多谢"
            | "继续"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pure_confirmations_are_detected_and_substantive_turns_are_not() {
        assert!(is_pure_confirmation("好的！"));
        assert!(is_pure_confirmation("ok."));
        assert!(is_pure_confirmation("谢谢"));
        assert!(!is_pure_confirmation("以后默认用中文回答"));
        assert!(!is_pure_confirmation("ok, remember my timezone is UTC+8"));
    }

    #[test]
    fn writer_response_parses_plain_and_fenced_json() {
        let fenced = "```json\n{\"candidates\":[{\"kind\":\"preference\",\"body\":\"Use short sentences\",\"source_type\":\"stated\",\"source_log_ids\":[\"s:1\"]}]}\n```";
        let parsed = parse_writer_response(fenced).expect("parses fenced JSON");
        assert_eq!(parsed.candidates.len(), 1);
        assert_eq!(parsed.candidates[0].kind, "preference");
        assert!(parse_writer_response("Offline fixture: plain text").is_err());
    }
}
