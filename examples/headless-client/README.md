# Headless Rust client

`cayw-client` is an independent Cargo workspace and an installable consumer of the public Runtime SDK and authenticated local IPC. It has no UI, plugin, remote Server or VerifyTool dependency. Use Rust 1.95+ on macOS. Keep the nested `Cargo.lock`; its registry package identities match the root lock.

The [headless Chat guide](../../docs/headless-chat.md) includes isolated installation paths, a complete [host configuration](host.example.json), two user turns, approvals, cancellation and the installed offline regression command. The [Issue #6 validation record](../../docs/validation/issue-6-headless-chat-macos.md) separates local fixture results, authorized real-model results and their verified limits.

From the repository root:

```sh
cargo +1.95.0 install --path crates/cayw-runtime --bin cayw --no-default-features --locked
cargo +1.95.0 install --path examples/headless-client --locked
```

The Runtime server and SDK client use the same `HeadlessConfig` JSON. Model execution requires its `model` object. Copy [host.example.json](host.example.json), set the authorized service, API, exact model ID, base URL, API version and positive output limit, then provide the credential only through the environment variable named by `model.api_key_env`. The example selects Magpie's `group/auto-deepseek-v4-1-flash` route. OpenAI, Anthropic and both Gemini modes use the same configuration entry; the [guide](../../docs/headless-chat.md#连接模型服务和本机-ipc) lists the supported combinations. The host may select any valid HTTP(S) deployment hostname, port and path prefix, including `localhost`. Default adapters honor environment proxy settings and `NO_PROXY`, disable automatic redirects and retries, and retain TLS certificate verification. Only SDK `prompt` reads the selected model credential. SDK query and control commands need no model key. IPC clients read only their IPC token; the server owns model settings. The old three-variable model setup no longer selects or overrides a model. Starting the host or creating a Session does not send a model request. A `prompt` does. Real model requests need the user's authorization and budget.

To run through IPC, start the installed host. The workspace directory must exist, and the database parent must exist. Supply an IPC token of at least 16 bytes through an environment variable such as `CAYW_IPC_TOKEN`.

```sh
cayw serve --db /absolute/state/session.sqlite --workspace /absolute/workspace --listen 127.0.0.1:4000 --token-env CAYW_IPC_TOKEN --config /absolute/state/host.json
cayw-client --mode ipc --address 127.0.0.1:4000 --token-env CAYW_IPC_TOKEN create --label "Local work"
cayw-client --mode ipc --address 127.0.0.1:4000 --token-env CAYW_IPC_TOKEN prompt --session SESSION_ID --text "Carry out the authorized task" --cursor-file /absolute/state/events.cursor.json
```

The CLI host sets its IPC request timeout to the configured task execution limit plus 60 seconds. The default 300-second task limit gives a 360-second IPC timeout; `budgets.max_run_time_ms: 900000` gives 960 seconds. The host reports the actual value as `runtime.health.request_timeout_ms`. Before executing a command, the IPC client reads this value over a short-lived authenticated connection, closes that connection, and applies the value to its execution and observation connections. Missing or invalid values fail before task admission. There is no separate timeout CLI option. Task limits, Host call limits, cancellation and disconnect cleanup still apply. A custom IPC host must report its own actual timeout; the general Runtime SDK health value does not infer transport settings.

A prompt reports each pending approval as an event and waits. Use another client invocation to make an explicit trusted decision:

```sh
cayw-client --mode ipc --address 127.0.0.1:4000 --token-env CAYW_IPC_TOKEN approve --session SESSION_ID --approval APPROVAL_ID --decision allow_once
```

Other decisions are `allow_task` and `deny`. `allow_task` applies only to the original exact capability and argument scope during its original task. Model text cannot approve an operation. To send a second user turn, invoke `prompt` again with the same Session ID and the saved cursor file. The Runtime retains the original conversation and tool results.

SDK mode runs the same Runtime in the client process. Give the same database, workspace and any configured host policy to each invocation. Only `prompt` requires model configuration and its credential:

```sh
cayw-client --mode sdk --db /absolute/state/session.sqlite --workspace /absolute/workspace --config /absolute/state/host.json create
cayw-client --mode sdk --db /absolute/state/session.sqlite --workspace /absolute/workspace --config /absolute/state/host.json prompt --session SESSION_ID --text "Carry out the authorized task" --cursor-file /absolute/state/events.cursor.json
```

`--config` loads the public `HeadlessConfig`. SDK `create`, queries, approval, cancellation and recovery do not require a `model` object or read its credential. They open the persisted Runtime with a provider that fails explicitly if model execution is unexpectedly attempted. Without `--config`, host settings use Runtime defaults; supply the existing host policy when opening a database that uses other settings. SDK `prompt` requires both the configured model and its selected credential before task admission. Host settings omitted from the file retain their Runtime defaults. Unknown or contradictory model settings fail before the model credential is read or a database is created. Permission and process profiles belong to this trusted host configuration. They cannot be supplied in model tool arguments. The client does not read Runtime SQL tables or inspect a private Core database.

All commands are shown by `--help`. `get`, `tasks`, `audit`, `events`, `cancel` and `recover` require `--session`. `cancel` also requires `--task TASK_ID`, captured from the original prompt's `task` record. It reads that exact task, checks its Session, and sends its complete cancellation identity. A delayed cancellation never selects a newer task in the same Session. `events` accepts `--after`, `--limit` (1–1000), `--wait-ms` (0–30000) and `--cursor-file`. It reads one bounded page. Call it until the page is empty to read the current suffix. IPC also has a default 1 MiB frame limit; a valid item count can still exceed it. When an event page is too large, the client halves its requested item count at the same cursor until a page fits. This applies to `events` and prompt event reads. If a single event cannot fit, the read fails without advancing its cursor. Never resubmit a prompt to recover a read. `audit` accepts `--after` and `--limit`; continue from the last audit cursor to inspect original process and cleanup records. Audit cursors can have gaps within one Session; event cursors are contiguous. Recovery is explicit and does not replay prompts or clear unknown effects.

Machine output is one JSON object per line:

- `runtime`: nonsecret connection/model metadata.
- `result`: `{command, session_id, result}` with the original public API result.
- `event`: `{session_id, cursor, event}` where `event` is the complete `EventRecord`.
- `task`: the original queued task and its later state.
- `prompt_finished`: `{session_id, session, task, after}` after reading the final event suffix.
- `cancel_requested` and `cancel_ack`: separate observations of cancellation intent and its acknowledgment.

Errors go to stderr as `{type:"error", command, session_id, code, message, outcome_unknown}` and exit nonzero. A successful SDK/RPC return alone does not mean a task completed: `prompt` checks its original task and exits nonzero for failed, cancelled, draining or unknown tasks. Usage comes from the original model messages; missing usage is unknown, and a zero numeric field does not prove zero cost.

`prompt` first validates and reads the saved event suffix. It then admits one task and executes it once. It watches events through a separately opened Runtime or authenticated IPC connection. It validates the complete page for contiguous cursors before emitting it. After the page has been written and stdout flushed, it atomically replaces and syncs the cursor file. A failed output or cursor write can replay an already printed page after an explicit restart; the saved cursor never silently advances past an incomplete page. The caller must consume stdout successfully before treating the page as processed. A cursor belonging to another Session is rejected before task admission.

Ctrl+C retains the original Task ID, waits to observe that task's claimed Worker/Attempt and queue fence, sends the complete `CancelTarget`, keeps the original execution future alive, then reads the original task and final event suffix. It never sends the initial unclaimed fence 0 while `resume` may claim the task. The acknowledgment includes the exact target. Identity changes return `stale_cancellation`; the client does not retarget or replay. A cancellation acknowledgment is not process cleanup completion. Failed cleanup remains visible as draining or unknown and is not reported as successful. An IPC execution connection failure is returned without opening a replacement or resending the task. A new explicit client invocation can inspect its Session, tasks, audit and events.

The package tests use a local HTTP server with fixed OpenAI-compatible streaming responses. Those responses exercise the real HTTP adapter, Runtime, Luau, Host file operations and authenticated IPC. They are offline fixtures, not real Magpie evidence:

```sh
cargo +1.95.0 test --manifest-path examples/headless-client/Cargo.toml --all-targets --locked --offline
cargo +1.95.0 clippy --manifest-path examples/headless-client/Cargo.toml --all-targets --locked --offline -- -D warnings
cargo +1.95.0 fmt --manifest-path examples/headless-client/Cargo.toml -- --check
```
