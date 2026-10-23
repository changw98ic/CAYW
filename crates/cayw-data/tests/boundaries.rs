use cayw_ai::CancellationToken;
use cayw_data::{Limits, execute};
use serde_json::{Value, json};

async fn check(name: &str, args: Value, limits: Limits, prefix: &str) {
    let error = execute(name, args, CancellationToken::new(), limits)
        .await
        .unwrap_err();
    assert!(
        error.message.starts_with(prefix),
        "expected {prefix}, got {error}"
    );
    assert!(!error.outcome_unknown);
}

#[tokio::test]
async fn every_parser_rejects_deep_input_before_unbounded_recursion() {
    let depth = Limits {
        max_depth: 4,
        ..Limits::default()
    };
    for (format, text) in [
        ("json", "[[[[[0]]]]]"),
        ("yaml", "[[[[[0]]]]]"),
        ("toml", "a = [[[[[0]]]]]"),
        ("toml", "a.b.c.d.e = 0"),
        ("xml", "<a><b><c><d><e/></d></c></b></a>"),
    ] {
        check(
            "data.parse",
            json!({"format":format,"text":text}),
            depth,
            "depth_limit:",
        )
        .await;
    }
    let over = format!("{}0{}", "[".repeat(20_000), "]".repeat(20_000));
    check(
        "data.parse",
        json!({"format":"json","text":over}),
        Limits::default(),
        "depth_limit:",
    )
    .await;
}

