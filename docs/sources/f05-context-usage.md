# F05 来源记录：上下文 / token / 费用概览（SLICE F）

## 标识

| 字段 | 值 |
| --- | --- |
| 功能编号 | F05 |
| 总规划 | [#33](https://github.com/changw98ic/CAYW/issues/33) |
| 切片 | SLICE F — context / token / cost overview 首刀 |
| 采用方式 | 设计参考（非代码移植、非外部运行时依赖） |
| CAYW 实现 | `crates/cayw-context` |

## 参考来源

| 项目 | 链接 | 许可 | 使用范围 |
| --- | --- | --- | --- |
| dsh-context | https://github.com/bowenliang123/dsh-context | 上游声明（npm `dsh-context`） | 仅参考 Context Insights / Token·Cost·Cache 视图、跨会话卡片、Fleet 父子总览、上下文演进的产品形状 |

未复制上游 TypeScript / React 源码，未引入 Cordis / DSH 运行时依赖。CAYW 实现为独立 Rust 只读折叠层，输入为与 `model_requests` 账本字段对齐的快照；未知费用保持空值，不编造价目。

## 本切片交付

- `SessionOverview` / `CrossSessionOverview` / `FleetOverview`
- token 分量组成、缓存命中率（分母为 0 时省略）、请求演进步骤
- 单元测试与 `offline_overview` 示例
- 本源记录与 crate README

## 明确未交付（后续切片）

- 消息类别组成（user / inject / skill / assistant / tool）
- Insights 热力图 / 7 日费用柱图 / 日历筛选
- TUI / WebUI 渲染接线
- Runtime 公开 API 直接返回本视图（可后续薄封装）

## 验收口径（本切片）

本地 `cargo test -p cayw-context` 证明：父子同屏用量、未知费用不编造、缓存命中率计算、跨会话聚合与异质价目坍缩为未知。不把本切片标为 F05 全范围完成。
