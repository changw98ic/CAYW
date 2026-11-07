use async_trait::async_trait;
use cayw_agent::{Agent, AgentError, AgentTool, ExecutionMode};
use cayw_ai::{
    ArgumentValidator, AssistantEvent, AssistantMessage, AssistantStream, CancellationToken,
    Content, Message, Model, ModelRequest, Provider, StopReason, ToolCall, ToolDefinition,
};
use cayw_codemode::{
    CallStatus, CapabilityError, CapabilityHost, CapabilityStop, CodeMode, CodeModeTool, ErrorKind,
    Limits,
};
use futures::{StreamExt, stream};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::Notify;

#[derive(Default)]
struct Host {
    received: Mutex<Vec<(String, Value)>>,
    tokens: Mutex<Vec<CancellationToken>>,
    started: Notify,
}

fn definition(name: &str, parameters: Value) -> ToolDefinition {
    ToolDefinition {
        name: name.into(),
        description: format!("Offline capability {name}"),
        parameters,
    }
}

#[async_trait]
impl CapabilityHost for Host {
    fn capabilities(&self) -> Vec<ToolDefinition> {
        let empty = json!({"type":"object","properties":{},"additionalProperties":false});
        vec![
            definition(
                "echo",
                json!({"type":"object","properties":{"value":{}},"required":["value"],"additionalProperties":false}),
            ),
            definition(
                "add",
                json!({"type":"object","properties":{"left":{"type":"integer"},"right":{"type":"integer"}},"required":["left","right"],"additionalProperties":false}),
            ),
            definition("fail", empty.clone()),
            definition("stop_budget", empty.clone()),
            definition("stop_cancelled", empty.clone()),
            definition("stop_timeout", empty.clone()),
            definition("unknown", empty.clone()),
            definition("hang", empty.clone()),
            definition("large", empty.clone()),
            definition("fail_large", empty.clone()),
            definition("unknown_large", empty.clone()),
            definition("unsafe_integer", empty),
        ]
    }

    async fn call(
        &self,
        name: &str,
        arguments: Value,
        cancel: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        self.received
            .lock()
            .unwrap()
            .push((name.into(), arguments.clone()));
        self.tokens.lock().unwrap().push(cancel.clone());
        self.started.notify_one();
        match name {
            "echo" => Ok(arguments["value"].clone()),
            "add" => Ok(json!(
                arguments["left"].as_i64().unwrap() + arguments["right"].as_i64().unwrap()
            )),
            "fail" => Err(CapabilityError::new("permission denied")),
            "stop_budget" => Err(CapabilityError::stopped(
                CapabilityStop::Budget,
                "host quota reached",
            )),
            "stop_cancelled" => Err(CapabilityError::stopped(
                CapabilityStop::Cancelled,
                "host stopped",
            )),
            "stop_timeout" => Err(CapabilityError::stopped(
                CapabilityStop::Timeout,
                "host deadline",
            )),
            "unknown" => Err(CapabilityError::unknown("response lost after dispatch")),
            "fail_large" => Err(CapabilityError::new("失".repeat(4000))),
            "unknown_large" => Err(CapabilityError::unknown("未".repeat(4000))),
            "large" => Ok(json!("x".repeat(8192))),
            "unsafe_integer" => Ok(json!(9_007_199_254_740_993_u64)),
            "hang" => futures::future::pending().await,
            _ => Err(CapabilityError::new("host rejected an unregistered name")),
        }
    }
}

fn mode(limits: Limits) -> (Arc<Host>, CodeMode) {
    let host = Arc::new(Host::default());
    let code = CodeMode::new(host.clone(), limits).unwrap();
    (host, code)
}

#[tokio::test]
async fn real_luau_branches_loops_and_multiple_calls_preserve_order() {
    let (host, code) = mode(Limits::default());
    let report = code
        .execute(
            r#"
        local total = 0
        for i = 1, 4 do
            if i % 2 == 0 then
                total = cayw.call("add", {left = total, right = i})
                cayw.emit({step = i, total = total})
            end
        end
        local value = cayw.call("echo", {value = {text = "你好，世界", count = utf8.len("你好")}})
        return {sum = total, value = value}
    "#,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        report.return_value,
        json!({"sum":6,"value":{"text":"你好，世界","count":2}})
    );
    assert_eq!(
        report.outputs,
        vec![json!({"step":2,"total":2}), json!({"step":4,"total":6})]
    );
    assert_eq!(
        report
            .calls
            .iter()
            .map(|call| (call.sequence, call.name.as_str(), call.status))
            .collect::<Vec<_>>(),
        vec![
            (1, "add", CallStatus::Succeeded),
            (2, "add", CallStatus::Succeeded),
            (3, "echo", CallStatus::Succeeded)
        ]
    );
    assert_eq!(
        host.received.lock().unwrap()[1].1,
        json!({"left":2,"right":4})
    );
    assert!(!report.outcome_unknown);
}

