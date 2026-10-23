# cayw-providers

Implements `cayw_ai::Provider` for native OpenAI Responses, Anthropic Messages and the Gemini Developer API. OpenAI Chat Completions has one implementation, `cayw_ai::providers::OpenAiProvider`; this crate does not duplicate it.

## Public API

- `ResponsesProvider::new(base_url, api_key) -> Result<Self, ProviderConfigError>` appends `/responses`. `standard(api_key)` uses `https://api.openai.com/v1`. It sends native Responses input and tool items, preserves `item.id` separately from `call_id`, and requires `response.completed` before success.
- `AnthropicProvider::new(base_url, api_key) -> Result<Self, ProviderConfigError>` appends `/messages`. `standard(api_key)` uses `https://api.anthropic.com/v1` and sends `anthropic-version: 2023-06-01`.
- `GeminiProvider::new(base_url, api_key) -> Result<Self, ProviderConfigError>` defaults to `/models/{model}:streamGenerateContent?alt=sse`. `standard(api_key)` uses `https://generativelanguage.googleapis.com/v1beta`.
- `GeminiProvider::with_mode(GeminiMode::GenerateContent)` selects `/models/{model}:generateContent` and translates the complete JSON response into the same event sequence. `GeminiMode::StreamGenerateContent` selects streaming. Model IDs accept `name` or `models/name`.
- `GeminiProvider::with_gateway_routes()` explicitly permits a safe `provider/model` route, optionally prefixed by `models/`. The default accepts one model segment. Neither form accepts traversal, encoded separators, query parameters or fragments. Runtime enables this only for `service = magpie`.
- All three implement `stream(ModelRequest, CancellationToken) -> AssistantStream`. Creating the stream performs no network work. A consumed stream starts with `Start` and ends with exactly one `Done` or `Error`.
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

Keys are explicit, nonempty HTTP header values. Responses uses `Authorization: Bearer`; Anthropic uses `x-api-key`; Gemini uses `x-goog-api-key`, never URL query credentials. Base URLs must be HTTP(S), without embedded credentials, a query or a fragment. Existing base paths are retained. The built-in client disables proxies, redirects and retries. It does not read local model authentication settings. There is no detached networking task or hidden retry loop.

Cancellation is checked before sending, while waiting for response headers, while reading response bytes, during JSON scanning, and between content parts and normalized events. SSE framing scans at most 4 KiB before yielding to the runtime; JSON scanning yields every 4 KiB or 64 structural steps. Gemini produces one event per consumer poll, including each text delta and the separate tool start/delta/end events. Empty parts also pass through cancellation checks. Poll the stream to observe cancellation as `Error` with `StopReason::Aborted`. Dropping the stream drops its pending HTTP futures, parser and response body. Anthropic releases its response on `message_stop`, and Responses on a validated `response.completed`, without waiting for HTTP EOF. As with any remote operation, closing the connection cannot promise that a service has stopped processing or billing an already accepted request.

HTTP errors, including 429 rate limits, report status codes as terminal failures. The host decides whether and when to issue another request; `Retry-After` is not acted on automatically. Protocol errors use local descriptions and do not echo response bodies, argument values, API keys or endpoint URLs. SSE frames are limited to 8 MiB, total streamed response bytes to 64 MiB, and non-streamed Gemini JSON to 16 MiB. A response also permits at most 8,192 data frames, 4,096 content fragments and 128 tool calls, counted across all frames. Gemini counts each part, including empty text; Anthropic counts each content-block start and delta, including tool-argument fragments. Each JSON frame or assembled tool-argument object permits at most 65,536 value nodes and 128 container levels. Exceeding any bound emits one terminal error. These are adapter resource limits, separate from model token limits. A host can use its cancellation token to implement a deadline.

The shared strict JSON parser also limits a non-string scalar token to 4 KiB. OpenAI/Magpie additionally limit retained content to 8 MiB and cumulative full-message snapshot copying to 64 MiB; the check runs before copying. These byte/work bounds do not estimate model tokens.

