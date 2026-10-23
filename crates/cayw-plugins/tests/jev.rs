#![cfg(feature = "jev")]
use cayw_plugins::{jev::*, *};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

struct Evaluator {
    invalid: bool,
}
#[async_trait]
impl JevEvaluator for Evaluator {
    fn evaluator_id(&self) -> &str {
        "offline-evaluator"
    }
    async fn evaluate(
        &self,
        _input: JevInput,
        _cancel: CancellationToken,
    ) -> Result<JevScores, PluginError> {
        Ok(JevScores {
            entailment: if self.invalid { f64::NAN } else { 0.8 },
            contradiction: 0.1,
            neutral: 0.1,
        })
    }
}
fn jev_input() -> JevInput {
    JevInput {
        premise: "all fixtures run offline".into(),
        hypothesis: "this fixture needs no model".into(),
    }
}
#[tokio::test]
async fn jev_requires_registered_evaluator_and_scores_remain_provisional() {
    let unavailable = Jev::new(None, Duration::from_secs(1));
    assert!(matches!(
        unavailable
            .evaluate(jev_input(), CancellationToken::new())
            .await,
        Err(PluginError::Unavailable(_))
    ));
    let evaluator = Jev::new(
        Some(Arc::new(Evaluator { invalid: false })),
        Duration::from_secs(1),
    );
    let report = evaluator
        .evaluate(jev_input(), CancellationToken::new())
        .await
        .unwrap();
    assert!(report.provisional);
    assert_eq!(report.scores.entailment, 0.8);
    assert_eq!(report.input, jev_input());
    let invalid = Jev::new(
        Some(Arc::new(Evaluator { invalid: true })),
        Duration::from_secs(1),
    );
    assert!(matches!(
        invalid
            .evaluate(jev_input(), CancellationToken::new())
            .await,
        Err(PluginError::Protocol(_))
    ));
}

struct PluginEvaluator {
    manifest: PluginManifest,
}
#[async_trait]
impl Plugin for PluginEvaluator {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }
    async fn call(
        &self,
        _method: &str,
        _input: Value,
        _cancel: CancellationToken,
    ) -> Result<Value, PluginError> {
        Ok(json!({"entailment":0.7,"contradiction":0.2,"neutral":0.1}))
    }
}
#[tokio::test]
async fn jev_registry_adapter_obeys_plugin_permissions_and_unload() {
    let registry = Arc::new(PluginRegistry::new());
    let manifest = PluginManifest {
        name: "local-jev".into(),
        version: "1".into(),
        protocol_version: PLUGIN_PROTOCOL_VERSION,
        kind: PluginKind::Native,
        capabilities: vec![ToolDefinition {
            name: "jev.score".into(),
            description: "offline scoring fixture".into(),
            parameters: json!({
                "type":"object","properties":{"premise":{"type":"string"},"hypothesis":{"type":"string"}},"required":["premise","hypothesis"],"additionalProperties":false,
            }),
        }],
        permissions: Default::default(),
    };
    let plugin = Arc::new(PluginEvaluator { manifest });
    registry
        .load_native(plugin.clone(), PluginContext::default())
        .unwrap();
    let evaluator = Jev::new(
        Some(Arc::new(PluginJevEvaluator {
            registry: registry.clone(),
            plugin_name: "local-jev".into(),
            method: "jev.score".into(),
            timeout: Duration::from_secs(1),
        })),
        Duration::from_secs(2),
    );
    assert!(matches!(
        evaluator
            .evaluate(jev_input(), CancellationToken::new())
            .await,
        Err(PluginError::Denied(_))
    ));
    registry.unload("local-jev").unwrap();
    registry
        .load_native(plugin, PluginContext::new(["jev.score".into()], []))
        .unwrap();
    assert_eq!(
        evaluator
            .evaluate(jev_input(), CancellationToken::new())
            .await
            .unwrap()
            .scores
            .entailment,
        0.7
    );
    registry.unload("local-jev").unwrap();
    assert!(matches!(
        evaluator
            .evaluate(jev_input(), CancellationToken::new())
            .await,
        Err(PluginError::NotFound(_))
    ));
}
