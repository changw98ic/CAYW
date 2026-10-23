use cayw_ai::*;
use serde_json::json;

fn tool(name: &str) -> ToolDefinition {
    ToolDefinition {
        name: name.into(),
        description: "test tool".into(),
        parameters: json!({"type":"object"}),
    }
}

#[test]
fn transcript_replays_sections_and_tool_changes() {
    let messages = vec![
        Message::system("base"),
        Message::System(SystemMessage {
            sections: [
                ("one".into(), Some("old".into())),
                ("two".into(), Some("delete".into())),
            ]
            .into(),
            tools_added: vec![tool("first")],
            ..Default::default()
        }),
        Message::System(SystemMessage {
            sections: [("one".into(), Some("new".into())), ("two".into(), None)].into(),
            tools_removed: vec!["first".into()],
            tools_added: vec![tool("second")],
            ..Default::default()
        }),
    ];
    let current = current_system(&messages);
    assert_eq!(current.prompt, "base\n\nnew");
    assert_eq!(current.tools, vec![tool("second")]);
    assert!(tool_changes(&messages, &[tool("second")]).is_none());
    assert!(tool_changes(&messages, &[tool("first")]).is_some());
}

#[test]
fn tool_arguments_require_complete_objects() {
    assert_eq!(parse_tool_arguments("").unwrap(), json!({}));
    assert_eq!(parse_tool_arguments(r#"{"n":2}"#).unwrap(), json!({"n":2}));
    for input in ["[]", "null", "1", "\"a\"", "{\"n\":", "{\"n\":2} junk"] {
        assert!(parse_tool_arguments(input).is_err(), "accepted {input}");
    }
}

#[test]
fn duplicate_object_fields_are_rejected_at_every_depth() {
    for input in [
        r#"{"city":"Paris","city":"Tokyo"}"#,
        concat!(r#"{"city":"Paris",""#, r"\", r#"u0063ity":"Tokyo"}"#),
        r#"{"items":[{"n":1,"n":2}]}"#,
        r#"{"outer":{"n":1,"n":1}}"#,
    ] {
        let error = parse_tool_arguments(input).expect_err(input);
        assert!(error.to_string().contains("duplicate"), "{error}");
    }
    let input = r#"{"items":[{"n":1},{"n":2}],"nullable":null,"flag":true,"text":"你好","signed":-3,"fraction":1.25,"large":18446744073709551615}"#;
    assert_eq!(
        parse_tool_arguments(input).unwrap(),
        json!({"items":[{"n":1},{"n":2}],"nullable":null,"flag":true,"text":"你好","signed":-3,"fraction":1.25,"large":u64::MAX})
    );
}

#[test]
fn validation_is_strict_and_local_only() {
    let validator = ArgumentValidator::new(&json!({"type":"object", "properties":{"n":{"type":"integer"}}, "required":["n"], "additionalProperties":false})).unwrap();
    assert!(validator.validate(&json!({"n":2})).is_ok());
    let error = validator.validate(&json!({"n":"2"})).unwrap_err();
    assert!(error.to_string().contains("/n"));
    for schema in [
        json!({"$ref":"file:///etc/passwd"}),
        json!({"$ref":"https://example.invalid/schema"}),
        json!({"$defs":{"bad":{"$dynamicRef":"https://example.invalid"}}}),
    ] {
        assert!(ArgumentValidator::new(&schema).is_err());
    }
    assert!(ArgumentValidator::new(&json!({"type":"object","properties":{"x":{"$ref":"#/$defs/n"}},"$defs":{"n":{"type":"integer"}}})).is_ok());
}

#[test]
fn explicit_unsigned_integer_conversion_never_rounds_or_truncates() {
    for (text, expected) in [
        ("30000", 30000_u64),
        ("8192.0", 8192),
        ("3e4", 30000),
        ("300000e-1", 30000),
        (" 0008192.000 ", 8192),
        ("0.0e100", 0),
        ("9007199254740993.0", 9007199254740993),
        ("18446744073709551615", u64::MAX),
    ] {
        assert_eq!(
            unsigned_tool_integer(&json!(text)).unwrap(),
            expected,
            "{text}"
        );
    }
    for (value, expected) in [
        (json!(30000.0), 30000),
        (json!(8192.0), 8192),
        (json!(0.0), 0),
        (json!(9007199254740991.0), 9007199254740991),
    ] {
        assert_eq!(unsigned_tool_integer(&value).unwrap(), expected, "{value}");
    }
    for value in [
        json!(9007199254740992.0),
        json!(18446744073709551616.0),
        json!(-1.0),
        json!("8192.5"),
        json!("9007199254740993.1"),
        json!("1e-1"),
        json!("18446744073709551616"),
        json!("1e100"),
        json!("-1"),
        json!("true"),
        json!("NaN"),
        json!("Infinity"),
        json!(""),
        json!("1e"),
        json!("1."),
        json!(".1"),
        json!(true),
        json!(null),
        json!(1.5),
    ] {
        assert!(unsigned_tool_integer(&value).is_err(), "accepted {value}");
    }
}

#[test]
fn alternative_schema_errors_identify_fields_without_converting_arguments() {
    let validator = ArgumentValidator::new(&json!({
        "type": "object",
        "oneOf": [
            {"properties": {"action": {"const": "exec"}, "options": {
                "type": "object", "oneOf": [
                    {"properties": {"yield_time_ms": {"type": "integer"}, "max_output_tokens": {"type": "integer"}},
                     "required": ["yield_time_ms", "max_output_tokens"], "additionalProperties": false},
                    {"properties": {"preset": {"const": "default"}}, "required": ["preset"], "additionalProperties": false}
                ]}}, "required": ["action", "options"], "additionalProperties": false},
            {"properties": {"action": {"const": "wait"}, "cell_id": {"type": "string"}},
             "required": ["action", "cell_id"], "additionalProperties": false}
        ]
    })).unwrap();
    let invalid = json!({"action": "exec", "options": {"yield_time_ms": "30000", "max_output_tokens": "8192"}});
    let original = invalid.clone();
    let error = validator.validate(&invalid).unwrap_err().to_string();
    assert!(
        error.contains("/options/yield_time_ms") && error.contains("integer"),
        "{error}"
    );
    assert!(error.contains("/options/max_output_tokens"), "{error}");
    assert_eq!(invalid, original);
    assert!(validator.validate(&json!({"action": "exec", "options": {"yield_time_ms": 30000, "max_output_tokens": 8192}})).is_ok());
}

// Break caught: protocol replay identities and opaque signatures disappear after persistence.
#[test]
fn assistant_protocol_metadata_survives_serialization_without_entering_content() {
    let assistant = AssistantMessage::new(&Model::new("magpie", "test"));
    let mut value = serde_json::to_value(&assistant).unwrap();
    let metadata = json!({"openai_responses":{"output":[
        {"type":"reasoning","id":"rs-original","encrypted_content":"opaque-synthetic","summary":[]},
        {"type":"function_call","id":"fc-original","call_id":"call-original","name":"read","arguments":"{}","status":"completed"}
    ]}});
    value["provider_metadata"] = metadata.clone();
    let restored: AssistantMessage = serde_json::from_value(value).unwrap();
    let encoded = serde_json::to_value(&restored).unwrap();
    assert_eq!(encoded["provider_metadata"], metadata);
    assert_eq!(encoded["content"], json!([]));
    assert!(
        serde_json::to_value(assistant)
            .unwrap()
            .get("provider_metadata")
            .is_none()
    );
}

#[test]
fn message_wire_format_and_usage_round_trip() {
    let model = Model::new("fixture", "test");
    let mut assistant = AssistantMessage::new(&model);
    assistant
        .content
        .push(Content::ToolCall(ToolCall::new("1", "read", json!({}))));
    assistant.usage = Usage {
        input: 5,
        output: 10,
        cache_read: 2,
        cache_write: 3,
        reasoning: 4,
        total_tokens: 0,
    };
    assistant.usage.update_total();
    assert_eq!(assistant.usage.total_tokens, 20);
    let message = Message::Assistant(assistant);
    let value = serde_json::to_value(&message).unwrap();
    assert_eq!(value["role"], "assistant");
    assert_eq!(value["content"][0]["type"], "toolCall");
    assert_eq!(serde_json::from_value::<Message>(value).unwrap(), message);
}
