use cayw_agent::{
    ai::{providers::OpenAiProvider, *},
    *,
};
use serde_json::{Value, json};
use std::sync::Arc;
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
