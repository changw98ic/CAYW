//! Runtime value capabilities must keep the original Host admission, start and
//! settlement path. A matching session id is not authorization, while an exact
//! host permission can reuse a value across turns with compare-and-set versions.
use cayw_ai::{
    AssistantEvent, AssistantStream, CancellationToken, Message, Model, ModelRequest, Provider,
};
use cayw_core::{
    CapabilityCallRecord, CapabilityOutcome, CapabilityStage, CoreEvent, EventRecord, SessionState,
};
use cayw_runtime::{ApprovalChoice, Runtime, RuntimeConfig, ScopedPermission};
use futures::stream;
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc, time::Duration};

struct Turns {
    scripts: Vec<(&'static str, &'static str)>,
}
impl Provider for Turns {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let prompt = last_user_text(&request);
        let script = self
            .scripts
            .iter()
            .find(|(needle, _)| prompt.contains(needle))
            .map(|(_, script)| *script)
            .unwrap_or("return nil");
        let message = cayw_runtime::fixture::script_response(
            &request,
            script,
            &format!("values-{}", request.messages.len()),
        );
        Box::pin(stream::iter(vec![AssistantEvent::Done { message }]))
    }
}
fn last_user_text(request: &ModelRequest) -> String {
    request
        .messages
        .iter()
        .rev()
        .find_map(|message| match message {
            Message::User { .. } => Some(message.text()),
            _ => None,
        })
        .unwrap_or_default()
}
fn runtime(
    temp: &tempfile::TempDir,
    provider: Turns,
    permissions: Vec<ScopedPermission>,
) -> Runtime {
    std::fs::create_dir(temp.path().join("workspace")).unwrap();
    Runtime::open(
        temp.path().join("session.db"),
        temp.path().join("workspace"),
        Arc::new(provider),
        Model::new("fixture", "values"),
        RuntimeConfig {
            max_run_time: Duration::from_secs(10),
            permissions,
            ..Default::default()
        },
    )
    .unwrap()
}
async fn pending(runtime: &Runtime, id: &str) -> cayw_core::ApprovalRequest {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(pending) = runtime.get(id).unwrap().pending_approvals.first() {
                return pending.clone();
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap()
}
fn store_args(value: Value, version: u64) -> Value {
    json!({"key":"state","value":value,"expected_version":version})
}
fn events(runtime: &Runtime, id: &str) -> Vec<EventRecord> {
    runtime.events(id, 0, 1000).unwrap()
}
fn value_groups(runtime: &Runtime, id: &str) -> BTreeMap<(String, u64), Vec<CapabilityCallRecord>> {
    let mut groups: BTreeMap<(String, u64), Vec<CapabilityCallRecord>> = BTreeMap::new();
    for event in events(runtime, id) {
        let call = match event.event {
            CoreEvent::CapabilityPlanned { call }
            | CoreEvent::CapabilityStarted { call }
            | CoreEvent::CapabilitySettled { call }
                if matches!(call.name.as_str(), "cells.store" | "cells.load") =>
            {
                call
            }
            _ => continue,
        };
        groups
            .entry((call.key.execution_id.clone(), call.key.sequence))
            .or_default()
            .push(call);
    }
    groups
}
fn completed_result(call: &CapabilityCallRecord) -> &Value {
    match call.outcome.as_ref() {
        Some(CapabilityOutcome::Completed { result }) => result,
        outcome => panic!("expected completed value call, got {outcome:?}"),
    }
}
fn stage_calls(runtime: &Runtime, id: &str, stage: CapabilityStage) -> Vec<CapabilityCallRecord> {
    events(runtime, id)
        .into_iter()
        .filter_map(|event| match event.event {
            CoreEvent::CapabilityPlanned { call }
            | CoreEvent::CapabilityStarted { call }
            | CoreEvent::CapabilitySettled { call }
                if matches!(call.name.as_str(), "cells.store" | "cells.load")
                    && call.stage == stage =>
            {
                Some(call)
            }
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn cross_task_value_reuse_without_host_permission_is_not_executed() {
    let temp = tempfile::tempdir().unwrap();
    let rt = runtime(
        &temp,
        Turns {
            scripts: vec![
                ("store seed", "store('state',{count=7},0)"),
                (
                    "load across task",
                    "local current=load('state'); if current then return current.value.count else return -1 end",
                ),
            ],
        },
        vec![ScopedPermission::exact(
            "cells.store",
            store_args(json!({"count":7}), 0),
        )],
    );
    let id = rt.create(None).unwrap().id;
    let seeded = rt
        .prompt(&id, "store seed", CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(seeded.state, SessionState::Idle);
    assert!(seeded.pending_approvals.is_empty());

    let worker_rt = rt.clone();
    let worker_id = id.clone();
    let worker = tokio::spawn(async move {
        worker_rt
            .prompt(&worker_id, "load across task", CancellationToken::new())
            .await
    });
    let approval = pending(&rt, &id).await;
    assert_eq!(approval.input.capability, "cells.load");
    assert!(
        matches!(
            &approval.input.target,
            cayw_core::ApprovalTarget::CapabilityCall { .. }
        ),
        "{approval:?}"
    );
    rt.approve(&id, &approval.id, ApprovalChoice::Deny, "test-user")
        .unwrap();
    let blocked = worker.await.unwrap().unwrap();
    assert_eq!(blocked.state, SessionState::Idle);
    assert!(blocked.pending_approvals.is_empty());

    let groups = value_groups(&rt, &id);
    let planned = stage_calls(&rt, &id, CapabilityStage::Planned);
    let started = stage_calls(&rt, &id, CapabilityStage::Started);
    let settled = stage_calls(&rt, &id, CapabilityStage::Settled);
    assert_eq!(planned.len(), 2, "{groups:?}");
    assert_eq!(started.len(), 1, "{groups:?}");
    assert_eq!(settled.len(), 2, "{groups:?}");
    let store = planned
        .iter()
        .find(|call| call.name == "cells.store")
        .unwrap();
    let load = planned
        .iter()
        .find(|call| call.name == "cells.load")
        .unwrap();
    assert_ne!(store.task_id, load.task_id);
    assert_eq!(store.lease.session_id, load.lease.session_id);
    assert_eq!(
        completed_result(
            settled
                .iter()
                .find(|call| call.name == "cells.store")
                .unwrap()
        ),
        &json!({"version":1})
    );
    let denied = settled
        .iter()
        .find(|call| call.name == "cells.load")
        .unwrap();
    assert!(matches!(
        &denied.outcome,
        Some(CapabilityOutcome::NotExecuted { reason })
            if reason.contains("permission denied")
    ));
    assert!(
        !started.iter().any(|call| call.name == "cells.load"),
        "denied load crossed the Host start boundary: {groups:?}"
    );
    assert!(
        !blocked
            .messages
            .iter()
            .any(|message| message.text().contains("\"count\":7")),
        "denied value leaked into history: {blocked:?}"
    );
}

#[tokio::test]
async fn authorized_value_reuse_keeps_cas_versions_across_turns() {
    let temp = tempfile::tempdir().unwrap();
    let rt = runtime(
        &temp,
        Turns {
            scripts: vec![
                (
                    "seed value",
                    "store('state',{count=1},0); return load('state')",
                ),
                (
                    "advance value",
                    "local current=load('state'); store('state',{count=2},current.version); return load('state')",
                ),
            ],
        },
        vec![
            ScopedPermission::exact("cells.store", store_args(json!({"count":1}), 0)),
            ScopedPermission::exact("cells.load", json!({"key":"state"})),
            ScopedPermission::exact("cells.store", store_args(json!({"count":2}), 1)),
        ],
    );
    let id = rt.create(None).unwrap().id;
    let seeded = rt
        .prompt(&id, "seed value", CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(seeded.state, SessionState::Idle);
    assert!(seeded.pending_approvals.is_empty());
    let advanced = rt
        .prompt(&id, "advance value", CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(advanced.state, SessionState::Idle);
    assert!(advanced.pending_approvals.is_empty());

    let groups = value_groups(&rt, &id);
    let planned = stage_calls(&rt, &id, CapabilityStage::Planned);
    let started = stage_calls(&rt, &id, CapabilityStage::Started);
    let settled = stage_calls(&rt, &id, CapabilityStage::Settled);
    assert_eq!(planned.len(), 5, "{groups:?}");
    assert_eq!(
        started.len(),
        5,
        "every authorized value call starts: {groups:?}"
    );
    assert_eq!(settled.len(), 5, "{groups:?}");
    let task_ids = planned
        .iter()
        .map(|call| call.task_id.clone())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(task_ids.len(), 2, "seed and advance are different tasks");
    let sessions = planned
        .iter()
        .map(|call| call.lease.session_id.clone())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(sessions.len(), 1, "both turns share one value session");

    let results = settled
        .iter()
        .map(|call| (call.name.as_str(), completed_result(call).clone()))
        .collect::<Vec<_>>();
    assert_eq!(results[0], ("cells.store", json!({"version":1})));
    assert_eq!(
        results[1],
        ("cells.load", json!({"version":1,"value":{"count":1}}))
    );
    // The second turn reads the version left by the first turn, then advances it.
    assert_eq!(
        results[2],
        ("cells.load", json!({"version":1,"value":{"count":1}}))
    );
    assert_eq!(results[3], ("cells.store", json!({"version":2})));
    assert_eq!(
        results[4],
        ("cells.load", json!({"version":2,"value":{"count":2}}))
    );
    assert!(
        planned
            .iter()
            .all(|call| call.stage == CapabilityStage::Planned),
        "{groups:?}"
    );
}

#[tokio::test]
async fn value_calls_pair_original_core_and_host_start_settle_once() {
    let temp = tempfile::tempdir().unwrap();
    let rt = runtime(
        &temp,
        Turns {
            scripts: vec![(
                "paired value",
                "store('state',{ok=true},0); return load('state')",
            )],
        },
        vec![
            ScopedPermission::exact("cells.store", store_args(json!({"ok":true}), 0)),
            ScopedPermission::exact("cells.load", json!({"key":"state"})),
        ],
    );
    let id = rt.create(None).unwrap().id;
    let result = rt
        .prompt(&id, "paired value", CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(result.state, SessionState::Idle);

    let groups = value_groups(&rt, &id);
    assert_eq!(groups.len(), 2, "{groups:?}");
    for records in groups.values() {
        assert_eq!(
            records.iter().map(|call| call.stage).collect::<Vec<_>>(),
            [
                CapabilityStage::Planned,
                CapabilityStage::Started,
                CapabilityStage::Settled
            ],
            "each value call has exactly one original Host start and settle: {records:?}"
        );
        let planned = &records[0];
        let started = &records[1];
        let settled = &records[2];
        for later in [started, settled] {
            assert_eq!(later.key, planned.key);
            assert_eq!(later.name, planned.name);
            assert_eq!(later.arguments, planned.arguments);
            assert_eq!(later.parent_call_id, planned.parent_call_id);
            assert_eq!(later.task_id, planned.task_id);
            assert_eq!(later.source_hash, planned.source_hash);
            assert_eq!(later.lease, planned.lease);
        }
        assert!(planned.authorization.is_none(), "{planned:?}");
        assert!(
            started.authorization.is_some(),
            "Host start must record the exact permission proof: {started:?}"
        );
        assert!(matches!(
            &settled.outcome,
            Some(CapabilityOutcome::Completed { .. })
        ));
    }
    let settled = stage_calls(&rt, &id, CapabilityStage::Settled);
    assert_eq!(
        settled
            .iter()
            .map(|call| completed_result(call).clone())
            .collect::<Vec<_>>(),
        vec![
            json!({"version":1}),
            json!({"version":1,"value":{"ok":true}})
        ]
    );
}
