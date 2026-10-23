# Task Capability Requests Implementation Plan

> **For agentic workers:** Follow the existing executing-plans, test-driven-development and verification-before-completion guidance. The user has authorized continued local implementation and validation. Do not commit, push, open a PR, close an Issue or start paid/model calls.

**Goal:** Complete Issue #8 on macOS ARM64 using the existing Requests, Runtime, Host and Device path, including authenticated IPC and actual controlled project execution.

**Architecture:** The model can request an exact host-configured operation but cannot make a decision. The trusted SDK and authenticated IPC host controls use the same durable Requests state machine. Every admitted call still crosses the ordinary Runtime permissions, budgets, Core/Cells records, Host validation and Device execution. Revocation cancels the original task and retains process cleanup and unknown-outcome rules.

**Tech Stack:** Rust 1.95.0, existing Tokio/Serde/SQLite dependencies, Luau CodeMode, local IPC v1, Core v3, macOS ARM64. No new dependencies are planned.

## Global Constraints

- Work only in `/Volumes/ORICO/cayw-issue8-integration-20261022-a`, branch `feat/issue8-task-capabilities`.
- Preserve `/Volumes/ORICO/cayw-issue7-integration-20261022-a` and its final v2 evidence unchanged.
- The baseline inventory is `f2e0af2ba4a16b2eb54d98ca0ab4bcf1554bbc226cb8dbf4aeeeefda934d5233`, 492 entries. Its inherited uncommitted Issue #5/#6/#7 work is part of this checkout, not Git HEAD and not new Issue #8 work.
- The frozen Issue #8 contract and eight requirement rows are in `/Volumes/ORICO/cayw-issue8-evidence-20261022-a/issue8-contract.json` and `acceptance-scope.json`.
- All code searches use CodeGraph. Its main-checkout output only locates symbols; verify exact I8 files before editing.
- Do not add a general Shell/Node/Python model tool or use a request grant to bypass lower Host limits.
- Do not make `EvaluationTicket`, `AuthorizationReceipt` or `HostActor` deserializable to accept model input.
- Default tests use synthetic Providers. Actual Host/Device programs and file effects must be separately recorded as actual local execution; they are not real-model evidence.
- Use the Issue #8 recorder and target only. Preserve every failed run with a unique label and raw output.
- Do not claim acceptance from source review, a subagent report, compiler success or a generated pass field.

## Task 1: Freeze acceptance and establish the baseline

Files: the external `baseline.json`, `baseline-source.json`, `issue8-contract.json`, `acceptance-scope.json`, `validate.py`; this plan.

- [x] Create an independent ordinary clone from the sealed I7 checkout and verify the entire inventory and I7 preservation.
- [x] Freeze the authoritative Issue body and eight acceptance rows.
- [x] Read Runtime request admission, task settlement, SDK request APIs, IPC dispatch and existing task-request tests.
- [x] Delegate Requests-only and Host/Device-only source review with no edits or test execution.
- [x] Create the I8 recorder with `ROOT` and `CARGO_TARGET_DIR` pointing only to I8.
- [x] Run the Requests package baseline (`baseline-requests-r01`: 56 ordinary tests and 2 compile-fail doctests) and Runtime request regression (`request-entry-green-r02`: 7 task, 5 dynamic, 4 IPC and 3 configuration tests). All passed with unchanged source during each command. The Runtime run follows the IPC/configuration changes and is not a pre-change baseline.

## Task 2: Add trusted IPC controls using the existing state machine

Files: `crates/cayw-runtime/src/rpc.rs`, `crates/cayw-runtime/tests/request_ipc.rs`; public contract documentation after validation.

Consumes: `Runtime::request_operations`, `request_status`, `request_events`, `request_task_state`, `request_evaluate`, `request_approve`, `request_deny`, `request_evaluator_unavailable`, `request_revoke`, `close_request_task`, `enqueue_input_with_intent`.

Produces: strict authenticated `request.operations`, `request.status`, `request.events`, `request.task_state`, `request.decide`, `request.revoke`, `request.close_task` and `task.enqueue_with_intent` RPC methods. A final `request.decide` internally evaluates and then applies one trusted decision with fixed actor `rpc-user`; the opaque Rust ticket never comes from JSON. It is not registered as a model capability. Ordinary `task.enqueue` continues rejecting an injected intent field.

- [x] Write real-loopback tests first for first-request evaluation, no effect before approval, fixed-actor decision, strict fields, immutable duplicate decision, status/events, pre-execution revoke and unauthenticated/wrong-token rejection.
- [x] Preserve `request-ipc-red-r01` (three missing-method behavior failures), then `request-ipc-green-r01` (four passed).
- [x] Add strict dispatch with a private server-side ticket, fixed actor, pre-evaluation reason/lifetime validation and conservative Request error codes. No approval method is a model capability.
- [x] Add launch `requests` settings using the existing `RequestConfig` and `OperationCatalog`. Preserve `request-config-red-r01` (two unsupported-field failures), then its three passed tests in `request-entry-green-r02`; unknown/forged/out-of-scope settings remain rejected.
- [x] Reproduce the default request-event page bug in `request-config-green-ipc-page-red-r01` and use the Requests maximum of 64. `request-entry-green-r02` passed all 19 selected tests with unchanged source.

## Task 3: Prove real project execution and the exact scope

Files: focused Runtime integration tests and an executable local acceptance driver; use existing process profile APIs. Final filenames follow the reviewed Host/Device fixture interface rather than creating a second process executor.

Consumes: exact host `Profile`, `RequestPolicy`, trusted `TaskIntent`, the canonical SDK/IPC controls and original task `CancelTarget`.

Produces: raw per-scenario request/event/task/Host/Device/effect evidence under a fresh external directory, bound to the source and executable.

