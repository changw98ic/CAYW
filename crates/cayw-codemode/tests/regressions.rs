use async_trait::async_trait;
use cayw_ai::{CancellationToken, ToolDefinition};
use cayw_codemode::{
    CallStatus, CapabilityError, CapabilityHost, CodeMode, CodeModeError, ErrorKind, Limits,
};
use serde_json::{Value, json};
use std::{
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

#[derive(Default)]
struct EffectHost {
    effects: AtomicUsize,
}

#[async_trait]
impl CapabilityHost for EffectHost {
    fn capabilities(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "effect".into(),
            description: "Count actual dispatches".into(),
            parameters: json!({"type":"object"}),
        }]
    }

    async fn call(
        &self,
        _: &str,
        _: Value,
        _: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        self.effects.fetch_add(1, Ordering::SeqCst);
        Ok(json!(true))
    }
}

// Break caught: conversion/validation consumes the deadline, then a ready host
// future is polled before either the timer or cancellation can stop dispatch.
#[tokio::test]
async fn argument_preparation_cannot_dispatch_after_the_deadline() {
    let host = Arc::new(EffectHost::default());
    let code = CodeMode::new(
        host.clone(),
        Limits {
            timeout: Duration::from_millis(20),
            memory_bytes: 64 * 1024 * 1024,
            output_bytes: 16 * 1024 * 1024,
            ..Limits::default()
        },
    )
    .unwrap();
    let error = code
        .execute(
            "cayw.call('effect', {data=table.create(1000000, true)})",
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Timeout);
    assert_eq!(host.effects.load(Ordering::SeqCst), 0);
    assert!(error.report.calls.is_empty());
    assert!(!error.report.outcome_unknown);
}

#[tokio::test]
async fn cancellation_during_argument_preparation_starts_no_effect() {
    let host = Arc::new(EffectHost::default());
    let code = CodeMode::new(
        host.clone(),
        Limits {
            memory_bytes: 64 * 1024 * 1024,
            output_bytes: 16 * 1024 * 1024,
            ..Limits::default()
        },
    )
    .unwrap();
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    // A separate OS thread can cancel while the runtime thread is converting
    // arguments; this deliberately does not rely on a Tokio yield in the VM.
    let canceller = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(20));
        trigger.cancel();
    });
    let result = code
        .execute(
            "cayw.call('effect', {data=table.create(1000000, true)})",
            cancel,
        )
        .await;
    canceller.join().unwrap();
    let error = result.unwrap_err();
    assert_eq!(error.kind, ErrorKind::Cancelled);
    assert_eq!(host.effects.load(Ordering::SeqCst), 0);
    assert!(error.report.calls.is_empty());
    assert!(!error.report.outcome_unknown);
}

struct DeepHost {
    shape: String,
    cancel: CancellationToken,
}

#[async_trait]
impl CapabilityHost for DeepHost {
    fn capabilities(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "deep".into(),
            description: "Return a host-created value without parsing JSON".into(),
            parameters: json!({"type":"object"}),
        }]
    }

    async fn call(
        &self,
        _: &str,
        _: Value,
        _: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        let mut value = Value::Null;
        let depth = match self.shape.as_str() {
            "boundary" => 64,
            "one_over" => 65,
            _ => 10_000,
        };
        for _ in 0..depth {
            value = if self.shape == "objects" {
                let mut object = serde_json::Map::new();
                object.insert("child".into(), value);
                Value::Object(object)
            } else {
                Value::Array(vec![value])
            };
        }
        if self.shape == "invalid_sibling" {
            value = Value::Array(vec![json!(9_007_199_254_740_993_u64), value]);
        } else if self.shape == "object_sibling" {
            let mut object = serde_json::Map::new();
            object.insert("a_invalid".into(), json!(9_007_199_254_740_993_u64));
            object.insert("z_deep".into(), value);
            value = Value::Object(object);
        }
        if self.shape == "cancelled" {
            self.cancel.cancel();
        }
        Ok(value)
    }
}

