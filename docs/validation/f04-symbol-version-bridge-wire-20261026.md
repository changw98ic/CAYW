# F04 wire — producer + Host/Trace/editor shared bridge (2026-10-26)

**Branch**: `feat/f04-symbol-version-bridge`  
**Scope**: audit remediation — real producer, Host tool entry, one shared bridge instance.

## Commands

```bash
cargo test -p cayw-codeintel --locked
cargo test -p cayw-host --test codeintel --locked
cargo clippy -p cayw-codeintel -p cayw-host --all-targets --locked -- -D warnings
```

## Delivered

- Producer: `publish_bytes` / `publish_path` / `publish_outline_json` + lightweight Rust outline
- Host tools: `codeintel.refresh`, `codeintel.definitions`, `codeintel.file_version`, `codeintel.symbols`
- Shared `SymbolVersionBridge` via `Clone` (`HostConfig::codeintel`, `EditorConfig::codeintel`, `serve_with_codeintel`)
- Trace POST `/api/codeintel/definitions` and `/api/codeintel/file-version` when bridge attached

## Not in this wire

Full LSP, impact cascade, SLICE E auto symbol attachment on Trace nodes.
