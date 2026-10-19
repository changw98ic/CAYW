use async_trait::async_trait;
use cayw_ai::{CancellationToken, ToolDefinition};
use cayw_codemode::{CallStatus, CapabilityError, CapabilityHost, CodeMode, ErrorKind, Limits};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::Notify;

struct NeverSettles {
    started: Notify,
    calls: AtomicUsize,
}
#[async_trait]
impl CapabilityHost for NeverSettles {
    fn capabilities(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "block".into(),
            description: "an uncooperative host future".into(),
            parameters: json!({"type":"object"}),
        }]
    }
    async fn call(
        &self,
        _: &str,
        _: Value,
        _: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.started.notify_one();
        std::future::pending().await
    }
}
fn host() -> Arc<NeverSettles> {
    Arc::new(NeverSettles {
        started: Notify::new(),
        calls: AtomicUsize::new(0),
    })
}

// Break caught: waiting forever for a host after cancellation/time limit, or treating it as known failure.
#[tokio::test]
async fn timeout_drops_an_uncooperative_host_future_and_reports_unknown() {
    let fixture = host();
    let code = CodeMode::new(
        fixture.clone(),
        Limits {
            timeout: Duration::from_millis(30),
            ..Limits::default()
        },
    )
    .unwrap();
    let error = code
        .execute("cayw.call('block', {})", CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Timeout);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    assert_eq!(error.report.calls[0].status, CallStatus::OutcomeUnknown);
    assert!(error.report.outcome_unknown);
}

#[tokio::test]
async fn cancellation_does_not_require_a_cooperative_host() {
    let fixture = host();
    let code = CodeMode::new(fixture.clone(), Limits::default()).unwrap();
    let cancel = CancellationToken::new();
    let (execution, ()) = tokio::join!(
        code.execute("cayw.call('block', {})", cancel.clone()),
        async {
            fixture.started.notified().await;
            cancel.cancel();
        }
    );
    let error = execution.unwrap_err();
    assert_eq!(error.kind, ErrorKind::Cancelled);
    assert!(error.report.outcome_unknown);
}

#[tokio::test]
async fn an_abort_cannot_be_caught_with_pcall() {
    let code = CodeMode::new(
        host(),
        Limits {
            instruction_budget: 200,
            ..Limits::default()
        },
    )
    .unwrap();
    let error = code
        .execute(
            r#"
        local protected = pcall
        if protected then
            while true do protected(function() while true do end end) end
        else
            while true do end
        end
    "#,
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::InstructionLimit);
}

#[tokio::test]
async fn a_huge_return_value_is_part_of_the_output_budget() {
    let code = CodeMode::new(
        host(),
        Limits {
            output_bytes: 128,
            ..Limits::default()
        },
    )
    .unwrap();
    let error = code
        .execute("return string.rep('r', 1024)", CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::OutputLimit);
    assert!(error.report.return_value.is_null());
}

#[tokio::test]
async fn huge_arguments_are_refused_before_dispatch() {
    let fixture = host();
    let code = CodeMode::new(
        fixture.clone(),
        Limits {
            output_bytes: 128,
            ..Limits::default()
        },
    )
    .unwrap();
    let error = code
        .execute(
            "cayw.call('block', {data = string.rep('a', 1024)})",
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::OutputLimit);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    assert!(!error.report.outcome_unknown);
}

#[tokio::test]
async fn metatables_and_array_helpers_cannot_smuggle_sparse_or_object_keys() {
    for script in [
        "cayw.emit(cayw.array({[2] = 2}))",
        "cayw.emit(cayw.array({key = 'hidden'}))",
        "return {cayw.null, nil, 'hole'}",
        "cayw.emit({[false] = 'key'})",
        "return function() end",
        "return 1, 2",
    ] {
        let code = CodeMode::new(host(), Limits::default()).unwrap();
        let error = code
            .execute(script, CancellationToken::new())
            .await
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::Conversion, "{script}: {error}");
    }
}

#[tokio::test]
async fn a_deep_json_value_is_rejected_before_stack_exhaustion() {
    let code = CodeMode::new(host(), Limits::default()).unwrap();
    let error = code.execute("local root = {}; local t = root; for i=1,200 do t.child = {}; t = t.child end; cayw.emit(root)", CancellationToken::new()).await.unwrap_err();
    assert_eq!(error.kind, ErrorKind::Conversion);
}

#[test]
fn catalogs_reject_duplicate_names_and_external_schema_references() {
    struct BadCatalog(Vec<ToolDefinition>);
    #[async_trait]
    impl CapabilityHost for BadCatalog {
        fn capabilities(&self) -> Vec<ToolDefinition> {
            self.0.clone()
        }
        async fn call(
            &self,
            _: &str,
            _: Value,
            _: CancellationToken,
        ) -> Result<Value, CapabilityError> {
            unreachable!()
        }
    }
    let definition = |name: &str, parameters| ToolDefinition {
        name: name.into(),
        description: String::new(),
        parameters,
    };
    for definitions in [
        vec![
            definition("same", json!({"type":"object"})),
            definition("same", json!({"type":"object"})),
        ],
        vec![definition(
            "external",
            json!({"$ref":"https://example.com/forbidden"}),
        )],
        vec![definition("", json!({"type":"object"}))],
    ] {
        let error = CodeMode::new(Arc::new(BadCatalog(definitions)), Limits::default())
            .err()
            .expect("invalid catalog accepted");
        assert_eq!(error.kind, ErrorKind::InvalidCatalog);
    }
}
