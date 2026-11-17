# cayw-cells

Durable Luau cells over `cayw-codemode`. `CellManager` owns one task's cells and
passes every capability invocation through the trusted `CellBackend`. SQLite
commits an intent before dispatch and its result before Lua resumes, so a lost
VM is never silently replayed. `CellTool` is the only model-visible
`execute_code` entry; it exposes `exec` and `wait`.

## Limits are optional

`CellLimits` uses `Option<...>` for artificial caps. `None` means the host did
not configure that cap, so the runtime does not reject work for exceeding a
built-in default.

- Optional and default `None`: `max_cells`, `script_bytes`, `output_bytes`,
  `output_items`, `item_bytes`, `media_bytes`, `max_output_tokens`,
  `max_yield_time`, `store_bytes`, `store_keys`, `json_depth`.
- Required with concrete values: `max_running` (default `64`, must be greater
  than zero) and `cache_entries` (default `128`, must be greater than zero).
- When `max_cells` is `Some`, `max_running` must not exceed it.

Validation rejects only unusable configuration: a zero required value, a
`Some(0)` cap, `max_output_tokens` below the 1024-byte minimum, or a zero
`max_yield_time`. It does not impose upper ceilings such as 10,000 cells,
1 MiB scripts, 64 MiB output, 64-level JSON, or 300 s waits.

When `json_depth` or a byte cap is `None`, JSON validation skips that check
instead of substituting a default ceiling. `SqliteJournal::records` reads every
row for the session/task when `max_cells` is `None`; a configured value still
bounds the query and reports overflow.

`max_running` limits executing VMs, not accepted submissions. A cell waits in NotStarted when all VM slots are occupied; cancellation settles that queued cell before dispatch. Backends receive queued/dispatched hooks so a parent task can yield its own execution slot while waiting.

Complete errors are retained in the journal. A page too small for the error returns `error_truncated: true`, `has_more: true` and `required_budget`; requesting that budget returns the complete error. Page summaries never replace the durable error.

`CellManager::new` does not reject a large host capability catalog. The catalog
is supplied by the host; it is not a model-controlled budget.

## `execute_code` tool

`exec` takes `action`, `script`, optional `yield_time_ms` and
optional `max_output_tokens`. `wait` additionally requires `cell_id` and
`cursor`. The schema keeps the minimum `max_output_tokens >= 1024`; it has no
artificial `maxLength` for the script and no upper `maximum` for
`yield_time_ms` or `max_output_tokens`.

The tool's default wait is 1000 ms for both actions, reduced only by a host's
configured `max_yield_time`. This lets short operations return their output in
the first call: the 2026-11-16 live trace measured 44–468 ms per short cell.
Output, explicit yield, permission waiting or completion can return earlier.
This is a response wait, not an execution timeout: reaching it leaves the cell
running for a later `wait`. Explicit `yield_time_ms: 0` still returns immediately.
The Rust manager's explicitly supplied `ExecRequest`/`WaitRequest` values are
unchanged.

When the model omits `max_output_tokens`, the page budget is the canonical
131072-byte slice, reduced only by a configured `max_output_tokens`. An
unconfigured `max_yield_time` accepts any representable millisecond value. If
the resulting deadline is not representable, the wait sleeps until an event or
cancellation instead of overflowing.

## Unknown outcomes and read invalidation

`CellStatus::OutcomeUnknown`, `IntentStatus::OutcomeUnknown`, and the
`outcome_unknown` fields remain factual audit states. They do not freeze the
session.

Within one cell run, an unknown capability outcome stops that cell and calls
`ReadCoordinator::invalidate` for the session. Cached reads are rejected, so a
later read performs a fresh capture and cannot serve stale bytes. Later tasks
and cells in the same session can still be created, run, store/load values, and
capture reads. `SqliteJournal::is_frozen` reports `false` for ordinary unknown
cells, including cells settled by `recover_stopped`.

A trusted backend lifecycle failure is different: when a record carries
`backend_error`, the journal freezes the session until a matching
`HostExecutionSettlement` repairs it. Ordinary capability evidence cannot clear
that failure.

## Stopped-executor scope

`StoppedScope` keeps `stopped_scope`, `settle`, and `finish` exclusive across
connections and session VMs. `thaw_stopped_session` rejects only a live
`Running`/`Yielded` cell or a record with `backend_error`; historical
`OutcomeUnknown` records do not block a thaw. `finish` requires no active
session VM and invalidates reads before `ReadCoordinator::thaw`.

## Tests

```sh
cargo +1.95.0 test -p cayw-cells --locked
cargo +1.95.0 clippy -p cayw-cells --all-targets --locked -- -D warnings
```
