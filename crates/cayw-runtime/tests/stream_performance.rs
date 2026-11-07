//! Local stage probe. Captured tools are never registered or executed.
use cayw_agent::AgentEvent;
use cayw_ai::{
    AssistantMessage, CancellationToken, Message, Model, ModelRequest, Provider, StopReason,
    StreamOptions, providers::OpenAiProvider,
};
use cayw_core::{CoreEvent, SessionStore};
use cayw_runtime::{Runtime, RuntimeConfig, TaskState};
use futures::StreamExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{path::Path, sync::Arc, time::Instant};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

async fn http(body: Vec<u8>) -> (Arc<OpenAiProvider>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        for round in 0..2 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut block = [0; 8192];
            loop {
                let read = socket.read(&mut block).await.unwrap();
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
            let final_body = b"data: {\"id\":\"local-final\",\"model\":\"probe\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"local replay complete\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\ndata: [DONE]\n\n";
            let response = if round == 0 { &body[..] } else { final_body };
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                response.len()
            );
            socket.write_all(headers.as_bytes()).await.unwrap();
            for chunk in response.chunks(4096) {
                socket.write_all(chunk).await.unwrap();
            }
            socket.shutdown().await.unwrap();
        }
    });
    (
        Arc::new(OpenAiProvider::new(format!("http://{address}/v1"), "").unwrap()),
        server,
    )
}

fn seed(store: &SessionStore, session: &str, bytes: usize) {
    let lease = store.begin_attempt(session, "local-history-seed").unwrap();
    let input = Message::user("x".repeat(bytes));
    let mut assistant = AssistantMessage::new(&Model::new("local", "seed"));
    assistant.stop_reason = StopReason::Stop;
    assistant.content.push(cayw_ai::Content::Text {
        text: "Local history seed complete.".into(),
    });
    for event in [
        AgentEvent::AgentStart,
        AgentEvent::TurnStart,
        AgentEvent::MessageStart {
            message: input.clone(),
        },
        AgentEvent::MessageEnd { message: input },
        AgentEvent::MessageStart {
            message: Message::Assistant(assistant.clone()),
        },
        AgentEvent::MessageEnd {
            message: Message::Assistant(assistant.clone()),
        },
        AgentEvent::TurnEnd {
            message: assistant,
            tool_results: vec![],
        },
        AgentEvent::AgentEnd { messages: vec![] },
    ] {
        store.append_agent_event(&lease, &event).unwrap();
    }
    store.finish_attempt(&lease).unwrap();
}

fn save(output: &Path, name: &str, value: &Value) {
    let path = output.join(name);
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .unwrap();
    serde_json::to_writer_pretty(file, value).unwrap();
}

