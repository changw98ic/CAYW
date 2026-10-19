# cayw-codemode

`CodeMode::new` captures the host's advertised capabilities. `CodeMode::execute` runs Luau in a fresh sandbox. `CapabilityHost` owns permissions and side effects; `CodeModeTool` exposes the single `execute_code` agent tool. Scripts use `cayw.discover`, `cayw.describe`, `cayw.call`, `cayw.emit`, `cayw.null` and `cayw.array`. The sandbox has no ambient filesystem, process or network access.

The wall time limit covers setup, compilation, script execution, argument preparation and awaited host calls. The runtime checks cancellation and the deadline again after preparing arguments, immediately before first polling a host call. An execution stopped before dispatch has no call entry or unknown effect. A call that has started but has no settled response remains `outcome_unknown`; dropping its future cancels its child token but cannot undo external effects.

Host JSON results pass a nonrecursive walk before serialization, copying or Lua conversion. The walk checks the 64-level depth limit, exact numeric representation and remaining JSON payload budget. Rejected results are drained without recursive destruction, including unchecked siblings. A host success is recorded before these checks, so an omitted or invalid result retains `Succeeded` in the audit.

`Limits::output_bytes` is shared by arguments, host results and errors, emits, the return value and retained diagnostic text. JSON escaping counts toward the budget. Original host errors must fit before any audit error text is retained. Every retained error message is limited to 2 KiB of UTF-8; a final diagnostic is shortened further to fit the remaining payload budget, or is empty if no nonempty prefix fits. The report's `output_bytes` includes retained diagnostics. Report metadata, including error kind, call status and `outcome_unknown`, remains available when text is omitted. Call names are separately limited to 256 bytes and the number of audit records is bounded by `host_calls`.

Run the offline tests from the repository root:

```sh
cargo +1.95.0 test -p cayw-codemode --locked
cargo +1.95.0 clippy -p cayw-codemode --all-targets --locked -- -D warnings
```

`tests/regressions.rs` exercises dispatch after expensive argument preparation, bounded error messages and budget exhaustion. Its deep-value test launches the test executable as independent child processes and constructs 10,000-level arrays and objects through the public `CapabilityHost` API. It also checks rejection with an invalid sibling and with a tiny budget, and verifies that the returned audit can be copied, serialized and dropped safely. These are offline fixtures. macOS is the local validation platform; Windows and Linux still need separate runs.

The runtime cannot preempt synchronous Rust work inside an uncooperative host, recover a host's own panic, or undo a host side effect. The host must honor cancellation and reconcile unknown effects before retrying.
