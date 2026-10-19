use async_trait::async_trait;
use cayw_verify_tool::sdk::{
    NegativeControl, Options, Result, Scenario, ScenarioContext, ScenarioReport, run_scenarios,
};
use serde_json::{Value, json};
use std::{fs, path::PathBuf};
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
        report.check(
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
