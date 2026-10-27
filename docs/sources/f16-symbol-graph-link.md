# F16 来源记录：Trace 符号挂载（SLICE E）

## 标识

| 字段 | 值 |
| --- | --- |
| 功能编号 | F16（剩余：工程文件/符号/内容版本关联） |
| 总规划 | [#33](https://github.com/changw98ic/CAYW/issues/33) |
| 承接 | [#31](https://github.com/changw98ic/CAYW/issues/31) Trace Debug |
| 切片 | SLICE E — `feat/f16-symbol-graph` |
| 采用方式 | 设计参考组合（非代码移植） |
| CAYW 实现 | `cayw-trace-debug::symbol_graph` + 复用 `cayw-codeintel` |

## 参考来源

| 项目 | 链接 | 使用范围 |
| --- | --- | --- |
| dsh-maze（原 dsh-trace-compare） | https://github.com/lamost423/dsh-maze | 运行检查器 / 证据跳转的产品形状 |
| pi-lens | https://github.com/apmantza/pi-lens | 符号/文件版本供 Trace 与 Agent 共用的形状；不复制 LSP / impact cascade |
| #31 用户需求 | Issue 正文 | 真实 trace → 架构/节点图；错误摘要回链原始证据 |

未引入 Node 画布运行时，未做「未声明节点计划的任意程序自动构图」。

## 本切片交付

- `NodeSymbolHang` / `ErrorEvidenceLink`：节点挂载文件版本、可选 `SymbolId`/`SourceLocation`，失败步骤回链证据游标与 diagnostics
- `SymbolGraphLinker`：从节点 `implementation`（`path::symbol` / `path#symbol` / 纯路径）解析并查询 F04 桥接；非路径实现（如 `luau`）不挂载
- `TraceStore::open` / `open_with_workspace`：默认产品入口创建桥并挂上工程根；`ingest` / `revise` 先经 codeintel 生产者（`publish_under_workspace` / 轻量 Rust outline）把 `implementation` 路径写入桥，再经 linker 写入 `Project::symbol_hangs` 并持久化；`open_with_bridge` 仍供 Host 注入共享桥；`refresh_symbol_hangs` 在桥接版本变化后更新
- 烟测：`tests/symbol_graph.rs`（含 ingest 路径）、`tests/http.rs::ingest_symbol_hangs_surface_through_http_state`
- 验收笔记：`docs/validation/issue-33-f16-symbol-graph.md`

## 明确未交付

- 任意程序自动构图 / 跨文件语义级联
- 完整 React Flow 画布改版
- F17 局部重算（见 SLICE D / PR #39）
