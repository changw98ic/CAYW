# cayw-providers

Implements `cayw_ai::Provider` for Anthropic Messages and the Gemini Developer API. OpenAI Chat Completions has one implementation, `cayw_ai::providers::OpenAiProvider`; this crate does not duplicate it.

## Public API

- `AnthropicProvider::new(base_url, api_key) -> Result<Self, ProviderConfigError>` appends `/messages`. `standard(api_key)` uses `https://api.anthropic.com/v1` and sends `anthropic-version: 2023-06-01`.
- `GeminiProvider::new(base_url, api_key) -> Result<Self, ProviderConfigError>` defaults to `/models/{model}:streamGenerateContent?alt=sse`. `standard(api_key)` uses `https://generativelanguage.googleapis.com/v1beta`.
- `GeminiProvider::with_mode(GeminiMode::GenerateContent)` selects `/models/{model}:generateContent` and translates the complete JSON response into the same event sequence. `GeminiMode::StreamGenerateContent` selects streaming. Model IDs accept `name` or `models/name`.
- Both implement `stream(ModelRequest, CancellationToken) -> AssistantStream`. Creating the stream performs no network work. A consumed stream starts with `Start` and ends with exactly one `Done` or `Error`.
- `ProviderRegistry::new()` and `register(name, Arc<dyn Provider>) -> Result<(), ProviderConfigError>` configure exact routing by `request.model.provider`. Blank or duplicate names fail. Unknown names emit an error without fallback. Hosts may register the existing OpenAI implementation here.

```rust,no_run
use cayw_ai::{CancellationToken, Message, Model, ModelRequest, Provider, StreamOptions};
use cayw_providers::{AnthropicProvider, GeminiProvider, ProviderRegistry};
use futures::StreamExt;
use std::sync::Arc;

# async fn example() -> Result<(), Box<dyn std::error::Error>> {
// Keys come from the host. No environment or authentication file is consulted.
let mut providers = ProviderRegistry::new();
providers.register("anthropic", Arc::new(AnthropicProvider::standard("host-provided-key")?))?;
providers.register("gemini", Arc::new(GeminiProvider::standard("host-provided-key")?))?;
let request = ModelRequest {
    model: Model::new("anthropic", "host-selected-model-id"),
    messages: vec![Message::user("Hello")],
    options: StreamOptions { max_tokens: Some(512), ..Default::default() },
};
let mut stream = providers.stream(request, CancellationToken::new());
while let Some(event) = stream.next().await {
    // Forward the normalized events to the host.
    let _ = event;
}
# Ok(())
# }
```

The request model ID remains in `AssistantMessage.model`; the service's model identifier is stored separately in `response_model`, and its response ID in `response_id`. Constructors do not discover models or infer capabilities from a model name.

## HTTP behavior

Keys are explicit, nonempty HTTP header values. Anthropic uses `x-api-key`; Gemini uses `x-goog-api-key`, never URL query credentials. Base URLs must be HTTP(S), without embedded credentials, a query or a fragment. Existing base paths are retained. The built-in client disables proxies, redirects and retries. It does not read local model authentication settings. There is no detached networking task or hidden retry loop.

Cancellation is checked before sending, while waiting for response headers, while reading response bytes, during JSON scanning, and between content parts and normalized events. SSE framing scans at most 4 KiB before yielding to the runtime; JSON scanning yields every 4 KiB or 64 structural steps. Gemini produces one event per consumer poll, including each text delta and the separate tool start/delta/end events. Empty parts also pass through cancellation checks. Poll the stream to observe cancellation as `Error` with `StopReason::Aborted`. Dropping the stream drops its pending HTTP futures, parser and response body. Anthropic releases its response on `message_stop` without waiting for HTTP EOF. As with any remote operation, closing the connection cannot promise that a service has stopped processing or billing an already accepted request.

HTTP errors, including 429 rate limits, report status codes as terminal failures. The host decides whether and when to issue another request; `Retry-After` is not acted on automatically. Protocol errors use local descriptions and do not echo response bodies, argument values, API keys or endpoint URLs. SSE frames are limited to 8 MiB, total streamed response bytes to 64 MiB, and non-streamed Gemini JSON to 16 MiB. A response also permits at most 8,192 data frames, 4,096 content fragments and 128 tool calls, counted across all frames. Gemini counts each part, including empty text; Anthropic counts each content-block start and delta, including tool-argument fragments. Each JSON frame or assembled tool-argument object permits at most 65,536 value nodes and 128 container levels. Exceeding any bound emits one terminal error. These are adapter resource limits, separate from model token limits. A host can use its cancellation token to implement a deadline.

The adapter retains one bounded JSON tree plus the growing message and a constant number of event snapshots. It never buffers all of a Gemini frame's event snapshots. Because the normalized contract includes the whole partial message on every event, copying work and a consumer that retains all events can still grow with fragment count times message size. Consume and release events as they arrive; the adapter's byte/fragment/call limits bound its own retained state, not the host's event archive. JSON mode still waits for the full bounded HTTP body before parsing it; individual string decoding, schema validation and one message clone are synchronous bounded operations, not preemptible instructions.

