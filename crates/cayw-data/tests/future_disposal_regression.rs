use cayw_ai::CancellationToken;
use cayw_data::{Limits, execute};
use serde_json::{Map, Value};
use std::process::Command;

fn child(mode: &str) {
    let output = Command::new(std::env::current_exe().expect("current test executable"))
        .args(["--exact", "lifecycle_child", "--ignored", "--nocapture"])
        .env("CAYW_DATA_LIFECYCLE_MODE", mode)
        .output()
        .expect("run the disposal probe in a separate process");
    assert!(
        output.status.success(),
        "{mode} child exited with {}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("deep arguments disposed"),
        "the child must actually execute the disposal probe"
    );
}

#[test]
fn dropping_an_unpolled_future_disposes_deep_arguments_in_a_subprocess() {
    child("unpolled");
}

#[test]
fn selecting_cancellation_disposes_an_unpolled_future_in_a_subprocess() {
    child("select");
}

#[test]
fn polling_a_future_rejects_and_disposes_deep_arguments_in_a_subprocess() {
    child("polled");
}

#[test]
#[ignore = "invoked by the parent tests in a separate process"]
fn lifecycle_child() {
    fn require_send<T: Send>(value: T) -> T {
        value
    }

    let mode = std::env::var("CAYW_DATA_LIFECYCLE_MODE").expect("parent selects the probe mode");
    let mut value = Value::Null;
    for _ in 0..100_000 {
        value = Value::Array(vec![value]);
    }
    let mut args = Map::new();
    args.insert("format".into(), Value::String("json".into()));
    args.insert("value".into(), value);
    let token = CancellationToken::new();
    // Construction and an unpolled drop must also work outside a Tokio runtime.
    let future = require_send(execute(
        "data.serialize",
        Value::Object(args),
        token.clone(),
        Limits::default(),
    ));
    if mode == "unpolled" {
        drop(future);
    } else {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("test runtime");
        runtime.block_on(async move {
            match mode.as_str() {
                "select" => {
                    token.cancel();
                    tokio::select! {
                        biased;
                        () = token.cancelled() => {},
                        result = future => panic!("cancelled branch must win: {result:?}"),
                    }
                }
                "polled" => {
                    let error = future.await.expect_err("deep input must be rejected");
                    assert!(error.message.starts_with("depth_limit:"), "{error:?}");
                    assert!(!error.outcome_unknown);
                }
                _ => panic!("unknown disposal probe mode"),
            }
        });
    }
    println!("deep arguments disposed");
}
