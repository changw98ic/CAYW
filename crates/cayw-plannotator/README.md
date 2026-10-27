# cayw-plannotator

CAYW 批注 / 计划审阅反馈（F08 首刀，SLICE I）：在计划、Markdown、diff、本地页就地批注，生成绑定 **Session + 目标 + 文件版本** 的结构化反馈，并注入 Agent **后续** attempt。

版本不匹配时**拒绝静默套用**——旧批注不会在新内容上默默生效。

## 首刀范围

- `FeedbackStore`：进程内记录 `StructuredFeedback`
- `apply_feedback_for_attempt`：按 live 文件版本核对后产出 Applied / Reject
- `inject`：`Applied` → Agent SystemMessage section `cayw-plannotator-feedback`
- Runtime 计划接线：`session.plan` 返回服务端内容摘要；`plannotator.submit` 携带读到的 Task、版本和摘要，与计划更新共用协调；`plannotator.list` 读取记录，下次 `session.prompt` 核对后注入
- 持久化：Core session control event `plannotator_feedback_recorded`（协作账本同路径）
- 决策：`Approve` / `DenyWithFeedback` / `ApproveWithNotes`
- 目标种类：`Plan` / `Markdown` / `Diff` / `LocalPage`
- 协议版本：`PLANNOTATOR_PROTOCOL_VERSION = 1`

可选 WebUI 工作台提供当前计划的审阅与批注。读取新计划后需要重新审阅，旧意见不会自动移到新版本。本 crate 本身不渲染界面；Pi 扩展安装、完整 TUI 批注编辑器及其他目标的产品界面不在当前范围。

## 快速使用

```rust
use cayw_plannotator::*;

let mut store = FeedbackStore::new();
let fb = store.submit(draft)?;
let outcomes = apply_feedback_for_attempt(&store, &ctx)?;
```

## 来源与边界

- 功能：F08（[#33](https://github.com/changw98ic/CAYW/issues/33)）
- 采用方式：设计参考（非代码移植）
- 参考：[backnotprop/plannotator](https://github.com/backnotprop/plannotator)、[@plannotator/pi-extension](https://pi.dev/packages/@plannotator/pi-extension)
- 记录：[`docs/sources/f08-plannotator-feedback.md`](../../docs/sources/f08-plannotator-feedback.md)

## 验证

```sh
cargo test -p cayw-plannotator --locked
cargo clippy -p cayw-plannotator --all-targets --locked -- -D warnings
cargo run -p cayw-plannotator --example offline_feedback --locked
```

测试为本地纯函数 / 内存存储，无网络、无 Provider、无真实 UI。