- [x] `project-program-build-r06` built and immediately preserved the existing `cayw-ai` contracts test in a new exclusive target. `actual-project-test-r06` actually ran its exact test once through SDK and once through authenticated IPC; both returned 1 passed/0 failed. `actual-project-crosscheck-r06` verified identical full r06 source, separate workspaces and original request/receipt/start/settlement identities. Earlier r05 originals retain their recorded identity.
- [x] `request-outcomes-boundaries-r04` passed 3 boundary, 8 outcome and 8 serve-configuration tests. Final r06 workspace execution reran those targets and the original request IPC/task/dynamic tests with zero failures. Allow, deny, original request/evaluation deadlines, unavailable and repeated decisions retain distinct states/events.
- [x] Actual separate Luau cells rejected six forged actor/identity fields, model decision capability, echoed approval and read Skill text. Actual boundary cells rejected generic Shell/Node/Python names and process-field extensions. The Skill text test does not certify the later Skill loader.
- [x] Changed profile/filter/operation and cross-task/Session/attempt/reopen contrasts passed. Existing Requests host-configuration tests in the final r06 run checked configuration/root binding. Program/env binding in source, terminal configuration changes and model field expansion refusals are distinct evidence; no standalone per-field ordinary test-profile host mutation run or OS filesystem sandbox is claimed.
- [x] Deny, expiry and unavailable leave the same task's ordinary authorized read usable. Capability and Host call counts, three time clamps, two output clamps and original cleanup capacity were actually exercised.

## Task 4: Prove revoke/cancel/cleanup using the original task

Files: focused Runtime request/process integration tests and the acceptance driver; product code only if a reproduced defect requires it.

- [x] Revoke before dispatch and assert no child/effect starts; original IPC tests also reject late decisions after end.
- [x] `request-processes-first-r01` passed 8 real process tests. SDK and authenticated IPC revoke both observed the original PID/start identity, stopped that incarnation and retained its earlier file effect.
- [x] Started process cancellation retains Core `OutcomeUnknown`; reopen/recover does not replay or clear its blocking state. Raw stage JSON and database files remain under the command's `request-processes-r01` evidence directory.
- [x] Inject a `processes_stopped` audit failure: the original Task remains Draining, Requests Closing and one slot held; another Session gets QueueFull. Original-owner drain records cleanup but retains the slot; explicit recovery releases capacity while business effects remain unknown.
- [x] The same recorded run exercised three time clamps, two raw-output clamps and a shared Host call budget. This proves the current Host/Runtime process limits and cleanup; it does not prove Device ResourcePermit or #18 memory/resource policy.
- [x] Preserved actual missing-IPC/config behavior failures and the default request-page defect before their repairs. `preserved-failures-r05.json` also records test compile errors, the correctly rejected r04 source-changing driver run and the interrupted r04 full check separately; none is relabeled as a product behavior failure.

## Task 5: Independent reproduction and final delivery

Files: `docs/validation/issue-8-task-capabilities-macos.md`, this plan, the #8 row and validation link in `docs/development-map.md`; external source/build/raw-log inventories and final receipt.

- [x] Independent r06 `request_outcomes` reproduction passed all eight tests, including forged fields/text, cross-task authority and the direct SDK status/revoke contrasts added after the r05 evidence gap. Three saved SDK stages match original SQLite; 141 originals were unchanged. Retained independent r03 outcomes/process execution remains r03, with unchanged product/process/fixture source. It is not relabeled as r06.
- [x] `issue8-final-r06-*` completed all 16 checks at frozen inventory `99126d9c5a24cd3ddcf3730cfad8f8566d99d2c79447f5f752b02a02b3c3c323`, 500 entries. `final-checks-audit-r06.json` verified every full manifest and raw-log digest: workspace 1,543 passed/0 failed/7 ignored; independent clients 10 and 3 passed. Three nested result lines are excluded. Four existing offline-example name-collision warnings are retained.
- [x] The recorded default Runtime tree excludes UI, remote, plugin, VerifyTool and Codex. Actual macOS 27.0.1 ARM64, Rust/Cargo 1.95.0 and build identities are retained per command.
- [x] Read `independent-eight-conditions-final-r06-worker-v1.md` and `.json`, then rehash all 26 references. Read the separate boundary/project report and directly rehash its 674 originals. The reports record no remaining acceptance blocker in their declared scope. Check nine cases and 42 original calls directly against Core/Cells; retain the corrected Queue/Core fence checker diagnostic and original failure.
- [x] Reconcile all eight requirements with actual commands and artifacts in `acceptance-matrix-r06.json` (SHA-256 `8f002486cf878bc23208653733852e37ae842052eb42c3bb0abbca2003ce99f4`). Rehash its 24 distinct references; keep fixed Provider, actual local execution and historical source scopes explicit.
- [x] Write the final [validation record](../../validation/issue-8-task-capabilities-macos.md), including SDK evidence, r06 actual project identities, failures and limits. Update only the #8 roadmap status row; dependencies and graph stay unchanged.
- [x] Seal source, precise baseline delta, raw artifacts and failures in `delivery-candidate-r06-v1.json`; actual `delivery-candidate-readiness-r06-v1.json` exits 0 after rehashing 1,574 originals, eight conditions, 16 commands and all 492 original I7 entries. Preserve this version. The subsequent `final-local-delivery-v1.json` and `final-readiness-v1.json` bind the final three documentation changes; final completion depends on that actual successful readback.
- [x] Record local macOS ARM64 scope, other architectures unverified, zero real-model/paid calls and uncommitted/GitHub-unsynced status. Retain the #9 gate: proceed only after the final delivery and its readiness readback actually succeed.
