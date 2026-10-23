# Crash Recovery Implementation Plan

> **For agentic workers:** Use the already loaded executing-plans and test-driven-development guidance. Work only in `/Volumes/ORICO/cayw-issue7-integration-20261022-a`. Keep Issue #6 sealed. Do not commit, push, create a PR, merge or modify Issues.

**Goal:** Complete Issue #7 on macOS ARM64 with repeatable real process kills, original effect evidence, trusted reconciliation and execution ownership checks.

**Architecture:** Reuse Runtime recovery, Task locks, Cells stopped scopes, Core append-only events and Requests authority. Add missing identity and evidence validation to these paths. Different stores retain their actual commit order; failed repair keeps the original work blocked.

**Tech Stack:** Rust 1.95, Tokio, SQLite, Luau, public Rust SDK and authenticated loopback IPC. Fixed offline Provider responses drive real local effects. No paid model calls are authorized for this Issue.

## Constraints

- Scope is the frozen `issue7-contract.json` and `acceptance-scope.json` in `/Volumes/ORICO/cayw-issue7-evidence-20261022-a`.
- Source baseline is Issue #6 inventory `379d4e9854e6122a639a46238c33652b30251e754a1720007ae8c96d51ea4859`, 477 files. The checkout also contains inherited uncommitted Issue #5/#6 changes.
- Searches use CodeGraph at `/Volumes/ORICO/CAYW`; verify known files in this checkout because its contents differ from the index.
- No UI, plugin, remote or other platform features. Only necessary source consumers migrate to the canonical public contract.
- Preserve raw failures and unique evidence files. Never equate an exception, Future Drop or disconnect with a killed process.
- Do not use PID absence alone as proof that the original executor and Device resources stopped.
- No universal shell tool is exposed to the model.

## Current local checkpoint

The seven frozen criteria now have repeatable macOS ARM64 evidence. The independent 40-window SIGKILL matrix, six receipt-based reconciliation scenarios and 73 unchanged refusals retain their r01/r02 builds. Requests repair has three retained kill windows and an independent three-window reproduction; SDK/IPC terminal ordering has four retained scenarios and an independent four-scenario reproduction. Original artifacts were inspected, including 4,492 rehashed supplemental files. The final r07 source inventory is `8ae5331813036af69c99904fbbd0b08224be35b252b62faad63e98d44fbb5a5f`: all fifteen workspace/client checks exited 0, with 1,516 default workspace tests passed, 0 failed and 7 ignored; independent clients passed 10 and 3 tests separately. Historical programs keep their actual build identities and are linked by exact source revisions. Only the roadmap, this plan and the final validation record change after r07 tests. The final delivery and readiness receipts are the authority for the subsequent sealing checks; this plan is not a substitute for those results. No real model, paid model or GitHub write was performed.

## 1. Reproduce and fix stale cancellation

Files: Runtime `lib.rs`, `queue.rs`, `requests.rs`, `rpc.rs`; SDK/IPC consumers and their tests; `tests/recovery_controls.rs`.

- [x] Run the existing Runtime test target and bind its raw log to the baseline: 7 passed, 0 failed; source stable.
- [x] Add a real authenticated IPC test: complete the original Task, enqueue a replacement in the same Session, submit the old Session-only cancel. Confirm failure because it currently returns `cancelled=true`.
- [x] Replace Session-only cancellation with an explicit target containing the original Task and its current Worker/Attempt/fence identity. Unclaimed queued work has an explicit absent execution identity.
- [x] Check the complete target while holding lifecycle coordination, before closing Requests or signalling an active token. Queue writes select only the original Task and identity.
- [x] Migrate consumers to capture the original Task identity. No weak legacy RPC remains. Stale/malformed targets must not change either Task, events, authority or audit.
- [x] Exercise both completed-before-cancel and cancel-before-completed orders through SDK and authenticated IPC, with continuous Core event cursors and a single durable terminal.

