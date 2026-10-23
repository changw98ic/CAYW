# Runtime 功能实施记录

用户要求：使用 workflow 把所有功能都实现。范围以 GitHub Issues #1–#18 的实际交付、非范围和验收为准；原 Rust pi 内核及 Magpie 验收是起点。

## 实施顺序

1. 核对全部 Issues 和统一接口，写出逐项交付/证据/缺口。
2. 并行实现独立基础：`cayw-core` 持久会话与事件、`cayw-device` 受控进程和准入、`cayw-codemode` Luau 与 Rust 能力桥。各自完成后独立复查。
3. 把权限、预置文件/Git/构建/测试能力、宿主 MCP/Skills 接口与 Agent 接到同一持久运行接口，提供版本化 IPC 服务和客户端；验证取消、断连、游标续订、恢复和未知结果阻止重放。
4. 在同一接口上实现公平调度、附加模型协议、可选 Native/External 模块以及 TUI/WebUI；再实现可选 Server 的出站 Device 协议、StrongFlow 冻结与显式 Fork、已明确职责的扩展、资源档位和可选加密同步。
5. 逐项集成、测试、独立复查、修复。macOS/Linux 保留实际输出；Windows、独立两机和未授权真实 Provider 不以本地模拟充当验收。

这份记录不是完成证明。每个 Issue 的完整验收还取决于真实实现和可复跑证据，不能因本文件或某个代理自报完成而关闭 Issue。

## 接口与文件所有权

- 保留 `cayw-ai` 的 `Provider`/消息/流契约和 `cayw-agent` 的显式工具/取消/事件链；新增功能在真实需要时进入实现 crate。
- 根 workspace 由主代理维护，新 crate 使用 `crates/*` 成员声明；代理不得同时格式化或改写他人 crate。
- `cayw-core` 负责持久状态权威，事件与状态更新同事务提交；Session/Attempt/Worker 独立身份，过期执行者不能结算。
- `cayw-device` 负责进程归属、树取消、输出和资源准入限制；它不是 Agent 的任意命令入口，也不声称安全沙箱。
- `cayw-codemode` 只通过宿主 `CapabilityHost` 异步调用能力；宿主持有权限权威。无限循环、内存、输出、调用次数和取消必须真有限制。
- 后续运行层统一串联持久状态、权限、CodeMode 和 Device；UI/Server/可选模块不能成为普通 Chat 的运行依赖。

## 必须保持的边界

- 不重置既有未提交成果，不提交/推送、改写 GitHub 评论或关闭 Issue。
- 不复制旧仓库代码，不引入 Codex 运行依赖；新依赖核对 Apache-2.0 兼容许可证。
- 不默认给模型 Shell/Node/Python 通用入口；真实能力缺口可申请，次数不等于授权。
- 未知副作用停止运行，宿主核对后才能继续，不盲目重放；取消不等于外部副作用回滚。
- MCP/Skills/CodeGraph 由宿主提供，Core 不重复创建 loader 或连接。
- StrongFlow 卸载时保持会话绑定并冻结，只有用户明确确认才能 Fork 普通会话。
- 本次沿用 Magpie + DS Flash 的有限真实测试授权，不扩展成大规模付费、多 Provider 模型质量/成本实验。
- 不把 JEv/Fusion 未定义的产品职责自行编造；可借鉴经过核对的接口含义，代码独立实现。
