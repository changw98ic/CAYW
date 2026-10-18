# AGENTS.md

本文件为在本仓库工作的 AI 编程助手提供指引。

## 仓库现状

CAYW（Code As Your Wish）计划做成 Rust 原生、模块化的 Agent Runtime，用来逐步替换 WinWinCode 对 Codex 内核的依赖。目前仓库**只有文档**（`README.md`、`docs/development-map.md`），没有源码、没有 Cargo 工程，因此也没有构建、检查或测试命令。等第一批真实代码（Issue #1）落地后，再把实际命令补进本文件。

- 文档中的内核、模块、工具策略都是**计划**，不要描述成已实现的功能。
- 不要为了“占位”创建空 crate、空目录或空的架构骨架；只在交付真实可运行能力时加代码。
- 不复制 WinWinCode / CodePatchBay 的旧代码；可借鉴分析过的设计，但代码和许可证不自动继承。
- 最终运行和依赖中不得包含 Codex；普通开源依赖可以使用。

## 文档分工

- `README.md`：项目方向、计划中的模块边界、CodeMode 工具策略、当前优先事项。
- `docs/development-map.md`：18 个 GitHub Issues 的分阶段路线图（A–E）、硬依赖表和 mermaid 依赖图。
- GitHub Issues（https://github.com/changw98ic/CAYW/issues）：每个任务的范围、交付和验收的权威来源。路线图只维护依赖、阶段和入口。

修改路线图时：
- 依赖表和 mermaid 图必须同步更新；新增依赖前确认目标 Issue 存在且不形成环。
- 取消或拆分任务时同步修正所有相关链接，不留悬空引用。
- 状态只有在对应交付和可复跑验收证据完成后才能从“计划”改为“已完成”。
- `README.md` 与路线图中重复出现的优先级描述（如 Stage A 优先、#6 不抢跑）要保持一致。

## 计划中的架构要点（需要读多处文档才能拼出的部分）

- **Headless Core 为中心**：Core 独立运行，不依赖 UI、Server、Harness 或可选工作流模块。TUI / WebUI / Server / StrongFlow / JEv / Fusion / Observer 都是可插拔的，普通 Chat 不依赖它们。
- **当前优先级是 Stage A 执行底座**：#1 最小 Rust SDK + 版本化 IPC + Session/事件契约 → #2 三平台验证基线 → #3 Luau 语言原型 与 #4 最小 Device 进程树治理并行 → #5 预置 CodeMode 能力集。#6 流式多轮对话闭环要等 Stage A 稳固后再做。
- **CodeMode 是统一工具编排入口**：优先用 Luau 实现（Monty 只作可选对照），底层由受控的 Rust 能力执行；所有执行路径共用权限、调用配对、取消和预算。
- **默认不向 Agent 暴露 Bash/Shell/Node/Python 通用执行入口**。常规文件、检索、Git、构建/测试需求由预置 CodeMode 能力覆盖；真实缺口按任务申请开放（#8），申请次数不能自动授权。这条限制只针对 Agent 的工具暴露，不限制开发本项目时使用的工具链。
- **Session 与 Worker 解耦**：Session 是持久逻辑，Worker 是可更换的执行者。恢复时先核对 unknown 副作用，不能盲目重放。
- **模块接入两种方式**：可信 Native Module 通过 Cargo feature 静态链接；External Module 通过独立进程 IPC 接入（进程隔离不等于安全沙箱）。
- **MCP / Skills 由宿主统一接入**，Core 不重复创建 loader 或 MCP 连接；CodeGraph / AST / search 留在外部宿主能力中。
- **StrongFlow 卸载后**：已绑定的 Session 提示不可用并保持冻结，只提供由用户确认的显式 Fork 转换，不能静默绕过。
- **Device 持有权威内容**；可选 Server 下 Device 只出站连接，无需入站端口。

## 证据与授权约束

- 结项看交付和可复跑验收，不以文档完成或模型自报代替。证据需绑定源码/构建、配置、平台、输入、权限和实际结果。
- Windows / macOS / Linux 三平台验证贯穿所有交付；不可用的平台或场景标为“未验证”。不写固定毫秒级的脆弱 CI 断言。
- fixture/合成数据与真实 Provider/任务证据分开记录。真实模型调用或付费测试需要明确授权，默认不运行。
- 本项目使用 Apache License 2.0（`LICENSE`）。引入第三方依赖或借鉴代码时确认其许可证与 Apache-2.0 兼容。
- 不更改许可证、仓库权限、旧仓库状态、网站或外部自动化，除非任务明确授权。
