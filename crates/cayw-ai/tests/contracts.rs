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
