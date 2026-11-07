# 重构实施与验证记录

本记录对应 `CAYW 重构实施方案.md` 的六阶段实施。冻结起点为 `e738dae1af73bf0ad3fe15f729a4557a2d9f4d24`，分支为 `integrate/f01-f20-20261027`。交付源码尚未提交。最终文件哈希、原始命令、退出码和日志由 `.local/refactor-20261029/` 下的收据绑定；历史报告不作为当前源码的通过证据。

## 环境与证据范围

- macOS 27.0.1（26A434），Apple M4，arm64，16 GiB。
- Rust/Cargo 1.95.0，Node 24.4.1，SQLite 测量版本 3.53.2。
- 工作树：`/Volumes/ORICO/CAYW/.claude/worktrees/integrate-f01-f20-20261027`。
- 默认检查不继承模型 API key。本轮没有新增真实模型或付费调用。
- 只验证本机 macOS ARM64。不外推其他 macOS 架构。
- 原始日志存放在本机被忽略的 `.local` 目录。仓库内保留测试、固定样本、复跑命令和本记录。远端 CI 尚未执行。

## 实施结果

| 阶段 | 交付 |
| --- | --- |
| 0 | DDG 按 UTF-8 字符边界截断，识别大小写不同的 anchor；现有 macOS CI 安装 Node 24 并准备 Editor/Pi 的 npm 依赖；修复反馈消费游标竞态和顺序；Trace 公共入口保留一次结构校验；架构与路线图按职责归位 |
| 1 | `cayw-agent-types` 承接同一 `AgentEvent`/`ToolResult` 定义；Agent 原路径重导出；Core 正常依赖移除 Agent 执行实现；保留旧 JSON 与 Core v3 样本 |
| 2 | `cayw-queue` 承接持久队列、Task、取消目标、输入及资源准入；Runtime 保留授权、跨组件锁、执行清理、恢复和错误转换；Requests 重导出同一 TaskIntent |
| 3 | 复用 `core.start.before`/`core.start.committed`，增加真实进程恢复矩阵；补 Planned 和零 capability 的 Core 恢复断言；VerifyTool 使用窄 `Runtime::approval` 查询 |
| 4 | 模型账本准备代码去重；五个集成目标合为 `model_ledger`；保留 21 条单元和 24 条集成测试的各自行为 |
| 5 | 新增真实 SQLite、公开 API 的持久化基准；先使普通 `mutate` 复用缓存并在提交后回填；独立诊断确认多会话零命中后，再改为有界的按会话缓存；两项优化分别复测 |

`Runtime::store()` 继续公开，文档明确其为可信宿主入口。Runtime 恢复仍须调用 `Runtime::recover()`，以先确认原执行者清理。Core v3 与 IPC v1 保持不变。没有降低 WAL/FULL、wait 日志校验或 unknown 保留要求。

队列的终态与审计在同一队列事务提交。Requests 使用独立存储，不承诺两个数据库共同原子提交。Requests 终态已提交、队列 COMMIT 失败时，Runtime 保留 `draining` 和执行名额，等待显式恢复。确定性延迟外键测试覆盖此窗口。

## 公开 API 迁移

以下直接校验方法原先返回 `RuntimeError`，现在返回 `QueueError`：

- `TaskInput::validate`
- `TaskResources::validate`
- `ResourcePolicy::validate`

调用方可匹配 `cayw_runtime::QueueError::InvalidInput`。Runtime 重导出的是 `cayw_queue::QueueError` 本身。进入 Runtime 边界时使用 `RuntimeError::from` 转换。Runtime 的入队、运行和 RPC 仍返回原层的错误。重导出不代表这三处方法的返回类型保持不变。

`TaskIntent` 在 Queue 中只有一份定义，Requests 与 Runtime 重导出同一类型。输入 JSON 和 Task 持久表结构不变。类型 crate 仍依赖 `cayw-ai`，这次只解除实现依赖，没有建立独立磁盘格式。

新增 `Runtime::approval(session_id, approval_id)`，返回 `Result<ApprovalRequest, RuntimeError>`，供 VerifyTool 只读查询审批。该方法不提供可变 Core 操作。

## 固定历史样本

两个样本均由冻结旧实现生成，普通测试只读，不在当前实现上重生成。

| 样本 | 大小 | SHA-256 |
| --- | --- | --- |
| `crates/cayw-agent/tests/fixtures/agent_events.golden.json` | 7,254 B | `0e61257ea651329423c7532263daf17514d0c3008579bf30800e83f0f74eedc8` |
| `crates/cayw-core/tests/fixtures/v3-sample.sqlite` | 57,344 B | `fe3d760f6c2b1d7b9ac0aca0a5163a93b19f8f3ffb1655093fc73d5ae6000532` |

