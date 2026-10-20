use async_trait::async_trait;
use cayw_verify_tool::sdk::{
    NegativeControl, Options, Result, Scenario, ScenarioContext, ScenarioReport, run_scenarios,
};
use serde_json::{Value, json};
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU32, Ordering},
};

struct LargeEvidence {
    executions: AtomicU32,
}
#[async_trait]
impl Scenario for LargeEvidence {
    fn name(&self) -> &str {
        "large-evidence"
    }
    fn specification(&self, _: NegativeControl) -> Value {
        json!({"inputs":{"payload_bytes":8 * 1024 * 1024},"permissions":{"grants":[]}})
    }
    async fn execute(&self, context: &ScenarioContext, report: &mut ScenarioReport) -> Result<()> {
        self.executions.fetch_add(1, Ordering::SeqCst);
        let actual = "x".repeat(8 * 1024 * 1024);
        report.observe("large_output", &actual)?;
        fs::write(context.root.join("effect.txt"), b"completed sample")?;
        report.task_check(
            "payload_observed",
            actual.len() == 8 * 1024 * 1024,
            "Actual output size",
        );
        Ok(())
    }
}

#[tokio::test]
async fn oversized_sample_index_stops_sampling_without_losing_raw_results() {
    let parent = temp();
    let out = parent.join("large");
    let scenario = LargeEvidence {
        executions: AtomicU32::new(0),
    };
    let result = run_scenarios(
        Options {
            out: out.clone(),
            samples: 20,
            build_receipt: None,
            scenarios: vec![],
            negative_control: NegativeControl::None,
        },
        &[&scenario],
    )
    .await;
    assert!(
        result.is_err(),
        "producer reported success for a bundle the reader refuses: {}",
        cayw_verify_tool::evidence::Bundle::read(&out)
            .err()
            .map(|error| error.to_string())
            .unwrap_or_default()
    );
    let completed = scenario.executions.load(Ordering::SeqCst);
    assert!(
        (2..20).contains(&completed),
        "sampling continued after the read boundary"
    );
    assert!(fs::metadata(out.join("samples.jsonl")).unwrap().len() <= 64 * 1024 * 1024);
    assert!(
        !out.join("summary.json").exists(),
        "incomplete bundle claimed a final result"
    );
    assert!(cayw_verify_tool::evidence::Bundle::read(&out).is_err());
    for iteration in 1..=completed {
        let raw = out.join(format!("samples/{iteration:04}/large-evidence"));
        assert_eq!(
            fs::read(raw.join("effect.txt")).unwrap(),
            b"completed sample"
        );
        let observations: Vec<Value> = fs::read_to_string(raw.join("records.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(observations.last().unwrap()["kind"], "scenario_terminal");
        let stored: Value =
            serde_json::from_slice(&fs::read(raw.join("report.json")).unwrap()).unwrap();
        assert_eq!(stored["status"], "passed");
    }
    fs::remove_dir_all(parent).unwrap();
}
struct Custom;
#[async_trait]
impl Scenario for Custom {
    fn name(&self) -> &str {
        "external-extension"
    }
    fn specification(&self, _: NegativeControl) -> Value {
        json!({"inputs":{"sentinel":"public trait"},"permissions":{"grants":[]}})
    }
    async fn execute(&self, context: &ScenarioContext, report: &mut ScenarioReport) -> Result<()> {
        let path = context.root.join("sentinel.txt");
        fs::write(&path, b"public trait")?;
        let actual = fs::read(&path)?;
        report.observe("external_extension_result", json!({"bytes":actual}))?;
        report.task_check(
            "custom_scenario_was_invoked",
            actual == b"public trait",
            "Custom implementation uses public runner and evidence API",
        );
        Ok(())
    }
}
fn temp() -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "cayw-custom-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&path).unwrap();
    path
}
#[tokio::test]
async fn external_scenario_uses_the_same_report_and_cannot_be_vacuously_passed() {
    let parent = temp();
    let out = parent.join("custom");
    assert!(
        run_scenarios(
            Options {
                out: out.clone(),
                samples: 1,
                build_receipt: None,
                scenarios: vec![],
                negative_control: NegativeControl::None
            },
            &[&Custom]
        )
        .await
        .unwrap()
    );
    let sample: Value = serde_json::from_slice(
        &fs::read(out.join("samples/0001/external-extension/report.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(sample["status"], "passed");
    assert_eq!(sample["scenario"], "external-extension");
    let bundle = cayw_verify_tool::evidence::Bundle::read(&out).unwrap();
    assert_eq!(bundle.samples.len(), 1);
    assert_eq!(bundle.summary["semantic_passed"], true);
    assert!(
        sample["observations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|record| record["kind"] == "external_extension_result")
    );
    let untouched = parent.join("invalid");
    assert!(
        run_scenarios(
            Options {
                out: untouched.clone(),
                samples: 1,
                build_receipt: None,
                scenarios: vec![],
                negative_control: NegativeControl::None
            },
            &[]
        )
        .await
        .is_err()
    );
    assert!(!untouched.exists());
    fs::remove_dir_all(parent).unwrap();
}
