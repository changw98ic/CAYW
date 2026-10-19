use async_trait::async_trait;
use cayw_ai::{CancellationToken, ToolDefinition};
use cayw_codemode::{CallStatus, CapabilityError, CapabilityHost, CodeMode};
use serde_json::{Value, json};
use std::sync::Arc;

// This is only a test composition of the existing host interface. Production
// embeds definitions/execute in its one permission/audit gate.
struct Host;

#[async_trait]
impl CapabilityHost for Host {
    fn capabilities(&self) -> Vec<ToolDefinition> {
        cayw_data::definitions()
    }
    async fn call(
        &self,
        name: &str,
        args: Value,
        cancel: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        cayw_data::execute(name, args, cancel, cayw_data::Limits::default()).await
    }
}

// Break caught: declaring data tools without a working route from real Luau calls.
#[tokio::test]
async fn real_luau_parses_selects_serializes_and_hashes_through_host_audit() {
    let mode = CodeMode::new(Arc::new(Host), cayw_codemode::Limits::default()).unwrap();
    let script = include_str!("../examples/data.luau");
    let report = mode
        .execute(script, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        report.return_value["csv"],
        json!("\"id\",\"note\"\n\"002\",\"雪\"\n\"001\",\"hello, world\"\n")
    );
    assert_eq!(
        report.return_value["digest"]["hex"],
        json!("48e10a5a877b0ef6232e6cc7367f7546c6fac14e8f0f07e276aee16adad8b1c0")
    );
    let names: Vec<_> = report.calls.iter().map(|call| call.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "data.parse",
            "data.query",
            "data.transform",
            "data.serialize",
            "hash.digest"
        ]
    );
    assert!(
        report
            .calls
            .iter()
            .all(|call| call.status == CallStatus::Succeeded)
    );
    assert!(!report.outcome_unknown);
}