The adapter retains one bounded JSON tree plus the growing message and a constant number of event snapshots. It never buffers all of a Gemini frame's event snapshots. Because the normalized contract includes the whole partial message on every event, copying work and a consumer that retains all events can still grow with fragment count times message size. Consume and release events as they arrive; the adapter's byte/fragment/call limits bound its own retained state, not the host's event archive. JSON mode still waits for the full bounded HTTP body before parsing it; individual string decoding, schema validation and one message clone are synchronous bounded operations, not preemptible instructions.

## Messages and tools

System messages are folded using `cayw_ai::current_system`: prompt fragments and current named sections become one system instruction; tool additions, replacements and removals determine the active declarations. Custom host messages do not leave the host.

Supported conversation content is text and explicitly enabled user images. Set `Model.supports_images = true` only for a model the host has verified. Images must have nonempty standard base64 and an allowed MIME type: PNG, JPEG or WebP for both providers, plus GIF for Anthropic. The adapter validates base64 and MIME declaration, not the decoded image format. Assistant images, audio, documents and image tool results are rejected explicitly. Tool results support text plus `structured_content` JSON; host-only `details` are not transmitted. Anthropic sends structured JSON as a text block; Gemini sends it in `functionResponse.response.output` or `.error`, retaining accompanying text separately.

Tool declarations must have a valid local JSON Schema with an object root. Remote schema references are refused. Responses and historical tool calls must have complete object arguments satisfying the corresponding declaration, a nonempty ID and a declared name. Duplicate JSON keys, invalid JSON, partial argument extensions, duplicate IDs, missing results and mismatched result names fail. Historical calls are checked against the declarations in effect at that point in the transcript; new calls use the current folded declarations. The caller must retain the system declaration events alongside conversation history.

Anthropic streams `input_json_delta` fragments and validates the assembled object at `content_block_stop`. A zero-argument tool may use a complete empty `input` object without deltas. Gemini's default function calls contain complete `args` objects; omitted `args` means an empty object. This adapter does not enable Gemini's optional partial-argument streaming. Native Gemini IDs are preserved. Calls without IDs receive a unique per-response sequence ID, with collisions checked against the supplied transcript. Those generated IDs pair calls and results inside CAYW. Replay preserves the original wire ID when one existed and leaves it absent when it was absent. Results are sent in the original call order, including when tools finished in reverse order. Gemini tool history without its original parts is rejected before networking; text-only history can be mapped without metadata. Parallel calls with the same name remain separate.

Every `outcome_unknown` tool result stops the next request before networking. Incomplete, failed or aborted assistant history is also rejected; the host must reconcile or replace it with valid paired history before continuing. A conversation must start with a user turn, and the request must end in a user or complete tool-result turn. This API does not implement assistant prefilling.

## Protocol capability matrix

The four-format matrix below records adapter behavior verified by local TCP HTTP fixtures, crate version 0.1.0, Rust 1.95.0 on macOS. Fixture results do not establish a remote model's capabilities. Exact Magpie models, paths, real task inputs/configuration and source/build receipts are linked in the [service matrix](../../docs/validation/issue-10-provider-protocols-macos.md#服务api-与真实任务矩阵).

