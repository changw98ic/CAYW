#![cfg(feature = "fusion")]
use cayw_plugins::{fusion::*, *};
use serde_json::json;
use std::path::PathBuf;
use std::time::Duration;

fn manifest() -> PluginManifest {
    PluginManifest {
        name: "fusion".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        protocol_version: PLUGIN_PROTOCOL_VERSION,
        kind: PluginKind::External,
        capabilities: vec![ToolDefinition {
            name: "fusion.collect".into(),
            description: "Collect host-supplied candidate answers and list disagreements. Does not call a model or elect a winner.".into(),
            parameters: json!({"type": "object"}),
        }],
        permissions: Default::default(),
    }
}

fn sample_arguments() -> serde_json::Value {
    json!({
        "input": {
            "question": "offline answer",
            "canonical_context": {"b": 2, "a": 1},
            "constraints": ["do not use network"],
            "answer_schema": {
                "type": "object",
                "properties": {"n": {"type": "integer"}},
                "required": ["n"],
                "additionalProperties": false
            }
        },
        "candidates": [
            {"provider_id": "alpha", "response": {"status": "answer", "value": {"n": 1}, "usage": null}},
            {"provider_id": "beta", "response": {"status": "answer", "value": {"n": 2}, "usage": null}},
            {"provider_id": "gamma", "response": {"status": "failed", "message": "offline fixture failure", "usage": null}}
        ],
        "timeout_ms": 1000
    })
}

#[tokio::test]
async fn external_fusion_lists_disagreement_then_unload_is_not_loaded() {
    let registry = PluginRegistry::new();
    let description = registry
        .load_external(
            ExternalConfig::new(
                PathBuf::from(env!("CARGO_BIN_EXE_cayw-plugin-fusion")),
                Vec::new(),
                manifest(),
            ),
            PluginContext::new(["fusion.collect".into()], []),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let arguments = sample_arguments();
    let data = registry
        .call(
            "fusion",
            "fusion.collect",
            arguments.clone(),
            CancellationToken::new(),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    let report: FusionReport = serde_json::from_value(data).unwrap();
    let input: FusionInput = serde_json::from_value(arguments["input"].clone()).unwrap();
    assert_eq!(report.input, input);
    assert_eq!(report.input_digest, input.digest().unwrap());
    assert_eq!(
        report
            .candidates
            .iter()
            .map(|candidate| candidate.provider_id.as_str())
            .collect::<Vec<_>>(),
        ["alpha", "beta", "gamma"]
    );
    assert!(matches!(
        &report.candidates[0].outcome,
        CandidateOutcome::Answer { value, .. } if value == &json!({"n": 1})
    ));
    assert!(matches!(
        &report.candidates[1].outcome,
        CandidateOutcome::Answer { value, .. } if value == &json!({"n": 2})
    ));
    assert!(matches!(
        &report.candidates[2].outcome,
        CandidateOutcome::Failed { message, .. } if message == "offline fixture failure"
    ));
    assert_eq!(report.disagreements, vec![("alpha".into(), "beta".into())]);
    let encoded = serde_json::to_value(&report).unwrap();
    assert!(encoded.get("winner").is_none());
    assert!(encoded.get("confidence").is_none());
    assert!(encoded.get("vote").is_none());

    registry.unload("fusion").unwrap();
    let missing = registry
        .call(
            "fusion",
            "fusion.collect",
            arguments,
            CancellationToken::new(),
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
    assert_eq!(missing, PluginError::NotFound("fusion".into()));
    assert_eq!(description.manifest.name, "fusion");
}

#[tokio::test]
async fn external_fusion_missing_field_is_an_error() {
    let registry = PluginRegistry::new();
    registry
        .load_external(
            ExternalConfig::new(
                PathBuf::from(env!("CARGO_BIN_EXE_cayw-plugin-fusion")),
                Vec::new(),
                manifest(),
            ),
            PluginContext::new(["fusion.collect".into()], []),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let mut arguments = sample_arguments();
    arguments["input"]
        .as_object_mut()
        .unwrap()
        .remove("question");
    let missing = registry
        .call(
            "fusion",
            "fusion.collect",
            arguments,
            CancellationToken::new(),
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
    assert!(matches!(missing, PluginError::Failed(_)));
    assert!(!missing.outcome_unknown());
}
