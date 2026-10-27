# F16 wire — refresh indexed symbols on workspace content change (2026-10-26)

**Branch**: `feat/f16-symbol-graph`<br>
**PR**: https://github.com/changw98ic/CAYW/pull/48<br>
**Scope**: audit remediation — already-indexed implementation paths must re-publish into the bridge when on-disk content digest changes (not only first appearance).

## Commands

```bash
cargo test -p cayw-codeintel --locked
cargo test -p cayw-trace-debug --test symbol_graph --locked
cargo test -p cayw-trace-debug --locked
cargo clippy -p cayw-codeintel -p cayw-trace-debug --all-targets --locked --no-deps -- -D warnings
```

## Delivered

- `feed_plan_from_workspace`: if a plan path is already indexed, compare on-disk `content_sha256` to the bridge entry; republish via `publish_under_workspace` when they differ (ingest / revise / `refresh_symbol_hangs` all share this path)
- Unchanged digests are skipped so host/manual outlines are not overwritten by the lightweight producer on every pass
- Test: `workspace_file_change_refreshes_indexed_symbols_on_reingest` — index → modify → `refresh_symbol_hangs` → FileVersion bumps, new symbols appear, removed symbols gone from hangs

## Gaps (still out of slice)

- No continuous filesystem watcher; refresh is plan-driven on ingest/revise/`refresh_symbol_hangs` (not background mtime polling)
- Full Host `codeintel.refresh` tool wiring remains on F04 / #42
- Non-Rust languages still publish empty symbol lists (file hang only)
