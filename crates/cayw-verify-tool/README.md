# CAYW offline verifyTool

The `sdk-ipc` mode exercises the public `cayw_runtime::Runtime` SDK and authenticated `cayw_ipc::Client`/server over loopback TCP. The `luau` mode below retains the direct CodeMode/Host measurement CLI.

```sh
cargo +1.95.0 run -p cayw-verify-tool --offline --locked -- sdk-ipc --out /Volumes/ORICO/cayw-sdk-new --samples 3
```

Each sample uses a new directory. The seven built-in scenarios are `cold-sdk` (fresh child process through first completed SDK prompt), `sdk-lifecycle` (first and warm prompts, then reopen), `ipc-lifecycle` (two authenticated prompts execute the actual write/read pipeline), `ipc-reconnect` (disconnect during a pending Provider call, read a synced cursor with a new authenticated connection), `ipc-cancel` (actual `rpc.cancel`, durable cancelled task and idle session), `host-restart-before-dispatch`, and `host-restart-unknown`. Both restart scenarios kill and wait for the original fixture host before reopening and explicitly calling `Runtime::recover`. Before dispatch, no script or Host operation runs. After a synced local synthetic effect, the original capability remains unknown, new work is refused, and neither the old task nor the effect is replayed. Readiness is acknowledged only after syncing the effect file and its containing directory on Unix; this is a process-kill scenario, not a power-loss simulation.

The fixed Provider makes no model or remote network calls. It supplies a Luau write/read pipeline to the real Agent, Cells, CodeMode and Host. The first prompt creates the file; the next prompt supplies `overwrite=true` and the known existing content SHA-256 required by Host compare-and-swap. Both scripts and both exact write grants are retained. Exact fixture grants are recorded as test configuration; they are never represented as user approval. A transparent fixture relay records actual IPC request/response frames, connection identities, request IDs, cancellation targets and authentication replies. Its token is a fixed public fixture string, not a user credential. The wrong-token attempt must fail authentication. Cancellation checks exactly one original wire response against the consumed result or error and preserves any `outcome_unknown` classification. Durable task cancellation, idle session and absence of effects are checked separately; those facts never rewrite the original reply.

`meta.json` binds inputs, expected results, configuration, permission scope, fault points, platform, compiler/build information and executable hash. `samples.jsonl` and per-sample `report.json` include actual checks, raw events, full capability origins and timings. `records.jsonl` synchronously retains observations as they happen; `provider.jsonl` retains actual fixed Provider inputs. IPC and child stdout/stderr have separate files and hashes. Unknown outcomes stay unknown; the fixture never invents a product reconciliation. Summary status and process exit fail if any scenario fails or is unsupported/unverified. Outputs are never overwritten.

The public `sdk::Scenario`, `ScenarioContext`, `ScenarioReport` and `run_scenarios` APIs support custom scenarios with the same evidence format. Run the downstream-style proof with:

```sh
cargo +1.95.0 run -p cayw-verify-tool --example custom_scenario --offline --locked -- /Volumes/ORICO/cayw-custom-new
```

`--scenario NAME` selects one built-in scenario. The deliberate negative controls below must exit nonzero, preserve their raw observations, and report the named semantic failure. Their expected failures are separate validation evidence, not passing workload samples.

```sh
cargo +1.95.0 run -p cayw-verify-tool --offline --locked -- sdk-ipc --out /Volumes/ORICO/cayw-bad-fixture-new --samples 1 --scenario sdk-lifecycle --negative-control wrong-script
cargo +1.95.0 run -p cayw-verify-tool --offline --locked -- sdk-ipc --out /Volumes/ORICO/cayw-bad-cursor-new --samples 1 --scenario ipc-reconnect --negative-control skip-cursor
```

All elapsed values are raw nanoseconds from a monotonic clock. Observations and IPC frames are synchronously synced within the measured spans, so these values include evidence-writing overhead. Cold includes loader, runtime startup, first execution, evidence writes and process exit; OS caches are not flushed. Warm uses the same Runtime for the next prompt. Cancellation measures the fault request through observed durable terminal and idle. A watchdog bounds liveness only; no fixed millisecond performance threshold is applied. Memory uses process lifetime peak RSS, recorded in native bytes on macOS or KiB on Linux, never a per-task allocation or delta. Unsupported/unobserved memory is null. Real model/task success and repair cost, and unexecuted Windows/Linux platforms, remain unverified. A passing local run never claims Issue #2 is complete. Final executed evidence must retain the exact frozen source/dependency closure and lockfile; compile-time source hashes remain explicitly caller-declared.

## Direct Luau measurements