## 2. Bind trusted reconciliation and preserve original messages

Files: Core `types.rs`, `store.rs`, `store/capabilities.rs`; Runtime `recovery.rs` and `rpc.rs`; existing Core/Runtime reconciliation tests.

- [x] Reproduce acceptance of insufficient nonempty evidence, wrong expected origin and contradictory reliable Cells evidence before changing product behavior.
- [x] Require expected original identities and structured evidence through one canonical contract. Bind authorization to the trusted host; IPC cannot self-assign an actor.
- [x] Validate reliable original receipts and contradictions before committing Core. Preserve the existing frozen, exact-retry recovery for failures after a commit.
- [x] Reproduce the original tool-result projection changing after reconciliation. Preserve original messages and event prefixes; append the trusted decision separately. Continued execution must consume an explicitly reconciled history without replaying the tool.
- [x] Cover known success, known failure, not executed, insufficient and contradictory evidence, full identity mutations, reopen and identical versus changed retries.

## 3. Recover a stopped Requests authority explicitly

Files: Requests `authority.rs`, `store.rs` and Runtime authority opening/recovery paths; public SDK/IPC query and administration boundary as required.

- [x] Reproduce a real write/commit failure or kill leaving the authority pending/stopped and preventing Runtime reopen.
- [x] Provide read-only inspection and a trusted explicit repair/replacement path retaining original Session/Task evidence and revoking all old grants. Never delete the authority marker to bypass it.
- [x] Require stopped original owners and verify original storage identity. Partial repair stays blocked. Cover repeat repair, stale authority and failure during repair.

## 4. Real process-kill matrix

Files: Runtime `tests/crash_recovery_matrix.rs`, `tests/crash_reconciliation.rs`, `tests/fixtures/recovery.rs`; test-only fault points in AI/Core/Cells/Runtime; repository-external command and original-record audit scripts. These tests use the public Runtime/IPC/Host path directly; they do not claim to run through VerifyTool.

- [x] Bind named points to actual before/after durable commits: Cells intent, Core plan, Core start, effect, Core capability result, Cells call result/ack, Cells VM result, Core execution result, parent result, Attempt terminal, Requests terminal and Queue terminal.
- [x] Use a fixed offline Provider, real Host authorization and independently durable local effects with original identity and entry/effect counts.
- [x] At every reachable point, wait on a deterministic checkpoint, read original records, kill the actual child, wait for its exit, then reopen the same Session. Preserve raw databases/WAL, requests, output, effect and exit evidence.
- [x] Known results remain original; unstarted work requires explicit new execution; started uncertain work remains unknown. No automatic prompt, VM or effect replay.
- [x] Check a live original Worker is rejected before kill. Check stale results, approvals and cancellation against a later original Session attempt.
- [x] Cover committed result before receipt using an IPC handler gate after `Runtime::call` returns, then kill before sending the original response. Reconnect only to query the original Task.

## 5. Combined failures and full delivery

- [x] Reuse and rerun existing Core/Cells/Requests/Queue write-failure and Device cleanup-failure tests; add missing recovery-tail and kill/reopen combinations.
- [x] Independently reproduce key kill, stale-owner and partial-commit cases using saved programs from the recorded frozen builds. Inspect raw artifacts, bind unchanged product/build inputs through the exact revision chain, and separately run all final r07 source checks. Do not relabel historical programs as r07.
- [x] Run formatting, workspace check/test, Clippy and offline example on the delivered source. Run the independent headless client checks after API migration.
- [x] Record source/build/configuration/platform/input/permissions, each injected point, actual exit, raw records and effect decision. Bind exact changes relative to the sealed Issue #6 inventory.
- [x] Update the Issue #7 validation record and local roadmap only after all frozen criteria have repeatable evidence. GitHub remains unchanged.

This plan is not acceptance evidence. Baseline and failing-test results are retained under `/Volumes/ORICO/cayw-issue7-evidence-20261022-a`.