## Messages and tools

System messages are folded using `cayw_ai::current_system`: prompt fragments and current named sections become one system instruction; tool additions, replacements and removals determine the active declarations. Custom host messages do not leave the host.

Supported conversation content is text and explicitly enabled user images. Set `Model.supports_images = true` only for a model the host has verified. Images must have nonempty standard base64 and an allowed MIME type: PNG, JPEG or WebP for both providers, plus GIF for Anthropic. The adapter validates base64 and MIME declaration, not the decoded image format. Assistant images, audio, documents and image tool results are rejected explicitly. Tool results support text plus `structured_content` JSON; host-only `details` are not transmitted. Anthropic sends structured JSON as a text block; Gemini sends it in `functionResponse.response.output` or `.error`, retaining accompanying text separately.

Tool declarations must have a valid local JSON Schema with an object root. Remote schema references are refused. Responses and historical tool calls must have complete object arguments satisfying the corresponding declaration, a nonempty ID and a declared name. Duplicate JSON keys, invalid JSON, partial argument extensions, duplicate IDs, missing results and mismatched result names fail. Historical calls are checked against the declarations in effect at that point in the transcript; new calls use the current folded declarations. The caller must retain the system declaration events alongside conversation history.

Anthropic streams `input_json_delta` fragments and validates the assembled object at `content_block_stop`. A zero-argument tool may use a complete empty `input` object without deltas. Gemini's default function calls contain complete `args` objects; omitted `args` means an empty object. This adapter does not enable Gemini's optional partial-argument streaming. Native Gemini IDs are preserved. Calls without IDs receive a unique per-response sequence ID, with collisions checked against the supplied transcript. Those IDs are retained in normalized history and sent on replay in both `functionCall.id` and `functionResponse.id`; parallel calls with the same name remain separate.

Every `outcome_unknown` tool result stops the next request before networking. Incomplete, failed or aborted assistant history is also rejected; the host must reconcile or replace it with valid paired history before continuing. A conversation must start with a user turn, and the request must end in a user or complete tool-result turn. This API does not implement assistant prefilling.

## Protocol capability matrix

All passing results here refer to the loopback fixtures in `tests/http.rs`, crate version 0.1.0, Rust 1.95.0 on macOS. No service/model capability is inferred from these fixtures.

| Capability | Anthropic Messages | Gemini streamGenerateContent | Gemini generateContent | Evidence |
| --- | --- | --- | --- | --- |
| Text and usage | Fixture passed | Fixture passed | JSON tool/usage fixture passed | `anthropic_streams_text_usage_and_native_request`, `gemini_streams_text_usage_and_native_request`, `gemini_generate_content_uses_json_endpoint_and_preserves_provider_call_id` |
| Tool call/result round trip | Fixture passed | Parallel same-name calls passed | Native/generated IDs and reordered success/error result replay passed | `anthropic_tool_round_trip_preserves_schema_ids_names_and_structured_results`, `gemini_parallel_same_name_calls_get_distinct_ids_and_replay_correct_names`, `gemini_generate_content_replays_paired_tool_results_with_original_ids` |
| User image mapping and system changes | Fixture passed | Fixture passed | Explicit image mapping and disabled-image rejection passed | `both_protocols_fold_system_changes_and_send_user_images_explicitly`, `gemini_generate_content_maps_explicit_images_and_rejects_disabled_images` |
| Cancel/drop before headers or during body | Fixture passed | Fixture passed | Partial JSON-body fixture passed; header waiting uses shared transport | `cancellation_and_drop_abort_waiting_for_response_headers`, `both_protocols_are_lazy_and_cancel_or_drop_the_network_body`, `gemini_json_mode_cancel_and_drop_close_a_partial_body` |
| HTTP/JSON/protocol failures, 429, no retry or redirect | Fixture passed | Fixture passed | Dedicated malformed, truncated, duplicate-key and trailing JSON fixtures passed | `both_protocols_report_bad_json_http_errors_and_unsupported_content_once`, `stream_protocol_errors_never_produce_a_success_terminal`, `both_protocols_never_retry_or_follow_redirects`, `gemini_generate_content_rejects_malformed_and_ambiguous_json_once` |
| Bounded large frames and parse cancellation | Shared framing/JSON and response-limit fixtures passed | 500/2,000/4,000 part allocation and cancellation fixtures passed | Same dedicated JSON fixtures passed | `tests/bounds.rs`, `gemini_large_frame_preserves_all_text_and_tool_events`, `gemini_same_thread_cancellation_interrupts_a_single_large_frame`, `response_fragment_and_tool_limits_cover_both_gemini_modes` |
| Thinking and signatures | Explicit rejection | Explicit rejection | Explicit rejection | `invalid_history_and_unsupported_inputs_fail_before_network`, `stream_protocol_errors_never_produce_a_success_terminal` |
| Real service, real images, Windows/Linux | Unverified | Unverified | Unverified | Explicit host authorization and additional platforms required |

