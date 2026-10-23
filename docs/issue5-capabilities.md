# Issue 5 预置 CodeMode 能力契约矩阵

本文逐条列出 Issue [#5](https://github.com/changw98ic/CAYW/issues/5) 八组能力中**实际存在**的每个原语和 catalog 能力。每一行说明：参数、返回、精确权限与资源范围、预算、取消与失败语义、SDK/认证 IPC 入口、成功入口与适用负例入口，以及实际跑过的日志。

本文描述逐能力契约、具名场景和原始证据。2026-10-21 已完成本机 macOS arm64 范围验收；完整命令、源码清单、结果与保留边界见[验收记录](validation/issue-5-preset-capabilities-macos.md)。GitHub 状态未在本次工作中修改。

## 状态与证据规则

- **已跑（日志）**：在本轮 Issue 5 证据集中有实际运行日志，日志内 `test result: ok`。日志绝对路径见 §0.8。
- **覆盖层**：逐能力表分别标明执行器、Host、Runtime 或认证 IPC 的实际覆盖；不能把某一层通过写成所有入口均有独立专项。最终 E-FT 已运行下表引用的具名测试。
- **不写未执行场景为通过**；**不把范围缺口写成"不适用"**；**已有实现不等于验收**。
- 失败或中间日志（例如 `dynamic-requests-bridge-regression.log` 的 1 failed）不作为通过证据；以同名的最终 ok 日志为准。
- 合成 Provider、真实本机执行与真实模型证据分开。本轮使用固定离线 Provider、测试宿主和本机适配器；Provider 包括 `cayw_runtime::fixture` 与各测试自己的离线实现。文件、Git、进程和认证 IPC 使用真实本机路径。**没有真实模型调用**，也没有付费测试。
- 环境：Rust 1.95.0、macOS arm64（`aarch64-apple-darwin`）、debug 构建。工作树 `/Volumes/ORICO/cayw-issue5-integration-20261021-a`，分支 `feat/issue5-preset-capabilities`。Core 协议/存储 v3，IPC 帧协议 v1，两者是不同层，不统一数字。
- 部分历史日志使用目标目录 `/Volumes/ORICO/cayw-issue5-target-20261021-a`；本轮第三/四/五组使用独立目标目录 `/Volumes/ORICO/cayw-preset-workspace-target-20261021-a`。目标目录未删除，证据目录原路径未改动。

---

## 0. 所有能力共用的契约

### 0.1 模型可见入口与分发路径

- 持久 Runtime 只向模型注册一个工具：`execute_code`（`crates/cayw-cells/src/tool.rs`）。不注册第二个 CodeMode 工具，不注册旧文件系统直连工具。
- 脚本内用 `cayw.call(name, args)` 调用任意 catalog 能力。每一次调用都经：Runtime 队列与尝试租约 → Cells VM → `CellBackend` → 作用域 Host（`bridge::scoped_host`）→ Core planned/started/settled 记录 → Workspace/Data/Git/Profile/Terminal/External 执行器。目录里有名字不等于有权限。
- `cayw.discover` / `cayw.describe` / `toolDefinition` / `tool_search` / `ALL_TOOLS` 只做发现，不产生授权。
- 脚本环境没有 `io`、`os`、`package`、`require`、`print`、`pcall`、`xpcall`，也没有 `math.tointeger`。string/table/math/utf8 受限。`load(key)` 是受控的会话值读取 helper，经 `cells.load` 逐次授权；它不能加载或执行任意代码。

### 0.2 精确权限

- 授权单位是 `ScopedPermission { capability, scope }`。`scope` 是参数的规范化 JSON：对象键递归排序、数组保持顺序、数字不转换类型（`crates/cayw-runtime/src/permissions.rs` 的 `canonical_scope`）。
- 匹配是**全等**：不接受前缀、通配、同级文件、兄弟目录或"同一能力换参数"。一次授权只覆盖那一个能力名加那一组完整参数。
- 未授权的精确调用：Core 先写 durable planned 调用和待审批记录，Session 进入 awaiting approval，Host 尚未产生效果。审批选择为 `AllowOnce` / `AllowTask` / `Deny`。
  - `AllowOnce` 只放行这一次目标调用；另一个能力或另一组参数仍需自己的审批。
  - `Deny` 不执行，且不能被同一任务/范围的另一次审批覆盖。
  - 等待期间取消：待审批请求被拒绝/关闭，无文件效果，任务进入 cancelled，迟到的 allow 不能复活。
- 撤销：`Runtime::revoke(session, grant, actor)`（SDK）与 IPC `approval.revoke`。撤销写 grant 记录、使该 Session 的共享读缓存失效（`inner.reads.invalidate(id)`）并取消任务。
- 任务请求审批层（`request_policy`）可对**已存在的精确 Host 操作**追加一次任务请求审批；它不自动授权，也不绕过 Host 权限门。

### 0.3 资源范围

- 工作区根由 Host 持有的目录句柄固定。路径必须是工作区相对路径；拒绝 `..`、绝对路径、Windows 前缀和保留设备名；不能寻址 `.git` 或任何 `.cayw-*` 组件（大小写不敏感）。不跟随符号链接；socket、设备、FIFO、特殊条目被拒绝。路径被重命名后，已持有的根句柄保持原目录，不按外部路径重新打开。
- Git 写入固定使用工作区根下的真实仓库，拒绝符号链接 `.git`、`gitdir:` 文件、bare 仓库、alternates、危险本地配置；hooks、filter、外部 diff/pager/credential、签名和网络全部禁用；身份由 Host 提供，模型参数不能改作者/提交者。
- 终端（`exec_command`）只接受应用配置好的 profile；程序、参数、环境、工作目录和 I/O 模式（pipe 或 macOS PTY）由 Host 固定。启动时模型只给 profile 名；之后只能对 Host 返回的 session 发送 `write_stdin` 数据或按游标读取，不能更换程序、argv、环境、cwd 或终端尺寸。
- 外部能力（`search.query`、`mcp.*`、`skills.*`）经宿主已注入的 source/Registry 消费者；Core 不新建 MCP 连接、Skills loader 或持久索引。资源 URI 对 Bridge 是不透明字符串，不作为文件路径打开。
- 本机子任务（`agent.run`）在同一数据库里创建真实子 Session/Task/Attempt；子任务权限只来自宿主配置的 `LocalAgents.permissions`，不能继承或提升父权限。

### 0.4 预算

Host 级（`cayw_host::HostConfig`，默认值以源码为准）：`max_argument_bytes`、`max_result_bytes`、`max_total_bytes`（本 Host 实例累计序列化参数+结果字节）、`max_calls`（本 Host 实例累计调用数）、`max_file_bytes`、`max_list_entries`、`call_timeout`（含等待权限与执行，审计写入在取消后仍会等待）。

Runtime 级（`cayw_runtime::RuntimeConfig`）：`max_capability_calls`（默认 128）、`max_capability_bytes`（默认 8 MiB）、`max_turns`、`max_run_time`、`max_total_tokens`、`max_prompt_bytes`、`max_history_bytes`。

能力本地预算：

| 域 | 关键默认值 |
| --- | --- |
| Cells（`CellLimits`） | `script_bytes` 64 KiB、`output_bytes` 1 MiB、`output_items` 256、`item_bytes` 64 KiB、`media_bytes` 32 KiB、`max_output_tokens` 128 KiB、`max_yield_time` 30 s、`store_bytes` 1 MiB、`store_keys` 256、`json_depth` 32、`max_cells` 256、`max_running` 8、`cache_entries` 128 |
| VM（`cayw_codemode::Limits`） | `memory_bytes` 16 MiB、`instruction_budget` 100000 检查点、计算 `timeout` 5 s、单次 `host_wait` 60 s（最多 1 h）、`shutdown_grace` 2 s、`host_calls` 64、共享 JSON `output_bytes` 1 MiB；Host 等待与受控 store/load 暂停计算时钟，各自受 `host_wait` 限制；显式 yield 暂停到 wait 续跑 |
| Workspace（`Limits`） | `max_file_bytes` 1 MiB、`max_batch_bytes` 16 MiB、`max_batch_operations` 64（schema 上限 256）、`max_batches` 128、`max_tree_entries` 4096、`max_depth` 32、`max_argument_bytes` 8 MiB、`max_output_bytes` 8 MiB、`max_archive_bytes` 16 MiB、`max_archive_entries` 4096、`max_archive_expanded_bytes` 16 MiB、`max_compression_ratio` 100、`timeout` 30 s |
| Data（`Limits`） | `input_bytes` 1 MiB、`output_bytes` 1 MiB、`max_depth` 32、`max_nodes` 100000、`max_rows` 10000、`max_diff_work` 1000000 |
| Terminal（`TerminalLimits`） | `max_sessions` 4、`max_sessions_total` 64、`max_input_bytes_per_write` 64 KiB、`max_input_bytes_per_session` 1 MiB、`max_output_bytes_per_session` 1 MiB、`max_read_bytes` 64 KiB、`max_wait` 30 s |
| Git | 提交消息 ≤ 64 KiB、stage 路径 1..256 且唯一、`git.log` limit 1..100、对象 ID 必须 40 或 64 位十六进制 |
| LocalAgents | `max_children` 4、`max_depth` 1、`shared_total_tokens` 32768、`max_calls` 16、`max_output_bytes` 256 KiB、`max_time` 30 s |

超预算必须分两段判断，不能一律写成普通 `Err`：

- **效果开始前**：参数/schema、参数字节、`max_calls`、`max_total_bytes`、`max_file_bytes`、`max_list_entries` 等检查失败，或 Host 预算已用满、授权被拒，都是已知失败（`RuntimeError::Budget` / 普通 `CapabilityError`）。下一次调用在授权前失败时不产生效果。只读能力（如 `fs.read`、`fs.tree`、`data.*`）的结果超过结果预算且没有副作用，也可以是已知失败。
- **副作用已经开始后**：写入、补丁、移动、删除、批次提交、Git 写或进程已经启动，随后才发现结果无法在 `max_result_bytes`/`max_total_bytes` 内表示，或审计结算失败，必须返回 `outcome_unknown` 并冻结 Host。此时不能把"已发生的效果 + 结果放不下"降级成普通 `Err`。E-CFS 的 `large_host_result_does_not_erase_a_completed_effect`、`over_budget_host_results_keep_success_status_with_omitted_payload`、`output_limit_stops_emits_without_discarding_successful_audit` 覆盖了"效果已完成、结果被省略/超限"的保留语义。Host 侧 E-G150 `host::result_and_directory_budgets_prevent_unbounded_data` 断言写入已提交但结果超预算时返回 unknown 且文件存在；`builtin_result_or_audit_loss_preserves_unknown_and_blocks_recovery_bypass` 覆盖目录已创建后结果或审计丢失。E-PW 的调用前预算负例不代替这两项。
- 纯计算的 `data.*`/`text.diff`/`hash.digest`/`codec.*` 没有外部副作用，超预算只能是已知失败；它们不能用来外推有副作用能力的行为。

### 0.5 取消与失败语义

- 取消令牌贯穿 Runtime → Cells → Host → 执行器。丢弃运行 Future 会取消令牌并释放内存运行状态；这不证明外部副作用已撤销。
- 已知失败用普通 `Err`；无法确认副作用必须返回 `outcome_unknown`。Host 一旦出现 unknown 会 latch：`Host::outcome_unknown()` 为真，后续调用停止，Session 需要核对。
- 读栅栏（read fence）：前一个变更调用留下 `pending` 时，新 Host 的变更调用返回 unknown `workspace mutation requires original owner recovery`。
- 批次阶段：`prepared`、`intent`、`committing`、`committed`、`aborted`、`recovering_commit`、`recovering_rollback`、`rolled_back`、`outcome_unknown`。`pending()` 包含 intent/committing/recovering_commit/recovering_rollback/outcome_unknown。pending 批次挡住普通写入、profile 与 Git 写，但 status/tree/archive/Git 只读仍可用。
- 恢复流程（真实产品路径）：`Runtime::recover(session)` → `Runtime::reconcile_capability`（给出已知结果与证据）→ 修复 Cells 记录、清除 `backend_error`、`sync_host_fences` 调用 `host.recover_mutation` → 之后新 Runtime 才能执行 `fs.recover_change_batch`。不自动重放。
- 总运行时限（`max_run_time`）：等待 `user.ask` 时超限，Runtime 返回 `Err(Budget("time"))`，任务终态为 Failed，`cancel_requested=false`，错误文本为 `task exceeded its time budget`。该场景没有未确认的外部效果。已开始的写入或进程仍按真实结算结果保留 unknown，不能由人类等待场景外推所有超时都无 unknown。具名入口为 `crates/cayw-runtime/tests/host_wait.rs::runtime_absolute_deadline_still_ends_a_paused_human_wait`；E-RT 已实际执行并通过上述精确终态断言。
- 取消、`reset()`、重开、重复恢复和 fork 都不能把 unknown 变成已知成功。

### 0.6 SDK 与认证 IPC 入口

- SDK：`cayw_runtime::Runtime` 的 `open`、`create`、`prompt`、`enqueue_input`、`approve`、`revoke`、`cancel`、`events`、`get`、`tasks`、`task`、`recover`、`reconcile`、`reconcile_capability`、`answer_question`、`questions`、`pending_questions`、`plan`、`capabilities`、`grants`、`fork`。
- IPC v1：只接受 loopback（`127.0.0.1` / `::1`），连接第一帧必须是带宿主 token 的 `hello`，成功后返回版本与限制。方法映射：`session.create`、`session.get`、`session.list`、`session.prompt`、`session.events`、`session.cancel`、`session.grants`、`session.recover`、`session.reconcile`、`capability.reconcile`、`session.fork`、`approval.decide`、`approval.revoke`、`capabilities.list`、`session.questions`、`session.pending_questions`、`session.plan`、`question.answer`、`session.tasks`、`task.get`、`task.enqueue`、`task.resume`。
- IPC 审批 actor 固定为 `rpc-user`；模型或客户端提供的 `actor` 非法。SDK actor 是可信宿主输入。回答澄清问题只走 `question.answer` / `Runtime::answer_question`，回答不产生权限授权。
- 错误：`ClientError { code, message, outcome_unknown }`。`outcome_unknown=true` 时必须先核对持久状态，不能盲目重试。Core 事件是持久拉取分页，不是推送通道。
- 每个能力在表中"SDK/IPC"列只列它自己的入口；模型侧统一是"在 `execute_code` 脚本里 `cayw.call`"。
- 认证 IPC 的具名覆盖：E-RT `ipc::authenticated_rpc_uses_real_runtime_and_durable_cursor_pages` 实际连接并调用 `capabilities.list`；`killed_binary_before_approval_is_recovered_as_not_executed_without_replay` 实际调用 `session.recover`。`session.grants`、`session.reconcile`、`capability.reconcile` 是 `rpc.rs` 对同名 Runtime SDK 的薄适配，没有独立的该方法 IPC 专项；权限与核对行为由 `shared_read*.rs`、`preset_workspace_data.rs` 经真实 SDK 覆盖，不把它们写成已跑的独立 IPC 场景。

### 0.7 独立前后状态核对

Issue 5 要求独立检查文件/Git/进程前后状态、持久事件、调用与结果，不能只看脚本退出码。已跑的入口做了这些核对：

- 第三/四/五组（`preset-workspace-data.log`）：断言磁盘实际字节、`CoreEvent::CapabilitySettled` 的 outcome、任务终态、无遗留待审批。
- Git（`git-green-final-v2.log`、`runtime-git-green-v1.log`）：断言真实仓库 HEAD/index tree/分支/工作树注册的前后状态。
- 终端（`terminal-tests-delivery-serial-final.log`、`runtime-terminal-tests-final.log`）：断言真实 OS 进程被回收（`assert_gone`）、输出游标、非零退出。
- 子任务（E-CHD `children-drain-test.log`）：断言真实子 Session/Task 身份与终态，并覆盖未确认清理不释放句柄、不冒充完成。

### 0.8 证据日志索引（仓外原路径）

目录：`/Volumes/ORICO/cayw-issue5-evidence-20261021-a/`

| 键 | 文件 | 日志内结果 |
| --- | --- | --- |
| E-PW | `preset-workspace-data.log` | ok. 13 passed; 0 failed; exit 0 |
| E-PW-C | `preset-workspace-data-clippy.log` | Clippy `-D warnings` 干净 |
| E-RV | `runtime-values-delivery-clean-target.log` | ok（3 values + 2 shared-read invalidation） |
| E-RVX | `runtime-values-existing-runtime-final.log` | ok. 6 passed |
| E-VAL | `runtime-values-and-shared-read-final.log` | ok（5 passed，同 E-RV 集合） |
| E-INV | `runtime-shared-read-invalidation-delivery.log` | ok. 2 passed |
| E-CSR | `cells-shared-read-binary.log` | ok. 11 passed |
| E-CV | `cells-values-authorization-final.log` | ok. 7 passed |
| E-CS | `cells-settlement-binary.log` | ok. 15 passed |
| E-CA | `cells-agent-binary.log` | ok. 2 passed |
| E-CF | `cells-forwarding-cells.log` | ok. 1 passed |
| E-CS1 | `cells-existing-cells-store-load-final.log` | ok. 1 passed |
| E-CH | `cells-existing-hardening-store-load-final.log` | ok. 2 passed |
| E-CI | `cells-existing-independent-store-final.log` | ok. 4 passed |
| E-CC | `cells-hardening-catalog-final.log` | ok. 1 passed |
| E-CP | `cells-forwarding-independent.log` | ok. 1 passed |
| E-CL | `cells-forwarding-hardening.log` | ok. 1 passed |
| E-CR | `cells-restart-binary.log` | ok. 2 passed |
| E-CM | `codemode-service-settlement-green.log` | ok（host_wait 2 + service_settlement 5） |
| E-PIPC | `/Volumes/ORICO/issue5-evidence/preset-ipc-rerun-20261021T174629.log` | ok（collaboration 9 + baseline 1 + 认证 IPC 4） |
| E-HWR | `/Volumes/ORICO/issue5-evidence/host-wait-rerun-20261021T175508.log` | ok（Runtime 慢回答/人类等待 3） |
| E-CMP | `completion-green-v2.log` | ok. 4 passed（较早一轮） |
| E-CPG | `completion-pagination-v1.log` | ok. 6 passed（含跳游标、扩大预算、重复 wait 只写一次） |
| E-GH | `git-green-final-v2.log` | ok. 12 passed（host git） |
| E-GR | `runtime-git-green-v1.log` | ok. 1 passed（runtime git） |
| E-GL | `git-host-lib-v1.log` | ok. 5 passed（host 进程快照单元测试） |
| E-TH | `terminal-tests-delivery-serial-final.log` | ok. 14 passed（host terminal） |
| E-TR | `runtime-terminal-tests-final.log` | ok. 4 passed（runtime terminal） |
| E-HW | `host-wait-green-v2.log` | ok. 2 passed（codemode host_wait） |
| E-BR | `bridge-resources-after-contract.log` | ok. 8 passed（bridge resources） |
| E-BA | `bridge-green-all-final2.log` | ok（journal 15 / local_integration 9 / registry 8 / regressions 8 / resources 7） |
| E-BT | `bridge-runtime-existing-tests.log` | ok. 21 passed（runtime 外部能力） |
| E-BTR | `bridge-runtime-resources.log` | ok. 3 passed（runtime 资源） |
| E-IP | `input-green-v2.log` | ok. 7 passed（任务输入/图像） |
| E-CHD | `children-drain-test.log` | ok. 10 passed（本轮最终；较早一轮 `children-test-final.log` 为 9 passed） |
| E-RQ | `requests-all-final.log` | ok（clock_atomicity 1 / failure_fences 5 / host_configuration 1 / lifecycle 16 / receipts 14 等） |
| E-DR3 | `dynamic-requests-green3.log` | ok. 4 passed（动态能力配置失效） |
| E-CFS | `codemode-full-suite-host-wait-20261021-a.log` | ok. 79 passed、0 failed（adversarial 8 / agent_tool 2 / cancellation 7 / execution 19 / host_wait 2 / luau_runtime 30 / regressions 6 / service_settlement 5） |
| E-CCL | `codemode-clippy-all-targets-20261021-a.log` | 日志为 `Checking cayw-codemode ... Finished dev profile`，无 warning/error；主代理说明该轮命令为 Clippy `--all-targets` |
| E-DR4 | `dynamic-requests-final-tests2.log` | 历史一轮 ok（dynamic_requests 5 + shared_read_requests 3），该轮含 `unused_assignments` warning；它不证明后续源码有无 warning，当前整树状态以最终严格 Clippy 日志为准 |
| E-RC2 | `runtime-combination-v2.log` | **不是产品行为证据**：Runtime lib 编译失败（E0061/E0063，`launch_worker` 缺第 7 个 `Arc<AtomicBool>` 参数、`RunContext` 缺 `failed` 字段），未运行任何测试；保留为在途组合构建记录 |
| E-GSS | `git-snapshot-host-suite-final.log` | ok. 147 passed、0 failed（lib 5 / external_scopes 15 / git 12 / git_snapshot 7 / host 41 / process_contracts 4 / read_fence_hardening 10 / repair_regressions 20 / shared_read 13 / terminal 14 / workspace_reads 6） |
| E-GSC | `git-snapshot-clippy-final.log` | 日志为 `Checking cayw-host ... Finished dev profile`，无 warning/error；主代理说明该轮为严格 Clippy |
| E-B15 | `bridge-all15-final2.log` | ok. 15 passed、0 failed；包含修复后的 `interleaved_cells_retain_distinct_execution_parent_and_source_hash_on_one_handle` |
| E-TC | `/Volumes/ORICO/issue5-evidence/toolchain-final-20261021T181810.log` | ok. 5 passed、0 failed（日志未列用例名；源码 `crates/cayw-runtime/tests/toolchain.rs`） |
| E-TCC | `/Volumes/ORICO/issue5-evidence/toolchain-clippy-20261021T181713.log` | Runtime 及其依赖的严格 Clippy 日志，无 warning/error；不是整个 workspace 的证据 |
| E-G150 | `git-bool-host-suite.log` | Host 全包 150 passed、0 failed；包含 git_snapshot 10 项，三项配置布尔值对照 |
| E-GBC | `git-bool-clippy.log` | Host `--all-targets -- -D warnings` 通过 |
| E-RT | `integrate-runtime-suite-v3.log` | Runtime 全包通过，含收紧后的 host_wait 3 项、共享读各组、认证 IPC、进程清理；运行期间其他文件仍有改动，不能作为最终整树证据 |
| E-IN24 | `issue5-independent-rerun-20261021T102230Z.log` | 独立 target：completion 6 + preset_workspace_data 13 + toolchain 5，共 24 passed、0 failed |
| E-VHW | `verify-tool-local-green.log` | runner 9 + luau_measurements 11 通过；HostTimeout 显式 host_wait=1 秒，报告与重验保留两个时限 |
| E-V21 | `verify-tool-lib-green.log`、`verify-tool-sdk-cli-green.log` | VerifyTool lib 21 + 真实 SDK/IPC CLI 4 通过；验证器迁移到统一 `TaskInput`，原页/游标/身份/终态检查保留 |
| E-CMS | `/Volumes/ORICO/issue5-evidence/cells-media-search-20261021T103937Z/cayw-cells-package-20261021T103937Z.log` | 历史一轮：`cells.rs` 21 passed；hardening 12 passed、2 failed。保留原失败；修复后的结果见 E-CH90/E-FT |
| E-CH90 | `/Volumes/ORICO/issue5-evidence/cells-hardening-repair-20261021T105546Z/cayw-cells-full.log` | Cells 全包 90 passed、0 failed、3 ignored；小 item 预算和独立 host_wait 两项已修复；本轮完整严格检查另见 E-FL |
| E-WD128 | `/Volumes/ORICO/cayw-issue5-evidence-20261021-b/workspace-data-final-20261021T110521Z.log` | Workspace/Data 两包 128 passed、0 failed、1 ignored；显式启用 `cayw-workspace/fault-injection`，含 recovery 11 项及两包严格 Clippy |
| E-RM | `remove-partial-green-v1.log`、`remove-partial-green-v1.json` | 递归删除的首次效果前/后错误和取消对照通过；实际部分删除保留 unknown，重开不继续删除；E-WD128/E-FT 再次执行同一测试 |
| E-FF | `final-fmt-v3.log`、`final-fmt-v3.json` | `cargo fmt --all -- --check`，exit 0，运行期间源码清单不变 |
| E-FC | `final-check-v2.log`、`final-check-v2.json` | `cargo check --workspace --locked`，exit 0，运行期间源码清单不变 |
| E-FT | `final-test-v2.log`、`final-test-v2.json` | `cargo test --workspace --locked`，exit 0；221 个顶层目标汇总 1453 passed、0 failed、7 ignored；不把子进程重复结果加到顶层总数；运行期间源码清单不变 |
| E-FL | `final-clippy-v2.log`、`final-clippy-v2.json` | `cargo clippy --workspace --all-targets --locked -- -D warnings`，exit 0，无 Rust warning/error，运行期间源码清单不变 |
| E-FO | `final-offline-v2.log`、`final-offline-v2.json` | `cargo run -p cayw-agent --example offline --locked`，exit 0；真实工具返回 5，五条消息，`running=false` |

**当轮状态说明（dynamic_requests / runtime bridge）**：E-RT 又执行并通过 dynamic_requests 5 项、shared_read_requests 3 项、bridge 15 项及 runtime_bridge 21 项。`interleaved_cells_retain_distinct_execution_parent_and_source_hash_on_one_handle` 的 Provider fixture 等待两个真实 cell 都完成。`dynamic-requests-bridge-regression.log`（14 passed、1 failed）保留为修复前的真实失败。E-RT 执行期间仍有其他文件修改，最终稳定整树以最终 workspace 日志为准。

**最终组合证据**：E-FF/E-FC/E-FT/E-FL/E-FO 全部退出 0，各自运行期间源码清单不变，并共同绑定清单 SHA-256 `be365d6de0f683d8c51531e625b5068c195f21b7978bd73336fdbf1b5699e4d5`、锁文件 SHA-256 `b3329220076fc60e7e240096eccf9e279886e33d218cbf27104c20c2e9871060`。每份 JSON 记录命令、平台、默认 feature、target 和日志 SHA-256；同名 `-source.json` 记录逐文件字节摘要。E-FT 已重新执行下表引用的 Cells、CodeMode、Host、Runtime、Workspace/Data、Bridge、Requests、Device 和 VerifyTool 场景。`runtime-combination-v2` 的在途编译失败保留为历史；不把它当作执行过的行为测试。

**结果范围**：E-FT 的 1453 是 221 个顶层目标的汇总，包含已存在的其他 workspace 包和两项编译失败预期的文档测试，不是 1453 个互不重复的 Issue #5 场景。7 个默认忽略入口中，6 个是由父测试启动的隔离子进程入口，另 1 个是本项范围外的远程二进制示例。Cargo 另提示三个包的 `offline` 示例同名；完整测试仍成功，E-FO 随后用 `-p cayw-agent` 明确选择并运行了该包的示例。Rust 严格 Clippy 无告警，不等于测试命令没有这条 Cargo 提示。

**文档收尾**：上述最终命令结束后只更新本矩阵、实施计划、验收记录和路线图。产品、测试、Cargo 清单及锁文件未因文档收尾变化；收尾后的清单对照另存于验收记录所指的本地证据，不回写旧日志或旧清单。

---

## 1. 组一：Luau 单元与会话值

契约来源：`crates/cayw-cells/src/tool.rs`、`crates/cayw-cells/src/types.rs`、`crates/cayw-cells/src/values.rs`、`crates/cayw-codemode/src/runtime.rs`、`crates/cayw-runtime/src/values.rs`。

| 原语/能力 | 参数 → 返回 | 精确权限与资源范围 | 预算/取消/失败 | 成功与负例入口（日志） | SDK/IPC |
| --- | --- | --- | --- | --- | --- |
| `execute_code` exec | `{action:"exec", script, yield_time_ms, max_output_tokens}` → `CellPage{cell_id,status,vm_alive,cursor,next_cursor,outputs[],has_more,required_budget,outcome_unknown,error}` | `execute_code` 是模型侧唯一的 DurableTool，**本身不建立 exact `ScopedPermission`、不经过 PermissionGate、不产生能力 planned 调用或权限审批**；它有自己的持久 cell 记录，但脚本内容只是受 `script_bytes` 64 KiB 限制的输入。真正的授权发生在脚本内每次 `cayw.call(name,args)`：每个子能力各自按"能力名 + 规范化参数"独立授权。`yield_time_ms` ≤ 30 s；`max_output_tokens` ≥ 1024 且 ≤ 128 KiB | 超脚本/输出/指令/内存预算为已知失败；VM 内某个 host 子调用超时/未知 → 该子调用 unknown 并停止后续真实调用；token 取消在检查点生效 | ✅ 成功：E-PW 全部 13 项（真实 Runtime 经 `execute_code` 执行 `cayw.call`）；E-CA `agent_uses_one_entry_exec_then_wait_with_multiple_internal_host_operations`。负例：E-PW `invalid_arguments_escape_and_tree_bound_fail_before_effect`（schema 合法但 Host 拒绝；各场景保留一个已结算的已知失败记录，没有文件效果） | `session.prompt` / `task.enqueue` 投递的是 `TaskInput`（文本 + 可选图像），不是脚本；配置的 Provider 自己选择并给出 `execute_code` 脚本。`approval.decide` 只放行脚本内某个精确子能力；`session.events` 读事件 |
| `execute_code` wait | `{action:"wait", cell_id, cursor, yield_time_ms, max_output_tokens, continue_execution?}` → 同上 `CellPage` | 同 exec：wait 只观察或续跑已有 cell，**不新增权限、不再走 Gate**；已有子调用的授权在各自 `cayw.call` 时已经确定。`cell_id` 是 exec 返回的全局唯一 ID；游标只读增量，不重放 | 页装不下下一条输出 → `required_budget`，游标不变；`continue_execution=false` 只观察不续跑；重启只恢复持久记录，不恢复 Lua 栈 | ✅ 成功：E-CA 同名测试（一个 exec 后 wait）；E-CS `dropped_exec_and_wait_poll_the_same_capability_through_cleanup`。负例：E-CPG `skipping_an_oversized_first_item_cannot_hide_unread_output`（跳游标不能隐藏未读输出）、`a_larger_page_and_repeated_wait_preserve_output_without_replaying_effects`（扩大预算与重复 wait 不重放已发生效果） | 同 exec：`session.prompt`/`task.enqueue` 只投递 `TaskInput`，Provider 决定脚本；wait 本身没有独立 IPC 方法 |
| `cells.store` | `{key(1..128), value, expected_version(≥0)}` → `{version}` | 精确 `cells.store` + 规范化参数；session 只来自 `CellExecution` 不可变来源，模型参数不能给 `session_id`，多余字段被拒 | `store_bytes` 1 MiB、`store_keys` 256、`json_depth` 32；journal 写失败映射为 **unknown**（冻结 Session，不重放） | ✅ 成功：E-RV `authorized_value_reuse_keeps_cas_versions_across_turns`；E-CS1 `store_load_are_durable_cas_snapshots_and_isolated_by_session`；E-CH（2 项边界）；E-CI（4 项）；E-CC。负例：E-CV `cross_task_value_reuse_requires_backend_authorization`、`value_arguments_reject_extra_fields_and_session_overrides`、`explicit_authorizer_denies_before_the_executor`、`unhandled_backend_defaults_to_deny`；E-RVX `cross_task_value_reuse_without_host_permission_is_not_executed` | 模型侧 `cayw.call`；宿主侧 `Runtime::revoke` / `approval.revoke` 使后续读取不再命中旧授权 |
| `cells.load` | `{key}` → `{version,value}`；键不存在返回 `null` | 精确 `cells.load` + `{key}`；同样不接受 session 覆盖 | 读取前先经 Host 授权；已冻结 Session 返回 `Frozen`；快照超 `store_bytes`/深度为已知失败 | ✅ 成功：同 `cells.store` 各项（E-RV、E-CS1、E-CH、E-CI、E-CV）。负例：E-CV 同上 | 同 `cells.store` |
| `yield_control()` 与 `yield_time_ms` | 无参（VM 服务 `yield`）；`yield_time_ms` 控制一次 exec/wait 的让出时长 → 返回早退但**不取消**被管理的 VM | 属 VM 内部原语，不是 catalog 能力；不产生 Host 调用 | 只有显式 continuation 才唤醒；暂停计算时钟但保留输出/指令/内存预算；host wait 到期把未结算 store 标为 unknown | ✅ 成功：E-CM `explicit_yield_pauses_the_clock_and_is_not_bounded_by_host_wait`；E-HW/E-CFS `calculation_clock_resumes_after_a_host_reply`、`two_host_waits_do_not_spend_the_vm_calculation_budget`。E-FT Cells `controlled_yield_keeps_output_instruction_and_memory_budgets`、`controlled_yield_pauses_active_time_but_keeps_other_budgets` 实际覆盖暂停后仍保留其他预算，`yield_is_owned_and_resumed_only_by_explicit_wait` 覆盖原 cell 归属与显式继续 | 无独立 IPC 方法；由 `execute_code` 的 `yield_time_ms` 体现 |
| `max_output_tokens` 分页预算 | exec/wait 参数 → `CellPage` 的 `cursor`/`next_cursor`/`has_more`/`required_budget` | 序列化 UTF-8 **字节**预算，不是 provider token；最小值 1024 | 页装不下下一条 → `required_budget` 且游标不变；溢出停止后续写入但保留已完成审计 | ✅ 成功：E-CPG `a_larger_page_and_repeated_wait_preserve_output_without_replaying_effects`（扩大预算后继续读取，效果不重放）。负例：E-CPG `skipping_an_oversized_first_item_cannot_hide_unread_output`（首个超大项不能借跳游标绕过未读输出） | 同 exec/wait |
| `text(value)` | 字符串 → `CellOutput::Text{text}` | VM 本地输出原语，无权限 | 受 `item_bytes`/`output_bytes`/`output_items` 限制 | ✅ 成功：E-CM `local_output_and_exit_keep_their_existing_results`。负例：E-FT Cells `capacity_output_and_store_limits_are_enforced_without_cursor_overwrite`、`controlled_yield_keeps_output_instruction_and_memory_budgets` 与 CodeMode `output_limit_stops_emits_without_discarding_successful_audit`，覆盖超限、已有输出保留和后续效果停止 | 输出随 `CellPage.outputs` 返回 |
| `image(mime,data)` | `mime` + 规范 base64 → `CellOutput::Image` | VM 本地输出原语；MIME 只允许 `image/png`/`jpeg`/`webp`/`gif`；必须规范 padded base64 | 解码后 ≤ `media_bytes` 32 KiB；空或非规范 base64 为已知失败；序列化项还受 `item_bytes` 和累计输出预算限制 | ✅ E-CMS `typed_output_mime_base64_and_notify_are_strict`、`media_decoded_byte_boundary_and_notify_length_stop_before_host_effects`：8 字节边界成功，9 字节、空载荷、坏 MIME/编码拒绝，后续 Host 效果为零。只校验 MIME/编码/字节上限，不宣称完整图片解码 | 输出随 `CellPage.outputs`，来源由原 cell 身份和 cursor 绑定；任务图像输入见组八 |
| `audio(mime,data)` | `mime` + 规范 base64 → `CellOutput::Audio` | 同上；MIME 只允许 `audio/wav`/`mpeg`/`ogg`/`webm`/`flac` | 同 image | ✅ E-CMS 上述两项：带 WAV MIME 的 8 字节载荷成功，`audio/mp3` 与坏 Base64 拒绝；不宣称完整音频解码 | 输出随 `CellPage.outputs` |
| `notify(message,completed,total)` | 进度元数据 → `CellOutput::Notify` | VM 本地输出原语；只允许进度元数据，禁止二进制/任意嵌套载荷 | `message` ≤ 512 字节；`completed ≤ total` | ✅ E-CMS 上述两项：512 ASCII 字符成功，513 字符、进度倒置拒绝；失败后不执行 Host 调用 | 输出随 `CellPage.outputs` |
| `exit(code)` | `i32`（省略为 0） → 结束当前 cell | VM 本地原语；不产生 unknown | 结束后不再执行后续真实操作；不是 unknown 效果 | ✅ 成功：E-CF `exit_stops_following_real_operations_and_is_not_an_unknown_effect`；E-CA `cell_exit_ends_only_the_cell_and_agent_produces_its_next_message` | 结果经 `execute_code` 返回；`session.events` 可见 |
| `ALL_TOOLS` | 无参只读数组 → catalog 能力名列表 | 只读；发现不等于授权 | 只读数组不可改写 | ✅ 成功：E-CFS `discovery_is_bound_to_the_host_catalog`（真实 Luau 只看到 Host catalog 中的名字） | 无独立 IPC 方法 |
| `toolDefinition(name)` | 名字 → 该能力定义 | 只读发现；不授权 | 未知名称为已知失败 | ✅ 成功：E-CFS `discovery_returns_only_names_and_description_is_requested_explicitly`（描述按需请求，未知名称有界失败） | 无独立 IPC 方法 |
| `tool_search(query,limit)` | `query`（默认 `''`）、`limit`（默认 16，范围 1..64） → 名称或描述匹配的能力 | VM 本地只读发现；不授权、不启动 Host 调用 | 搜索结果受 Cells `item_bytes`/`json_depth` 限制 | ✅ E-CMS `tool_search_default_and_explicit_limits_truncate_description_matches`：仅描述命中，默认 16、显式 3 与 64 截断正确；0/65/非整数/负数/数字字符串拒绝，后续 Host 调用为零 | 无独立 IPC 方法 |
| `cayw.call(name,args)` | 能力名 + 参数 → 能力结果 | 每次调用独立精确授权；未授权先挂起审批 | 共享 Host `max_calls`/`max_total_bytes` 与 Runtime `max_capability_calls`/`max_capability_bytes` | ✅ 成功：E-PW 全部 13 项；E-BT `all_six_external_operations_use_existing_consumer_through_luau`。负例：E-PW 各负例测试 | 模型侧唯一调用原语 |
| `cayw.emit(value)` / `return` | JSON 值（含 table） → 输出项 | VM 本地输出原语 | 受输出字节/条目预算 | ✅ E-PW 使用 `return`；E-FT Cells `every_emit_is_durable_at_yield_and_overflow_stops_the_next_write`、`raw_emit_item_and_total_byte_limits_stop_following_effects`、`public_output_page_fits_byte_budget_and_preserves_unread_items` 覆盖逐项持久输出、分页保留与超限后零效果 | 输出随 `CellPage.outputs` |
| `cayw.discover` / `cayw.describe` | 发现/描述 → 能力元数据 | 只读发现；不授权 | 元数据受结果字节限制 | ✅ E-FT CodeMode `discovery_is_bound_to_the_host_catalog`、`discovery_returns_only_names_and_description_is_requested_explicitly`、`unknown_names_have_bounded_accounted_errors_for_describe_and_call`；Cells `small_item_budget_initializes_and_still_bounds_introspection_results` 另确认目录可初始化但实际自省返回仍受单项预算 | 无独立 IPC 方法 |
| 任务完成核对（Runtime 内部，不是 catalog 能力） | 最终回答前核对：VM 是否仍在运行、连续输出是否消费完、非零退出、未结算调用 | Runtime 内部判定 | 未消费输出或非零退出不能报完成；不能自动重跑或丢弃输出 | ✅ 成功：E-CPG 6 项：`final_response_cannot_complete_a_yielded_vm`、`final_response_cannot_discard_unread_output`、`nonzero_exit_cannot_be_reported_as_a_completed_task`、`explicitly_waited_and_consumed_zero_exit_can_complete`、`skipping_an_oversized_first_item_cannot_hide_unread_output`、`a_larger_page_and_repeated_wait_preserve_output_without_replaying_effects`（E-CMP 为较早 4 项一轮） | 任务终态经 `session.events` / `session.tasks` 读取 |

---

## 2. 组二：共享只读合并、结果复用、版本快照与失效

契约来源：`crates/cayw-runtime/src/shared_read.rs`、`crates/cayw-runtime/src/cells.rs:501`、`crates/cayw-runtime/src/lib.rs:258`、`crates/cayw-cells/src/cache.rs`、`crates/cayw-host/src/read_fence.rs`。

**范围说明（不是"不适用"）**：只有显式登记为可共享的 `fs.read` 才走合并/复用；其他读取仍走普通 Host 调用。启用条件是 `HostConfig.frozen_reads=true` 且 Runtime 登记 `ReadPolicy{capability:"fs.read", reuse:true, coalesce:true}`。不是 catalog 里的独立能力，所以下面按语义条目列，而不是按能力名。

| 语义条目 | 契约 | 预算/取消/失败 | 成功与负例入口（日志） | SDK/IPC |
| --- | --- | --- | --- | --- |
| 共享登记与合并 | 只有 `fs.read` 走 `shared_read::call`；先 `admit_frozen_read` 独立校验/计费/授权，再取共享代际租约 | 每个原始调用各自计费；共享 Host 调用与响应字节预算 | ✅ E-CSR `bridge_acknowledges_each_original_backend_only_after_its_terminal_and_dependencies_commit`、`publication_waits_for_original_acknowledgment_and_cancelled_waiter_leaves_leader`、`noncoalescing_policy_uses_uncached_capture_without_replacing_original_leader`、`full_cache_uses_original_uncached_capture_and_never_replaces_a_live_owner` | 无独立 IPC 方法；模型侧 `cayw.call('fs.read', ...)` |
| 结果复用（warm hit） | 同一 `ReadStamp` 下第二个原始 cell 复用已发布字节，但仍各自付费、各自保留依赖快照 | 每个原始调用都花自己的 Runtime/Host 调用预算 | ✅ E-RT `shared_read::two_original_cells_pay_two_grants_and_keep_exact_dependencies_for_one_capture`、`shared_read_authority::warm_read_reuse_spends_each_original_host_and_runtime_call_budget`、`ready_read_charges_each_original_runtime_response_bytes`；E-CSR 的拒绝发布对照保留 | 同左 |
| 依赖版本快照 | 快照含 `workspace`、`incarnation`、`generation`、`semantics`；随候选值一起持久化 | 证据超 `MAX_METADATA_BYTES` 为持久化失败 | ✅ E-CSR `oversized_and_deep_capture_never_reaches_waiters_or_a_ready_entry` | `session.events` 读 `runtime_shared_read` 宿主事件 |
| 写入后失效 | 另一个 Runtime 的相同写入改变 generation，旧命中不再交付 | 旧值不交付；重新真实捕获 | ✅ E-INV `real_runtime_read_write_read_captures_the_new_bytes`（E-RV 同项） | 同左 |
| 撤销后失效 | `Runtime::revoke` 取消任务并使该 Session 共享读缓存失效，下一次读不是旧值 | 撤销后旧租约不再放行 | ✅ E-INV `runtime_revoke_cancels_the_task_and_the_next_read_is_not_stale`（E-RV 同项） | SDK `Runtime::revoke`；IPC `approval.revoke` |
| 中断执行对账 | 原始读 unknown 时保留 unknown 父记录，不重放、不猜测；核对 Cells/Core 终态后修复 | unknown latch；恢复需显式证据 | ✅ E-CSR `cancelled_leader_capture_wakes_waiters_without_promotion`、`dropped_capture_wakes_waiters_without_detached_capture`、`dropped_candidate_wakes_followers_and_rejects_future_hits_without_recapture`、`known_candidate_rejection_releases_only_original_publication`、`cancelled_ready_hit_and_invalidated_generation_never_deliver_old_value` | SDK `Runtime::recover` / `reconcile_capability`；IPC `session.recover` / `capability.reconcile` |
| 栅栏等待与原始授权复核 | 等待 Host 栅栏期间持续复核原始精确授权与请求回执；替换授权不能顶替 | 过期/撤销的原始授权在等待中失败，不交付字节 | ✅ E-RT `shared_read_authority::original_grant_is_rechecked_during_fence_wait_and_replacement_cannot_substitute`、`shared_read::original_once_grant_cannot_expire_or_be_replaced_during_fence_wait`、`original_request_receipt_expires_or_is_revoked_while_read_waits_for_fence` | 同左 |
| 失败恢复不重放 | Cells 终态失败或 Core 记录失败时，恢复精确依赖与已知字节，不重读、不重放 | 恢复失败保持 unknown | ✅ E-RT `shared_read_recovery::failed_cells_terminal_recovers_exact_dependencies_and_known_core_bytes_without_replay`、`failed_evidence_or_core_terminal_never_replays_or_guesses_the_capture`；同日志 `shared_read_requests` 3 项通过，保留原请求收据与根目录身份 | 同左 |

---

## 3. 组三：文件与变更

契约来源：`crates/cayw-host/src/catalog.rs`、`crates/cayw-workspace/src/catalog.rs`、`crates/cayw-workspace/README.md`、`crates/cayw-host/src/filesystem.rs`。返回结构以各 README 表为准。

| 能力 | 参数 → 返回 | 精确权限与资源范围 | 预算/取消/失败 | 成功与负例入口（日志） | SDK/IPC |
| --- | --- | --- | --- | --- | --- |
| `fs.read` | `{path}` → `{path,content,bytes,sha256}` | 精确 `fs.read`+`{path}`；工作区相对、无 `..`、不跟随链接；不寻址 `.git`/`.cayw-*` | 单文件 ≤ `max_file_bytes`；结果 ≤ `max_result_bytes`；`frozen_reads=true` 时走 §2 共享路径 | ✅ 成功：E-RVX `prompt_executes_real_luau_host_write_read_and_reopens_history`（真实 Runtime 读写）；E-CSR 11 项共享读。负例：E-PW `invalid_arguments_escape_and_tree_bound_fail_before_effect`（`../escape.txt` 越界） | `cayw.call`；`session.prompt`/`task.enqueue` 触发 |
| `fs.list` | `{path}` → `{path,entries:[{name,kind,size}]}` | 精确 `fs.list`+`{path}`；目录直接项、不跟随链接 | 条目 ≤ `max_list_entries` | ✅ E-G150 `host::real_file_round_trip_requires_compare_and_swap_and_exact_patch`、`listing_and_denied_builtins_never_create_workspace_files`、`result_and_directory_budgets_prevent_unbounded_data` | 同左 |
| `fs.stat` | `{path}` → `{path,kind,size,readonly}` | 精确 `fs.stat`+`{path}`；最终链接只报告类型，不跟随 | 单条元数据 | ✅ E-G150 `host::real_file_round_trip_requires_compare_and_swap_and_exact_patch`、`paths_symlinks_and_byte_limits_are_enforced` | 同左 |
| `fs.write` | `{path,content,overwrite?,expected_sha256?}` → `{bytes,sha256}`（新建/替换后原子可见） | 精确 `fs.write`+完整参数（含 content 与 hash）；替换必须 `overwrite=true` 且给 `expected_sha256`，二者缺一为已知失败 | content ≤ `max_file_bytes`；调用前超 `max_calls`/参数或总字节预算为已知失败。**提交成功后**结果无法在 `max_result_bytes`/`max_total_bytes` 内表示或审计结算失败 → `outcome_unknown`，不能降级为普通 `Err` | ✅ 成功：E-PW `host_write_patch_tree_and_committed_batch_are_real_runtime_paths` 等 13 项；E-RVX 同项。负例：E-PW `write_byte_limit_and_host_call_budget_are_known_failures`（调用前）、`denied_and_cancelled_write_never_touch_the_workspace`、`invalid_arguments_escape_and_tree_bound_fail_before_effect`；E-G150 `host::result_and_directory_budgets_prevent_unbounded_data` 另断言写入后结果溢出返回 unknown，保留已写文件 | 同左 |
| `fs.patch` | `{path,old,new,expected_sha256}` → 原子替换后结果 | 精确 `fs.patch`+完整参数；`old` 必须唯一精确匹配（含重叠情形，`aa` 在 `aaa` 中被拒） | `old`/`new` ≤ `max_file_bytes`；hash 不符或调用前预算拒绝为已知失败。替换已提交但结果/结算超预算 → `outcome_unknown` | ✅ 成功：E-PW `host_write_patch_tree_and_committed_batch_are_real_runtime_paths`。负例：E-FT Host `patch_rejects_overlapping_matches_and_changed_or_missing_targets` 实际调用 `fs.patch`，拒绝重叠匹配、缺失目标和不存在的 old 文本，断言不创建缺失文件、原字节不变、Host 未冻结；`real_file_round_trip_requires_compare_and_swap_and_exact_patch` 拒绝过期 hash。E-WD128/E-FT Workspace `patch_preconditions_reject_missing_files_and_absent_old_text` 则覆盖批次 patch 的前置检查，两层分别记录 | 同左 |
| `fs.mkdir` | `{path,expected_absent:true}` → `{path,created:true}` | 精确 `fs.mkdir`+参数；只建一层，父目录必须已存在 | 路径长度 ≤ 4096；调用前预算/权限拒绝为已知失败。目录已创建但结果/审计结算失败 → `outcome_unknown` | ✅ E-G150 `host::workspace_capabilities_do_not_fall_through_the_old_filesystem_dispatch`；E-WD128/E-FT Workspace `mkdir_requires_an_existing_parent_and_an_absent_target` 覆盖缺失父目录与已存在目标；E-FT Host `builtin_result_or_audit_loss_preserves_unknown_and_blocks_recovery_bypass` 覆盖目录已创建后的结果/审计丢失 | 同左 |
| `fs.move` | `{source,destination,expected_sha256,expected_destination_absent:true}` → `{source,destination,sha256,moved:true}` | 精确 `fs.move`+完整参数；仅普通文件；同文件系统；先建硬链接再删源，存在两路径窗口 | 失败/取消在链接之后为 **unknown**，可能留下两个路径 | ✅ E-G150 `host::workspace_capabilities_do_not_fall_through_the_old_filesystem_dispatch`（真实移动与后续树读取） | 同左 |
| `fs.remove` | `{path,expected_sha256?,recursive?,expected_root_sha256?,max_entries?}` → `{path,removed_entries,root_sha256?}` | 精确 `fs.remove`+完整参数；递归删除需显式 `recursive=true` + 根 hash + 条目上限；拒绝受保护条目；不跟随链接 | 首次删除后中断为 **unknown**；顺序删除可部分完成 | ✅ E-G150 `host::workspace_capabilities_do_not_fall_through_the_old_filesystem_dispatch`（文件与空目录）；E-WD128/E-FT Workspace `tree_move_exact_delete_and_recursive_hash` 覆盖递归成功；`recursive_remove_failure_and_cancellation_keep_partial_effects_unknown` 覆盖首次效果前/后错误与取消及重开不重放，不宣称删除可回滚 | 同左 |
| `fs.tree` | `{path,max_entries}` → `{path,root_sha256,entries:[{path,kind,bytes,sha256,mode}],protected_entries_omitted:true}` | 精确 `fs.tree`+参数；根计入 `max_entries`；受保护条目省略，链接/特殊条目拒绝 | `max_entries` 1..100000（默认受 `max_tree_entries` 约束） | ✅ 成功：E-PW `host_write_patch_tree_and_committed_batch_are_real_runtime_paths`。负例：E-PW `invalid_arguments_escape_and_tree_bound_fail_before_effect`（`max_entries` 越界） | 同左 |
| `fs.prepare_change_batch` | `{operations[1..256]}`，每项 write/patch/remove → `{batch_id,batch_digest,status:"prepared",user_files_changed:false,operations:[...]}` | 精确 `fs.prepare_change_batch`+完整 operations；不写用户路径；候选语法校验（JSON/TOML 等，未知语言拒绝） | `max_batch_bytes`、`max_batch_operations`、`max_batches`；prepare 不产生用户文件效果 | ✅ 成功：E-PW `host_write_patch_tree_and_committed_batch_are_real_runtime_paths`、`interrupted_change_batch_recovers_through_a_fresh_runtime`、`third_party_change_blocks_batch_recovery_through_host` | 同左 |
| `fs.submit_change_batch` | `{batch_id,expected_batch_digest}` → 批次状态报告 | 精确 `fs.submit_change_batch`+`{batch_id,digest}`；提交前在共享锁下重查每个路径 | 安装中断且已改文件 → `outcome_unknown`；未改文件 → `aborted` 已知失败；pending 批次挡住后续写入 | ✅ 成功：E-PW 同上 3 项。负例：E-PW `interrupted_change_batch_recovers_through_a_fresh_runtime`（阻断第二项安装，产出 unknown） | 同左 |
| `fs.batch_status` | `{batch_id?}` → 单批次报告或 `{batches:[...]}` | 精确 `fs.batch_status`+参数；只读观察，不加锁 | 读盘证据含 old/new/current/matches/diagnostic；pending 批次只读仍可用 | ✅ 成功：E-PW 3 项（提交、恢复、第三方阻断）；E-WD128/E-FT Workspace `batch_status_without_an_id_lists_every_published_journal` 核对不传 ID 时完整返回两个已发布批次及各自状态 | 同左 |
| `fs.recover_change_batch` | `{batch_id,expected_batch_digest,action:"commit"/"rollback"}` → 批次状态报告 | 精确 `fs.recover_change_batch`+完整参数；恢复前检查每个 blob 和路径；第三方改动阻断所有恢复写入 | 中断可再中断并可续；不静默应用 prepared/aborted；恢复写入前先满足栅栏 owner 恢复 | ✅ 成功：E-PW `interrupted_change_batch_recovers_through_a_fresh_runtime`（经 `Runtime::recover`+`reconcile_capability` 后 rollback 成功）。负例：E-PW `third_party_change_blocks_batch_recovery_through_host`（返回 `OutcomeUnknown`，含"third-party change blocks recovery"，文件不变） | SDK `recover`/`reconcile_capability`；IPC `session.recover`/`capability.reconcile` |

---

## 4. 组四：检索与结构定位

契约来源：`crates/cayw-workspace/src/catalog.rs`、`crates/cayw-workspace/README.md`（搜索/大纲语义）、`crates/cayw-workspace/src/search.rs`、`crates/cayw-workspace/src/code.rs`。

| 能力 | 参数 → 返回 | 精确权限与资源范围 | 预算/取消/失败 | 成功与负例入口（日志） | SDK/IPC |
| --- | --- | --- | --- | --- | --- |
| `fs.grep` | `{path,pattern,literal?,multiline?,dot_matches_new_line?,case_insensitive?,respect_gitignore?,max_matches?,context_lines?}` → `{path,matches,skipped,scan_complete,truncated,protected_entries_omitted,respect_gitignore,scan}` | 精确 `fs.grep`+完整参数；完整 UTF-8 文件；二进制/非 UTF-8 显式跳过；不跟随链接；受保护条目省略 | pattern ≤ 16384 字节、编译 ≤ 1 MiB、DFA 缓存 256 KiB、嵌套 64、后缀工作量 256 MiB；`max_matches` 默认 100、`context_lines` 0..64；扫描边界失败报错而非暗示完整 | ✅ 成功：E-PW `grep_glob_outline_and_diff_hunks_return_host_results`。负例：E-PW `search_and_code_failures_are_known_before_any_effect`（非法正则）、`search_permission_denial_and_call_budget_are_enforced`（拒权、调用预算） | `cayw.call`；`session.prompt`/`task.enqueue` |
| `fs.glob` | `{path,pattern,max_results?,respect_gitignore?}` → `{path,pattern,paths,skipped,scan_complete,truncated,protected_entries_omitted,respect_gitignore,scan}` | 精确 `fs.glob`+完整参数；支持 `**`/`*`/`?`/字符类/交替；默认读嵌套 `.gitignore`（无环境配置）；不跟随链接 | `max_results` 默认 1000；ignore 输入 ≤ 64 KiB、1024 行、4096 字节/行；越界路径失败 | ✅ 成功：E-PW `grep_glob_outline_and_diff_hunks_return_host_results`。负例：E-PW `search_and_code_failures_are_known_before_any_effect`（`../outside`）、`search_permission_denial_and_call_budget_are_enforced` | 同左 |
| `code.outline` | `{path,language:"rust",max_symbols?}` → `{path,language,sha256,symbols,total_symbols,truncated,syntax_only:true}` | 精确 `code.outline`+完整参数；只解析单个 Rust 文件；不展开宏、不读外部模块、不解析名字 | `max_symbols` 默认 1000；复杂但合法的 Rust 可能被资源上限拒绝；解析后整体返回 | ✅ 成功：E-PW `grep_glob_outline_and_diff_hunks_return_host_results`。负例：E-PW `search_and_code_failures_are_known_before_any_effect`（坏 Rust 语法） | 同左 |
| `code.diff_hunks` | `{diff,max_hunks?}` → `{sha256,hunks,total_hunks,truncated,validated_complete:true}` | 精确 `code.diff_hunks`+完整参数；只读传入 diff，不访问 Git 或文件系统 | `max_hunks` 默认 1000；即使返回被截断也校验整个 diff；非法计数/顺序/路径失败 | ✅ 成功：E-PW `grep_glob_outline_and_diff_hunks_return_host_results`。负例：E-WD128/E-FT Workspace `diff_hunks_rejects_malformed_counts_order_paths_and_trailing_body` 拒绝坏计数、顺序、路径和尾随正文；`diff_hunks_enforces_line_and_output_bounds_and_pre_cancel` 覆盖行数、输出预算与预取消 | 同左 |

---

## 5. 组五：数据计算

契约来源：`crates/cayw-data/src/catalog.rs`、`crates/cayw-data/README.md`（格式边界与 Limits）、`crates/cayw-workspace/src/archive.rs`。所有数据能力是纯计算：不读文件、不起进程、不访问网络。

| 能力 | 参数 → 返回 | 精确权限与资源范围 | 预算/取消/失败 | 成功与负例入口（日志） | SDK/IPC |
| --- | --- | --- | --- | --- | --- |
| `data.parse` | `{format:json/toml/yaml/xml/csv,text}` → `{value}` | 精确 `data.parse`+完整参数；输入显式在参数里 | 受 Data `input_bytes`/`output_bytes`/`max_depth`/`max_nodes`/`max_rows`；JSON 拒绝重复键/溢出整数，YAML 拒绝锚点别名，XML 拒绝 DTD/实体，CSV 全字符串 | ✅ 成功：E-PW `data_formats_query_transform_diff_hash_and_codecs_round_trip`。负例：E-PW `data_syntax_and_depth_failures_are_known`（坏 JSON、超深度）、`data_permission_denial_and_call_budget_are_enforced` | `cayw.call`；`session.prompt`/`task.enqueue` |
| `data.serialize` | `{format,value}` → `{text}` | 精确 `data.serialize`+完整参数 | 输出为紧凑确定形式；受输出字节限制 | ✅ 成功：E-PW `data_formats_query_transform_diff_hash_and_codecs_round_trip`。负例：E-WD128/E-FT Data `toml_round_trip_and_unsupported_types_are_explicit` 与 `shared_budgets_and_pre_cancel_cover_serialize_query_and_transform`，覆盖不可表示值、输出预算和预取消 | 同左 |
| `data.query` | `{value,pointer}` → `{value}` | 精确 `data.query`+完整参数；RFC 6901 `~0`/`~1`；数组下标拒绝 `00`/`-`/负数/不存在 | 受节点/行/深度/输出字节 | ✅ 成功：E-PW `data_formats_query_transform_diff_hash_and_codecs_round_trip`。负例：E-WD128/E-FT Data `pointer_escaping_and_explicit_selection_do_not_coerce_types` 与 `shared_budgets_and_pre_cancel_cover_serialize_query_and_transform`，覆盖无效指针/类型及深度、节点、预取消 | 同左 |
| `data.transform` | `{value,pointer?,operation:"select_fields",fields}` 或 `{...,"select_rows",indices}` → `{value}` | 精确 `data.transform`+完整参数；字段必须存在；行下标从 0 开始、按请求顺序、禁止重复 | 受节点/行/深度/输出字节 | ✅ 成功：E-PW `data_formats_query_transform_diff_hash_and_codecs_round_trip`。负例：E-WD128/E-FT Data `integer_tokens_and_unique_selections_are_checked_after_schema` 与 `shared_budgets_and_pre_cancel_cover_serialize_query_and_transform`，覆盖整数/重复选择、深度、节点和预取消 | 同左 |
| `text.diff` | `{before,after,context?}` → `{diff}` | 精确 `text.diff`+完整参数；`context` 0..100（默认 3） | 受 `max_diff_work`（矩阵单元+比较字节）；相同内容返回空 diff；保留 CRLF 与无末行换行标记 | ✅ 成功：E-PW `data_formats_query_transform_diff_hash_and_codecs_round_trip`。负例：E-WD128/E-FT Data `identical_diff_still_validates_integer_context_and_row_limits`、`limits_reject_input_output_depth_nodes_rows_and_diff_work`、`a_running_diff_yields_to_executor_cancellation`，覆盖整数、行数、工作量与运行中取消 | 同左 |
| `hash.digest` | `{algorithm:"sha256"/"blake3",input:{kind:"utf8"/"base64"/"bytes",value}}` → `{hex}` | 精确 `hash.digest`+完整参数；输入必须显式选表示；base64 规范 padded；hex 小写 | 32 字节小写 hex；无任意长度输出 | ✅ 成功：E-PW `data_formats_query_transform_diff_hash_and_codecs_round_trip`（sha256/blake3 对 "abc"）。负例：E-WD128/E-FT Data `hash_rejects_unknown_algorithms_and_input_kinds` 实际拒绝 md5、未知输入种类和多余字段；`digests_match_published_vectors` 则为正确摘要对照，不冒充负例 | 同左 |
| `codec.encode` | `{encoding:"base64"/"hex",input:{kind,value}}` → `{text}` 或 `{bytes}` | 精确 `codec.encode`+完整参数 | 受输入/输出字节与节点预算 | ✅ 成功：E-PW `data_formats_query_transform_diff_hash_and_codecs_round_trip`（base64 字节、hex 字符串）。负例：E-WD128/E-FT Data `base64_chunk_boundaries_preserve_bytes_and_reject_early_padding`、`binary_input_arrays_use_the_same_node_budget_as_json_data`、`binary_arrays_obey_depth_and_row_limits`，覆盖编码边界与字节数组预算 | 同左 |
| `codec.decode` | `{encoding,text,output:"bytes"/"utf8"}` → `{bytes}` 或 `{text}` | 精确 `codec.decode`+完整参数；非法 UTF-8 返回 `encoding` 错误，绝不用替换字符 | 同 encode | ✅ 成功：E-PW `data_formats_query_transform_diff_hash_and_codecs_round_trip`（base64→utf8、hex→bytes）。负例：E-WD128/E-FT Data `codecs_distinguish_utf8_bytes_and_canonical_base64`、`base64_chunk_boundaries_preserve_bytes_and_reject_early_padding`，覆盖非法 UTF-8、非规范 Base64 和提前 padding | 同左 |
| `archive.list` | `{path,format:"zip"/"tar"}` → `{path,archive_sha256,entries:[{path,kind,bytes,compressed_bytes}]}` | 精确 `archive.list`+完整参数；只读工作区内 ZIP/未压缩 TAR；不解压、不写文件 | `max_archive_bytes`、`max_archive_entries`、`max_archive_expanded_bytes`、`max_compression_ratio`；路径/类型/重复/祖先冲突全部校验 | ✅ 成功：E-PW `archive_list_and_read_use_the_workspace_host_path`。负例：E-PW `archive_list_and_read_use_the_workspace_host_path`（entry 穿越 `../escape` 被拒） | 同左 |
| `archive.read` | `{path,format,entry}` → `{path,archive_sha256,entry,data:{encoding:"utf8"/"base64",content,bytes,sha256}}` | 精确 `archive.read`+完整参数；只解压选中的 ZIP 条目并校验实际输出/CRC；TAR 逐条检测截断；不解压到磁盘 | 同 list；二进制显式 base64 | ✅ 成功/负例：同 `archive.list` 的 E-PW 测试 | 同左 |

---

## 6. 组六：Git

契约来源：`crates/cayw-host/src/git.rs`、`crates/cayw-host/src/process.rs`。读命令只观察有界私有快照；写命令在真实仓库执行，但经过更严格结构检查。没有 push、没有网络协议。

| 能力 | 参数 → 返回 | 精确权限与资源范围 | 预算/取消/失败 | 成功与负例入口（日志） | SDK/IPC |
| --- | --- | --- | --- | --- | --- |
| `git.status` | `{}` → 进程信封 `{success,exit_code,stdout,stderr,termination,output_complete}` | 精确 `git.status`+`{}`；只读私有快照，禁用锁与 hooks | 进程输出 ≤ `max_process_output_bytes`；deadline 由 `call_timeout` 与进程超时取小 | ✅ 成功：E-GH `git_catalog_is_policy_gated_and_calls_use_permission_and_audit`。负例：E-GH 权限/目录策略测试 | `cayw.call` |
| `git.diff` | `{path?,staged?}` → 同上进程信封 | 精确 `git.diff`+完整参数；禁用外部 diff 与文本转换 | 同 status；仓库快照受 `max_file_bytes`/`max_list_entries` 共享预算 | ✅ 成功：E-GH 同名测试 | 同左 |
| `git.log` | `{limit(1..100)}` → 同上进程信封 | 精确 `git.log`+`{limit}`；无 decorations、无签名命令 | 同 status | ✅ 成功：E-GH 同名测试 | 同左 |
| `git.show` | `{commit,path?}` → 同上进程信封 | 精确 `git.show`+完整参数；`commit` 必须是完整 40/64 位十六进制 | 同 status | ✅ 成功：E-GH 同名测试 | 同左 |
| `git.blame` | `{path,commit?}` → 同上进程信封 | 精确 `git.blame`+完整参数；只从私有快照读；非完整 commit ID 在授权前拒绝 | 同 status | ✅ 成功：E-GH `blame_reads_a_snapshot_and_rejects_non_full_commit_ids_before_permission` | 同左 |
| `git.stage` | `{paths[1..256,唯一],expected_head,expected_index_tree}` → `{head,index_tree,paths}` | 精确 `git.stage`+完整参数；无隐式 pathspec 展开；Host 固定程序与身份 | 路径字节和 ≤ `max_argument_bytes`；HEAD/index 不符为已知失败；改动后失败为 unknown | ✅ 成功：E-GH `stage_and_commit_require_exact_head_index_tree_and_record_host_identity`。负例：E-GH `stage_rejects_unsafe_paths_and_directories_without_changing_the_index`、`writes_refuse_a_symlinked_or_external_git_directory`、`writes_refuse_alternates_and_dangerous_local_config` | 同左 |
| `git.commit` | `{message,expected_head,expected_index_tree}` → `{commit,parent,tree}` | 精确 `git.commit`+完整参数；作者/提交者由 Host 提供；hooks 与签名禁用 | `message` ≤ 64 KiB 且非空、无 NUL；提交树与索引树不符为 unknown | ✅ 成功：E-GH `stage_and_commit_require_exact_head_index_tree_and_record_host_identity`。负例：E-GH `commit_disables_hooks_and_signing_even_if_repository_config_asks_for_them` | 同左 |
| `git.branch` | `{action:"create"/"delete",name,commit,expected_head}` → `{action,name,commit}` | 精确 `git.branch`+完整参数；受控本地分支名；校验 HEAD | 已存在/不存在或 HEAD 不符为已知失败；分支结果无法确认时为 unknown | ✅ 成功：E-GH `branch_writes_check_head_and_controlled_names_and_safe_delete`。负例：同项 | 同左 |
| `git.worktree` | `{action:"create"/"remove",name,branch?,commit?,expected_head}` → `{action,name,path,branch?,commit?}` | 精确 `git.worktree`+完整参数；只允许根下固定 `.cayw-worktrees`；无 force、无外部路径 | remove 拒绝外部 gitdir 与脏工作树；注册/路径状态不确定为 unknown | ✅ 成功：E-GH `worktree_operations_stay_under_the_fixed_root_and_verify_registration`。负例：E-GH `worktree_remove_refuses_an_external_gitdir_pointer`、`worktree_remove_never_forces_a_dirty_linked_worktree` | 同左 |
| Git 私有快照与配置隔离（读路径，不是独立 catalog 能力） | 不新增模型参数；所有 Git 读能力共用同一有界快照 | 快照只从持有的 nofollow 目录复制 HEAD/index/sharedindex/refs/packed-refs/shallow/`info/exclude`/objects/工作树；保留支持的被动配置。显式空 boolean 为 false，无等号的隐式键为 true；`filemode=false` 与 `autocrlf=true` 的干净仓库结果与真实 Git 一致。拒绝 SHA-256 仓库、sparse checkout、被跟踪的 `.cayw-*` 路径、对象/工作树链接、alternates 和共享预算溢出；local filter/include/fsmonitor/hook 零执行 | 快照受 `max_file_bytes`/`max_list_entries` 共享预算；清理未确认为 unknown | ✅ E-G150 `git_snapshot.rs` 10 项：原 7 项配置/布局/不执行 helper 对照，加 `explicit_empty_filemode_is_false_and_matches_real_git`、`explicit_empty_sparse_checkout_is_false_and_allowed`、`implicit_sparse_checkout_key_is_true_and_rejected`；同日志另有 `git_passive_metadata_retains_local_excludes_and_split_indexes`、`git_historical_and_deleted_parent_paths_work_without_following_links`、`git_snapshot_refuses_worktree_and_object_links_and_shared_budget_overflow`、`git_fixed_commands_use_real_device_without_running_repo_helpers` | 无独立 SDK/IPC；各读能力经 `cayw.call` |
| Git 运行时组合路径 | 模型脚本内一次调用多个 Git 能力 → Core 调用记录 + 真实仓库前后状态 | 每次调用独立精确授权 | 中断的设备写入保持 unknown，不重试 | ✅ 成功：E-GR `runtime_git_capabilities_record_core_calls_and_change_a_synthetic_repo`；E-GL 5 项进程快照单元测试（E-GH `interrupted_device_write_remains_unknown_and_is_not_retried`） | `session.events` 读 Core 能力记录 |

---

## 7. 组七：工具链与进程

契约来源：`crates/cayw-host/src/catalog.rs`、`crates/cayw-host/src/process.rs`、`crates/cayw-host/src/terminal.rs`、`crates/cayw-device`（进程监督与资源调度）。Runtime 的五类 profile 成功测试实际运行 `/usr/bin/printenv` 和宿主固定环境，核对五个能力的选择、参数和完整分派；它不声称在模型任务里分别完成了 Cargo 构建、测试、检查、整理格式和静态检查。项目自身的这些命令另由最终工作区验收执行。

| 能力 | 参数 → 返回 | 精确权限与资源范围 | 预算/取消/失败 | 成功与负例入口（日志） | SDK/IPC |
| --- | --- | --- | --- | --- | --- |
| `build.run` | `{profile}` → `{success,exit_code,stdout,stderr,termination,output_complete}` | 仅在配置了 build profile 时存在；程序/参数/环境/cwd 由应用固定；模型无 argv、无 shell | profile `timeout`、`max_output_bytes` 与 `call_timeout` 取小；非零退出是已确认终态，不是 unknown | ✅ E-RT/E-IN24 `toolchain::five_configured_profiles_run_through_the_real_runtime`；`agent_argv_env_or_program_and_wrong_profile_kinds_are_known_refusals`、`a_denied_profile_permission_starts_no_process_and_leaves_no_effect`、`a_nonzero_profile_exit_is_a_confirmed_result_not_an_unknown_effect`、`cancelling_a_running_profile_settles_unknown_and_reaps_the_process` | `cayw.call` |
| `test.run` | `{profile,filter?}` → 同上 | 同 build；`filter` 仅在该 profile `allow_filter=true` 时接受，且必须是单个受限位置参数 | filter ≤ 256 字节；不允许时已知失败 | ✅ E-RT/E-IN24 上述 5 项；Host 合法/非法 filter 由 E-G150 `test_filter_is_one_literal_argument_and_nonzero_exit_is_confirmed` 覆盖 | 同左 |
| `check.run` | `{profile}` → 同上 | 同 build | 同 build | ✅ E-RT/E-IN24 上述 5 项；E-G150 `check_lint_and_format_profiles_use_fixed_commands_and_reject_agent_argv` | 同左 |
| `lint.run` | `{profile}` → 同上 | 同 build | 同 build | ✅ E-RT/E-IN24 上述 5 项；E-G150 同一 Host profile 测试 | 同左 |
| `format.run` | `{profile}` → 同上 | 同 build | 同 build | ✅ E-RT/E-IN24 上述 5 项；E-G150 同一 Host profile 测试 | 同左 |
| `exec_command` | `{profile}` → `{session_id,profile,state:"running",output_cursor:0,input_bytes:0,remaining_ms}` | 仅在 `terminal_handoffs` 显式启用时存在；Host 固定程序/参数/环境/cwd/I/O（pipe 或 macOS PTY）；模型只给 profile 名 | `max_sessions`、`max_sessions_total`、profile `timeout`、`max_output_bytes`；启动中断为 unknown | ✅ 成功：E-TH `exec_selects_only_configured_profile_and_exact_permission`、`pty_session_reports_controlling_terminal_and_normal_exit`；E-TR `runtime_terminal_success_reads_the_actual_host_returned_session`。负例：E-TR `runtime_terminal_permission_denial_never_starts_the_configured_program`；E-TH `session_and_input_budgets_do_not_reset_after_wait_or_close` | `cayw.call`；`session.events` |
| `write_stdin` | `{session_id,action:"write"/"read"/"wait"/"close"/"cancel",request_id,data?,cursor?,expected_cursor?,max_bytes?,wait_ms?}` → 按 action 返回（write：`{accepted_bytes,input_bytes,output_cursor,status}`；read：`{cursor,next_cursor,bytes,text,data_bytes,eof,truncated,status}`；wait/close/cancel：`{status}`） | 精确 `write_stdin`+完整参数；只能操作本任务 Host 返回的 session_id；`request_id` 幂等；write 发送原始 UTF-8 | `max_input_bytes_per_write` 64 KiB、`max_read_bytes` 64 KiB、`max_wait` 30 s；`close`/`cancel` 停止并 join，不声称业务回滚；清理未确认为 unknown | ✅ 成功：E-TH `write_is_idempotent_and_read_pages_raw_utf8_bytes`、`all_terminal_output_read_allows_completion`、`wait_does_not_reset_the_session_deadline`、`another_task_cannot_address_this_sessions_handle`。负例：E-TH `close_and_cancel_never_claim_a_known_rollback`、`output_limit_retains_unknown_and_blocks_further_reads`、`nonzero_terminal_exit_blocks_completion_and_survives_drain`、`unread_terminal_output_blocks_completion_and_survives_drain`；E-TR `runtime_cancel_reaps_the_original_terminal_os_process` | 同左 |
| Device 管理（非 catalog 能力） | 不是模型可见能力；是 Host/Device 内部职责：进程监督、资源调度、PTY、清理确认 | 由 Host 配置和 Device 持有；模型不能直接调用 | 清理/持久化失败保留 unknown，不释放执行名额，不自动重放 | ✅ E-FT Device 93 项：lib 29、cleanup_completion 3、独立 early_call/last_owner_join/thread_exit 各 1、interactive 16、macos_lifecycle 7、process 25、scheduler 10。包括 `pty_mode_uses_a_controlling_terminal_and_the_same_managed_cleanup`、`supervisor_holds_the_permit_during_denied_cleanup_and_reaps_before_release`、`cleanup_failure_waits_for_supervisor_join_and_retains_resource_after_last_owner_drop`。Host 终端与 Runtime 终端四项也在 E-FT 再次执行；只证明本机 macOS arm64 的具名场景 | 宿主内部；经 Runtime 任务终态体现 |

---

## 8. 组八：人机与宿主协作

契约来源：`crates/cayw-runtime/src/collaboration.rs`、`crates/cayw-runtime/src/children.rs`、`crates/cayw-runtime/src/input.rs`、`crates/cayw-host/src/catalog.rs`（external）、`crates/cayw-bridge/src/model.rs`、`crates/cayw-bridge/src/registry.rs`。

**外部能力证据说明**：下表 `search.query`/`mcp.*`/`skills.*` 引用的 E-BT 是上一轮 21 passed 的契约路径。E-RT 已重新执行并通过 bridge 15 项和 runtime_bridge 21 项，包含等待所有真实 cell 完成的交错调用。`dynamic-requests-bridge-regression.log` 的 1 failed 保留为修复前的真实记录。外部服务使用本机适配器和固定 fixture，不宣称已验收后续 #9 的真实来源与连接生命周期。

| 能力/原语 | 参数 → 返回 | 精确权限与资源范围 | 预算/取消/失败 | 成功与负例入口（日志） | SDK/IPC |
| --- | --- | --- | --- | --- | --- |
| `user.ask` | `{question(1..4096),options?[≤16,每项≤1024]}` → `{question_id,answer,answered_at}` | 精确 `user.ask`+完整参数；问句绑定完整原调用身份（task/attempt/capability key）；模型没有回答能力 | 阻塞等待宿主回答；取消/过期返回已知失败；回答不产生权限授权 | ✅ 成功：E-PIPC `question_round_trip_is_durable_and_answered_by_host`、`cross_runtime_answer_uses_only_core_lease_queue_and_coordination`、`permission_expiry_stops_the_original_wait_as_known_failure`；E-HWR `default_compute_timeout_does_not_end_a_six_second_human_wait`、`runtime_absolute_deadline_still_ends_a_paused_human_wait`、`short_host_call_timeout_ends_the_wait_as_a_known_failure`。负例：E-PIPC `duplicate_answers_are_idempotent_or_rejected_and_never_grant`、`cancelled_question_keeps_its_fact_and_rejects_late_answer`、`denied_approval_never_persists_a_question`、`catalog_exposes_only_model_collaboration_and_rpc_answers` | 宿主 SDK `Runtime::answer_question`；IPC `question.answer`；读取 `session.questions`/`session.pending_questions` |
| `plan.update` | `{expected_version,steps[≤32]}`（每步 `{id(≤64,^[A-Za-z0-9_.:-]+$),text(≤1024),status}`） → `{task_id,attempt_id,previous_version,version,source:"model",steps}` | 精确 `plan.update`+完整参数；按任务版本 CAS；至多一个 `in_progress`；id 唯一 | 序列化步骤 ≤ 64 KiB；版本不符为已知失败；无版本写入 | ✅ 成功：E-PIPC `plan_update_uses_per_task_version_cas_and_validates_steps`、`invalid_plan_arguments_never_write_a_plan_event`、`catalog_exposes_only_model_collaboration_and_rpc_answers`；`collaboration_baseline.rs::model_plan_update_reaches_core_as_a_successful_capability`。认证 IPC 往返：E-PIPC `authenticated_rpc_round_trip_carries_image_plan_question_and_answer`。负例：`unauthenticated_transports_cannot_answer_or_touch_durable_facts`、`cancelling_resume_over_ipc_expires_question_without_unknown_effect`、`disconnecting_the_owning_connection_expires_question_without_unknown_effect` | 读取 IPC `session.plan`；SDK `Runtime::plan` |
| `agent.run` | `{prompt(1..256 KiB),label?}` → `{session_id,task_id,label,depth,state,output}` | 仅在 `local_agents` 显式启用时存在；子 Session/Task/Attempt 独立；子权限只来自 `LocalAgents.permissions`；不能继承/提升父权限；深度超限拒绝 | 树内共享 token/调用/字节预算；`max_children`、`max_depth`、`max_calls`、`max_output_bytes`、`max_time`（并受父剩余时间上限）；队列满立即拒绝，父等子不死锁 | ✅ 成功：E-CHD 10 项（`agent_run_starts_a_real_child_session_and_task_and_records_identity`、`shared_token_budget_is_consumed_by_the_child_not_reset` 等）。负例：E-CHD `child_permissions_are_independent_and_cannot_inherit_parent_grants`、`nesting_beyond_max_depth_is_refused_before_any_grandchild_task`、`concurrency_one_rejects_the_child_immediately_and_never_deadlocks`、`parent_cancellation_reaches_the_child_and_waits_for_its_terminal`、`child_result_is_bounded_by_max_output_bytes`、`model_arguments_cannot_add_permissions_or_other_authority`、`disabled_configuration_removes_agent_run_from_the_catalog`、`unconfirmed_child_cleanup_keeps_the_handle_slot_and_never_claims_finished` | `cayw.call`；子任务经 `session.list`/`session.tasks`/`task.get` 观察 |
| 图像输入（任务输入，不是 catalog 能力） | `TaskInput{text,images:[ImageInput{mime_type,data}]}`；MIME 只允许 `image/png`/`jpeg`/`webp`/`gif`；规范 padded 标准 base64 | 模型必须支持图像；`max_prompt_bytes` 计 text+MIME+编码数据；持久化后按原内容传给 Provider | 非法 MIME/编码/超限在入队前已知失败；纯文本模型带图在入队前拒绝 | ✅ 成功：E-IP `task_enqueue_accepts_text_and_canonical_images_as_one_input`、`task_enqueue_without_images_uses_the_same_input_shape`、`session_prompt_delivers_the_image_content_to_the_provider`、`task_input_message_keeps_text_and_images_in_order`。负例：E-IP `task_enqueue_rejects_invalid_images_before_insert`、`task_enqueue_rejects_images_for_a_text_only_model_before_insert` | SDK `Runtime::enqueue_input`/`prompt`；IPC `task.enqueue`/`session.prompt`（`images` 字段） |
| `search.query` | `{query(1..8192),path?,limit?}` → 应用已有有界搜索服务的结果 | 精确 `search.query`+完整参数；经宿主注入的 source；可能产生远程计费/工作，属 `may_mutate` | 结果受 Host 结果字节与 source 预算 | ✅ 成功：E-BT `all_six_external_operations_use_existing_consumer_through_luau`。负例：E-BT `host_and_registry_denial_dispatch_zero_sources_and_have_no_source_journal`（E-BA `local_integration` 同项） | `cayw.call` |
| `mcp.discover` | `{server?}` → 发现的能力列表 | 精确 `mcp.discover`+参数；只经宿主已有 MCP 连接；发现不授权 | source 预算；拒绝时零 dispatch | ✅ 成功：E-BT `all_six_external_operations_use_existing_consumer_through_luau`；E-BA `local_integration`（9 passed）、`registry`（8 passed）。负例：E-BA `host_gate_denies_discovery_skill_read_and_tool_execution_without_touching_source` | 同左 |
| `mcp.describe` | `{server,tool}` → 该工具描述 | 精确 `mcp.describe`+完整参数 | 同 discover | ✅ 成功：E-BT `all_six_external_operations_use_existing_consumer_through_luau`；E-BA `registry_filters_names_schema_and_content_separately_and_discovery_never_grants_invoke` | 同左 |
| `mcp.call` | `{server,tool,arguments}` → 工具结果 | 精确 `mcp.call`+完整参数；经宿主已有连接与权限；属 `may_mutate` | 结果过大保留 unknown；取消保留 unknown dispatch | ✅ 成功：E-BT `all_six_external_operations_use_existing_consumer_through_luau`。负例：E-BT `bridge_result_bound_is_known_for_skill_and_unknown_for_effectful_tool`、`source_failure_and_source_result_bounds_keep_their_known_or_unknown_outcomes` | 同左 |
| `mcp.resources.list` | `{server?}` → `{resources:[{uri,description,mime_type,source}]}` | 精确 `mcp.resources.list`+参数；只列 manifest 明确声明的资源；URI 对 Bridge 不透明 | 受 source 结果字节；不可见资源不列出 | ✅ 成功：E-BR 8 项（`resources_list_uses_the_catalog_path_and_respects_source_filters_and_visibility` 等）；E-BTR `runtime_lists_and_reads_the_pinned_resource_through_its_original_dispatch` | `cayw.call` |
| `mcp.resources.read` | `{server,uri}` → manifest 校验后的资源内容 | 精确 `mcp.resources.read`+完整参数；URI/MIME 必须与 manifest 完全一致；恰好一个内容字段 | 载荷 ≤ 预算；未知 URI/坏参数在 dispatch 前拒绝；取消/重连/迟到结果有栅栏 | ✅ 成功：E-BR `resource_read_uses_source_operation_dispatch_record_and_original_policy`、`local_mcp_fixture_lists_and_reads_a_manifest_pinned_resource`；E-BTR 3 项。负例：E-BR `resource_read_rejects_unknown_uri_and_bad_arguments_before_dispatch`、`resource_read_validates_uri_mime_exactly_one_content_and_payload_bound`、`resource_read_cancellation_reconnect_and_late_results_are_fenced`；E-BTR `denied_resource_read_reaches_no_source_and_records_no_dispatch`、`resource_dispatch_record_must_bind_the_original_server_and_uri` | 同左 |
| `skills.list` | `{}` → `{skills:[{name,description,content_sha256,source}]}` | 精确 `skills.list`+`{}`；只列宿主已有 skill source | source 预算；拒绝时零 dispatch | ✅ 成功：E-BT `all_six_external_operations_use_existing_consumer_through_luau`；E-BA `local_integration`、`registry`。负例：E-BA `host_gate_denies_discovery_skill_read_and_tool_execution_without_touching_source` | 同左 |
| `skills.read` | `{name}` → 命名 skill 内容 | 精确 `skills.read`+`{name}`；内容访问单独可见性 | 同 list | ✅ 成功：E-BT `all_six_external_operations_use_existing_consumer_through_luau`；E-BT `bridge_result_bound_is_known_for_skill_and_unknown_for_effectful_tool` | 同左 |

---

## 9. 收尾核对

1. Cells 两项历史失败已修复。E-CH90 与 E-FT 均通过全部 90 项。目录初始化不再消耗单项输出额度，但实际自省返回仍受同一额度限制；已开始且超过 `host_wait` 后仍无法结算的调用保留 unknown。
2. Workspace/Data 的 128 项、Device 的 93 项及 Host 直接 `fs.patch` 新断言均在 E-FT 实际执行。递归删除对照明确区分首次效果前已知失败与首次效果后 unknown，重开不自动重放。
3. `capabilities.list` 已有真实认证 IPC 对照；`session.grants`、`session.reconcile`、`capability.reconcile` 的实际覆盖层见 §0.6，不声称它们各自有独立 IPC 专项。固定离线 Provider、本机适配器与真实模型范围见状态规则，本轮真实模型请求为 0。
4. E-FF/E-FC/E-FT/E-FL/E-FO 完成最终五项检查，E-IN24 完成独立关键复跑。完整结果、忽略入口、历史失败、源码清单与文档收尾说明见[验收记录](validation/issue-5-preset-capabilities-macos.md)。本地交付已完成；未在本次工作中提交、推送、合并或关闭 GitHub Issue。
