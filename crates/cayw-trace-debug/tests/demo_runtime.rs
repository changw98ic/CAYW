//! Independent control client for the Issue 31 demo.
//!
//! The demo source is included as a module so this test drives the real
//! `Runtime`, real Host, real CodeMode and a real authenticated loopback IPC
//! server without reimplementing any of them. The test never touches the
//! Runtime directly: every task is admitted and run through `cayw_ipc::Client`.

#[allow(dead_code)]
#[path = "../examples/demo.rs"]
mod demo;

use cayw_core::{CoreEvent, EventRecord, ExecutionOutcome, ExecutionRecord};
use cayw_ipc::{Client, ClientConfig};
use cayw_trace_debug::{Edit, Status, TraceStore};
use serde_json::{Value, json};
use std::{error::Error, time::Duration};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

const TOKEN: &str = "issue31-demo-control-token-0123456789abcdef";
const TRACE: &str = "cayw.trace.v1";

async fn rpc(client: &Client, method: &str, params: Value) -> TestResult<Value> {
    Ok(client.request(method, params).await?)
}

async fn events(client: &Client, session: &str) -> TestResult<Vec<EventRecord>> {
    // The IPC/Runtime page limit is 1000. Read bounded pages until the session
    // has no more records; never request an oversized page.
    const PAGE: usize = 1000;
    let mut all = Vec::new();
    let mut after = 0_u64;
    loop {
        let page = rpc(
            client,
            "session.events",
            json!({"session_id":session,"after":after,"limit":PAGE,"wait_ms":0}),
        )
        .await?;
        let page: Vec<EventRecord> = serde_json::from_value(page)?;
        let Some(last) = page.last() else {
            break;
        };
        after = last.cursor;
        let count = page.len();
        all.extend(page);
        if count < PAGE {
            break;
        }
    }
    Ok(all)
}

fn trace_value(data: &Value) -> &Value {
    &data["entry"]["output"]["value"]
}

/// Finds the execution whose manifest names `plan_id`. The manifest is a real
/// Cells output observed through the Runtime's `trace_node` Host event.
fn manifest_execution(events: &[EventRecord], plan_id: &str) -> Option<String> {
    events.iter().find_map(|event| match &event.event {
        CoreEvent::Host { kind, data } if kind == "trace_node" => {
            let value = trace_value(data);
            (value["schema"] == json!(TRACE)
                && value["trace"]["kind"] == json!("manifest")
                && value["trace"]["plan"]["id"] == json!(plan_id))
            .then(|| data["execution_id"].as_str().map(str::to_owned))
            .flatten()
        }
        _ => None,
    })
}

fn traces<'a>(events: &'a [EventRecord], execution_id: &str) -> Vec<&'a Value> {
    events
        .iter()
        .filter_map(|event| match &event.event {
            CoreEvent::Host { kind, data }
                if kind == "trace_node" && data["execution_id"] == json!(execution_id) =>
            {
                let value = trace_value(data);
                (value["schema"] == json!(TRACE)).then_some(value)
            }
            _ => None,
        })
        .collect()
}

fn execution<'a>(events: &'a [EventRecord], execution_id: &str) -> Option<&'a ExecutionRecord> {
    events.iter().find_map(|event| match &event.event {
        CoreEvent::ExecutionFinished { execution } if execution.execution_id == execution_id => {
            Some(execution)
        }
        _ => None,
    })
}

fn node_traces(traces: &[&Value], kind: &str) -> Vec<String> {
    traces
        .iter()
        .filter(|value| value["trace"]["kind"] == json!(kind))
        .filter_map(|value| value["trace"]["node_id"].as_str().map(str::to_owned))
        .collect()
}

fn completed_result<'a>(traces: &[&'a Value], node: &str) -> &'a Value {
    traces
        .iter()
        .find(|value| {
            value["trace"]["kind"] == json!("completed") && value["trace"]["node_id"] == json!(node)
        })
        .map(|value| &value["trace"]["result"])
        .expect("the node has no completed trace")
}

