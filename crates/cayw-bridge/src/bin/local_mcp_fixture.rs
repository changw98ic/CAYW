//! A small MCP 2026-07-28 stdio service for explicit local integration.
//! Each request carries version/capabilities; no initialization handshake exists.
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{self, BufRead, Write},
};

const VERSION: &str = "2026-07-28";
const TEXT: &str = "# Local addition\nUse the add tool with two integer arguments.\n";
fn send(value: Value) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    serde_json::to_writer(&mut stdout, &value)?;
    stdout.write_all(b"\n")?;
    stdout.flush()
}
fn result(id: &Value, mut value: Value) -> io::Result<()> {
    value["_meta"] = json!({"io.modelcontextprotocol/serverInfo":{
        "name":"cayw-local-fixture","version":"2"}});
    send(json!({"jsonrpc":"2.0","id":id,"result":value}))
}
fn error(id: &Value, code: i64, message: &str) -> io::Result<()> {
    send(json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}}))
}
fn tool(name: &str, description: &str, input: Value) -> Value {
    json!({"name":name,"description":description,"inputSchema":input,
        "outputSchema":{"type":"object","properties":{"sum":{"type":"integer"},"server_pid":{"type":"integer"},"connections":{"type":"integer"}},"required":["sum","server_pid","connections"],"additionalProperties":false}})
}
// Explicit host-only fault controls for actual transport negative tests.
fn fault_response(fault: &str, id: &Value) -> io::Result<bool> {
    match fault {
        "tool-rejected" => error(
            id,
            -32602,
            "host-selected fixture rejection before execution",
        )?,
        "tool-exit" => std::process::exit(7),
        "oversized-frame" => {
            send(json!({"jsonrpc":"2.0","id":id,"result":{"padding":"x".repeat(8192)}}))?
        }
        "malformed-error" => send(
            json!({"jsonrpc":"2.0","id":id,"error":{"code":"invalid","message":"malformed fixture error"}}),
        )?,
        "changed-identity" => send(json!({"jsonrpc":"2.0","id":id,"result":{
            "_meta":{"io.modelcontextprotocol/serverInfo":{"name":"replacement","version":"3"}},
            "content":[],"structuredContent":{"sum":42,"server_pid":std::process::id(),"connections":1}}}))?,
        "tool-error" => result(
            id,
            json!({"isError":true,"content":[{"type":"text","text":"fixture could not confirm effect"}]}),
        )?,
        "server-request" => send(
            json!({"jsonrpc":"2.0","id":100000,"method":"unsupported/server-request","params":{}}),
        )?,
        "frame-flood" => {
            for _ in 0..40 {
                send(json!({"jsonrpc":"2.0","method":"notifications/fixture","params":{}}))?;
            }
        }
        "stderr" => {
            let mut stderr = io::stderr().lock();
            stderr.write_all(&[b'x'; 4096])?;
            stderr.flush()?;
            return Ok(false);
        }
        _ => return Ok(false),
    }
    Ok(true)
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let fault = std::env::var("CAYW_MCP_FIXTURE_FAULT").unwrap_or_default();
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
        let method = request["method"].as_str().unwrap_or("");
        if method == "notifications/cancelled" && request.get("id").is_none() {
            if let Some(request_id) = params["requestId"].as_u64()
                && let Some(old_id) = pending.remove(&request_id)
            {
                // Cancellation does not undo effects. Deliberately send an old
                // result so the client must ignore it rather than settle a new call.
                result(
                    &old_id,
                    json!({"content":[{"type":"text","text":"late unused result"}],
                    "structuredContent":{"sum":999,"server_pid":std::process::id(),"connections":1}}),
                )?;
            }
            continue;
        }
        if request.get("id").is_none() {
            continue;
        }
        let meta = &params["_meta"];
        let Some(version) = meta["io.modelcontextprotocol/protocolVersion"].as_str() else {
            error(id, -32602, "missing per-request protocol version")?;
            continue;
        };
        if !meta["io.modelcontextprotocol/clientCapabilities"].is_object() {
            error(id, -32602, "missing per-request client capabilities")?;
            continue;
        }
        if version != VERSION {
            send(json!({"jsonrpc":"2.0","id":id,"error":{"code":-32022,
                "message":"Unsupported protocol version",
                "data":{"supported":[VERSION],"requested":version}}}))?;
            continue;
        }
        if method == "tools/call" && fault_response(&fault, id)? {
            continue;
        }
        if method == "resources/read" && fault == "resource-wait" {
            pending.insert(
                id.as_u64().ok_or("invalid fixture resource id")?,
                id.clone(),
            );
            continue;
        }
        match method {
            "ping" => result(id, json!({}))?,
            "tools/list" => {
                let input = json!({"type":"object","properties":{"a":{"type":"integer"},"b":{"type":"integer"}},"required":["a","b"],"additionalProperties":false});
                result(
                    id,
                    json!({"resultType":"complete","tools":[tool("add","Add two integers locally",input),
                    tool("wait","Wait until cancellation; emits progress but grants no permission",json!({"type":"object","properties":{},"additionalProperties":false}))]}),
                )?;
            }
            "tools/call" => match params["name"].as_str() {
                Some("add") => {
                    let arguments = &params["arguments"];
                    let sum = arguments
                        .as_object()
                        .filter(|value| value.len() == 2)
                        .and_then(|_| arguments["a"].as_i64())
                        .and_then(|a| arguments["b"].as_i64().and_then(|b| a.checked_add(b)));
                    if let Some(sum) = sum {
                        result(
                            id,
                            json!({"content":[{"type":"text","text":sum.to_string()}],
                            "structuredContent":{"sum":sum,"server_pid":std::process::id(),"connections":1}}),
                        )?;
                    } else {
                        error(id, -32602, "invalid integer arguments")?;
                    }
                }
                Some("wait")
                    if params["arguments"]
                        .as_object()
                        .is_some_and(|args| args.is_empty()) =>
                {
                    pending.insert(id.as_u64().ok_or("invalid fixture id")?, id.clone());
                    send(
                        json!({"jsonrpc":"2.0","method":"notifications/progress","params":{
                        "progressToken":params["_meta"]["progressToken"],"progress":1,"total":2}}),
                    )?;
                }
                _ => error(id, -32602, "unknown tool or invalid arguments")?,
            },
            "resources/list" => result(
                id,
                json!({"resultType":"complete","resources":[{"uri":"fixture://local/addition","name":"addition",
                    "description":"Local addition instructions resource",
                    "mimeType":"text/markdown"}]}),
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
