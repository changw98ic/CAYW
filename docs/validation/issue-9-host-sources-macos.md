# Issue #9：宿主 MCP、Skill 与按需代码检索

验收日期：2026-10-22。实际平台为 macOS 27.0.1（26A434）、ARM64，工具链为 Rust/Cargo 1.95.0。其他 macOS 架构未验证。

[Issue #9](https://github.com/changw98ic/CAYW/issues/9) 的七项条件已按原件逐条核对。r18 的十六项完整检查均退出 0，源码前后相同。默认 workspace 为 1,585 项通过、0 失败、7 项忽略；独立 headless-client 为 10 项通过，issue1-client 为 3 项通过。另一执行者在同一份源码上复跑三组 Runtime 来源测试，15 项通过。主线程重新核对其 75 份引用原件，并回读 24 个实际数据库、252 份原件和 18 组来源调用配对。这些范围重叠，不相加为不同测试总数。

模型输入使用固定 Provider。实际 MCP 子进程、Markdown 文件读取、Luau、Host、SDK、认证本机 IPC 和 SQLite 均真实运行。保存后的程序再次通过 SDK 和 IPC 各得到 42，并共用一个服务进程和一个 Skill 加载器。真实模型调用为 0，付费调用为 0，没有使用 #6 的预算。改动未提交，GitHub 未同步。

## 源码与原件身份

| 项目 | 实际身份 |
| --- | --- |
| 源码目录 | `/Volumes/ORICO/cayw-issue9-integration-20261022-a` |
| 分支 | `feat/issue9-host-sources` |
| Git HEAD | `d6d8977249acdd8b54a2bba3db49edbb67465651`；包含未提交文件，HEAD 不单独代表交付 |
| 证据目录 | `/Volumes/ORICO/cayw-issue9-evidence-20261022-a` |
| 冻结 Issue | `issue9-contract.json`；远端 updatedAt 为 `2026-10-19T14:53:51Z`，当时 OPEN |
| Issue SHA-256 | `1f04bf001c247df3cb9a667e9d54d29f0a75fcd69a2cdd23747883062459983d` |
| 完整 #8 基线 | `385eb8bc07fcf244a682de8647bcfb8ae9bc592fb1ab128225327d07f6841c89`，500 条 |
| tested r18 内容清单 | `af5be6e3ad8576ef53e029f86dfcc54deb23a7cd5f80d470773e7de9ad9005ba`，516 条，相对 #8 为 27 个变化路径 |
| tested r18 清单文件 SHA-256 | `d5774e3f117a04825765431c82676c33c26b1c36eb09491b2f35f0897e3f09a5` |
| tested r18 源码 ZIP SHA-256 | `29c06eead6cf1b1b45ee150c5e7fc0c32854308517e658570fba0ef1bd5106a4` |
| Cargo.lock SHA-256 | `2b9c60acc20829fdc16eb4ed7c3e65f14a8e97e6fa25f31bed05a9b9c10b750f` |
| 工具链 | `1.95.0-aarch64-apple-darwin`；rustc `1.95.0 (59807616e 2026-04-14)`；Cargo `1.95.0 (f2d3ce0bd 2026-03-21)` |
| 构建目录 | `/Volumes/ORICO/cayw-issue9-target-20261022-a`；debug，`CARGO_INCREMENTAL=0` |

下文相对证据路径均位于上述证据目录。`tested-source-r18/` 保存完整清单、源码归档和相对 #8 的变化。最终交付另行归档。测试结束后只更新本记录、实施计划、路线图和 Device 的一处过时注释；封存脚本逐字确认 Device 变化只有该注释，不能把旧树摘要当最终树摘要。

该 checkout 继承 #5–#8 的完整交付。相对 Git HEAD 的全部差异不属于 #9 新增。`issue8-preserved-r18.json` 已核对 #8 的 500 条源码和封存明确引用的 1,581 份文件；没有重跑或改写 #8。其 seal 摘要仍为 `35cca0b8e933c4407a8fb90d695211041d602e6bd796588ca6f27962a7dda230`，readiness 摘要仍为 `4759acf9310bd2b20468f78e8e4d014dbad947ae15a115b7b0e18e57be2ab8cc`。

## 交付行为和支持范围

宿主包 `cayw-sources` 提供 `StdioSource`、`FileSkills` 和 `WorkspaceSearch`。宿主明确准备来源，再把唯一执行句柄交给 Registry。控制对象只允许观察、关闭和等待清理，不提供绕过 Host 的工具调用入口。Runtime 的普通及构建依赖树不含该宿主包、plugin、远程组件、界面、VerifyTool 或 Codex。

stdio 使用 MCP 2026-07-28：请求 `_meta` 携带版本、客户端能力与信息，结果绑定 serverInfo。没有 initialize 握手，也没有旧协议并行入口。只支持有界完整工具和资源目录；拒绝分页、版本不符和缺少元数据。每行一条 JSON-RPC，stdout 只传协议，stderr 为有界诊断。程序、参数、环境和 cwd 来自可信配置；环境先清空。取消携带原 requestId，超时、Drop 和关闭会移除原 pending 项，迟到结果丢弃。不自动重试、重连或重放。不交付 HTTP/SSE MCP、订阅或服务器请求。

Skill 使用宿主明确列出的绝对 Markdown 路径和可信名称、描述。加载时计算正文摘要，列目录时不读正文；批准 skills.read 后才向模型返回原文。正文中的角色、权限、include 或批准声称均不解析。读取核对文件与父目录身份、大小及摘要，拒绝同大小改写、同内容 inode 替换、叶或父 symlink 和父目录替换。系统目录 alias 在可信准备阶段解析后固定物理路径。加载不等于事务文件快照；已经开始的阻塞读取可能在 Future 被丢弃后结束，其迟到结果不会交给调用者。

每个来源绑定 Registry 实例、来源 ID、版本、origin、generation 和 manifest 摘要。原 Session、Task、attempt、worker、execution、调用序号、参数摘要与父调用记录保持一致。旧 consumer 固定原来源，不跟随替换；disconnect 后 reconnect 的 generation 从 1 经 2 到 3。撤销在 Registry 生命周期内永久有效。物理 owner key 不重复移交，失败安装也不回收已消费的 key。

`search.query` 复用已有 Workspace `fs.grep`，使用字面匹配、两行上下文和 gitignore。它固定自己的根目录句柄，目录被改名或替换不能换根。它不创建 Core 代码索引或第二套检索服务。准备复用 `.cayw-store`，因此需要可写根。结果保留忽略、受保护、跳过和扫描范围；`scan_complete` 只表示允许范围扫描完毕。它不声称与另一个独立 Host 共用根句柄，也不等同 CodeGraph 语义检索。

## 七项条件与实际结果

| 条件 | 实际证据 | 核对结果 |
| --- | --- | --- |
| 1. SDK/IPC 真实接入与使用 | Runtime `host_sources`，保存的 `host_sources` 示例 | 两条公开路径发现工具、读取真实 Skill，并用正文 17/25 调用真实 add，结果 42；集成测试还分别列出和读取资源 |
| 2. 单一宿主入口与按需上下文 | 示例 control/stats、Provider 原请求、Runtime 普通依赖树 | 同一 PID，spawned=1、tools/list=1、resources/list=1、tool_calls=2；Skill load_scans=1、on_demand_reads=2；首请求只有 execute_code，正文和工具 schema 不默认注入 |
| 3. 独立权限与无权操作拒绝 | `host_sources`、`host_source_search`，独立拒绝原件 | 发现、读取、执行分别批准；恶意 Skill 不能授权；错 token、跨任务批准、伪造 Skill 和缺少参数拒绝；被拒检索 queries=0，受限调用没有来源 dispatch |
| 4. 来源和原调用绑定 | Core/Cells/Queue 原数据库、Skill 替换场景 | 原身份、序号转换、参数和来源摘要一致；旧 generation 1 批准拒绝，只有新批准在 generation 3 读到替换正文 a=18 |
| 5. 断连、重连、取消与迟到 | `host_source_lifecycle`、Bridge 生命周期测试 | 执行前拒绝无调用；执行中取消、断连、撤销保留 unknown；故意迟到的 sum=999 丢弃；重开不产生新 attempt 或执行，不重放；旧 consumer 不转到新来源 |
| 6. 统一预算和有界结果 | Runtime 预算测试、sources stdio/Skill 边界测试 | 原次数、期限和结果限额生效；服务错误关联原调用；只读取消、超时和 Drop 释放 pending 槽；转义后的 JSON 字节受限；已执行结果被外层预算拒绝时保留来源成功及外层 unknown |
| 7. 宿主检索和移除后 Chat | `host_source_search`、sources `search` | 实际 marker 带原来源和三行上下文；拒绝时零扫描；移除后普通 Chat 返回 plain chat ok；明确调用旧或缺失来源有诊断，无新增来源执行；无匹配也保存排除范围 |

`acceptance-matrix-r18.json` 的 SHA-256 为 `3e48d983d9a47481285f3bf4ed705c541ccaaa55319aad0e0c96e82aee9e1309`。它逐条引用原始命令、数据库回读和限制。通过字段是索引，不代替实际原件。

一次正常集成测试有七次 Core 能力调用，但只有资源读、Skill 读、工具执行三次 SourceHandle 执行，即三对 prepared/settled。元数据操作不产生来源执行配对。Core 序号从 0 开始，Cells/Bridge 从 1 开始。IPC 使用公开 session.events 核原事件，没有虚构 session.bridge_dispatches 接口。

## 保存程序和数据库核对

`retained-build-r18.json` 绑定实际 Cargo 构建。包装器在同一命令内立即复制、同步并计算程序摘要，不依赖之后可能被覆盖的共享 target 文件。

| 保存程序 | 字节 | SHA-256 |
| --- | --- | --- |
| retained-programs-r18/cayw-runtime-mcp-fixture | 1,264,872 | `eed0ead0fda8fe2c694afeae45d6515abf0a33e223982e8d650c45db6f379ca4` |
| retained-programs-r18/host_sources | 62,454,176 | `9e97b0e31659c80ce4f79b3471a561af222c085a0985211aa2255924363cfb2b` |

`retained-example-r18.json/.log` 实际运行保存程序。SDK 和 IPC 的结果均为 a=17、b=25、sum=42，服务 PID 均为 62104。SDK Task 为 `471f893f-daeb-4570-99ce-5c05ffa01b64`；IPC Task 为 `f5a6c4d5-e607-4e62-9bd1-5fe0b4f46fb2`。每个模式有 44 条 Core 事件和两对来源执行：Skill 读和工具调用。最终 stdio 为 reaped=true、cleanup_complete=true、failure=null，Skill closed=true，IPC 已停止。随机认证 token 没有保存。

`example-readback-r18/result.json`（SHA-256 `8e2a8744606de73768636b181001e2a9c13903f52b26cf43e3e88d8a7fe438fc`）将实际返回值与 Core、Cells、Task、来源和 Provider 请求核对。`coordinator-independent-readback-r18/result.json`（SHA-256 `974758f3cfbe8e8c4de82f61c038cdfb2bbfa34653315c813d831b35fbe308b7`）另读独立复跑的 34 个场景目录、24 个数据库、252 份原件和 18 组来源调用。

独立报告 `independent-readback-r18.json` 的 SHA-256 为 `9a66432249dff513ed716b8a2dfcfae58035089147b80e0f5db302242e00ecc7`。主线程重新核对全部 75 份引用文件和实际 15 项测试输出。数据库 owner 已停止，且 WAL 不存在或为空后，使用 immutable 只读 URI；非空 WAL 必须另做一致快照。主库和已有旁文件在回读前后重新核摘要。

## Device 输出竞态与历史失败

r12 完整 workspace 曾在 `exact_output_budget_is_complete_and_finite_overflow_is_reported` 失败：进程正常退出，但 output_complete=false。原失败完整保留。同一个已保存测试程序的 50 轮单项和 30 轮并行整目标随后均通过；这不能消除原失败。

双启动暂停测试实际复现 Pipes 问题：A 正常退出并产生 abc，B 停在执行程序前并暂时继承 A 的管道写端，A 无法读到结束。修复只在 macOS Device 内串行化准备到 spawn 返回及父侧写端关闭的短窗口。执行、输出读取和清理仍并发。等待锁时使用原取消令牌、原期限并持有原资源名额。启动前取消或超时不创建第二个子进程。没有放宽输出完整性断言或延长清理期限。

`device-overlap-red-r16` 和 `device-overlap-green-r16` 保留真实失败和修复结果；r17 Device 全部 95 项通过，r18 workspace 再执行对应用例。PTY 的受控失败轮没有复现输出不完整，不能外推。r12 没有句柄跟踪，不能声称直接识别了原失败的具体继承者。启动锁只协调本进程内 Device 启动，外部宿主 fork 不参与；它不提供完整句柄隔离或操作系统沙箱。

其他实际修复包括：stdio 只读取消/期限/Drop 遗留 pending；Skill 仅核原文字节而漏算 JSON 转义；检索 summary 丢失排除范围。各项有保存的失败和成功对照，r18 workspace 再运行。测试辅助错误包括结果信封读取错误、误把元数据算作来源执行、测试 Provider 误看最后的 System 消息、证据目录碰撞，以及 Clippy 指出的辅助写法；按实际原因修正，未放宽权限、unknown、整数范围或预算。

`retained-command-history-r18.json` 索引历史命令、失败日志和源码变化。旧版本的源码身份和结果分别保留。早期 SQLite mode=ro 回读创建过空 WAL/SHM；原主库与引用文件摘要保持，旁文件未删除。后续检查使用上述 immutable 前提。不能宣称早期回读完全没有文件系统写入。

## 完整检查和保留限制

`final-checks-audit-r18.json`（SHA-256 `2436471ba07c1fcf06fe9800d5a304bfc669714b4c31a891c1083eef21b31e75`）核对十六项完整命令、日志和同源清单：

| 检查 | 命令或范围 |
| --- | --- |
| 1 | `cargo fmt --all -- --check` |
| 2 | `cargo check --workspace --locked --offline` |
| 3 | `cargo test --workspace --locked --offline` |
| 4 | `cargo clippy --workspace --all-targets --locked --offline -- -D warnings` |
| 5 | `cargo clippy -p cayw-runtime -p cayw-core -p cayw-cells --features fault-injection --all-targets --locked --offline -- -D warnings` |
| 6 | `cargo run -p cayw-agent --example offline --locked --offline` |
| 7 | `cargo tree -p cayw-runtime --no-default-features --edges normal,build --locked --offline` |
| 8–11 | headless-client 独立 manifest 的 fmt 与 all-targets check/test/clippy |
| 12–15 | issue1-client 独立 manifest 的 fmt 与 all-targets check/test/clippy |
| 16 | issue1-client 实际运行 `all`，保存全新场景目录 |

客户端 check/test/clippy 带 `--locked --offline`，Clippy 带 `-- -D warnings`。统计使用每个 Cargo 顶层目标最后一条结果，三条嵌套子进程结果不重复累计。七个忽略入口分别是 Cells 的 crash_effect_child、crash_child、live_scope_child，Data 的 lifecycle_child，VerifyTool 的 cleanup_failure_runner、fixture_child，以及 remote 的 server_binary_device_example_and_operator_commands_work_on_loopback。远程二进制示例未运行；其余六个是父测试调用的子程序入口，不当作默认独立执行通过。

没有运行 --all-features。最终 fault-injection 命令只证明编译和严格检查，不能称该 feature 全部测试通过。完整 workspace 运行了继承的可选成员回归，不表示扩大本项产品范围或验收了界面、plugin 和远程交付。

workspace 日志有四条既有 offline/offline.dSYM 同名警告。命令退出 0，但不能说无警告。指定 cayw-agent 的独立示例实际得到工具结果 5、回答 2 + 3 = 5、5 messages 与 running=false。

清理证明资源停止，不证明外部效果回滚。正常来源测试的 stdio 快照在关闭前，不能用其中 reaped=false 推断泄漏；最终清理看生命周期 after-stop/cleanup 和保存示例。替换拒绝案例还可能命中原工作区 owner 恢复门槛，不能单独归因为 Registry unknown。外层结果预算拒绝不能抹去已经执行的来源成功。Search 保守保留 unknown，不据此声称实际文件发生写入。

## 复跑和最终交付

从本项 checkout 运行，使用全新证据前缀和目录，期间保持源码不变：

```sh
export RUSTUP_TOOLCHAIN=1.95.0-aarch64-apple-darwin
export CARGO_INCREMENTAL=0
export CARGO_TARGET_DIR=/Volumes/ORICO/cayw-issue9-target-local-rerun-01
export CAYW_ISSUE9_ARTIFACTS_DIR=/Volumes/ORICO/cayw-issue9-artifacts-local-rerun-01
cargo test -p cayw-runtime --test host_sources --test host_source_lifecycle \
  --test host_source_search --locked --offline
cargo test -p cayw-sources -p cayw-bridge --locked --offline
```

完整命令可复用 `python3 -I /Volumes/ORICO/cayw-issue9-evidence-20261022-a/run-final-checks.py issue9-local-rerun-01`。记录器固定本项 checkout 和 target，拒绝覆盖旧记录。保存程序示例的构建与运行方法见 [宿主包说明](../../crates/cayw-sources/README.md)。不要通过重跑 r18 标签覆盖原件。

最终入口为证据目录的 `final-local-delivery-v1.json` 和 `final-readiness-v1.json`。`final-source-v1/` 保存最终完整清单、归档和相对 #8 的变化。封存核验 tested r18 与最终源码之间只有三份文档和一处注释变化，再核原日志、七项矩阵、独立原件、保存程序和 #8 封存。最终结论以实际生成并重新核对的 readiness 和其指向的 seal 为准。本文不嵌入自身最终摘要，避免循环依赖。
