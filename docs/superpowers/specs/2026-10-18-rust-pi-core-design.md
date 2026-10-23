# Rust pi 内核设计

本次交付以本机 `@earendil-works/pi-agent-core` / `pi-ai` 1.3.0 的消息与循环语义为参考，独立编写 Rust 实现。提供可以离线运行的内核，不把会话持久化、IPC、Luau、Device 或完整 pi 应用宣称为已交付。

## 分层与接口

- `cayw-ai`：统一消息、模型、工具声明、用量、流事件、参数校验、transcript 中的 system/工具变化，以及 OpenAI Chat Completions HTTP 流适配。
- `cayw-agent`：有状态 Agent、可克隆的运行控制句柄、异步事件订阅、工具执行、steering/follow-up 队列和请求/轮次/工具回调。
- Provider 接收拥有所有权的请求与取消令牌，返回 `AssistantStream`。失败与取消通过流终态表达。Agent 对提前结束、缺失终态与 Provider panic 再做保护。
- 工具通过 Rust trait 显式注册。内核不预置任意命令执行能力。运行状态通过快照读取；只允许空闲时替换模型、工具与历史。

## 行为约束

1. 一轮模型输出结束后执行工具，把结果写入历史，再请求模型。assistant 的 message_end 早于工具回调。
2. 默认并行：参数准备和 before 回调按调用顺序执行；工具结束事件按完成顺序；结果消息按模型调用顺序。任一工具要求顺序时，整批顺序执行。
3. 严格解析完整 JSON 对象，并按 JSON Schema 校验；不隐式转换类型。需要转换的工具可实现 prepare_arguments。截断响应中的工具一律不执行。
4. 未知工具、无效参数、工具错误和 panic、回调错误转为明确错误结果。取消中的工具结果标识执行结果未知，避免把停止等待误报为副作用回滚。
5. steering 在已发起工具批次完成后消费；follow-up 在自然停止时消费。两种队列均支持单条或全部消费。continue 在空历史上报错且不丢队列；assistant 尾部需要队列输入。
6. prepare_request 在每次请求前调用；finish_turn 在 turn_end 前调用；prepare_next_turn 在下一 turn_start 前调用。finish_turn 的 end 不消费队列，continue 只保证一次后续请求。错误和取消始终退出。
7. 订阅回调按注册顺序等待；agent_end 回调完成前仍属运行中。丢弃运行 Future 会取消令牌并释放运行状态，不能永久卡在 busy。
8. 工具进度只在本次执行存续时接收；完成后关闭进度入口。成功结果可携带 structured_content，after 回调替换 content 时默认丢弃旧 structured_content。
9. system 内容、命名 sections 与工具增删保存在历史；HTTP 请求根据历史折叠当前工具与 system 内容。模型切换不重用其他模型的思考内容。
10. 第一版用标准 OpenAI Chat Completions 请求与 SSE 响应。本地服务验证分片、工具增量、用量、错误、取消和提前断流；没有真实 Provider 验证时明确标为未验证。

## 验收与限制

实现离线示例及覆盖调度、取消、工具配对与流解析的测试。执行 fmt、workspace tests、clippy 和示例；本机 macOS 记录实际结果，可用时补 Linux。Windows 未运行不得标为通过。真实模型或付费调用须另有明确授权。

本实现保留上游 MIT 版权通知，CAYW 自有部分仍为 Apache-2.0。不会复制 WinWinCode/Codex 源码。此次不提交或推送源码，除非用户另行要求。GitHub #1 的 SDK/IPC/持久化验收保持未完成；领取任务的实施说明已整理为本地更正稿，远端替换被自动审批拒绝，尚未发布。
