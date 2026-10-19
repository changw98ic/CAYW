//! A deliberately small MCP 2025-11-25 stdio fixture, not a general MCP server.
//! Implements initialize, tools/list/call, resources/list/read, ping and cancel.
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{self, BufRead, Write},
};

const VERSION: &str = "2025-11-25";
const TEXT: &str = "# Local addition\nUse the add tool with two integer arguments.\n";
fn send(value: Value) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    serde_json::to_writer(&mut stdout, &value)?;
    stdout.write_all(b"\n")?;
    stdout.flush()
}
fn result(id: &Value, value: Value) -> io::Result<()> {
    send(json!({"jsonrpc":"2.0","id":id,"result":value}))
}
fn error(id: &Value, code: i64, message: &str) -> io::Result<()> {
    send(json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}}))
}
fn tool(name: &str, description: &str, input: Value) -> Value {
    json!({"name":name,"description":description,"inputSchema":input,
        "outputSchema":{"type":"object","properties":{"sum":{"type":"integer"},"server_pid":{"type":"integer"},"initializations":{"type":"integer"}},"required":["sum","server_pid","initializations"],"additionalProperties":false}})
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut initialized = false;
    let mut ready = false;
    let mut initializations = 0_u64;
    let mut pending = BTreeMap::new();
    let mut input = io::stdin().lock();
    loop {
        let mut line = Vec::new();
        let bytes =
            std::io::Read::take(&mut input, 1024 * 1024 + 1).read_until(b'\n', &mut line)?;
        if bytes == 0 {
            break;
        }
        if line.len() > 1024 * 1024 || !line.ends_with(b"\n") {
            return Err("fixture request exceeds frame limit or is incomplete".into());
        }
        let request: Value = serde_json::from_slice(&line)?;
        if request["jsonrpc"] != "2.0" {
            return Err("invalid JSON-RPC version".into());
        }
        let id = &request["id"];
        let params = &request["params"];
        match request["method"].as_str().unwrap_or("") {
            "initialize" if !initialized && params["protocolVersion"] == VERSION => {
                initialized = true;
                initializations += 1;
                result(
                    id,
                    json!({"protocolVersion":VERSION,"capabilities":{"tools":{},"resources":{}},
                    "serverInfo":{"name":"cayw-local-fixture","version":"1"}}),
                )?;
            }
            "notifications/initialized" if initialized => ready = true,
            "notifications/cancelled" => {
                if let Some(request_id) = params["requestId"].as_u64()
                    && let Some(old_id) = pending.remove(&request_id)
                {
                    // A cancellation is a request to stop, not a rollback. This
                    // deliberately late result must never settle a newer call.
                    result(
                        &old_id,
                        json!({"content":[{"type":"text","text":"late unused result"}],
                        "structuredContent":{"sum":999,"server_pid":std::process::id(),"initializations":initializations}}),
                    )?;
                }
            }
            "ping" => result(id, json!({}))?,
            _ if !ready => error(id, -32000, "fixture is not initialized")?,
            "tools/list" => {
                let input = json!({"type":"object","properties":{"a":{"type":"integer"},"b":{"type":"integer"}},"required":["a","b"],"additionalProperties":false});
                result(
                    id,
                    json!({"tools":[tool("add","Add two integers locally",input),
                    tool("wait","Wait until cancellation; emits progress but grants no permission",json!({"type":"object","properties":{},"additionalProperties":false}))]}),
                )?;
            }
            "tools/call" => match params["name"].as_str() {
                Some("add") => {
                    let sum = params["arguments"]["a"].as_i64().and_then(|a| {
                        params["arguments"]["b"]
                            .as_i64()
                            .and_then(|b| a.checked_add(b))
                    });
                    if let Some(sum) = sum {
                        result(
                            id,
                            json!({"content":[{"type":"text","text":sum.to_string()}],
                            "structuredContent":{"sum":sum,"server_pid":std::process::id(),"initializations":initializations}}),
                        )?;
                    } else {
                        error(id, -32602, "invalid integer arguments")?;
                    }
                }
                Some("wait") => {
                    pending.insert(id.as_u64().ok_or("invalid fixture id")?, id.clone());
                    send(
                        json!({"jsonrpc":"2.0","method":"notifications/progress","params":{
                        "progressToken":params["_meta"]["progressToken"],"progress":1,"total":2}}),
                    )?;
                }
                _ => error(id, -32602, "unknown tool")?,
            },
            "resources/list" => result(
                id,
                json!({"resources":[{"uri":"fixture://local/addition","name":"addition","mimeType":"text/markdown"}]}),
            )?,
            "resources/read" if params["uri"] == "fixture://local/addition" => result(
                id,
                json!({"contents":[{"uri":"fixture://local/addition","mimeType":"text/markdown","text":TEXT}]}),
            )?,
            _ => error(id, -32601, "method not found")?,
        }
    }
    Ok(())
}
