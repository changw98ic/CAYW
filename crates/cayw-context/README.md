# cayw-context

CAYW 上下文 / token / 费用 / 缓存观测视图（F05 首刀，SLICE F）。

从**原生模型请求账本快照**折叠出会话卡片、跨会话 KPI、请求演进步骤，以及共享预算血缘上的父子 Agent 总览。本 crate **不编造价格**、不假设 tokenizer、不改写持久账本；未知费用保持空值。

## 首刀范围

- 会话：`SessionOverview`（token / cost / cache / 组成 / 演进）
- 跨会话：`CrossSessionOverview`
- 父子：`FleetOverview`（同一 `budget_session_id`）
- 组成：按 Provider 已报 token 分量（input / output / cache_read / cache_write / reasoning）
- 协议版本：`CONTEXT_PROTOCOL_VERSION = 1`

本切片**不**交付：消息类别组成（user / inject / skill …）、Insights 热力图、TUI/WebUI 渲染接线、Runtime `model_requests` 直接导出（后续可经同一视图层接入）。

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
