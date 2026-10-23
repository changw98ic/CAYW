# Independent Issue 1 client

This executable is a downstream consumer of the public CAYW Rust SDK and IPC. It is a separate nested Cargo workspace with its own retained `Cargo.lock`; the root product workspace does not include it. Its path dependencies consume the real Runtime, Core, Cells, Luau and Host implementations. It does not depend on `cayw-verify-tool`, a UI, the optional Server/plugin features or Codex.

Every Provider response and approval choice is a fixed synthetic fixture. File writes/reads are performed by the real Host. The process-interruption scenario uses an explicitly injected local synthetic service, writes and syncs its receipt, then deliberately leaves the request incomplete. No model service, user credential, paid API or external search service is called. The IPC authentication token is public fixture data.

From the source root, use Rust 1.95.0 and a fresh output directory. The following are the exact package commands (the executable requires an absolute output path and creates it exclusively):

```sh
export CARGO_TARGET_DIR="$PWD/target-issue1"
export TMPDIR=/tmp/
export CARGO_NET_OFFLINE=true
cargo +1.95.0 check --manifest-path examples/issue1-client/Cargo.toml --all-targets --offline --locked
cargo +1.95.0 test --manifest-path examples/issue1-client/Cargo.toml --all-targets --offline --locked
cargo +1.95.0 clippy --manifest-path examples/issue1-client/Cargo.toml --all-targets --offline --locked -- -D warnings
cargo +1.95.0 fmt --manifest-path examples/issue1-client/Cargo.toml -- --check
cargo +1.95.0 run --manifest-path examples/issue1-client/Cargo.toml --offline --locked -- all /tmp/cayw-issue1-run-new
```

Offline commands require the frozen dependencies to be present in the local Cargo cache. The nested lock's registry version/source/checksum identities match the frozen root lock. Keep both locks; do not generate a new lock or run `cargo update` as part of reproduction. This client adds only Tokio's standard-input feature to the preexisting manifest so its child host can stop through an explicit pipe command.

To run a narrower public example:

```sh
cargo +1.95.0 run --manifest-path examples/issue1-client/Cargo.toml --offline --locked -- sdk /tmp/cayw-issue1-sdk-new
cargo +1.95.0 run --manifest-path examples/issue1-client/Cargo.toml --offline --locked -- ipc /tmp/cayw-issue1-ipc-new
```

`all` runs six scenarios; `sdk` runs the first, and `ipc` runs the remaining five. Output files use `create_new` so evidence from a previous attempt cannot be overwritten.

| Scenario directory | Actual checks |
| --- | --- |
| `sdk` | Public SDK creates Sessions, starts actual prompt requests, reads events while active, and applies explicit synthetic deny/cancel/allow choices. Cancellation uses a separately opened Runtime on the same database; the original task and its single audit terminal must both report cancellation. Late allow is rejected before joining the worker. An explicit replacement task needs fresh permission, a different worker/attempt and a higher fence; denying it causes no file effect. Allow once writes and separately approves the read. |
| `ipc` | The same lifecycle via authenticated TCP `Client` requests and the real Runtime `RpcHandler`. Cancellation goes through a second authenticated server backed by the separately opened peer Runtime, with the same terminal, late approval and replacement checks. |
| `reconnect` | Process a contiguous prefix, sync the saved Session/cursor file and directory before disconnect, reconnect, read exactly the suffix in bounded pages, compare it and the full log, verify one original prompt/task. The event and control connections have distinct ownership. |
| `transport` | Explicit incompatible IPC frame v2 gets `unsupported_version` from v1 and EOF; bad token rejection is saved with unchanged Session/Provider facts. Authenticated requests with an extra prompt field, missing prompt text and zero event-page limit retain their original parameters, exact rejection and before/after Session, tasks, events, permissions, Provider and file observations. Client/server capacity and oversized-frame rejection remain known unexecuted; disconnect an active prompt and separately inspect durable cancellation. Raw requests on the real Runtime connection prove distinct original/cancel/late-cancel/subsequent response pairing. |
| `restart-before` | Spawn this executable in `host` mode, hold at actual Host approval, kill the child and wait for confirmed exit before reopen. Original logical Session/task/worker facts remain. Explicit recovery marks unstarted work not executed, and an explicit new task gets a new attempt/worker and higher fence in the same Session. |
| `restart-unknown` | Child passes actual approval, enters real Host dispatch to the local synthetic service and syncs a receipt. Kill/wait/reopen preserves original started capability as unknown; new prompt/fork is blocked, repeated recovery cannot clear it, no effect is replayed. Explicit public reconciliation uses the actual original receipt to repair Core and Cells. |

Cancellation retains the original `task.enqueue` receipt, reads that exact Task after claim and sends its full `CancelTarget` over SDK or authenticated IPC. The queue fence is separate from the Core lease fence. A terminal target returns false and cannot select a replacement Task.

The unknown fixture explicitly installs a `ReconciliationVerifier`. It reads the saved original Session, synced effect receipt and confirmed child-exit facts. It accepts only the fixed `rpc-user` actor, exact original `expected` identity and the receipt's exact known outcome; all other tool/capability repairs are rejected. Ordinary Runtime configurations have no verifier and reject reconciliation. These local fixture files are not a receipt format for an external service.

Reopened hosts stop through their input pipe, return actual Provider/service invocation counters and are awaited to exit. The unknown scenario requires both counters to remain zero through reopen, recovery, rejected new work and explicit reconciliation. The before-dispatch scenario requires a Provider call only for the explicitly requested replacement task. A live owner's recovery is rejected before the original child is killed.

The top-level `acceptance.json` exists only after every selected scenario passes. Raw event pages, Session views, task records, synced cursor, original response errors, child ready/exit facts and synthetic receipt are retained beside each `result.json`. Timeouts are watchdogs for missing progress, never fixed millisecond performance assertions. The parent uses notifications/semaphores, durable events and pipe markers to coordinate the demonstration.

Package tests reject changed tool name/arguments/ID, duplicate tool terminals and results sent with another attempt/worker lease through public Core APIs. They assert rejected writes do not advance the cursor or change the view. Cursor negative checks reject skipped, repeated or foreign-Session events. Existing Core/IPC/Runtime tests provide broader transport and lifecycle coverage.

See the complete [SDK and IPC contract](../../docs/issue1-sdk-ipc-contract.md) for the identity/state rules, error handling, bounded resources and disconnect semantics. Core/store version is v3; IPC outer-frame version is v1. The old Issue comment's JSONL/Unix socket sketch is superseded by the canonical SQLite/authenticated-loopback implementation.

The first release targets macOS only, following the user's 2026-10-19 decision. This client's Issue #1 functional acceptance is complete locally on macOS ARM64 with Rust 1.95.0. Windows and Linux are outside the current release scope; no adaptation or validation is required for this delivery. Other macOS architectures, real model calls, full Issue #2 and Issue #6 acceptance are not covered here. A child process kill is not a power-loss durability test. GitHub Issue #1 was closed at the user's request on 2026-10-19; [Issue #19](https://github.com/changw98ic/CAYW/issues/19) tracks later Linux and Windows support. This client, the required product workspace and its acceptance documentation were merged into `main` through PR #20. Raw execution artifacts remain in the documented local evidence directories.
