#![cfg(target_os = "macos")]
//! A real second process must keep the effective scheduler/resource policy fixed.
use cayw_ai::{AssistantStream, CancellationToken, Model, ModelRequest, Provider};
use cayw_runtime::{ResourcePolicy, Runtime, RuntimeConfig};
use std::{path::Path, process::Stdio, sync::Arc, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

struct NeverCalled;
impl Provider for NeverCalled {
    fn stream(&self, _: ModelRequest, _: CancellationToken) -> AssistantStream {
        panic!("policy-only fixture must not invoke a model");
    }
}
fn open(
    root: &Path,
    concurrency: usize,
    memory: u64,
) -> Result<Runtime, cayw_runtime::RuntimeError> {
    Runtime::open(
        root.join("session.db"),
        root.join("workspace"),
        Arc::new(NeverCalled),
        Model::new("fixture", "policy-process"),
        RuntimeConfig {
            max_concurrent_runs: concurrency,
            max_queued_runs: 8,
            resources: ResourcePolicy {
                max_declared_memory_bytes: memory,
            },
            ..Default::default()
        },
    )
}

#[test]
#[ignore = "subprocess helper invoked explicitly by the parent test"]
fn held_runtime_process() {
    use std::io::Write;
    let root =
        std::env::var_os("CAYW_POLICY_PROCESS_FIXTURE").expect("parent supplies the fixture root");
    let runtime = open(Path::new(&root), 1, 100).unwrap();
    println!("policy fixture ready");
    std::io::stdout().flush().unwrap();
    let mut release = String::new();
    std::io::stdin().read_line(&mut release).unwrap();
    assert_eq!(release.trim(), "release");
    drop(runtime);
}

#[tokio::test]
async fn stopped_policy_can_change_after_the_original_process_releases_ownership() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("workspace")).unwrap();
    let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "held_runtime_process",
            "--nocapture",
        ])
        .env("CAYW_POLICY_PROCESS_FIXTURE", &root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap()).lines();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let line = output
                .next_line()
                .await
                .unwrap()
                .expect("fixture exited before opening Runtime");
            if line == "policy fixture ready" {
                break;
            }
        }
    })
    .await
    .expect("fixture did not publish its held policy");
    let requests = cayw_requests::RequestConfig {
        max_tasks: cayw_requests::RequestConfig::default().max_tasks + 1,
        ..Default::default()
    };
    let actor = cayw_requests::HostActor::new("process-policy-fixture").unwrap();
    let request_error = Runtime::adjust_request_limits(
        root.join("session.db"),
        requests.clone(),
        &actor,
        "change after the original process exits",
    )
    .expect_err("offline request adjustment must preserve the original process policy");
    assert!(
        request_error
            .to_string()
            .contains("another runtime connection"),
        "{request_error}"
    );
    let error = open(&root, 2, 200)
        .err()
        .expect("a second process must not replace a live policy");
    assert!(
        error.to_string().contains("another runtime connection"),
        "{error}"
    );
    let matching = open(&root, 1, 100).unwrap();
    assert_eq!(matching.scheduler(10).unwrap().max_concurrent_runs, 1);
    assert_eq!(
        matching
            .resources()
            .unwrap()
            .policy
            .max_declared_memory_bytes,
        100
    );
    drop(matching);
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"release\n")
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(10), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    let request_policy = Runtime::adjust_request_limits(
        root.join("session.db"),
        requests.clone(),
        &actor,
        "change after the original process exits",
    )
    .unwrap();
    assert_eq!(request_policy, requests);
    let amended = Runtime::open(
        root.join("session.db"),
        root.join("workspace"),
        Arc::new(NeverCalled),
        Model::new("fixture", "policy-process"),
        RuntimeConfig {
            max_concurrent_runs: 2,
            max_queued_runs: 8,
            request_limits: requests,
            resources: ResourcePolicy {
                max_declared_memory_bytes: 200,
            },
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(amended.scheduler(10).unwrap().max_concurrent_runs, 2);
    assert_eq!(
        amended
            .resources()
            .unwrap()
            .policy
            .max_declared_memory_bytes,
        200
    );
}
