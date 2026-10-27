# F08 来源记录：批注反馈 / Plannotator（SLICE I）

## 标识

| 字段 | 值 |
| --- | --- |
| 功能编号 | F08 |
| 总规划 | [#33](https://github.com/changw98ic/CAYW/issues/33) |
| 切片 | SLICE I — annotation feedback 首刀 |
| 采用方式 | 设计参考（非代码移植、非外部运行时依赖） |
| CAYW 实现 | `crates/cayw-plannotator` |

## 参考来源

| 项目 | 链接 | 许可 | 使用范围 |
| --- | --- | --- | --- |
| plannotator | https://github.com/backnotprop/plannotator | 上游仓库声明为准 | 仅参考计划/Markdown/diff 就地批注、结构化反馈回 Agent、Approve / Deny+notes 产品形状 |
| @plannotator/pi-extension | https://pi.dev/packages/@plannotator/pi-extension | 上游包声明为准 | 仅参考审阅决策回传与 Session 作用域交互，不安装 Pi 扩展 |

未复制上游 TypeScript / React / 浏览器 UI 源码，未引入 Pi / Cordis / DSH 运行时依赖。CAYW 实现为独立 Rust 结构化反馈层：绑定 Session、目标与文件版本；版本不匹配时拒绝静默套用。

## 本切片交付

- `StructuredFeedback` / `FeedbackStore` / `apply_feedback_for_attempt` / `inject`
- 批注一条 → 后续 attempt 可见；版本不匹配拒绝静默套用
- Runtime/Agent 注入：`Applied` → SystemMessage section `cayw-plannotator-feedback`
- 持久化：Core `plannotator_feedback_recorded` control event；RPC `plannotator.submit` / `list`；WebUI 暴露 `session.plan` + 上述 RPC
- 单元测试、Runtime 集成测试与 `offline_feedback` 示例
- 本源记录与 crate README

## 明确未交付（后续切片）

- 计划 diff 高亮、Ask AI / Inbox；当前计划的浏览器审阅与批注已接入可选工作台
- Pi 扩展安装与 `/plannotator-*` 命令
- 完整 TUI 批注编辑器（本切片仅挂现有 `session.plan` / RPC 入口）
- 对 F04 `cayw-codeintel` 的硬依赖（本切片内嵌兼容形状的 `FileVersionBinding`）

## 验收口径（本切片）

本地 `cargo test -p cayw-plannotator` 证明：批注可见于后续 attempt；live 版本不一致时产出显式拒绝而非 Applied。不把本切片标为 F08 全范围完成。
