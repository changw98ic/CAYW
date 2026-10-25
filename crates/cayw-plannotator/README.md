# cayw-plannotator

CAYW 批注 / 计划审阅反馈（F08 首刀，SLICE I）：在计划、Markdown、diff、本地页就地批注，生成绑定 **Session + 目标 + 文件版本** 的结构化反馈，并注入 Agent **后续** attempt。

版本不匹配时**拒绝静默套用**——旧批注不会在新内容上默默生效。

## 首刀范围

- `FeedbackStore`：进程内记录 `StructuredFeedback`
- `apply_feedback_for_attempt`：按 live 文件版本核对后注入
- 决策：`Approve` / `DenyWithFeedback` / `ApproveWithNotes`
- 目标种类：`Plan` / `Markdown` / `Diff` / `LocalPage`
- 协议版本：`PLANNOTATOR_PROTOCOL_VERSION = 1`

本切片**不**交付：浏览器 Plannotator UI、Pi 扩展安装、TUI/WebUI 渲染接线、Core 持久账本。

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