fn seconds(started: Instant) -> f64 {
    started.elapsed().as_secs_f64()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CAYW_STREAM_FIXTURE and new CAYW_STREAM_OUTPUT; only local HTTP"]
async fn local_stream_stages() {
    let fixture = std::env::var_os("CAYW_STREAM_FIXTURE").expect("safe fixture");
    let output =
        std::path::PathBuf::from(std::env::var_os("CAYW_STREAM_OUTPUT").expect("new output"));
    std::fs::create_dir(&output).unwrap();
    let body = std::fs::read(fixture).unwrap();
    // Check the safe replacement itself before creating any Runtime.
    let mut tool_fragments = 0;
    for frame in std::str::from_utf8(&body).unwrap().split("\n\n") {
        let Some(payload) = frame.strip_prefix("data: ") else {
            continue;
        };
        if payload == "[DONE]" {
            continue;
        }
        let value: Value = serde_json::from_str(payload).unwrap();
        for choice in value["choices"].as_array().into_iter().flatten() {
            for call in choice["delta"]["tool_calls"]
                .as_array()
                .into_iter()
                .flatten()
            {
                let function = &call["function"];
                if let Some(name) = function["name"].as_str() {
                    assert_eq!(name, "stream_probe");
                }
                if function["arguments"].is_string() {
                    tool_fragments += 1;
                }
            }
        }
    }
    assert!(tool_fragments > 0);
    // The existing magpie configuration permits unsigned thinking history.
    // The HTTP endpoint above remains a loopback-only fixture server.
    let model = Model::new("magpie", "opencode-go/deepseek-v4.1-flash");
    let (provider, server) = http(body.clone()).await;
    let started = Instant::now();
    let mut stream = provider.stream(
        ModelRequest {
            model: model.clone(),
            messages: vec![Message::user("read-only stream probe")],
            options: StreamOptions::default(),
        },
        CancellationToken::new(),
    );
    let mut updates = Vec::new();
    let mut terminal = None;
    while let Some(event) = stream.next().await {
        if event.is_terminal() {
            terminal = Some(event.message().clone());
        } else {
            updates.push(event);
        }
    }
    let provider_seconds = seconds(started);
    server.abort();
    assert_eq!(terminal.as_ref().unwrap().stop_reason, StopReason::ToolUse);
    save(
        &output,
        "provider.json",
        &json!({"seconds": provider_seconds, "updates": updates.len(), "tool_fragments": tool_fragments, "terminal": terminal, "external_model_requests": 0}),
    );
    let store = SessionStore::open(output.join("core.db")).unwrap();
    let session = store.create_session("stage probe").unwrap().id;
    seed(&store, &session, 256 * 1024);
    let lease = store.begin_attempt(&session, "stream-probe").unwrap();
    let start = Message::Assistant(AssistantMessage::new(&model));
    for event in [
        AgentEvent::AgentStart,
        AgentEvent::TurnStart,
        AgentEvent::MessageStart { message: start },
    ] {
        store.append_agent_event(&lease, &event).unwrap();
    }
    let mut lease_seconds = 0.;
    let mut persist_seconds = 0.;
    let mut selected_seconds = 0.;
    let started = Instant::now();
    for (index, event) in updates.iter().enumerate() {
        let before = Instant::now();
        store.validate_lease(&lease).unwrap();
        lease_seconds += seconds(before);
        let before = Instant::now();
        store
            .append_agent_event(
                &lease,
                &AgentEvent::MessageUpdate {
                    message: event.message().clone(),
                    event: Box::new(event.clone()),
                },
            )
            .unwrap();
        persist_seconds += seconds(before);
        if index.is_multiple_of(100) {
            let before = Instant::now();
            store
                .events_by_kind(&session, 0, 1000, "host", Some("model_request"))
                .unwrap();
            selected_seconds += seconds(before);
        }
    }
    save(
        &output,
        "core.json",
        &json!({"seconds": seconds(started), "updates": updates.len(), "history_bytes": 256*1024, "lease_seconds": lease_seconds, "persist_seconds": persist_seconds, "selected_seconds": selected_seconds}),
    );
    for observe in [false, true] {
        let path = output.join(if observe { "observed" } else { "unobserved" });
        std::fs::create_dir(&path).unwrap();
        let workspace = path.join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let (provider, server) = http(body.clone()).await;
        let runtime = Runtime::open(
            path.join("session.db"),
            workspace,
            provider,
            model.clone(),
            RuntimeConfig::default(),
        )
        .unwrap();
        let session = runtime.create(None).unwrap().id;
        seed(runtime.store(), &session, 256 * 1024);
        let seeded = runtime.get(&session).unwrap();
        let history = json!({
            "messages": seeded.messages.len(),
            "message_json_bytes": serde_json::to_vec(&seeded.messages).unwrap().len(),
            "cursor": seeded.cursor,
            "synthetic_user_text_bytes": 256 * 1024,
        });
        let observer = if observe {
            let runtime = runtime.clone();
            let session = session.clone();
            Some(tokio::spawn(async move {
                let mut count = 0;
                let mut read_seconds = 0.;
                let mut bytes = 0;
                loop {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    let started = Instant::now();
                    let view = runtime.get(&session).unwrap();
                    bytes += serde_json::to_vec(&view).unwrap().len();
                    read_seconds += seconds(started);
                    count += 1;
                    if view.attempt.is_none() {
                        return json!({"count": count,"seconds": read_seconds,"bytes": bytes});
                    }
                }
            }))
        } else {
            None
        };
        let started = Instant::now();
        let result = runtime
            .prompt(
                &session,
                "Local inert replay. No tools are registered for the captured name.",
                CancellationToken::new(),
            )
            .await;
        let elapsed = seconds(started);
        let observation = match observer {
            Some(observer) => observer.await.unwrap(),
            None => Value::Null,
        };
        // A returned SessionView can contain a failed Task. Always clean up the
        // local server, including when no follow-up HTTP request was sent.
        server.abort();
        if let Err(error) = server.await {
            assert!(error.is_cancelled(), "{error}");
        }
        let tasks = runtime.tasks(&session).unwrap();
        let ledger = runtime.model_requests(&session).unwrap();
        let view = runtime.get(&session).unwrap();
        let mut after = 0;
        let mut host_calls = 0;
        while after < view.cursor {
            for event in runtime.events(&session, after, 1000).unwrap() {
                after = event.cursor;
                if matches!(event.event, CoreEvent::CapabilityStarted { .. }) {
                    host_calls += 1;
                }
            }
        }
        save(
            &output,
            if observe {
                "runtime-observed.json"
            } else {
                "runtime-unobserved.json"
            },
            &json!({"seconds": elapsed,"error": result.as_ref().err().map(ToString::to_string),"observer": observation,"cursor": view.cursor,"host_calls": host_calls,"tasks": tasks,"history": history,"ledger": ledger,"external_model_requests": 0,"fixture_sha256": format!("{:x}", Sha256::digest(&body))}),
        );
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].state, TaskState::Completed, "{tasks:?}");
        assert_eq!(host_calls, 0);
        assert!(!ledger.budget_unknown);
        assert_eq!(ledger.reserved_tokens, 0);
    }
}