#[tokio::test]
async fn parser_output_expansion_is_bounded_as_encoded_json() {
    let limits = Limits {
        output_bytes: 80,
        ..Limits::default()
    };
    for (format, text) in [
        (
            "json",
            "[0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29]",
        ),
        (
            "yaml",
            "[0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29]",
        ),
        ("toml", "a.b.c.d.e.f.g.h.i.j.k.l.m.n = 1"),
        ("xml", "<a><b/><b/><b/><b/><b/></a>"),
        ("csv", "a,b,c\n1,2,3\n1,2,3\n1,2,3\n1,2,3\n1,2,3\n"),
    ] {
        check(
            "data.parse",
            json!({"format":format,"text":text}),
            limits,
            "output_limit:",
        )
        .await;
    }
    check("codec.decode", json!({"encoding":"base64","text":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA","output":"bytes"}), limits, "output_limit:").await;
}

#[tokio::test]
async fn array_and_node_budgets_apply_to_all_value_formats() {
    for (format, text) in [
        ("json", "[1,2,3]"),
        ("yaml", "[1,2,3]"),
        ("toml", "x=[1,2,3]"),
        ("csv", "x\n1\n2\n3\n"),
        ("xml", "<x><a/><b/><c/></x>"),
    ] {
        check(
            "data.parse",
            json!({"format":format,"text":text}),
            Limits {
                max_rows: 2,
                ..Limits::default()
            },
            "row_limit:",
        )
        .await;
    }
    for (format, text) in [
        ("json", "{\"a\":1,\"b\":2,\"c\":3}"),
        ("yaml", "{a: 1, b: 2, c: 3}"),
        ("toml", "a=1\nb=2\nc=3"),
        ("xml", "<a x='1' y='2' z='3'/>"),
        ("csv", "a,b,c\n1,2,3\n"),
    ] {
        check(
            "data.parse",
            json!({"format":format,"text":text}),
            Limits {
                max_nodes: 4,
                ..Limits::default()
            },
            "node_limit:",
        )
        .await;
    }
}

#[tokio::test]
async fn serde_formats_do_not_change_negative_zero_or_control_characters() {
    let value =
        json!({"zero":-0.0,"control":"\u{0}\u{1}\u{8}\u{c}\t\r\n", "quoted":"\\\"", "float":2.0});
    for format in ["json", "toml", "yaml"] {
        let encoded = execute(
            "data.serialize",
            json!({"format":format,"value":value}),
            CancellationToken::new(),
            Limits::default(),
        )
        .await
        .unwrap();
        let decoded = execute(
            "data.parse",
            json!({"format":format,"text":encoded["text"]}),
            CancellationToken::new(),
            Limits::default(),
        )
        .await
        .unwrap();
        assert_eq!(decoded["value"], value, "{format}");
        assert!(
            decoded["value"]["zero"]
                .as_f64()
                .unwrap()
                .is_sign_negative()
        );
        assert!(decoded["value"]["float"].as_number().unwrap().is_f64());
    }
}

#[tokio::test]
async fn xml_references_and_normalized_whitespace_preserve_meaning() {
    let value =
        json!({"name":"r", "attributes":{"a":"\t\n\r雪&\""}, "children":[{"text":"\r\n\t<&雪"}]});
    let encoded = execute(
        "data.serialize",
        json!({"format":"xml","value":value}),
        CancellationToken::new(),
        Limits::default(),
    )
    .await
    .unwrap();
    let decoded = execute(
        "data.parse",
        json!({"format":"xml","text":encoded["text"]}),
        CancellationToken::new(),
        Limits::default(),
    )
    .await
    .unwrap();
    assert_eq!(decoded["value"], value);
    for text in [
        "<r>\u{1}</r>",
        "<r>&#xFFFF;</r>",
        "<r>]]></r>",
        "<r a='&bad;' />",
        "<r a='&#1;' />",
        "<r a='1'\na='2'/>",
        "<r></r foo>",
        "<r a='x'junk='y'/>",
    ] {
        assert!(
            execute(
                "data.parse",
                json!({"format":"xml","text":text}),
                CancellationToken::new(),
                Limits::default()
            )
            .await
            .is_err(),
            "accepted {text:?}"
        );
    }
}

#[tokio::test]
async fn yaml_nonstring_keys_are_rejected_even_when_nested() {
    for text in [
        "{null: x}",
        "{true: x}",
        "a: {1.0: x}",
        "{a: {x: 1, x: 2}}",
        "{<<: {a: 1}}",
        "a: &v test",
        "a: 01",
        "a: +1",
        "a: 0o12",
        "a: 1_000",
    ] {
        assert!(
            execute(
                "data.parse",
                json!({"format":"yaml","text":text}),
                CancellationToken::new(),
                Limits::default()
            )
            .await
            .is_err(),
            "accepted {text}"
        );
    }
}

#[tokio::test]
async fn integer_tokens_and_unique_selections_are_checked_after_schema() {
    for (name, args) in [
        (
            "codec.encode",
            json!({"encoding":"base64","input":{"kind":"bytes","value":[-0.0]}}),
        ),
        ("text.diff", json!({"before":"a","after":"b","context":1.0})),
        (
            "data.transform",
            json!({"value":[1,2],"operation":"select_rows","indices":[0.0]}),
        ),
        (
            "data.transform",
            json!({"value":[1,2],"operation":"select_rows","indices":[0,0]}),
        ),
        (
            "data.transform",
            json!({"value":{"a":1},"operation":"select_fields","fields":["a","a"]}),
        ),
    ] {
        check(name, args, Limits::default(), "arguments:").await;
    }
    for text in [
        "01",
        "+1",
        "1.",
        ".1",
        "NaN",
        "[true false]",
        "{\"x\":0,}",
        "\"bad\nstring\"",
    ] {
        check(
            "data.parse",
            json!({"format":"json","text":text}),
            Limits::default(),
            "syntax:",
        )
        .await;
    }
}

#[tokio::test]
async fn diff_separates_distant_hunks_and_retains_crlf_and_missing_newlines() {
    let diff = execute(
        "text.diff",
        json!({"before":"a\nb\nc\nd\ne\n", "after":"x\nb\nc\nd\ny\n", "context":0}),
        CancellationToken::new(),
        Limits::default(),
    )
    .await
    .unwrap();
    assert_eq!(
        diff,
        json!({"diff":"--- before\n+++ after\n@@ -1,1 +1,1 @@\n-a\n+x\n@@ -5,1 +5,1 @@\n-e\n+y\n"})
    );
    let diff = execute(
        "text.diff",
        json!({"before":"x\r\n", "after":"x\n", "context":0}),
        CancellationToken::new(),
        Limits::default(),
    )
    .await
    .unwrap();
    assert_eq!(
        diff["diff"],
        json!("--- before\n+++ after\n@@ -1,1 +1,1 @@\n-x\r\n+x\n")
    );
    let diff = execute(
        "text.diff",
        json!({"before":"x", "after":"", "context":0}),
        CancellationToken::new(),
        Limits::default(),
    )
    .await
    .unwrap();
    assert_eq!(
        diff["diff"],
        json!("--- before\n+++ after\n@@ -1,1 +0,0 @@\n-x\n\\ No newline at end of file\n")
    );
    check(
        "text.diff",
        json!({"before":"x\n".repeat(10_000), "after":"y\n".repeat(10_000)}),
        Limits::default(),
        "diff_work_limit:",
    )
    .await;
}

// One executor thread continues to run cancellation while CPU work is offloaded.
// No wall-clock duration is asserted; the large diff must still be in flight.
#[tokio::test(flavor = "current_thread")]
async fn a_running_diff_yields_to_executor_cancellation() {
    let cancel = CancellationToken::new();
    let future = execute(
        "text.diff",
        json!({"before":"a\n".repeat(1_900),"after":"b\n".repeat(1_900)}),
        cancel.clone(),
        Limits {
            max_diff_work: 16_000_000,
            ..Limits::default()
        },
    );
    tokio::pin!(future);
    tokio::select! {
        biased;
        result = &mut future => panic!("operation completed before cancellation: {result:?}"),
        () = tokio::task::yield_now() => cancel.cancel(),
    }
    let result = future.await.unwrap_err();
    assert!(result.message.starts_with("cancelled:"), "{result}");
    assert!(!result.outcome_unknown);
}

#[tokio::test]
async fn ordinary_toml_float_arrays_do_not_consume_nesting_budget() {
    let value = json!({"items":[1.0,2.0,3.0,4.0,5.0,6.0,7.0,8.0]});
    let limits = Limits {
        max_depth: 4,
        ..Limits::default()
    };
    let encoded = execute(
        "data.serialize",
        json!({"format":"toml","value":value}),
        CancellationToken::new(),
        limits,
    )
    .await
    .unwrap();
    let decoded = execute(
        "data.parse",
        json!({"format":"toml","text":encoded["text"]}),
        CancellationToken::new(),
        limits,
    )
    .await
    .unwrap();
    assert_eq!(decoded["value"], value);
}

#[tokio::test]
async fn toml_strings_escape_del_and_unicode_controls() {
    let value = json!({"\u{7f}":"\u{7f}\u{85}\u{2028}\u{2029}雪"});
    let encoded = execute(
        "data.serialize",
        json!({"format":"toml","value":value}),
        CancellationToken::new(),
        Limits::default(),
    )
    .await
    .unwrap();
    let decoded = execute(
        "data.parse",
        json!({"format":"toml","text":encoded["text"]}),
        CancellationToken::new(),
        Limits::default(),
    )
    .await
    .unwrap();
    assert_eq!(decoded["value"], value);
}

#[tokio::test]
async fn empty_and_single_empty_csv_fields_round_trip() {
    for value in [
        json!({"header":["a"],"rows":[[""],["x"]]}),
        json!({"header":[""],"rows":[]}),
        json!({"header":["a","b"],"rows":[]}),
    ] {
        let encoded = execute(
            "data.serialize",
            json!({"format":"csv","value":value}),
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
        assert_eq!(decoded["value"], value);
    }
}

#[tokio::test]
async fn yaml_json_float_serialization_preserves_sign_and_exponents() {
    let value = json!([1.23e-200, 1.0e200, -0.0, "false", "2026-10-18", "1e309"]);
    let encoded = execute(
        "data.serialize",
        json!({"format":"yaml","value":value}),
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
    assert_eq!(decoded["value"], value);
}

#[tokio::test]
async fn base64_chunk_boundaries_preserve_bytes_and_reject_early_padding() {
    let bytes: Vec<u8> = (0..6200).map(|i| (i % 256) as u8).collect();
    let encoded = execute(
        "codec.encode",
        json!({"encoding":"base64","input":{"kind":"bytes","value":bytes}}),
        CancellationToken::new(),
        Limits::default(),
    )
    .await
    .unwrap();
    let decoded = execute(
        "codec.decode",
        json!({"encoding":"base64","text":encoded["text"],"output":"bytes"}),
        CancellationToken::new(),
        Limits::default(),
    )
    .await
    .unwrap();
    assert_eq!(decoded["bytes"], json!(bytes));
    let invalid = format!("{}Zg==AAAA", "AAAA".repeat(1023));
    check(
        "codec.decode",
        json!({"encoding":"base64","text":invalid,"output":"bytes"}),
        Limits::default(),
        "syntax:",
    )
    .await;
}

#[tokio::test]
async fn deeply_nested_argument_values_fail_without_recursive_disposal() {
    let mut value = Value::Null;
    for _ in 0..20_000 {
        value = Value::Array(vec![value]);
    }
    let mut args = serde_json::Map::new();
    args.insert("format".into(), Value::String("json".into()));
    args.insert("value".into(), value);
    check(
        "data.serialize",
        Value::Object(args),
        Limits::default(),
        "depth_limit:",
    )
    .await;
}

#[tokio::test]
async fn shared_budgets_and_pre_cancel_cover_serialize_query_and_transform() {
    let cases = [
        (
            "data.serialize",
            json!({"format":"json","value":{"text":"x".repeat(200)}}),
            Limits {
                output_bytes: 32,
                ..Limits::default()
            },
            "output_limit:",
        ),
        (
            "data.query",
            json!({"value":[[[[[0]]]]],"pointer":""}),
            Limits {
                max_depth: 4,
                ..Limits::default()
            },
            "depth_limit:",
        ),
        (
            "data.query",
            json!({"value":[1,2,3,4,5],"pointer":""}),
            Limits {
                max_nodes: 4,
                ..Limits::default()
            },
            "node_limit:",
        ),
        (
            "data.transform",
            json!({"value":[[[[[0]]]]],"operation":"select_rows","indices":[0]}),
            Limits {
                max_depth: 4,
                ..Limits::default()
            },
            "depth_limit:",
        ),
        (
            "data.transform",
            json!({"value":{"a":1,"b":2,"c":3},"operation":"select_fields","fields":["a"]}),
            Limits {
                max_nodes: 4,
                ..Limits::default()
            },
            "node_limit:",
        ),
    ];
    for (name, args, limits, prefix) in cases {
        check(name, args, limits, prefix).await;
    }

    for (name, args) in [
        (
            "data.serialize",
            json!({"format":"json","value":{"text":"x"}}),
        ),
        ("data.query", json!({"value":{"a":1},"pointer":"/a"})),
        (
            "data.transform",
            json!({"value":{"a":1},"operation":"select_fields","fields":["a"]}),
        ),
    ] {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let error = execute(name, args, cancel, Limits::default())
            .await
            .unwrap_err();
        assert!(error.message.starts_with("cancelled:"), "{name}: {error}");
        assert!(!error.outcome_unknown);
    }
}

#[tokio::test]
async fn invalid_large_arguments_produce_a_small_schema_error() {
    let args = json!({"encoding":"hex","input":{"kind":"bytes","value":vec!["not-a-byte"; 2_000]}});
    let error = execute(
        "codec.encode",
        args,
        CancellationToken::new(),
        Limits::default(),
    )
    .await
    .unwrap_err();
    assert!(error.message.starts_with("arguments:"));
    assert!(
        error.message.len() < 512,
        "schema error expanded to {} bytes",
        error.message.len()
    );
}

#[tokio::test]
async fn yaml_strings_preserve_unicode_line_separators_and_controls() {
    let value = json!({"\u{7f}":"\u{7f}\u{85}\u{2028}\u{2029}\u{feff}雪"});
    let encoded = execute(
        "data.serialize",
        json!({"format":"yaml","value":value}),
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
    assert_eq!(decoded["value"], value);
}

#[tokio::test]
async fn identical_diff_still_validates_integer_context_and_row_limits() {
    check(
        "text.diff",
        json!({"before":"same", "after":"same", "context":1.0}),
        Limits::default(),
        "arguments:",
    )
    .await;
    check(
        "text.diff",
        json!({"before":"a\nb\nc\n", "after":"a\nb\nc\n"}),
        Limits {
            max_rows: 2,
            ..Limits::default()
        },
        "row_limit:",
    )
    .await;
}

#[tokio::test]
async fn invalid_limits_fail_without_work() {
    for limits in [
        Limits {
            input_bytes: 0,
            ..Limits::default()
        },
        Limits {
            max_depth: 65,
            ..Limits::default()
        },
        Limits {
            max_diff_work: usize::MAX,
            ..Limits::default()
        },
    ] {
        check(
            "data.parse",
            json!({"format":"json","text":"{}"}),
            limits,
            "invalid_limits:",
        )
        .await;
    }
}
