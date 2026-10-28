//! Explicit local replay of one complete HTTP capture; never contacts a model service.
use cayw_ai::{CancellationToken, Model, providers::OpenAiProvider};
use cayw_core::CoreEvent;
use cayw_runtime::{Runtime, RuntimeConfig, TaskState};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{path::PathBuf, sync::Arc, time::Instant};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CAYW_REPLAY_CAPTURE and a new CAYW_REPLAY_OUTPUT directory"]
async fn complete_capture_preserves_stream_and_accounting() {
    let capture = PathBuf::from(std::env::var_os("CAYW_REPLAY_CAPTURE").expect("capture path"));
    let output = PathBuf::from(std::env::var_os("CAYW_REPLAY_OUTPUT").expect("new output path"));
    std::fs::create_dir(&output).unwrap();
    let body = std::fs::read(&capture).unwrap();
    assert!(body.ends_with(b"data: [DONE]\n\n"));
    let capture_sha = format!("{:x}", Sha256::digest(&body));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut block = [0_u8; 8192];
        loop {
            let read = stream.read(&mut block).await.unwrap();
            assert_ne!(read, 0);
            request.extend_from_slice(&block[..read]);
            if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                let headers = std::str::from_utf8(&request[..end]).unwrap();
                let length = headers
                    .lines()
                    .find_map(|line| {
                        let (key, value) = line.split_once(':')?;
                        key.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap();
                if request.len() >= end + 4 + length {
                    break;
                }
            }
        }
        let headers = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(headers.as_bytes()).await.unwrap();
        // Fixed transport chunks on both sides of the comparison.
        for chunk in body.chunks(4096) {
            stream.write_all(chunk).await.unwrap();
        }
        stream.shutdown().await.unwrap();
    });
    let model = Model::new("local-capture", "opencode-go/deepseek-v4.1-flash");
    let provider = Arc::new(OpenAiProvider::new(format!("http://{address}/v1"), "").unwrap());
    let config = RuntimeConfig {
        max_total_tokens: 1_000_000,
        max_run_time: std::time::Duration::from_secs(1800),
        ..Default::default()
    };
    let workspace = output.join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let runtime = Runtime::open(
        output.join("session.db"),
        &workspace,
        provider,
        model,
        config,
    )
    .unwrap();
    let session = runtime
        .create(Some("local HTTP capture replay".into()))
        .unwrap()
        .id;
    let started = Instant::now();
    let result = runtime
        .prompt(
            &session,
            "Replay the captured response without external tools.",
            CancellationToken::new(),
        )
        .await;
    let elapsed = started.elapsed();
    server.await.unwrap();
    let ledger = runtime.model_requests(&session).unwrap();
    let task = runtime.tasks(&session).unwrap().pop().unwrap();
    let snapshot = runtime.get(&session).unwrap();
    let mut after = 0;
    let mut normalized = Vec::new();
    let mut agent_events = 0;
    let mut host_calls = 0;
    while after < snapshot.cursor {
        let page = runtime.events(&session, after, 1000).unwrap();
        assert!(!page.is_empty());
        for event in page {
            after = event.cursor;
            if let CoreEvent::Agent { event } = event.event {
                agent_events += 1;
                let mut value = serde_json::to_value(event).unwrap();
                remove_timing(&mut value);
                normalized.push(value);
            } else if matches!(event.event, CoreEvent::CapabilityStarted { .. }) {
                host_calls += 1;
            }
        }
    }
    let encoded = serde_json::to_vec(&normalized).unwrap();
    let message = &snapshot.messages.last().unwrap();
    let receipt = json!({
        "capture": capture, "capture_sha256": capture_sha,
        "external_model_requests": 0, "local_http_requests": 1,
        "elapsed_seconds": elapsed.as_secs_f64(), "result_error": result.as_ref().err().map(ToString::to_string),
        "task_state": task.state, "event_count": snapshot.cursor, "agent_events": agent_events,
        "host_calls": host_calls, "normalized_agent_events_sha256": format!("{:x}", Sha256::digest(&encoded)),
        "ledger": ledger, "last_message": message,
    });
    std::fs::write(
        output.join("receipt.json"),
        serde_json::to_vec_pretty(&receipt).unwrap(),
    )
    .unwrap();
    assert_eq!(task.state, TaskState::Completed);
    assert!(result.is_ok(), "{result:?}");
    assert!(!ledger.budget_unknown);
    assert_eq!(ledger.reserved_tokens, 0);
    assert_eq!(ledger.requests.len(), 1);
    assert_eq!(ledger.settled_tokens, 25_177);
    assert_eq!(host_calls, 0);
    println!(
        "{}",
        json!({"elapsed_seconds": elapsed.as_secs_f64(), "agent_events": agent_events, "settled_tokens": ledger.settled_tokens, "output": output})
    );
}

fn remove_timing(value: &mut Value) {
    match value {
        Value::Object(fields) => {
            fields.remove("timestamp");
            fields.remove("duration_ms");
            for value in fields.values_mut() {
                remove_timing(value);
            }
        }
        Value::Array(values) => values.iter_mut().for_each(remove_timing),
        _ => {}
    }
}
