# CAYW VerifyTool

VerifyTool is a reusable test consumer of the public Runtime SDK, authenticated local IPC, Luau CodeMode and Host. It runs real local operations against SQLite and workspaces, retains the original observations, and reads or compares the resulting reports. It is not a Runtime dependency. Its built-in Provider supplies fixed offline responses; these commands do not call a real model.

The current delivery and validation scope is the macOS host and architecture recorded in each run. A passing run does not establish another architecture or all of Issue #2.

## Run and read reports

Use Rust/Cargo 1.95 or newer from the repository root. Choose new output directories whose parents already exist. On macOS, a short `TMPDIR` avoids local socket path limits. Keep build output in an external `CARGO_TARGET_DIR` when disk space requires it.

```sh
cargo +1.95.0 run -p cayw-verify-tool --locked -- sdk-ipc --out /tmp/cayw-sdk-new --samples 3
cargo +1.95.0 run -p cayw-verify-tool --locked -- luau --out /tmp/cayw-luau-new --samples 3
cargo +1.95.0 run -p cayw-verify-tool --locked -- read /tmp/cayw-sdk-new
cargo +1.95.0 run -p cayw-verify-tool --locked -- compare /tmp/cayw-sdk-new /tmp/cayw-sdk-other
```

`--samples N` repeats every selected scenario, with N in 1..=1000. Outputs are never merged or overwritten, including existing empty directories. `sdk-ipc --scenario NAME` selects one scenario. Luau accepts `--workspace NEW_DIRECTORY`; its default is `OUT/workspace`.

The ten SDK/IPC scenarios are:

| Name | Actual operation and boundary |
| --- | --- |
| `cold-sdk` | Start and wait for a fresh original child process through its first completed SDK prompt. |
| `sdk-lifecycle` | Run first and warm prompts, close Runtime, reopen and query the same persisted session. |
| `ipc-lifecycle` | Authenticate local Clients and execute two actual write/read prompts. |
| `ipc-approval` | Keep execution and control connections separate; approve the original write and read independently and verify permission precedes each start. |
| `ipc-rejections` | Reject wrong authentication, unknown protocol version, an over-limit frame without a newline, extra prompt parameters, a forged approval actor and a zero event-page limit. Check dispatch counts and unchanged business state. |
| `ipc-event-reconnect` | Disconnect only the independent event connection, retain the one original prompt, and resume its exact event suffix from a synced cursor through a new authenticated connection. |
| `ipc-reconnect` | Lose the connection holding the execution request, retain its actual cancellation/unknown reply classification, and query the original durable task without replaying its prompt. |
| `ipc-cancel` | Send actual `rpc.cancel` for the original prompt request, then check durable task cancellation, idle state and absence of file effects. |
| `host-restart-before-dispatch` | Kill and wait for the original fixture host before dispatch; reopen and explicitly recover the original task without executing the script. |
| `host-restart-unknown` | Kill and wait after a synced synthetic local effect; recover the original identities with unknown outcome, refuse new work and preserve the one effect without replay. |

Restart readiness follows syncing the fixture effect and its directory. These are process-kill tests, not power-loss tests or coverage of every Runtime crash window. The fixture token is public test data. Exact preconfigured grants are test configuration. Only `ipc-approval` exercises actual product approval through the local IPC actor.

Each pipeline executes real Luau and Host write/read operations. A second prompt includes the existing content digest and `overwrite=true` required by Host. IPC relays keep original bytes, connection IDs, request IDs, replies and cancellation targets. Raw oversize rejection goes directly to the server rather than through the relay.

## Direct Luau cases

The eleven cases cover fresh-process first success; first and next execute in a running process; UTF-8 file write/read/list/stat; a syntax failure followed by its corrected script in the same workspace; instruction-budget exhaustion; cancellation while an injected Host operation is pending; permission denial; unavailable script globals; an unregistered capability; and strict argument rejection.

Each execute creates a fresh Luau VM. A warm process does not imply VM reuse. File operations use the real Host permission gate and durable audit. The ordinary Host has its configured file/data catalog; process profiles, Git and external services are not enabled by this fixture.

The syntax correction is fixed offline input. Its `repair_attempt` observations and `repair_summary` retain both the original failure and correction, their errors, identities and measured durations. Totals include every recorded attempt. Missing durations remain null; unmeasured token usage and money remain `not_measured` with null values. This does not prove a model can repair a script.

The cancellation case injects a local pending `search.query` fixture. It has no network operation or subprocess. Cancellation starts after the fixture has entered its future. The real Host's unknown classification and durable settlement are retained; the fixture does not claim the product has reconciled the effect.

## Evidence format and validation

Both modes use schema v2:

- `meta.json`: scenario inputs, permissions, configuration, measurement boundaries, platform, protocol and build/executable facts.
- `samples.jsonl`: each sample's identity, actual task checks, status, timings, observations and attachment descriptors.
- `samples/<iteration>/<scenario>/report.json`: the same sample report without index metadata.
- `records.jsonl`: synchronously retained original observations, from scenario start to terminal.
- Bound attachments: original Provider requests, IPC frames, Luau records/audits, saved cursors, child output/exit records and restart fault records as applicable.
- `summary.json`: statuses, min/nearest-rank median/p95/max grouped by scenario and phase, repair totals and attachment hashes.

`read` rechecks hashes, safe relative attachment paths, sample/start/terminal agreement and derived statistics. Built-in scenarios also recheck actual effects, full original task/executor/call identities, event order, original requests and consumed tool results. It accepts legal intermediate pages only when the original final page has been completely consumed. Missing required records, changed content, unknown final execution, or a task with no concrete result check cannot become a passing task. Symlink escapes and parent paths are rejected.

