# Rust pi 内核

本文记录最初 `cayw-ai` / `cayw-agent` 内存内核的接口与当时范围，不代表当前工作区仍缺少持久化、Luau、权限或 IPC。当前实现和 Issue #1 验收见 [项目说明](../README.md) 与 [SDK/IPC 验收](validation/issue1-sdk-ipc.md)。

`cayw-ai` 与 `cayw-agent` 是 CAYW 的第一层可运行 Rust 内核。参考本机安装的 `@earendil-works/pi-ai` / `pi-agent-core` 1.3.0，独立实现统一消息、模型流和工具循环；不依赖 Node.js、pi 包或 Codex。参考来源的 MIT 通知保存在 [third-party/pi-MIT.txt](../third-party/pi-MIT.txt)，CAYW 自有代码仍为 Apache-2.0。

## 两个库如何配合

```text
宿主 → Agent.prompt → Provider.stream → 模型流事件
                  ↓ 工具调用
            AgentTool.execute → 工具结果 → 下一次 Provider.stream
```

- `cayw-ai`：`Message`、`Content`、`ToolDefinition`、`Model`、`Usage`、`AssistantEvent`、`Provider`；system 内容/sections/工具增删重放；完整参数解析与 JSON Schema 校验。
- `cayw-agent`：`Agent` 和 `AgentControl`，只在内存中保存历史；显式注册 `AgentTool`；异步订阅 `AgentEvent`；通过 `AgentHooks` 介入请求、工具和轮次。
- `cayw-ai::providers::OpenAiProvider`：OpenAI Chat Completions 的 HTTP/SSE 实现。调用方显式提供 base URL 和 key，库不读取环境变量、登录文件或本机凭据。

离线示例是一条真实执行 Rust 工具的链路，模型决策来自固定测试数据：

```sh
cargo run -p cayw-agent --example offline --locked
```

示例源文件是 [`crates/cayw-agent/examples/offline.rs`](../crates/cayw-agent/examples/offline.rs)。本地 HTTP 全链测试是 [`crates/cayw-agent/tests/http_loop.rs`](../crates/cayw-agent/tests/http_loop.rs)：真实 HTTP 请求 → 分片流 → Rust 工具 → 带配对工具结果的第二次请求 → 最终回答，不访问外部模型。

## Provider 接口

```rust
pub type AssistantStream = futures::stream::BoxStream<'static, AssistantEvent>;

pub trait Provider: Send + Sync {
    fn stream(&self, request: ModelRequest, cancel: CancellationToken) -> AssistantStream;
}
```

流携带文本、思考和工具调用的开始/增量/结束事件，最后为 `Done` 或 `Error`。Agent 对缺失终态、Provider panic、重复工具调用编号和不一致的结束原因再做检查；不执行错误或被截断响应中的工具。

`OpenAiProvider::new(base_url, key)` 返回 `Result`，会在 base URL 后追加 `/chat/completions`。默认客户端不自动重试或跟随跳转；`with_client` 注入的客户端由宿主负责策略。创建事件流不会立刻联网，开始消费才发请求。流被丢弃时停止请求。

HTTP 适配当前仅支持单个 choice、function 工具、文本和可选用户图片。工具图片转成文字占位；思考内容不写回历史。旧 `function_call`、音频和 refusal 明确返回错误。SSE 支持跨网络分片的 UTF-8、CR/LF/CRLF、多行 data、注释和尾事件。单事件和工具参数限制为 8 MiB；协议缺少 finish_reason 或 `[DONE]` 时返回错误。用量中缓存与普通输入分开，reasoning 是 output 子集，不重复相加。

这不是任意兼容服务的支持承诺。Anthropic、Gemini、OAuth、自动模型目录、价格计算、Provider 协议切换尚未实现。`ThinkingLevel` 是请求值，不表示任意模型都接受该档位。

## Agent 与控制

```rust
let agent = Agent::new(provider, model);
agent.set_tools(tools)?;
let control = agent.control();
let result = agent.prompt(Message::user("hello")).await?;
```

- `prompt` / `prompt_messages`：开始一次运行，直到自然停止、出错、取消或达到轮次上限；返回本次新增消息和最后的 assistant 消息。
- `continue_run`：继续已有历史。空历史或仅 system/custom 的历史报错且不消费队列。assistant 尾部需要 steering 或 follow-up 输入。
- `snapshot`：模型、完整历史、是否运行中、流式消息、未结工具调用、错误和待核对标志。
- `AgentControl::steer`：当前模型和已发起工具批次结束后注入；不跳过当前工具。
- `AgentControl::follow_up`：模型自然停止时消费。两类队列默认每次一条，也可 `QueueMode::All`。
- `AgentControl::abort` / `wait_for_idle`：请求取消和等待全部结算。
- `set_model`、`set_stream_options`、`set_tools`、`replace_messages`、`set_hooks`、`reset` 仅空闲可调用。同一 Agent 同时最多一次运行；默认最多 64 次模型请求，可用 `set_max_turns` 修改。

