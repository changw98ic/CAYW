//! Reusable live verification for a real-model Core database.
//!
//! The caller stops its Runtime first. This module only touches the supplied
//! session in the supplied verification database. It checks event order and
//! cursors through two independent connections, injects a real deferred foreign
//! key failure at COMMIT, checks the failed write published no observable
//! state, removes only its own trigger/tables, writes the same event once,
//! appends a valid host/control pair, and finally fails and then finishes the
//! attempt. The host cursor update and the finish-attempt persist/mutate path
//! each get their own injected COMMIT failure.

use cayw_core::{
    AttemptLease, CoreError, CoreEvent, EventRecord, MAX_EVENT_PAGE, SessionSnapshot, SessionStore,
};
use rusqlite::Connection;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::Path;

const HOST_KIND: &str = "runtime_live_host_probe";
const CONTROL_KIND: &str = "runtime_live_control_probe";
const COMMIT_KIND: &str = "runtime_live_commit_probe";
const CLEANUP: &str = "DROP TRIGGER IF EXISTS runtime_live_fail_commit; DROP TABLE IF EXISTS runtime_live_fixture_deferred; DROP TABLE IF EXISTS runtime_live_fixture_parent;";

pub fn verify(
    database: &Path,
    session: &str,
    out: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut report = json!({
        "kind": "cayw-core-runtime-live-verification", "database": database.display().to_string(),
        "session": session, "ok": false, "phase": "start",
        "cache_stats": "unavailable: no public cache counters; internal cache removal was not directly observed. Behavior verified: both connections matched the pre-failure snapshots and logs after each injected COMMIT failure. Other-session cache retention is not observable and no product API was added",
    });
    let result = run(database, session, &mut report);
    report["ok"] = json!(result.is_ok());
    if let Err(error) = &result {
        report["error"] = json!(error.to_string());
    }
    if result.is_ok() {
        report["phase"] = json!("complete");
    }
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(out)?;
    file.write_all(&serde_json::to_vec_pretty(&report)?)?;
    file.sync_all()?;
    result
}

