use cayw_agent::{
    ai::{providers::OpenAiProvider, *},
    *,
};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

async fn read_request(socket: &mut TcpStream) -> Value {
    let mut bytes = Vec::new();
    let header_end = loop {
        let mut byte = [0];
        socket.read_exact(&mut byte).await.unwrap();
        bytes.push(byte[0]);
        if bytes.ends_with(b"\r\n\r\n") {
            break bytes.len();
        }
    };
    let headers = std::str::from_utf8(&bytes).unwrap();
    let length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().unwrap())
        })
        .unwrap();
    let mut body = vec![0; length];
    socket.read_exact(&mut body).await.unwrap();
    assert!(
        std::str::from_utf8(&bytes[..header_end])
            .unwrap()
            .starts_with("POST /v1/chat/completions ")
    );
    serde_json::from_slice(&body).unwrap()
}

// Break caught: a usage-only HTTP frame stays inside the parser until more
// content arrives, so cancelling the Agent loses already received counts.
#[tokio::test]
async fn cancellation_after_usage_only_http_frame_keeps_the_report() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            assert_eq!(read_request(&mut socket).await["model"], "fixture-model");
            let body = format!("data: {}\n\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                json!({"choices":[{"index":0,"delta":{"content":"partial"},"finish_reason":null}]}),
                json!({"choices":[],"usage":{"prompt_tokens":20,"completion_tokens":3,"total_tokens":23}}),
                json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}));
            let headers = format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
            socket.write_all(headers.as_bytes()).await.unwrap();
            socket.write_all(body.as_bytes()).await.unwrap();
        });
        let provider = OpenAiProvider::new(format!("http://{address}/v1"), "fixture-key").unwrap();
        let agent = Agent::new(Arc::new(provider), Model::new("openai", "fixture-model"));
        let control = agent.control();
        agent.subscribe(move |event, _| {
            let control = control.clone();
            async move {
                if let AgentEvent::MessageUpdate { event, .. } = event
                    && serde_json::to_value(&event).unwrap()["type"] == "usage_update"
                { control.abort(); }
                Ok(())
            }
        });
        let result = agent.prompt(Message::user("observe usage then cancel")).await.unwrap();
        assert_eq!(result.last_message.stop_reason, StopReason::Aborted);
        assert_eq!(result.last_message.usage_status, UsageStatus::Reported);
        assert_eq!(result.last_message.usage.total_tokens, 23);
        assert_eq!(result.last_message.usage_fields["prompt_tokens"], 20);
        assert!(matches!(agent.snapshot().messages.last(), Some(Message::Assistant(message)) if message.usage.total_tokens == 23));
        server.await.unwrap();
    }).await.expect("local HTTP usage cancellation stalled");
}