JSON 覆盖十个事件变体和两个工具结果，包含 unknown 与可选字段。字段改名负例证明固定样本会拒绝不兼容变更。SQLite 包含一条已结束会话与 22 条事件。测试复制样本后打开、读取并执行 idle 恢复；它证明旧数据可读，不单独证明 unknown 恢复。unknown 和进程清理由各自测试覆盖。

两份生成器原件随仓库保存在 [`refactor-fixture-generators-20261029/`](refactor-fixture-generators-20261029/README.md)，含文件哈希与独立旧提交副本的复跑方法。当前 Cargo 测试不会编译这些生成器。本机 `.local/refactor-20261029/` 下的 `baseline-source.json`、`baseline-generators.json`、`historical-fixtures.json` 保留归档与样本哈希；`baseline-golden-generate-01/` 和 `baseline-v3-generate-01/` 保留实际命令、退出码及原始日志。

## 模型账本测试试点

同一个 `pilot-e738dae` 路径、同一个 target、Rust 1.95.0、两个构建任务。生产代码前缀逐字节相同。每组先预热依赖，再仅清理 Runtime 包重建三次；`CARGO_INCREMENTAL=0`。这组数据不等于全空 target 的冷构建。

| 配置 | Runtime 包重建中位数 | 无改动构建 | 21 条单元测试命令 | 24 条集成测试命令 |
| --- | ---: | ---: | ---: | ---: |
| 原测试 | 51.646 s | 0.544 s | 0.758 s | 9.628 s |
| 仅去重 | 51.495 s | 0.490 s | 0.738 s | 9.547 s |
| 去重并合并目标 | 50.057 s | 0.501 s | 0.706 s | 6.268 s |

表中执行时间包含 Cargo 调用开销，执行各跑一次。合并目标另以单线程执行，24 条全通过，命令耗时 17.144 s。三组逐条测试名核对一致，不只比较数量。合并相对原测试减少包重建中位数约 3.1%，单次集成命令耗时减少约 34.9%，据此保留这组合并。没有推广到其他功能组。

`model_requests.rs` 只整理测试模块内的准备代码：增加 72 行、删除 163 行，净减 91 行。这个数字只指该文件。没有删除不同不变量。原五个集成文件内容原样移动到 `tests/model_ledger/`，共 1,891 行；旧路径显示的删除属于文件移动，不是删除测试。新入口为：

```sh
cargo +1.95.0 test -p cayw-runtime --test model_ledger --locked
cargo +1.95.0 test -p cayw-runtime --test model_ledger provider_usage:: --locked
cargo +1.95.0 test -p cayw-runtime --lib model_requests::tests --locked
```

原 `--test provider_usage` 等命令用于旧版本历史记录。当前使用 `model_ledger` 加模块过滤。完整逐项清单见 [`refactor-model-ledger-behaviors-20261029.md`](refactor-model-ledger-behaviors-20261029.md)，计时数据见 [`refactor-model-ledger-timings-20261029.json`](refactor-model-ledger-timings-20261029.json)。本机逐项结果为 `.local/refactor-20261029/pilot-behavior-map.json`；计时原件为该目录下的 `pilot-comparison-01/comparison.json` 及各 `pilot-*` 收据。

21 条单元覆盖预留、派发边界、重复用量观察、终态保存、取消、过期租约、未知预留和可信核对。24 条集成覆盖 HTTP 已知/未知用量、存储失败、父子与分叉预算、上下文查询、本机 HTTP 截止时间和显式合成用量。测试使用固定响应或本机 HTTP，不代表真实付费模型验收。

## 基线与失败记录

冻结原提交实际执行了 fmt、check、workspace test 与 Clippy。默认测试为 2,641 passed、0 failed、11 ignored，不以 ignored 充当通过。11 项分别为：

- 八个由父测试启动的进程辅助入口：`crash_effect_child`、`crash_child`、`live_scope_child`、`lifecycle_child`、`held_runtime_process`、`configured_transport_child`、`cleanup_failure_runner`、`fixture_child`。
- 一个需要单独构建 Device example 的本机二进制场景：`server_binary_device_example_and_operator_commands_work_on_loopback`。本轮单独执行的实际结果列在最终验证中。
- 两个需要外部 fixture 输入的场景：`complete_capture_preserves_stream_and_accounting` 需要 `CAYW_REPLAY_CAPTURE`；`local_stream_stages` 需要 `CAYW_STREAM_FIXTURE`。本轮未提供这两份输入，未执行。

保留以下失败，后续结果不覆盖原件：

