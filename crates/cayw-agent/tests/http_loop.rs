use cayw_agent::{
    ai::{providers::OpenAiProvider, *},
    *,
};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

async fn request(socket: &mut TcpStream) -> Value {
    let mut bytes = Vec::new();
    let header_end;
    loop {
        let mut byte = [0];
        socket.read_exact(&mut byte).await.unwrap();
        bytes.push(byte[0]);
        if bytes.ends_with(b"\r\n\r\n") {
            header_end = bytes.len();
            break;
        }
    }
    let headers = String::from_utf8(bytes).unwrap();
    let length: usize = headers
        .lines()
        .find_map(|line| {
            line.split_once(':')
                .filter(|(key, _)| key.eq_ignore_ascii_case("content-length"))
                .map(|(_, value)| value.trim().parse().unwrap())
        })
        .unwrap();
    let mut body = vec![0; length];
    socket.read_exact(&mut body).await.unwrap();
    assert!(headers[..header_end].starts_with("POST /v1/chat/completions "));
    serde_json::from_slice(&body).unwrap()
}
async fn respond(socket: &mut TcpStream, delta: Value, reason: &str) {
    let body = format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"id":"response-1","model":"fixture","choices":[{"index":0,"delta":delta,"finish_reason":null}]}),
        json!({"id":"response-1","model":"fixture","choices":[{"index":0,"delta":{},"finish_reason":reason}]})
    );
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    socket.write_all(header.as_bytes()).await.unwrap();
    for fragment in body.as_bytes().chunks(7) {
        socket.write_all(fragment).await.unwrap();
    }
    socket.shutdown().await.unwrap();
}
struct Echo;
#[async_trait]
impl AgentTool for Echo {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "echo".into(),
            description: "return input".into(),
            parameters: json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}),
        }
    }
    async fn execute(&self, context: ToolContext) -> Result<ToolResult, String> {
        Ok(ToolResult::text(
            context.arguments["text"].as_str().unwrap(),
        ))
    }
}

#[tokio::test]
async fn actual_http_adapter_and_agent_complete_tool_round_trip() {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut first, _) = listener.accept().await.unwrap();
            let body = request(&mut first).await;
            assert_eq!(body["tools"][0]["function"]["name"], "echo");
            assert_eq!(body["messages"].as_array().unwrap().last().unwrap()["role"], "user");
            respond(&mut first, json!({"role":"assistant","tool_calls":[{"index":0,"id":"echo-1","type":"function","function":{"name":"echo","arguments":"{\"text\":\"你好\"}"}}]}), "tool_calls").await;
            let (mut second, _) = listener.accept().await.unwrap();
            let body = request(&mut second).await;
            let result = body["messages"].as_array().unwrap().last().unwrap();
            assert_eq!(result["role"], "tool");
            assert_eq!(result["tool_call_id"], "echo-1");
            assert_eq!(result["content"][0]["text"], "你好");
            respond(&mut second, json!({"role":"assistant","content":"已完成"}), "stop").await;
        });
        let provider = OpenAiProvider::new(format!("http://{address}/v1"), "").unwrap();
        let agent = Agent::new(Arc::new(provider), Model::new("local-fixture", "fixture"));
        agent.set_tools(vec![Arc::new(Echo)]).unwrap();
        let result = agent.prompt(Message::user("echo 你好")).await.unwrap();
        assert_eq!(result.last_message.stop_reason, StopReason::Stop);
        assert_eq!(content_text(&result.last_message.content), "已完成");
        assert_eq!(result.messages.iter().filter(|message| matches!(message, Message::ToolResult(_))).count(), 1);
        server.await.unwrap();
    }).await.expect("local HTTP loop stalled");
}

struct SideEffectProbe {
    executions: Arc<AtomicUsize>,
}

#[async_trait]
impl AgentTool for SideEffectProbe {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "side_effect_probe".into(),
            description: "runs a stateful probe".into(),
            parameters: json!({"type":"object"}),
        }
    }
    async fn execute(&self, _context: ToolContext) -> Result<ToolResult, String> {
        self.executions.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResult::text("probe effect performed"))
    }
}

async fn respond_raw(socket: &mut TcpStream, body: &str) {
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    socket.write_all(header.as_bytes()).await.unwrap();
    for fragment in body.as_bytes().chunks(7) {
        socket.write_all(fragment).await.unwrap();
    }
    socket.shutdown().await.unwrap();
}

