use cayw_ai::{ArgumentValidator, CancellationToken};
use cayw_data::{Limits, definitions, execute};
use serde_json::{Value, json};

async fn call(name: &str, args: Value) -> Value {
    execute(name, args, CancellationToken::new(), Limits::default())
        .await
        .unwrap()
}

async fn rejects(name: &str, args: Value, kind: &str) {
    let error = execute(name, args, CancellationToken::new(), Limits::default())
        .await
        .expect_err("invalid data was accepted");
    assert!(error.message.starts_with(kind), "{error}");
    assert!(!error.outcome_unknown);
}

// Removing the catalog or allowing unknown argument fields breaks host validation.
#[tokio::test]
async fn catalog_and_dispatch_have_one_strict_contract() {
    let catalog = definitions();
    let names: Vec<_> = catalog.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "data.parse",
            "data.serialize",
            "data.query",
            "data.transform",
            "text.diff",
            "hash.digest",
            "codec.encode",
            "codec.decode"
        ]
    );
    for definition in catalog {
        let validator = ArgumentValidator::new(&definition.parameters).unwrap();
        assert!(validator.validate(&json!({"unknown":true})).is_err());
    }
    rejects("parse", json!({}), "unknown_capability:").await;
    rejects(
        "data.parse",
        json!({"format":"json", "text":"{}", "extra":1}),
        "arguments:",
    )
    .await;
    rejects(
        "data.parse",
        json!({"format":"json", "text":42}),
        "arguments:",
    )
    .await;
}

// Converting strings to numbers, losing Unicode/escapes, or rounding integers fails.
#[tokio::test]
async fn json_round_trip_preserves_types_unicode_and_integer_precision() {
    let value = json!({"text":"中文 🦀\n\"\\", "literal":"001", "number":18446744073709551615_u64, "items":[null, false, 1.25]});
    let serialized = call("data.serialize", json!({"format":"json", "value":value})).await;
    let parsed = call(
        "data.parse",
        json!({"format":"json", "text":serialized["text"]}),
    )
    .await;
    assert_eq!(parsed, json!({"value":value}));
}

#[tokio::test]
async fn json_rejects_bad_syntax_duplicate_keys_and_overflowing_integers() {
    for text in [
        "{",
        "[1,]",
        "{\"x\":1,\"x\":2}",
        "18446744073709551616",
        "-9223372036854775809",
        "1e999",
        "{} {}",
    ] {
        rejects(
            "data.parse",
            json!({"format":"json", "text":text}),
            "syntax:",
        )
        .await;
    }
}

#[tokio::test]
async fn toml_round_trip_and_unsupported_types_are_explicit() {
    let expected = json!({"title":"雪\n\"", "enabled":true, "server":{"port":8080}, "items":[1,2], "float":1.5});
    let serialized = call("data.serialize", json!({"format":"toml", "value":expected})).await;
    assert_eq!(
        call(
            "data.parse",
            json!({"format":"toml", "text":serialized["text"]})
        )
        .await,
        json!({"value":expected})
    );
    for value in [
        json!({"x":null}),
        json!({"x":18446744073709551615_u64}),
        json!([1, 2]),
    ] {
        rejects(
            "data.serialize",
            json!({"format":"toml", "value":value}),
            "unsupported:",
        )
        .await;
    }
    for text in ["x = inf", "x = nan", "x = 2026-10-18"] {
        rejects(
            "data.parse",
            json!({"format":"toml", "text":text}),
            "unsupported:",
        )
        .await;
    }
    rejects(
        "data.parse",
        json!({"format":"toml", "text":"a = ["}),
        "syntax:",
    )
    .await;
}

#[tokio::test]
async fn yaml_block_input_and_safe_serialization_preserve_values() {
    let expected = json!({"name":"雪", "quoted":"001", "items":[true, null, 2, {"x":"a\nb"}]});
    let parsed = call("data.parse", json!({"format":"yaml", "text":"name: 雪\nquoted: '001'\nitems:\n  - true\n  - null\n  - 2\n  - x: |\n      a\n      b\n"})).await;
    assert_eq!(parsed["value"]["items"][3]["x"], json!("a\nb\n"));
    let serialized = call("data.serialize", json!({"format":"yaml", "value":expected})).await;
    assert_eq!(
        call(
            "data.parse",
            json!({"format":"yaml", "text":serialized["text"]})
        )
        .await,
        json!({"value":expected})
    );
}

#[tokio::test]
async fn yaml_refuses_lossy_or_expanding_documents() {
    for text in [
        "x: 1\nx: 2",
        "1: value",
        "? [a, b]\n: c",
        "a: &a [x]\nb: *a",
        "a: .nan",
        "a: .Inf",
        "a: !!binary abc",
        "---\na\n---\nb",
        "x: 0x10",
        "x: 18446744073709551616",
        "x: [",
    ] {
        let error = execute(
            "data.parse",
            json!({"format":"yaml", "text":text}),
            CancellationToken::new(),
            Limits::default(),
        )
        .await
        .expect_err(text);
        assert!(!error.outcome_unknown);
    }
}

