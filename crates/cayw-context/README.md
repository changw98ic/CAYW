# cayw-context

CAYW 上下文 / token / 费用 / 缓存观测视图（F05 首刀，SLICE F）。

从**原生模型请求账本快照**折叠出会话卡片、跨会话 KPI、请求演进步骤，以及共享预算血缘上的父子 Agent 总览。本 crate **不编造价格**、不假设 tokenizer、不改写持久账本；未知费用保持空值。

## 首刀范围

- 会话：`SessionOverview`（token / cost / cache / 组成 / 演进）
- 跨会话：`CrossSessionOverview`
- 父子：`FleetOverview`（同一 `budget_session_id`）
- 组成：按 Provider 已报 token 分量（input / output / cache_read / cache_write / reasoning）
- 协议版本：`CONTEXT_PROTOCOL_VERSION = 1`

本 crate 仍是纯折叠层。Runtime 侧通过 `session_ledger_snapshot_from_model_requests` / `Runtime::session_context_overview` / IPC `session.context_overview` 接入同一账本（见 `cayw-runtime`）。

本切片**不**交付：消息类别组成（user / inject / skill …）、Insights 热力图、TUI/WebUI 渲染接线。

## 快速使用

```rust
use cayw_context::*;

let overview = build_session_overview(&snapshot);
let fleet = build_fleet_overview("budget-root", &[parent, child])?;
assert_eq!(fleet.cost.status, CostStatus::Unknown); // 无可信价目时保持空
```

## 来源与边界

- 功能：F05（[#33](https://github.com/changw98ic/CAYW/issues/33)）
- 采用方式：设计参考（非代码移植）
- 参考：[bowenliang123/dsh-context](https://github.com/bowenliang123/dsh-context) 的上下文洞察 / 组成 / 跨会话 / Fleet 产品形状
- 记录：[`docs/sources/f05-context-usage.md`](../../docs/sources/f05-context-usage.md)

## 验证

```sh
cargo test -p cayw-context --locked
cargo clippy -p cayw-context --all-targets --locked -- -D warnings
cargo run -p cayw-context --example offline_overview --locked
```

测试为本地纯函数折叠，无网络、无 Provider、无真实 UI。

上下文规划使用 UTF-8 字节上界估算来安排摘要和压缩，不把该估算当作模型真实 token 数来拒绝请求或裁成固定摘录。主会话仍按既定策略保留记忆、当前请求和活动工作，并选择保留的原始回合；原始持久日志不删除。已有且可核对的摘要/压缩结果仍可替换对应范围，忘记请求和工具调用配对仍生效。配置的模型窗口与输出预留继续用于规划；精确上下文有效性由 Provider 检查，当前未接入模型 tokenizer。