- `baseline-fmt-01`：本轮附加生成器未格式化。生成器移出自动测试后，`baseline-fmt-02` 通过。
- `baseline-clippy-01`：本轮附加基准有两个可合并 if。移出附加文件后，原提交 `baseline-clippy-02` 通过；随后已修基准。
- `refactor-check-01`：`collaboration.rs` 仍匹配旧 Runtime 错误类型。改为 QueueError 后 `refactor-check-02` 通过。
- `refactor-fmt-02`：并行写入的基准格式尚未统一，矩阵整体失败。随后统一格式，最终单独检查。
- `refactor-core-02`：新增测试误把正在执行的父工具当作无待结束调用。修正前置断言后 `refactor-core-03` 通过。
- `refactor-final-clippy-01`：队列泛型终态方法保留了三处 `QueueError` 到自身的重复转换，Clippy 拒绝。删除重复转换后，39 条队列测试、Runtime 提交失败回归及 `refactor-final-clippy-02` 均通过。
- `diag-bounded-build-01`：独立诊断副本在关闭 store 后才读取缓存保留量，Rust 拒绝编译。改为关闭前读出，在新的 `diag-bounded-02` 副本复跑；失败副本和收据保留，产品代码不受影响。

## 已完成的恢复与边界验证

以下首轮命令已实际完成。按会话缓存定稿后的复跑结果见最终验证。计数按各次调用分别列出，不相加成不同测试的总数。

| 检查 | 实际结果 | 本机收据 |
| --- | --- | --- |
| 新进程启动前恢复测试，`fault-injection` | 1 passed、0 failed；两个真实 SIGKILL 窗口 | `refactor-process-windows-01` |
| 现有 SDK/IPC 崩溃矩阵，`fault-injection` | 2 passed、0 failed；SDK 26 个、IPC 14 个窗口 | `refactor-crash-matrix-01` |
| 崩溃后可信核对与原 Session 继续，`fault-injection` | 3 passed、0 failed，包含子进程入口 | `refactor-crash-reconcile-01` |
| Runtime 的 `plugins,pi-compat,strongflow,remote` 全目标编译 | 退出码 0 | `refactor-feature-check-01` |
| 插件执行、生命周期、Pi/MCP 与 review services | 16 passed、0 failed | `refactor-features-tests-01` |
| 独立 headless-client：fmt、check、test、Clippy | 四项通过；41 passed、0 failed、0 ignored | `refactor-client-{fmt,check,test,clippy}-01` |
| 真实本机 Server/Device/operator 二进制 | 1 passed、0 failed、0 ignored | `refactor-remote-binary-01` |
| Agent 离线示例 | 工具结果 5；最终 5 条消息，`running=false` | `refactor-offline-01` |
| Core 与 Queue 的 normal 依赖树 | Core 的项目依赖只有 types/ai；Queue 只有 ai | `refactor-tree-{core,queue}-01` |

新进程测试复用两个现有故障点：`core.start.before` 与 `core.start.committed`。测试先确认旧执行者存活时恢复返回 Busy；真实 SIGKILL 后，新 Runtime 仍不能代替旧 Host 证明清理。只有可信宿主核对原 Task、完整 Core lease、supervisor 身份和空的进程清单后，恢复才释放队列名额。开始记录尚未提交时 capability 为 NotExecuted；已经提交时保留 OutcomeUnknown，Cells 保持冻结。旧 fence 不复用，原模型账本不改变，原工具不重放。

进程原件存放在 `.local/refactor-20261029/process-windows-01/` 与 `crash-matrix-01/` 的各次随机目录中，含 `executed-windows.json`、SQLite 和子进程输出。目录名中的 `windows` 指故障窗口，运行平台为 macOS。复跑入口为：

```sh
cargo +1.95.0 test -p cayw-runtime --features fault-injection --test refactor_process_recovery --locked -- --nocapture
cargo +1.95.0 test -p cayw-runtime --features fault-injection --test crash_recovery_matrix --locked -- --nocapture
cargo +1.95.0 test -p cayw-runtime --features fault-injection --test crash_reconciliation --locked -- --nocapture
```

设置 `CAYW_PROCESS_RECOVERY_ROOT` 或 `CAYW_CRASH_MATRIX_ROOT` 到新的目录可保留相应场景原件；不设置时沿用测试的临时目录处理。

独立客户端的锁文件已离线更新，只增加两个内部包及其依赖接线，没有升级外部依赖。该客户端是独立 workspace，未用根 workspace 结果代替它的检查。本机二进制场景只证明 loopback，不外推为两台设备联调。

## 最终验证

首轮全仓默认测试为 2,651 passed、0 failed、11 ignored。随后 Clippy 发现并修复三处同类型错误转换，修复后全仓 Clippy 通过。以上结果发生在按会话缓存定稿前，不能替代最终源码检查。

