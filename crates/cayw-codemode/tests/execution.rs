use async_trait::async_trait;
use cayw_ai::{CancellationToken, ToolDefinition};
use cayw_codemode::{CallStatus, CapabilityError, CapabilityHost, CodeMode, ErrorKind, Limits};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
use tokio::sync::Notify;

#[derive(Default)]
struct Host {
    effects: AtomicUsize,
    started: Notify,
    cancelled: Notify,
}

#[async_trait]
impl CapabilityHost for Host {
    fn capabilities(&self) -> Vec<ToolDefinition> {
        ["echo", "add", "effect", "fail", "unknown", "wait", "large", "large_integer"]
            .into_iter()
            .map(|name| ToolDefinition {
                name: name.into(),
                description: format!("Offline fixture {name}"),
                parameters: if name == "add" {
                    json!({"type":"object","properties":{"a":{"type":"integer"},"b":{"type":"integer"}},"required":["a","b"],"additionalProperties":false})
                } else {
                    json!({"type":"object"})
                },
            })
            .collect()
    }

    async fn call(
        &self,
        name: &str,
        arguments: Value,
        cancel: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        match name {
            "echo" => Ok(arguments),
            "add" => {
                Ok(json!({"sum":arguments["a"].as_i64().unwrap()+arguments["b"].as_i64().unwrap()}))
            }
            "effect" => {
                self.effects.fetch_add(1, Ordering::SeqCst);
                Ok(json!({"written":true}))
            }
            "fail" => Err(CapabilityError::new("denied by host")),
            "unknown" => {
                self.effects.fetch_add(1, Ordering::SeqCst);
                Err(CapabilityError::unknown(
                    "remote write acknowledgement lost",
                ))
            }
            "wait" => {
                self.effects.fetch_add(1, Ordering::SeqCst);
                self.started.notify_one();
                cancel.cancelled().await;
                self.cancelled.notify_one();
                Err(CapabilityError::unknown("interrupted write"))
            }
            "large" => {
                self.effects.fetch_add(1, Ordering::SeqCst);
                Ok(json!({"data":"x".repeat(8192)}))
            }
            "large_integer" => Ok(json!({"value":9_007_199_254_740_993_u64})),
            _ => panic!("unregistered capability was dispatched"),
        }
    }
}

fn mode(host: Arc<Host>, limits: Limits) -> CodeMode {
    CodeMode::new(host, limits).unwrap()
}

