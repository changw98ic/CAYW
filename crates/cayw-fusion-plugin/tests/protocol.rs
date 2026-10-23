//! Offline tests of the real `cayw-fusion-plugin` child process through the
//! host `PluginRegistry`, plus in-process `serve` checks. No network, model
//! provider or credential is used.

use cayw_ai::{CancellationToken, Usage};
use cayw_fusion_plugin::{COLLECT_METHOD, ProviderSpec, ServeOptions, manifest, serve};
use cayw_plugins::fusion::{CandidateOutcome, FusionInput, FusionReport, collect_fusion};
use cayw_plugins::{ExternalConfig, PluginContext, PluginError, PluginRegistry};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Generous; only failure paths wait this long.
const CALL_TIMEOUT: Duration = Duration::from_secs(60);

fn plugin_config(argv: &[&str]) -> ExternalConfig {
    let mut config = ExternalConfig::new(
        PathBuf::from(env!("CARGO_BIN_EXE_cayw-fusion-plugin")),
        argv.iter().map(|argument| (*argument).into()).collect(),
        manifest(),
    );
    config.call_timeout = CALL_TIMEOUT;
    config
}

async fn load(argv: &[&str]) -> PluginRegistry {
    let registry = PluginRegistry::new();
    registry
        .load_external(
            plugin_config(argv),
            PluginContext::new([COLLECT_METHOD.into()], []),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    registry
}

async fn call(registry: &PluginRegistry, arguments: Value) -> Result<Value, PluginError> {
    registry
        .call(
            "fusion",
            COLLECT_METHOD,
            arguments,
            CancellationToken::new(),
            CALL_TIMEOUT,
        )
        .await
}

fn input() -> Value {
    json!({
        "question": "offline fusion question",
        "canonical_context": {"b": [2, 1], "a": {"z": true, "y": null}},
        "constraints": ["no network"],
        "answer_schema": {
            "type": "object",
            "properties": {"n": {"type": "integer"}},
            "required": ["n"],
            "additionalProperties": false
        }
    })
}

fn answer(id: &str, n: Value) -> Value {
    json!({"provider_id": id, "kind": "offline", "behavior": {"mode": "answer", "value": {"n": n}}})
}

fn arguments(providers: Vec<Value>, timeout_ms: u64) -> Value {
    json!({"input": input(), "providers": providers, "per_provider_timeout_ms": timeout_ms})
}

#[tokio::test]
async fn external_report_matches_in_process_collect_fusion() {
    let usage = Usage {
        input: 4,
        output: 2,
        total_tokens: 6,
        ..Default::default()
    };
    let providers = vec![
        json!({"provider_id": "p0", "kind": "offline", "behavior": {"mode": "answer", "value": {"n": 1}, "usage": usage}}),
        answer("p1", json!(2)),
        json!({"provider_id": "p2", "kind": "offline", "behavior": {"mode": "fail", "message": "offline failure"}}),
        answer("p3", json!("not an integer")),
        answer("p4", json!(1)),
    ];
    let registry = load(&[]).await;
    let data = call(&registry, arguments(providers.clone(), 30_000))
        .await
        .unwrap();
    let report: FusionReport = serde_json::from_value(data).unwrap();

    // Same library, same offline sources, no process boundary.
    let sources = providers
        .into_iter()
        .map(|spec| {
            serde_json::from_value::<ProviderSpec>(spec)
                .unwrap()
                .build(&ServeOptions::default())
                .unwrap()
        })
        .collect();
    let expected_input: FusionInput = serde_json::from_value(input()).unwrap();
    let expected = collect_fusion(
        expected_input.clone(),
        sources,
        Duration::from_secs(30),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(report, expected);

    assert_eq!(report.input, expected_input);
    assert_eq!(report.input_digest, expected_input.digest().unwrap());
    assert!(
        report
            .candidates
            .iter()
            .all(|candidate| candidate.input_digest == report.input_digest)
    );
    let ids: Vec<_> = report
        .candidates
        .iter()
        .map(|candidate| candidate.provider_id.as_str())
        .collect();
    assert_eq!(ids, ["p0", "p1", "p2", "p3", "p4"]);
    assert!(
        matches!(&report.candidates[0].outcome, CandidateOutcome::Answer { value, usage: Some(usage) } if value == &json!({"n": 1}) && usage.total_tokens == 6)
    );
    assert!(
        matches!(&report.candidates[2].outcome, CandidateOutcome::Failed { message, .. } if message == "offline failure")
    );
    assert!(matches!(
        &report.candidates[3].outcome,
        CandidateOutcome::InvalidAnswer { .. }
    ));
    // Distinct valid answers are listed; the two equal answers are not a vote.
    assert_eq!(
        report.disagreements,
        vec![
            ("p0".to_string(), "p1".to_string()),
            ("p1".to_string(), "p4".to_string())
        ]
    );
}

#[tokio::test]
async fn provider_timeout_and_panic_become_candidates_and_process_keeps_serving() {
    let registry = load(&["--allow-fault-injection"]).await;
    let providers = vec![
        json!({"provider_id": "slow", "kind": "offline", "behavior": {"mode": "hang"}}),
        json!({"provider_id": "broken", "kind": "offline", "behavior": {"mode": "panic"}}),
        answer("ok", json!(3)),
    ];
    // The hanging provider is bounded by its own timeout, well below CALL_TIMEOUT.
    let data = call(&registry, arguments(providers, 500)).await.unwrap();
    let report: FusionReport = serde_json::from_value(data).unwrap();
    assert_eq!(report.candidates[0].outcome, CandidateOutcome::TimedOut);
    assert!(
        matches!(&report.candidates[1].outcome, CandidateOutcome::Failed { message, .. } if message.contains("panicked"))
    );
    assert!(matches!(
        &report.candidates[2].outcome,
        CandidateOutcome::Answer { .. }
    ));
    assert!(report.disagreements.is_empty());

    registry.check_available("fusion").unwrap();
    let again = call(
        &registry,
        arguments(
            vec![
                answer("a", json!(1)),
                answer("b", json!(1)),
                answer("c", json!(1)),
            ],
            30_000,
        ),
    )
    .await
    .unwrap();
    assert_eq!(again["disagreements"], json!([]));
}

#[tokio::test]
async fn invalid_calls_are_known_failures_and_process_keeps_serving() {
    let registry = load(&[]).await;
    let valid = || {
        vec![
            answer("a", json!(1)),
            answer("b", json!(2)),
            answer("c", json!(3)),
        ]
    };
    let mut invalid_schema = arguments(valid(), 1_000);
    invalid_schema["input"]["answer_schema"] = json!({"type": 5});
    let cases = [
        // Each passes the manifest schema; the plugin itself rejects it.
        arguments(
            vec![
                answer("a", json!(1)),
                answer("A", json!(1)),
                answer("c", json!(1)),
            ],
            1_000,
        ),
        arguments(
            vec![
                answer("a", json!(1)),
                answer("b", json!(1)),
                json!({"provider_id": "c", "kind": "remote_model"}),
            ],
            1_000,
        ),
        arguments(
            vec![
                answer("a", json!(1)),
                answer("b", json!(1)),
                json!({"provider_id": "c", "kind": "offline", "behavior": {"mode": "crash"}}),
            ],
            1_000,
        ),
        arguments(
            vec![
                answer("a", json!(1)),
                answer("b", json!(1)),
                json!({"provider_id": "c", "kind": "offline", "behavior": {"mode": "answer", "value": {}, "extra": 1}}),
            ],
            1_000,
        ),
        invalid_schema,
    ];
    for arguments in cases {
        let error = call(&registry, arguments).await.unwrap_err();
        assert!(matches!(error, PluginError::Failed(_)), "{error:?}");
        assert!(!error.outcome_unknown());
        registry.check_available("fusion").unwrap();
    }
    // Rejected by the host before reaching the process.
    let error = call(&registry, json!({"providers": []})).await.unwrap_err();
    assert!(matches!(error, PluginError::InvalidArguments(_)));
    let data = call(&registry, arguments(valid(), 30_000)).await.unwrap();
    assert_eq!(data["disagreements"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn oversized_report_is_a_known_error_not_a_dead_process() {
    // The host accepts 64 KiB lines; the plugin is configured to emit at most
    // 16 KiB, so a ~40 KiB report must be refused by the plugin itself.
    let registry = load(&["--max-line-bytes", "16384"]).await;
    let big = json!("y".repeat(20 * 1024));
    let providers = vec![
        json!({"provider_id": "a", "kind": "offline", "behavior": {"mode": "answer", "value": {"n": 1, "pad": big}}}),
        json!({"provider_id": "b", "kind": "offline", "behavior": {"mode": "answer", "value": {"n": 2, "pad": big}}}),
        answer("c", json!(3)),
    ];
    let mut request = arguments(providers, 30_000);
    // Allow the padding so the answers are valid and kept in the report.
    request["input"]["answer_schema"] = json!({"type": "object"});
    let error = call(&registry, request).await.unwrap_err();
    assert!(
        matches!(&error, PluginError::Failed(message) if message.contains("line limit")),
        "{error:?}"
    );
    registry.check_available("fusion").unwrap();
}

#[tokio::test]
async fn raw_protocol_answers_malformed_calls_and_keeps_serving() {
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_cayw-fusion-plugin"))
        .env_clear()
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut send = async |line: String| {
        stdin
            .write_all(format!("{line}\n").as_bytes())
            .await
            .unwrap();
        stdin.flush().await.unwrap();
    };
    send(json!({"type": "handshake", "protocol_version": 1}).to_string()).await;
    let handshake: Value =
        serde_json::from_str(&stdout.next_line().await.unwrap().unwrap()).unwrap();
    assert_eq!(handshake["type"], "handshake");
    assert_eq!(
        handshake["manifest"],
        serde_json::to_value(manifest()).unwrap()
    );

    send(
        json!({"type": "call", "request_id": 1, "method": "fusion.unknown", "arguments": {}})
            .to_string(),
    )
    .await;
    send(json!({"type": "call", "request_id": 2, "method": COLLECT_METHOD, "arguments": {"unexpected": true}}).to_string()).await;
    send(r#"{"type":"call","request_id":3,"method":7}"#.into()).await;
    send("not json at all".into()).await;
    send(
        json!({"type": "call", "request_id": 4, "method": COLLECT_METHOD, "arguments": arguments(
            vec![answer("a", json!(1)), answer("b", json!(1)), answer("c", json!(1))],
            30_000,
        )})
        .to_string(),
    )
    .await;
    for request_id in 1..=3 {
        let response: Value =
            serde_json::from_str(&stdout.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(response["request_id"], request_id);
        assert_eq!(response["result"]["status"], "error");
        assert_eq!(response["result"]["outcome_unknown"], false);
    }
    // The unparseable line has no request id and gets no response.
    let response: Value =
        serde_json::from_str(&stdout.next_line().await.unwrap().unwrap()).unwrap();
    assert_eq!(response["request_id"], 4);
    assert_eq!(response["result"]["status"], "ok");
    assert_eq!(response["result"]["data"]["disagreements"], json!([]));

    drop(stdin);
    let status = tokio::time::timeout(CALL_TIMEOUT, child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(status.success());
}

#[tokio::test]
async fn closing_input_cancels_an_in_flight_collection() {
    let (mut host, plugin) = tokio::io::duplex(64 * 1024);
    let (plugin_read, plugin_write) = tokio::io::split(plugin);
    let server = tokio::spawn(serve(plugin_read, plugin_write, ServeOptions::default()));
    host.write_all(b"{\"type\":\"handshake\",\"protocol_version\":1}\n")
        .await
        .unwrap();
    let request = json!({"type": "call", "request_id": 1, "method": COLLECT_METHOD, "arguments": arguments(
        vec![
            json!({"provider_id": "a", "kind": "offline", "behavior": {"mode": "hang"}}),
            json!({"provider_id": "b", "kind": "offline", "behavior": {"mode": "hang"}}),
            json!({"provider_id": "c", "kind": "offline", "behavior": {"mode": "hang"}}),
        ],
        600_000,
    )});
    host.write_all(format!("{request}\n").as_bytes())
        .await
        .unwrap();
    let mut reader = BufReader::new(host);
    let mut handshake = String::new();
    reader.read_line(&mut handshake).await.unwrap();
    assert!(handshake.contains("\"handshake\""));
    // Closing the host side is the only cancel signal in protocol v1.
    drop(reader);
    let result = tokio::time::timeout(CALL_TIMEOUT, server)
        .await
        .expect("serve must stop well before the provider timeout")
        .unwrap();
    assert_eq!(result, Ok(()));
}

#[tokio::test]
async fn handshake_with_wrong_version_is_rejected() {
    let (mut host, plugin) = tokio::io::duplex(4096);
    let (plugin_read, plugin_write) = tokio::io::split(plugin);
    let server = tokio::spawn(serve(plugin_read, plugin_write, ServeOptions::default()));
    host.write_all(b"{\"type\":\"handshake\",\"protocol_version\":2}\n")
        .await
        .unwrap();
    let result = tokio::time::timeout(CALL_TIMEOUT, server)
        .await
        .unwrap()
        .unwrap();
    assert!(result.is_err());
}
