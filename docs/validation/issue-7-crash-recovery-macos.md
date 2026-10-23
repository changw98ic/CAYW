# Issue #7：macOS 崩溃恢复与原副作用核对

验收日期：2026-10-22。实际平台为 macOS 27.0.1（26A434）、ARM64，工具链为 Rust/Cargo 1.95.0。其他 macOS 架构未验证。

Issue #7 的七组冻结条件已在本记录的范围内通过本地验收。SDK 的 26 个窗口和认证本机 IPC 的 14 个窗口均实际杀掉原执行进程，合计 40 个 SIGKILL 窗口。另有六个杀进程后可信核对场景、73 次不改变原记录的拒绝，以及独立复现的 Requests 修复窗口和完成/取消顺序场景。最终第七轮十五项检查全部退出 0。默认 workspace 为 1,516 项通过、0 失败、7 项忽略；两个独立客户端的 10 项和 3 项另列。

本项使用固定离线 Provider 驱动实际本机文件效果、数据库提交和进程崩溃。真实模型请求为 0，付费模型请求为 0。#6 的真实模型授权、预算与封存证据没有用于本项。改动未提交，GitHub 未同步；这份本地记录不表示远端 Issue 已关闭。

## 源码与证据身份

- 源码目录：`/Volumes/ORICO/cayw-issue7-integration-20261022-a`。
- 分支：`feat/issue7-crash-recovery`；Git HEAD 为 `d6d8977249acdd8b54a2bba3db49edbb67465651`。
- 证据目录：`/Volumes/ORICO/cayw-issue7-evidence-20261022-a`。下文未写绝对路径的证据名均相对于该目录。
- 冻结范围：`issue7-contract.json` 与 `acceptance-scope.json`；对应 [Issue #7](https://github.com/changw98ic/CAYW/issues/7) 的 `updatedAt=2026-10-19T14:53:41Z`。
- #6 基线清单为 `379d4e9854e6122a639a46238c33652b30251e754a1720007ae8c96d51ea4859`，共 477 条。当前目录继承完整未提交的 #5/#6 交付，相对 HEAD 的全部差异不能统称 #7 新增。
- 最终通过检查的源码为 `issue7-final-frozen-source-r07/source.json`，共 491 条；相对 #6 有 98 条变化路径。内容清单摘要为 `8ae5331813036af69c99904fbbd0b08224be35b252b62faad63e98d44fbb5a5f`。
- 该清单文件的 SHA-256 为 `c2e6066b6d0d217e3a42a02f7be2a9292adebb9995e1eb4e9d83599c4e2d2464`；同目录 `source.zip` 的 SHA-256 为 `a645e99e2f142e33fc38e30f45573a61828653ce775bc549e6489cc37ea00521`。

每条主线程检查由仓外 `validate.py` 保存原命令、退出码、平台、工具链、源码前后清单和原始日志。生效的构建目录以命令中 `env CARGO_TARGET_DIR=...` 为准；收据顶层的默认 target 字段不能覆盖它。源码清单包括未提交文件及删除状态。

本记录、[实施计划](../superpowers/plans/2026-10-22-crash-recovery.md)和[路线图](../development-map.md)是最终测试后允许变化的三份文档。`final-local-delivery-v2.json` 单独保存交付源码清单、归档、相对 #6 的差异、文档差异与原件摘要；`final-readiness-r07-doc02.json` 和同名日志保存随后的逐件核对结果。最终交付清单与 tested r07 清单不同，不能互相替代。

第一版封存和逐件核对均退出 0，原件保留。随后的只读文档复核发现计划误把未提交改动说成属于 Git HEAD；已改为属于当前 checkout。第二版封存使用修正后的三文档，不替换第一版记录。该修正没有改变 Rust 源码、测试或构建输入。

## 交付行为

Session 是持久会话，Task 是一次用户任务，Attempt 是一次执行，Worker 是执行者。恢复先读取这些原身份。`unknown` 表示可能已经发生效果但没有足够证据确认；`draining` 表示执行资源仍待清理或确认。两者都不能自动重放工作，也不能提前释放执行名额。

1. 取消使用唯一的 `CancelTarget`：原 Session、Task、Worker、Attempt 和 Queue fence。fence 是防止旧执行者写入新执行的递增身份号。Runtime 在同一个生命周期协调锁内核对完整目标，再关闭原 Requests、写原 Queue 并通知原运行。旧 Session-only 取消入口已移除；旧 JSON 形状作为拒绝测试保留。Core 的 Attempt fence 与 Queue 的 Task fence 属于不同记录，不能互相合成。
2. 可信核对同时提交原调用身份、宿主操作者、证据与提议结果。宿主必须配置 `ReconciliationVerifier`；默认没有验证器时拒绝核对。IPC 的操作者固定为 `rpc-user`，请求不能自行指定可信身份。非空文字或模型自报不构成证据。
3. Core 和 Runtime 都拒绝与可靠本地结果矛盾的“未执行”裁决。父 launch 对应的子能力已有成功、失败或可靠结果时，不能再声称父 launch 未执行。独立 wait 调用按其自己的原身份核对。
4. 核对只追加裁决，不重写原工具消息、原结果中的 `outcome_unknown` 或持久事件前缀。继续任务只在新的 Agent 输入副本中使用已提交裁决，不重跑原 VM 或能力。Fork 的来源、固定游标与保存消息逐层核对；Cells 仍冻结时拒绝借 Fork 绕过修复。
5. Requests 的授权记录另有磁盘状态标记。事务先同步 Pending，业务提交后再同步 Ready；崩溃或写入失败会保留 Pending/Stopped。普通重开不能把它们当 Ready，也不能删除标记绕过。显式修复核对原存储身份及所有原执行者停止证据，撤销旧请求、nonce 和 grant，轮换授权代际，保留修复与启用记录。
6. 原 supervisor 丢失后的进程清理确认是可信宿主 SDK 操作。它必须核对原 Task 独占锁、操作系统进程身份、全部记录进程、Cells 和能力身份，并调用宿主资源验证函数。它不提供普通模型/RPC 入口。清理确认只证明资源已停，不证明业务效果撤销；随后仍需显式修复授权并恢复原任务。

Core 协议/存储保持 v3，IPC 外帧保持 v1。Core、Cells、Requests 和 Queue 分别提交；本项没有把这些数据库描述为天然原子。

## 七组条件与实际证据

| 冻结条件 | 实际核对 | 主要原件审计 |
| --- | --- | --- |
| 可达具名窗口实际崩溃 | SDK 26、认证 IPC 14 个窗口保存原进程 signal 9、提交点、库文件和文件效果；另补 Requests 修复三窗口及独立三窗口复现 | `owner-final-originals-audit-r01.json`、`supplemental-originals-bound-r07.json` |
| 同一 Session 恢复且不重放 | 重开查询原 Task、原调用与事件；外部效果内容/次数、Provider 次数不增；unknown 继续阻止新效果 | `owner-final-originals-audit-r01.json`、`independent-continuation-originals-audit-r02.json` |
| 可信成功、失败、未执行与拒绝 | SDK/IPC 各三类杀进程后核对；73 次证据或身份拒绝保持原记录；另有 Core/Runtime 矛盾反例 | `independent-continuation-originals-audit-r02.json`、`independent-defects-audit-r01.json` |
| 原执行者与旧身份隔离 | 原执行者仍活、清理未确认或归属不明时拒绝接管并保留容量；旧结果、审批和取消不能修改新执行 | 原 owner/continuation 审计、`final-checks-audit-r07.json` |
| 完成/取消提交顺序 | SDK 与认证 IPC 各覆盖两种先后，共四场景；独立执行者另跑四场景；迟到/重复操作与两次重开保持已提交事实和连续游标 | `supplemental-originals-bound-r07.json` |
| 部分提交与清理失败 | Core/Cells/Requests/Queue 写入失败和 Device 清理失败保留原身份、unknown/draining 与名额；只有显式核对、修复和恢复才能推进 | 原 owner/defects 审计、补充三修复窗口、最终回归 |
| 公开 SDK 与认证 IPC | 查询、拒绝、可信核对、同会话重开和显式新任务均走公开路径；独立执行者实际复现关键崩溃、活执行者和组合失败 | 三份独立审计、补充原件审计、最终客户端检查 |

`final-coverage-r07.json` 逐项列出上述对应关系。该文件是证据索引，不是以一个通过字段代替原件。`original-effects-rechecked-r07.json` 在最终源码上下文重新读取原 40 窗与六核对记录；它没有重新运行场景，也没有声称重新从全部原始 SQLite 导出了所有表。

## 40 个实际 SIGKILL 窗口

主矩阵原件在 `independent-owner-final-20261022-r01/`。13 个构建/执行阶段均退出 0。每个窗口先等待具名提交点，保存原记录，再杀掉实际子进程并等待退出，随后重开同一 Session。窗口不是用普通异常、Future Drop 或连接断开替代。

| 路径 | 原注入点 |
| --- | --- |
| SDK：26 个 | `cells.intent.before`、`cells.intent.committed`、`core.plan.before`、`core.plan.committed`、`core.start.before`、`core.start.committed`、`host.effect.before`、`host.effect.visible`、`host.effect.durable`、`core.capability_terminal.before`、`core.capability_terminal.committed`、`cells.call_terminal.before`、`cells.call_terminal.committed`、`cells.response_ack.before`、`cells.execution_terminal.committed`、`core.execution_terminal.before`、`core.execution_terminal.committed`、`core.tool_result.before`、`core.tool_result.committed`、`core.attempt_terminal.before`、`core.attempt_terminal.committed`、`requests.task_end.before`、`requests.task_end.committed`、`queue.task_terminal.commit.before`、`queue.task_terminal.commit.committed`、`sdk.reply.before` |
| 认证 IPC：14 个 | `cells.intent.committed`、`core.start.committed`、`host.effect.before`、`host.effect.visible`、`host.effect.durable`、`core.capability_terminal.before`、`core.capability_terminal.committed`、`cells.call_terminal.committed`、`core.tool_result.committed`、`requests.task_end.before`、`requests.task_end.committed`、`queue.task_terminal.commit.before`、`queue.task_terminal.commit.committed`、`rpc.reply.before` |

两种模式各有四次原执行者仍活的拒绝、五次需核对的拒绝。结果提交但客户端尚未收到的 IPC 场景在 `Runtime::call` 返回后、原响应发送前阻塞，再杀掉进程。重连只读取原 Task，不发送替代请求。

外部效果独立保存原调用身份、进入次数、文件内容和效果次数。已提交结果保持原事实；确有证据未执行时，也必须由可信宿主显式发起新的执行。unknown 不自动重试。本矩阵证明进程被 SIGKILL 后的恢复，不证明机器断电或存储设备断电恢复。

## 六个可信核对场景与独立反例

`final-independent-review-r02/six-kill/` 在 SDK 和认证 IPC 中各完成成功、操作系统可确认的失败、未执行三种场景，共六次实际杀进程后核对。

| 路径 / 原效果 | 保持原记录不变的拒绝数 | 核对后原服务进入次数 |
| --- | --- | --- |
| IPC / completed | 12 | 1 |
| IPC / failed | 12 | 1 |
| IPC / not_executed | 11 | 0 |
| SDK / completed | 13 | 1 |
| SDK / failed | 13 | 1 |
| SDK / not_executed | 12 | 0 |

合计 73 次拒绝。随后在同一 Session 显式创建的新 Task 正常完成，没有再次进入原服务。原 unknown 工具消息和事件前缀保留，裁决追加在后。

`independent-defects-audit-r01.json` 核对 Runtime 两个和 Core 两个独立真实文件反例，每个反例保存 26 个数据库原件。这些反例不是全部经 Provider/VM/SIGKILL，不能计入上面的六次进程崩溃。它们验证可靠子结果与父“未执行”矛盾，以及 Core 已提交但 Cells 仍冻结时的拒绝。

公开 Core 的独立核对还覆盖 12 个原身份变异、8 个 Fork 来源损坏、默认无验证器拒绝和合法三层 Fork。它使用真实 SQLite 与合成公开契约输入，没有 Provider、VM 或外部效果。Core 14 项、Runtime 12 项回归另有原日志。StrongFlow 的一个既有 Fork 消费者回归只证明本项接口没有破坏该消费，不表示 plugin 已验收。

## 补充修复窗口与终态顺序

### Requests 修复三窗口

作者根为 `g1-r04-author-01/`。保存程序 `programs/cayw_requests-c0635ccb727f74c9` 的 SHA-256 为 `ebf9ab58c1a5d69135186fcd941c03e20b8b8ca512a830663d9e4a66ce4af72d`。三个子进程均实际 signal 9，父测试通过。独立执行者在 `supplemental-independent-r05-01/g1-independent/` 另复现三个窗口。

| 窗口 | 杀进程时已保存的事实 |
| --- | --- |
| prepared | 准备事务已 COMMIT；原代际的磁盘标记仍为 pending；数据库已记录新代际、stopped=1、activation=0 |
| ready | 新代际的 Ready 标记已同步；数据库仍 stopped=1、activation=0，尚未启用 |
| activated | 启用事务已 COMMIT；磁盘标记为新代际 ready；数据库 stopped=0、activation=1；进程内新代际尚未发布 |

保存原 SQLite、WAL/SHM/journal、授权标记或明确的 absent 记录。修复精确重试返回相同收据，原全表不变。三窗口输入的 `inspection.tasks=[]`，没有历史 Task 或旧 grant；因此它只补修复提交窗口，旧身份、旧授权失效和完整历史由其他具名场景证明。

### SDK/IPC 完成与取消顺序

作者成功根为 `g2-r05-author-01/`。程序 `programs/1-cancel_completion_order-7b8d07ee6075aadf` 的 SHA-256 为 `0942d224a0c538ccf3cab453986c9fdae403d26153bcabac79c98f03fe2060c0`。四项通过；独立执行者在 `supplemental-independent-r05-01/g2-independent-02/` 另跑四项，均通过。

测试在一次已经进入的 Provider poll 中阻塞操作系统线程，用其他运行线程先接受取消，再释放同一次 poll。Core 的 `MessageEnd` 必须确实消费同 response_id、Stop 和原内容；仅准备好回复但随后被 Drop 不算成功消费。反向顺序先确认完成提交，再发迟到取消。两种顺序均核对原 Core 事件、Queue 终态、Requests 结算、迟到/重复取消、旧 Task resume 拒绝和两次重开。

Core 的 Idle 表示当前没有执行，Queue 的 Cancelled 和 Requests 的 Ended(Cancelled) 描述原任务的终态。这些状态词不要求相同。全表业务比较只排除 Requests 合法推进的 `last_now_ms` 及对应标记时钟；原始时钟保留，每阶段均核对标记与数据库一致。

补充原件的最终只读审计为 `supplemental-independent-r05-audit-only-01/audit.json`，SHA-256 为 `ef26eb9222359fbd1315130dfc0b6fa7a8967539d8bc393b615f5c10d4d90898`。`supplemental-originals-bound-r07.json` 重新核对五个互不嵌套证据根中的 4,492 个原件，作者/独立各三处 Requests 强杀、各四个终态顺序场景。此数量不等于最终全部历史证据文件数。

## 构建版本如何关联

| 原执行 | 保存的构建身份 | 与最终源码的关系 |
| --- | --- | --- |
| 主 40 窗、原执行者/部分提交和四反例 | r01 | 保留原程序、依赖、源码归档及原始命令 |
| 六个可信核对与继续任务、Core 来源核对 | r02 | 保留原构建，按 r01→r02→r03 精确差异关联 |
| Requests 三修复强杀 | r04 | 新的 cfg(test) 保存原件代码已实际构建并执行；没有只靠源码推断 |
| 完成/取消四顺序 | r05 | 修复新测试语法后实际构建、运行；独立保存程序再执行 |
| 最终默认 workspace、两个客户端及严格检查 | r07 | 使用完整 r07 清单实际运行；修改后的恢复消费者已在这一轮执行 |

r03→r07 恰有五处文件变化：Requests 的测试模块、新终态顺序测试、恢复测试消费者、TUI 假服务回归、独立客户端 helper 排列。`exact-source-revision-r03-r07.json` 逐件列出前后字节和摘要。`crates/*/src` 中除具名 `cfg(test)` 模块外的运行源码、所有 Cargo 清单/锁文件和 build.rs 在这段链中没有变化。客户端 helper 是原字节移动，不能据此称所有应用源码逐字相同。

原故障矩阵、六核对测试和其 fixture 输入保持不变。旧程序仍称 r01/r02/r04/r05，没有重新标为 r07，也没有声称所有历史场景在 r07 重建运行。

## 已观察失败及修正

三个产品问题分别修复：Runtime 误接受可靠失败子能力对应父 launch 未执行；Core 已提交而 Cells 尚冻结时，Runtime 允许 Fork 绕过；公开 Core 误接受可靠成功/失败子能力对应父 launch 未执行。正式失败、修复后回归与四个独立反例均保留。

后续整合检查还遇到以下问题：

- 早期 `ipc_process_cleanup`、`serve_config` 和其他正向消费者仍按旧 Session-only 取消调用。迁移为保存原 Task、读取同一 Task 当前完整目标；故意发送旧形状的负例保留。
- TUI 回归的逻辑方法是 `session.prompt`，实际 wire 方法已经是 `task.enqueue`。假服务只监听旧方法，导致三秒 watchdog 超时。只修假服务分支与原参数断言，没有延长等待或删除“关闭时不得发送取消”的断言，也没有修改产品传输路径。
- r04 的新 G2 测试把数组 `.map(...)` 直接放入 `json!`，编译报 `no rules expected '.'`。只给表达式加括号；这是编译失败，不是行为失败。r05 作者与独立四场景随后实际通过。
- r05 独立客户端 Clippy 报 `items after a test module`。原 helper 原字节移到测试模块前，没有加 lint allow。r06 定向检查和 r07 完整检查均退出 0。
- r06 全仓恢复测试在重开和取消处返回 `DatabaseBusy`，精确消息为 `request authority is busy`。未改源码的定向复现再次在重开处失败。运行中的授权检查会申请同一文件锁，并在持锁期间同步 Pending、SQLite 和 Ready；进程身份标记已写入，不代表授权锁此刻空闲。

r06 的最后一项只修测试消费者。底层默认锁等待仍为 50ms。测试只对上述精确授权锁获取失败作有界条件等待，其他错误立即失败。取消失败后还必须读同一原 Task，确认完整目标不变、仍 Running 且 `cancel_requested=false`；如果取消已经写入 Queue，立即失败，不重试。原清理失败、保留容量、身份拒绝及 unknown 断言逐字保留。Requests 七项真实持锁/停止契约测试通过，恢复目标三项通过，随后 r07 默认全仓通过。

这次定向通过日志没有 `AUTHORITY_CONTENTION_RETRY`，因此不证明这次运行实际进入 helper 重试分支。原取消错误日志也不能区分是前置授权检查还是关闭 Requests 时失败；关闭失败后 Queue 可能已经接受取消。没有保存具体锁持有者栈或单次 fsync 耗时，也没有证明定时检查补过期 tick 导致这次故障。诊断和精确 r06→r07 差异见 `authority-busy-diagnosis-r07.json` 与 `exact-source-revision-r06-r07.json`。

仓外独立检查器也有失败：Core probe 的 u64 不能直接用作 ToSql；六核对审计误读 `raw_os_error` 层级；G2 审计误把消息包装新增的 role 和运行时 duration 当成原回答变化。只修具名检查器并核对原件，原失败日志保留。G2 的原 response_id、Stop、内容、完整身份与持久记录检查没有放宽。六核对没有因字段错误重新运行来替换原证据，原 suite 的 `completed_all=false` 仍保留。

`failure-index-reconciliation-r01.json` 保留较早失败。`failure-supplement-r07.json` 另核十条具名后续失败及实际成功替代记录；十条不是全部历史失败的总数。G2 包装误判等原件仍保存在各证据根。

## 最终十五项检查

运行环境使用 `RUSTUP_TOOLCHAIN=1.95.0-aarch64-apple-darwin`、`CARGO_INCREMENTAL=0`。主线程生效构建目录为 `/Volumes/ORICO/cayw-issue7-final-target-20261022-a`。以下共同绑定 tested r07 清单，每条命令运行前后源码不变，全部退出 0。

| 检查 | 实际命令或范围 | 记录前缀 |
| --- | --- | --- |
| 1 | `cargo fmt --all -- --check` | `issue7-final-r07-fmt` |
| 2 | `cargo check --workspace --locked --offline` | `issue7-final-r07-check` |
| 3 | `cargo test --workspace --locked --offline` | `issue7-final-r07-test` |
| 4 | `cargo clippy --workspace --all-targets --locked --offline -- -D warnings` | `issue7-final-r07-clippy` |
| 5 | `cargo clippy -p cayw-runtime -p cayw-core -p cayw-cells --features fault-injection --all-targets --locked --offline -- -D warnings` | `issue7-final-r07-fault-clippy` |
| 6 | `cargo run -p cayw-agent --example offline --locked --offline` | `issue7-final-r07-offline` |
| 7–10 | `examples/headless-client/Cargo.toml` 的 all-targets check/test/clippy，以及 fmt | `issue7-final-r07-headless-*` |
| 11–14 | `examples/issue1-client/Cargo.toml` 的 all-targets check/test/clippy，以及 fmt | `issue7-final-r07-issue1-*` |
| 15 | `cargo run --manifest-path examples/issue1-client/Cargo.toml --locked --offline -- all <本轮独立输出目录>` | `issue7-final-r07-issue1-all` |

客户端 check/test/clippy 均带 `--locked --offline`；Clippy 带 `-- -D warnings`。第 15 项的精确输出路径为 `/Volumes/ORICO/cayw-issue7-evidence-20261022-a/issue7-final-r07-issue1-artifacts`，覆盖 sdk、ipc、reconnect、transport、restart-before、restart-unknown。

| Rust 测试计数 | 顶层目标 | 通过 | 失败 | 忽略 |
| --- | --- | --- | --- | --- |
| 默认 workspace | 230 | 1,516 | 0 | 7 |
| 独立 headless-client | 2 | 10 | 0 | 0 |
| 独立 issue1-client | 2 | 3 | 0 | 0 |

`final-checks-audit-r07.json` 依据各顶层目标最后一条 result 计数，三条父测试内部子进程输出单列，没有重复累计。七个默认忽略入口中，六个是由父测试单独启动的子程序入口；远程二进制场景未运行。没有运行 `--all-features`。fault-injection Clippy 证明该配置编译和严格检查通过，不能替代上面的实际故障注入执行。40 窗、六核对和补充业务场景数不加到 Rust 测试总数中。

完整 workspace 日志仍有既存多个 `offline` 示例输出文件同名提示，命令实际退出 0。本记录不把它写成“全程无提示”。Agent 示例另行明确指定包并退出 0。

默认 Runtime 运行依赖另外执行：

```sh
cargo tree -p cayw-runtime --no-default-features --locked --offline \
  --target aarch64-apple-darwin -e normal,features
```

收据为 `issue7-runtime-dependencies-r07.json`。实际运行依赖不含 TUI、WebUI、remote、VerifyTool、plugins 或 Codex。检查整个 workspace 会编译其他成员，不表示无界面 Runtime 启用了这些组件。

## 复跑与封存核对入口

常规完整检查可以在源码目录运行以下命令。新输出前缀必须未使用，记录器拒绝覆盖旧日志。运行前须保存当前源码清单；新结果只证明新清单，不会自动变成历史 r07 结果。

```sh
python3 -I /Volumes/ORICO/cayw-issue7-evidence-20261022-a/run-final-checks.py issue7-local-rerun-01
```

实际故障测试的构建与运行入口如下。须使用 macOS、同一固定工具链、独立构建目录及新的证据输出根。精确环境、保存后的程序 argv、原件根和 hash 见各构建/执行收据；测试默认临时输出不能替代正式保存原件的驱动。

```sh
cargo test -p cayw-runtime --features fault-injection \
  --test crash_recovery_matrix --locked --offline -- --nocapture
cargo test -p cayw-runtime --features fault-injection \
  --test crash_reconciliation --locked --offline -- --nocapture
cargo test -p cayw-runtime --test cancel_completion_order \
  --locked --offline -- --nocapture
cargo test -p cayw-requests --lib --locked --offline \
  store::recovery_tests::killed_repair_process_retries_preparation_ready_and_activation_windows \
  -- --exact --nocapture
```

`run-authority-crash-evidence.py` 的 `--execute` 要求新的 `--label`、`--out`、精确 `--frozen-manifest` 和 `--frozen-inventory`；它保存实际构建程序、依赖和三窗口原件。`g2_cancel_completion.py --execute <新证据根>` 要求 `CAYW_REVIEW_FROZEN_MANIFEST` 与 `CAYW_REVIEW_FROZEN_INVENTORY`，保存四顺序原件。三份收尾文档变化后，不能把旧 r07 manifest 直接当成当前完整清单传入；驱动会拒绝。

最终封存由 `assemble-delivery.py` 输入十五项审计、tested r07、失败索引、文档检查、七条件索引、精确差异链、默认依赖收据和补充原件审计。它重新核对 #6 的每条封存源码及真实交付摘要，保存当前源码与全部具名历史证据根清单。随后执行：

```sh
python3 -I /Volumes/ORICO/cayw-issue7-evidence-20261022-a/check-completion-readiness.py \
  /Volumes/ORICO/cayw-issue7-evidence-20261022-a/final-local-delivery-v2.json
```

这一步逐件核对交付归档、支持证据、所有原件和当前源码。最终封存与 readiness 的实际退出状态应读其收据和原日志，不能仅凭本文件中的说明判断后来修改过的源码。

## 验证边界

本项只验收 macOS ARM64 的本机无界面路径。其他 macOS 架构、机器断电、远程恢复、UI、plugin 整项功能和其他平台未在本项验收。SIGKILL 不等于断电；Device 清理成功不等于业务效果撤销；进程隔离不等于安全沙箱。

可信宿主验证器是部署方必须提供的授权边界。测试中的宽验证器不构成实际授权。Requests 修复、停止证明与恢复各自完成自己的步骤，不能把一次成功当成跨库自动回滚或跨系统恰好执行一次的承诺。后续任务或源码变化需重新验证相应范围。