fn run(
    database: &Path,
    session: &str,
    report: &mut Value,
) -> Result<(), Box<dyn std::error::Error>> {
    report["phase"] = json!("history");
    let store = SessionStore::open(database)?;
    let peer = SessionStore::open(database)?;
    let original = store.snapshot(session)?;
    let original_events = read_all(&store, session, original.cursor)?;
    verify_events(session, original.cursor, &original_events)?;
    same(
        &peer.snapshot(session)?,
        &original,
        "connections disagree on the snapshot",
    )?;
    same(
        &read_all(&peer, session, original.cursor)?,
        &original_events,
        "connections disagree on the event log",
    )?;
    report["before"] = json!({"cursor": original.cursor, "events": original_events.len(), "messages": original.messages.len(), "messages_sha256": messages_digest(&original)?});

    report["phase"] = json!("attempt");
    let (lease, origin) = match original.attempt.clone() {
        Some(lease) => (lease, "existing"),
        None => (
            store.begin_attempt(session, "runtime-live-verifier")?,
            "started-for-verification",
        ),
    };
    let baseline = store.snapshot(session)?;
    let baseline_events = read_all(&store, session, baseline.cursor)?;
    verify_events(session, baseline.cursor, &baseline_events)?;
    same(
        &baseline.messages,
        &original.messages,
        "attempt setup changed messages",
    )?;
    same(
        &peer.snapshot(session)?,
        &baseline,
        "peer disagrees on the attempt baseline",
    )?;
    report["baseline"] =
        json!({"cursor": baseline.cursor, "attempt_origin": origin, "attempt_fence": lease.fence});

    report["phase"] = json!("stale-rejections");
    let stale = baseline.cursor.saturating_sub(1);
    require(stale != baseline.cursor, "cannot construct a stale cursor")?;
    let control = |cursor: u64, attempt: Option<&AttemptLease>, actor: &str, kind: &str| {
        store.append_control_event(session, cursor, attempt, actor, kind, json!({}))
    };
    expect_error(
        control(stale, Some(&lease), "host", "runtime_live_stale_cursor"),
        "InvalidEvent",
    )?;
    let mut forged = lease.clone();
    forged.attempt_id = format!("{}-forged", lease.attempt_id);
    forged.fence = forged.fence.saturating_add(1);
    expect_error(
        store.append_host_event(&forged, "runtime_live_stale_lease", json!({})),
        "StaleLease",
    )?;
    expect_error(
        control(
            baseline.cursor,
            Some(&forged),
            "host",
            "runtime_live_stale_control",
        ),
        "StaleLease",
    )?;
    expect_error(
        control(
            baseline.cursor,
            Some(&lease),
            " ",
            "runtime_live_empty_actor",
        ),
        "InvalidInput",
    )?;
    same(
        &store.snapshot(session)?,
        &baseline,
        "a rejected write changed the snapshot",
    )?;
    same(
        &read_all(&store, session, baseline.cursor)?,
        &baseline_events,
        "a rejected write changed the event log",
    )?;
    report["stale_rejections"] = json!({"stale_cursor": "InvalidEvent", "stale_lease": "StaleLease", "empty_actor": "InvalidInput", "unchanged": true});

    report["phase"] = json!("host-commit-failure");
    let probe = baseline
        .cursor
        .checked_add(1)
        .ok_or("session cursor is exhausted")?;
    let trigger = install_commit_failure(database, session, probe)?;
    let failed = store.append_host_event(&lease, COMMIT_KIND, json!({"stage": "before_cleanup"}));
    trigger.execute_batch(CLEANUP)?;
    drop(trigger);
    let failure = expect_commit_failure(failed)?;
    same(
        &store.snapshot(session)?,
        &baseline,
        "a failed host COMMIT published the current session snapshot",
    )?;
    same(
        &peer.snapshot(session)?,
        &baseline,
        "a failed host COMMIT published the peer snapshot",
    )?;
    same(
        &read_all(&store, session, baseline.cursor)?,
        &baseline_events,
        "a failed host COMMIT changed the event log",
    )?;
    same(
        &read_all(&peer, session, baseline.cursor)?,
        &baseline_events,
        "a failed host COMMIT changed the peer event log",
    )?;
    report["host_commit_failure"] = json!({
        "path": "append_host_event cursor update",
        "cursor": probe,
        "error": failure,
        "same_connection": "snapshot_and_log_unchanged",
        "peer": "snapshot_and_log_unchanged",
        "cache": "behavior verified; internal cache removal not directly observed"
    });

    report["phase"] = json!("single-write-after-cleanup");
    let retried =
        store.append_host_event(&lease, COMMIT_KIND, json!({"stage": "after_cleanup"}))?;
    let after_retry = store.snapshot(session)?;
    require(
        retried.cursor == probe && retried.attempt.as_ref() == Some(&lease),
        "the retry is not the expected lease-bound event",
    )?;
    require(
        after_retry.cursor == probe
            && after_retry.messages == original.messages
            && after_retry.attempt.as_ref() == Some(&lease),
        "the retry changed the snapshot beyond the cursor",
    )?;
    same(
        &peer.snapshot(session)?,
        &after_retry,
        "the peer disagrees after the retry",
    )?;
    require(
        count_kind(&read_all(&store, session, probe)?, COMMIT_KIND) == 1,
        "the retry was not committed exactly once",
    )?;
    report["single_write_after_cleanup"] =
        json!({"cursor": retried.cursor, "committed_once": true});

    report["phase"] = json!("host-control");
    let host = store.append_host_event(&lease, HOST_KIND, json!({"sequence": 1}))?;
    let control_event = store.append_control_event(
        session,
        host.cursor,
        Some(&lease),
        "host",
        CONTROL_KIND,
        json!({"sequence": 2}),
    )?;
    let tail = store.events(session, probe, MAX_EVENT_PAGE)?;
    require(
        host.cursor == probe + 1 && control_event.cursor == probe + 2,
        "host/control cursors are not consecutive",
    )?;
    require(
        host.attempt.as_ref() == Some(&lease) && control_event.attempt.as_ref() == Some(&lease),
        "host/control are not lease-bound",
    )?;
    require(
        tail.len() == 2 && is_kind(&tail[0], HOST_KIND) && is_kind(&tail[1], CONTROL_KIND),
        "host/control order is not host then control",
    )?;
    let mut expected = baseline.clone();
    expected.cursor = control_event.cursor;
    same(
        &store.snapshot(session)?,
        &expected,
        "host/control changed the snapshot beyond the cursor",
    )?;
    same(
        &peer.snapshot(session)?,
        &expected,
        "the peer disagrees after host/control",
    )?;
    same(
        &expected.messages,
        &original.messages,
        "host/control changed the original messages",
    )?;
    report["host_control"] = json!({"host_cursor": host.cursor, "control_cursor": control_event.cursor, "order": "host_then_control"});

    report["phase"] = json!("finish-commit-failure");
    let before_finish = store.snapshot(session)?;
    let before_finish_events = read_all(&store, session, before_finish.cursor)?;
    verify_events(session, before_finish.cursor, &before_finish_events)?;
    same(
        &before_finish,
        &expected,
        "the finish baseline differs from the host/control snapshot",
    )?;
    same(
        &read_all(&peer, session, before_finish.cursor)?,
        &before_finish_events,
        "the peer disagrees on the finish baseline",
    )?;
    let finish_probe = before_finish
        .cursor
        .checked_add(1)
        .ok_or("session cursor is exhausted")?;
    let trigger = install_commit_failure(database, session, finish_probe)?;
    let failed_finish = store.finish_attempt(&lease);
    trigger.execute_batch(CLEANUP)?;
    drop(trigger);
    let finish_failure = expect_commit_failure(failed_finish)?;
    let retained = store.snapshot(session)?;
    same(
        &retained,
        &before_finish,
        "a failed finish_attempt published the current session snapshot",
    )?;
    require(
        retained.attempt.as_ref() == Some(&lease) && retained.messages == original.messages,
        "a failed finish_attempt released the attempt or changed messages",
    )?;
    same(
        &peer.snapshot(session)?,
        &before_finish,
        "a failed finish_attempt published the peer snapshot",
    )?;
    same(
        &read_all(&store, session, before_finish.cursor)?,
        &before_finish_events,
        "a failed finish_attempt changed the event log",
    )?;
    same(
        &read_all(&peer, session, before_finish.cursor)?,
        &before_finish_events,
        "a failed finish_attempt changed the peer event log",
    )?;
    report["finish_commit_failure"] = json!({
        "path": "finish_attempt persist/mutate",
        "cursor": finish_probe,
        "error": finish_failure,
        "attempt_retained": true,
        "same_connection": "snapshot_and_log_unchanged",
        "peer": "snapshot_and_log_unchanged",
        "cache": "behavior verified; internal cache removal not directly observed"
    });

    report["phase"] = json!("finish-attempt-after-cleanup");
    store.finish_attempt(&lease)?;
    let finished = store.snapshot(session)?;
    let finished_events = read_all(&store, session, finished.cursor)?;
    verify_events(session, finished.cursor, &finished_events)?;
    require(
        finished.cursor == finish_probe && finished.attempt.is_none(),
        "finish_attempt did not commit at the next cursor or clear the attempt",
    )?;
    require(
        finished.messages == original.messages,
        "finish_attempt changed the original messages",
    )?;
    require(
        finished_events.len() == before_finish_events.len() + 1,
        "finish_attempt did not commit exactly once",
    )?;
    same(
        &finished_events[..before_finish_events.len()],
        &before_finish_events[..],
        "the successful finish_attempt changed earlier history",
    )?;
    let last = finished_events
        .last()
        .ok_or("the finished event log is empty")?;
    require(
        matches!(&last.event, CoreEvent::AttemptFinished { lease: actual } if actual == &lease),
        "the new event is not the expected AttemptFinished",
    )?;
    require(
        last.attempt.as_ref() == Some(&lease),
        "the finished event is not lease-bound",
    )?;
    same(
        &peer.snapshot(session)?,
        &finished,
        "the peer disagrees after finish_attempt",
    )?;
    same(
        &read_all(&peer, session, finished.cursor)?,
        &finished_events,
        "the peer event log disagrees after finish_attempt",
    )?;
    report["finish_attempt"] = json!({"cursor": finished.cursor, "attempt_cleared": true, "messages_unchanged": true, "committed_once": true, "peer_consistent": true});
    Ok(())
}

