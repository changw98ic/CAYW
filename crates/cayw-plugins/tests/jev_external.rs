#![cfg(feature = "jev")]

use cayw_plugins::jev::*;
use cayw_plugins::*;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

fn jev_binary() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_cayw-plugin-jev"))
}

fn config() -> ExternalConfig {
    ExternalConfig::new(jev_binary(), Vec::new(), jev_manifest())
}

fn jev_manifest() -> PluginManifest {
    PluginManifest {
        name: "jev".into(),
        version: "1".into(),
        protocol_version: PLUGIN_PROTOCOL_VERSION,
        kind: PluginKind::External,
        capabilities: vec![ToolDefinition {
            name: "jev.place".into(),
            description: "Split a session into parts and place each part into one memory".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "session": { "type": "string" }
                },
                "required": ["session"],
                "additionalProperties": false
            }),
        }, ToolDefinition {
            name: "jev.score".into(),
            description: "Score one premise and hypothesis with local Laya, or report that scoring is unavailable".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "premise": { "type": "string" },
                    "hypothesis": { "type": "string" }
                },
                "required": ["premise", "hypothesis"],
                "additionalProperties": false
            }),
        }],
        permissions: Default::default(),
    }
}

fn sample() -> Value {
    json!({
        "premise": "all fixtures run offline",
        "hypothesis": "this fixture needs no model"
    })
}

struct Ordinary {
    manifest: PluginManifest,
}

#[async_trait]
impl Plugin for Ordinary {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }
    async fn call(
        &self,
        _method: &str,
        arguments: Value,
        _cancel: CancellationToken,
    ) -> Result<Value, PluginError> {
        Ok(arguments)
    }
}

fn ordinary_manifest() -> PluginManifest {
    PluginManifest {
        name: "ordinary-chat".into(),
        version: "1".into(),
        protocol_version: PLUGIN_PROTOCOL_VERSION,
        kind: PluginKind::Native,
        capabilities: vec![ToolDefinition {
            name: "chat.echo".into(),
            description: "Ordinary registry call that does not depend on JEv".into(),
            parameters: json!({ "type": "object" }),
        }],
        permissions: Default::default(),
    }
}

