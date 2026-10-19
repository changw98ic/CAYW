//! Explicit live smoke test. Running this example sends real model requests.
use cayw_agent::{ai::*, *};
use futures::StreamExt;
use serde_json::json;
use std::{
    error::Error,
    io,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::time::timeout;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;
const TIMEOUT: Duration = Duration::from_secs(90);

fn check(condition: bool, message: &str) -> TestResult {
    if condition {
        Ok(())
    } else {
        Err(io::Error::other(message).into())
    }
}

fn report_message(case: &str, message: &AssistantMessage) {
    println!(
        "{}",
        json!({
            "event": "assistant_end",
            "case": case,
            "requested_model": message.model,
            "response_model": message.response_model,
            "response_id": message.response_id,
            "stop_reason": message.stop_reason,
            "text": content_text(&message.content),
            "tool_calls": message.tool_calls(),
            "usage": message.usage,
            "duration_ms": message.duration_ms,
            "error": message.error_message,
        })
    );
}

async fn text_stream(provider: &dyn Provider, model: &Model) -> TestResult {
    let request = ModelRequest {
        model: model.clone(),
        messages: vec![Message::user(
            "Reply with exactly CAYW_MAGPIE_STREAM_OK and nothing else.",
        )],
        options: StreamOptions {
            max_tokens: Some(1024),
            thinking_level: ThinkingLevel::Low,
            ..Default::default()
        },
    };
    let cancel = CancellationToken::new();
    let mut stream = provider.stream(request, cancel.clone());
    let mut text_deltas = 0;
    let mut thinking_deltas = 0;
    let mut terminals = Vec::new();
    let outcome = timeout(TIMEOUT, async {
        while let Some(event) = stream.next().await {
            match &event {
                AssistantEvent::TextDelta { delta, .. } if !delta.is_empty() => text_deltas += 1,
                AssistantEvent::ThinkingDelta { delta, .. } if !delta.is_empty() => {
                    thinking_deltas += 1
                }
                _ => {}
            }
            if event.is_terminal() {
                terminals.push(event.message().clone());
            }
        }
    })
    .await;
    cancel.cancel();
    outcome.map_err(|_| io::Error::other("text_stream timed out"))?;
    println!(
        "{}",
        json!({"event":"stream_counts", "case":"text_stream", "text_deltas":text_deltas,
            "thinking_deltas":thinking_deltas, "terminals":terminals.len()})
    );
    for message in &terminals {
        report_message("text_stream", message);
    }
    check(terminals.len() == 1, "expected one stream terminal")?;
    let message = &terminals[0];
    check(
        message.stop_reason == StopReason::Stop,
        "text stream failed",
    )?;
    check(text_deltas > 0, "no text deltas received")?;
    check(
        content_text(&message.content).trim() == "CAYW_MAGPIE_STREAM_OK",
        "unexpected text stream answer",
    )?;
    println!("{}", json!({"case":"text_stream", "result":"passed"}));
    Ok(())
}

struct ReadProbe {
    value: String,
    executions: Arc<AtomicUsize>,
}

#[async_trait]
impl AgentTool for ReadProbe {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "read_probe".into(),
            description: "Read a synthetic probe value from this Rust process. The value can only be learned by calling this tool.".into(),
            parameters: json!({
                "type":"object",
                "properties":{"key":{"type":"string","enum":["cayw-rust"]}},
                "required":["key"],
                "additionalProperties":false
            }),
        }
    }

    async fn execute(&self, context: ToolContext) -> Result<ToolResult, String> {
        if context.arguments["key"] != "cayw-rust" {
            return Err("unexpected probe key".into());
        }
        self.executions.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResult {
            structured_content: Some(json!({"value":self.value})),
            ..ToolResult::text(self.value.clone())
        })
    }
}

async fn prompt(agent: &Agent, case: &str, input: &str) -> TestResult<RunResult> {
    println!("{}", json!({"event":"prompt", "case":case, "text":input}));
    let result = timeout(TIMEOUT, agent.prompt(Message::user(input)))
        .await
        .map_err(|_| io::Error::other(format!("{case} timed out")))??;
    report_message(case, &result.last_message);
    check(!agent.snapshot().is_running, "agent did not return to idle")?;
    Ok(result)
}

