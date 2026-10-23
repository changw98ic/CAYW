# Issue 1 进度更正稿

此稿尚未发布。2026-10-18，替换 [原领取评论](https://github.com/changw98ic/CAYW/issues/1#issuecomment-6249572398) 的操作被自动审批拒绝：需要对替换该评论的明确授权。远端评论保持原样。以下文字已整理好，供后续授权发布。

---

领取，实施顺序已调整为先做 Rust pi 内核，再在其上推进本 Issue 的 Session / SDK / IPC 交付。此前单 crate `cayw-core` 的实施方案已被替换。

截至 2026-10-18，代码在本地 `pi-core-rust` 分支，尚未提交或推送：

- `cayw-ai`：统一消息与流事件、system/工具声明重放、严格 JSON Schema 参数校验、OpenAI Chat Completions HTTP 流适配。
- `cayw-agent`：多轮模型/工具循环、顺序/并行工具、取消、steering/follow-up 队列、异步事件订阅、请求/工具/轮次回调。
- 工具结果未知时停止当前与后续运行，必须由宿主核对历史；不自动重放副作用。
- macOS ARM64 与 Linux ARM64 各通过 58 项离线/本地 HTTP 测试，离线工具示例通过；macOS 的 fmt、check、Clippy 通过。随后按用户“接上 magpie 拿 ds flash 测试”的授权，在 macOS 上通过 Magpie + DS Flash 完成 4 项真实检查：流式文本、Rust 工具往返、多轮上下文和中途取消。Windows 仍未验证，实际账单未核对。
- 离线验收在 `docs/validation/rust-pi-core.md`，真实模型证据在 `docs/validation/rust-pi-core-magpie.md`，均保留源码 SHA-256、命令与原始输出；参考 pi 1.3.0 的 MIT 通知已保留。

本次内核原型尚不包含持久化 Session/Worker 身份、事件游标恢复、版本化 IPC、审批存储或进程树治理，不能作为本 Issue 或 #6 的完整验收。Issue 保持开放，原有验收项不勾选。