fn install_commit_failure(
    database: &Path,
    session: &str,
    cursor: u64,
) -> Result<Connection, Box<dyn std::error::Error>> {
    let connection = Connection::open(database)?;
    let quoted = session.replace('\'', "''");
    connection.execute_batch(&format!(
        "PRAGMA foreign_keys=ON; BEGIN IMMEDIATE; {CLEANUP} \
         CREATE TABLE runtime_live_fixture_parent(id INTEGER PRIMARY KEY); \
         CREATE TABLE runtime_live_fixture_deferred(parent INTEGER REFERENCES runtime_live_fixture_parent(id) DEFERRABLE INITIALLY DEFERRED); \
         CREATE TRIGGER runtime_live_fail_commit AFTER INSERT ON core_events \
         WHEN NEW.session_id = '{quoted}' AND NEW.cursor = {cursor} \
         BEGIN INSERT INTO runtime_live_fixture_deferred VALUES(1); END; COMMIT;"
    ))?;
    Ok(connection)
}

fn expect_commit_failure<T>(
    result: Result<T, CoreError>,
) -> Result<String, Box<dyn std::error::Error>> {
    match result {
        Err(CoreError::Database(error)) => Ok(error.to_string()),
        Err(other) => fail(format!("expected a COMMIT database failure, got {other:?}")),
        Ok(_) => fail("the deferred foreign key trigger did not fail the COMMIT"),
    }
}

