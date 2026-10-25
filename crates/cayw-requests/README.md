# cayw-requests

A runnable task-scoped request lifecycle over the existing `cayw-host` and Device boundaries. The two model-facing tools record requests and inspect status. Requests never execute capabilities, launch processes, load skills/MCP, create connections to external services, or grant themselves permission.

## Lifecycle

The application starts a task with `TaskIdentity { session_id, task_id, attempt_id }`, a captured `OperationCatalog`, a trusted `HostActor`, and either general intent or an explicit user instruction for a project test/script.

Every request records a purpose, concrete gap, operation ID, capability, and exact JSON arguments. The host selects operations from the actual configured Host catalog. Each selected operation binds its schema, permission policy ID, fixed profile ID/arguments when applicable, and a host/Device permission binding. The catalog fingerprint also binds the canonical workspace, configured profiles, environment/argv, file/process budgets, and data/workspace limits. The host must rotate `permission_policy_id` when changing opaque injected service/gate/syntax-validator implementations or Device authority. Executables and injected implementations remain trusted host resources.

- Invalid purpose/gap or unsupported/mismatched operations produce `invalid` with validity reasons. Invalid requests do not contribute to a threshold and do not supersede an existing valid grant.
- A general valid request below the repeat threshold starts at `requested`. Repeated structurally valid requests for the same exact operation in the same attempt move the informational state to `awaiting_evaluation` at `RequestConfig::repeated_valid_threshold` (default 2).
- The threshold only selects that hint. A trusted host may call `evaluate()` on the very first structurally valid request; the threshold never withholds evaluation and never grants by itself. Structural validity does not prove the model's claimed gap. The trusted evaluator must verify the actual need.
- Host-recorded `ExplicitProjectTestOrScript` intent plus an existing exact `build.run` or `test.run` profile also starts directly at `awaiting_evaluation`. A model boolean or assertion cannot supply that intent.
- `evaluate()` issues a private, non-deserializable, single-use evaluator ticket with an actor and deadline. It does not grant execution. Only trusted Rust host code calls `approve`, `deny`, or `evaluator_unavailable` with that ticket and a nonempty reason.
- A grant is exact and bounded by its host-selected lifetime. It cannot add programs, arguments, profiles, external services, filesystem authority, or permissions to the underlying Host/Device.
- New valid requests supersede older live requests for that exact operation. Duplicate/late decisions, duplicate terminal transitions, and old tickets fail closed.
- `revoke`, observed deadline expiry, `replace_attempt`, and `end_task` invalidate the affected grants and pending evaluator tickets. Attempt IDs cannot be reused. Ended task IDs remain tombstones and cannot restart. Another session/task/attempt cannot inspect or consume the grant.

`end_task` invalidates all subsequent calls through the wrapper, including ordinary task work. It does not undo an already admitted operation; the runtime must also cancel the task's supplied `CancellationToken` when stopping a running task. Host/Device still owns execution cancellation, process termination, budgets, and unknown-effect reconciliation.

## Host integration

`RequestStore::open(absolute_sqlite_path, config)` opens a public, file-backed store. The application owns the path, parent directory, permissions, and backup policy. A reopen with the same configuration is the ordinary path; an old configuration is rejected with `PolicyMismatch` rather than silently changing the durable policy.

Operational limits are not frozen. Trusted host code calls `adjust_config(next, actor, reason)` on a live store. The new value is validated, persisted, and written with an immutable previous/next history row in the same transaction; `config_history()` reads that audit. Adjusting below current unfinished-task usage is rejected. After a restart, `persisted_config()` reads the durable value and `open_with_trusted_adjustment()` opens with it and applies a requested change. A handle opened with the previous configuration fails closed on its next operation.

Use `RequestHostConfig::catalog()` to capture schemas from a real validated Host, call `start_task`, then construct `RequestHost` with the same configuration and identity. `RequestHost` implements the existing `cayw_codemode::CapabilityHost`, combining normal Host capabilities with exactly `capability.request` and `capability.status`. Their schemas reject unknown top-level fields; no task identity, evaluator, approval, grant, or process configuration fields are accepted. Scope arguments are bounded and must equal a host-selected exact operation; Host validates its own schema and operation-specific restrictions again before executing.

The wiring sequence in trusted Rust code is:

```rust,ignore
let store = Arc::new(RequestStore::open(sqlite_path, RequestConfig::default())?);
let catalog = wiring.catalog()?; // wiring: RequestHostConfig
store.start_task(&identity, intent, &catalog, &actor)?;
let host = RequestHost::new(store.clone(), identity.clone(), wiring)?;

// Model sees only capability.request/status; it cannot obtain an evaluator ticket.
let request = store.request(&identity, &catalog, request_input)?;
let ticket = store.evaluate(&identity, &request.id, &actor)?;
// The application verifies purpose, gap and scope before taking this host action.
store.approve(&ticket, &catalog, 60_000, "verified user need and exact profile")?;

// Invoke through CodeMode or the existing CapabilityHost interface.
let result = host.call("test.run", json!({"profile":"project-test"}), cancel).await?;
store.end_task(&identity, TaskOutcome::Completed, &actor, "task finished")?;
```

The executable integration is in `tests/lifecycle.rs`, including the complete HostConfig, actual process profile, permission gate, audit sink fixture, store, and model tool calls. It uses the compiled Rust test executable as the application-selected profile, reads a real project input from a temporary workspace, and writes a marker. No shell or interpreter is involved. The execution audit fixture is in memory; production supplies the existing durable `AuditSink`. The request lifecycle itself uses actual SQLite.