#[tokio::test]
async fn fragmented_snapshot_regression_executes_one_tool_and_completes_next_turn() {
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let body = include_str!("../../cayw-ai/tests/fixtures/sanitized_live04_snapshot.sse")
                .replace("execute_code", "side_effect_probe");
            let (mut first, _) = listener.accept().await.unwrap();
            let input = request(&mut first).await;
            assert_eq!(input["tools"][0]["function"]["name"], "side_effect_probe");
            respond_raw(&mut first, &body).await;
            let (mut second, _) = listener.accept().await.unwrap();
            let input = request(&mut second).await;
            let messages = input["messages"].as_array().unwrap();
            assert_eq!(messages.last().unwrap()["role"], "tool");
            assert_eq!(
                messages.last().unwrap()["content"][0]["text"],
                "probe effect performed"
            );
            assert_eq!(
                messages
                    .iter()
                    .filter(|message| message["role"] == "tool")
                    .count(),
                1
            );
            respond(&mut second, json!({"content":"completed once"}), "stop").await;
        });
        let executions = Arc::new(AtomicUsize::new(0));
        let provider = OpenAiProvider::new(format!("http://{address}/v1"), "").unwrap();
        let agent = Agent::new(Arc::new(provider), Model::new("magpie", "fixture"));
        agent
            .set_tools(vec![Arc::new(SideEffectProbe {
                executions: executions.clone(),
            })])
            .unwrap();
        let result = agent
            .prompt(Message::user("run one probe and finish"))
            .await
            .unwrap();
        assert_eq!(result.last_message.stop_reason, StopReason::Stop);
        assert_eq!(content_text(&result.last_message.content), "completed once");
        assert_eq!(executions.load(Ordering::SeqCst), 1);
        assert_eq!(
            result
                .messages
                .iter()
                .filter(|message| matches!(message, Message::ToolResult(_)))
                .count(),
            1
        );
        server.await.unwrap();
    })
    .await
    .expect("fragmented HTTP tool loop stalled");
}

#[tokio::test]
async fn tool_with_side_effect_executes_only_once_when_second_turn_has_invalid_finish_reason() {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            // Turn 1: user asks to run probe; model calls side_effect_probe tool
            let (mut first, _) = listener.accept().await.unwrap();
            let body = request(&mut first).await;
            assert_eq!(body["tools"][0]["function"]["name"], "side_effect_probe");
            respond(
                &mut first,
                json!({
                    "role": "assistant",
                    "tool_calls": [{
                        "index": 0,
                        "id": "probe-call-1",
                        "type": "function",
                        "function": {
                            "name": "side_effect_probe",
                            "arguments": "{}"
                        }
                    }]
                }),
                "tool_calls",
            )
            .await;

            // Turn 2: tool result is received. Upstream returns finish_reason: "tool_calls"
            // but without any tool calls (the exact upstream bug from reg01-reg06), followed
            // by a trailing usage frame and [DONE].
            let (mut second, _) = listener.accept().await.unwrap();
            let body = request(&mut second).await;
            let result = body["messages"].as_array().unwrap().last().unwrap();
            assert_eq!(result["role"], "tool");
            assert_eq!(result["tool_call_id"], "probe-call-1");
            assert_eq!(result["content"][0]["text"], "probe effect performed");

            let turn2_body = format!(
                "data: {}\n\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                json!({
                    "id": "resp-2",
                    "model": "fixture",
                    "choices": [{
                        "index": 0,
                        "delta": {"role": "assistant", "content": "Ran probe successfully. 60 passed."},
                        "finish_reason": null
                    }]
                }),
                json!({
                    "id": "resp-2",
                    "model": "fixture",
                    "choices": [{
                        "index": 0,
                        "delta": {},
                        "finish_reason": "tool_calls"
                    }]
                }),
                json!({
                    "id": "resp-2",
                    "model": "fixture",
                    "choices": [],
                    "usage": {
                        "prompt_tokens": 1420,
                        "completion_tokens": 380,
                        "total_tokens": 1800
                    }
                })
            );
            respond_raw(&mut second, &turn2_body).await;
        });

        let executions = Arc::new(AtomicUsize::new(0));
        let probe_tool = Arc::new(SideEffectProbe {
            executions: executions.clone(),
        });
        let provider = OpenAiProvider::new(format!("http://{address}/v1"), "").unwrap();
        let agent = Agent::new(Arc::new(provider), Model::new("local-fixture", "fixture"));
        agent.set_tools(vec![probe_tool]).unwrap();

        let result = agent.prompt(Message::user("run probe")).await.unwrap();

        // 1. Tool side-effect executed exactly ONCE in Turn 1
        assert_eq!(
            executions.load(Ordering::SeqCst),
            1,
            "tool must have executed exactly once and never replayed"
        );

        // 2. The task terminates with an explicit protocol error
        assert_eq!(result.last_message.stop_reason, StopReason::Error);
        assert_eq!(
            result.last_message.error_message.as_deref(),
            Some("tool_calls finish_reason without tool calls")
        );

        // 3. Trailing usage is retained and reported, not dropped or zeroed
        assert_eq!(result.last_message.usage_status, UsageStatus::Reported);
        assert_eq!(result.last_message.usage.input, 1420);
        assert_eq!(result.last_message.usage.output, 380);
        assert_eq!(result.last_message.usage.total_tokens, 1800);

        // 4. No subsequent execution or tool calls are allowed
        assert_eq!(
            executions.load(Ordering::SeqCst),
            1,
            "tool execution count remains strictly 1"
        );

        server.await.unwrap();
    })
    .await
    .expect("local HTTP loop stalled");
}