按会话缓存定稿后的全部 19 个最终检查步骤均已通过。产品文件在各次检查前后没有变化。最终非文档交付共 835 个文件，清单哈希为 `d9161f327e1f7c76489f469a33c4c01cc49d939c77542c46f7fa1a0ca5667e9c`，见 [`refactor-source-20261029.json`](refactor-source-20261029.json)。每项检查的完整命令、退出码、日志哈希、计数和源码匹配结果见 [`refactor-check-results-20261029.json`](refactor-check-results-20261029.json)，19 项均与交付文件集合和哈希完全一致。

| 检查 | 结果 | 命令耗时 | 本机收据 |
| --- | --- | ---: | --- |
| 全仓格式检查 | 退出码 0 | 1.571 s | `refactor-postlint-final-fmt-01` |
| 全仓编译检查 | 退出码 0 | 7.494 s | `refactor-postlint-final-check-01` |
| 全仓默认测试 | 2,656 passed、0 failed、11 ignored | 837.293 s | `refactor-postlint-final-tests-01` |
| 全仓所有目标 Clippy，警告视为错误 | 退出码 0 | 24.040 s | `refactor-postlint-final-clippy-01` |
| 已跟踪文件差异空白检查 | 退出码 0 | 0.048 s | `refactor-postlint-final-diff-01` |
| 新进程启动前恢复 | 1 passed、0 failed、0 ignored；两个 SIGKILL 窗口 | 13.329 s | `refactor-process-windows-02` |
| SDK/IPC 崩溃矩阵 | 2 passed、0 failed、0 ignored；SDK 26、IPC 14 个窗口 | 35.046 s | `refactor-crash-matrix-02` |
| 崩溃后可信核对与原 Session 继续 | 3 passed、0 failed、0 ignored | 10.499 s | `refactor-crash-reconcile-02` |
| Runtime 可选功能全目标编译 | 退出码 0 | 15.011 s | `refactor-feature-check-02` |
| 插件、Pi/MCP 与 review services | 16 passed、0 failed、0 ignored | 14.676 s | `refactor-features-tests-02` |
| 独立客户端测试 | 41 passed、0 failed、0 ignored | 16.751 s | `refactor-client-test-02` |
| 本机 Server/Device/operator 二进制 | 1 passed、0 failed、0 ignored | 0.209 s | `refactor-remote-binary-02` |

独立客户端的 fmt、check 和 Clippy 也在定稿源码上通过。Core/Queue 正常依赖树再次核对一致，离线示例仍得到工具结果 5、最终 5 条消息和 `running=false`。对应收据为 `refactor-client-{fmt,check,clippy}-02`、`refactor-tree-{core,queue}-02`、`refactor-offline-02`。原始故障目录为本机 `process-windows-02/` 和 `crash-matrix-02/` 下的新随机目录，三个随机目录的清单路径分别列入检查结果索引的 `sigkill_windows.{process,ipc,sdk}.evidence`。

按会话缓存定向 Core 测试为 134 passed、0 failed、0 ignored。普通复用的正式对照共 54 次；有界缓存另有 24 次 release 对照、六次异常配置追加复跑和六次实际缓存计数。测量不与 Cargo 检查同时运行。性能记录保留退化配置、额外内存成本与异常首轮，没有用复跑抹掉原件。

本轮已安装 Editor/Pi 的 Node 依赖，WebUI 的 90 项和 Visual Editor 的 42 项 Node 脚本测试均通过，原件为 `editor-deps-01`、`pi-deps-01`、`webui-node-01`、`visual-node-01`。相关源码仍与这些检查时一致，收据与通过数见 [`refactor-node-checks-20261029.json`](refactor-node-checks-20261029.json)。这些是脚本测试，不是浏览器 UI 验证。现有 CI 配置已补 Node 24 与两处安装步骤；远端 GitHub Actions 未运行。

默认测试命令仍打印冻结基线已有的 Cargo 警告：多个包的 `offline` example 输出文件同名，涉及可执行文件及 dSYM。命令退出码为 0；本轮没有重命名这些示例。最终 Clippy 无警告。不能把默认测试写成无警告运行。

默认 11 个 ignored 的分类与前文一致：八个辅助入口由父测试启动，一个本机二进制场景已单独补跑，两个需要外部 fixture 输入的场景未执行。当前结果只证明这份源码在本机 macOS ARM64 的上述范围，不表示所有 Issue 已验收。

本轮六阶段的本地实施和上述验收已完成。Core v3、IPC v1、FULL、wait 日志校验、unknown 和旧执行者清理要求均保留。全部七次失败收据的索引见 [`refactor-retained-failures-20261029.json`](refactor-retained-failures-20261029.json)。以上检查在提交前的工作树上运行，源码绑定以文件清单和哈希为准。

持久化测量见 [`refactor-persistence-20261029.md`](refactor-persistence-20261029.md)，计时与业务验收分别记录。远端 GitHub Actions、其他 macOS 架构和新增真实模型调用均未验证。