For every ordinary Host call, the adapter checks the active task and retains the injected upstream `cayw_host::PermissionGate`. For a selected capability name, **every** argument set requires an exact active request grant; changing profile/filter/path cannot fall through to an ordinary upstream permission. Other capability names continue through their ordinary upstream gate, so request denial, evaluator unavailability, or timeout leaves unrelated granted capabilities usable. Selecting one capability name therefore protects all its operations; choose the managed set deliberately.

`RequestGate` rechecks SQLite after an upstream permission wait. The full `RequestHost` wrapper additionally rechecks before and after durable audit start, settling a known denial if permission changed while the audit write was awaited. Calls admitted before revocation can already have effects; revocation cannot roll back those effects. Do not expose a second ungated Host or raw process runner beside this wrapper. The standalone `RequestGate` is available for other host owners; they must preserve the same execution audit checks when composing their own wrapper.

## Persistence and limits

Lifecycle events and the current projection commit together under a SQLite `IMMEDIATE` transaction with foreign keys, WAL, and `synchronous=FULL`. Triggers reject event updates/deletions and task/request tombstone deletions. Failures during event insertion roll back the matching state change. Raw database access remains a trusted host boundary, not a sandbox against an administrator editing database files.

Configured task, request, attempt, event, and conservative logical storage reservations bound retained state. `max_tasks` and the task portion of `max_state_bytes` are operational: they count unfinished `Active`/`Closing` tasks, and `end_task` returns that task's count and reservation to the pool. Finished tasks keep immutable tombstones, requests, and events, so ordinary zero-request Chat does not consume a permanent lifetime allowance. The event budget is separate and remains a lifetime limit on immutable audit rows. Authority repair preparation holds a temporary state reservation until activation commits; activation returns that reservation while retaining the immutable repair and activation records. Reopening uses the same unfinished-task state accounting. Admission reserves space for all later terminal transitions, so exhausted request quotas cannot prevent revoke/end. There is no implicit garbage collection or DB reset that would forget revocation. Physical SQLite file/WAL overhead is separate from the logical payload budget and needs host filesystem quota/maintenance if a hard disk cap is required.

Inputs have byte, depth (32), and JSON node (4096) limits; selected catalogs allow at most 64 exact operations and 512 KiB. Reasons/instructions allow 2048 bytes. Serialized output pages are bounded, and `events(identity, after_sequence, limit)` pages at most 64 entries. Counter arithmetic, SQLite integer conversions, and deadlines are checked. The persisted clock high-water mark prevents a clock rollback from reviving observed expiration, including after a rejected mutation. Evaluation expiry is checked on use; no background scheduler is required. SQLite busy waits are bounded (default 50 ms, maximum 1000 ms).

## Validation

From the repository root after this package is a workspace member:

```sh
cargo +1.95.0 test -p cayw-requests --locked
cargo +1.95.0 check -p cayw-requests --all-targets --locked
cargo +1.95.0 clippy -p cayw-requests --all-targets --locked -- -D warnings
rustfmt +1.95.0 --edition 2024 --check crates/cayw-requests/src/lib.rs crates/cayw-requests/tests/lifecycle.rs
```

The suite covers invalid repetition, configurable eligibility, first-round explicit intent, forged tool fields, exact schemas/permissions, evaluator races on independent SQLite connections, reopen, revoke, task end, old attempts/tickets, expiry and clock rollback, event/projection rollback, terminal capacity reservations, cancellation before dispatch, a real profile, and ordinary granted file work after denial/unavailability/timeout. It also revokes during upstream permission waiting and audit start to prove the wrapper does not dispatch from an earlier successful check.

Validated locally on macOS with Rust 1.95.0; Windows and Linux remain unverified. Test provider/human decisions are deterministic host fixtures. No live models, paid calls, network services, or external application writes are needed by the suite.

## Runtime integration

`cayw-runtime` binds the actual Session/Task/attempt, captured Host and dynamic service policy, ordinary permissions, Core/Cells call origins and original process cleanup to this authority. Its SDK exposes request status/events, evaluation, decision and revocation. Authenticated local IPC offers the same host controls with fixed actor `rpc-user`; the private evaluation ticket stays inside the server. The launch configuration's optional `requests` section selects exact operations and `RequestConfig` limits. Configuration alone grants nothing. See the [SDK / IPC contract](../../docs/issue1-sdk-ipc-contract.md) and [headless configuration](../../docs/headless-chat.md).

An embedding application still owns its trusted human/policy decision source, ordinary Host permissions and durable execution audit. Actor names and model assessments are not authentication. The Requests crate itself does not run a Task, manage a process, connect to MCP or present UI. Runtime task transitions and this separate request database are not one cross-database transaction; lifecycle coordination keeps authority Closing until original work and cleanup settle, and retains capacity when persistence or cleanup fails. These responsibilities use the same Requests/Host/Device path rather than an independent execution channel.

Dependencies added here are Apache-2.0 compatible: async-trait (MIT/Apache-2.0), rusqlite (MIT, bundled SQLite public domain), serde/serde_json (MIT/Apache-2.0), sha2 (MIT/Apache-2.0), thiserror (MIT/Apache-2.0), uuid (MIT/Apache-2.0), and test-only tempfile/tokio (MIT). No Codex dependency is included.