#[tokio::test]
async fn xml_mixed_content_keeps_order_and_escapes_attributes_and_text() {
    let expected = json!({"name":"根", "attributes":{"id":"a&\"b"}, "children":[{"text":"前&"}, {"name":"b", "attributes":{}, "children":[{"text":"雪<"}]}, {"text":"后"}]});
    let parsed = call("data.parse", json!({"format":"xml", "text":"<根 id=\"a&amp;&quot;b\">前&amp;<b>雪&lt;</b><![CDATA[后]]></根>"})).await;
    assert_eq!(parsed, json!({"value":expected}));
    let serialized = call("data.serialize", json!({"format":"xml", "value":expected})).await;
    assert_eq!(
        call(
            "data.parse",
            json!({"format":"xml", "text":serialized["text"]})
        )
        .await,
        json!({"value":expected})
    );
}

#[tokio::test]
async fn xml_rejects_entities_duplicate_attributes_invalid_names_and_dropped_nodes() {
    for text in [
        "<!DOCTYPE r [<!ENTITY x SYSTEM 'file:///etc/passwd'>]><r>&x;</r>",
        "<r a='1' a='2'/>",
        "<1bad/>",
        "<r></s>",
        "<r>&missing;</r>",
        "<r>&#0;</r>",
        "<r><!--comment--></r>",
        "<r/><?pi value?>",
        "<r/><s/>",
        "<r>",
        "text",
        "<r a='<bad'/>",
    ] {
        assert!(
            execute(
                "data.parse",
                json!({"format":"xml", "text":text}),
                CancellationToken::new(),
                Limits::default()
            )
            .await
            .is_err(),
            "accepted {text}"
        );
    }
    rejects(
        "data.serialize",
        json!({"format":"xml", "value":{"name":"bad name", "attributes":{}, "children":[]}}),
        "syntax:",
    )
    .await;
    rejects(
        "data.serialize",
        json!({"format":"xml", "value":{"name":"r", "attributes":{}, "children":[], "lost":1}}),
        "arguments:",
    )
    .await;
}

#[tokio::test]
async fn csv_preserves_strings_newlines_empty_fields_and_header_order() {
    let table = json!({"header":["id","note","empty"], "rows":[["001","雪,\"quote\"\nline",""],["true","x","0"]]});
    let serialized = call("data.serialize", json!({"format":"csv", "value":table})).await;
    assert_eq!(
        call(
            "data.parse",
            json!({"format":"csv", "text":serialized["text"]})
        )
        .await,
        json!({"value":table})
    );
    for text in [
        "a,a\n1,2\n",
        "a,b\n1\n",
        "a\n\"unterminated",
        "a\nx\"y\n",
        "a\n\"x\"bad\n",
        "a\n\n",
        "",
    ] {
        assert!(
            execute(
                "data.parse",
                json!({"format":"csv", "text":text}),
                CancellationToken::new(),
                Limits::default()
            )
            .await
            .is_err(),
            "accepted {text:?}"
        );
    }
    rejects(
        "data.serialize",
        json!({"format":"csv", "value":{"header":["a"],"rows":[[1]]}}),
        "arguments:",
    )
    .await;
}

#[tokio::test]
async fn pointer_escaping_and_explicit_selection_do_not_coerce_types() {
    assert_eq!(
        call(
            "data.query",
            json!({"value":{"a/b":{"~x":["001", false]}},"pointer":"/a~1b/~0x/0"})
        )
        .await,
        json!({"value":"001"})
    );
    assert_eq!(
        call(
            "data.transform",
            json!({"value":{"a":1,"b":"2"},"operation":"select_fields","fields":["b"]})
        )
        .await,
        json!({"value":{"b":"2"}})
    );
    assert_eq!(call("data.transform", json!({"value":{"items":["a","b","c"]},"pointer":"/items","operation":"select_rows","indices":[2,0]})).await, json!({"value":["c","a"]}));
    for pointer in ["a", "/~2", "/00", "/-", "/2"] {
        assert!(
            execute(
                "data.query",
                json!({"value":[1],"pointer":pointer}),
                CancellationToken::new(),
                Limits::default()
            )
            .await
            .is_err()
        );
    }
    rejects(
        "data.transform",
        json!({"value":{"a":1},"operation":"select_fields","fields":["missing"]}),
        "selection:",
    )
    .await;
    rejects(
        "data.transform",
        json!({"value":[1],"operation":"select_rows","indices":["0"]}),
        "arguments:",
    )
    .await;
}

