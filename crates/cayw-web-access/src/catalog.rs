//! Host/Runtime catalog surface for `web.search` / `web.fetch` / `web.extract`.

use cayw_ai::ToolDefinition;
use serde_json::{Value, json};

use crate::types::capabilities;

fn object(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false
    })
}

fn string(max: usize) -> Value {
    json!({"type": "string", "minLength": 1, "maxLength": max})
}

/// Canonical tool definitions for the host catalog. Never includes `search.query`.
pub fn definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: capabilities::WEB_SEARCH.into(),
            description: "Search the public web. Returns ranked hits and citations. Does not scan the local workspace (use search.query / fs.grep for that). PDF full-text and video are out of scope.".into(),
            parameters: object(
                json!({
                    "query": string(8192),
                    "limit": {"type": "integer", "minimum": 1, "maximum": 20}
                }),
                &["query"],
            ),
        },
        ToolDefinition {
            name: capabilities::WEB_FETCH.into(),
            description: "HTTP GET a public http(s) URL and store the body under a content_ref citation. Does not render pages or execute scripts.".into(),
            parameters: object(json!({"url": string(2048)}), &["url"]),
        },
        ToolDefinition {
            name: capabilities::WEB_EXTRACT.into(),
            description: "Fetch a public http(s) URL and extract readable text from HTML (or minimal PDF parenthesis/literal strings — not a full PDF engine). Video is unsupported.".into(),
            parameters: object(json!({"url": string(2048)}), &["url"]),
        },
    ]
}