`subscribe` 接收异步回调，按注册顺序等待，`agent_end` 回调处理完成后才变为空闲。回调可以读快照、排队或取消；不要在回调内部等待同一次运行空闲，也不要启动并等待同一 Agent 的新 prompt。回调应尽快返回并响应取消；内核不会为了取消而跳过等待中的存储回调。监听器错误会取消运行，其他监听器仍收到结算事件，prompt 返回 `AgentError::Listener`。

丢弃 prompt Future 会取消子令牌并释放 running 状态，但不会凭空完成尚未处理的事件。被丢弃的运行不能作为持久化恢复证据。排队消息在写入历史前若遇到回调失败或 Future 被丢弃，会放回原队列。

## 工具与回调

`AgentTool` 提供模型可见声明、可选参数准备、可选执行顺序和异步 execute。所有能力由宿主显式注入，库没有 Bash、Shell、Node、Python 或文件执行入口。

参数必须是完整 JSON 对象，并严格通过 JSON Schema；不会把字符串数字等隐式转成其他类型。工具可以在 `prepare_arguments` 显式转换。Schema 外部引用被拒绝，编译声明不会读取网络或文件。工具进度通道有界，已接收更新在结束前处理，执行结束后的迟到更新返回 false。

默认工具并行执行；参数准备与 before 回调先按原顺序完成。任一工具要求 `ExecutionMode::Sequential`，整批都顺序执行。工具结束事件按实际完成顺序发出，工具结果按模型调用顺序写入历史。所有结果都设置 `terminate` 才停止自动模型后续请求。

| 回调 | 时机与结果 |
| --- | --- |
| `prepare_request` | 每次模型请求前；用户消息已提交，可返回替换的上下文/模型/选项 |
| `before_tool_call` | 参数校验后；assistant message_end 已发出，可阻止调用并指定原因 |
| `after_tool_call` | 工具结束前，可逐字段替换结果；替换 content 时未显式保留的 structured_content 被丢弃 |
| `finish_turn` | 结果入历史后、turn_end 前；End 不消费队列，Continue 只保证一次后续请求 |
| `prepare_next_turn` | turn_end 后、下一 turn_start 前，可替换上下文或加入消息；期间新增 steering 会再次检查 |

工具声明与实际可执行工具分开。system 消息保存工具增删，Agent 请求前同步实际注册工具的变化；HTTP 请求折叠当前 system 和工具集。自定义消息保留在历史中，默认不发送模型；宿主可在 prepare_request 中显式转换或裁剪请求上下文。

## 取消与未知结果

取消是停止本次等待，不保证文件、远程请求或外部进程的副作用被撤销。工具真正开始后被取消，或 execute panic，结果保守标为 `outcome_unknown`。工具主动返回结果也可设置该字段。

只要发现未知结果，当前运行停止，后续 prompt/continue/reset 都会被拒绝；不会让模型自动重试。宿主核对真实副作用后，才通过 `replace_messages` 提供已配对、没有未知标记的历史。`replace_messages` 是宿主的明确核对边界，不是运行时自动恢复。普通 `execute` 的 `Err` 表示工具能确认失败；无法确认是否执行成功时，应返回带 `outcome_unknown` 的错误结果。

尚未实现磁盘日志、Session/Worker 身份、崩溃恢复、版本化 IPC、统一审批存储、执行围栏和进程树治理；本原型不是完整的 #1 或 #6 验收。Luau CodeMode 仍是下一层统一编排方向，没有引入 pi 的 JavaScript 执行环境。

## 验证入口

[离线验收记录](validation/rust-pi-core.md) 列出 macOS / Linux 的 58 项测试、命令和缺口。普通 `cargo test` 使用固定数据或 loopback HTTP，不请求真实模型。

另外已按用户授权，在 macOS 上通过 Magpie 0.1.1285 和 `group/auto-deepseek-v4-1-flash` 完成 4 项真实检查，实际响应模型为 `deepseek-v4.1-flash`：流式文本、Rust 工具往返、后续轮次记住工具值、中途取消。使用现有 `OpenAiProvider` 和 `Agent`，不需要修改内核协议；这不是 Luau CodeMode 测试。详细结果和原始日志见 [真实模型验收记录](validation/rust-pi-core-magpie.md)。

显式运行入口（会发送真实请求，可能计费；需先获得授权）：

```sh
MAGPIE_BASE_URL=http://127.0.0.1:3425/v1 \
MAGPIE_MODEL=group/auto-deepseek-v4-1-flash \
cargo run -p cayw-agent --example magpie --locked
```

运行前在当前环境设置 `MAGPIE_API_KEY`。示例不读取其他 Agent 的认证文件，也不打印凭据。每次 prompt/直接流检查限时 90 秒，模型请求最多输出 1024 或 2048 token；Agent 每次运行最多 3 轮。工具只读取进程内生成的合成值，没有文件、Shell 或外部副作用。取消只证明本地请求和 Agent 状态结算，不能证明上游停止计费。
