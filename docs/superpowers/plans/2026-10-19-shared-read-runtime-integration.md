# Shared read integration plan

> Continue the user-authorized workflow implementation. This slice implements the missing shared read connection in Issue #5; it does not close that Issue or authorize real model calls, publishing, or Git writes.

**Goal:** Two independently authorized `fs.read` calls in one Runtime attempt can use one immutable file capture while each retains its own Core, Cells and Host records. A cooperating write from any session invalidates the prior generation.

**Architecture:** All calls go through the original `CellBackend::call`. Runtime uses the existing Cells read coordinator after each call has its own fixed scope and Host admission. An owned completion guard publishes a leader result only after its own Cells terminal commits. Host provides an explicitly frozen file view and a root-relative read/write generation fence, using its held directory handle. No second cache or loader is added.

**Scope:** `cayw-host`, `cayw-cells`, then `cayw-runtime`, with one integration writer for all three after the previous Runtime writer freezes its closure. Later consumers migrate to the new public Rust contract; no compatibility implementation of the old read bypass remains.

## Constraints

- Actual user authorization remains “使用 workflow 把所有功能都实现”. Implement working code, then verify; do not ask again whether to proceed.
- Use exact known paths, manifests and module declarations. CodeGraph has no CAYW index and already said not to call again in this session. No replacement code-search command, initialization or upgrade.
- No real models, paid services, credentials, new MCP connections, process bypass tools, Git writes, publishing or configuration changes.
- No Core/Device/Workspace/Harness edits by this writer. The prior Runtime Bridge task must be stable and frozen first. Root Cargo.lock belongs to Main.
- Keep Core sequence zero-based and Cells sequence one-based with checked conversion. Preserve execution, task, full lease, parent, source hash, exact arguments, original grant and original request receipt.
- Unknown results freeze further work. Existing known terminals cannot be replaced. Cancellation polls the original future; no automatic retry, leader promotion or detached capture.
- macOS results establish only macOS evidence. Other platforms must reject unsupported paths explicitly or remain unverified.

## Task 1: Host owned read lifecycle and workspace fence

Known files: `crates/cayw-host/src/lib.rs`, `src/filesystem.rs`, `src/catalog.rs`, plus new focused modules/tests declared from those files.

- Factor the existing validation, argument/result budget, permission, audit-start, settlement and Drop guards so owned read tickets use the same checks as ordinary Host calls. Convert the borrowed response budget to owned shared state; do not copy a permissive second execution pipeline.
- An owned non-Clone read ticket retains the original scope, child token, deadline, budget reservation and audit identifier. The ticket can outlive the local scoped Host.
- Register only `fs.read` as a reusable frozen read. Capture exact bounded UTF-8 bytes under the held root using nofollow regular-file reads. SHA-256 describes those exact bytes, not a later reread.
- Expose a stamp containing actual workspace identity, incarnation, generation and a digest of the actual read schema/configuration/bounds. Identity comes from the held root, never an ambient path reopen.
- Use one root-relative stable fence file and a separate durably replaced state file. Reads retain shared fence ownership; potentially mutating calls take the exclusive fence. Acquire without blocking the async executor; honor original deadline/cancel.
- Before dispatch, a writer durably advances generation and records the original mutation as pending. Only a confirmed terminal plus durable audit can clear that marker. Unknown, dropped audit or failed persistence leaves it pending and blocks reuse. Check overflow/corruption instead of wrapping.
- All mutation routes must participate: file writes/patches, Workspace changes/recovery, named process profiles and unclassified external/module operations that may alter the root. Where an external route cannot be covered yet, explicitly disable reuse for it rather than silently assume it is read-only.
- A read ticket releases the Host execution lane before waiting for another read. Neither a writer waiting for read leases nor reader settlement can hold a mutex/lane required by the other. Retain the read lease through Host/Core settlement and Cells acknowledgment.
- Provide a trusted exact-evidence recovery method for pending fences. Match stamp, original audit identifier, capability and argument digest under stopped-owner/Device cleanup evidence. No model-facing clear/reset boolean.
- Specify that reuse is an explicit frozen file view in a cooperating Host generation. Arbitrary outside editors ignoring Host fences are not observed automatically. Do not substitute mtime/size for this contract or claim a whole-workspace transactional snapshot.

Acceptance: two admitted tickets have distinct audits and shared budget charges; one can capture and another consume its frozen value. A write from another independently opened Host waits for live readers, advances generation even for identical content and cannot reuse an old result. Held-root rename/symlink rejection, cancel, Drop, audit/storage faults and pending recovery remain checked.

## Task 2: Cells original-call acknowledgment and coordinator

Known files: `crates/cayw-cells/src/backend.rs`, `bridge.rs`, `cache.rs`, `types.rs`, `settlement.rs`, `manager.rs`, `lib.rs` and exact Cargo target tests.