#[tokio::test]
async fn discovery_returns_only_names_and_description_is_requested_explicitly() {
    let (_, code) = mode(Limits::default());
    let report = code
        .execute(
            r#"
        local names = cayw.discover()
        local schema = cayw.describe("add")
        return {first = names[1], description = schema.description, parameters = schema.parameters}
    "#,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(report.return_value["first"], "add");
    assert_eq!(report.return_value["description"], "Offline capability add");
    assert_eq!(
        report.return_value["parameters"]["required"],
        json!(["left", "right"])
    );
    assert!(report.calls.is_empty());
}

#[tokio::test]
async fn json_null_empty_arrays_and_shared_tables_round_trip_without_coercion() {
    let (_, code) = mode(Limits::default());
    let report = code.execute(r#"
        local shared = {ok = true}
        return cayw.call("echo", {value = {null = cayw.null, array = cayw.array(), object = {}, a = shared, b = shared, values = {false, 1.5, "雪", cayw.null}}})
    "#, CancellationToken::new()).await.unwrap();
    assert_eq!(
        report.return_value,
        json!({"null":null,"array":[],"object":{},"a":{"ok":true},"b":{"ok":true},"values":[false,1.5,"雪",null]})
    );
}

#[tokio::test]
async fn json_rejects_cycles_sparse_tables_mixed_keys_and_unsupported_values() {
    let (_, code) = mode(Limits::default());
    for script in [
        "local t = {}; t.self = t; return t",
        "return {[1] = true, [3] = false}",
        "return {[1] = true, name = 'mixed'}",
        "return {[false] = 'invalid key'}",
        "return function() end",
        "return 0/0",
        "return 1/0",
        "return 9007199254740992",
        "return string.char(255)",
        "return setmetatable({}, {__index = function() return true end})",
        "return cayw.array({field = true})",
        "local t = {}; for i=1,70 do t = {t} end; return t",
        "return 1, 2",
    ] {
        let error = code
            .execute(script, CancellationToken::new())
            .await
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::Conversion, "{script}: {error}");
        assert!(error.report.calls.is_empty());
        assert!(!error.report.outcome_unknown);
    }
}

#[tokio::test]
async fn arguments_and_catalog_are_checked_before_starting_host_effects() {
    let (host, code) = mode(Limits::default());
    for (script, kind) in [
        (
            "return cayw.call('missing', {})",
            ErrorKind::UnknownCapability,
        ),
        (
            "return cayw.call('add', {left='1', right=2})",
            ErrorKind::Arguments,
        ),
        (
            "return cayw.call('add', {left=1, right=2, extra=true})",
            ErrorKind::Arguments,
        ),
        ("return cayw.call('add', {})", ErrorKind::Arguments),
        ("return cayw.call('add', {1,2})", ErrorKind::Arguments),
        ("return cayw.call('add')", ErrorKind::Arguments),
        ("return cayw.call(1, {})", ErrorKind::Arguments),
        ("return cayw.call('add', {}, true)", ErrorKind::Arguments),
        (
            "return cayw.describe('missing')",
            ErrorKind::UnknownCapability,
        ),
    ] {
        let error = code
            .execute(script, CancellationToken::new())
            .await
            .unwrap_err();
        assert_eq!(error.kind, kind, "{script}: {error}");
        assert!(error.report.calls.is_empty());
    }
    assert!(host.received.lock().unwrap().is_empty());
}

#[tokio::test]
async fn sandbox_exposes_no_ambient_io_or_protected_call_escape() {
    let (_, code) = mode(Limits::default());
    let report = code
        .execute(
            r#"
        return {os == nil, io == nil, require == nil, package == nil,
            load == nil, loadstring == nil, loadfile == nil, dofile == nil,
            process == nil, debug == nil, getfenv == nil, setfenv == nil,
            coroutine == nil, pcall == nil, xpcall == nil, print == nil,
            type(coroutine) == 'table', type(pcall) == 'function',
            type(xpcall) == 'function'}
    "#,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        report.return_value,
        json!([
            true, true, true, true, true, true, true, true, true, true, true, true, false, false,
            false, true, true, true, true
        ])
    );
    let error = code
        .execute(
            "cayw.call = function() return true end",
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Script);
}

#[tokio::test]
async fn every_execution_has_an_isolated_environment() {
    let (_, code) = mode(Limits::default());
    let report = code
        .execute(
            "secret = 'old run'; return secret",
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(report.return_value, json!("old run"));
    let report = code
        .execute("return secret == nil", CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(report.return_value, true);
}

#[tokio::test]
async fn script_failure_keeps_successful_calls_and_prior_emits() {
    let (_, code) = mode(Limits::default());
    let error = code
        .execute(
            "cayw.call('echo', {value=7}); cayw.emit('saved'); error('later failure')",
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Script);
    assert_eq!(error.report.outputs, vec![json!("saved")]);
    assert_eq!(error.report.calls[0].status, CallStatus::Succeeded);
    assert_eq!(error.report.calls[0].result, Some(json!(7)));
    assert!(!error.report.outcome_unknown);
}

#[tokio::test]
async fn known_and_unknown_host_errors_keep_prior_success_and_stop_further_calls() {
    let (host, code) = mode(Limits::default());
    for (name, kind, status, unknown) in [
        ("fail", ErrorKind::Capability, CallStatus::Failed, false),
        (
            "unknown",
            ErrorKind::OutcomeUnknown,
            CallStatus::OutcomeUnknown,
            true,
        ),
    ] {
        let error = code.execute(&format!("cayw.call('echo', {{value=true}}); cayw.call('{name}', {{}}); cayw.call('echo', {{value=false}})"), CancellationToken::new()).await.unwrap_err();
        assert_eq!(error.kind, kind);
        assert_eq!(error.report.calls.len(), 2);
        assert_eq!(error.report.calls[0].status, CallStatus::Succeeded);
        assert_eq!(error.report.calls[1].status, status);
        assert_eq!(error.report.outcome_unknown, unknown);
    }
    assert_eq!(host.received.lock().unwrap().len(), 4);
}

#[tokio::test]
async fn host_call_limit_prevents_the_next_effect_and_preserves_completed_calls() {
    let (host, code) = mode(Limits {
        host_calls: 2,
        ..Limits::default()
    });
    let error = code
        .execute(
            "for i=1,3 do cayw.call('echo', {value=i}) end",
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::HostCallLimit);
    assert_eq!(error.report.calls.len(), 2);
    assert!(
        error
            .report
            .calls
            .iter()
            .all(|call| call.status == CallStatus::Succeeded)
    );
    assert_eq!(host.received.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn output_limit_stops_emits_without_discarding_successful_audit() {
    let (_, code) = mode(Limits {
        output_bytes: 64,
        ..Limits::default()
    });
    let error = code
        .execute(
            "cayw.call('echo', {value=1}); cayw.emit('before'); cayw.emit(string.rep('x', 100))",
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::OutputLimit);
    assert_eq!(error.report.calls[0].status, CallStatus::Succeeded);
    assert_eq!(error.report.outputs, vec![json!("before")]);
    assert!(error.report.output_bytes <= 64);
}

#[tokio::test]
async fn over_budget_host_results_keep_success_status_with_omitted_payload() {
    let (_, code) = mode(Limits {
        output_bytes: 64,
        ..Limits::default()
    });
    let error = code
        .execute("return cayw.call('large', {})", CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::OutputLimit);
    assert_eq!(error.report.calls[0].status, CallStatus::Succeeded);
    assert!(error.report.calls[0].result_omitted);
    assert!(error.report.calls[0].result.is_none());
    assert!(!error.report.outcome_unknown);
}

#[tokio::test]
async fn host_error_payloads_share_the_budget_without_losing_unknown_status() {
    let (_, code) = mode(Limits {
        output_bytes: 64,
        ..Limits::default()
    });
    for (name, status, unknown) in [
        ("fail_large", CallStatus::Failed, false),
        ("unknown_large", CallStatus::OutcomeUnknown, true),
    ] {
        let error = code
            .execute(
                &format!("return cayw.call('{name}', {{}})"),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::OutputLimit);
        assert_eq!(error.report.calls[0].status, status);
        assert_eq!(error.report.outcome_unknown, unknown);
        assert!(error.report.calls[0].result_omitted);
        assert!(error.report.calls[0].error.is_none());
        assert!(error.report.output_bytes <= 64);
    }
}

#[tokio::test]
async fn lossy_host_integer_is_rejected_after_recording_host_success() {
    let (_, code) = mode(Limits::default());
    let error = code
        .execute(
            "return cayw.call('unsafe_integer', {})",
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Conversion);
    assert_eq!(error.report.calls[0].status, CallStatus::Succeeded);
    assert!(!error.report.outcome_unknown);
}

#[tokio::test]
async fn json_budget_counts_escaped_utf8_and_repeated_shared_values() {
    let (_, code) = mode(Limits {
        output_bytes: 13,
        ..Limits::default()
    });
    let report = code
        .execute("return {a='雪\\n'}", CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(report.output_bytes, 13);
    let error = code
        .execute(
            "local s = '12345'; return {s,s,s}",
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::OutputLimit);
}

#[tokio::test]
async fn json_conversion_stops_at_the_budget_before_expanding_more_shared_values() {
    let (_, code) = mode(Limits {
        output_bytes: 128,
        ..Limits::default()
    });
    let error = code
        .execute(
            "local s = string.rep('x', 100); return {s, s, function() end}",
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::OutputLimit);
    assert!(error.report.outputs.is_empty());
    assert!(error.report.return_value.is_null());
    assert_eq!(
        error.report.output_bytes,
        serde_json::to_vec(&error.message).unwrap().len()
    );
}

#[tokio::test]
async fn vm_allocation_and_source_are_bounded() {
    let (_, code) = mode(Limits {
        memory_bytes: 256 * 1024,
        ..Limits::default()
    });
    let error = code
        .execute(
            "return string.rep('x', 4 * 1024 * 1024)",
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::MemoryLimit);
    let error = code
        .execute(&" ".repeat(128 * 1024), CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::MemoryLimit);
}

#[tokio::test]
async fn infinite_cpu_loop_is_stopped_by_instruction_checkpoints() {
    let (_, code) = mode(Limits {
        instruction_budget: 32,
        ..Limits::default()
    });
    let error = code
        .execute("while true do end", CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::InstructionLimit);
    assert_eq!(error.report.instruction_checkpoints, 33);
    assert!(!error.report.outcome_unknown);
}

#[tokio::test]
async fn cpu_loop_and_pending_host_call_use_their_own_deadlines() {
    let (_, code) = mode(Limits {
        instruction_budget: u64::MAX,
        timeout: Duration::from_millis(40),
        ..Limits::default()
    });
    let error = code
        .execute("while true do end", CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Timeout);
    assert!(!error.report.outcome_unknown);
    // Separate budget so Luau setup under loaded CI cannot expire before hang's
    // first poll; wait for started before asserting unknown settlement.
    let (host, code) = mode(Limits {
        instruction_budget: u64::MAX,
        timeout: Duration::from_secs(1),
        host_wait: Duration::from_secs(1),
        shutdown_grace: Duration::from_millis(100),
        ..Limits::default()
    });
    let mut execution =
        Box::pin(code.execute("return cayw.call('hang', {})", CancellationToken::new()));
    tokio::select! {
        result = &mut execution => panic!(
            "returned before hang host started (timeout raced with setup): {result:?}"
        ),
        () = host.started.notified() => {}
    }
    let error = execution.await.unwrap_err();
    assert_eq!(error.kind, ErrorKind::Timeout);
    assert!(error.report.outcome_unknown);
    assert_eq!(error.report.calls[0].status, CallStatus::OutcomeUnknown);
    assert!(host.tokens.lock().unwrap()[0].is_cancelled());
}

#[tokio::test(flavor = "current_thread")]
async fn pure_cpu_execution_yields_so_same_thread_cancellation_can_run() {
    let (_, code) = mode(Limits {
        instruction_budget: u64::MAX,
        timeout: Duration::from_secs(2),
        ..Limits::default()
    });
    let cancel = CancellationToken::new();
    let (result, ()) = tokio::join!(code.execute("while true do end", cancel.clone()), async {
        tokio::task::yield_now().await;
        cancel.cancel();
    });
    let error = result.unwrap_err();
    assert_eq!(error.kind, ErrorKind::Cancelled);
    assert!(!error.report.outcome_unknown);
}

#[tokio::test]
async fn script_error_text_is_bounded_without_erasing_completed_capabilities() {
    let (_, code) = mode(Limits::default());
    let error = code
        .execute(
            "cayw.call('echo', {value=42}); error(string.rep('雪', 8000))",
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Script);
    assert!(error.message.len() <= 2048);
    assert_eq!(error.report.calls[0].status, CallStatus::Succeeded);
    assert_eq!(error.report.calls[0].result, Some(json!(42)));
}

#[tokio::test]
async fn async_host_bridge_keeps_restricted_globals_after_suspension() {
    struct YieldingHost;
    #[async_trait]
    impl CapabilityHost for YieldingHost {
        fn capabilities(&self) -> Vec<ToolDefinition> {
            vec![definition("yield", json!({"type":"object"}))]
        }
        async fn call(
            &self,
            _: &str,
            _: Value,
            _: CancellationToken,
        ) -> Result<Value, CapabilityError> {
            tokio::task::yield_now().await;
            Ok(json!(null))
        }
    }
    let code = CodeMode::new(Arc::new(YieldingHost), Limits::default()).unwrap();
    let report = code.execute("for i=1,3 do cayw.call('yield', {}) end; return {type(coroutine), type(pcall), type(xpcall), debug == nil, os == nil}", CancellationToken::new()).await.unwrap();
    assert_eq!(
        report.return_value,
        json!(["table", "function", "function", true, true])
    );
    assert!(
        report
            .calls
            .iter()
            .all(|call| call.status == CallStatus::Succeeded)
    );
}

#[tokio::test]
async fn cancellation_before_execution_starts_no_capabilities() {
    let (host, code) = mode(Limits::default());
    let cancel = CancellationToken::new();
    cancel.cancel();
    let error = code
        .execute("cayw.call('echo', {value=1})", cancel)
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Cancelled);
    assert!(error.report.calls.is_empty());
    assert!(host.received.lock().unwrap().is_empty());
}

#[tokio::test]
async fn cancellation_during_a_started_capability_keeps_unknown_and_cancels_child() {
    let (host, code) = mode(Limits::default());
    let cancel = CancellationToken::new();
    let (result, ()) = tokio::join!(
        code.execute(
            "cayw.call('echo', {value=3}); return cayw.call('hang', {})",
            cancel.clone()
        ),
        async {
            loop {
                host.started.notified().await;
                if host.received.lock().unwrap().len() == 2 {
                    break;
                }
            }
            cancel.cancel();
        }
    );
    let error = result.unwrap_err();
    assert_eq!(error.kind, ErrorKind::Cancelled);
    assert_eq!(error.report.calls[0].status, CallStatus::Succeeded);
    assert_eq!(error.report.calls[1].status, CallStatus::OutcomeUnknown);
    assert!(error.report.outcome_unknown);
    assert!(
        host.tokens
            .lock()
            .unwrap()
            .iter()
            .all(CancellationToken::is_cancelled)
    );
}

#[tokio::test]
async fn dropping_the_execution_future_cancels_started_capability_tokens() {
    let (host, code) = mode(Limits::default());
    let mut execution =
        Box::pin(code.execute("return cayw.call('hang', {})", CancellationToken::new()));
    tokio::select! {
        biased;
        result = &mut execution => panic!("hanging host unexpectedly settled: {result:?}"),
        () = host.started.notified() => (),
    }
    drop(execution);
    assert!(host.tokens.lock().unwrap()[0].is_cancelled());
}

#[tokio::test]
async fn known_host_failure_is_catchable_and_another_authorized_call_can_continue() {
    let (host, code) = mode(Limits::default());
    let report = code
        .execute(
            r#"
        local ok, message = pcall(function() return cayw.call('fail', {}) end)
        local next_value = cayw.call('echo', {value = 'recovered'})
        local ok2, handled = xpcall(function() return cayw.call('fail', {}) end,
            function(error_value) return 'handled: ' .. tostring(error_value) end)
        return {ok = ok, message = tostring(message), next = next_value, ok2 = ok2, handled = handled}
    "#,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(report.return_value["ok"], false);
    assert!(
        report.return_value["message"]
            .as_str()
            .unwrap()
            .contains("permission denied")
    );
    assert_eq!(report.return_value["next"], "recovered");
    assert_eq!(report.return_value["ok2"], false);
    assert!(
        report.return_value["handled"]
            .as_str()
            .unwrap()
            .starts_with("handled: ")
    );
    assert_eq!(
        report
            .calls
            .iter()
            .map(|call| (call.name.as_str(), call.status))
            .collect::<Vec<_>>(),
        vec![
            ("fail", CallStatus::Failed),
            ("echo", CallStatus::Succeeded),
            ("fail", CallStatus::Failed)
        ]
    );
    assert!(!report.outcome_unknown);
    assert_eq!(host.received.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn nested_pcall_and_coroutines_cannot_swallow_unknown_or_dispatch_more_calls() {
    let (host, code) = mode(Limits::default());
    let error = code
        .execute(
            r#"
        local caught = pcall(function() return cayw.call('unknown', {}) end)
        local co = coroutine.create(function()
            pcall(function() return cayw.call('echo', {value = 'inside coroutine'}) end)
        end)
        coroutine.resume(co)
        pcall(function() return cayw.call('echo', {value = 'after unknown'}) end)
        return {caught = caught, finished = true}
    "#,
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::OutcomeUnknown);
    assert!(error.report.outcome_unknown);
    assert_eq!(error.report.calls.len(), 1);
    assert_eq!(error.report.calls[0].name, "unknown");
    assert_eq!(error.report.calls[0].status, CallStatus::OutcomeUnknown);
    assert_eq!(host.received.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn pcall_cannot_swallow_cancellation_before_the_next_dispatch() {
    let (host, code) = mode(Limits {
        timeout: Duration::from_secs(1),
        shutdown_grace: Duration::from_millis(100),
        ..Limits::default()
    });
    let cancel = CancellationToken::new();
    let (result, ()) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(
            code.execute(
                r#"
            local first = pcall(function()
                return pcall(function() return cayw.call('hang', {}) end)
            end)
            local second = pcall(function() return cayw.call('echo', {value = 'after cancel'}) end)
            return {first = first, second = second}
        "#,
                cancel.clone()
            ),
            async {
                host.started.notified().await;
                cancel.cancel();
            }
        )
    })
    .await
    .expect("protected Host cancellation must finish its original cleanup");
    let error = result.unwrap_err();
    assert_eq!(error.kind, ErrorKind::Cancelled);
    assert!(error.report.outcome_unknown);
    assert_eq!(error.report.calls.len(), 1);
    assert_eq!(error.report.calls[0].name, "hang");
    assert_eq!(error.report.calls[0].status, CallStatus::OutcomeUnknown);
    assert_eq!(host.received.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn pcall_cannot_swallow_the_host_call_budget() {
    let (host, code) = mode(Limits {
        host_calls: 1,
        ..Limits::default()
    });
    let error = code
        .execute(
            r#"
        local first = pcall(function() return cayw.call('echo', {value = 1}) end)
        local second = pcall(function() return cayw.call('echo', {value = 2}) end)
        local third = pcall(function() return cayw.call('echo', {value = 3}) end)
        return {first = first, second = second, third = third}
    "#,
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::HostCallLimit);
    assert_eq!(error.report.calls.len(), 1);
    assert_eq!(host.received.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn trusted_host_stops_cannot_be_swallowed_by_nested_protected_calls() {
    for (name, kind) in [
        ("stop_budget", ErrorKind::Budget),
        ("stop_cancelled", ErrorKind::Cancelled),
        ("stop_timeout", ErrorKind::Timeout),
    ] {
        let (host, code) = mode(Limits::default());
        let error = code
            .execute(
                &format!(
                    r#"
            pcall(function()
                xpcall(function() cayw.call('{name}', {{}}) end,
                    function() return 'handled' end)
                cayw.call('echo', {{value='must not run'}})
            end)
            return 'must not complete'
        "#
                ),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind, kind);
        assert_eq!(error.report.calls.len(), 1);
        assert_eq!(error.report.calls[0].status, CallStatus::Failed);
        assert!(!error.report.outcome_unknown);
        assert_eq!(host.received.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn memory_exhaustion_inside_handlers_cannot_be_swallowed() {
    for script in [
        "xpcall(function() error('known failure') end, function() return string.rep('x',8*1024*1024) end); cayw.call('echo',{value='after memory stop'})",
        "pcall(function() xpcall(function() error('known failure') end, function() return string.rep('x',8*1024*1024) end) end); cayw.call('echo',{value='after memory stop'})",
    ] {
        let (host, code) = mode(Limits {
            memory_bytes: 2 * 1024 * 1024,
            ..Limits::default()
        });
        let error = code
            .execute(script, CancellationToken::new())
            .await
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::MemoryLimit, "{script}: {error}");
        assert!(!error.report.outcome_unknown);
        assert!(error.report.calls.is_empty());
        assert!(host.received.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn memory_exhaustion_inside_coroutines_cannot_be_swallowed() {
    for script in [
        "local co=coroutine.create(function() return string.rep('x',8*1024*1024) end); coroutine.resume(co); cayw.call('echo',{value='after memory stop'})",
        "pcall(coroutine.wrap(function() return string.rep('x',8*1024*1024) end)); cayw.call('echo',{value='after memory stop'})",
    ] {
        let (host, code) = mode(Limits {
            memory_bytes: 2 * 1024 * 1024,
            ..Limits::default()
        });
        let error = code
            .execute(script, CancellationToken::new())
            .await
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::MemoryLimit, "{script}: {error}");
        assert!(!error.report.outcome_unknown);
        assert!(error.report.calls.is_empty());
        assert!(host.received.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn pure_computation_coroutines_run_without_reaching_the_host() {
    let (host, code) = mode(Limits::default());
    let report = code
        .execute(
            r#"
        local co = coroutine.create(function(a)
            local acc = a
            for i = 1, 3 do acc = acc + coroutine.yield(acc) end
            return acc
        end)
        local ok1, v1 = coroutine.resume(co, 1)
        local ok2, v2 = coroutine.resume(co, 2)
        local ok3, v3 = coroutine.resume(co, 3)
        local ok4, v4 = coroutine.resume(co, 4)
        if not (ok1 and ok2 and ok3 and ok4) then error('coroutine resume failed') end
        local generator = coroutine.wrap(function()
            for i = 1, 3 do coroutine.yield(i * 10) end
        end)
        local a, b, c = generator(), generator(), generator()
        local caught, message = pcall(coroutine.wrap(function() error('wrapped failure', 0) end))
        return {v1 = v1, v2 = v2, v3 = v3, v4 = v4,
            a = a, b = b, c = c, caught = caught, message = tostring(message)}
    "#,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let mut value = report.return_value;
    let message = value.as_object_mut().unwrap().remove("message").unwrap();
    assert!(message.as_str().unwrap().contains("wrapped failure"));
    assert_eq!(
        value,
        json!({"v1":1,"v2":3,"v3":6,"v4":10,"a":10,"b":20,"c":30,"caught":false})
    );
    assert!(report.calls.is_empty());
    assert!(host.received.lock().unwrap().is_empty());
}

#[tokio::test]
async fn wrapped_coroutines_preserve_exact_return_arity_including_trailing_nils() {
    let (host, code) = mode(Limits::default());
    let report = code
        .execute(
            r##"
        local zero_fn = coroutine.wrap(function() end)
        local zero_count = select("#", zero_fn())

        local nil_fn = coroutine.wrap(function() return nil end)
        local nil_count = select("#", nil_fn())

        local pair_fn = coroutine.wrap(function() return 1, nil end)
        local pair_count = select("#", pair_fn())

        local yield_fn = coroutine.wrap(function()
            coroutine.yield(nil)
            return nil
        end)
        local yield_count = select("#", yield_fn())
        local return_count = select("#", yield_fn())

        return {
            zero = zero_count,
            single_nil = nil_count,
            trailing_nil = pair_count,
            yielded_nil = yield_count,
            returned_nil = return_count,
        }
    "##,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(
        report.return_value,
        json!({
            "zero": 0,
            "single_nil": 1,
            "trailing_nil": 2,
            "yielded_nil": 1,
            "returned_nil": 1,
        })
    );
    assert!(report.calls.is_empty());
    assert!(host.received.lock().unwrap().is_empty());
}

#[tokio::test]
async fn host_calls_inside_script_coroutines_are_rejected_before_dispatch() {
    let (host, code) = mode(Limits::default());
    let report = code
        .execute(
            r#"
        local co = coroutine.create(function()
            local ok, message = pcall(function() return cayw.call('echo', {value = 'inside'}) end)
            return ok, tostring(message)
        end)
        local resumed, inside_ok, message = coroutine.resume(co)
        local outside = cayw.call('echo', {value = 'outside'})
        return {resumed = resumed, inside_ok = inside_ok, message = message, outside = outside}
    "#,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(report.return_value["resumed"], true);
    assert_eq!(report.return_value["inside_ok"], false);
    assert!(
        report.return_value["message"]
            .as_str()
            .unwrap()
            .contains("coroutine")
    );
    assert_eq!(report.return_value["outside"], "outside");
    assert_eq!(
        report
            .calls
            .iter()
            .map(|call| (call.name.as_str(), call.status))
            .collect::<Vec<_>>(),
        vec![("echo", CallStatus::Succeeded)]
    );
    assert_eq!(host.received.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn each_execution_can_write_its_own_globals_without_leaking_them() {
    let (_, code) = mode(Limits::default());
    let report = code
        .execute(
            r#"
        answer = 42
        _G.answer = _G.answer + 1
        function greeting() return 'hello' end
        return {answer = answer, global = _G.answer, greeting = greeting()}
    "#,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        report.return_value,
        json!({"answer":43,"global":43,"greeting":"hello"})
    );
    let report = code
        .execute(
            "return {answer = answer, greeting = greeting}",
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(report.return_value, json!({}));
}

#[test]
fn only_one_agent_tool_is_exposed_with_strict_script_arguments() {
    let (_, code) = mode(Limits::default());
    let tool = CodeModeTool::new(code);
    let definition = tool.definition();
    assert_eq!(definition.name, "execute_code");
    assert_eq!(tool.execution_mode(), Some(ExecutionMode::Sequential));
    let validator = ArgumentValidator::new(&definition.parameters).unwrap();
    assert!(validator.validate(&json!({"script":"return true"})).is_ok());
    for arguments in [
        json!({}),
        json!({"script":1}),
        json!({"script":"return true","shell":"sh"}),
    ] {
        assert!(validator.validate(&arguments).is_err());
    }
}

struct FixtureProvider {
    messages: Mutex<VecDeque<AssistantMessage>>,
    requests: Mutex<Vec<ModelRequest>>,
}

impl Provider for FixtureProvider {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
        self.requests.lock().unwrap().push(request);
        let message = self
            .messages
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected model request");
        stream::iter([AssistantEvent::Done { message }]).boxed()
    }
}

fn agent_fixture(script: &str, limits: Limits) -> (Agent, Arc<Host>, Arc<FixtureProvider>) {
    let model = Model::new("fixture", "offline");
    let mut call = AssistantMessage::new(&model);
    call.content = vec![Content::ToolCall(ToolCall::new(
        "code-1",
        "execute_code",
        json!({"script":script}),
    ))];
    call.stop_reason = StopReason::ToolUse;
    let mut final_message = AssistantMessage::new(&model);
    final_message.content = vec![Content::text("finished")];
    final_message.stop_reason = StopReason::Stop;
    let provider = Arc::new(FixtureProvider {
        messages: Mutex::new([call, final_message].into()),
        requests: Mutex::default(),
    });
    let agent = Agent::new(provider.clone(), model);
    let (host, code) = mode(limits);
    agent
        .set_tools(vec![Arc::new(CodeModeTool::new(code))])
        .unwrap();
    (agent, host, provider)
}

fn code_result(messages: &[Message]) -> &cayw_ai::ToolResultMessage {
    messages
        .iter()
        .find_map(|message| match message {
            Message::ToolResult(result) if result.tool_name == "execute_code" => Some(result),
            _ => None,
        })
        .expect("execute_code result missing from agent transcript")
}

#[tokio::test]
async fn agent_runs_one_code_tool_with_multiple_host_calls_and_a_structured_report() {
    let (agent, host, provider) = agent_fixture(
        "local total = 0; for i=1,3 do total = cayw.call('add', {left=total, right=i}) end; return total",
        Limits::default(),
    );
    let result = agent.prompt(Message::user("sum integers")).await.unwrap();
    assert_eq!(result.last_message.content, vec![Content::text("finished")]);
    let tool = code_result(&result.messages);
    assert!(!tool.is_error);
    assert!(!tool.outcome_unknown);
    let content = tool.structured_content.as_ref().unwrap();
    assert_eq!(content["report"]["return_value"], 6);
    assert_eq!(content["report"]["calls"].as_array().unwrap().len(), 3);
    assert_eq!(host.received.lock().unwrap().len(), 3);
    assert_eq!(provider.requests.lock().unwrap().len(), 2);
    assert!(!agent.snapshot().needs_reconciliation);
}

#[tokio::test]
async fn agent_stops_after_known_host_stops_without_another_model_turn() {
    for name in ["stop_budget", "stop_timeout", "stop_cancelled"] {
        let (agent, host, provider) = agent_fixture(
            &format!("pcall(function() cayw.call('{name}', {{}}) end)"),
            Limits::default(),
        );
        let result = agent
            .prompt(Message::user("exercise trusted stop"))
            .await
            .unwrap();
        let tool = code_result(&result.messages);
        assert!(tool.is_error);
        assert!(!tool.outcome_unknown);
        assert_eq!(provider.requests.lock().unwrap().len(), 1, "{name}");
        assert_eq!(host.received.lock().unwrap().len(), 1);
        assert!(!agent.snapshot().needs_reconciliation);
    }
}

#[tokio::test]
async fn agent_stops_after_unknown_even_when_its_error_payload_exceeds_the_budget() {
    let (agent, _, provider) = agent_fixture(
        "cayw.call('echo', {value=true}); cayw.call('unknown_large', {})",
        Limits {
            output_bytes: 64,
            ..Limits::default()
        },
    );
    let result = agent.prompt(Message::user("run code")).await.unwrap();
    let tool = code_result(&result.messages);
    assert!(tool.is_error);
    assert!(tool.outcome_unknown);
    assert!(agent.snapshot().needs_reconciliation);
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    let content = tool.structured_content.as_ref().unwrap();
    assert_eq!(content["error"]["kind"], "output_limit");
    assert_eq!(content["report"]["calls"][0]["status"], "succeeded");
    assert_eq!(content["report"]["calls"][1]["status"], "outcome_unknown");
    assert_eq!(
        agent
            .prompt(Message::user("do not retry blindly"))
            .await
            .unwrap_err(),
        AgentError::UnresolvedToolEffects
    );
}

#[tokio::test]
async fn agent_keeps_unknown_when_host_wait_times_out_for_a_started_host_call() {
    let (agent, host, provider) = agent_fixture(
        "return cayw.call('hang', {})",
        Limits {
            // The VM timeout only bounds computation and must cover Luau setup
            // under loaded macOS CI. The started Host wait is bounded separately
            // by host_wait, which expires while hang remains pending.
            timeout: Duration::from_secs(1),
            host_wait: Duration::from_millis(300),
            shutdown_grace: Duration::from_millis(100),
            ..Limits::default()
        },
    );
    let prompt = agent.prompt(Message::user("run code"));
    tokio::pin!(prompt);
    // Prove the host call started before asserting unknown settlement. If the
    // Host wait expires during setup, call_host pops the unstarted call.
    tokio::select! {
        result = &mut prompt => panic!(
            "agent returned before hang host started (timeout raced with setup): {result:?}"
        ),
        () = host.started.notified() => {}
    }
    let result = tokio::time::timeout(Duration::from_secs(10), prompt)
        .await
        .expect("hang must settle through CodeMode shutdown grace")
        .unwrap();
    let tool = code_result(&result.messages);
    assert!(tool.is_error);
    assert!(tool.outcome_unknown);
    assert!(agent.snapshot().needs_reconciliation);
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    assert!(host.tokens.lock().unwrap()[0].is_cancelled());
    assert_eq!(
        tool.structured_content.as_ref().unwrap()["error"]["kind"],
        "timeout"
    );
}

#[test]
fn invalid_limits_and_catalogs_fail_before_any_execution() {
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
            timeout: Duration::MAX,
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
        assert!(
            matches!(CodeMode::new(Arc::new(Host::default()), limits), Err(error) if error.kind == ErrorKind::InvalidLimits)
        );
    }
    struct Catalog(Vec<ToolDefinition>);
    #[async_trait]
    impl CapabilityHost for Catalog {
        fn capabilities(&self) -> Vec<ToolDefinition> {
            self.0.clone()
        }
        async fn call(
            &self,
            _: &str,
            _: Value,
            _: CancellationToken,
        ) -> Result<Value, CapabilityError> {
            unreachable!("invalid catalogs never run")
        }
    }
    for definitions in [
        vec![definition("x", json!({})), definition("x", json!({}))],
        vec![definition("", json!({}))],
        vec![definition(
            "bad",
            json!({"$ref":"https://example.invalid/schema"}),
        )],
        vec![definition("bad", json!({"type": "invalid"}))],
    ] {
        assert!(
            matches!(CodeMode::new(Arc::new(Catalog(definitions)), Limits::default()), Err(error) if error.kind == ErrorKind::InvalidCatalog)
        );
    }
}
