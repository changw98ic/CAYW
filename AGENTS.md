# AGENTS.md

本文件为在本仓库工作的 AI 编程助手提供指引。

## 仓库现状

CAYW（Code As Your Wish）是正在开发的 Rust 原生、模块化 Agent Runtime，用来逐步替换 WinWinCode 对 Codex 内核的依赖。最初参考 pi 1.3.0 独立编写 `cayw-ai` 和 `cayw-agent`，随后已加入持久化 Core、公开 Runtime/IPC、Luau/Cells、Host、Device，以及权限申请、宿主能力接入、界面和可选模块等实现。根 workspace 当前有 20 个成员；实际名单和 feature 以 Cargo 清单为准。

**当前首发只面向 macOS。** 2026-10-19 用户明确取消本阶段的 Windows、Linux 实现与验收要求；不再为这两个平台开展适配、探针、环境排查或测试，也不以其缺口阻塞首发交付。后续支持独立跟踪于 [Issue #19](https://github.com/changw98ic/CAYW/issues/19)，创建任务不表示现在恢复平台工作。已有平台研究仅保留为历史记录。当前执行证据覆盖 macOS ARM64，不能外推到未经验证的 macOS 架构或场景。

- **已有实现不等于完整 Issue 验收。** SDK、持久化、Luau、Device 等已有离线实现与局部测试，不能再写成全部尚未实现；也不能把这些测试外推为全部功能或所有 macOS 架构通过。
- 原始内核范围见 `docs/rust-pi-core.md`，离线基线见 `docs/validation/rust-pi-core.md`，有限真实模型测试见 `docs/validation/rust-pi-core-magpie.md`。后续 Core 修复见 `docs/validation/core-boundary-repairs.md`，Runtime 宿主接入见 `docs/validation/runtime-bridge.md`，SDK/IPC 早期离线场景见 `docs/validation/sdk-ipc-harness.md`，Issue #1 的独立客户端、跨实例取消修复和完整默认 workspace 回归见 `docs/validation/issue1-sdk-ipc.md`；Workspace 读取接口和限制见 `crates/cayw-workspace/README.md`，Host 接线回归见 `crates/cayw-host/tests/workspace_reads.rs`，diff 文件结束标记修复见 `docs/validation/workspace-diff-eof.md`，共享读取接线与独立复核见 `docs/validation/shared-read-runtime.md`，Device 交互进程及主树清理验证见 `docs/validation/device-interactive-process.md`，网页、审批、取消与断线恢复验证见 `docs/validation/webui-runtime.md`。每份记录只证明自己的冻结源码和平台，最新组合必须核对新命令及原始日志。
- Issue #1 的独立 Rust 客户端、Session/事件契约和跨实例取消修复已接主树，macOS ARM64 离线交付通过独立复核及主树复跑；按当前首发范围，#1 的本地功能验收完成。完整默认 workspace 测试通过 1,185 项、失败 0 项、忽略 5 个入口；其中四个为父测试调用的子进程入口，一个远程二进制场景本次未运行。七项启用全部 Runtime feature 的模块测试、严格 Clippy、所有目标/feature 编译和离线示例通过；这些测试有重叠，不相加。完整 workspace 所有 feature 的执行仍未验证。
- 持久化 Runtime 的真实 Provider 全链属于 #6，尚未验收。共享读取已接入并单独复核；Device 交互进程与共用启动关闭修复已接主树，81 项 Device 测试及 5 项 Runtime 清理测试通过。External Module 统一进程清理的独立交付仍有测试失败，长会话读取优化已交回但尚未接主树；两者均未包含在 Issue #1 本地验收中。#1 已于 2026-10-19 按用户要求关闭，Linux/Windows 后续支持由 #19 单独跟踪；#1 实现、独立客户端和验收说明随本次提交交付。其余 GitHub Issues 未因此关闭。
- 不要为了“占位”创建空 crate、空目录或空的架构骨架；只在交付真实可运行能力时加代码。
- 不复制 WinWinCode / CodePatchBay 的旧代码；可借鉴分析过的设计，但代码和许可证不自动继承。
- 最终运行和依赖中不得包含 Codex；普通开源依赖可以使用。

## 开发命令与内核边界

需要 Rust 1.95+。在仓库根目录执行：

```sh
cargo fmt --all -- --check
cargo check --workspace --locked
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo run -p cayw-agent --example offline --locked
```

单个测试可用：

```sh
cargo test -p cayw-agent --test agent model_tool_model_loop_preserves_events_and_history
cargo test -p cayw-agent --test http_loop actual_http_adapter_and_agent_complete_tool_round_trip
cargo test -p cayw-ai --test contracts validation_is_strict_and_local_only
```

`cargo fmt` 需要 rustfmt，`cargo clippy` 需要 Clippy。当前开发机默认 stable 没有 Clippy，但已安装的 `1.95.0` 工具链有，可执行 `cargo +1.95.0 clippy ...`。如 PATH 没有 Cargo，可在命令环境中加入 `$HOME/.cargo/bin`；不要误判为 Rust 未安装。

真实模型检查是独立示例：`cargo run -p cayw-agent --example magpie --locked`。需先获当次真实调用授权并在环境中设置 `MAGPIE_API_KEY`，可选 `MAGPIE_BASE_URL`（默认 `http://127.0.0.1:3425/v1`）和 `MAGPIE_MODEL`（默认 `group/auto-deepseek-v4-1-flash`）。普通 `cargo test` 不会运行它。配置和已验证范围见 `docs/validation/rust-pi-core-magpie.md`；不打印凭据，不将中途取消的用量零值当作零费用。

- `cayw-ai::Provider` 接收请求和取消令牌，返回可消费的事件流；错误必须有终态。HTTP 适配不读取本机模型凭据，默认不重试或跟随跳转。
- `cayw-agent::Agent` 维护内存中的历史；`AgentControl` 可在运行中读取快照、插入消息和取消。事件订阅按注册顺序等待；回调不要等待同一次运行的 `wait_for_idle()`，以免等待自身。
- 工具全部由宿主显式注册。参数严格校验，不隐式转换；有转换需要时实现工具自己的 `prepare_arguments`。工具可以进度更新，整批结果按调用原顺序入历史。
- `outcome_unknown` 表示可能已发生副作用；当前运行和后续运行都停止，`reset()` 也不能清除该状态。宿主核对后通过 `replace_messages()` 提供完整且已配对的历史。工具的普通 `Err` 应表示已知失败；无法确认副作用时必须返回带 `outcome_unknown` 的结果。
- `cayw-agent` 的运行 Future 被 Drop 时会取消令牌并释放其内存运行状态；这不证明外部副作用已撤销。持久化 Runtime 必须另外核对 Core/Cells/Requests 记录和原执行者身份，等待 Device 清理确认后才能释放执行名额；清理或持久化失败时保留未知状态，不自动重放。
- Core 协议/存储当前为 v3；`cayw-ipc` 的帧协议当前为 v1。这是不同层的版本，不能为统一数字而修改。Core 能力序号从 0 开始，Cells 从 1 开始，转换必须检查边界。
- Workspace 执行器本身不是权限边界，不能直接暴露给模型；经 Host 完成严格校验、授权、调用开始、预算和终态记录后再执行。`fs.grep`/`fs.glob`/`code.outline`/`code.diff_hunks` 只有这一条实际分发路径。
- 根目录句柄固定工作区；路径被重命名后不能重新按外部路径打开来换根。Unix 进程组不等于安全沙箱；macOS 的 PTY 与进程清理按对应 Issue 验收。Windows Job Objects 不在当前首发范围。

## 文档分工

- `README.md`：项目方向、计划中的模块边界、CodeMode 工具策略、当前优先事项。
- `docs/development-map.md`：19 个 GitHub Issues 的分阶段路线图（A–E 及后续平台支持）、硬依赖表和 mermaid 依赖图。
- GitHub Issues（https://github.com/changw98ic/CAYW/issues）：每个任务功能范围、交付和验收的权威来源；当前平台范围以本文件记录的用户最新决定为准。路线图维护依赖、阶段、入口及本地状态，远端 Issue 未同步时须明确说明。

修改路线图时：
- 依赖表和 mermaid 图必须同步更新；新增依赖前确认目标 Issue 存在且不形成环。
- 取消或拆分任务时同步修正所有相关链接，不留悬空引用。
- 状态只有在对应交付和可复跑验收证据完成后才能从“计划”改为“已完成”。
- `README.md` 与路线图中重复出现的优先级描述（先做 Rust pi 内核，再推进 Stage A；#6 全链不抢跑）要保持一致。

## 架构约束与后续方向

- **Headless Core 为中心**：Core 独立运行，不依赖 UI、Server、Harness 或可选工作流模块。TUI / WebUI / Server / StrongFlow / JEv / Fusion / Observer 都是可插拔的，普通 Chat 不依赖它们。
- **先做 Rust pi 内核，再夯实 Stage A**：消息/模型流/工具循环已有有限真实往返记录，SDK/IPC、持久化、Luau、Device 和预置 CodeMode 也已进入实现与局部验证。仍按 #1 → #2 → #3/#4 → #5 的底座要求补齐验收，再完成 #6 的真实模型、统一权限、持久化和取消全链；早期原型或单项测试不能替代这些交付。
- **CodeMode 是统一工具编排入口**：优先用 Luau 实现（Monty 只作可选对照），底层由受控的 Rust 能力执行；所有执行路径共用权限、调用配对、取消和预算。
- **默认不向 Agent 暴露 Bash/Shell/Node/Python 通用执行入口**。常规文件、检索、Git、构建/测试需求由预置 CodeMode 能力覆盖；真实缺口按任务申请开放（#8），申请次数不能自动授权。这条限制只针对 Agent 的工具暴露，不限制开发本项目时使用的工具链。
- **Session 与 Worker 解耦**：Session 是持久逻辑，Worker 是可更换的执行者。恢复时先核对 unknown 副作用，不能盲目重放。
- **模块接入两种方式**：可信 Native Module 通过 Cargo feature 静态链接；External Module 通过独立进程 IPC 接入（进程隔离不等于安全沙箱）。
- **MCP / Skills 由宿主统一接入**，Core 不重复创建 loader 或 MCP 连接；CodeGraph 等持久代码索引、跨文件语义分析和外部 search 由宿主提供。Workspace 已有有界文本检索与 Rust 单文件语法大纲，不能把它们称为完整语义索引，也不能在 Core 再建一份。
- **StrongFlow 卸载后**：已绑定的 Session 提示不可用并保持冻结，只提供由用户确认的显式 Fork 转换，不能静默绕过。
- **Device 持有权威内容**；可选 Server 下 Device 只出站连接，无需入站端口。

## 证据与授权约束

- 结项看交付和可复跑验收，不以文档完成或模型自报代替。证据需绑定源码/构建、配置、平台、输入、权限和实际结果。
- 当前首发仅要求 macOS 实现与验收；Windows、Linux 不在本阶段范围，也不是首发阻碍。macOS 内未经执行的架构或场景仍标为“未验证”。不写固定毫秒级的脆弱 CI 断言。
- fixture/合成数据与真实 Provider/任务证据分开记录。真实模型调用或付费测试需要明确授权，默认不运行。
- 本项目使用 Apache License 2.0（`LICENSE`）。引入第三方依赖或借鉴代码时确认其许可证与 Apache-2.0 兼容。
- 不更改许可证、仓库权限、旧仓库状态、网站或外部自动化，除非任务明确授权。
