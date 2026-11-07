# F16 wire — default workspace symbol producer (2026-10-26)

**Branch**: `feat/f16-symbol-graph`<br>
**PR**: https://github.com/changw98ic/CAYW/pull/48<br>
**Scope**: audit remediation — default product startup must feed the ingest/mount bridge from real workspace sources (not an empty bridge with no producer).

## Commands

```bash
cargo test -p cayw-codeintel --locked
cargo test -p cayw-trace-debug --test symbol_graph --locked
cargo test -p cayw-trace-debug --locked
cargo clippy -p cayw-codeintel -p cayw-trace-debug --all-targets --locked --no-deps -- -D warnings
```

## Delivered

- Reused F04-style producer on this branch: `publish_bytes` / `publish_under_workspace` / lightweight Rust outline (no Host crate conflict)
- Default `TraceStore::open` / `open_with_workspace` / CLI `--workspace` | `CAYW_WORKSPACE` attach a workspace root
- On ingest/revise, `feed_plan_from_workspace` publishes missing `implementation` paths into the bridge before `SymbolGraphLinker` hangs
- Tests: `default_open_with_workspace_sources_feeds_bridge_on_ingest`, `default_open_without_matching_workspace_file_still_skips_hang`

## Gaps (still out of slice)

- Full Host `codeintel.refresh` tool wiring remains on F04 / #42 (not duplicated here to avoid cross-PR Host conflict)
- No whole-tree startup scan; producer is plan-driven from the open workspace
- ~~Existing indexed paths not refreshed on edit~~ → fixed in f16-refresh-on-change-20261026.md
- Non-Rust languages publish with empty symbol lists (file hang only)
- EditorConfig / Trace sharing one Host bridge instance is still an F04 product gap
