# #33 SLICE C / F04 符号·版本桥接 — 验收笔记

**分支**：`feat/f04-symbol-version-bridge`  
**基线**：`origin/dev` @ `9cc509d`  
**范围**：仅 SLICE C 首刀；不合并；不触碰 PR #37–#41。

## 命令

```bash
cargo test -p cayw-codeintel --locked
cargo clippy -p cayw-codeintel --all-targets --locked -- -D warnings
cargo run -p cayw-codeintel --example offline_bridge --locked
```

## 烟测对照

| 要求 | 覆盖 |
| --- | --- |
| 改一文件 → 版本 bump | `publish_bumps_version_only_when_digest_changes` |
| Trace/Agent 拿到同一 symbol/file 版本 | `agent_editor_trace_share_same_symbol_and_file_version` |
| 错误路径回链源码位置 | `diagnostic_deep_links_versioned_source_and_rejects_stale` |
| 不重复 `code.outline` | 桥接只接受 `SymbolInput` 快照，无 syn/解析依赖 |

## 非目标

F04 全量 LSP、跨文件影响、依赖图、Host 工具接线、SLICE E Trace 挂载。
