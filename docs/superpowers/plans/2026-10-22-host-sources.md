# Host Sources Implementation Plan

> **For agentic workers:** Apply the existing executing-plans, test-driven-development and verification-before-completion guidance. Continued local implementation and validation are authorized. Do not commit, push, create a PR, close an Issue or start real-model/paid calls. Do not restart the main workflow.

**Goal:** Deliver Issue #9 on macOS ARM64 through the public SDK and authenticated IPC, with an actual local MCP child and actual Markdown Skill files.

**Architecture:** Keep Registry/Bridge as the injected source boundary and Runtime as the existing durable execution path. Add a real `cayw-sources` host package that prepares one bounded stdio connection or an explicit file Skill source, then transfers its execution handle to Registry. Core never constructs either adapter. Runtime continues deriving original Session/Task/attempt/call identity, enforcing Host permissions/budgets and persisting source dispatches. Search uses an explicitly injected host callback over the existing bounded Workspace capability, with source provenance, instead of a new index.

**Tech Stack:** Rust 1.95.0, Tokio, Serde, the existing UUID/libc versions where needed, Luau CodeMode, MCP 2026-07-28 stdio, Core v3 and local IPC v1. Existing ordinary dependency versions are retained; licenses must remain Apache-2.0 compatible.

## Global Constraints

- Work in `/Volumes/ORICO/cayw-issue9-integration-20261022-a`, branch `feat/issue9-host-sources`.
- Keep the original I7/I8 checkouts and their seals unchanged. The exact inherited baseline is 500 entries at `385eb8bc07fcf244a682de8647bcfb8ae9bc592fb1ab128225327d07f6841c89`; Git HEAD alone does not identify it.
- Frozen Issue and seven exact conditions: `/Volumes/ORICO/cayw-issue9-evidence-20261022-a/issue9-contract.json`, `acceptance-scope.json`.
- Use only the I9 recorder and target for command evidence. Preserve failures under new names; never rewrite old receipts or logs.
- CodeGraph searches use the indexed main checkout only to locate symbols; read the exact I9 file before editing. No text/file search substitutes.
- Follow macOS-only scope. No UI, plugin, remote service, general model shell or Core index.
- Maintain one canonical MCP protocol; remove the old initialize/initialized implementation rather than retain compatibility.
- Unknown effects stay unknown across cancellation, disconnect, reconnect and reopen. A stopped child does not prove effects were undone.
- SDK host configuration and authenticated local IPC are trusted controls, not OS isolation. File paths and program arguments never come from model text.
- Actual local child/file execution uses fixed Providers; keep it separate from real-model evidence, which remains zero.

## Task 1: Freeze scope and verify existing behavior

- [x] Create the ordinary I9 clone from the complete final I8 source and verify I8 preservation.
- [x] Save the authoritative Issue, baseline and seven acceptance rows.
- [x] Read the old fixture/adapter, Registry/SourceHandle boundary, Runtime bridge journal and relevant architecture.
- [x] Record current MCP references from Context7; protocol is 2026-07-28 stdio with per-request `_meta`, no handshake.
- [x] Run `baseline-bridge-r01`: 48 tests passed, zero failed/ignored; full source unchanged. Raw command/log/manifest are saved externally.
- [x] Read the separate Runtime-only analysis and check its concrete suggested SDK/IPC gaps against actual tests.

## Task 2: Reusable real host sources and canonical MCP fixture

Files: new `crates/cayw-sources/{Cargo.toml,src/lib.rs,src/stdio.rs,src/skills.rs,tests/stdio.rs,tests/skills.rs}`; workspace manifest/lock; the existing Bridge fixture and support/example consumers.

Interfaces: `StdioConfig`, `StdioSource::connect(config, cancel)`, prepared `manifest()`/`server_info()`/`into_parts()`, read-only `StdioControl::snapshot()`/`close()`/`wait_closed()`; `FileSkill`, `FileSkills::load(entries, limits)`, `manifest()`/`into_handle()`, read-only load/read statistics. The source identity remains an explicit trusted host declaration at Registry registration.

