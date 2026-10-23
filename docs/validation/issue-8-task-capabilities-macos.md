# Issue #8：任务限定的能力申请与可信裁决

验收日期：2026-10-22。实际平台为 macOS 27.0.1（26A434）、ARM64，工具链为 Rust/Cargo 1.95.0。其他 macOS 架构未验证。

[Issue #8](https://github.com/changw98ic/CAYW/issues/8) 的八项条件已逐条核对。r06 同源码的十六项最终检查均退出 0。默认 workspace 为 1,543 项通过、0 失败、7 项忽略；两个独立客户端分别为 10 项和 3 项通过。现有项目测试通过 SDK 和认证本机 IPC 各实际执行一次，每次为 1 项通过、0 项失败。独立执行者在 r06 复跑八项申请状态测试，并核对新增 SDK 反例、原数据库和全部最终命令。最终源码和原件的交付身份以文末封存与复核结果为准。

模型回复和宿主裁决使用固定本地测试输入。Luau、Host、Device、项目测试程序、文件效果、持久数据库与本机认证 IPC 均实际运行。真实模型请求为 0，付费请求为 0；没有使用 #6 的真实模型预算。改动未提交，GitHub 未同步。

## 源码、构建和原件

| 项目 | 实际身份 |
| --- | --- |
| 源码目录 | `/Volumes/ORICO/cayw-issue8-integration-20261022-a` |
| 分支 | `feat/issue8-task-capabilities` |
| Git HEAD | `d6d8977249acdd8b54a2bba3db49edbb67465651`；交付包含未提交文件，HEAD 本身不代表交付源码 |
| 证据根目录 | `/Volumes/ORICO/cayw-issue8-evidence-20261022-a` |
| 冻结 Issue | `issue8-contract.json`，远端 `updatedAt=2026-10-19T14:53:46Z`，当时状态 OPEN |
| Issue 原件 SHA-256 | `cf0f68f6b7714200cab6c167489c3f54624187be38923e8f73c495d7198f9ec9` |
| 继承的 #7 基线 | `f2e0af2ba4a16b2eb54d98ca0ab4bcf1554bbc226cb8dbf4aeeeefda934d5233`，492 条 |
| tested r06 内容清单 | `99126d9c5a24cd3ddcf3730cfad8f8566d99d2c79447f5f752b02a02b3c3c323`，500 条，相对基线 17 个变化路径 |
| tested r06 清单文件 SHA-256 | `e5221d72766428261ca8f6bbc7bedaec178e09ac24e17d5237cb0d01483c4675` |
| tested r06 源码归档 SHA-256 | `7047fdb994dacfe6d1fc74a3cb2c32ced177087b9d579d656fa861e68c3fbdad` |
| Cargo.lock SHA-256 | `b3329220076fc60e7e240096eccf9e279886e33d218cbf27104c20c2e9871060` |
| 工具链 | `1.95.0-aarch64-apple-darwin`；rustc `1.95.0 (59807616e 2026-04-14)`；Cargo `1.95.0 (f2d3ce0bd 2026-03-21)` |
| 常规构建目录 | `/Volumes/ORICO/cayw-issue8-target-20261022-a`；debug，`CARGO_INCREMENTAL=0` |
| 项目程序独占构建目录 | `/Volumes/ORICO/cayw-issue8-project-build-target-r06` |

下文未写绝对路径的原件名称均相对于证据根目录。`issue8-frozen-source-r06/` 保存完整 `source.json`、`delta.json`、`source.zip` 和 `freeze.json`。每条记录命令保存原始输出、argv、平台、工具链、锁文件摘要及命令前后的完整源码清单。清单包括未提交文件和删除状态。

当前 checkout 继承 #5/#6/#7 的完整本地交付。相对 Git HEAD 的全部差异不能统称 #8 新增。#8 没有新增第三方依赖，Cargo 清单和锁文件相对 #7 基线不变。#7 第二版封存的 SHA-256 保持为 `4bbb42bddba7d3338c17fb63d9f80825ba97b3f127d1244ea8e92f846920af61`；本次已逐项重新核对其 492 条原源码，最终封存与复核还会再次检查。

## 交付行为

模型目录只有 `capability.request` 与 `capability.status`。模型提交用途、能力缺口、操作编号、能力名和完整参数。Session、Task 和 attempt 由宿主提供；输入中的 actor、role、approved 或身份字段不能成为授权。

可信 SDK 使用不可从 JSON 重建的评估票据。认证 IPC 提供 `task.enqueue_with_intent`、`request.operations`、`request.status`、`request.events`、`request.task_state`、`request.decide`、`request.revoke` 和 `request.close_task`。`request.decide` 固定操作者为 `rpc-user`，先校验原因与授权期限，再用同一私有票据评估并决定。票据不传给客户端，决定方法不注册为模型能力。评估和最终裁决是两个持久转换，中间撤销、到期、结束或写入失败都能拒绝原决定，不自动重建票据重试。

启动配置增加可选 `requests`，复用同一 `RequestConfig` 和 `OperationCatalog`。加载配置不会授权、启动程序或发送模型请求。普通 `permissions` 是另一层检查；请求授权不能代替它。反过来，普通权限也不能代替请求授权。按能力名保护范围后，换 profile 或增加参数不会退回普通权限。

授权绑定原 Session、Task、attempt、request、能力、完整参数、宿主程序与环境配置、已打开的工作区根、服务配置、策略、期限和预算。新有效同范围申请替代旧申请及旧 grant。次数只决定何时可评估，不会自动批准。宿主记录用户明确的项目测试需求后，第一条精确申请即可进入评估。

批准后的每次调用仍通过 Luau 参数校验、共享预算、普通权限、Requests 再检查、原调用开始和 Host/Device 执行。任务结束关闭请求授权；运行中撤销取消原 Task。已发生的文件效果保留，无法确认完整结果时继续标为 `outcome_unknown`，即“可能已执行，尚待可信核对”。清理确认只证明资源停止，不证明业务效果已撤销。

## 八项条件与实际场景

| 条件 | 原测试或实际运行 | 核对的结果 |
| --- | --- | --- |
| 1. 模型不能自行授权 | `request_outcomes`、`request_ipc`、`serve_config` | 六类伪造字段各在独立 cell 拒绝；模型调用 `request.decide` 报未知能力；实际读取批准回显和 `SKILL.md` 后仍无评估或 grant；IPC actor 固定 |
| 2. 精确身份与范围 | `request_boundaries`、`request_outcomes`、`dynamic_requests`、Requests 的 `host_configuration`、`lifecycle` 和 `receipts` | 改 profile/filter 和操作编号不能借普通权限执行；模型增加程序、环境和路径字段拒绝；其他 Task/Session/attempt 与重开后的新任务不能借原 grant；原根句柄和服务配置保持绑定 |
| 3. 首轮评估与次数 | `request_ipc`、`task_requests`、Requests `lifecycle`、实际项目样例 | 可信项目需求的第一条精确申请可评估；阈值可配置但仍需宿主决定；无效申请不增加有效次数 |
| 4. 允许、拒绝、到期、不可用与重复 | `request_outcomes`、`request_ipc`、实际项目样例 | 状态、原因与事件分别核对；申请和评估期限使用原绝对期限；重复或迟到决定拒绝；批准前无 test.run 开始，批准后原申请收据先于原开始和结算 |
| 5. 撤销、取消和清理 | `request_processes`、`task_requests`、`request_ipc` | 执行前撤销无 PID/效果；SDK/IPC 运行中撤销停止原进程，保留既有文件效果和未知结果；清理记录失败时保留原身份和名额 |
| 6. 生命周期与公开接口 | `request_outcomes`、`request_ipc`、`task_requests`、Requests `lifecycle` | 完成、取消、到期、重开后原授权不泄漏；认证前与错 token 调用不分派；SDK 错 Task 查询/撤销、错 Task 票据及终态/重开后的迟到撤销分别拒绝；正常终态查询仍可读历史 |
| 7. 普通工具和共享预算 | `request_outcomes`、`request_processes` | 拒绝、到期、评估不可用之后，同 Task 仍能读取已授权的 ordinary.txt；申请、读取和执行共享调用次数；三层时间、两层输出和进程清理限额保持 |
| 8. 实际项目执行与通用解释器拒绝 | `task_project_test`、`request_boundaries` | 同次构建封存的现有项目测试经 SDK/IPC 各运行一次；Shell/Node/Python 等未知能力以及 path/args/program/env/cwd 扩展各有独立拒绝 |

逐条证据矩阵为 `acceptance-matrix-r06.json`，SHA-256 `8f002486cf878bc23208653733852e37ae842052eb42c3bb0abbca2003ce99f4`。主线程读完独立报告后生成矩阵，并重新核对全部 24 个不同引用文件。矩阵中的通过字段是已核结论的索引，不能代替原测试、数据库和效果证据。

宿主配置摘要包含 program/argv/env 与进程限额。terminal 配置逐项变化另有失效测试，模型程序字段扩展也已实际拒绝。本次没有逐项替换普通 test profile 的宿主 program/env 后执行旧收据的专门测试，不能把上述不同证据说成这一实测。对于无法读取内部身份的 external/gate/validator/Device 实现，宿主更改执行策略时须更换 `permission_policy_id`；不声称同 ID 下任意替换内部实现都会自动被发现。

## 真实项目测试程序

实际目标是既有 `cayw-ai/tests/contracts.rs` 中的 `validation_is_strict_and_local_only`。宿主固定程序和参数为保存后的测试程序，加上 `validation_is_strict_and_local_only --exact --nocapture`。环境配置为空，filter 禁用。模型不能替换程序、参数或环境。

程序在全新独占 target 中实际编译。构建包装器在同一记录命令内立即复制、同步并计算程序摘要，将完整 Cargo `compiler-artifact` 与保存记录写入同一原始日志。后续核对不依赖可能被另一次构建覆盖的旧 target 路径。

- 编译命令记录：`project-program-build-r06.json`；原日志 SHA-256 为 `2bdbd813376d6e094588852e68cf8dd40a98fc01549b83294a8805c5489f9696`。
- 保存程序：`project-program-r06/cayw-ai-contracts`；SHA-256 为 `63552cbccafbecd89956c3ef2c026a250ce7f6a8c928ba7b60cb2fe4d01b76d8`。
- 目标源码 SHA-256：`ecbf8b5d8d71c4b1f49d64ea056beac2ad62a14d354a472203bf61610b367d39`。
- 完整来源：`project-program-r06/artifact.json` 和 `build.json`，以及 `project-program-prepare-r06.json`。
- 实际运行：`actual-project-test-r06.json`；原日志 SHA-256 为 `59b9b7990601e534a6101322f55a734b5ea3eff4634af2371afed49edb9600c0`。
- 交叉核对：`actual-project-crosscheck-r06-result.json`，对应记录命令 `actual-project-crosscheck-r06.json`，exit 0。

| 路径 | 原 Task | 原 Request | 原 Core execution / sequence |
| --- | --- | --- | --- |
| SDK | `19929d96-5adb-4e93-8282-f559ec95989a` | `cce6d011-1176-43ae-971a-e29ed367e299` | `0c8f6303-c097-4da6-b82f-62977d0614b7` / 1 |
| 认证 IPC | `b33d1317-9cc8-46ac-a3b2-b36142adc562` | `442c8e8a-3a00-40fb-8f33-b55b417fe009` | `f247d4e2-f2b3-423d-8fb2-61405470898b` / 1 |

每条路径保留配置、原 Task、批准前申请、批准记录、Core 事件、Host 审计、Requests 事件、实际 stdout/stderr、最终 Task 与重开 Task。批准前没有 test.run 开始。批准后只有一条对应开始和结算，原 Session/Task/attempt/worker、Core lease、父调用、源码摘要、参数和调用序号保持一致。原 Core 游标依次为 Planned=32、授权收据=33、Started=34、Settled=35。两个任务都 Completed，Requests 已 Ended，重开后原 Task 与游标不变，running=0。

两份 stdout 均包含测试全名和 `1 passed; 0 failed`，exit_code=0、success=true、termination=exited、output_complete=true。构建、准备、运行及交叉核对的完整源码清单全部等于 tested r06。每个案例把 Runtime 打开在其结果目录下的独立 workspace；源码根只用于来源核对，没有产生三个 Host 状态文件。原 PID 的独立回查均为不存在。

## SDK 直接错误身份对照

r05 独立复核发现 Runtime SDK 查询和撤销缺少直接错误身份实跑。r06 在已有跨 Task/Session 用例中补齐对照，不改变产品实现，也没有为制造失败而改坏产品。

主线程 `sdk-identity-contrasts-r06` 单项实际通过，独立 `independent-sdk-outcomes-r06` 八项实际通过；最终 r06 workspace 又执行同一用例。两条命令的完整源码清单均为 tested r06，前后不变。

| 保存阶段 | 直接 SDK 结果 | 前后核对 |
| --- | --- | --- |
| wrong_live_task | 另一运行中 Task 查询、撤销原 request 均返回 NotFound | 两 Task 均未取消；两申请、授权状态、Task 与事件完全不变 |
| ended_original_task | 原 Task 完成后的迟到 revoke 返回 StaleApproval | 原历史与另一个 Task 均不变 |
| reopened_ended_original_task | 重开后迟到 revoke 仍返回 StaleApproval | 重开前后及拒绝前后的原记录相同 |

主线程三阶段 JSON 位于 `sdk-identity-r06/request-outcome-1gyHKY/`；独立三阶段 JSON 位于 `independent-sdk-outcomes-r06/request-outcome-RlfQVH/`。这些快照已与原 SQLite 核对。错误 actor 没有写入撤销或决定事件。独立跨任务案例中，四个 Task 的普通读取均成功；其他或新 Task 的三次 test.run 均 NotExecuted，没有进程开始。

`independent-sdk-readback-r06-worker-v1.json` 核对 141 份原件前后摘要不变。`independent-sdk-origin-pairing-r06-worker-v1.json` 核对九个案例的 42 条原调用。主线程另写并实际运行 `check-sdk-original-pairing.py`，结果 `sdk-pairing-coordinator-readback-r06.json` 的 SHA-256 为 `671f7233df48319f7a357630d0a553f55e87730bbb4790917cbc5c648868f27f`。原 ExecutionStarted、Planned、Started（实际执行时）、Settled、Cells 和 SQLite 均对应；NotExecuted 没有 Started。

终态 status 正常保留历史可读性。能够读取自己原申请的历史，不代表获得新授权。SDK 错 Task approve/deny 票据、IPC 错 Task status/decide、未握手与错 token 的查询/裁决/撤销另有实际正反例。

## 撤销、效果与预算原件

`request-processes-first-r01` 的八个测试实际通过。`process-crosscheck-r02.json` 再核十五份阶段快照与原 SQLite、原进程身份及磁盘效果。独立执行者在 r03 实际复跑八项进程测试和八项申请状态测试，共十六项通过。当前 r06 默认 workspace 也实际执行八项进程测试。

运行中撤销先观察原 PID、进程组与启动时间，以及已同步的一行 project test effect。撤销后原进程停止，完成文件没有出现，既有一行效果保留。Core 仍为 OutcomeUnknown，Session 仍需核对，重开与恢复不重放。清理持久化失败场景在写 processes_stopped 时注入错误：原 Task 为 Draining，Requests 为 Closing，running=1，另一 Session 被 QueueFull 拒绝。原 owner 补记清理后名额仍占用；显式 recover 才使 running=0。原效果和业务 unknown 始终保留。

时间对照分别选择 profile、Host process、Host call 的三秒限额，其他后备限额更长。测试使用宽裕 watchdog，不断言固定毫秒完成。三种对照的终态都含 TimedOut。输出对照分别选择 profile 和 Host 的 1,024 字节上限，程序尝试输出 8,192 字节；终态均为 OutputLimit，保存 1,024 字节 stdout、0 字节 stderr。共享 Host 次数对照只允许申请一次和执行一次；再次执行 NotExecuted，文件效果只出现一次。

另一项能力次数对照将两次申请和一次普通读取计入同一个三次限额。后续执行虽已有 grant，仍因调用次数耗尽而拒绝，原 Task 以预算失败结束。这是预期限制结果，不是未通过的测试。

## 独立复核和历史版本的范围

最终八条件报告为 `independent-eight-conditions-final-r06-worker-v1.md` 和 `.json`，SHA-256 分别为 `49b35a9caaafb4ff38c94bc19aab0603af89f5519703f21a2c8f56858286a9f4`、`7cc6af7e39a9d3f29042a31dec7fab01025c074607a3cd21c5566d858a94ee5a`。主线程读完两份报告，并重新核对其 26 个引用文件。报告未确认产品缺陷或剩余八条件阻断，不代替主线程封存。

该执行者在 r06 独立运行 request_outcomes，实际 8 passed/0 failed。原日志 SHA-256 为 `037bb4ee90e54ff3fcba7d5279c659d6e72b596cb5df48db05e9d984b899bcfb`。其 `independent-final-command-audit-r06-worker-v1.json` 还逐份读取主线程十六项命令的收据、源码清单和原日志，重算测试数量；它没有宣称自己重跑 r06 workspace 或进程组。

项目和历史边界由另一独立执行者核对，报告 `independent-boundaries-project-readback-r06.json`，SHA-256 为 `8acf26043f6521918fb0ed15dbf5a7685841981f29547f6f8f4f638d4a32acfd`。检查器实际退出 0，记录 4,479 项检查和 674 份原件前后摘要一致。主线程又重算这 674 份原件。该报告核 r06 项目链与 r04 的五个边界案例、十七个独立 cell，不冒称已证明新增 SDK 对照实际执行。

保留的 `independent-request-reproduction-r03.json` 前后清单为 `f05cb8c45ca0b318b9c436e47af0621c607040d13caaaf4fb05215805f66d393`。原日志 SHA-256 为 `d60645a620d600d6aa7892f187778c3ca110037da49124bb63551e3b67a84c37`。`independent-request-readback-r03.json` 的 SHA-256 为 `5ba0accdd1c500e6d258fc100be6f9e96a26263d6493044dd7ed04b33583f219`，核十九个案例、三十四份观察与 221 份原件前后摘要一致。

r03→r04 只修边界测试宏表达式；r04→r05 只改项目样例的独立 workspace 与来源字段；r05→r06 只补 SDK 测试、计划和新增验证文档。进程测试、fixture 与 164 条产品 src 路径从 r03 到 r06 不变。r03 原件保持原身份，不覆盖 r06 新增 SDK 断言。r06 默认检查、独立八项状态测试和实际项目程序均另行执行。r04 边界命令没有在同一命令内保存 fixture 二进制；不能用当前共享 target 字节证明其历史程序字节。141、221、674 三组原件可能重叠，不相加为互不重复的总数。

## 最终十六项检查

以下命令均绑定 tested r06，逐条 exit 0、运行前后源码不变。完整命令和日志摘要见 `final-checks-audit-r06.json`，其 SHA-256 为 `e3b7955519f194bda4bb19218f1a5f61d55edde34e2fc84e556ff5bbc8ff8e91`。

| 检查 | 实际命令或范围 | 记录前缀 |
| --- | --- | --- |
| 1 | `cargo fmt --all -- --check` | issue8-final-r06-fmt |
| 2 | `cargo check --workspace --locked --offline` | issue8-final-r06-check |
| 3 | `cargo test --workspace --locked --offline` | issue8-final-r06-test |
| 4 | `cargo clippy --workspace --all-targets --locked --offline -- -D warnings` | issue8-final-r06-clippy |
| 5 | `cargo clippy -p cayw-runtime -p cayw-core -p cayw-cells --features fault-injection --all-targets --locked --offline -- -D warnings` | issue8-final-r06-fault-clippy |
| 6 | `cargo run -p cayw-agent --example offline --locked --offline` | issue8-final-r06-offline |
| 7 | `cargo tree -p cayw-runtime --no-default-features --locked --offline` | issue8-final-r06-runtime-tree |
| 8–11 | examples/headless-client/Cargo.toml 的 fmt 和 all-targets check/test/clippy | issue8-final-r06-headless-* |
| 12–15 | examples/issue1-client/Cargo.toml 的 fmt 和 all-targets check/test/clippy | issue8-final-r06-issue1-* |
| 16 | `cargo run --manifest-path examples/issue1-client/Cargo.toml --locked --offline -- all <全新结果目录>` | issue8-final-r06-issue1-all |

客户端 check/test/clippy 均带 `--locked --offline`，Clippy 带 `-- -D warnings`。第十六项输出目录为 `issue8-final-r06-issue1-artifacts/`，实际覆盖 SDK、IPC、事件续读、传输限制、审批前重开和未知结果重开。

| Rust 测试计数 | 顶层目标 | 通过 | 失败 | 忽略 |
| --- | --- | --- | --- | --- |
| 默认 workspace | 235 | 1,543 | 0 | 7 |
| 独立 headless-client | 2 | 10 | 0 | 0 |
| 独立 issue1-client | 2 | 3 | 0 | 0 |

计数使用每个 Cargo 顶层目标最后一条结果，三条嵌套子进程汇总不重复累计。七个默认忽略入口原样保留：Cells 的 crash_effect_child、crash_child、live_scope_child，Data 的 lifecycle_child，VerifyTool 的 cleanup_failure_runner、fixture_child，以及 remote 的 server_binary_device_example_and_operator_commands_work_on_loopback。远程二进制示例没有运行；其余六个是父测试使用的子程序入口，不当作默认独立执行通过。没有运行 --all-features。fault-injection Clippy 是编译和严格检查证据，不代替该 feature 的完整测试执行。

完整 workspace 原日志保留四条既有 offline 示例输出同名提示，分别涉及 offline 与 offline.dSYM。命令仍退出 0。Agent 示例另行指定包，实际得到工具结果 5、回答 2 + 3 = 5、5 messages 与 running=false。默认 Runtime 依赖树不含 TUI、WebUI、remote、plugin、VerifyTool 或 Codex；完整 workspace 回归其他成员不表示 Runtime 依赖这些组件。

## 失败、拒绝核证与修正

`preserved-failures-r06.json` 衔接旧失败索引，并增加 SDK 检查器诊断和历史源码比较诊断；SHA-256 为 `a689db1d551c9699cd64fff253cbb3dc3ca08de9f14a8a73f0545e3c58ae00f3`。旧日志、失败源码、成功替代和中断原件均保留。

| 原记录 | 实际问题 | 修正与后续结果 |
| --- | --- | --- |
| request-ipc-red-r01 | 三项行为失败：可信申请 IPC 方法不存在 | 增加严格分派和服务端私有票据；四项 IPC 测试随后通过，r06 再通过 |
| request-config-red-r01 | 两项行为失败：启动配置不接受 requests | 复用唯一 Requests 限额和目录绑定；三项配置测试随后通过，r06 再通过 |
| request-config-green-ipc-page-red-r01 | 默认事件页 100 超过 Requests 最大 64 | 申请事件使用专用默认 64；保留显式上限检查；后续通过 |
| request-outcomes-boundaries-first-r03 | JSON 宏数组表达式缺少括号，场景未运行 | 只修测试写法；r04 三边界、八状态、八配置共十九项通过，r06 再通过 |
| actual-project-test-r04 | 两实际项目测试成功，但样例把源码根作为工作区，新增三个 Host 状态文件 | 核摘要后移至 project-source-workspace-artifacts-r04 保留；样例改独立 workspace；r05/r06 重建重跑，源码前后相同 |
| actual-project-crosscheck-r04 | 检查器正确拒绝上述源码变化 | 不放宽检查；r05/r06 交叉核对成功 |
| SDK 原件检查器首轮 | 错误要求 Queue Task fence 等于 Core Attempt fence，报 call lease lost | 两者分别按 Task 和 Session 递增；改核原 Core lease、执行与原调用；九案例 42 调用通过，未改产品 |

r04 完整检查在发现源码变化后主动停止。fmt/check 已退出 0，workspace test 没有完整收据，后续项目没有执行。不补造通过数或退出码。r05、r06 分别另行完成全部十六项。

仓外进程检查器初稿曾把第二个预算拒绝调用配到第一个进程。修正为 execution_id 加 sequence 精确匹配：NotExecuted 无进程记录，每个实际进程对应原结算，随后十五阶段核对通过。另一旧检查器误要求预算耗尽的 Task Completed；实际 Failed 才符合预算语义。SDK 检查器错误则保留失败源码、traceback 和根因文件；这些检查器修正均未改产品或放宽预算。

主线程补测试后，曾将当前 live 文件直接与 r05 历史源码摘要比较，按预期发现 request_outcomes.rs 不同。随后在 r05 原归档核历史源码，当前故意变更单列，仓外原件重新核摘要。此事是历史比较对象选错，不是原件损坏或产品缺陷。

## 复跑入口与限制

从本项源码目录执行以下命令。所有结果目录和前缀须全新；记录器拒绝覆盖旧日志。

```sh
export RUSTUP_TOOLCHAIN=1.95.0-aarch64-apple-darwin
export CARGO_INCREMENTAL=0
export CARGO_TARGET_DIR=/Volumes/ORICO/cayw-issue8-target-local-rerun-01
cargo test -p cayw-runtime --test request_config --test request_ipc \
  --test request_outcomes --test request_boundaries --test request_processes \
  --test task_requests --test dynamic_requests --test serve_config \
  --locked --offline -- --nocapture --test-threads=1
```

需要保留原数据库和阶段 JSON 时，在命令环境分别设置全新 CAYW_REQUEST_OUTCOME_EVIDENCE_DIR、CAYW_REQUEST_BOUNDARY_EVIDENCE_DIR 和 CAYW_REQUEST_PROCESS_EVIDENCE_DIR。普通测试使用临时目录。

记录器 ROOT 固定为本项 checkout。源码目录不变时，全套检查可使用 `python3 -I /Volumes/ORICO/cayw-issue8-evidence-20261022-a/run-final-checks.py issue8-local-rerun-01`。新运行保留新身份，不覆盖原 r06。

实际项目复跑先保存同次构建程序，再执行样例和原件核对。以下变量仅用于开发者运行，不是模型通用执行工具；所有目标目录必须不存在，构建与运行期间源码保持不变：

```sh
export CAYW_I8_EVIDENCE=/Volumes/ORICO/cayw-issue8-evidence-20261022-a
python3 -I "$CAYW_I8_EVIDENCE/validate.py" project-build-local-01 \
  python3 -I "$CAYW_I8_EVIDENCE/build-project-program.py" \
  /Volumes/ORICO/cayw-project-build-local-01 "$CAYW_I8_EVIDENCE/project-program-local-01"
python3 -I "$CAYW_I8_EVIDENCE/validate.py" project-prepare-local-01 \
  python3 -I "$CAYW_I8_EVIDENCE/prepare-project-program.py" \
  "$CAYW_I8_EVIDENCE/project-build-local-01.json" "$CAYW_I8_EVIDENCE/project-program-local-01"
python3 -I "$CAYW_I8_EVIDENCE/validate.py" project-run-local-01 \
  cargo run -p cayw-runtime --example task_project_test --locked --offline -- \
  /Volumes/ORICO/cayw-issue8-integration-20261022-a \
  "$CAYW_I8_EVIDENCE/project-program-local-01/build.json" "$CAYW_I8_EVIDENCE/project-run-local-01"
python3 -I "$CAYW_I8_EVIDENCE/validate.py" project-crosscheck-local-01 \
  python3 -I "$CAYW_I8_EVIDENCE/verify-project-evidence.py" \
  "$CAYW_I8_EVIDENCE/project-run-local-01.json" "$CAYW_I8_EVIDENCE/project-run-local-01" \
  "$CAYW_I8_EVIDENCE/project-crosscheck-local-01-result.json"
```

SDK 是可信宿主的进程内接口。HostActor::new 只校验名称，不认证操作者。IPC token 由宿主持有，测试 token 是公开合成数据；token 不构成操作系统隔离或服务器身份认证。固定工作区和程序参数也不构成操作系统文件沙箱。获准程序仍持有宿主用户的系统权限。

本项覆盖 Host/Runtime 调用、时间、输出、进程清理和原 Task 名额。没有接入 Device ResourcePermit，不代表 #18 内存/GPU/资源策略完成。读取 SKILL.md 的批准声称只证明文本不能授权，不代表 #9 Skill loader 已验收。Core、Cells、Requests、Queue 分别提交；清理停止资源后，业务 unknown 仍需可信核对。

## 交付封存入口

`seal-delivery.py` 将当前源码、完整归档、相对 #7 基线的精确变化、八项矩阵和仓外原件摘要写入全新 final-local-delivery 文件。`verify-readiness.py` 再读取封存、原日志、完整源码清单、实际项目证据和 #7 原源码。测试后只允许本验证记录、实施计划和路线图三份文档变化。

首次快照 `delivery-candidate-r06-v1.json` 与实际退出 0 的 `delivery-candidate-readiness-r06-v1.json` 已核 1,574 份原件、八条件、十六条最终命令和 #7 原源码。该版完整保留。最后只更新本记录、实施计划和 #8 路线图状态，最终相对基线变化为 18 个路径。

最终具名入口为证据根目录中的 `final-local-delivery-v1.json`、`final-local-delivery-v1-source.json`、`final-local-delivery-v1-source.zip`、`final-local-delivery-v1-delta.json` 与 `final-readiness-v1.json`。前四份保存最终源码和证据身份，后一份重新核对原件。最终完成须以实际退出 0 的 final-readiness-v1.json 及其指向的封存为准，不能仅凭本文件、计划勾选或矩阵通过字段判断。文档不嵌入自身最终摘要，避免循环依赖。封存和复核均不运行真实模型，不写 Git 或远端系统。
