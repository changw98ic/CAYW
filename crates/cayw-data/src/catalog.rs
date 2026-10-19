use cayw_ai::ToolDefinition;
use serde_json::{Value, json};

fn object(properties: Value, required: &[&str]) -> Value {
    json!({"type":"object","properties":properties,"required":required,"additionalProperties":false})
}

pub(crate) fn definitions() -> Vec<ToolDefinition> {
    let format = json!({"enum":["json","toml","yaml","xml","csv"]});
    let text = json!({"type":"string"});
    let binary = json!({"oneOf":[
        object(json!({"kind":{"const":"utf8"},"value":text}), &["kind","value"]),
        object(json!({"kind":{"const":"base64"},"value":text}), &["kind","value"]),
        object(json!({"kind":{"const":"bytes"},"value":{"type":"array","items":{"type":"integer","minimum":0,"maximum":255}}}), &["kind","value"])
    ]});
    let variants = [
        (
            "data.parse",
            "Parse explicit UTF-8 text as JSON, TOML, a safe YAML subset, ordered XML nodes or a string CSV table.",
            object(json!({"format":format,"text":text}), &["format", "text"]),
        ),
        (
            "data.serialize",
            "Serialize a value without silently changing unsupported types.",
            object(json!({"format":format,"value":{}}), &["format", "value"]),
        ),
        (
            "data.query",
            "Resolve an RFC 6901 JSON pointer; missing paths and noncanonical array indices fail.",
            object(json!({"value":{},"pointer":text}), &["value", "pointer"]),
        ),
        (
            "data.transform",
            "Explicitly select object fields or array row indices at an optional JSON pointer. Return the selected value.",
            json!({"oneOf":[
                object(json!({"value":{},"pointer":text,"operation":{"const":"select_fields"},"fields":{"type":"array","items":text}}), &["value","operation","fields"]),
                object(json!({"value":{},"pointer":text,"operation":{"const":"select_rows"},"indices":{"type":"array","items":{"type":"integer","minimum":0}}}), &["value","operation","indices"])
            ]}),
        ),
        (
            "text.diff",
            "Compute a bounded line-based unified diff with LF separators and original CRLF content retained.",
            object(
                json!({"before":text,"after":text,"context":{"type":"integer","minimum":0,"maximum":100}}),
                &["before", "after"],
            ),
        ),
        (
            "hash.digest",
            "SHA-256 or BLAKE3 digest of explicitly typed input bytes. Returns lowercase hex.",
            object(
                json!({"algorithm":{"enum":["sha256","blake3"]},"input":binary}),
                &["algorithm", "input"],
            ),
        ),
        (
            "codec.encode",
            "Encode explicitly typed bytes as canonical padded standard Base64 or lowercase hex.",
            object(
                json!({"encoding":{"enum":["base64","hex"]},"input":binary}),
                &["encoding", "input"],
            ),
        ),
        (
            "codec.decode",
            "Decode strict standard Base64 or hex to explicit bytes or validated UTF-8 text.",
            object(
                json!({"encoding":{"enum":["base64","hex"]},"text":text,"output":{"enum":["bytes","utf8"]}}),
                &["encoding", "text", "output"],
            ),
        ),
    ];
    variants
        .into_iter()
        .map(|(name, description, parameters)| ToolDefinition {
            name: name.into(),
            description: description.into(),
            parameters,
        })
        .collect()
}
