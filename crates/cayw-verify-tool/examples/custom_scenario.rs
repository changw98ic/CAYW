//! A downstream-style extension: only public SDK/Scenario APIs are used.
use async_trait::async_trait;
use cayw_runtime::{Runtime, RuntimeConfig, fixture::OfflineProvider};
use cayw_verify_tool::sdk::{
    NegativeControl, Options, Result, Scenario, ScenarioContext, ScenarioReport, run_scenarios,
};
use serde_json::{Value, json};
use std::{fs, path::PathBuf, sync::Arc};

struct CreateReopen;
#[async_trait]
impl Scenario for CreateReopen {
    fn name(&self) -> &str {
        "custom-create-reopen"
    }
    fn specification(&self, _: NegativeControl) -> Value {
        json!({"input":{"label":"Custom public SDK scenario","source":"offline synthetic"},
            "permissions":{"grants":[],"product_user_approval":false},"fault_points":[],
            "provider":"OfflineProvider injected, never called","operation":"create then reopen with no execution"})
    }
    async fn execute(&self, context: &ScenarioContext, report: &mut ScenarioReport) -> Result<()> {
        let workspace = context.root.join("workspace");
        fs::create_dir(&workspace)?;
        let path = context.root.join("session.sqlite");
        let open = || {
            Runtime::open(
                &path,
                &workspace,
                Arc::new(OfflineProvider::default()),
                cayw_ai::Model::new("offline-synthetic", "custom-scenario"),
                RuntimeConfig::default(),
            )
        };
        let runtime = open()?;
        let before = runtime.create(Some("Custom public SDK scenario".into()))?;
        report.observe("created_session", &before)?;
        drop(runtime);
        let reopened = open()?;
        let after = reopened.get(&before.id)?;
        report.observe("reopened_session", &after)?;
        report.check("custom_public_sdk_roundtrip",before==after&&after.messages.is_empty()&&after.attempt.is_none(),
            "A user-defined scenario can persist and reopen a real SDK session without modifying verifyTool");
        Ok(())
    }
}
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let out = PathBuf::from(args.next().ok_or("usage: custom_scenario NEW_DIRECTORY")?);
    if args.next().is_some() {
        return Err("usage: custom_scenario NEW_DIRECTORY".into());
    }
    let passed = run_scenarios(
        Options {
            out,
            samples: 1,
            scenarios: vec![],
            negative_control: NegativeControl::None,
        },
        &[&CreateReopen],
    )
    .await?;
    if !passed {
        return Err("custom scenario failed".into());
    }
    Ok(())
}