## Stop reasons, usage and capability limits

Anthropic `end_turn` and `stop_sequence` map to `Stop`; `max_tokens` and `model_context_window_exceeded` map to `Length`; `tool_use` maps to `ToolUse` only with valid completed calls. The stream requires `message_start`, paired block events, a stop reason and `message_stop`. Gemini `STOP` maps to `ToolUse` when calls are present and otherwise `Stop`; `MAX_TOKENS` maps to `Length` only without incomplete tool calls. Gemini waits for stream EOF after `finishReason` so trailing usage metadata is retained. Missing terminal protocol events, empty supported content, safety blocks, unsupported finishes and unknown content produce `Error`.

Anthropic's `max_tokens` must be supplied through `StreamOptions.max_tokens` or `Model.max_output_tokens`. There is no guessed model default. Gemini maps this limit to `generationConfig.maxOutputTokens` when supplied. Temperature is optional and must be within the protocol range (Anthropic 0–1; Gemini 0–2); individual models may further restrict it.

Anthropic usage is cumulative: later counters replace only provided counters. Input, output, cache reads and cache writes remain separate. Gemini subtracts cached prompt tokens from normal input; its output includes candidate and thought tokens, with thought tokens also recorded as the reasoning subset. Gemini's supplied total is authoritative. Neither adapter invents prices or token counts when metadata is absent.

Thinking controls, thinking blocks and signatures are explicitly unsupported. Any non-`Off` `ThinkingLevel`, thinking history, returned thinking content or Gemini `thoughtSignature` produces a clear error. In particular, a Gemini model that requires replayed thought signatures for tool calls cannot complete a tool loop through this adapter. The shared `ToolCall` type has no signature slot; signatures are never stripped or replaced with a bypass marker. Server-side tools, grounding, code execution, Vertex AI authentication, Anthropic thinking/redacted-thinking and model-specific parameter discovery are outside this crate's supported protocol surface.

## Reproducible validation

Run from the repository root with Rust 1.95:

```sh
/Users/chengwen/.cargo/bin/cargo +1.95.0 test -p cayw-providers --locked
/Users/chengwen/.cargo/bin/cargo +1.95.0 check -p cayw-providers --locked
/Users/chengwen/.cargo/bin/cargo +1.95.0 clippy -p cayw-providers --all-targets --locked -- -D warnings
```

The 33 tests in `tests/http.rs` run real loopback TCP HTTP servers with synthetic, documented protocol fixtures. They verify text fragments before response completion, tool round trips, parallel function IDs, schema transmission, folded system changes, user image mapping, cumulative usage, JSON and HTTP failures, incomplete streams, cancellation and stream drop while awaiting headers or SSE/JSON body and between tool events, response size/fragment/call/frame limits, and no retry or redirect (including HTTP 429 and 503). Three parser unit tests cover strict JSON, cooperative cancellation inside arrays/strings, and node/depth limits. No test calls an external model endpoint or reads a real key.

`tests/bounds.rs` runs alone in a separate test executable with an allocation counter, so concurrent tests do not contaminate its retained/peak measurements. It exercises both Gemini modes with 500, 2,000 and 4,000 text parts, consumes only the first delta, and cancels. On macOS/Rust 1.95.0, a 2,000-part SSE fixture (88,085 bytes) retained 64,954,094 bytes before this fix and 1,426,389 bytes afterward; the 4,000-part fixture retained 2,850,261 bytes. JSON mode retained 1,475,043 and 2,964,577 bytes respectively. These are observed allocations above the test baseline, not RSS or universal byte-exact bounds; the regression uses a generous 8 MiB threshold. It also injects cancellation at a fixed poll checkpoint during large JSON decoding over TCP, before an invalid trailing suffix can be reached. The tests assert work/content boundaries, not fixed millisecond performance.

Validated environment: macOS (Darwin), Rust 1.95.0. Real Anthropic and Gemini service behavior, real images, paid calls, Windows and Linux remain **unverified**. Passing fixture tests is not Issue 10's real-provider or three-platform acceptance evidence.

Protocol references consulted through Context7:

- [Anthropic Messages and streaming types](https://github.com/anthropics/anthropic-sdk-typescript/blob/main/src/resources/messages/messages.ts)
- [Anthropic streaming usage tests](https://github.com/anthropics/anthropic-sdk-typescript/blob/main/tests/api-resources/MessageStream.test.ts)
- [Gemini GenerateContent REST reference](https://ai.google.dev/api/generate-content)

Dependencies use the existing workspace stack plus `base64` 0.22 (MIT OR Apache-2.0). No provider SDK, Codex dependency, credential loader or general execution tool is introduced.
