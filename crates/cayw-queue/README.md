# cayw-queue

Durable task identity, priority/fairness admission, declared-memory reservations and
cancellation for the CAYW runtime. Opening a database never claims work.

## Boundary

`cayw-queue` owns the queue state machine and its SQLite schema. It does not depend on
`cayw-core`, `cayw-requests`, `cayw-runtime`, the Host or the Agent execution code. The
host Runtime owns cross-process coordination, initialization of the shared
`runtime_audit` table, request authority and permission checks, then maps
`QueueError` onto `RuntimeError`. Queue transitions write their audit rows in the
same queue transaction.

The host opens a queue through its own wrapper because policy adoption needs the Runtime
policy lock. The queue itself only exposes `read_policy` and `adopt_policy`; it never
acquires the host lock.

## Canonical task intent

`TaskIntent` is defined once, here. `cayw-requests` re-exports this exact type and does
not keep a second copy. Public queue entry points check `TaskIntent::validate`
before accepting a task. Runtime relies on that check instead of repeating its
rules. The request store checks its own trusted authority input. No model-facing
path infers intent from prompt text.

## Error boundary

Queue validation returns `QueueError`, including:

- `TaskInput::validate`
- `TaskResources::validate`
- `ResourcePolicy::validate`
- queue, cancellation, recovery and terminal operations

The host converts with `From<QueueError> for RuntimeError` (`InvalidInput`/`Busy`/
`QueueFull`/`Cancelled`/`Worker`/`StaleCancellation`/`Journal`/`Json`). The conversion
belongs to the host layer; the queue does not import the Runtime error type.

When migrating callers, direct calls to `TaskInput::validate`,
`TaskResources::validate`, and `ResourcePolicy::validate` now return `QueueError`
instead of `RuntimeError`. Match `QueueError::InvalidInput` in these callers, or
convert with `RuntimeError::from` at a Runtime boundary. `cayw_runtime::QueueError`
re-exports the same type for clients that already depend on Runtime; a separate
queue dependency is not required to name this error. Runtime operations continue
to return `RuntimeError`.

## Terminal settlement

`Queue::finish` runs the host `settle` callback inside the same `IMMEDIATE` transaction
as the task terminal and audit row. The callback's error type is preserved, so a failed
request-authority write rolls back the queue terminal and audit together. The Requests
store commits separately: if its terminal succeeds and the later queue COMMIT fails,
Runtime retains the queue slot in `draining` until explicit recovery. These stores do
not share an atomic commit. `outcome_unknown` is never rewritten by an accepted
cancellation.

## Test support

`connection_for_test` is compiled only with the `test-support` feature. The Runtime unit
test uses it to install a deferred foreign key so COMMIT fails deterministically after a
real Requests `end_task` succeeded, with no timers and no production PRAGMA change.
