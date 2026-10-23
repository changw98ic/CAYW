//! Waiting for a host reply preserves the VM's remaining calculation time.
use async_trait::async_trait;
use cayw_ai::{CancellationToken, ToolDefinition};
use cayw_codemode::{CallStatus, CapabilityError, CapabilityHost, CodeMode, ErrorKind, Limits};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::sync::Semaphore;

struct WaitingHost {
    entered: Semaphore,
    release: Semaphore,
}
impl WaitingHost {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            entered: Semaphore::new(0),
            release: Semaphore::new(0),
        })
    }
}
#[async_trait]
impl CapabilityHost for WaitingHost {
    fn capabilities(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "reply".into(),
            description: "A controlled host reply".into(),
            parameters: json!({"type":"object","additionalProperties":false}),
        }]
    }
    async fn call(
        &self,
        name: &str,
        arguments: Value,
        cancel: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        assert_eq!(name, "reply");
        assert_eq!(arguments, json!({}));
        self.entered.add_permits(1);
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(CapabilityError::new("reply cancelled before delivery")),
            permit = self.release.acquire() => {
                permit.unwrap().forget();
                Ok(json!({"answer":"accepted"}))
            }
        }
    }
}
fn limits() -> Limits {
    Limits {
        timeout: Duration::from_secs(1),
        instruction_budget: u64::MAX,
        ..Limits::default()
    }
}
async fn release_after_calculation_budget(host: &WaitingHost) {
    tokio::time::timeout(Duration::from_secs(10), host.entered.acquire())
        .await
        .expect("the actual Host call must start")
        .unwrap()
        .forget();
    // A whole second of margin makes this a time-budget contrast, not a
    // millisecond scheduling assertion. No timing upper bound is asserted.
    tokio::time::sleep(Duration::from_secs(2)).await;
    host.release.add_permits(1);
}

#[tokio::test]
async fn two_host_waits_do_not_spend_the_vm_calculation_budget() {
    let host = WaitingHost::new();
    let mode = CodeMode::new(host.clone(), limits()).unwrap();
    let execution = tokio::spawn(async move {
        mode.execute(
            "local a=cayw.call('reply',{}); local b=cayw.call('reply',{}); return {a=a,b=b}",
            CancellationToken::new(),
        )
        .await
    });
    release_after_calculation_budget(&host).await;
    release_after_calculation_budget(&host).await;
    let report = execution
        .await
        .unwrap()
        .expect("waiting must preserve calculation time");
    assert_eq!(
        report.return_value,
        json!({"a":{"answer":"accepted"},"b":{"answer":"accepted"}})
    );
    assert_eq!(report.calls.len(), 2);
    assert!(
        report
            .calls
            .iter()
            .all(|call| call.status == CallStatus::Succeeded)
    );
    assert!(!report.outcome_unknown);
}

#[tokio::test]
async fn calculation_clock_resumes_after_a_host_reply() {
    let host = WaitingHost::new();
    let mode = CodeMode::new(host.clone(), limits()).unwrap();
    let execution = tokio::spawn(async move {
        mode.execute(
            "cayw.call('reply',{}); while true do end",
            CancellationToken::new(),
        )
        .await
    });
    release_after_calculation_budget(&host).await;
    let error = tokio::time::timeout(Duration::from_secs(10), execution)
        .await
        .expect("calculation must still be bounded")
        .unwrap()
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Timeout);
    assert_eq!(error.report.calls.len(), 1);
    assert_eq!(error.report.calls[0].status, CallStatus::Succeeded);
    assert!(!error.report.outcome_unknown);
}