| Capability / limit | Chat Completions | Responses | Anthropic Messages | Gemini streamGenerateContent | Local evidence |
| --- | --- | --- | --- | --- | --- |
| Streaming text, stop reasons and usage | Passed; terminal sentinel required | Passed; `response.completed` required | Passed; `message_stop` required | Passed; valid `finishReason` and stream EOF required | `cayw-ai/tests/openai.rs`; `tests/responses.rs`; `tests/http.rs` |
| Tool arguments and paired results | JSON fragments, original call IDs and order | Interleaved JSON fragments; item IDs separate from call IDs | `input_json_delta`, original IDs and block order | Complete `functionCall.args`; optional partial-argument streaming is not enabled | Above files; `cayw-runtime/tests/provider_pairing.rs` |
| Native history and hidden content | Explicit Magpie routing permits unsigned `reasoning_content`; foreign metadata rejected | Original completed output items and encrypted reasoning replayed | Original thinking/signature/redacted blocks replayed | Original model parts, absent/native wire IDs and opaque signatures replayed | `tests/responses.rs`; `tests/anthropic_thinking.rs`; `tests/gemini_gateway.rs` |
| Cancellation and dropping streams | Before request, headers and body | Headers, body and tool events | Headers, body and signed blocks | Headers, body, parsing and part events | `cayw-ai/tests/openai.rs`; `tests/responses.rs`; `tests/http.rs`; `tests/anthropic_thinking.rs` |
| Errors, 429, truncation, bounds; no retry or redirect | Passed | Passed | Passed | Passed | `tests/network_lifecycle.rs`; protocol tests; `tests/bounds.rs` |
| Unsupported capabilities | Images/thinking require explicit model/service support; unsupported content fails | Visible reasoning and unsupported output items fail | Explicit thinking controls, non-direct callers and server-side tools fail | Visible thinking, grounding, code execution and Vertex authentication fail | Protocol rejection tests; limits below |
| Real task scope | Exact DS Flash route through SDK on r02 | Exact GPT-6 Luna route through authenticated IPC on r02 | Exact Haiku route through SDK on r03 | Exact Gemini route through authenticated IPC on r02 | Per-row source/input/config/build links in the service matrix |
| Official direct endpoints, other models and real images | Unverified | Unverified | Unverified | Unverified | No such support claim follows from a fixture or gateway result |

Gemini `generateContent` is an additional local JSON mode, outside the four real-task closing rows. Its fixtures cover text/tools/usage, native/generated IDs, reordered success/error results, explicit image mapping and disabled-image rejection, malformed/truncated/duplicate-key/trailing JSON, partial-body cancellation/drop and shared response limits (`tests/http.rs`, `tests/bounds.rs`, `tests/gemini_gateway.rs`). Both Gemini modes preserve the same original parts and signature boundary. Native user-image and system-change mappings are fixture-tested; real image behavior remains unverified.

## Stop reasons, usage and capability limits

Anthropic `end_turn` and `stop_sequence` map to `Stop`; `max_tokens` and `model_context_window_exceeded` map to `Length`; `tool_use` maps to `ToolUse` only with valid completed calls. The stream requires `message_start`, paired block events, a stop reason and `message_stop`. Gemini `STOP` maps to `ToolUse` when calls are present and otherwise `Stop`; `MAX_TOKENS` maps to `Length` only when no `functionCall` is present. Any `functionCall` with `MAX_TOKENS`, including one with complete arguments, is rejected. Gemini waits for stream EOF after `finishReason` so trailing usage metadata is retained. Missing terminal protocol events, empty supported content, safety blocks, unsupported finishes and unknown content produce `Error`.

Anthropic's `max_tokens` must be supplied through `StreamOptions.max_tokens` or `Model.max_output_tokens`. There is no guessed model default. Gemini maps this limit to `generationConfig.maxOutputTokens` when supplied. It omits `candidateCount`, which Gemini 3 and later do not support; the service's default candidate count applies. Temperature is optional and must be within the protocol range (Anthropic 0–1; Gemini 0–2); individual models may further restrict it.

Anthropic usage is cumulative: later counters update only fields that are present, and a counter cannot decrease. Input, output, cache reads and cache writes remain separate. Gemini subtracts cached prompt tokens from normal input; its output includes candidate and thought tokens, with thought tokens also recorded as the reasoning subset. A supplied Gemini total must agree with the complete counters. Counter types, cache/reasoning subsets, addition overflow and inconsistent totals are checked.