fn read_all(
    store: &SessionStore,
    session: &str,
    cursor: u64,
) -> Result<Vec<EventRecord>, Box<dyn std::error::Error>> {
    let mut records = Vec::new();
    let mut after = 0;
    while (records.len() as u64) < cursor {
        let page = store.events(session, after, MAX_EVENT_PAGE)?;
        let Some(last) = page.last() else {
            return fail(format!("event log ended at {after}, expected {cursor}"));
        };
        require(last.cursor > after, "an event page did not advance")?;
        after = last.cursor;
        records.extend(page);
    }
    Ok(records)
}

fn verify_events(
    session: &str,
    cursor: u64,
    events: &[EventRecord],
) -> Result<(), Box<dyn std::error::Error>> {
    require(
        events.len() as u64 == cursor,
        "the event log length differs from the cursor",
    )?;
    let mut protocol = None;
    for (index, record) in events.iter().enumerate() {
        require(
            record.session_id == session && record.cursor == index as u64 + 1,
            "an event envelope or cursor is inconsistent",
        )?;
        match protocol {
            Some(value) if value != record.protocol_version => {
                return fail("the protocol version changed inside one log");
            }
            None => protocol = Some(record.protocol_version),
            _ => {}
        }
    }
    Ok(())
}

fn is_kind(record: &EventRecord, kind: &str) -> bool {
    matches!(&record.event, CoreEvent::Host { kind: actual, .. } if actual == kind)
}

fn count_kind(records: &[EventRecord], kind: &str) -> usize {
    records
        .iter()
        .filter(|record| is_kind(record, kind))
        .count()
}

fn same<T: PartialEq + ?Sized>(
    left: &T,
    right: &T,
    label: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    require(left == right, label)
}

fn require(condition: bool, label: &str) -> Result<(), Box<dyn std::error::Error>> {
    if condition { Ok(()) } else { fail(label) }
}

fn expect_error<T>(
    result: Result<T, CoreError>,
    expected: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let actual = match result {
        Err(CoreError::InvalidEvent(_)) => "InvalidEvent",
        Err(CoreError::StaleLease) => "StaleLease",
        Err(CoreError::InvalidInput(_)) => "InvalidInput",
        Err(error) => return fail(format!("expected {expected}, got {error:?}")),
        Ok(_) => return fail(format!("expected {expected}, got success")),
    };
    require(actual == expected, "unexpected Core error variant")
}

fn messages_digest(snapshot: &SessionSnapshot) -> Result<String, Box<dyn std::error::Error>> {
    let mut hasher = Sha256::new();
    hasher.update(serde_json::to_vec(&snapshot.messages)?);
    Ok(format!("{:x}", hasher.finalize()))
}

fn fail<T>(message: impl Into<String>) -> Result<T, Box<dyn std::error::Error>> {
    Err(message.into().into())
}
