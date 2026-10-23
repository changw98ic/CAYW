//! Dynamic Runtime services must bind to the same task request authority as
//! ordinary Host capabilities. Discovery is not permission: policy scope,
//! admission receipt, execution and settlement all retain the original origin.
use cayw_ai::{AssistantEvent, AssistantStream, CancellationToken, Model, ModelRequest, Provider};
use cayw_core::{CapabilityOutcome, CoreEvent, SessionState};
use cayw_requests::{OperationScope, RequestRecord, RequestState};
use cayw_runtime::{
    LocalAgents, RequestPolicy, Runtime, RuntimeConfig, RuntimeError, ScopedPermission, SessionView,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::Semaphore;

struct Script {
    script: &'static str,
    calls: AtomicUsize,
    pause: Option<Arc<Semaphore>>,
}
impl Provider for Script {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        let message = cayw_runtime::fixture::script_response(
            &request,
            self.script,
            &format!("dynamic-request-{}", request.messages.len()),
        );
        let pause = (self.calls.fetch_add(1, Ordering::SeqCst) == 1)
            .then(|| self.pause.clone())
            .flatten();
        Box::pin(async_stream::stream! {
            if let Some(pause) = pause { pause.acquire_owned().await.unwrap().forget(); }
            yield AssistantEvent::Done { message };
        })
    }
}