This binary runs the existing `cayw-codemode::CodeMode` and `cayw-host::Host`. It does not implement another executor, file tool pipeline, model adapter, or Monty runner. Every case file write/read/list/stat uses the real Host, its permission gate and durable audit. Directory creation and result-file storage belong to the harness itself.

```sh
/Users/chengwen/.cargo/bin/cargo +1.95.0 run -p cayw-verify-tool --locked -- luau --out /tmp/cayw-luau-new --samples 3
```

`--samples N` means **N repetitions of each of 11 fixed cases**, with N in 1..=1000. Out and workspace must be new directories, including when an existing directory is empty. The out parent must exist. Workspace defaults to `OUT/workspace`; `--workspace NEW_DIRECTORY` may instead select a sibling location with an existing parent, or a direct child of out. Existing paths are never merged or overwritten. Reusing the generated data requires reading the existing output or running into a new location.

The fixed cases cover fresh-process first success, first/next execute in a running process, real UTF-8 file write/read/list/stat and SHA-256 comparison, syntax failure followed by an explicitly paired corrected script in the same workspace, infinite-loop checkpoint-budget exhaustion, cancellation during a pending host operation, real Host permission denial, absence of script `os`/`io` and related globals, an unregistered capability, and strict argument rejection.

The syntax repair is a fixed offline pair. The second script replaces the incomplete assignment with an actual `fs.read` call, then runs through the real Host. It is not evidence that a model can repair a script. Ordinary Hosts advertise the exact sorted default catalog: `archive.list`, `archive.read`, `code.diff_hunks`, `code.outline`, `codec.decode`, `codec.encode`, `data.parse`, `data.query`, `data.serialize`, `data.transform`, `fs.batch_status`, `fs.glob`, `fs.grep`, `fs.list`, `fs.mkdir`, `fs.move`, `fs.patch`, `fs.prepare_change_batch`, `fs.read`, `fs.recover_change_batch`, `fs.remove`, `fs.stat`, `fs.submit_change_batch`, `fs.tree`, `fs.write`, `hash.digest`, `text.diff`. The sandbox check compares against this explicit list; shell, process profiles, Git and external services require separate configuration and are absent in the ordinary Host.

The cancellation Host adds an explicitly injected `search.query` fixture, with a fixed query and exact permission grant. It waits asynchronously until cancellation; it has no network operation, subprocess, detached task, or file effect. Cancellation happens only after the fixture has entered its pending future. The actual Host retains `outcome_unknown` for cancellation of admitted external work. This result is preserved. The sample checks that work has stopped and that a settled audit record is durable before `execute` returns; it does not claim that unknown work has been reconciled in the product.

## Source binding

The binary records the build target, compiler version from `RUSTC --version`, package version, build profile, platform/kernel/architecture, executable SHA-256, per-case scripts and digests, budgets, permission decisions and inputs. Configuration includes all enabled data and workspace limits and identifies the unmodified default workspace checker as `cayw_workspace::JsonTomlValidator` (JSON and TOML parsing only). This is known from the harness's default configuration; it is not introspection or verification of an arbitrary custom checker. `build.rs` accepts an **explicit build-time** `CAYW_SOURCE_MANIFEST_SHA256` containing 64 hexadecimal digits. The caller must prepare and retain a source manifest covering the relevant sources, lockfile and build configuration, then provide its real SHA-256 when building/running. Changing that variable triggers rebuilding; setting it only when launching an already compiled executable cannot change the baked value. The binary records the supplied digest; it cannot reconstruct or certify the caller's manifest.

If the build input is absent, `meta.json` says `unbound`, `summary.json` says `unbound_not_acceptance`, and `acceptance_eligible` is false even when every semantic check passes. Do not use such a run as acceptance evidence. A correctly source-bound, passing run is still limited to its recorded offline fixtures and local platform. A supplied digest alone is not independent proof of source provenance; retain the manifest, binary and build inputs alongside the output.

## Measurement scope

