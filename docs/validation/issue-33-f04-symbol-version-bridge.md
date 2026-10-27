# #33 SLICE C / F04 符号·版本桥接 — 验收笔记

**分支**：`feat/f04-symbol-version-bridge`<br>
**基线**：`origin/dev` @ `9cc509d`<br>
**范围**：仅 SLICE C 首刀；不合并；不触碰 PR #37–#41。

## 命令

```bash
cargo test -p cayw-codeintel --locked
cargo test -p cayw-host --test codeintel --locked
cargo clippy -p cayw-codeintel -p cayw-host --all-targets --locked -- -D warnings
cargo run -p cayw-codeintel --example offline_bridge --locked
```

## 烟测对照

| 要求 | 覆盖 |
| --- | --- |
| 改一文件 → 版本 bump | `publish_bumps_version_only_when_digest_changes` |
| Trace/Agent 拿到同一 symbol/file 版本 | `agent_editor_trace_share_same_symbol_and_file_version` |
| 错误路径回链源码位置 | `diagnostic_deep_links_versioned_source_and_rejects_stale` |
| 不重复完整 `code.outline` | 桥接可接受快照；producer 轻量扫描或复用 Workspace `code.outline` |
| Host 工具闭环 | `codeintel.refresh` → `codeintel.definitions`；与 Trace clone 同版本 |
| 共享实例 | `HostConfig::codeintel` / `EditorConfig::codeintel` / `serve_with_codeintel` |

## 非目标

F04 全量 LSP、跨文件影响、依赖图、SLICE E Trace 自动挂载。
