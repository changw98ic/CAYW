use cayw_ai::CancellationToken;
use cayw_data::{Limits, execute};
use serde_json::{Value, json};

#[tokio::test]
async fn csv_round_trip_preserves_bom_headers_quotes_and_rows() {
    for header in [
        json!(["\u{feff}"]),
        json!(["\u{feff}id"]),
        json!(["\u{feff}id", "id"]),
        json!(["\u{feff}id\"quoted"]),
        json!(["\u{feff}id\"quoted", "id"]),
    ] {
        let columns = header.as_array().unwrap().len();
        let original = json!({
            "header": header,
            "rows": [vec!["001"; columns], vec!["\u{feff}雪,\"quote\"\r\nnext"; columns]],
        });
        let encoded = execute(
            "data.serialize",
            json!({"format":"csv","value":original}),
            CancellationToken::new(),
            Limits::default(),
        )
        .await
        .unwrap();
        let decoded = execute(
            "data.parse",
            json!({"format":"csv","text":encoded["text"]}),
            CancellationToken::new(),
            Limits::default(),
        )
        .await
        .unwrap();
        assert_eq!(decoded["value"], original);
    }
}

#[tokio::test]
async fn csv_file_bom_precedes_quoted_headers_without_changing_literal_bom_fields() {
    for original in [
        json!({"header":["id", "note"],"rows":[["001", "雪"]]}),
        json!({"header":["\u{feff}id", "id"],"rows":[["001", "雪"]]}),
        json!({"header":["\u{feff}"],"rows":[["001"]]}),
    ] {
        let encoded = execute(
            "data.serialize",
            json!({"format":"csv","value":original}),
            CancellationToken::new(),
            Limits::default(),
        )
        .await
        .unwrap();
        for text in [
            format!("\u{feff}{}", encoded["text"].as_str().unwrap()),
            format!(
                "\u{feff}{}",
                encoded["text"].as_str().unwrap().replace('\n', "\r\n")
            ),
        ] {
            let decoded = execute(
                "data.parse",
                json!({"format":"csv","text":text}),
                CancellationToken::new(),
                Limits::default(),
            )
            .await
            .unwrap();
            assert_eq!(decoded["value"], original);
        }
    }
    for text in ["\u{feff}\n\"id\"\n", "\u{feff}\"id\"x\n", "\u{feff}\"id\n"] {
        let error = execute(
            "data.parse",
            json!({"format":"csv","text":text}),
            CancellationToken::new(),
            Limits::default(),
        )
        .await
        .expect_err("a file BOM must not relax CSV syntax");
        assert!(error.message.starts_with("syntax:"), "{text:?}: {error:?}");
        assert!(!error.outcome_unknown);
    }
}

#[tokio::test]
async fn yaml_quoted_merge_spelling_round_trips_as_an_ordinary_nested_key() {
    let original = json!({"<<":"literal","nested":[{"<<":{"<<":"雪"}}]});
    let encoded = execute(
        "data.serialize",
        json!({"format":"yaml","value":original}),
        CancellationToken::new(),
        Limits::default(),
    )
    .await
    .unwrap();
    let decoded = execute(
        "data.parse",
        json!({"format":"yaml","text":encoded["text"]}),
        CancellationToken::new(),
        Limits::default(),
    )
    .await
    .unwrap();
    assert_eq!(decoded["value"], original);

    for text in [
        "nested:\n  '<<': literal\n",
        "nested: {\"<<\": literal}\n",
        r#"nested: {"<<": literal}"#,
    ] {
        let decoded = execute(
            "data.parse",
            json!({"format":"yaml","text":text}),
            CancellationToken::new(),
            Limits::default(),
        )
        .await
        .unwrap();
        assert_eq!(decoded["value"], json!({"nested":{"<<":"literal"}}));
    }
}