`read` exits successfully for a structurally valid report even when the recorded task failed. Running a workload exits unsuccessfully for failed, unsupported or unverified samples. Each evidence file, including the combined `samples.jsonl` index, has the reader's 64 MiB limit. If the next completed sample would exceed the index limit, the producer stops sampling and returns an error before appending it. Completed per-sample reports, observations and effects remain on disk; the incomplete bundle has no successful final summary. Other oversized evidence files are retained and rejected by the producer as well. An original-child cleanup or receipt failure stops subsequent sampling. The original process owner performs stop/wait and bounded dual-stream drainage for normal exit, failure, timeout and dropped futures; observing a new process or reused PID is not cleanup evidence.

`compare` retains each side's own source/executable identity. Known differences in task, input, permission, model, platform, protocol, compiler, target, optimization, profile or measurement boundary produce `incomparable`. Missing required observations or build inputs produce `pending`. Contradictory or damaged evidence produces `invalid`. Only `comparable` exits successfully. Actual timing observations are not comparison conditions; there is no fixed millisecond performance threshold.

## Build observations and remaining gaps

Set `CAYW_SOURCE_MANIFEST_PATH` while building to an explicit JSON manifest:

```json
{
  "schema": "cayw.build-source.v2",
  "root": "/absolute/physical/CAYW/root",
  "scope": "explicitly selected source and build inputs",
  "files": [{ "path": "Cargo.toml" }]
}
```

The example shows the format only. The manifest must also name `Cargo.lock`, `crates/cayw-verify-tool/build.rs`, `crates/cayw-verify-tool/src/lib.rs`, `crates/cayw-runtime/src/lib.rs` and `crates/cayw-ipc/src/lib.rs`, plus the actual inputs being measured. Build scripts reject another root, duplicate or unsafe paths and missing required files. They read and hash the named bytes; a caller's expected hash cannot establish those bytes. A list is not a proof that all compiler inputs were selected. Setting the environment variable only when launching an existing executable cannot change its embedded measurements.

Both run modes accept `--build-receipt FILE` before allocating output. Public `provenance::build_receipt` APIs capture and check explicit pre/post source snapshots, successful raw Cargo JSON, public `.d` files, artifacts, generated input observations and the selected executable. Current-program verification also checks the immutable list embedded during compilation. The writer binds the receipt as `build-receipt.json`; the reader repeats its checks.

Even a consistent receipt remains `pending`. Stable Cargo JSON and public dependency files do not trace every build-script environment/file read, procedural macro, native compiler, linker, configuration or invocation input. Pre/post hashes do not observe source at the instant the compiler reads it. Cached artifacts and generated input measurements retain their gaps. `known_inputs_verified` and `compiled_listed_inputs_verified` describe only the checks actually made; neither means complete build provenance. `source_bound` and `self_verified_source` stay false. A boolean or caller-supplied digest cannot promote a comparison to complete evidence.

## Reuse the public interface

Implement `sdk::Scenario::specification` and `execute`, retain observations/attachments with `ScenarioReport`, and register concrete `task_check` results. A custom scenario without those checks stays unverified. Generic cleanup checks do not establish task success. A custom consumer remains responsible for its task-specific result checks; the generic reader cannot infer arbitrary application semantics.

```sh
cargo +1.95.0 run -p cayw-verify-tool --example custom_scenario --locked -- /tmp/cayw-custom-new
cargo +1.95.0 build --manifest-path examples/verify-tool-client/Cargo.toml --locked
```

The independent [verify-tool-client](../../examples/verify-tool-client/README.md) has its own workspace and lockfile. It reuses the public scenario, report, reader, comparison and build-receipt APIs to create, close, reopen and query actual Runtime sessions. Its deliberate wrong-result command must fail while retaining all three samples.

## Measurement boundaries and checks

Durations use a monotonic clock in nanoseconds and include evidence writes within the measured span. Cold `parent_spawn_to_exit_ns` covers the original child from spawning through output and exit, including loader, runtime, fixture, Host/CodeMode setup and first execution. OS caches are not flushed. SDK first/warm prompt and IPC connection/close phases are kept separate.

`host_setup_ns` and `codemode_setup_ns` measure their public constructors. `first_execute_ns` includes VM setup, compilation, script execution and VM destruction. `case_execute_ns` measures the selected execution; `case_total_ns` includes fixture setup and checks. `vm_setup_only_ns` stays null because the public API has no separate timer. `scenario_total` additionally includes report and attachment work. Cancellation timing begins after the fixture enters the pending operation.

On the validated macOS host, process lifetime peak RSS is recorded in bytes. It is not per-task allocation, a delta or a claim about another platform. Unsupported or unobserved values remain null. Real Provider latency, task success and repair cost remain unverified until separately authorized and executed.

```sh
cargo +1.95.0 fmt -p cayw-verify-tool -- --check
cargo +1.95.0 check -p cayw-verify-tool --all-targets --locked
cargo +1.95.0 test -p cayw-verify-tool --locked
cargo +1.95.0 clippy -p cayw-verify-tool --all-targets --locked -- -D warnings
```

Tests exercise the actual executable and local APIs. Counterexamples alter original calls, events, pages, permissions, effects, process receipts, costs and attachments while refreshing outer hashes, so an integrity hash alone cannot make damaged evidence valid. Retain each executed command, exit status, source/build identity and raw logs. This README describes the interface and method; it is not an executed acceptance result.
