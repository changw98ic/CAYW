# Headless 多轮 Chat 实施计划

> **For agentic workers:** 按当前会话的 executing-plans、test-driven-development 和 verification-before-completion 指引完成。本地开发已由用户授权持续推进；不自动提交、推送、合并或关闭 Issue。

**Goal:** 完成 Issue #6 所需的可安装 Runtime、独立 Rust 客户端和固定离线全链回归，为另行授权的 Magpie 真实验收提供实际可运行入口。

**Architecture:** 保留同一 Runtime、Agent、Core、Cells、Host 与 Device 执行链。独立客户端通过公开 SDK 或认证本机 IPC 调用；配置只由可信宿主提供。事件消费者保存已处理的连续游标，重接不重新提交 prompt。

**Tech Stack:** Rust 1.95、Tokio、SQLite、Luau、OpenAI Chat Completions SSE；本次只在 macOS arm64 执行。

## 范围与约束

- 交付目录：`/Volumes/ORICO/cayw-issue6-integration-20261021-a`，分支 `feat/issue6-headless-runtime`。
- 基线为 Issue #5 最终交付清单 `472572d54498c2f31177a3fac2131c3cb0083311d985ad4225b1a0fd39124950`，基线提交 `d6d8977249acdd8b54a2bba3db49edbb67465651`。该目录包含未提交的 #5 交付；不将这些内容混称 #6 新改动。
- #5 原目录保持验收时状态。#6 原始条件保存于仓外 `cayw-issue6-evidence-20261021-a/issue6-contract.json`。
- 代码搜索使用主仓 CodeGraph；索引指向主仓时，以已知路径读取本交付目录核对实际源码。
- Runtime 与客户端实际构建关闭 plugin、UI、remote；不依赖 VerifyTool 服务或 Codex。
- 模型路由固定 `group/auto-deepseek-v4-1-flash`；真实协议为 Magpie 的 OpenAI Chat Completions 兼容 API。现有宿主可显式注入该适配器，不新增一套 Runtime。
- 旧真实模型授权只覆盖 #2、#3、#4，且已停用。用户随后另行授权本项“允许验收 使用 magpie DS flash 预算 500m”；新额度独立记账，不挪用旧授权。离线 HTTP fixture 与真实模型证据分开记录。
- unknown、原进程清理、迟到审批与旧 attempt 的约束保持；不因取消返回、进程退出或模型自报而宣称完成。
- 不改 architecture、AGENTS、UI、plugin、远程、系统配置、许可证或外部自动化。

## 任务和文件归属

- [x] 1. 明确模型用量是否实际报告。`cayw-ai/src/types.rs` 与 `providers/openai.rs` 增加 `AssistantMessage.usage_status`（unknown/partial/reported），保留实际计数；缺失不冒充零费用。用本机 HTTP 测试先证明缺口，再覆盖完全缺失、部分、完整零值、分块补齐、错误和取消。
- [x] 2. 接通可信运行配置。`cayw-runtime/src/launch.rs` 提供 `HeadlessConfig::load` 和 `into_runtime_config`；`cayw serve --config` 复用同一入口。JSON 限定 budgets、宿主固定 process profiles、精确 permissions。省略不新增授权，坏配置在创建状态和监听前拒绝。子进程测试核对真实运行、审批和取消。
- [x] 3. 交付独立 `examples/headless-client`。`cayw-client --mode sdk|ipc` 接收真实输入与已有 Session，支持 create/get/tasks/events/prompt/approve/cancel/recover/audit。SDK 显式注入 Provider；IPC 使用地址和环境中的 token。输出 JSONL，保存连续 cursor，Ctrl+C 发取消后继续核对原任务。客户端只用公开接口，不读 Core 私有数据库。
- [x] 4. 运行已安装的 Runtime 与独立客户端，分别完成 SDK/IPC 两用户回合。第一回合经真实 Luau/Rust 文件能力消费结果；第二回合依赖上一回合事实。固定 HTTP 响应与请求记录明确标为离线合成，核对消息、Task/Attempt/Worker、父子调用、外部文件、连续事件、重接与重开。
- [x] 5. 复跑审批允许/拒绝、模型流取消、工具执行取消、Device 清理、执行连接丢失、错误认证/版本/参数/越界以及最小 unknown/恢复。已存在的场景直接复用，按实际执行结果记录，不重复建设 #7 全崩溃矩阵。
- [x] 6. 核对实际安装、依赖图、features、二进制、进程与原始日志。运行格式、workspace 编译/测试/严格 Clippy 和 Agent 离线示例；独立客户端单独编译和回归。独立执行者复跑关键公开接口与故障。
- [x] 7. 完成授权后的真实模型验收运行和有界预算核对。SDK/IPC 各两个真实用户回合、审批拒绝、模型流与工具取消、未知结果拒重放、游标续读、重开及最小恢复均有原件；不自动重试、换模型或挪用旧授权。缺用量保留未知，全部失败与修复成本纳入独立 500M 账本。
- [x] 8. 更新 Runtime/客户端使用说明和本次验证记录。路线图只记录有证据的本地进展，GitHub 未同步则明示；不提前勾选远端验收。

## 证据

每条命令保存原始日志、退出码、系统/架构、工具链、锁文件、features、源码清单与运行期间差异。并行开发期的测试只绑定各自实际源码；最终检查在源码稳定后执行。二进制安装到仓外独立前缀，不覆盖用户全局工具。不复制私人会话或读取凭据。

最终本地结果：默认 workspace 1,480 项通过、0 失败，独立客户端 9 项通过；格式、编译、严格 Clippy 与 Agent 离线示例退出 0。Runtime c 与客户端 d 的 SDK/IPC 安装组合各完成两用户回合，完整场景 18 次本机合成请求；未知会话错误字段已按现有 IPC v1 规则修复。独立文档核对的两处预算表述已修正。证据与范围见[验证记录](../../validation/issue-6-headless-chat-macos.md)。仓外 `final-local-delivery-v1.json` 绑定最终文档、与 #5 的差异及已通过命令。

随后授权真实阶段已取得 106 次实际 HTTP 响应。SDK/IPC 各完成两个用户回合，分别为 10 次和 7 次模型请求；最终取消补跑共 12 次请求。全部请求已报告 231,417 token，缺用量保留 5,500,000 token，未结算为 0。两个成功会话与最终取消原件的只读核证均退出 0。独立控制报告的结论与引用原件已核，11 项交付条件的收据检查退出 0，整项 macOS ARM64 本地验收完成。文档状态更新后的源码与证据绑定于真实目录的 `final-live-delivery-v1.json`；改动尚未提交，GitHub 未同步。旧本地收据的 0 次真实请求保留为历史结果。
