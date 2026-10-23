use cayw_ai::ModelRequest;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::OnceLock,
};

fn directory(name: &str) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let parent = std::env::var_os("CAYW_ACCESS_EVIDENCE_OUT")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let root = parent.join(format!(
        "{name}-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    fs::create_dir(&root).unwrap();
    root
}
fn cli(arguments: &[&str], root: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cayw-verify-tool"))
        .args(arguments)
        .arg(root)
        .output()
        .unwrap()
}
fn json_file(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}
fn write_json(path: &Path, value: &Value) {
    fs::write(path, serde_json::to_vec(value).unwrap()).unwrap();
}
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn hash(path: &Path) -> Value {
    json!(digest(&fs::read(path).unwrap()))
}
fn save_output(root: &Path, name: &str, output: &Output) {
    fs::write(root.join(format!("{name}.stdout.log")), &output.stdout).unwrap();
    fs::write(root.join(format!("{name}.stderr.log")), &output.stderr).unwrap();
    fs::write(
        root.join(format!("{name}.exit.log")),
        format!("{}\n", output.status),
    )
    .unwrap();
}
fn baseline(name: &str) -> &'static Path {
    static APPROVAL: OnceLock<PathBuf> = OnceLock::new();
    static REJECTIONS: OnceLock<PathBuf> = OnceLock::new();
    let slot = if name == "ipc-approval" {
        &APPROVAL
    } else {
        &REJECTIONS
    };
    slot.get_or_init(|| {
        let parent = directory(name);
        let root = parent.join("baseline");
        let run = cli(
            &["sdk-ipc", "--samples", "1", "--scenario", name, "--out"],
            &root,
        );
        save_output(&parent, "generate", &run);
        assert!(
            run.status.success(),
            "CLI scenario failed: {}",
            String::from_utf8_lossy(&run.stderr)
        );
        let read = cli(&["read"], &root);
        save_output(&parent, "baseline-read", &read);
        assert!(
            read.status.success(),
            "original CLI report rejected: {}",
            String::from_utf8_lossy(&read.stdout)
        );
        root
    })
}
// Copies only fresh fixture output, never repository source or build inputs.
fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let destination = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &destination);
        } else {
            fs::copy(entry.path(), destination).unwrap();
        }
    }
}
fn sample_root(root: &Path, sample: &Value) -> PathBuf {
    root.join("samples")
        .join(sample["sample_id"].as_str().unwrap())
}
fn store(root: &Path, sample: &mut Value) {
    // Refresh outer integrity so rejection must come from the damaged evidence.
    for (index, observation) in sample["observations"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .enumerate()
    {
        observation["sequence"] = json!(index + 1);
    }
    let mut terminal_artifacts = sample["artifacts"].clone();
    terminal_artifacts
        .as_object_mut()
        .unwrap()
        .remove("records");
    sample["observations"]
        .as_array_mut()
        .unwrap()
        .last_mut()
        .unwrap()["value"]["artifacts"] = terminal_artifacts;
    let directory = sample_root(root, sample);
    let records = sample["observations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| serde_json::to_string(row).unwrap() + "\n")
        .collect::<String>();
    fs::write(directory.join("records.jsonl"), records).unwrap();
    sample["artifacts"]["records"]["sha256"] = hash(&directory.join("records.jsonl"));
    let mut report = sample.clone();
    report.as_object_mut().unwrap().remove("run_id");
    report.as_object_mut().unwrap().remove("meta");
    write_json(&directory.join("report.json"), &report);
    fs::write(
        root.join("samples.jsonl"),
        serde_json::to_string(sample).unwrap() + "\n",
    )
    .unwrap();
    let mut summary = json_file(&root.join("summary.json"));
    summary["artifacts"]["samples_sha256"] = hash(&root.join("samples.jsonl"));
    write_json(&root.join("summary.json"), &summary);
}
fn visit(value: &mut Value, change: &mut impl FnMut(&mut Value)) {
    change(value);
    match value {
        Value::Array(values) => values.iter_mut().for_each(|value| visit(value, change)),
        Value::Object(values) => values.values_mut().for_each(|value| visit(value, change)),
        _ => (),
    }
}
fn observation<'a>(sample: &'a mut Value, kind: &str, index: usize) -> &'a mut Value {
    &mut sample["observations"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .filter(|row| row["kind"] == kind)
        .nth(index)
        .unwrap()["value"]
}
fn refresh_attachment(sample: &mut Value, root: &Path, path: &str) {
    let digest = hash(&sample_root(root, sample).join(path));
    for descriptor in sample["artifacts"].as_object_mut().unwrap().values_mut() {
        if descriptor["path"] == path {
            descriptor["sha256"] = digest.clone();
        }
    }
}
fn transform_frames(sample: &mut Value, root: &Path, change: &mut impl FnMut(&mut Value)) {
    let path = sample_root(root, sample).join("access-ipc-frames.jsonl");
    let mut rows = fs::read_to_string(&path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    for row in &mut rows {
        let newline = row["utf8"].as_str().unwrap().ends_with('\n');
        visit(&mut row["frame"], change);
        let mut bytes = serde_json::to_vec(&row["frame"]).unwrap();
        if newline {
            bytes.push(b'\n');
        }
        row["utf8"] = json!(String::from_utf8(bytes.clone()).unwrap());
        row["bytes"] = json!(bytes.len());
        row["sha256"] = json!(digest(&bytes));
    }
    fs::write(
        path,
        rows.iter()
            .map(|row| serde_json::to_string(row).unwrap() + "\n")
            .collect::<String>(),
    )
    .unwrap();
    refresh_attachment(sample, root, "access-ipc-frames.jsonl");
}
fn mutation(name: &str, label: &str, damage: impl FnOnce(&Path, &mut Value)) -> bool {
    let parent = directory(label);
    let root = parent.join("damaged");
    copy_tree(baseline(name), &root);
    let mut sample: Value = serde_json::from_str(
        fs::read_to_string(root.join("samples.jsonl"))
            .unwrap()
            .trim(),
    )
    .unwrap();
    damage(&root, &mut sample);
    store(&root, &mut sample);
    let output = cli(&["read"], &root);
    save_output(&parent, "damaged-read", &output);
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    if output.status.success() || result["status"] != "invalid" {
        eprintln!("accepted damage {label}: {}", result);
        false
    } else {
        true
    }
}

// A true flag cannot replace the original approval, exact child identity,
// ordering, Provider-consumed result, or bytes written by the actual Host.
#[test]
fn approval_reader_rejects_missing_or_changed_original_execution_evidence() {
    let mut accepted = Vec::new();
    for label in [
        "missing-target",
        "changed-target",
        "actor",
        "lease",
        "task",
        "source",
        "parent",
        "decision-after-start",
        "provider-result",
        "actual-file",
    ] {
        if !mutation("ipc-approval", label, |root, sample| match label {
            "missing-target" => {
                observation(sample, "access_approval_waiting", 0)["approval"]["input"]
                    .as_object_mut()
                    .unwrap()
                    .remove("target");
            }
            "changed-target" => {
                observation(sample, "access_approval_waiting", 0)["approval"]["input"]["target"]["key"]
                    ["execution_id"] = json!("another-execution")
            }
            "actor" => {
                observation(sample, "access_approval_decided", 0)["response"]["decision"]["actor"] =
                    json!("model")
            }
            "lease" | "task" | "source" | "parent" => {
                let events = observation(sample, "access_approval_final", 0)["snapshot"]["events"]
                    .as_array_mut()
                    .unwrap();
                let call = &mut events
                    .iter_mut()
                    .find(|event| event["event"]["type"] == "capability_started")
                    .unwrap()["event"]["call"];
                match label {
                    "lease" => call["lease"]["fence"] = json!(999),
                    "task" => call["task_id"] = json!("other-task"),
                    "source" => call["source_hash"] = json!("sha256:other-source"),
                    _ => call["parent_call_id"] = json!("other-parent"),
                }
            }
            "decision-after-start" => {
                let events = observation(sample, "access_approval_final", 0)["snapshot"]["events"]
                    .as_array()
                    .unwrap();
                let decision = events
                    .iter()
                    .find(|event| event["event"]["type"] == "approval_decided")
                    .unwrap()
                    .clone();
                let start = events
                    .iter()
                    .find(|event| event["event"]["type"] == "capability_started")
                    .unwrap()
                    .clone();
                let mut swap = |value: &mut Value| {
                    if value["cursor"] == decision["cursor"] && value.get("event").is_some() {
                        value["event"] = start["event"].clone();
                    } else if value["cursor"] == start["cursor"] && value.get("event").is_some() {
                        value["event"] = decision["event"].clone();
                    }
                };
                visit(sample, &mut swap);
                transform_frames(sample, root, &mut swap);
            }
            "provider-result" => {
                let path = sample_root(root, sample).join("provider.jsonl");
                let mut entries = fs::read_to_string(&path)
                    .unwrap()
                    .lines()
                    .map(|line| serde_json::from_str::<Value>(line).unwrap())
                    .collect::<Vec<_>>();
                let message = entries[1]["request"]["messages"]
                    .as_array_mut()
                    .unwrap()
                    .iter_mut()
                    .find(|message| message["role"] == "toolResult")
                    .unwrap();
                message["structured_content"]["cell_id"] = json!("replaced-cell");
                let request: ModelRequest =
                    serde_json::from_value(entries[1]["request"].clone()).unwrap();
                entries[1]["request_sha256"] =
                    json!(digest(&serde_json::to_vec(&request).unwrap()));
                fs::write(
                    path,
                    entries
                        .iter()
                        .map(|entry| serde_json::to_string(entry).unwrap() + "\n")
                        .collect::<String>(),
                )
                .unwrap();
                visit(sample, &mut |value| {
                    if value.get("provider_requests").is_some()
                        && value["provider_requests"]
                            .as_array()
                            .is_some_and(|rows| rows.len() == 2)
                    {
                        value["provider_requests"] = json!(entries);
                    }
                });
                refresh_attachment(sample, root, "provider.jsonl");
            }
            "actual-file" => fs::write(
                sample_root(root, sample).join("workspace/result.txt"),
                b"wrong actual effect\n",
            )
            .unwrap(),
            _ => unreachable!(),
        }) {
            accepted.push(label);
        }
    }
    for name in ["access_ipc_frames", "access_provider_requests"] {
        let label = format!("missing-{name}");
        if !mutation("ipc-approval", &label, |_, sample| {
            sample["artifacts"].as_object_mut().unwrap().remove(name);
        }) {
            accepted.push(name);
        }
    }
    assert!(
        accepted.is_empty(),
        "approval damage still accepted: {accepted:?}"
    );
}

// Ignoring decision audits would accept erased, replayed or retargeted approval
// evidence even when the original capability events and outer digests are intact.
#[test]
fn approval_reader_rejects_missing_changed_or_reordered_decision_audit() {
    let mut accepted = Vec::new();
    for label in [
        "missing-start",
        "missing-commit",
        "duplicate-pair",
        "commit-before-start",
        "swapped-decisions",
        "start-session",
        "start-task",
        "start-attempt",
        "start-approval",
        "start-actor",
        "start-choice",
        "start-missing-actor",
        "start-extra-field",
        "commit-task",
        "commit-cursor",
        "commit-grant",
        "commit-extra-field",
        "first-pause-decided",
        "read-pause-missing",
        "read-pause-extra",
        "read-pause-timestamp",
        "final-missing-decisions",
        "final-worker",
        "final-reason",
    ] {
        if !mutation("ipc-approval", label, |_, sample| {
            let final_state = observation(sample, "access_approval_final", 0)["snapshot"].clone();
            for row in sample["observations"].as_array_mut().unwrap() {
                let waiting = row["kind"] == "access_approval_waiting";
                if !waiting && row["kind"] != "access_approval_final" {
                    continue;
                }
                let audit = row["value"]["snapshot"]["audit"].as_array_mut().unwrap();
                if label == "first-pause-decided" {
                    if waiting && audit.is_empty() {
                        audit.extend_from_slice(&final_state["audit"].as_array().unwrap()[..2]);
                    }
                    continue;
                }
                if audit.is_empty() {
                    continue;
                }
                match label {
                    "missing-start" => {
                        audit.remove(0);
                    }
                    "missing-commit" => {
                        audit.remove(1);
                    }
                    "duplicate-pair" => {
                        let pair = audit[..2].to_vec();
                        audit.splice(2..2, pair);
                    }
                    "commit-before-start" => audit.swap(0, 1),
                    "swapped-decisions" if !waiting => {
                        audit.swap(0, 2);
                        audit.swap(1, 3);
                        audit[1]["data"]["decision_cursor"] = json!(1);
                        audit[3]["data"]["decision_cursor"] = json!(3);
                    }
                    "start-session" => audit[0]["session_id"] = json!("other-session"),
                    "start-task" => audit[0]["task_id"] = json!("other-task"),
                    "start-attempt" => audit[0]["data"]["attempt_id"] = json!("other-attempt"),
                    "start-approval" => audit[0]["data"]["approval_id"] = json!("other-approval"),
                    "start-actor" => audit[0]["data"]["actor"] = json!("model"),
                    "start-choice" => audit[0]["data"]["choice"] = json!("allow_task"),
                    "start-missing-actor" => {
                        audit[0]["data"].as_object_mut().unwrap().remove("actor");
                    }
                    "start-extra-field" => audit[0]["data"]["extra"] = json!(true),
                    "commit-task" => audit[1]["task_id"] = json!("other-task"),
                    "commit-cursor" => audit[1]["data"]["decision_cursor"] = json!(3),
                    "commit-grant" => {
                        audit[1]["data"]["grant_id"] = final_state["grants"][1]["id"].clone()
                    }
                    "commit-extra-field" => audit[1]["data"]["extra"] = json!(true),
                    "read-pause-missing" if waiting => audit.clear(),
                    "read-pause-extra" if waiting => {
                        audit.extend_from_slice(&final_state["audit"].as_array().unwrap()[2..4]);
                    }
                    "read-pause-timestamp" if waiting => {
                        audit[0]["timestamp"] = json!(audit[0]["timestamp"].as_u64().unwrap() + 1);
                    }
                    "final-missing-decisions" if !waiting => {
                        audit.drain(..4);
                    }
                    "final-worker" if !waiting => {
                        audit[5]["data"]["worker_id"] = json!("other-worker")
                    }
                    "final-reason" if !waiting => {
                        audit[4]["data"]["reason"] = json!("other-reason")
                    }
                    _ => (),
                }
                // Preserve a contiguous envelope so semantic damage reaches the
                // decision sequence and full payload checks rather than a gap.
                for (index, record) in audit.iter_mut().enumerate() {
                    record["cursor"] = json!(index + 1);
                }
            }
        }) {
            accepted.push(label);
        }
    }
    assert!(
        accepted.is_empty(),
        "decision audit damage still accepted: {accepted:?}"
    );
}

// Each rejection needs its own original request/closure and complete unchanged
// facts; a list of passed check names cannot prove that a request was tested.
#[test]
fn rejection_reader_rejects_skipped_inputs_changed_wire_or_changed_business_facts() {
    let mut accepted = Vec::new();
    for label in [
        "missing-case",
        "raw-bytes",
        "raw-newline",
        "after-state",
        "both-state",
        "raw-dispatch",
        "client-dispatch",
        "client-error",
        "client-frame",
    ] {
        if !mutation("ipc-rejections", label, |root, sample| match label {
            "missing-case" => {
                sample["observations"]
                    .as_array_mut()
                    .unwrap()
                    .retain(|row| {
                        !(row["kind"] == "access_rejection"
                            && row["value"]["case"] == "wrong-token")
                    });
            }
            "raw-bytes" => {
                observation(sample, "access_rejection", 0)["wire"]["sent"][0]["bytes"][0] =
                    json!(32)
            }
            "raw-newline" => {
                let bytes = &mut observation(sample, "access_rejection", 2)["wire"]["sent"][1];
                let mut raw: Vec<u8> = serde_json::from_value(bytes["bytes"].clone()).unwrap();
                raw.push(b'\n');
                *bytes = json!({"bytes":raw,"length":raw.len(),"sha256":digest(&raw),"utf8":String::from_utf8(raw.clone()).unwrap()});
            }
            "after-state" => {
                observation(sample, "access_rejection", 0)["after"]["session"]["cursor"] = json!(2)
            }
            "both-state" => {
                for state in ["before", "after"] {
                    observation(sample, "access_rejection", 1)[state]["provider_calls"] = json!(1);
                }
            }
            "raw-dispatch" => {
                observation(sample, "access_rejection", 0)["handler_calls_after"] = json!(1)
            }
            "client-dispatch" => {
                let value = observation(sample, "access_rejection", 3);
                value["handler_calls_after"] = value["handler_calls_before"].clone();
            }
            "client-error" => {
                observation(sample, "access_rejection", 3)["error"]["outcome_unknown"] = json!(true)
            }
            "client-frame" => transform_frames(sample, root, &mut |value| {
                if value["method"] == "session.events" {
                    value["params"]["limit"] = json!(1);
                }
            }),
            _ => unreachable!(),
        }) {
            accepted.push(label);
        }
    }
    for name in ["access_ipc_frames", "access_provider_requests", "provider"] {
        let label = format!("missing-{name}");
        if !mutation("ipc-rejections", &label, |_, sample| {
            sample["artifacts"].as_object_mut().unwrap().remove(name);
        }) {
            accepted.push(name);
        }
    }
    assert!(
        accepted.is_empty(),
        "rejection damage still accepted: {accepted:?}"
    );
}