- [x] First add real stdio tests that send a versioned `tools/list` without initialize, reject missing/wrong version, and reject initialize. Retain the observed old-protocol failures.
- [x] Migrate the fixture and add the reusable stdio source: explicit absolute executable/cwd, cleared and explicit environment, bounded queues/frames/deadlines, per-request version/capabilities/clientInfo, serverInfo pinning, supported complete metadata pages only, separate resource reads, canonical cancellation and ignored late replies. No automatic reconnect or retry.
- [x] Count actual child starts, transmitted methods, progress, late replies, disconnect and cleanup. Keep a bounded event trace. Transfer exactly one SourceHandle; the control object cannot call tools.
- [x] Add real `.md` file tests before implementation: exact content/digest, list does not return/read bodies, configured size/UTF-8 checks, changed same-size content and symlink replacement refusal, cancellation/revoke and fresh source generation.
- [x] Implement explicit absolute Markdown paths, no body parsing or path construction from model names, bounded regular-file reads with no symlink traversal, immutable content/file identity checks, per-instance owner key and no load/connection creation from Core.
- [x] Replace the old example-only transport with use of the real source adapter. Preserve its lifecycle/ownership regression tests; move fixed Skill fixture behavior to an explicit test utility rather than an old protocol route.

## Task 3: Actual SDK and authenticated IPC delivery

Files: new Runtime `host_sources` integration tests and a runnable host-source example/driver, plus narrow product fixes only after reproduced failures.

Consumes: prepared sources, `Registry::register/reconnect/disconnect/revoke`, `RuntimeConfig.bridge`, existing exact permissions/approvals, `Runtime::bridge_dispatches`, SDK task APIs and the actual IPC server/client.

- [x] Build/launch the actual local fixture with an exclusive recorded build and load actual Skill files. Use the Skill content to select the declared add input; do not merely read and discard it.
- [x] Run SDK and authenticated IPC discovery, schema read, file Skill list/read/use, resources and source-tagged bounded host search. Save source/config/content digests, actual results and original Core/Cells/Host mapping in fresh external directories.
- [x] Assert catalog/name visibility, content access and tool invocation independently; deny without source read or tool effect. Cover wrong token, unknown fields, wrong argument types and forged identity/role/approval fields.
- [x] Exercise cross-Session/Task approval attempts, stale attempt/source pins, equal name with changed version/content/permissions and old authorization refusal. Preserve original and replacement call records.
- [x] Capture Provider inputs to prove bodies/schemas are not injected by default and appear only after the authorized reads.

## Task 4: Real lifecycle and budgets

- [x] Through Runtime SDK/IPC, exercise disconnect before dispatch and during a waiting actual tool, explicit reconnect, cancel, revoke, source removal/replacement and deliberately late old results.
- [x] Assert no reconnect replay, old response cannot settle a new call, original unknown persists and reopen does not clear/replay it.
- [x] Exercise Host call/time/output bounds and stdio message bounds. Bind service errors to original calls. Record conservative unknown results when external effects cannot be confirmed.
- [x] Remove the injected search source: ordinary Chat still runs, explicit missing search reports a clear error, no second index/search service is created.

## Task 5: Independent reproduction and whole-Issue delivery

Files: final validation record, this plan and only the #9 roadmap row; external source/evidence inventories and final seal.

- [x] Independent actual MCP/Skill normal, replacement, denied, cancel and disconnect reproduction, with original raw output and exact source identity. Coordinator reads original artifacts and checks reported results.
- [x] Run focused tests and final fmt/check/workspace tests/Clippy/offline example/default dependency checks, plus relevant independent client checks. Read full logs; distinguish ignored/nested/failed/interrupted runs and warnings.
- [x] Map all seven exact requirements to actual evidence and material limitations; no generated pass field or model report substitutes for originals.
- [x] Write validation and roadmap status only after actual whole-Issue evidence is complete.
- [x] Seal the complete source, exact delta from the I8 baseline, command/fixture/file evidence and failures under new paths. Rehash originals and previous I8 source before declaring local delivery.
- [x] Preserve uncommitted/GitHub-unsynced state and zero real-model/paid calls. Enter #10 only after #9 whole-Issue delivery.
