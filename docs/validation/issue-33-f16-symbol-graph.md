# #33 SLICE E / F16 Trace 符号挂载 — 验收笔记

**分支**：`feat/f16-symbol-graph`  
**基线**：`origin/dev` @ `9cc509d`（本分支 cherry-pick F04 / SLICE C 以提供 `cayw-codeintel`）  
**范围**：仅 SLICE E；**请勿自动合并**；不触碰已有并行 PR 的专属文件范围之外的无关改动。

## 命令

```bash
cargo test -p cayw-trace-debug --test symbol_graph --locked
cargo test -p cayw-codeintel --locked
cargo clippy -p cayw-trace-debug --all-targets --locked --no-deps -- -D warnings
```

## 烟测对照

| 要求 | 覆盖 |
| --- | --- |
| 真实任务节点显示文件 + 符号 + 版本 | `real_task_nodes_show_file_symbol_and_version` |
| 错误摘要跳转证据 | `error_summary_deep_links_hang_and_evidence_cursor` |
| 未发布路径不虚构挂载 | `unpublished_path_does_not_invent_hang` |
| 非工程实现不挂载 | `luau` / `local` 由 `parse_implementation` 忽略 |

## 非目标

任意程序自动构图、跨文件影响、F17 局部重算、完整前端画布重做。