- `parent_spawn_to_exit_ns` is timed by the parent around `current_exe __cold_case`. It includes process creation, loader/runtime startup, CLI parsing, Tokio initialization, audit setup, real Host and CodeMode setup, fresh Luau VM creation/compilation/execution/destruction, child serialization and process exit. This is a fresh process measurement; OS disk caches are not cleared. It is not full Agent latency.
- `host_setup_ns` measures `Host::new`, including real schema/catalog and directory capability initialization. Audit-file creation is outside this field.
- `codemode_setup_ns` measures `CodeMode::new` and its schema/catalog setup. This API has not created a Luau VM yet.
- `first_execute_ns` includes the first fresh VM setup, compilation, execution, host calls if present, and VM destruction. For `warm_process_first_success`, this is a separately recorded warmup execute; `case_execute_ns` measures the following execute in the same running process and CodeMode. Each execute creates a **new VM**, as the existing runtime does. Other cases measure their first execute on a newly configured Host/CodeMode inside the already running parent process.
- `vm_setup_only_ns` is null: the public API has no internal timer for VM setup. No private timing hook, alternate VM or estimated subtraction is used.
- `cancel_to_execute_return_ns` starts after the async fixture is actually entered and immediately before cancellation is requested. There is no fixed millisecond assertion. Semantic checks concern the observed state and audit pairing.
- `case_total_ns` covers fixture audit creation, Host/CodeMode setup, warmup where applicable, execution and semantic checks. Raw records expose the phases instead of pretending this is model or complete Agent latency.
- Supported macOS/Linux builds record `getrusage(RUSAGE_SELF).ru_maxrss` with its native unit: bytes on macOS, KiB on Linux. This is a cumulative high-water mark for that process, including initialization and previous cases. It is **not** per-case RSS, a delta, or peak allocation of an individual task. A cold child's record belongs to its own PID. Summary memory belongs to the parent. Unsupported platforms and system-call failures use null. No fabricated memory measurements are emitted.

All durations are monotonic elapsed nanoseconds. Summary min/median/p95/max use the available raw durations; median and p95 use nearest rank. Summaries explicitly separate cold process time from in-process execute time. There are no hard-coded performance pass/fail thresholds. Run release builds for performance assessment and retain their recorded profile; debug builds are suitable for checking semantics. No cross-runtime speed ranking is supported by these measurements.

## Output and failures

`meta.json` describes the run; `samples.jsonl` preserves each actual input, output, error, complete CodeMode report (calls, checkpoints, output bytes and unknown status), exact fixture grants/decisions, audit events, repair pairing and durations. Every record references the same run ID and metadata. `summary.json` includes artifact hashes and semantic results grouped by case. `audits/*.jsonl` retain actual synchronous `sync_all`-confirmed start/settled records used by the real Host. The explicit grants are **offline synthetic test fixtures**, never product or human approvals.

Each case defines its expected dispatched operations separately from available grants. The validator compares the exact names, JSON arguments and report sequence with actual permission grants or denials, then requires one unique, ordered durable start/settled pair for every granted call. Settled result/error payloads and unknown flags must agree with the report and Host. Missing, extra, duplicate, swapped or cross-call evidence fails even if the return value and declared grants look correct. A syntax failure has available grants but no permission decisions or calls; a denied operation has one failed report call and exact denial, with no audit start. A cancelled operation still requires durable settlement before return, including when its outcome remains unknown.

Expected runtime errors are successful semantic checks when their kind and effects match the case. A failed case remains a raw record and subsequent cases continue; any semantic mismatch yields a failing final exit code. Argument/path/setup failures that prevent creating output return an error directly. Audit/startup errors inside a case are recorded as case failures. Writing result files can itself fail and is reported as an infrastructure error. Unbound runs may exit successfully for semantics while remaining ineligible for acceptance.

```sh
/Users/chengwen/.cargo/bin/cargo +1.95.0 fmt -p cayw-verify-tool -- --check
/Users/chengwen/.cargo/bin/cargo +1.95.0 check -p cayw-verify-tool --all-targets --locked
/Users/chengwen/.cargo/bin/cargo +1.95.0 test -p cayw-verify-tool --locked
/Users/chengwen/.cargo/bin/cargo +1.95.0 clippy -p cayw-verify-tool --all-targets --locked -- -D warnings
```

The binary tests invoke the real executable, inspect its files and hashes, check the syntax-error/repair pair, cancellation, exact call evidence, immutable build binding and rejection of existing paths. The CLI integration test requires actual cancellation settlement; a dropped or missing settlement fails that test as well as the measured sample. Unit regressions also alter real file-operation samples to prove that missing permission and audit records cannot pass, and use explicitly synthetic cancellation records to test rejection of dropped/unknown inconsistencies. If the real runtime drops a pending Host future, the raw `dropped` audit event remains visible, all later cases still run, and the CLI must exit with failure. Passing unit detector tests does not imply the measured runtime passed acceptance. This README specifies the measurement method; it is not an executed result or an architecture decision. Actual platform results, retained raw evidence, and any ADR conclusion belong in the main session only after execution. Windows, other unavailable platforms, real models, complete Agent cancellation/persistence, and Monty comparison must remain unverified unless separately run.