`UsageStatus::Unknown` means no usable counters were reported; `Partial` means some required counters are absent; `Reported` requires explicit input and output, including explicit zero; `Invalid` records malformed or inconsistent counters. These states do not establish a settled bill. Known raw counter fields are retained in `AssistantMessage.usage_fields`, including rejected values. `UsageUpdate` emits received usage before the final protocol event, so cancellation and later errors retain the observations already received. No adapter invents a price or replaces missing counts with a claim of zero usage.

Explicit thinking controls and visible thinking content remain unsupported. Anthropic stores the accumulated original content blocks in `provider_metadata["anthropic"].content`. Empty thinking text is valid; complete thinking blocks require a nonempty opaque signature. Thinking text deltas append; signature deltas replace, following the native Messages accumulator. Redacted blocks keep their opaque `data`. These blocks never become normalized `Content::Thinking` or answer text. Replay preserves their block order and checks normalized text and tool IDs, names and arguments against the saved message. This validates format and mapping, not cryptographic authenticity. Truncated blocks, text changes after a signature, missing signatures and missing `message_stop` fail. A thinking-only response is not a completed visible answer. Gemini stores original model parts, including opaque `thoughtSignature` values, in `AssistantMessage.provider_metadata["gemini"]`; it keeps part boundaries and order and records the normalized call IDs separately. The adapter validates metadata structure and its agreement with normalized text/calls before replay. A signature must have a nonempty standard base64 representation; this is format validation, not authentication of its contents. `thought: true` is rejected. No bypass marker is generated.

Responses stores original completed output items in `provider_metadata["openai_responses"]`, including item IDs and opaque `encrypted_content` on reasoning items. It requests `store: false` and `include: ["reasoning.encrypted_content"]`. Replay requires those original items and checks their normalized text and calls against the saved message. Reasoning is not emitted as visible thinking. Tool results use `function_call_output.call_id`; item IDs are never substituted for call IDs. Missing completion, failed/incomplete responses and refusal content produce an error. Usage retains nested raw counters, separates cached input and treats reasoning as part of output.

Each native adapter accepts only its own metadata key. Chat Completions rejects nonempty protocol metadata. Server-side tools and non-direct Anthropic tool callers, grounding, code execution, Vertex AI authentication and model-specific parameter discovery are outside this crate's supported protocol surface.

## Reproducible validation

Run from the repository root with Rust 1.95:

```sh
/Users/chengwen/.cargo/bin/cargo +1.95.0 test -p cayw-providers --locked
/Users/chengwen/.cargo/bin/cargo +1.95.0 check -p cayw-providers --locked
/Users/chengwen/.cargo/bin/cargo +1.95.0 clippy -p cayw-providers --all-targets --locked -- -D warnings
```

`tests/http.rs` runs real loopback TCP HTTP servers with synthetic protocol fixtures. They verify text fragments before response completion, tool round trips, parallel function IDs, schema transmission, folded system changes, user image mapping, usage presence and consistency, JSON and HTTP failures, incomplete streams, cancellation and stream drop while awaiting headers or SSE/JSON body and between tool events, response size/fragment/call/frame limits, and no retry or redirect (including HTTP 429 and 503). Both native adapters use `cayw_ai::json::parse_response`; the strict parser and its unit tests have one implementation in `cayw-ai`. It rejects duplicate fields, checks node/depth limits, and yields during large arrays and strings. No test calls an external model endpoint or reads a real key.

`tests/anthropic_thinking.rs` checks empty and nonempty thinking text, signature replacement, opaque redacted data, serialization and multi-turn tool replay, malformed/missing signatures, block order, truncated streams, cancellation/drop, fragment bounds and invalid history rejection before networking. `cayw-runtime/tests/provider_protocols.rs` includes signed and redacted blocks in its Anthropic two-user-turn case, performs real file capabilities and reopens the same persisted session between user turns.

`tests/responses.rs` checks native Responses SSE with interleaved argument fragments, distinct item/call identities, text and nested usage, original encrypted reasoning replay, malformed/failed/incomplete streams, cancellation, drop and resource limits. `tests/gemini_gateway.rs` checks signed-parts replay in both Gemini modes, absent/native wire IDs, reverse-completed results, invalid metadata, missing original tool parts and explicit gateway routes. These tests use loopback fixtures and synthetic keys.

