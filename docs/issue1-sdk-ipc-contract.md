# Session SDK and IPC contract

This is the local Issue [#1](https://github.com/changw98ic/CAYW/issues/1) delivery contract. The executable downstream examples are in [`examples/issue1-client`](../examples/issue1-client/README.md). They use the public `cayw_runtime::Runtime`, `cayw_core::SessionStore` and `cayw_ipc::Client` interfaces. The runtime graph needs no UI, optional Server, VerifyTool, Codex or external code index. All model responses in these examples are fixed local fixtures; real Host file operations, SQLite commits and loopback TCP remain in the execution path.

## Identity and persistence

A Session is a durable conversation with a stable `id`. Its history, ordered events, approvals, original calls and execution facts survive a process exit. A Task is one explicit prompt admitted to the durable Runtime queue. An Attempt is one fenced execution of that task. A Worker is the replaceable executor identified by `worker_id`; it is not the Session and its identifier is not the operating system PID. Each attempt lease contains `session_id`, `attempt_id`, `worker_id` and the monotonically increasing `fence`. Core checks the complete lease in the same transaction as an execution write.

The canonical store is SQLite, Core protocol/schema v3. Events and the current projection commit together. Each event retains its Session, cursor, timestamp, Core version and original attempt lease when applicable. Capability records additionally retain the task, execution, parent model call, source hash, capability name, exact arguments, sequence, authorization and terminal outcome. Core capability sequence starts at zero; Cells intent sequence starts at one and conversion checks the boundary.

Opening a store or Runtime does not claim an old worker, replay a prompt, or restart a VM. `Runtime::recover` is explicit and checks that the original task/Cells execution ownership can be held exclusively. Pending Device cleanup must also be confirmed. Only then can recovery fence old leases, repair durable projections and release the task reservation. A process kill in the example is followed by `Child::wait` and exit-status inspection before reopening. This demonstrates process-exit recovery; it does not simulate sudden power loss.

The original Issue comment proposed JSONL storage and Unix sockets/Windows named pipes. That implementation sketch has been superseded by the existing SQLite Core v3 store and authenticated loopback TCP IPC v1. There is one current path; this delivery does not introduce a parallel JSONL or socket implementation.

## Public calls

The downstream `sdk` mode constructs `Runtime::open` with an explicit workspace, database, model, Provider and `RuntimeConfig`. It calls `create`, `prompt`, `wait_events`, `approve`, `cancel`, `tasks`, `get`, and reads actual Core events through `events`. No client accesses private Runtime fields.

The IPC handler uses the same Runtime implementation. Important method mappings are:

| IPC method | Public Runtime operation | Result |
| --- | --- | --- |
| `session.create` | `create` | `SessionView` |
| `session.get`, `session.list` | `get`, `list` | Current durable view(s) |
| `session.prompt` | `prompt` | View after this request stops |
| `session.events` | `wait_events` | Ordered `EventRecord` page |
| `session.cancel` | `cancel` | `{cancelled: bool}` |
| `approval.decide` | `approve` | Original `ApprovalRequest` with decision |
| `session.tasks`, `task.get` | `tasks`, `task` | Durable task identity/state |
| `task.enqueue`, `task.resume` | `enqueue`, `resume` | Explicit queue admission/execution |
| `session.recover` | `recover` | View after explicit stopped-owner recovery |
| `session.reconcile`, `capability.reconcile` | `reconcile`, `reconcile_capability` | View after evidence-based repair |

Parameters reject unknown fields. IPC approval actor is fixed by the host as `rpc-user`; a model-supplied `actor` is invalid. SDK actor is trusted host input. The example uses `synthetic-sdk-reviewer`, and all allow/deny choices are explicitly synthetic fixture decisions, not human approvals.

## Events and reconnect

Events are durable pull pages, not an ephemeral push channel. `events(session, after, limit)` excludes `after`; accepted limits are 1 through 1000. A future cursor is an error. `wait_events` and IPC `session.events` with `wait_ms` wait for a page, cancellation or the bounded wait deadline (at most 30 seconds). An empty page means no events were available before that deadline; it is not a Session terminal.

A consumer keeps the last contiguous event it has processed. It saves `{session_id, after}` durably before closing its connection. The example validates each cursor, writes a new file, calls `sync_all` on that file and its containing directory, and only then closes the event client's socket. It preserves a separate control connection so a subscription disconnect cannot cancel that connection's active prompt. On a new explicit connection it reads the cursor from disk and requests only the suffix. The example compares the entire resumed suffix with the Core log and also checks that saved prefix plus suffix equals the full log, with one original task and one user prompt.

Disconnecting the connection that owns an active `session.prompt` instead cancels that RPC and its Runtime execution. The original transport error may remain `outcome_unknown:true`; inspect the durable task and Session separately. A saved cursor allows reading what happened; it does not authorize resending the prompt. There is no automatic reconnect or replay.

## Pairing and state transitions

| Condition/action | Required resulting behavior |
| --- | --- |
| Idle Session receives explicit prompt | New Task, Attempt, Worker/fence; Session running |
| Exact Host capability needs permission | Durable planned call and pending approval; Session awaiting approval; no Host effect yet |
| Allow once | Only the exact target/scope can start; another capability needs its own approval |
| Deny | Call is not executed; denial cannot be overwritten or bypassed by another approval for the same task/scope |
| Cancel while pending | Pending request is denied/closed, no file effect; task becomes cancelled and late allow cannot revive it |
| Known call terminal | Stored once for the exact call/arguments/attempt; duplicate, unknown call, changed arguments/name or old-lease result rejected without appending |
| Active RPC connection closes | Original RPC conservatively reports uncertainty; execution cancellation and durable task state are inspected separately |
| Old owner exits before dispatch | Explicit recovery records not-executed effects; same logical Session becomes idle; original task is interrupted |
| Started effect has no durable terminal | Explicit recovery retains original `outcome_unknown`; Session needs reconciliation and refuses new work/fork |
| Trusted host verifies original effect receipt | Explicit reconciliation appends evidence for that original call; no source or prompt replay |
| Explicit next task after a known recovery | Same Session, new Task/Attempt/Worker and higher fence |

Cancellation and task completion are ordered by the durable queue write transaction, including cancellation from a separately opened Runtime or its authenticated IPC handler. If cancellation commits first, a known task ends as `cancelled` with exactly one `run_cancelled` audit record. If completion commits first, later cancellation returns false and cannot rewrite the terminal. When task request authority is configured, it stays Closing through original work/process cleanup and ends from that same terminal decision under lifecycle coordination. Genuine unknown effects retain `outcome_unknown` (Requests records failed); failed cleanup keeps the original task draining and its capacity reserved for explicit recovery. Cancellation never clears either condition. If Requests ended but the queue terminal commit failed, cancellation returns a recovery-required error instead of acknowledging a conflicting terminal; capacity stays held until explicit recovery. Pending approval cannot authorize the cancelled task or an explicit replacement, which requires fresh permission and a new worker/attempt/fence.

Session `idle` is reusable; it is not a permanent terminal for the whole conversation. Task, call, execution and approval terminals are immutable. A new task cannot alter an old terminal. An explicit Session cancellation retains `cancel_requested` on that task. Transport-token cancellation can instead retain a cancelled terminal with `cancel_requested:false`; inspect both facts instead of inferring which cancellation path ran from that flag alone. `Runtime::cancel` repeated after task completion reports false. Ordinary result errors must mean known failure; an uncertain side effect must be recorded as unknown. Neither reopen, cancel, finish, repeated recovery nor fork converts uncertainty to known success.

The standalone package's tests call the public Core API to reject mismatched, duplicate and cross-attempt tool results and check that neither cursor nor projection changes after rejection. Existing Core/IPC/Runtime tests cover the underlying state transitions and transport pairing. None of the new client proofs edits append-only SQL records.

## IPC version, errors and limits

IPC frame version is **1**. Core event/store version is **3**. These are different layers. `runtime.health.protocol_version` reports the Core version; it must not be used as the outer IPC frame version. A connection starts with authenticated `hello`. An incompatible outer version receives `unsupported_version` with the request ID and the server closes the connection before business dispatch. There is no fallback downgrade. The full strict frame contract and bounded defaults are in [`cayw-ipc/README.md`](../crates/cayw-ipc/README.md).

Every wire frame is one bounded JSON object followed by LF. Response IDs must correspond to a fully written, currently pending request on that same connection. Unknown, early, duplicate, malformed or wrong-version responses close the connection and conservatively fail remaining requests. Authentication and all resource limits come from the host; the example token is public synthetic data. Only loopback addresses are accepted. This transport is not a remote TLS service or operating system security boundary.

`ClientError` contains `code`, `message`, and `outcome_unknown`. For example:

| Error | Meaning and caller action |
| --- | --- |
| `unsupported_version`, `unauthorized` | Handshake rejected; fix contract/token before explicit new connection |
| `invalid_params`, `request_too_large`, `client_busy` | Local validation/capacity rejection before dispatch; outcome known |
| `busy` | Server's in-flight handler limit rejected dispatch; outcome known |
| `stale_approval`, `session_error` | Runtime rejected the request; inspect message/current Session |
| `needs_reconciliation` | Original unknown effect blocks new work; submit actual host evidence |
| `cancelled_outcome_unknown`, `timeout_outcome_unknown` | RPC cancelled/timed out after admission; inspect durable records |
| `connection_closed`, `connection_lost`, `protocol_error` for in-flight calls | Original request may have run; inspect durable state, never blindly retry |

The client and server intersect declared frame/in-flight/ID limits. Local over-capacity calls fail before sending; server over-capacity calls return `busy`. A bounded response queue closes a slow connection if it fills. Reads and writes have deadlines. A too-large or unretainable handler response can be unknown after effects; a locally rejected oversized request is known unexecuted. Cancellation targets a request in the same connection using a distinct request ID; original and cancellation acknowledgments are separately paired. Dropping an admitted client request future closes the shared connection. Explicit `request_with_cancel` keeps polling that request to its terminal. Transport cancellation alone does not prove side effects were undone.

## Reproduction and limits

Use the exact commands in the downstream README. The client's `all` command runs six independent scenarios and writes a machine-readable acceptance result plus raw events, views, task identities, saved cursors, child readiness/exit facts and synced synthetic receipts. Every output file is created exclusively; use a fresh output directory on each run.

The local evidence bundle records baseline/final source hashes, nested dependency identities, exact commands, environment, exit codes, logs, executable hashes and requirement rows. The cancellation repair retains an exact 403-file baseline separately, changes only the relevant Runtime/test/client/contract files and records the replayable patch. The root Cargo manifests/lock remain unchanged. The downstream lock keeps its actual smaller graph and the same frozen third-party identities as the root lock. No dependency upgrade is part of this delivery.

The current first release targets macOS only, following the user's 2026-10-19 scope decision. Issue #1's local functional acceptance is complete on Rust 1.95.0, macOS ARM64, debug builds and fixed offline Providers. Windows and Linux are outside the current release scope; their missing implementation or execution evidence does not block this delivery. Other macOS architectures are not certified by these results. Real Provider/model calls, paid tests, full Issue #2 performance acceptance, Issue #6's real-model chain and unrelated optional feature matrices remain outside this Issue #1 evidence. At the user's request, GitHub Issue #1 was closed on 2026-10-19 and later Linux/Windows support is tracked in [Issue #19](https://github.com/changw98ic/CAYW/issues/19). The implementation, independent client and this contract were merged into `main` through PR #20. Raw execution artifacts remain in the local evidence locations documented in the validation record.