// Break caught: bypassing Luau execution or losing the order of host calls and JSON values.
#[tokio::test]
async fn real_luau_runs_multiple_calls_loops_and_json_round_trip() {
    let code = mode(Arc::new(Host::default()), Limits::default());
    let report = code
        .execute(
            r#"
        local sum: number = 0
        for i = 1, 3 do
            sum += cayw.call("add", {a = i, b = 2}).sum
        end
        local data = cayw.call("echo", {
            sum = sum, flag = false, text = "中文", items = {1, cayw.null, 3},
            empty = {}, array = cayw.array(), nested = {ok = true}
        })
        cayw.emit(data)
        return sum
    "#,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(report.return_value, json!(12));
    assert_eq!(
        report.outputs,
        vec![json!({
            "sum":12,"flag":false,"text":"中文","items":[1,null,3],
            "empty":{},"array":[],"nested":{"ok":true}
        })]
    );
    assert_eq!(
        report
            .calls
            .iter()
            .map(|call| call.name.as_str())
            .collect::<Vec<_>>(),
        vec!["add", "add", "add", "echo"]
    );
    assert_eq!(
        report
            .calls
            .iter()
            .map(|call| call.sequence)
            .collect::<Vec<_>>(),
        vec![1, 2, 3, 4]
    );
    assert!(
        report
            .calls
            .iter()
            .all(|call| call.status == CallStatus::Succeeded)
    );
    assert!(!report.outcome_unknown);
}

#[tokio::test]
async fn discovery_is_bound_to_the_host_catalog() {
    let code = mode(Arc::new(Host::default()), Limits::default());
    let report = code
        .execute(
            r#"
        local names = cayw.discover()
        local add = cayw.describe("add")
        return {count = #names, name = add.name, required = add.parameters.required}
    "#,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        report.return_value,
        json!({"count":8,"name":"add","required":["a","b"]})
    );
    assert!(report.calls.is_empty());
}

#[tokio::test]
async fn syntax_error_is_reported_without_host_calls() {
    let error = mode(Arc::new(Host::default()), Limits::default())
        .execute("local = ???", CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Script);
    assert!(error.report.calls.is_empty());
}

#[tokio::test]
async fn vm_interrupt_stops_an_infinite_cpu_loop() {
    let limits = Limits {
        instruction_budget: 100,
        timeout: Duration::from_secs(2),
        ..Limits::default()
    };
    let code = mode(Arc::new(Host::default()), limits);
    let error = code
        .execute("while true do end", CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::InstructionLimit);
    assert!(error.report.instruction_checkpoints > 0);
}

#[tokio::test]
async fn wall_time_interrupts_cpu_work_even_without_an_async_yield() {
    let limits = Limits {
        instruction_budget: u64::MAX,
        timeout: Duration::from_millis(20),
        ..Limits::default()
    };
    let error = mode(Arc::new(Host::default()), limits)
        .execute("while true do end", CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Timeout);
}

#[tokio::test]
async fn allocation_is_limited_by_the_vm_allocator() {
    let limits = Limits {
        memory_bytes: 512 * 1024,
        ..Limits::default()
    };
    let error = mode(Arc::new(Host::default()), limits)
        .execute(
            "local data = string.rep('x', 8 * 1024 * 1024); return #data",
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::MemoryLimit, "{error}");
}

#[tokio::test]
async fn cancellation_during_async_host_keeps_unknown_effects() {
    let host = Arc::new(Host::default());
    let code = mode(host.clone(), Limits::default());
    let cancel = CancellationToken::new();
    let execution = code.execute("cayw.call('wait', {})", cancel.clone());
    let (result, ()) = tokio::join!(execution, async {
        host.started.notified().await;
        cancel.cancel();
    });
    let error = result.unwrap_err();
    assert_eq!(error.kind, ErrorKind::Cancelled);
    assert!(error.report.outcome_unknown);
    assert_eq!(error.report.calls[0].status, CallStatus::OutcomeUnknown);
    assert_eq!(host.effects.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_pre_cancelled_script_cannot_dispatch_a_capability() {
    let host = Arc::new(Host::default());
    let cancel = CancellationToken::new();
    cancel.cancel();
    let error = mode(host.clone(), Limits::default())
        .execute("cayw.call('effect', {})", cancel)
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Cancelled);
    assert!(error.report.calls.is_empty());
    assert_eq!(host.effects.load(Ordering::SeqCst), 0);
    assert!(!error.report.outcome_unknown);
}

#[tokio::test]
async fn host_call_budget_prevents_the_next_effect() {
    let host = Arc::new(Host::default());
    let limits = Limits {
        host_calls: 1,
        ..Limits::default()
    };
    let error = mode(host.clone(), limits)
        .execute(
            "cayw.call('effect', {}); cayw.call('effect', {})",
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::HostCallLimit);
    assert_eq!(host.effects.load(Ordering::SeqCst), 1);
    assert_eq!(error.report.calls.len(), 1);
    assert_eq!(error.report.calls[0].status, CallStatus::Succeeded);
}

#[tokio::test]
async fn output_limit_keeps_prior_success_and_emitted_values() {
    let host = Arc::new(Host::default());
    let limits = Limits {
        output_bytes: 128,
        ..Limits::default()
    };
    let error = mode(host.clone(), limits)
        .execute(
            "cayw.call('effect', {}); cayw.emit('saved'); cayw.emit(string.rep('x', 512))",
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::OutputLimit);
    assert_eq!(error.report.outputs, vec![json!("saved")]);
    assert_eq!(error.report.calls[0].status, CallStatus::Succeeded);
    assert_eq!(error.report.calls[0].result, Some(json!({"written":true})));
    assert_eq!(host.effects.load(Ordering::SeqCst), 1);
    assert!(!error.report.outcome_unknown);
}

#[tokio::test]
async fn large_host_result_does_not_erase_a_completed_effect() {
    let limits = Limits {
        output_bytes: 256,
        ..Limits::default()
    };
    let error = mode(Arc::new(Host::default()), limits)
        .execute("cayw.call('large', {})", CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::OutputLimit);
    assert_eq!(error.report.calls[0].status, CallStatus::Succeeded);
    assert!(error.report.calls[0].result.is_none());
    assert!(error.report.calls[0].result_omitted);
    assert!(!error.report.outcome_unknown);
}

#[tokio::test]
async fn unknown_effect_stops_further_calls_and_retains_audit() {
    let host = Arc::new(Host::default());
    let error = mode(host.clone(), Limits::default())
        .execute(
            "cayw.call('unknown', {}); cayw.call('effect', {})",
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::OutcomeUnknown);
    assert!(error.report.outcome_unknown);
    assert_eq!(error.report.calls[0].status, CallStatus::OutcomeUnknown);
    assert_eq!(host.effects.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn known_host_error_is_a_failed_call_with_a_clear_message() {
    let error = mode(Arc::new(Host::default()), Limits::default())
        .execute("cayw.call('fail', {})", CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Capability);
    assert_eq!(error.report.calls[0].status, CallStatus::Failed);
    assert!(error.message.contains("denied by host"));
    assert!(!error.report.outcome_unknown);
}

#[tokio::test]
async fn script_failure_after_a_success_keeps_the_call_result() {
    let error = mode(Arc::new(Host::default()), Limits::default())
        .execute(
            "cayw.call('effect', {}); error('later failure')",
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Script);
    assert_eq!(error.report.calls[0].status, CallStatus::Succeeded);
    assert_eq!(error.report.calls[0].result, Some(json!({"written":true})));
    assert!(!error.report.outcome_unknown);
}

#[tokio::test]
async fn strict_json_rejects_cycles_sparse_tables_and_unsupported_values() {
    for script in [
        "local t = {}; t.self = t; cayw.emit(t)",
        "cayw.emit({[1] = 'a', [3] = 'c'})",
        "cayw.emit({[1] = 'a', field = 'mixed'})",
        "cayw.emit(function() end)",
        "cayw.emit(0/0)",
        "cayw.emit(math.huge)",
        "cayw.emit(string.char(255))",
        "cayw.emit(setmetatable({}, {__index = {hidden = 1}}))",
    ] {
        let error = mode(Arc::new(Host::default()), Limits::default())
            .execute(script, CancellationToken::new())
            .await
            .unwrap_err();
        assert_eq!(
            error.kind,
            ErrorKind::Conversion,
            "script: {script}, error: {error}"
        );
        assert!(error.report.outputs.is_empty());
    }
}

#[tokio::test]
async fn integer_conversion_refuses_silent_precision_loss() {
    let error = mode(Arc::new(Host::default()), Limits::default())
        .execute(
            "return cayw.call('large_integer', {})",
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Conversion);
    assert_eq!(error.report.calls[0].status, CallStatus::Succeeded);
}

#[tokio::test]
async fn invalid_or_unregistered_calls_never_reach_the_host() {
    for (script, kind) in [
        ("cayw.call('missing', {})", ErrorKind::UnknownCapability),
        ("cayw.call('add', {a = '2', b = 3})", ErrorKind::Arguments),
        (
            "cayw.call('add', {a = 2, b = 3, extra = true})",
            ErrorKind::Arguments,
        ),
        (
            "cayw.call('effect', {broken = function() end})",
            ErrorKind::Conversion,
        ),
    ] {
        let host = Arc::new(Host::default());
        let error = mode(host.clone(), Limits::default())
            .execute(script, CancellationToken::new())
            .await
            .unwrap_err();
        assert_eq!(error.kind, kind, "{script}: {error}");
        assert_eq!(host.effects.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn sandbox_has_no_ambient_execution_and_each_run_has_fresh_globals() {
    let code = mode(Arc::new(Host::default()), Limits::default());
    let report = code.execute(r#"
        for _, name in {"os", "io", "package", "require", "load", "loadstring", "loadfile", "dofile", "process", "debug", "getfenv", "setfenv", "print"} do
            assert(_G[name] == nil, name)
        end
        assert(type(coroutine) == 'table')
        assert(type(pcall) == 'function' and type(xpcall) == 'function')
        saved = 'only this execution'
        return true
    "#, CancellationToken::new()).await.unwrap();
    assert_eq!(report.return_value, json!(true));
    assert_eq!(
        code.execute("return saved", CancellationToken::new())
            .await
            .unwrap()
            .return_value,
        Value::Null
    );
    let error = code
        .execute("table.insert = function() end", CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Script);
}

#[test]
fn every_limit_must_be_non_zero() {
    for limits in [
        Limits {
            memory_bytes: 0,
            ..Limits::default()
        },
        Limits {
            instruction_budget: 0,
            ..Limits::default()
        },
        Limits {
            timeout: Duration::ZERO,
            ..Limits::default()
        },
        Limits {
            host_calls: 0,
            ..Limits::default()
        },
        Limits {
            output_bytes: 0,
            ..Limits::default()
        },
    ] {
        let error = CodeMode::new(Arc::new(Host::default()), limits)
            .err()
            .expect("zero limit accepted");
        assert_eq!(error.kind, ErrorKind::InvalidLimits);
    }
}