fn capability_results<'a>(events: &'a [EventRecord], name: &str) -> Vec<&'a Value> {
    events
        .iter()
        .filter_map(|event| match &event.event {
            CoreEvent::CapabilitySettled { call } if call.name == name => match &call.outcome {
                Some(cayw_core::CapabilityOutcome::Completed { result }) => Some(result),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

async fn run_task(client: &Client, session: &str, text: &str) -> TestResult<String> {
    let task = rpc(
        client,
        "task.enqueue",
        json!({"session_id":session,"text":text}),
    )
    .await?;
    let task_id = task["id"]
        .as_str()
        .ok_or("task.enqueue did not return an id")?
        .to_owned();
    let _view = tokio::time::timeout(
        Duration::from_secs(90),
        rpc(client, "task.resume", json!({"task_id":task_id})),
    )
    .await??;
    let record = rpc(client, "task.get", json!({"task_id":task_id})).await?;
    assert_eq!(
        record["state"], "completed",
        "the real Runtime did not complete the task: {record}"
    );
    Ok(task_id)
}

#[test]
fn demo_requires_the_documented_loopback_ipc_arguments() {
    let options = demo::parse_options([
        "--root".to_owned(),
        "/tmp/new-demo-root".to_owned(),
        "--listen".to_owned(),
        "127.0.0.1:0".to_owned(),
        "--token-env".to_owned(),
        "CAYW_ISSUE31_TOKEN".to_owned(),
    ])
    .expect("the documented arguments were rejected");
    assert_eq!(options.token_env, "CAYW_ISSUE31_TOKEN");
    assert!(options.listen.ip().is_loopback());

    assert!(
        demo::parse_options([
            "--root".to_owned(),
            "/tmp/new-demo-root".to_owned(),
            "--listen".to_owned(),
            "0.0.0.0:4321".to_owned(),
            "--token-env".to_owned(),
            "CAYW_ISSUE31_TOKEN".to_owned(),
        ])
        .is_err(),
        "a non-loopback IPC listener was accepted"
    );
    assert_eq!(
        demo::Variant::from_prompt("run the initial condition with input.amount"),
        demo::Variant::InitialBug
    );
    assert_eq!(
        demo::Variant::from_prompt("run the fixed low branch"),
        demo::Variant::Low
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn demo_runs_real_host_graph_and_switches_branches() -> TestResult {
    let temporary = TempDir::new()?;
    // The demo requires a root that does not exist yet.
    let root = temporary.path().join("issue31-demo-root");
    let options = demo::Options {
        root,
        listen: "127.0.0.1:0".parse()?,
        token_env: "CAYW_ISSUE31_UNUSED".into(),
    };
    let demo = demo::start_with_token(&options, TOKEN.to_owned()).await?;
    let address = demo.address;

    // Authentication is real: a wrong token cannot pass the IPC hello.
    assert!(
        Client::connect(address, "wrong-token-0123456789abcdef")
            .await
            .is_err(),
        "the IPC server accepted an unauthenticated client"
    );
    let config = ClientConfig {
        request_timeout: Duration::from_secs(120),
        ..Default::default()
    };
    let client = Client::connect_with_config(address, TOKEN, config).await?;

    let health = rpc(&client, "runtime.health", json!({})).await?;
    assert_eq!(health["status"], "ok");
    assert_eq!(health["provider"], "offline-demo");

    // The demo must be idle until this control client admits a task.
    let before = events(&client, &demo.session_id).await?;
    assert!(
        !before
            .iter()
            .any(|event| matches!(event.event, CoreEvent::ExecutionStarted { .. })),
        "the demo executed something before task.enqueue/task.resume"
    );

    // First run: the unedited condition reads the absent `input.amount`.
    run_task(
        &client,
        &demo.session_id,
        "run the initial condition that reads the missing amount",
    )
    .await?;
    let initial_events = events(&client, &demo.session_id).await?;
    let initial_id = manifest_execution(&initial_events, "issue31-initial")
        .expect("the initial plan manifest was never published");
    let failed =
        execution(&initial_events, &initial_id).expect("the initial execution never finished");
    let error = match &failed.terminal {
        Some(ExecutionOutcome::Failed { error }) => error.clone(),
        other => panic!("the missing input.amount did not fail the real run: {other:?}"),
    };
    assert!(
        error.to_ascii_lowercase().contains("compare") || error.contains("nil"),
        "the failure is not the real Luau nil comparison: {error}"
    );
    let initial_traces = traces(&initial_events, &initial_id);
    let manifest = initial_traces
        .iter()
        .find(|value| value["trace"]["kind"] == json!("manifest"))
        .expect("no initial manifest");
    assert_eq!(
        manifest["trace"]["plan"]["nodes"].as_array().unwrap().len(),
        5
    );
    assert_eq!(
        node_traces(&initial_traces, "started"),
        ["input", "condition"]
    );
    assert_eq!(node_traces(&initial_traces, "completed"), ["input"]);
    assert_eq!(
        manifest["trace"]["plan"]["input"],
        json!({"label":"issue31 demo input","value":7})
    );

    // The plan input is the real fs.read result, not text produced by a model.
    let reads = capability_results(&initial_events, "fs.read");
    assert_eq!(
        reads.len(),
        1,
        "the first task did not perform exactly one read"
    );
    let captured = &manifest["trace"]["plan"]["context"]["captured_read"];
    assert_eq!(
        captured, reads[0],
        "plan context is not the real fs.read result"
    );
    assert_eq!(captured["content"], json!(demo::INPUT_FILE));
    assert_eq!(
        serde_json::from_str::<Value>(captured["content"].as_str().unwrap())?,
        manifest["trace"]["plan"]["input"]
    );
    assert_eq!(captured["bytes"], json!(demo::INPUT_FILE.len()));

    // Exactly one real write, and its effect is visible on disk.
    let writes = capability_results(&initial_events, "fs.write");
    assert_eq!(writes.len(), 1, "expected exactly one real fs.write");
    assert_eq!(
        std::fs::read_to_string(demo.workspace.join("side-effect.txt"))?,
        demo::SIDE_EFFECT_CONTENT
    );

    // Issue 31 core path: ingest the actual Core/trace_node facts and edit the
    // condition locally. Replay recomputes from the real plan input and follows
    // the new edge without touching the Runtime, Core, or the Host.
    let store = TraceStore::open(temporary.path().join("issue31-trace.json"))?;
    store.ingest(&initial_events)?;
    let captured_project = store
        .snapshot()
        .projects
        .get(&initial_id)
        .cloned()
        .expect("the failed actual execution was not captured");
    assert_eq!(captured_project.version(), 1);
    assert_eq!(captured_project.runtime_status, Status::Failed);
    let observed = captured_project.latest().expect("no observed evaluation");
    assert_eq!(observed.id, format!("observed:{initial_id}"));
    assert_eq!(observed.mode, "observed");
    assert_eq!(observed.status, Status::Failed);
    assert_eq!(
        observed
            .steps
            .iter()
            .map(|step| step.node_id.as_str())
            .collect::<Vec<_>>(),
        ["input", "condition"]
    );
    assert!(
        observed.steps[1]
            .error
            .as_deref()
            .is_some_and(|error| error.contains("compare") || error.contains("nil")),
        "the observed condition error is not the real Luau failure"
    );
    // The observed prefix starts at the real fs.read-derived input.
    assert_eq!(observed.steps[0].input, manifest["trace"]["plan"]["input"]);
    assert_eq!(observed.steps[1].input["value"], 7);
    assert_eq!(observed.steps[1].input["source_sha256"], captured["sha256"]);

    // Baseline Core facts. Local edits must not advance the Core cursor or
    // repeat the real side effect.
    let before_view = rpc(
        &client,
        "session.get",
        json!({"session_id":demo.session_id}),
    )
    .await?;
    let before_cursor = before_view["cursor"].as_u64().unwrap();
    assert_eq!(capability_results(&initial_events, "fs.write").len(), 1);

    let high_document = store
        .edit(
            &initial_id,
            Edit {
                expected_version: 1,
                node_id: "condition".into(),
                source: demo::CONDITION_FIXED.into(),
                params: json!({"threshold":5}),
                input_pointer: String::new(),
            },
            CancellationToken::new(),
        )
        .await?;
    let high_replay = high_document
        .projects
        .get(&initial_id)
        .and_then(|project| project.latest())
        .expect("the high edit produced no replay evaluation");
    assert_eq!(high_replay.version, 2);
    assert_eq!(high_replay.mode, "recompute");
    assert_eq!(high_replay.status, Status::Completed);
    assert_eq!(
        high_replay
            .steps
            .iter()
            .map(|step| step.node_id.as_str())
            .collect::<Vec<_>>(),
        ["input", "condition", "high", "result"]
    );
    assert!(
        high_replay.steps[0].reused,
        "nodes before the edit are reused from the captured run"
    );
    assert!(
        high_replay.steps[1..].iter().all(|step| !step.reused),
        "the edited node and affected downstream are recomputed"
    );
    assert_eq!(high_replay.steps[1].branch.as_deref(), Some("high"));
    let high_replay_output = high_replay
        .output
        .as_ref()
        .expect("high replay has no output");
    assert_eq!(high_replay_output["classification"], "high");
    assert_eq!(high_replay_output["threshold"], 5);

    // Same fixed source and a changed parameter takes the other edge.
    let low_document = store
        .edit(
            &initial_id,
            Edit {
                expected_version: 2,
                node_id: "condition".into(),
                source: demo::CONDITION_FIXED.into(),
                params: json!({"threshold":20}),
                input_pointer: String::new(),
            },
            CancellationToken::new(),
        )
        .await?;
    let low_replay = low_document
        .projects
        .get(&initial_id)
        .and_then(|project| project.latest())
        .expect("the low edit produced no replay evaluation");
    assert_eq!(low_replay.version, 3);
    assert_eq!(low_replay.status, Status::Completed);
    assert_eq!(
        low_replay
            .steps
            .iter()
            .map(|step| step.node_id.as_str())
            .collect::<Vec<_>>(),
        ["input", "condition", "low", "result"]
    );
    let low_replay_output = low_replay
        .output
        .as_ref()
        .expect("low replay has no output");
    assert_eq!(low_replay_output["classification"], "low");
    assert_eq!(low_replay_output["threshold"], 20);
    assert_ne!(low_replay_output, high_replay_output);

    // Editing the now-inactive high branch must not borrow v2's old input.
    let inactive = low_document.projects[&initial_id]
        .revisions
        .last()
        .unwrap()
        .plan
        .nodes
        .iter()
        .find(|node| node.id == "high")
        .unwrap();
    let blocked = store
        .edit(
            &initial_id,
            Edit {
                expected_version: 3,
                node_id: "high".into(),
                source: inactive.source.clone(),
                params: inactive.params.clone(),
                input_pointer: inactive.input_pointer.clone(),
            },
            CancellationToken::new(),
        )
        .await?;
    let blocked = blocked.projects[&initial_id].latest().unwrap();
    assert_eq!(blocked.version, 4);
    assert_eq!(blocked.status, Status::Blocked);
    assert!(blocked.steps.is_empty());
    assert!(
        blocked
            .gaps
            .iter()
            .any(|gap| gap.node_id.as_deref() == Some("high"))
    );

    let after_view = rpc(
        &client,
        "session.get",
        json!({"session_id":demo.session_id}),
    )
    .await?;
    assert_eq!(
        after_view["cursor"].as_u64().unwrap(),
        before_cursor,
        "a local edit advanced the Core cursor"
    );
    let after_edit_events = events(&client, &demo.session_id).await?;
    assert_eq!(
        after_edit_events.len(),
        initial_events.len(),
        "a local edit appended Core events"
    );
    assert_eq!(
        capability_results(&after_edit_events, "fs.write").len(),
        1,
        "a local edit repeated the real fs.write"
    );

    // Second run: the user edited the condition to `input.value`; the low
    // threshold now chooses the high branch.
    run_task(
        &client,
        &demo.session_id,
        "run the fixed condition on input.value high branch",
    )
    .await?;
    let high_events = events(&client, &demo.session_id).await?;
    let high_id = manifest_execution(&high_events, "issue31-high")
        .expect("the fixed high plan manifest was never published");
    match &execution(&high_events, &high_id)
        .expect("the fixed high execution never finished")
        .terminal
    {
        Some(ExecutionOutcome::Completed { .. }) => {}
        other => panic!("the fixed condition did not complete: {other:?}"),
    }
    let high_traces = traces(&high_events, &high_id);
    // Every live task rebuilds its plan from that task's own real fs.read.
    let high_manifest = high_traces
        .iter()
        .find(|value| value["trace"]["kind"] == json!("manifest"))
        .expect("no high manifest");
    let high_reads = capability_results(&high_events, "fs.read");
    let high_capture = &high_manifest["trace"]["plan"]["context"]["captured_read"];
    assert_eq!(
        high_capture,
        high_reads
            .last()
            .copied()
            .expect("the high task has no real read"),
        "the high plan did not use its own real fs.read"
    );
    assert_eq!(
        serde_json::from_str::<Value>(high_capture["content"].as_str().unwrap())?,
        high_manifest["trace"]["plan"]["input"]
    );
    assert_eq!(
        node_traces(&high_traces, "completed"),
        ["input", "condition", "high", "result"]
    );
    let high = completed_result(&high_traces, "result");
    assert_eq!(high["output"]["classification"], "high");
    assert_eq!(high["output"]["value"], 7);
    assert_eq!(high["output"]["threshold"], 5);
    assert_eq!(high["output"]["visited_nodes"], 4);
    assert_eq!(high["state"]["visited"], 4);

    // Third run: the same edited source with a changed threshold must select
    // the other edge and produce a different final output.
    run_task(
        &client,
        &demo.session_id,
        "run the fixed condition on input.value low branch",
    )
    .await?;
    let low_events = events(&client, &demo.session_id).await?;
    let low_id = manifest_execution(&low_events, "issue31-low")
        .expect("the fixed low plan manifest was never published");
    match &execution(&low_events, &low_id)
        .expect("the fixed low execution never finished")
        .terminal
    {
        Some(ExecutionOutcome::Completed { .. }) => {}
        other => panic!("the low threshold run did not complete: {other:?}"),
    }
    let low_traces = traces(&low_events, &low_id);
    let low_manifest = low_traces
        .iter()
        .find(|value| value["trace"]["kind"] == json!("manifest"))
        .expect("no low manifest");
    let low_reads = capability_results(&low_events, "fs.read");
    let low_capture = &low_manifest["trace"]["plan"]["context"]["captured_read"];
    assert_eq!(
        low_capture,
        low_reads
            .last()
            .copied()
            .expect("the low task has no real read"),
        "the low plan did not use its own real fs.read"
    );
    assert_eq!(
        node_traces(&low_traces, "completed"),
        ["input", "condition", "low", "result"]
    );
    let low = completed_result(&low_traces, "result");
    assert_eq!(low["output"]["classification"], "low");
    assert_eq!(low["output"]["value"], 7);
    assert_eq!(low["output"]["threshold"], 20);
    assert_ne!(low["output"], high["output"]);

    // Later tasks re-read the real file but never repeat the side effect.
    let all = events(&client, &demo.session_id).await?;
    assert_eq!(
        capability_results(&all, "fs.write").len(),
        1,
        "a later task repeated the real side effect"
    );
    assert!(
        capability_results(&all, "fs.read").len() >= 3,
        "each task should have performed a real fs.read"
    );
    assert_eq!(
        std::fs::read_to_string(demo.workspace.join("side-effect.txt"))?,
        demo::SIDE_EFFECT_CONTENT
    );

    client.close().await;
    demo.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn node_waits_for_real_read_approval_and_replay_uses_its_captured_response() -> TestResult {
    let temporary = TempDir::new()?;
    let demo = demo::start_with_token(
        &demo::Options {
            root: temporary.path().join("waiting-demo"),
            listen: "127.0.0.1:0".parse()?,
            token_env: "CAYW_ISSUE31_UNUSED".into(),
        },
        TOKEN.to_owned(),
    )
    .await?;
    let client = Client::connect(demo.address, TOKEN).await?;
    let runner = Client::connect(demo.address, TOKEN).await?;
    let task = rpc(
        &client,
        "task.enqueue",
        json!({"session_id":demo.session_id,"text":"run the waiting initial condition"}),
    )
    .await?;
    let task_id = task["id"].as_str().unwrap().to_owned();
    let run = tokio::spawn(async move {
        let result = rpc(&runner, "task.resume", json!({"task_id":task_id})).await;
        runner.close().await;
        result
    });
    let approval = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let view = rpc(
                &client,
                "session.get",
                json!({"session_id":demo.session_id}),
            )
            .await?;
            if let Some(approval) = view["pending_approvals"].as_array().unwrap().first() {
                return TestResult::Ok(approval.clone());
            }
            tokio::task::yield_now().await;
        }
    })
    .await??;
    assert_eq!(approval["input"]["capability"], "fs.read");
    assert_eq!(
        serde_json::from_str::<Value>(approval["input"]["scope"].as_str().unwrap())?,
        json!({"path":"approval-input.json"})
    );
    let waiting = events(&client, &demo.session_id).await?;
    let id = manifest_execution(&waiting, "issue31-waiting").expect("waiting graph manifest");
    let store = TraceStore::open(temporary.path().join("waiting-trace.json"))?;
    store.ingest(&waiting)?;
    let snapshot = store.snapshot();
    let project = &snapshot.projects[&id];
    assert_eq!(project.runtime_status, Status::Running);
    assert_eq!(project.latest().unwrap().steps[0].status, Status::Running);
    assert_eq!(project.calls.len(), 1);
    assert_eq!(project.calls[0].node_id.as_deref(), Some("input"));
    assert!(project.calls[0].outcome.is_none());
    assert!(project.calls[0].started_at_ms.is_none());
    assert!(
        !run.is_finished(),
        "the node ran before its read was approved"
    );

    rpc(
        &client,
        "approval.decide",
        json!({"session_id":demo.session_id,"approval_id":approval["id"],"decision":"allow_once"}),
    )
    .await?;
    tokio::time::timeout(Duration::from_secs(15), run).await???;
    let completed = events(&client, &demo.session_id).await?;
    store.ingest(&completed)?;
    let snapshot = store.snapshot();
    let project = &snapshot.projects[&id];
    assert_eq!(project.runtime_status, Status::Failed);
    assert_eq!(project.calls[0].node_id.as_deref(), Some("input"));
    assert!(matches!(
        project.calls[0].outcome,
        Some(cayw_core::CapabilityOutcome::Completed { .. })
    ));

    // Undo-style entry evaluation must answer the read from the captured result.
    let input = project.revisions[0].plan.nodes[0].clone();
    let replay = store
        .edit(
            &id,
            Edit {
                expected_version: 1,
                node_id: "input".into(),
                source: input.source,
                params: input.params,
                input_pointer: input.input_pointer,
            },
            CancellationToken::new(),
        )
        .await?;
    let evaluation = replay.projects[&id].latest().unwrap();
    assert_eq!(evaluation.mode, "recompute");
    assert_eq!(evaluation.steps[0].status, Status::Completed);
    assert_eq!(evaluation.steps[0].output.as_ref().unwrap()["value"], 7);
    assert_eq!(evaluation.steps[1].status, Status::Failed);
    assert_eq!(
        events(&client, &demo.session_id).await?.len(),
        completed.len()
    );
    assert_eq!(capability_results(&completed, "fs.write").len(), 1);
    client.close().await;
    demo.shutdown().await;
    Ok(())
}