`tests/network_lifecycle.rs` additionally checks the original five HTTP configurations, including OpenAI and Magpie using the existing Chat adapter. It checks single-request behavior, terminal handling across network write sizes, and truncated HTTP bodies. `cayw-runtime/tests/provider_pairing.rs` checks two actual fixture file writes whose tools finish in reverse order: the next HTTP request must retain the original call order, IDs, arguments and both text and structured results. That test uses the public Agent; Runtime authorization and two-user-turn execution are covered separately in `cayw-runtime/tests/provider_protocols.rs`.

`tests/bounds.rs` runs alone in a separate test executable with an allocation counter, so concurrent tests do not contaminate its retained/peak measurements. It exercises both Gemini modes with 500, 2,000 and 4,000 text parts, consumes only the first delta, and cancels. On macOS/Rust 1.95.0, a 2,000-part SSE fixture (88,085 bytes) retained 64,954,094 bytes before this fix and 1,426,389 bytes afterward; the 4,000-part fixture retained 2,850,261 bytes. JSON mode retained 1,475,043 and 2,964,577 bytes respectively. These are observed allocations above the test baseline, not RSS or universal byte-exact bounds; the regression uses a generous 8 MiB threshold. It also injects cancellation at a fixed poll checkpoint during large JSON decoding over TCP, before an invalid trailing suffix can be reached. The tests assert work/content boundaries, not fixed millisecond performance.

Recorded local environment: macOS 27.0.1 ARM64, Rust 1.95.0. Other macOS architectures remain **unverified**. The first release targets macOS; Linux and Windows are outside Issue 10's acceptance scope. Fixture results only prove their recorded source and scenarios. Official direct services, real images and other models remain **unverified**.

The user approved four exact Magpie gateway rows as Issue 10's acceptance scope on 2026-10-23. Authorized real tasks completed two user turns per row: Chat Completions / `opencode-go/deepseek-v4.1-flash` through SDK; Responses / `codex/gpt-6-luna` through authenticated IPC; Messages / `factory/claude-haiku-5-5` through SDK with version `2023-06-01`; and Gemini streaming / `antigravity/gemini-3.8-flash` through authenticated IPC. See the [service matrix and source-bound records](../../docs/validation/issue-10-provider-protocols-macos.md).

The original r02 run remains three successes and one Haiku failure (15 real requests). A separate r03 Haiku retry completed both turns (6 requests) and replayed three original signed empty thinking blocks, including across user turns. The original 126857-token reservation remains held; the retry settled 18542 reported tokens with no new outstanding reservation. Monetary cost remains unknown. Source-impact review found the r02-to-r03 implementation changes confined to Anthropic; the other three real rows were executed on r02 and were not rerun on r03. No official direct endpoint, old Magpie group route or unrecorded protocol/transport combination is inferred from these results.

Protocol references consulted through Context7:

- [Anthropic Messages and streaming types](https://github.com/anthropics/anthropic-sdk-typescript/blob/main/src/resources/messages/messages.ts)
- [Anthropic streaming usage tests](https://github.com/anthropics/anthropic-sdk-typescript/blob/main/tests/api-resources/MessageStream.test.ts)
- [Anthropic thinking/signature accumulation](https://github.com/anthropics/anthropic-sdk-typescript/blob/main/src/lib/MessageStream.ts)
- [Anthropic original thinking block replay](https://github.com/anthropics/anthropic-sdk-python/blob/main/src/anthropic/types/thinking_block_param.py)
- [Gemini GenerateContent REST reference](https://ai.google.dev/api/generate-content)

Dependencies use the existing workspace stack plus `base64` 0.22 (MIT OR Apache-2.0). No provider SDK, Codex dependency, credential loader or general execution tool is introduced.