// Break caught: Serialize, Clone or Drop reaches an unchecked deep Value and
// aborts the process. The child uses only the public CodeMode/CapabilityHost API.
#[test]
fn unchecked_deep_host_values_do_not_abort_the_process() {
    const CHILD_CASE: &str = "CAYW_CODEMODE_DEEP_VALUE_CASE";
    if let Ok(case) = std::env::var(CHILD_CASE) {
        let cancel = CancellationToken::new();
        let code = CodeMode::new(
            Arc::new(DeepHost {
                shape: case.clone(),
                cancel: cancel.clone(),
            }),
            Limits {
                output_bytes: if case == "tiny_budget" {
                    3
                } else {
                    1024 * 1024
                },
                ..Limits::default()
            },
        )
        .unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let result = runtime.block_on(code.execute("return cayw.call('deep', {})", cancel));
        if case == "boundary" {
            let report = result.unwrap();
            assert_eq!(report.calls[0].status, CallStatus::Succeeded);
            assert!(!report.calls[0].result_omitted);
            let mut leaf = &report.return_value;
            for _ in 0..64 {
                let values = leaf.as_array().expect("nested array is preserved");
                assert_eq!(values.len(), 1);
                leaf = &values[0];
            }
            assert!(leaf.is_null());
            serde_json::to_vec(&report.clone()).unwrap();
            return;
        }
        let error = result.unwrap_err();
        assert_eq!(
            error.kind,
            if case == "tiny_budget" {
                ErrorKind::OutputLimit
            } else {
                ErrorKind::Conversion
            }
        );
        assert_eq!(error.report.calls.len(), 1);
        assert_eq!(error.report.calls[0].status, CallStatus::Succeeded);
        assert!(error.report.calls[0].result_omitted);
        assert!(error.report.calls[0].result.is_none());
        assert!(!error.report.outcome_unknown);
        // Exercise the returned audit's own Clone, Serialize and Drop as well.
        let audit = error.clone();
        serde_json::to_vec(&audit.report).unwrap();
        return;
    }
    for case in [
        "arrays",
        "objects",
        "invalid_sibling",
        "object_sibling",
        "tiny_budget",
        "cancelled",
        "boundary",
        "one_over",
    ] {
        let child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "unchecked_deep_host_values_do_not_abort_the_process",
                "--nocapture",
            ])
            .env(CHILD_CASE, case)
            .output()
            .unwrap();
        assert!(
            child.status.success(),
            "{case}: child exited with {}; stdout: {}; stderr: {}",
            child.status,
            String::from_utf8_lossy(&child.stdout),
            String::from_utf8_lossy(&child.stderr),
        );
    }
}

fn assert_error_budget(error: &CodeModeError, limit: usize) {
    assert!(error.message.len() <= 2048);
    // An empty diagnostic denotes an omitted message when no payload budget
    // remains. Audit metadata still conveys kind, status and unknown effects.
    let mut retained = if error.message.is_empty() {
        0
    } else {
        serde_json::to_vec(&error.message).unwrap().len()
    };
    for call in &error.report.calls {
        for value in [call.arguments.as_ref(), call.result.as_ref()]
            .into_iter()
            .flatten()
        {
            retained += serde_json::to_vec(value).unwrap().len();
        }
        if let Some(message) = &call.error {
            assert!(message.len() <= 2048);
            retained += serde_json::to_vec(message).unwrap().len();
        }
    }
    for output in &error.report.outputs {
        retained += serde_json::to_vec(output).unwrap().len();
    }
    assert!(retained <= error.report.output_bytes);
    assert!(error.report.output_bytes <= limit);
}