#[tokio::test]
async fn yaml_still_rejects_actual_merges_anchors_aliases_and_explicit_tags() {
    for text in [
        "<<: {id: 1}\n",
        "nested: {<<: {id: 1}}\n",
        "base: &base {id: 1}\ncopy: {<<: *base}\n",
        "base: &base {id: 1}\ncopy: *base\n",
        "nested: {!!merge '<<': {id: 1}}\n",
        "nested: {!!str '<<': literal}\n",
    ] {
        let error = execute(
            "data.parse",
            json!({"format":"yaml","text":text}),
            CancellationToken::new(),
            Limits::default(),
        )
        .await
        .expect_err("merge semantics, aliases and tags remain unsupported");
        assert!(
            error.message.starts_with("unsupported:"),
            "{text}: {error:?}"
        );
        assert!(!error.outcome_unknown);
    }
}

fn byte_requests(value: &Value) -> Vec<(&'static str, Value)> {
    vec![
        ("data.serialize", json!({"format":"json","value":value})),
        (
            "data.parse",
            json!({"format":"json","text":value.to_string()}),
        ),
        (
            "codec.encode",
            json!({"encoding":"hex","input":{"kind":"bytes","value":value}}),
        ),
        (
            "codec.encode",
            json!({"encoding":"base64","input":{"kind":"bytes","value":value}}),
        ),
        (
            "hash.digest",
            json!({"algorithm":"sha256","input":{"kind":"bytes","value":value}}),
        ),
        (
            "hash.digest",
            json!({"algorithm":"blake3","input":{"kind":"bytes","value":value}}),
        ),
    ]
}

#[tokio::test]
async fn binary_input_arrays_use_the_same_node_budget_as_json_data() {
    for (value, max_nodes, expected_error) in [
        (json!([]), 0, Some("invalid_limits:")),
        (json!([]), 1, None),
        (json!([0]), 1, Some("node_limit:")),
        (json!([0]), 2, None),
        (json!([0, 255]), 2, Some("node_limit:")),
        (json!([0, 255]), 3, None),
        (
            json!([0, 1, 2, 3, 4, 5, 6, 7, 8, 9]),
            10,
            Some("node_limit:"),
        ),
        (json!([0, 1, 2, 3, 4, 5, 6, 7, 8, 9]), 11, None),
    ] {
        for (name, args) in byte_requests(&value) {
            let outcome = execute(
                name,
                args,
                CancellationToken::new(),
                Limits {
                    max_nodes,
                    ..Limits::default()
                },
            )
            .await;
            if let Some(prefix) = expected_error {
                let error = outcome.expect_err("the logical root and each byte consume a node");
                assert!(error.message.starts_with(prefix), "{name}: {error:?}");
                assert!(!error.outcome_unknown);
            } else {
                assert!(outcome.is_ok(), "{name}, nodes={max_nodes}: {outcome:?}");
            }
        }
    }
}

#[tokio::test]
async fn decoded_byte_arrays_count_the_root_and_each_byte() {
    for (hex, base64, expected, max_nodes, expected_error) in [
        ("", "", json!([]), 0, Some("invalid_limits:")),
        ("", "", json!([]), 1, None),
        ("00", "AA==", json!([0]), 1, Some("node_limit:")),
        ("00", "AA==", json!([0]), 2, None),
        ("00ff", "AP8=", json!([0, 255]), 2, Some("node_limit:")),
        ("00ff", "AP8=", json!([0, 255]), 3, None),
    ] {
        for (encoding, text) in [("hex", hex), ("base64", base64)] {
            let outcome = execute(
                "codec.decode",
                json!({"encoding":encoding,"text":text,"output":"bytes"}),
                CancellationToken::new(),
                Limits {
                    max_nodes,
                    ..Limits::default()
                },
            )
            .await;
            if let Some(prefix) = expected_error {
                let error = outcome.expect_err("the array root also consumes a node");
                assert!(error.message.starts_with(prefix), "{encoding}: {error:?}");
                assert!(!error.outcome_unknown);
            } else {
                assert_eq!(outcome.unwrap(), json!({"bytes":expected}));
            }
        }
    }
}