#[tokio::test]
async fn codecs_distinguish_utf8_bytes_and_canonical_base64() {
    assert_eq!(
        call(
            "codec.encode",
            json!({"encoding":"base64","input":{"kind":"bytes","value":[0,255,128]}})
        )
        .await,
        json!({"text":"AP+A"})
    );
    assert_eq!(
        call(
            "codec.decode",
            json!({"encoding":"base64","text":"AP+A","output":"bytes"})
        )
        .await,
        json!({"bytes":[0,255,128]})
    );
    assert_eq!(
        call(
            "codec.encode",
            json!({"encoding":"hex","input":{"kind":"utf8","value":"雪"}})
        )
        .await,
        json!({"text":"e99baa"})
    );
    assert_eq!(
        call(
            "codec.decode",
            json!({"encoding":"hex","text":"e99baa","output":"utf8"})
        )
        .await,
        json!({"text":"雪"})
    );
    assert_eq!(
        call(
            "codec.encode",
            json!({"encoding":"hex","input":{"kind":"base64","value":"AP+A"}})
        )
        .await,
        json!({"text":"00ff80"})
    );
    for text in ["Zg", "Zh==", " Zg==", "Zg===", "Zg==\n"] {
        rejects(
            "codec.decode",
            json!({"encoding":"base64","text":text,"output":"bytes"}),
            "syntax:",
        )
        .await;
    }
    rejects(
        "codec.decode",
        json!({"encoding":"hex","text":"ff","output":"utf8"}),
        "encoding:",
    )
    .await;
    rejects(
        "codec.decode",
        json!({"encoding":"hex","text":"0xz","output":"bytes"}),
        "syntax:",
    )
    .await;
    rejects(
        "codec.encode",
        json!({"encoding":"hex","input":{"kind":"bytes","value":[256]}}),
        "arguments:",
    )
    .await;
    rejects(
        "codec.encode",
        json!({"encoding":"hex","input":{"kind":"bytes","value":[1.0]}}),
        "arguments:",
    )
    .await;
}

#[tokio::test]
async fn digests_match_published_vectors() {
    assert_eq!(
        call(
            "hash.digest",
            json!({"algorithm":"sha256","input":{"kind":"utf8","value":"abc"}})
        )
        .await,
        json!({"hex":"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"})
    );
    assert_eq!(
        call(
            "hash.digest",
            json!({"algorithm":"blake3","input":{"kind":"bytes","value":[]}})
        )
        .await,
        json!({"hex":"af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"})
    );
}

#[tokio::test]
async fn unified_diff_has_correct_ranges_context_and_newline_markers() {
    assert_eq!(
        call(
            "text.diff",
            json!({"before":"a\nb\nc\n","after":"a\n雪\nc\n","context":1})
        )
        .await,
        json!({"diff":"--- before\n+++ after\n@@ -1,3 +1,3 @@\n a\n-b\n+雪\n c\n"})
    );
    assert_eq!(
        call("text.diff", json!({"before":"","after":"x","context":0})).await,
        json!({"diff":"--- before\n+++ after\n@@ -0,0 +1,1 @@\n+x\n\\ No newline at end of file\n"})
    );
    assert_eq!(
        call("text.diff", json!({"before":"same\n","after":"same\n"})).await,
        json!({"diff":""})
    );
}

#[tokio::test]
async fn limits_reject_input_output_depth_nodes_rows_and_diff_work() {
    let cases = [
        (
            "data.parse",
            json!({"format":"json","text":"{}"}),
            Limits {
                input_bytes: 8,
                ..Limits::default()
            },
            "input_limit:",
        ),
        (
            "codec.encode",
            json!({"encoding":"hex","input":{"kind":"utf8","value":"12345"}}),
            Limits {
                output_bytes: 8,
                ..Limits::default()
            },
            "output_limit:",
        ),
        (
            "data.parse",
            json!({"format":"json","text":"[[[[[0]]]]]"}),
            Limits {
                max_depth: 4,
                ..Limits::default()
            },
            "depth_limit:",
        ),
        (
            "data.parse",
            json!({"format":"json","text":"[0,1,2,3,4,5]"}),
            Limits {
                max_nodes: 5,
                ..Limits::default()
            },
            "node_limit:",
        ),
        (
            "data.parse",
            json!({"format":"csv","text":"a\n1\n2\n3\n"}),
            Limits {
                max_rows: 2,
                ..Limits::default()
            },
            "row_limit:",
        ),
        (
            "text.diff",
            json!({"before":"a\nb\nc\n","after":"x\ny\nz\n"}),
            Limits {
                max_diff_work: 4,
                ..Limits::default()
            },
            "diff_work_limit:",
        ),
    ];
    for (name, args, limits, kind) in cases {
        let error = execute(name, args, CancellationToken::new(), limits)
            .await
            .unwrap_err();
        assert!(
            error.message.starts_with(kind),
            "wanted {kind}, got {error}"
        );
        assert!(!error.outcome_unknown);
    }
}

#[tokio::test]
async fn pre_cancelled_execution_is_a_known_failure() {
    let token = CancellationToken::new();
    token.cancel();
    let error = execute(
        "data.parse",
        json!({"format":"json","text":"{}"}),
        token,
        Limits::default(),
    )
    .await
    .unwrap_err();
    assert!(error.message.starts_with("cancelled:"));
    assert!(!error.outcome_unknown);
}
