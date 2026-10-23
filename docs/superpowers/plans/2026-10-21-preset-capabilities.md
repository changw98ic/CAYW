# 预置 CodeMode 能力实施计划

> **For agentic workers:** 使用当前会话的 executing-plans 指引逐项实施。分块共享同一交付目录，文件归属由主代理协调。未经用户明确要求不提交、推送、合并或关闭。

**Goal:** 完整完成 Issue #5 的八组能力，实现公开 Rust SDK 与认证本机 IPC 共用的受控执行和验收。

**Architecture:** 持久运行只注册 Cells 的 execute_code。所有有权限含义的子操作均经 CellBackend、Core 原调用记录、Host 校验/预算/授权/开始/执行/结算；进程继续复用 Device。MCP 资源复用宿主注入的 Bridge，本机子任务复用同一 Runtime 的队列和持久记录。

**Tech Stack:** Rust 1.95、Tokio、SQLite、Luau；macOS arm64 实际执行。

## 全局约束

- 基线 d6d8977249acdd8b54a2bba3db49edbb67465651，分支 feat/issue5-preset-capabilities。
- 完整实现八组，不进入 #6，不新增真实模型调用。
- 所有代码搜索用 CodeGraph；在当前独立克隆核对具体源码。
- 不改 UI、plugin、远程、CI、部署、许可证或用户配置。不删除原目录和证据。
- 不新增旧接口兼容分支。Core v3 与 IPC v1 的层级差异保持。
- 每项新行为先执行可复现的失败测试，随后实现并重跑。所有日志放在仓外 issue5-evidence 目录。

## 实施顺序与文件归属

- [x] 1. 单元与会话值：cayw-cells 的 bridge/backend/journal 和相关测试。store/load 保留会话版本，改为逐次受控能力调用；不同任务须按当前授权读取，原权限不会随值继承。输出和目录保持已有实现。
- [x] 2. 完成检查：cayw-runtime 新 completion.rs、execution.rs 与 tests/completion.rs。最终回答前核对实际 VM、连续输出消费、非零退出与未结调用；不能自动重跑或丢弃输出。
- [x] 3. 共享读取：Runtime revoke 显式失效；tests/shared_read_invalidation.rs 补真实写入、撤销对照；tests/shared_read_authority.rs 核对原授权、栅栏等待和每次读取预算。只对显式注册为可共享的 fs.read 启用合并，其他读取仍走普通 Host。
- [x] 4. Git：Host 新 git.rs、process.rs 及 tests/git.rs。补 blame、stage、commit、branch、worktree。宿主选择程序与身份、固定根、禁止 hooks/filter/网络和外部路径，参数无任意 argv。真实前后状态验收。
- [x] 5. 终端：Host 新 terminal.rs 及 tests/terminal.rs。terminal_handoffs 显式启用、固定 profile、幂等输入和连续输出，Device 持有原进程和资源。关闭和取消真实 join。主代理接目录、配置、Runtime 进程归属。
- [x] 6. MCP 资源：cayw-bridge 目录内新增 ListResources/ReadResource、资源 manifest 与来源/载荷校验及 tests/resources.rs；主代理接 Host/Runtime 的名称、Schema 和原 dispatch 证据核对。
- [x] 7. 人机协作：Runtime 新 collaboration.rs 及 tests/collaboration.rs。澄清请求绑定完整原调用身份，答复只来自 SDK/认证 IPC；计划更新以版本和原任务归属持久记录。模型内容不能生成用户答复或授权。
- [x] 8. 图像输入：Runtime 新 input.rs，queue.rs、execution.rs、rpc.rs 和 tests/input.rs。唯一规范任务输入为类型化 Text/Image 内容，现有文本入口包装同一路径。校验模型声明、MIME、编码、大小；持久化后按原内容传入 Provider。
- [x] 9. 本机子任务：Runtime 新 children.rs 及 tests/children.rs。宿主显式启用，记录父子 Task/Session/Attempt；子任务独立精确授权并共享受限预算，父取消传播且等待真实清理。队列容量不足立即拒绝，不能父等子形成死锁。
- [x] 10. 完整能力矩阵及组合验收：docs/issue5-capabilities.md、docs/validation/issue-5-preset-capabilities-macos.md 与 Runtime 专项集成测试。每行说明参数、结果、权限、资源、预算、取消/失败、SDK/IPC 及实际证据。
- [x] 11. 格式、workspace 编译/测试/严格 Clippy、离线示例；独立复跑关键越权、分页、批次失败与取消；核对差异和原始日志。

## 验收记录规则

每轮记录命令、输入、源码差异和具名文件摘要、工具链/系统/实际架构、退出码与日志摘要。旧测试源码只作候选入口，实际运行后才登记通过。合成 Provider、真实本机执行与真实模型证据分开；本项不使用真实模型。清理未确认或记录失败保留 draining/unknown，不能为结束任务释放容量。

2026-10-21 完成上述 11 项本地交付和验收。最终工作区测试为 1453 passed、0 failed、7 ignored；格式、编译、严格 Clippy 和 Agent 离线示例退出 0。关键越权、分页、批次失败和取消由独立执行者复跑 24/24。原始日志、源码清单、忽略项及范围限制见[完整验收记录](../../validation/issue-5-preset-capabilities-macos.md)和[能力矩阵](../../issue5-capabilities.md)。最终命令结束后只更新交付文档，产品与测试源码保持不变。未提交、推送、合并或关闭 Issue。