#[tokio::test]
async fn binary_arrays_obey_depth_and_row_limits() {
    for (limits, value, hex, expected_error) in [
        (
            Limits {
                max_depth: 1,
                ..Limits::default()
            },
            json!([]),
            "",
            None,
        ),
        (
            Limits {
                max_depth: 1,
                ..Limits::default()
            },
            json!([0]),
            "00",
            Some("depth_limit:"),
        ),
        (
            Limits {
                max_depth: 2,
                ..Limits::default()
            },
            json!([0]),
            "00",
            None,
        ),
        (
            Limits {
                max_rows: 1,
                ..Limits::default()
            },
            json!([0, 1]),
            "0001",
            Some("row_limit:"),
        ),
    ] {
        let mut requests = byte_requests(&value);
        requests.push((
            "codec.decode",
            json!({"encoding":"hex","text":hex,"output":"bytes"}),
        ));
        for (name, args) in requests {
            let outcome = execute(name, args, CancellationToken::new(), limits).await;
            if let Some(prefix) = expected_error {
                let error = outcome.expect_err("logical arrays must obey all data limits");
                assert!(error.message.starts_with(prefix), "{name}: {error:?}");
            } else {
                assert!(outcome.is_ok(), "{name}: {outcome:?}");
            }
        }
    }
}

#[tokio::test]
async fn encoding_strings_remain_scalar_data_and_byte_work_remains_bounded() {
    let limits = Limits {
        max_nodes: 1,
        max_depth: 1,
        ..Limits::default()
    };
    for input in [
        json!({"kind":"utf8","value":"abc"}),
        json!({"kind":"base64","value":"YWJj"}),
    ] {
        let encoded = execute(
            "codec.encode",
            json!({"encoding":"hex","input":input}),
            CancellationToken::new(),
            limits,
        )
        .await
        .unwrap();
        assert_eq!(encoded, json!({"text":"616263"}));
        let digest = execute(
            "hash.digest",
            json!({"algorithm":"sha256","input":input}),
            CancellationToken::new(),
            limits,
        )
        .await
        .unwrap();
        assert_eq!(
            digest,
            json!({"hex":"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"})
        );
    }
    for (encoding, text) in [("hex", "616263"), ("base64", "YWJj")] {
        let decoded = execute(
            "codec.decode",
            json!({"encoding":encoding,"text":text,"output":"utf8"}),
            CancellationToken::new(),
            limits,
        )
        .await
        .unwrap();
        assert_eq!(decoded, json!({"text":"abc"}));
    }
    for (name, args, limits, prefix) in [
        (
            "codec.encode",
            json!({"encoding":"hex","input":{"kind":"utf8","value":"abc"}}),
            Limits {
                input_bytes: 32,
                ..limits
            },
            "input_limit:",
        ),
        (
            "hash.digest",
            json!({"algorithm":"sha256","input":{"kind":"base64","value":"YWJj"}}),
            Limits {
                input_bytes: 32,
                ..limits
            },
            "input_limit:",
        ),
        (
            "codec.encode",
            json!({"encoding":"hex","input":{"kind":"bytes","value":[0]}}),
            Limits {
                max_nodes: 2,
                max_depth: 2,
                output_bytes: 12,
                ..limits
            },
            "output_limit:",
        ),
        (
            "codec.decode",
            json!({"encoding":"hex","text":"00","output":"bytes"}),
            Limits {
                max_nodes: 2,
                max_depth: 2,
                output_bytes: 12,
                ..limits
            },
            "output_limit:",
        ),
        (
            "codec.decode",
            json!({"encoding":"base64","text":"YWJj","output":"utf8"}),
            Limits {
                output_bytes: 13,
                ..limits
            },
            "output_limit:",
        ),
    ] {
        let error = execute(name, args, CancellationToken::new(), limits)
            .await
            .unwrap_err();
        assert!(error.message.starts_with(prefix), "{name}: {error:?}");
        assert!(!error.outcome_unknown);
    }
}