fn workspace(temp: &tempfile::TempDir) -> std::path::PathBuf {
    let root = temp.path().join("workspace");
    std::fs::create_dir_all(&root).unwrap();
    root
}
fn store_arguments() -> Value {
    json!({"key":"state","value":{"count":7},"expected_version":0})
}
fn store_policy() -> RequestPolicy {
    RequestPolicy {
        permission_policy_id: "dynamic-services-fixture".into(),
        operations: vec![OperationScope::exact(
            "store-a",
            "cells.store",
            store_arguments(),
            "values-policy",
        )],
    }
}
fn base_config(policy: RequestPolicy, permissions: Vec<ScopedPermission>) -> RuntimeConfig {
    RuntimeConfig {
        max_run_time: Duration::from_secs(10),
        local_agents: Some(LocalAgents::default()),
        permissions,
        request_policy: Some(policy),
        ..Default::default()
    }
}
fn open(
    temp: &tempfile::TempDir,
    config: RuntimeConfig,
    script: &'static str,
    pause: Option<Arc<Semaphore>>,
) -> Runtime {
    Runtime::open(
        temp.path().join("session.db"),
        workspace(temp),
        Arc::new(Script {
            script,
            calls: AtomicUsize::new(0),
            pause,
        }),
        Model::new("fixture", "dynamic-requests"),
        config,
    )
    .unwrap()
}
async fn running_task(rt: &Runtime, session: &str) -> String {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(task) = rt.tasks(session).unwrap().into_iter().find(|task| {
                matches!(
                    task.state,
                    cayw_runtime::TaskState::Queued | cayw_runtime::TaskState::Running
                )
            }) {
                return task.id;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap()
}
async fn awaiting_request(rt: &Runtime, task: &str) -> RequestRecord {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(events) = rt.request_events(task, 0, 64) {
                for event in events.into_iter().rev() {
                    if let Some(id) = event.request_id {
                        let record = rt.request_status(task, &id).unwrap();
                        if record.state == RequestState::AwaitingEvaluation {
                            return record;
                        }
                    }
                }
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap()
}
fn settled(rt: &Runtime, session: &str, name: &str) -> cayw_core::CapabilityCallRecord {
    rt.events(session, 0, 1000)
        .unwrap()
        .into_iter()
        .find_map(|record| match record.event {
            CoreEvent::CapabilitySettled { call } if call.name == name => Some(call),
            _ => None,
        })
        .unwrap_or_else(|| panic!("missing settled {name}"))
}
fn started(rt: &Runtime, session: &str, name: &str) -> Vec<cayw_core::CapabilityCallRecord> {
    rt.events(session, 0, 1000)
        .unwrap()
        .into_iter()
        .filter_map(|record| match record.event {
            CoreEvent::CapabilityStarted { call } if call.name == name => Some(call),
            _ => None,
        })
        .collect()
}
fn task_request_admitted(rt: &Runtime, session: &str, name: &str) -> Value {
    rt.events(session, 0, 1000)
        .unwrap()
        .into_iter()
        .find_map(|record| match record.event {
            CoreEvent::Host { kind, data }
                if kind == "task_request_admitted" && data["name"] == name =>
            {
                Some(data)
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("missing admitted receipt for {name}"))
}

#[tokio::test]
async fn dynamic_names_bind_with_host_operations_and_configured_children() {
    let temp = tempfile::tempdir().unwrap();
    let policy = RequestPolicy {
        permission_policy_id: "dynamic-catalog-fixture".into(),
        operations: vec![
            OperationScope::exact(
                "host-read",
                "fs.read",
                json!({"path":"a.txt"}),
                "host-policy",
            ),
            OperationScope::exact("store-a", "cells.store", store_arguments(), "values-policy"),
            OperationScope::exact(
                "load-a",
                "cells.load",
                json!({"key":"state"}),
                "values-policy",
            ),
            OperationScope::exact(
                "ask-a",
                "user.ask",
                json!({"question":"Which target?"}),
                "collaboration-policy",
            ),
            OperationScope::exact(
                "plan-a",
                "plan.update",
                json!({"expected_version":0,"steps":[]}),
                "collaboration-policy",
            ),
            OperationScope::exact(
                "agent-a",
                "agent.run",
                json!({"prompt":"Summarize the task"}),
                "children-policy",
            ),
        ],
    };
    let rt = open(&temp, base_config(policy, vec![]), "return nil", None);
    let names: BTreeSet<_> = rt
        .request_operations()
        .unwrap()
        .into_iter()
        .map(|operation| operation.capability)
        .collect();
    assert_eq!(
        names,
        BTreeSet::from([
            "agent.run".to_string(),
            "cells.load".to_string(),
            "cells.store".to_string(),
            "fs.read".to_string(),
            "plan.update".to_string(),
            "user.ask".to_string(),
        ])
    );
}

#[tokio::test]
async fn control_tools_and_disabled_children_are_not_bindable() {
    let temp = tempfile::tempdir().unwrap();
    let control = RequestPolicy {
        permission_policy_id: "control-fixture".into(),
        operations: vec![OperationScope::exact(
            "request-a",
            "capability.request",
            json!({
                "purpose":"Need a protected read",
                "gap":"No ordinary read permission",
                "operation_id":"read-a",
                "capability":"fs.read",
                "arguments":{"path":"a.txt"},
            }),
            "request-policy",
        )],
    };
    let error = match Runtime::open(
        temp.path().join("control.db"),
        workspace(&temp),
        Arc::new(Script {
            script: "return nil",
            calls: AtomicUsize::new(0),
            pause: None,
        }),
        Model::new("fixture", "dynamic-requests"),
        base_config(control, vec![]),
    ) {
        Ok(_) => panic!("request control tools must not be bindable as dynamic operations"),
        Err(error) => error,
    };
    assert!(
        matches!(
            error,
            RuntimeError::Request(cayw_requests::RequestError::InvalidInput(_))
        ),
        "unexpected control-tool binding error: {error}"
    );

    let mut config = base_config(
        RequestPolicy {
            permission_policy_id: "child-fixture".into(),
            operations: vec![OperationScope::exact(
                "agent-a",
                "agent.run",
                json!({"prompt":"Do one task"}),
                "children-policy",
            )],
        },
        vec![],
    );
    config.local_agents = None;
    let error = match Runtime::open(
        temp.path().join("child.db"),
        workspace(&temp),
        Arc::new(Script {
            script: "return nil",
            calls: AtomicUsize::new(0),
            pause: None,
        }),
        Model::new("fixture", "dynamic-requests"),
        config,
    ) {
        Ok(_) => panic!("agent.run must be absent when local agents are not configured"),
        Err(error) => error,
    };
    assert!(
        matches!(
            error,
            RuntimeError::Request(cayw_requests::RequestError::InvalidInput(_))
        ),
        "unexpected disabled-child binding error: {error}"
    );
}

#[tokio::test]
async fn dynamic_operation_without_receipt_is_denied_before_start() {
    let temp = tempfile::tempdir().unwrap();
    let rt = open(
        &temp,
        base_config(
            store_policy(),
            vec![ScopedPermission::exact("cells.store", store_arguments())],
        ),
        "return cayw.call('cells.store',{key='state',value={count=7},expected_version=0})",
        None,
    );
    let id = rt.create(None).unwrap().id;
    let result = rt
        .prompt(&id, "store without request", CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(result.state, SessionState::Idle);
    assert!(
        started(&rt, &id, "cells.store").is_empty(),
        "a protected value call crossed the Host start boundary without a receipt"
    );
    let call = settled(&rt, &id, "cells.store");
    assert!(
        matches!(
            &call.outcome,
            Some(CapabilityOutcome::NotExecuted { reason })
                if reason.contains("no active host grant")
        ),
        "unexpected settlement without a receipt: {call:?}"
    );
}

#[tokio::test]
async fn approved_dynamic_operation_keeps_its_exact_receipt_and_result() {
    let temp = tempfile::tempdir().unwrap();
    let pause = Arc::new(Semaphore::new(0));
    let script = "local input={purpose='store exact state',gap='protected value store unavailable',operation_id='store-a',capability='cells.store',arguments={key='state',value={count=7},expected_version=0}}; cayw.call('capability.request',input); cayw.call('capability.request',input); yield_control(); return cayw.call('cells.store',{key='state',value={count=7},expected_version=0})";
    let rt = open(
        &temp,
        base_config(
            store_policy(),
            vec![ScopedPermission::exact("cells.store", store_arguments())],
        ),
        script,
        Some(pause.clone()),
    );
    let id = rt.create(None).unwrap().id;
    let worker_rt = rt.clone();
    let session = id.clone();
    let worker: tokio::task::JoinHandle<Result<SessionView, RuntimeError>> =
        tokio::spawn(async move {
            worker_rt
                .prompt(&session, "approved dynamic store", CancellationToken::new())
                .await
        });
    let task_id = running_task(&rt, &id).await;
    let original = awaiting_request(&rt, &task_id).await;
    let ticket = rt
        .request_evaluate(&task_id, &original.id, "fixture-host")
        .unwrap();
    rt.request_approve(
        &task_id,
        &ticket,
        Duration::from_secs(30),
        "approved exact dynamic value store",
    )
    .unwrap();
    pause.add_permits(1);
    let result = worker.await.unwrap().unwrap();
    assert_eq!(result.state, SessionState::Idle);

    let call = settled(&rt, &id, "cells.store");
    assert!(
        matches!(
            &call.outcome,
            Some(CapabilityOutcome::Completed { result })
                if result == &json!({"version":1})
        ),
        "approved dynamic store did not complete: {call:?}"
    );
    let admitted = task_request_admitted(&rt, &id, "cells.store");
    assert_eq!(admitted["receipt"]["request_id"], original.id);
    assert_eq!(
        admitted["receipt"]["operation"]["capability"],
        "cells.store"
    );
    assert_eq!(
        admitted["receipt"]["operation"]["arguments"],
        store_arguments()
    );
}

#[tokio::test]
async fn changed_dynamic_configuration_invalidates_the_old_catalog_and_receipt() {
    let temp = tempfile::tempdir().unwrap();
    let pause = Arc::new(Semaphore::new(0));
    let script = "local input={purpose='store exact state',gap='protected value store unavailable',operation_id='store-a',capability='cells.store',arguments={key='state',value={count=7},expected_version=0}}; cayw.call('capability.request',input); cayw.call('capability.request',input); yield_control(); return cayw.call('cells.store',{key='state',value={count=7},expected_version=0})";
    let original_config = base_config(
        store_policy(),
        vec![ScopedPermission::exact("cells.store", store_arguments())],
    );
    let rt = open(&temp, original_config.clone(), script, Some(pause.clone()));
    let id = rt.create(None).unwrap().id;
    let worker_rt = rt.clone();
    let session = id.clone();
    let worker: tokio::task::JoinHandle<Result<SessionView, RuntimeError>> =
        tokio::spawn(async move {
            worker_rt
                .prompt(&session, "catalog invalidation", CancellationToken::new())
                .await
        });
    let task_id = running_task(&rt, &id).await;
    let original = awaiting_request(&rt, &task_id).await;
    let ticket = rt
        .request_evaluate(&task_id, &original.id, "fixture-host")
        .unwrap();

    let mut changed_budget = original_config.clone();
    changed_budget.local_agents = Some(LocalAgents {
        max_children: 2,
        ..LocalAgents::default()
    });
    let budget_rt = open(&temp, changed_budget, "return nil", None);
    assert_policy_mismatch(budget_rt.request_approve(
        &task_id,
        &ticket,
        Duration::from_secs(30),
        "changed local agent budget",
    ));

    let mut changed_permissions = original_config.clone();
    changed_permissions
        .local_agents
        .as_mut()
        .unwrap()
        .permissions = vec![ScopedPermission::exact("fs.read", json!({"path":"a.txt"}))];
    let permissions_rt = open(&temp, changed_permissions, "return nil", None);
    assert_policy_mismatch(permissions_rt.request_approve(
        &task_id,
        &ticket,
        Duration::from_secs(30),
        "changed child permissions",
    ));

    let mut changed_cells = original_config.clone();
    changed_cells.cells.store_bytes = 2 * 1024 * 1024;
    let cells_rt = open(&temp, changed_cells, "return nil", None);
    assert_policy_mismatch(cells_rt.request_approve(
        &task_id,
        &ticket,
        Duration::from_secs(30),
        "changed value boundary",
    ));

    let mut changed_parent_permissions = original_config.clone();
    changed_parent_permissions
        .permissions
        .push(ScopedPermission::exact("fs.read", json!({"path":"a.txt"})));
    let parent_rt = open(&temp, changed_parent_permissions, "return nil", None);
    assert_policy_mismatch(parent_rt.request_approve(
        &task_id,
        &ticket,
        Duration::from_secs(30),
        "changed parent permission set",
    ));

    let mut changed_runtime_budget = original_config.clone();
    changed_runtime_budget.max_capability_calls += 1;
    let runtime_rt = open(&temp, changed_runtime_budget, "return nil", None);
    assert_policy_mismatch(runtime_rt.request_approve(
        &task_id,
        &ticket,
        Duration::from_secs(30),
        "changed shared capability budget",
    ));

    rt.request_deny(&task_id, &ticket, "fixture cleanup after catalog checks")
        .unwrap();
    pause.add_permits(1);
    let _ = worker.await.unwrap();
}

fn assert_policy_mismatch(result: Result<cayw_requests::RequestRecord, RuntimeError>) {
    let error = result.expect_err("a changed dynamic authority must reject the old catalog");
    assert!(
        matches!(
            error,
            RuntimeError::Request(cayw_requests::RequestError::PolicyMismatch)
        ),
        "unexpected catalog error: {error:?}"
    );
}
