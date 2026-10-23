# Rust pi Core Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 交付独立 Rust 模型/工具循环，离线示例和可复跑验收。

**Architecture:** `cayw-ai` 定义模型消息和流接口，并实现一个 HTTP 协议。`cayw-agent` 消费流、执行显式注册工具并维护状态和队列；网络适配不依赖 Agent。

**Tech Stack:** Rust 2024、Tokio、futures、serde、reqwest、jsonschema。

## Global Constraints

- 本原型声明并验证 Rust 1.95+；未测试更早版本，不宣称依赖的最低版本已单独推导。
- CAYW 自有代码 Apache-2.0；保留参考 pi 1.3.0 的 MIT 通知。
- 不依赖 Codex，不提供默认通用命令工具。
- 本计划的基线只用离线数据与本地 HTTP 服务。之后用户另行授权“接上 magpie 拿 ds flash 测试”，新增真实模型检查的证据独立保存在 `docs/validation/rust-pi-core-magpie.md`。
- Windows/macOS/Linux 仅记录实际运行证据。
- 不提交、推送或更改远程自动化。

---

### Task 1: 消息、工具声明和流接口

**Files:** `Cargo.toml`, `crates/cayw-ai/{Cargo.toml,src/lib.rs,src/types.rs,src/transcript.rs,src/validation.rs,tests/contracts.rs}`, `third-party/pi-MIT.txt`。

**Interfaces:**

```rust
pub type AssistantStream = futures::stream::BoxStream<'static, AssistantEvent>;
pub trait Provider: Send + Sync {
    fn stream(&self, request: ModelRequest, cancel: CancellationToken) -> AssistantStream;
}
```

- [x] 定义 `Message`, `Content`, `AssistantMessage`, `ToolCall`, `Model`, `ModelRequest`, `AssistantEvent`, `Usage`, `StopReason`。
- [x] 测试 system sections 和工具增删折叠、完整 JSON 对象解析、外部 schema 引用拒绝。
- [x] 实现 transcript 和 schema 校验，运行 `cargo test -p cayw-ai --test contracts`。

### Task 2: OpenAI Chat Completions 流

**Files:** `crates/cayw-ai/src/providers/{mod.rs,openai.rs,sse.rs}`, `crates/cayw-ai/tests/openai.rs`。

**Interfaces:** `OpenAiProvider::new(base_url, api_key)` 实现 `Provider`。请求体只由 `ModelRequest` 产生，不读取本机凭据。

- [x] 用本地 TCP HTTP 服务发送分片 SSE；核对请求中的 messages/tools 与工具结果 ID。
- [x] 实现文本/思考/工具参数增量、usage、finish_reason、HTTP 错误与取消。
- [x] 缺少结束标记、坏 JSON、缺少 finish_reason 输出 error；取消输出 aborted；验证所有测试离线。
- [x] 运行 `cargo test -p cayw-ai`。

### Task 3: Agent 状态、工具执行与队列

**Files:** `crates/cayw-agent/{Cargo.toml,src/lib.rs,src/types.rs,src/tool.rs,src/agent.rs,src/execution.rs,tests/agent.rs}`。

**Interfaces:** `Agent::new(Arc<dyn Provider>, Model)`；`prompt`/`continue_run` 异步返回本轮消息；`control()` 在运行期间插入消息、取消、等待空闲；`subscribe` 注册异步监听器。`AgentTool` 定义参数准备和异步执行。

- [x] 测试模型→工具→模型闭环和生命周期顺序。基础循环的首轮集成测试在实现后运行；未知副作用、队列丢失等回归另有先失败再修复记录，未将全部测试宣称为测试先行。
- [x] 实现快照、运行围栏和 await listeners；tool batch 按完成顺序发结束事件，按原顺序写历史。
- [x] 测试未知工具、参数错误、截断、panic、顺序覆盖、取消、进度关闭和 Future 丢弃。
- [x] 实现单条/全部 steering 与 follow-up，测试 assistant 尾部 continue 和空历史不消费。
- [x] 实现 prepare_request / finish_turn / prepare_next_turn / before_tool_call / after_tool_call；测试 end 保留队列、continue 不重复请求、错误硬退出。
- [x] 运行 `cargo test -p cayw-agent`。

### Task 4: 示例、文档和验收

**Files:** `crates/cayw-agent/examples/offline.rs`, `README.md`, `AGENTS.md`, `docs/development-map.md`, `docs/validation/rust-pi-core.md`。

- [x] 编写不访问网络的实际工具示例：`cargo run -p cayw-agent --example offline`。
- [x] 更新文档，把已实现内核与后续 Luau/IPC/持久化区分，原 Issue 保持未完成。
- [x] 执行 `cargo fmt --all -- --check`、`cargo test --workspace`、`cargo clippy --workspace --all-targets -- -D warnings`、示例；新发现失败修复后重跑有关检查。
- [x] 记录版本、平台、输入、权限、测试数量和未验证项；检查 diff，不自动提交。