#[tokio::test]
async fn external_jev_refuses_to_score_and_unload_keeps_ordinary_calls() {
    let registry = Arc::new(PluginRegistry::new());
    let mut process_config = config();
    process_config.environment.insert(
        "CAYW_JEV_PYTHON".into(),
        process_config
            .executable
            .join("unavailable-python")
            .to_str()
            .unwrap()
            .into(),
    );
    registry
        .load_external(
            process_config,
            PluginContext::new(["jev.score".into(), "jev.place".into()], []),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let loaded = registry
        .call(
            "jev",
            "jev.score",
            sample(),
            CancellationToken::new(),
            Duration::from_secs(2),
        )
        .await;
    match loaded {
        Err(PluginError::Failed(message)) => {
            assert!(message.contains("unavailable"), "{message}");
            assert!(!message.contains("entailment"), "{message}");
        }
        other => panic!("expected a clear unavailable result, got {other:?}"),
    }
    let empty = registry
        .call(
            "jev",
            "jev.score",
            json!({ "premise": " ", "hypothesis": "still present" }),
            CancellationToken::new(),
            Duration::from_secs(2),
        )
        .await;
    assert!(
        matches!(empty, Err(PluginError::Failed(ref message)) if message.contains("required")),
        "{empty:?}"
    );

    registry.unload("jev").unwrap();
    registry
        .load_native(
            Arc::new(Ordinary {
                manifest: ordinary_manifest(),
            }),
            PluginContext::new(["chat.echo".into()], []),
        )
        .unwrap();
    let ordinary = registry
        .call(
            "ordinary-chat",
            "chat.echo",
            json!({ "text": "still here" }),
            CancellationToken::new(),
            Duration::from_secs(1),
        )
        .await
        .unwrap();
    assert_eq!(ordinary["text"], "still here");
    let missing = registry
        .call(
            "jev",
            "jev.score",
            sample(),
            CancellationToken::new(),
            Duration::from_secs(1),
        )
        .await;
    assert!(
        matches!(missing, Err(PluginError::NotFound(_))),
        "{missing:?}"
    );

    let adapter = Jev::new(
        Some(Arc::new(PluginJevEvaluator {
            registry: registry.clone(),
            plugin_name: "jev".into(),
            method: "jev.score".into(),
            timeout: Duration::from_secs(1),
        })),
        Duration::from_secs(1),
    );
    let report = adapter
        .evaluate(
            JevInput {
                premise: "all fixtures run offline".into(),
                hypothesis: "this fixture needs no model".into(),
            },
            CancellationToken::new(),
        )
        .await;
    assert!(
        matches!(report, Err(PluginError::NotFound(_))),
        "{report:?}"
    );
}

#[tokio::test]
async fn crashed_jev_does_not_block_an_ordinary_call() {
    let registry = PluginRegistry::new();
    let mut process_config = config();
    // Only this test's child may be killed while other JEv tests run in parallel.
    let marker = format!("crash-fixture-{}", uuid::Uuid::new_v4());
    process_config.argv.push(marker.clone());
    let command = format!("{} {marker}", process_config.executable.display());
    registry
        .load_external(
            process_config,
            PluginContext::new(["jev.score".into(), "jev.place".into()], []),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    registry
        .load_native(
            Arc::new(Ordinary {
                manifest: ordinary_manifest(),
            }),
            PluginContext::new(["chat.echo".into()], []),
        )
        .unwrap();
    let listed = std::process::Command::new("ps")
        .args(["-axo", "pid=,command="])
        .output()
        .expect("list processes");
    let listing = String::from_utf8(listed.stdout).expect("process list");
    let child = listing.lines().find_map(|line| {
        let (pid, candidate) = line.trim().split_once(' ')?;
        (candidate == command).then_some(pid.to_string())
    });
    let Some(pid) = child else {
        panic!("spawned JEv process was not listed");
    };
    let status = std::process::Command::new("kill")
        .args(["-KILL", &pid])
        .status()
        .expect("signal the spawned JEv process");
    assert!(status.success(), "could not crash the spawned JEv process");
    let started = std::time::Instant::now();
    let ordinary = registry
        .call(
            "ordinary-chat",
            "chat.echo",
            json!({ "text": "after crash" }),
            CancellationToken::new(),
            Duration::from_secs(1),
        )
        .await
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(ordinary["text"], "after crash");
    registry.unload("jev").unwrap();
    let missing = registry
        .call(
            "jev",
            "jev.score",
            sample(),
            CancellationToken::new(),
            Duration::from_secs(1),
        )
        .await;
    assert!(
        matches!(missing, Err(PluginError::NotFound(_))),
        "{missing:?}"
    );
}

#[tokio::test]
async fn session_parts_are_placed_only_when_systemone_is_configured() {
    let registry = Arc::new(PluginRegistry::new());
    registry
        .load_external(
            config(),
            PluginContext::new(["jev.place".into()], []),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let missing = registry
        .call(
            "jev",
            "jev.place",
            json!({ "session": "user: remember this project uses Rust" }),
            CancellationToken::new(),
            Duration::from_secs(2),
        )
        .await;
    assert!(
        matches!(missing, Err(PluginError::Failed(ref message)) if message.contains("unavailable")),
        "{missing:?}"
    );
    let empty = registry
        .call(
            "jev",
            "jev.place",
            json!({ "session": " \n " }),
            CancellationToken::new(),
            Duration::from_secs(2),
        )
        .await;
    assert!(
        matches!(empty, Err(PluginError::Failed(ref message)) if message.contains("required")),
        "{empty:?}"
    );
    registry.unload("jev").unwrap();
}

#[test]
fn session_text_splits_and_a_choice_must_match_its_probabilities() {
    let parts = split_session("user: keep answers short\n\nassistant: the build failed");
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0].role, "user");
    assert_eq!(parts[1].role, "assistant");
    let batches = batch_part_indexes(&parts, 4000);
    assert_eq!(batches, vec![vec![0, 1]]);
    let mut raw = std::collections::BTreeMap::new();
    for place in MEMORY_PLACES {
        raw.insert(
            place.to_string(),
            if place == "preference" { 1.0 } else { 0.0 },
        );
    }
    let placed = normalize_placement(&parts[0], "preference", raw.clone()).unwrap();
    assert_eq!(placed.place, "preference");
    assert!(normalize_placement(&parts[0], "project", raw).is_err());
}

#[test]
fn three_noul_answers_normalize_and_invalid_answers_are_refused() {
    let scores = normalize_noul([0.2, 0.3, 0.5]).unwrap();
    assert_eq!(
        scores,
        JevScores {
            entailment: 0.2,
            contradiction: 0.3,
            neutral: 0.5,
        }
    );
    let leftover = normalize_noul([0.2, 0.2, 0.2]).unwrap();
    let sum = leftover.entailment + leftover.contradiction + leftover.neutral;
    assert!((sum - 1.0).abs() <= 1e-6, "{sum}");
    assert!((leftover.neutral - (1.0 / 3.0)).abs() <= 1e-12);
    for raw in [
        [0.0, 0.0, 0.0],
        [1.2, 0.1, 0.1],
        [f64::NAN, 0.5, 0.5],
        [f64::INFINITY, 0.0, 0.0],
        [-0.1, 0.6, 0.5],
    ] {
        assert!(
            matches!(normalize_noul(raw), Err(PluginError::Protocol(_))),
            "{raw:?}"
        );
    }
}