async fn tool_and_follow_up(provider: Arc<dyn Provider>, model: &Model) -> TestResult {
    let value = format!("CAYW_PROBE_{}_{}", timestamp_ms(), std::process::id());
    let executions = Arc::new(AtomicUsize::new(0));
    let agent = Agent::new(provider, model.clone());
    agent.set_max_turns(3)?;
    agent.set_stream_options(StreamOptions {
        max_tokens: Some(2048),
        thinking_level: ThinkingLevel::Low,
        ..Default::default()
    })?;
    agent.set_tools(vec![Arc::new(ReadProbe {
        value: value.clone(),
        executions: executions.clone(),
    })])?;
    agent.subscribe(|event, _| async move {
        match event {
            AgentEvent::ToolExecutionStart { call } => {
                println!("{}", json!({"event":"tool_start", "call":call}));
            }
            AgentEvent::ToolExecutionEnd {
                call,
                result,
                duration_ms,
            } => {
                println!(
                    "{}",
                    json!({"event":"tool_end", "call_id":call.id,
                    "result":result, "duration_ms":duration_ms})
                );
            }
            AgentEvent::MessageEnd {
                message: Message::Assistant(message),
            } if message.stop_reason == StopReason::ToolUse => {
                report_message("tool_request", &message);
            }
            _ => {}
        }
        Ok(())
    });
    let result = prompt(
        &agent,
        "tool_loop",
        "Call read_probe exactly once with key cayw-rust. Then reply with only its returned value, unchanged. Do not guess the value.",
    )
    .await?;
    check(
        result.last_message.stop_reason == StopReason::Stop,
        "tool loop failed",
    )?;
    check(
        executions.load(Ordering::SeqCst) == 1,
        "tool did not execute exactly once",
    )?;
    check(
        content_text(&result.last_message.content).trim() == value,
        "model did not return the real tool value",
    )?;
    let snapshot = agent.snapshot();
    let calls: Vec<_> = snapshot
        .messages
        .iter()
        .filter_map(|message| match message {
            Message::Assistant(message) => Some(message.tool_calls()),
            _ => None,
        })
        .flatten()
        .collect();
    let results: Vec<_> = snapshot
        .messages
        .iter()
        .filter_map(|message| match message {
            Message::ToolResult(message) => Some(message),
            _ => None,
        })
        .collect();
    check(
        calls.len() == 1 && results.len() == 1,
        "tool history is not one paired call/result",
    )?;
    check(
        calls[0].id == results[0].tool_call_id && calls[0].name == results[0].tool_name,
        "tool call/result identity mismatch",
    )?;
    check(
        !results[0].is_error && !results[0].outcome_unknown && !snapshot.needs_reconciliation,
        "tool outcome was not confirmed",
    )?;
    println!(
        "{}",
        json!({"case":"tool_loop", "result":"passed", "executions":1,
        "paired_call_id":calls[0].id, "expected_value":value})
    );

    agent.set_tools(vec![])?;
    let result = prompt(
        &agent,
        "follow_up",
        "Repeat only the exact value returned by read_probe in our previous turn. Do not add any other text.",
    )
    .await?;
    check(
        result.last_message.stop_reason == StopReason::Stop,
        "follow-up failed",
    )?;
    check(
        content_text(&result.last_message.content).trim() == value,
        "follow-up lost the tool value",
    )?;
    check(
        executions.load(Ordering::SeqCst) == 1,
        "follow-up unexpectedly executed a tool",
    )?;
    println!(
        "{}",
        json!({"case":"follow_up", "result":"passed", "executions":1})
    );
    Ok(())
}

async fn cancel_stream(provider: Arc<dyn Provider>, model: &Model) -> TestResult {
    let agent = Agent::new(provider, model.clone());
    agent.set_max_turns(1)?;
    agent.set_stream_options(StreamOptions {
        max_tokens: Some(1024),
        thinking_level: ThinkingLevel::Low,
        ..Default::default()
    })?;
    let observed = Arc::new(AtomicUsize::new(0));
    let count = observed.clone();
    let control = agent.control();
    agent.subscribe(move |event, _| {
        if let AgentEvent::MessageUpdate { event, .. } = event
            && let AssistantEvent::TextDelta { delta, .. } = *event
            && !delta.is_empty()
        {
            count.fetch_add(1, Ordering::SeqCst);
            control.abort();
        }
        async { Ok(()) }
    });
    let result = prompt(
        &agent,
        "cancel_stream",
        "Print the integers 1 through 200, one per line. Output only the integers and keep going until 200.",
    )
    .await?;
    check(
        observed.load(Ordering::SeqCst) > 0,
        "cancel test received no text delta",
    )?;
    check(
        result.last_message.stop_reason == StopReason::Aborted,
        "cancel did not produce aborted",
    )?;
    let snapshot = agent.snapshot();
    check(
        snapshot.streaming_message.is_none()
            && snapshot.pending_tool_calls.is_empty()
            && !snapshot.needs_reconciliation,
        "cancel left unsettled agent state",
    )?;
    println!(
        "{}",
        json!({"case":"cancel_stream", "result":"passed",
        "text_deltas_before_abort":observed.load(Ordering::SeqCst), "running":snapshot.is_running})
    );
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> TestResult {
    let base =
        std::env::var("MAGPIE_BASE_URL").unwrap_or_else(|_| "http://127.0.0.1:3425/v1".into());
    let key = std::env::var("MAGPIE_API_KEY").map_err(|_| {
        io::Error::other("Set MAGPIE_API_KEY explicitly; this example sends real requests")
    })?;
    check(!key.trim().is_empty(), "MAGPIE_API_KEY must not be empty")?;
    let model = Model::new(
        "magpie",
        std::env::var("MAGPIE_MODEL").unwrap_or_else(|_| "group/auto-deepseek-v4-1-flash".into()),
    );
    let provider: Arc<dyn Provider> = Arc::new(providers::OpenAiProvider::new(&base, key)?);
    println!(
        "{}",
        json!({"event":"live_test_start", "timestamp_ms":timestamp_ms(),
        "provider":"magpie", "requested_model":model.id, "timeout_seconds":TIMEOUT.as_secs(),
        "thinking_level":"low", "text_max_tokens":1024, "tool_max_tokens":2048,
        "max_agent_turns":3, "inputs":"synthetic", "credentials":"not logged"})
    );
    text_stream(provider.as_ref(), &model).await?;
    tool_and_follow_up(provider.clone(), &model).await?;
    cancel_stream(provider, &model).await?;
    println!(
        "{}",
        json!({"event":"live_test_end", "timestamp_ms":timestamp_ms(),
        "result":"passed", "cases":4})
    );
    Ok(())
}
