use cayw_verify_tool::sdk::{
    BuiltinScenario, NegativeControl, Scenario, ScenarioContext, ScenarioReport,
};
use serde_json::{Value, json};
use std::{fs, path::PathBuf, time::SystemTime};

struct Directory(PathBuf);
impl Directory {
    fn new(name: &str) -> Self {
        let parent = std::env::var_os("CAYW_ACCESS_TEST_OUT")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let unique = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = parent.join(format!("{name}-{}-{unique}", std::process::id()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        if std::env::var_os("CAYW_ACCESS_TEST_OUT").is_none() {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}

async fn execute(name: &str, directory: &Directory) -> ScenarioReport {
    let scenario = BuiltinScenario::parse(name).expect("access scenario is registered");
    let context = ScenarioContext {
        root: directory.0.clone(),
        iteration: 1,
        negative_control: NegativeControl::None,
        binding: json!({"source":"integration test; offline synthetic"}),
    };
    let mut report = ScenarioReport::new(
        name,
        1,
        scenario.specification(NegativeControl::None),
        context.binding.clone(),
    );
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        scenario.execute(&context, &mut report),
    )
    .await;
    if !matches!(&outcome, Ok(Ok(()))) {
        report.check("test_execution_terminal", false, format!("{outcome:?}"));
    }
    report.finish();
    fs::write(
        directory.0.join("test-report.json"),
        serde_json::to_vec_pretty(&report).unwrap(),
    )
    .unwrap();
    outcome
        .expect("access scenario watchdog")
        .expect("access scenario reaches its expected terminal");
    assert_eq!(report.status, "passed", "{:#?}", report.checks);
    assert!(!report.task_checks.is_empty());
    report
}

fn observations<'a>(report: &'a ScenarioReport, kind: &str) -> Vec<&'a Value> {
    report
        .observations
        .iter()
        .filter(|observation| observation.kind == kind)
        .map(|observation| &observation.value)
        .collect()
}

// Removing the real permission boundary, reusing the write approval for the read,
// or accepting a caller-supplied actor must break the observed execution facts.
#[tokio::test(flavor = "current_thread")]
async fn approvals_authorize_each_original_target_before_its_actual_effect() {
    let directory = Directory::new("ipc-approval");
    let report = execute("ipc-approval", &directory).await;
    let waiting = observations(&report, "access_approval_waiting");
    assert_eq!(waiting.len(), 2);
    assert_eq!(waiting[0]["approval"]["input"]["capability"], "fs.write");
    assert_eq!(waiting[0]["snapshot"]["file"], Value::Null);
    for (index, stage) in waiting.iter().enumerate() {
        let approval = &stage["approval"];
        assert!(approval["decision"].is_null());
        assert_eq!(approval["input"]["target"]["type"], "capability_call");
        assert_eq!(approval["input"]["target"]["key"]["sequence"], index);
        let starts = stage["snapshot"]["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|event| event["event"]["type"] == "capability_started")
            .collect::<Vec<_>>();
        assert_eq!(starts.len(), index);
        assert!(
            starts.iter().all(|event| {
                event["event"]["call"]["key"] != approval["input"]["target"]["key"]
            })
        );
    }
    assert_eq!(waiting[1]["approval"]["input"]["capability"], "fs.read");
    assert_ne!(waiting[0]["approval"]["id"], waiting[1]["approval"]["id"]);
    let decisions = observations(&report, "access_approval_decided");
    assert_eq!(decisions.len(), 2);
    for decision in decisions {
        assert_eq!(decision["response"]["decision"]["actor"], "rpc-user");
        assert_eq!(decision["response"]["decision"]["decision"], "approve");
        assert_eq!(decision["response"]["input"], decision["original"]["input"]);
    }
    let final_state = observations(&report, "access_approval_final");
    assert_eq!(final_state.len(), 1);
    let snapshot = &final_state[0]["snapshot"];
    assert_eq!(snapshot["session"]["state"], "idle");
    assert!(
        snapshot["session"]["pending_approvals"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(snapshot["tasks"].as_array().unwrap().len(), 1);
    assert_eq!(snapshot["tasks"][0]["state"], "completed");
    let starts = snapshot["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["event"]["type"] == "capability_started")
        .collect::<Vec<_>>();
    assert_eq!(starts.len(), 2);
    for (index, event) in starts.iter().enumerate() {
        let call = &event["event"]["call"];
        assert_eq!(call["authorization"]["actor"], "rpc-user");
        assert_eq!(call["authorization"]["target"], call["key"]);
        assert_eq!(
            call["authorization"]["approval_id"],
            waiting[index]["approval"]["id"]
        );
    }
    assert_eq!(
        fs::read(directory.0.join("workspace/result.txt")).unwrap(),
        "offline SDK/IPC fixture\n第二行\n".as_bytes()
    );
    assert_eq!(snapshot["provider_calls"], 2);
    assert_eq!(snapshot["provider_requests"].as_array().unwrap().len(), 2);
    assert_eq!(snapshot["grants"].as_array().unwrap().len(), 2);
    assert_eq!(snapshot["approvals"].as_array().unwrap().len(), 2);
    for approval in snapshot["approvals"].as_array().unwrap() {
        assert_eq!(approval["decision"]["actor"], "rpc-user");
    }
    let wire = fs::read_to_string(directory.0.join("access-ipc-frames.jsonl")).unwrap();
    let frames = wire
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    let prompt = frames
        .iter()
        .find(|frame| {
            frame["direction"] == "client_to_server" && frame["frame"]["method"] == "session.prompt"
        })
        .unwrap();
    let event_connections = frames
        .iter()
        .filter(|frame| {
            frame["direction"] == "client_to_server" && frame["frame"]["method"] == "session.events"
        })
        .map(|frame| frame["connection"].clone())
        .collect::<Vec<_>>();
    assert!(!event_connections.is_empty());
    assert!(
        event_connections
            .iter()
            .all(|connection| connection != &prompt["connection"])
    );
}

// Skipping any rejected input or silently mutating a task, event, permission,
// Provider count, or file must fail even if the server returns an error.
#[tokio::test(flavor = "current_thread")]
async fn rejected_wire_and_runtime_inputs_leave_every_observed_business_fact_unchanged() {
    let directory = Directory::new("ipc-rejections");
    let report = execute("ipc-rejections", &directory).await;
    let rejected = observations(&report, "access_rejection");
    let expected = [
        ("wrong-token", "unauthorized"),
        ("wrong-version", "unsupported_version"),
        ("oversized-no-newline", ""),
        ("prompt-extra-field", "invalid_params"),
        ("approval-forged-actor", "invalid_params"),
        ("events-zero-limit", "session_error"),
    ];
    assert_eq!(rejected.len(), expected.len());
    for (record, (case, code)) in rejected.iter().zip(expected) {
        assert_eq!(record["case"], case);
        assert_eq!(record["before"], record["after"], "{case}");
        assert_eq!(record["after"]["provider_calls"], 0);
        assert!(record["after"]["tasks"].as_array().unwrap().is_empty());
        assert_eq!(record["after"]["file"], Value::Null);
        assert_eq!(record["after"]["session"]["cursor"], 1);
        assert_eq!(record["after"]["sessions"].as_array().unwrap().len(), 1);
        for field in [
            "audit",
            "grants",
            "approvals",
            "provider_requests",
            "workspace",
        ] {
            assert!(
                record["after"][field].as_array().unwrap().is_empty(),
                "{case}: {field}"
            );
        }
        assert_eq!(record["after"]["events"].as_array().unwrap().len(), 1);
        assert_eq!(record["after"]["health"]["running"], 0);
        assert_eq!(record["after"]["health"]["queued"], 0);
        if record["wire"].is_object() {
            assert_eq!(record["wire"]["transport"], "direct_tcp");
            assert_eq!(
                record["handler_calls_before"],
                record["handler_calls_after"]
            );
        } else {
            assert_eq!(
                record["handler_calls_after"].as_u64().unwrap(),
                record["handler_calls_before"].as_u64().unwrap() + 1
            );
        }
        if case == "oversized-no-newline" {
            let bytes = record["wire"]["sent"][1]["bytes"].as_array().unwrap();
            assert!(bytes.len() > 1024);
            assert!(!bytes.iter().any(|byte| *byte == json!(10)));
            assert_eq!(record["wire"]["closed"], true);
            assert_eq!(record["wire"]["received"]["bytes"], json!([]));
            let hello: Value = serde_json::from_str(
                record["wire"]["hello_response_bytes"]["utf8"]
                    .as_str()
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(hello["result"]["version"], 1);
            assert_eq!(hello["result"]["max_frame_bytes"], 1024);
        } else if record["wire"].is_object() {
            assert_eq!(record["wire"]["response"]["error"]["code"], code);
            assert_eq!(record["wire"]["closed"], true);
            let response: Value =
                serde_json::from_str(record["wire"]["received"]["utf8"].as_str().unwrap()).unwrap();
            assert_eq!(response, record["wire"]["response"]);
            let hello: Value =
                serde_json::from_str(record["wire"]["sent"][0]["utf8"].as_str().unwrap()).unwrap();
            assert_eq!(hello["method"], "hello");
            assert_eq!(
                hello["version"],
                if case == "wrong-version" { 2 } else { 1 }
            );
            let prompt: Value =
                serde_json::from_str(record["wire"]["sent"][1]["utf8"].as_str().unwrap()).unwrap();
            assert_eq!(prompt["method"], "session.prompt");
        } else {
            assert_eq!(record["error"]["code"], code);
            assert_eq!(record["error"]["outcome_unknown"], false);
        }
    }
    assert!(!directory.0.join("workspace/result.txt").exists());
}