// Break caught: an external Fault bypasses both truncation and the shared
// payload accounting when it is downcast from an mlua error.
#[tokio::test]
async fn unknown_names_have_bounded_accounted_errors_for_describe_and_call() {
    for limit in [64, 8192] {
        for (script, kind) in [
            (
                "cayw.call('effect', {}); cayw.describe(string.rep('x', 100000))",
                ErrorKind::UnknownCapability,
            ),
            (
                "cayw.call('effect', {}); cayw.call(string.rep('x', 100000), {})",
                ErrorKind::UnknownCapability,
            ),
            (
                "cayw.call('effect', {}); cayw.describe(string.rep('雪', 100000))",
                ErrorKind::UnknownCapability,
            ),
            (
                "cayw.call('effect', {}); cayw.call(string.rep('雪', 100000), {})",
                ErrorKind::UnknownCapability,
            ),
            (
                "cayw.call('effect', {}); error(string.rep('雪', 100000))",
                ErrorKind::Script,
            ),
        ] {
            let host = Arc::new(EffectHost::default());
            let code = CodeMode::new(
                host.clone(),
                Limits {
                    output_bytes: limit,
                    ..Limits::default()
                },
            )
            .unwrap();
            let error = code
                .execute(script, CancellationToken::new())
                .await
                .unwrap_err();
            assert_eq!(error.kind, kind);
            assert_error_budget(&error, limit);
            assert_eq!(host.effects.load(Ordering::SeqCst), 1);
            assert_eq!(error.report.calls.len(), 1);
            assert_eq!(error.report.calls[0].status, CallStatus::Succeeded);
            assert_eq!(error.report.calls[0].result, Some(json!(true)));
            assert!(!error.report.outcome_unknown);
        }
    }
}

struct ErrorHost;

#[async_trait]
impl CapabilityHost for ErrorHost {
    fn capabilities(&self) -> Vec<ToolDefinition> {
        ["fail", "unknown", "hang"]
            .into_iter()
            .map(|name| ToolDefinition {
                name: name.into(),
                description: String::new(),
                parameters: json!({"type":"object"}),
            })
            .collect()
    }
    async fn call(
        &self,
        name: &str,
        _: Value,
        _: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        match name {
            "fail" => Err(CapabilityError::new("失\n".repeat(100_000))),
            "unknown" => Err(CapabilityError::unknown("未\n".repeat(100_000))),
            "hang" => std::future::pending().await,
            _ => unreachable!(),
        }
    }
}

#[tokio::test]
async fn every_host_error_is_bounded_without_losing_settled_status_or_unknown() {
    for limit in [1, 2, 64, 1024 * 1024] {
        for (name, expected_status, unknown) in [
            ("fail", CallStatus::Failed, false),
            ("unknown", CallStatus::OutcomeUnknown, true),
            ("hang", CallStatus::OutcomeUnknown, true),
        ] {
            let code = CodeMode::new(
                Arc::new(ErrorHost),
                Limits {
                    output_bytes: limit,
                    timeout: Duration::from_millis(30),
                    ..Limits::default()
                },
            )
            .unwrap();
            let error = code
                .execute(
                    &format!("cayw.call('{name}', {{}})"),
                    CancellationToken::new(),
                )
                .await
                .unwrap_err();
            assert_error_budget(&error, limit);
            if limit == 1 {
                assert!(error.report.calls.is_empty());
                assert!(!error.report.outcome_unknown);
            } else {
                assert_eq!(error.report.calls.len(), 1);
                assert_eq!(error.report.calls[0].status, expected_status);
                assert_eq!(error.report.outcome_unknown, unknown);
            }
        }
    }
}

#[tokio::test]
async fn an_error_after_the_last_payload_byte_keeps_audit_and_omits_its_message() {
    let code = CodeMode::new(
        Arc::new(EffectHost::default()),
        Limits {
            output_bytes: 6, // {} arguments and the true host result.
            ..Limits::default()
        },
    )
    .unwrap();
    let error = code
        .execute(
            "cayw.call('effect', {}); error('later')",
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Script);
    assert_eq!(error.report.calls[0].status, CallStatus::Succeeded);
    assert_eq!(error.report.calls[0].result, Some(json!(true)));
    assert!(error.message.is_empty());
    assert_error_budget(&error, 6);
}