- Add `ReadOrigin { execution: CellExecution, sequence: u64 }` and an owned Backend response containing result, optional DependencySnapshot, and an exactly-once completion guard. The guard acknowledges only after original Cells intent dependencies/result commit. Drop reports abandonment and freezes reuse; no fallible durable write is deferred to acknowledgment.
- Change `CellBackend::call` to return that owned response. Bridge persists Pending as today, always calls Backend, then commits the returned dependencies and terminal atomically in its original intent before acknowledging. Keep deep JSON disposal safe on every rejection/Drop.
- Remove the old registered-read bypass and `ReadBackend`/`ReadAuthorization` interface. Migrate all genuine users to the single new contract.
- Refactor the existing coordinator rather than adding another cache. It receives explicit trusted policies and per-call ReadSource objects. Sharing keys contain the same CellIdentity/permissions, capability, exact canonical arguments, workspace identity/incarnation/generation and semantics digest. Original cell/sequence belongs in evidence, not the sharing key.
- Before SHA exists, the key is a rendezvous for one capture. The leader captures immutable bytes, returns a candidate and publishes it only when its own Cells terminal is acknowledged. Followers wait, then record their own dependencies/terminal and acknowledge independently. Never broadcast oversized/unvalidated values.
- Cancellation checks precede already-ready hits. A follower cancels only its own wait. A cancelled/dropped leader stops that original capture and wakes followers with known read failure; no promotion/retry. Unknown freezes the session; restart drops ephemeral entries. Entry/ticket/epoch exhaustion must fail closed or choose uncached original execution, never wrap into a stale entry.
- Extend HostCallSettlement with exact optional dependencies and validate them before replacing any unresolved intent. Existing known snapshots/results are immutable. Update existing constructors in this package.

Acceptance: two origins and one capture; publication waits for leader Cells commit; leader/follower cancellation; no successful hit after invalidation/freeze; exact recovery dependencies; all existing Cells behavior and deep-input safeguards remain verified. Standalone tests must exercise the actual Backend call path rather than the removed bypass.

## Task 3: Runtime integration and all callers

Known files: `crates/cayw-runtime/src/lib.rs`, `cells.rs`, `requests.rs`, `execution.rs`, `recovery.rs`, existing `audit.rs` and `bridge.rs`.

- Preserve Bridge integration. Each capability still creates its original Core plan, CallScope, Request admission and budget charge.
- For trusted `fs.read`, create its independently admitted Host ticket, then invoke the existing coordinator inside Backend. Only leader performs byte capture. Every participant settles its own Host audit/Core terminal and original Cells terminal before returning bytes to Lua.
- Retain exact original GrantRecord and Request Admission/receipt across permission/lock/shared-result waiting. Recheck those objects after waits and before accepting success. New grants cannot replace them. Generalize the existing single-future request polling helper to cover the original ordinary grant without constructing another execution future.
- Append bounded canonical `runtime_shared_read` evidence with original full origin, role/source, stamp, dependencies and result digest before the Core terminal. This is attached to the existing capability, not a second effect journal.
- The owned response completion retains the original ticket and coordinator candidate. Cells commit failure invokes persistence_failed, cancels the run, leaves queue capacity for recovery and freezes reuse. It must not overwrite already-known Core success.
- Recovery checks read evidence against original Core plan/start/authorization/terminal, repairs exact Cells dependencies through StoppedScope, settles any pending Host generation fence only with original stopped-owner facts, then thaws/releases admission last. Never reopen current file content to guess an old answer.
- Migrate other workspace consumers of the replaced Rust APIs based on compiler diagnostics and exact known targets. Root lock changes, if necessary, go to Main. Avoid new dependencies where existing ones suffice.

Acceptance: real offline Runtime -> Cells/Luau -> Host `fs.read` scenario shows two different cells/parents/sequences, two grants/audits/terminals and exactly one capture; warm reuse still spends each call's budgets. Another Session/Runtime connection's write changes generation and triggers new capture. AllowOnce, expired/revoked original grants/receipts, cancel/drop, cursor/wait, persistence faults, restart and no-replay recovery have actual assertions.

## Task 4: Verification and handoff

- Run package-specific fmt/check/all-target tests/strict Clippy for changed packages, offline and locked, using Rust1.95 and /Volumes/ORICO target/TMPDIR.
- Run full workspace check/tests after required caller migration. No real Provider invocations.
- Retain the failed observations and corrected runs; use original assertions as applicable. Fix an actual failure rather than weaken permissions or unknown handling.
- Freeze the final production dependency closure, record before/after hashes, exact commands/platform/config/input/permission/outcomes and limitations in a local validation record. An independent verifier then tests this fixed slice only.
- Return source ownership and concise API signatures, tested counts, evidence paths and any actual blockers. Do not mark Issue#5 complete or claim untested platforms.
