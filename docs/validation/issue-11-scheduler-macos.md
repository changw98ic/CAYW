# Issue #11 macOS 多 Session 调度验收

2026-10-23，macOS ARM64 整项本地验收通过。主线程十五步完整检查与独立二十步在同一份源码上实际运行。主线程又核对独立原始日志、收据、实际测试程序及八条验收条件。GitHub [#11](https://github.com/changw98ic/CAYW/issues/11) 仍为 OPEN；本次没有提交、推送、PR、合并或远端状态更新。

## 范围与源码

本项交付本机多个 Session 的受理、容量、优先级、公平派发、安全审批等待、独立取消、恢复及公开查询。固定离线 Provider 提供输入；macOS 进程、文件、SQLite 和认证回环 IPC 实际执行。真实模型和付费请求均为 0，没有使用 #6 的模型预算。

| 项目 | 实际值 |
| --- | --- |
| 源码根 | `/Volumes/ORICO/cayw-issue11-integration-20261023-b` |
| 分支 | `feat/issue11-scheduler` |
| 基线 HEAD | `b14b401f37af877e03bbf11594a05dadb1289d47` |
| 测试源码文件 | 549，包含未提交修改和未忽略新文件，无缺失文件或符号链接 |
| 全部测试源码清单 SHA256 | `65dd284d5b347e71da65ec033c6635da487966cf7cee67b4c80b17a851627be2` |
| 测试源码 ZIP SHA256 | `739a640cac58b75159e768b3637f681611e9656d2c1ab3a7a6a317cc4fc0f888` |
| 根 Cargo.lock SHA256 | `4683a44f616e87fbf05b43d65821c9190d658026787e022dc5ef431ac3054c8b` |
| headless-client 锁 SHA256 | `59f004fc79e2f8b56cb923a871d37f5e335a746b84ad148bfcdfbc2fa4926d8b` |
| issue1-client 锁 SHA256 | `a4abfa90456b9ba3eb2ff19bf6dad86005f4190f375d7427b967bb9d39bf52a2` |
| 系统 | macOS 27.0.1，Build 26A434，Darwin 27.0.0 |
| 实际架构 | ARM64 / `aarch64-apple-darwin` |
| 工具链 | Rust / Cargo 1.95.0，debug/dev |
| target | `/Volumes/ORICO/cayw-issue11-target-20261023-b` |

HEAD 不代表本轮全部源码。清单按 `git ls-files -z --cached --others --exclude-standard` 的文件逐一记录长度及 SHA256，再按路径排序、用固定 JSON 编码计算摘要。每一步前后清单均一致。target 来自已有 dev target 的 APFS 缓存复制，关闭增量编译；不是干净构建。

测试后只补本文和 `docs/development-map.md`。交付清单、ZIP 和跟踪文件补丁由仓外 `seal_issue11.py` 另存，测试清单不回写。新增源码文件同时进入 ZIP 和清单，不能仅凭不包含未跟踪文件的 `git diff` 保存交付。

## 可观察行为

K 为同时持有的执行名额，范围 1..1024。C 为全部活动任务容量，包含排队、执行、审批等待、清理和待核对，C>=K，最大 100000。A 为提升等待者的派发窗口，范围 1..100000，默认 8。SDK 的 `max_queued_runs` 对应配置中的 `max_accepted_tasks`，表示全部活动任务容量。配置非法、队列已满或同 Session 已有活动任务时明确拒绝，不创建替代任务。

可信宿主在受理时选择 low/normal/high。enqueue 只保存任务；显式 resume 后才参与派发，重开不自动执行。未提升任务按优先级、再按就绪顺序派发。等待 A 次其他派发后提升；提升任务按原就绪顺序，先于未提升任务。在任务持续可执行且名额持续释放时，其他派发越过次数至多 A+C-1；并发新任务不增加该上界。实际计数是 `dispatch_count_at_low - low_ready_since_dispatch - 1`，减去 Low 自己的派发。审批后重新排队开始新的就绪轮次，不重置任务预算。

安全审批等待归还 K，继续占 C，并保留原 Task、Attempt、Worker、fence 和执行归属。归还前必须满足：没有活动模型请求，全部活动 cell 都在能力执行前等待，且原 Host 没有活动进程、未 join 的终端、未知状态、审计失败或未确认的终端完成事实。后台进程仍活动、输出未完整交付或清理未确认时保留原名额。允许/拒绝后原执行重新排队取得名额；取消直接停止和清理。

SDK 提供 `scheduler(limit)`、`scheduler_page(after, limit)`，认证 IPC 提供 `runtime.scheduler` 和 `task.enqueue_scheduled`。查询 limit 为 1..1000；Rust 显式传值，IPC 省略默认 100。页面按受理顺序；已结束的页末任务仍能作游标，未知游标拒绝。实时页面不冻结后续页，不表示未来派发顺序。原 IPC 字节限额保持。Core 协议/存储为 v3，IPC 帧为 v1。

恢复前核对原执行者和清理事实。没有 Attempt 的死亡认领解除 Worker、fence+1，保留输入、优先级和就绪位置，随后仍需显式 resume。已经开始的旧任务隔离为 Interrupted；保留历史 Attempt 不表示旧租约仍可写入。取消、清理或持久化不明保留原容量和待核对原因，不自动重放。

## 八条验收条件

权威原件为仓外 `issue11-current-final-preflight-r03.json`，Issue updatedAt 为 `2026-10-19T14:54:00Z`，SHA256 为 `9c6f1615f3b95d36042283fc98af9d1065e548ae21413d2e2b38ffe2e6d15c49`。

| 条件 | 主检查与独立实际证据 | 结果及含义 |
| --- | --- | --- |
| 1 配置、容量、状态、预算、有界查询 | Queue 单测；scheduler_load、scheduler_ipc、scheduler_recovery；两个独立客户端；终端调用和模型用量账本 | 通过。K/C/A 固定，活动状态和持名额明确，超限无新 Task，查询有界，原任务预算和原调用结算对应 |
| 2 有限公平机会 | Queue priority/FIFO/promote/new High/reopen/child 单测；独立 continuous_high 原件 | 通过。K1/C6/A2，上界 7；实测 Low 就绪计数 2、派发计数 5，其他派发为 5-2-1=2。Low 前三个 High 包含已在运行者，不能叫三次越过 |
| 3 至少三个 Session 同场 | 独立负载六个 Session：三个 High 各补充六轮、Low、Normal、审批；受控通知和完成许可 | 通过。负载期间原审批保持等待，未出现审批文件，Low 得到有限机会；不以固定毫秒断言公平 |
| 4 背压与有限排空 | QueueFull/Busy 原断言、IPC 拒绝收据、连续负载全部最终任务 | 通过。超限明确拒绝且无 Task；停止有限补充后 20 个可执行任务排空，再批准原审批，21 个已受理 Task 全部完成且不持名额 |
| 5 审批与原身份 | scheduler_approval、scheduler_failures、approvals；load 与认证 IPC 原件；VerifyTool 实际消费 | 通过。安全等待归还名额，其他精确授权文件推进；审批前无未授权文件；允许/拒绝、取消、迟到审批仍绑定原执行 |
| 6 真实工具并发和会话隔离 | 独立 scheduler_tools 三父测试及新十六份 JSON；scheduler_ipc 主场景和 cell 值隔离；主线程逐条核新原件 | 通过。两原进程并发，调用、权限、账本、游标和结果归属一致；跨 Session 取消/审批被拒绝，模型的 cell 值命名空间隔离；清理不明的 A 不提前归还名额 |
| 7 崩溃、恢复、旧执行者及未知占位 | durable_queue 七项；独立 scheduler_recovery 三项；五故障目标 36 项，包含真实 SIGKILL 和进程清理 | 通过。受理不自动执行，重开和恢复保留输入与公平位置，旧身份拒绝，恢复幂等；清理/持久化不明保留占位，无关安全工作使用其余名额 |
| 8 竞态和持久化失败 | Queue 事务/溢出/部分提交单测；独立 cancel_completion_order、cancel_terminal、scheduler_failures 和故障矩阵 | 通过。自然完成/取消两顺序均运行；失败不写成功，不删原任务或重放原效果；公开 Task、events、audit 保留阻塞原因 |

每个 Session 的 Core events 从 1 连续。audit 的序号来自全局自增表，同 Session 只要求严格递增，允许间隔。恢复后的历史 Task.fence 可比原 Attempt 租约增加 1；核对时没有把它误判为旧执行仍有效或身份倒退。

## 真实双进程与名额来源

测试通过宿主固定的 `terminal_handoffs` 选择 term-a/b/c；程序为实际测试可执行文件，程序、参数、环境和 cwd 由宿主固定。模型只能选择 profile 名。动态 write_stdin 的 session_id/request_id 使用原请求的精确审批，没有通配授权。没有删除 Host 同根写锁或移除 may_mutate 来制造并发。

独立新原件位于 `receipts/scheduler-tools/scheduler-tools/`，normal 八份、live 六份、永久回归两份，全部十六份 JSON 均为新运行产生。

- 正常模式：A/B 原 PID 为 49001/49002，同一屏障时两者活动并持名额，C 真排队。A 自然退出 0 后，read 尚未批准且输出未消费，仍持名额。取消后 A scope drained、Task Cancelled，未批准 read 为 not_executed。C 在 A 清理后取得名额。B/C 分别真实 stdout、EOF、exit 0、完整输出并清理；最终 running/queued 为 0/0。
- 运行中取消：A/B 原 PID 为 49004/49003。A 尚在运行时直接取消；原 exec 保持 completed，wait 实际为 failed，错误为 `cancelled while waiting for terminal`。OS 原进程身份已停止，但终端完成事实仍为 `Cancelled; complete=false`。Worker 返回要求显式恢复的错误，Task Draining、holds_slot=true、scope=open，无清理确认或运行终态。B 保持原执行，C 仍排队。B 真正完成输出及清理后 C 才准入。最终 A 原未知名额保留，B/C Completed，running/queued 为 1/0。
- 永久回归：exec-started 与 permission-park 绑定同一原 PID/Task，原进程仍活动、scope=open、审批未决，running 为 1，不能因下一审批而归还名额。

取消后的 cancel-issued 快照可能已看到 Draining 或停止进程。取消前活动证明来自原屏障、task_before 和测试中的原启动身份检查；没有额外要求取消后快照仍活动。live wait 合同允许已知的取消失败，或携带相同原终端事实的 outcome_unknown，禁止 completed。本轮实际是已知失败，Task 的清理未知另行保留。

主线程核对原 Session/Task/Attempt/Worker/fence、PID/启动秒和微秒/进程组/父 PID、Execution/call sequence、planned/started/settled、原工具来源、精确 scope 和可信审批。B/C stdout 各恰出现一次，EOF=true、truncated=false、exit=0、output_complete=true，输出消费字节和游标相符。正常清理序号先于 C；live 中 B 的 processes_stopped < run_finished < C 首 audit，且 C 开始时间不早于 B 终态。

每 Task 能力和 Host 调用上限均为 8。CodeMode 为 120 秒、100000 指令、64 次宿主调用；固定 profile 为 150 秒。exec/wait 后仍有有界预算下的 read-until-EOF 循环，不宣称没有循环。最终模型用量账本逐条核原身份、turn、128000 token 预算、合成 4/4/8 用量、结算与预留。合成用量不是真实账单；金额和币种保持 unknown/null，没有当作免费。

认证 IPC 拒绝错误 token、非法 priority、limit 0/1001、未知游标、跨 Session 取消和审批。错误取消返回 stale_cancellation，错误审批返回 stale_approval，原审批 Task 不变。owner Session 的 cell 值读回 `owner=s1`，另一 Session 读同 key 为 null。可信宿主按 Session 查自己的记录不等于模型可跨会话读取。

分页 SDK/IPC 同页、limit 1 遍历无重复，页末 Task 取消后仍可继续。limit 1000 接受只验证参数和页面，不表示运行过 1001 个活动任务。

## 主线程十五步完整检查

仓外证据根为 `/Volumes/ORICO/cayw-issue11-evidence-20261023-b`。主入口：

```sh
python3 -I /Volumes/ORICO/cayw-issue11-evidence-20261023-b/verify_issue11.py \
  --root /Volumes/ORICO/cayw-issue11-integration-20261023-b \
  --out /Volumes/ORICO/cayw-issue11-evidence-20261023-b \
  --target /Volumes/ORICO/cayw-issue11-target-20261023-b \
  --round r03
```

r03 已存在，复跑必须用新 round，不能覆盖原件。运行时间为 2026-10-23 13:39:47—13:54:01 UTC。固定环境：

```sh
export PATH="$HOME/.cargo/bin:$PATH"
export RUSTUP_TOOLCHAIN=1.95.0
export CARGO_INCREMENTAL=0
export CARGO_TARGET_DIR=/Volumes/ORICO/cayw-issue11-target-20261023-b
```

以下命令均在源码根执行。workspace 使用包默认 feature，故障目标显式加 fault-injection，没有 all-features。

| 步骤 | 实际命令 | 直接返回码 / 结果 |
| --- | --- | --- |
| 1 | `cargo fmt --all -- --check` | 0 |
| 2 | `cargo fmt --manifest-path examples/headless-client/Cargo.toml -- --check` | 0 |
| 3 | `cargo check --workspace --locked --offline` | 0 |
| 4 | `cargo clippy --workspace --all-targets --locked --offline -- -D warnings` | 0 |
| 5 | `cargo test --workspace --locked --offline --no-fail-fast` | 0；1741 通过、0 失败、7 忽略 |
| 6 | `cargo run -p cayw-agent --example offline --locked --offline` | 0；add 返回 5，五条消息，running=false |
| 7 | `cargo check --manifest-path examples/headless-client/Cargo.toml --all-targets --locked --offline` | 0 |
| 8 | `cargo test --manifest-path examples/headless-client/Cargo.toml --all-targets --locked --offline --no-fail-fast` | 0；13 通过 |
| 9 | `cargo clippy --manifest-path examples/headless-client/Cargo.toml --all-targets --locked --offline -- -D warnings` | 0 |
| 10 | `python3 -I scripts/test_validate_headless_chat.py` | 0；7 通过 |
| 11 | `cargo test -p cayw-runtime --features fault-injection --test cancel_completion_order --test cancel_terminal --test crash_reconciliation --test crash_recovery_matrix --test process_cleanup --locked --offline --no-fail-fast` | 0；36 通过 |
| 12 | `cargo fmt --manifest-path examples/issue1-client/Cargo.toml -- --check` | 0 |
| 13 | `cargo check --manifest-path examples/issue1-client/Cargo.toml --all-targets --locked --offline` | 0 |
| 14 | `cargo test --manifest-path examples/issue1-client/Cargo.toml --all-targets --locked --offline --no-fail-fast` | 0；3 通过 |
| 15 | `cargo clippy --manifest-path examples/issue1-client/Cargo.toml --all-targets --locked --offline -- -D warnings` | 0 |

完整 argv、每步源码检查、日志及实际测试程序摘要见 `verification-r03.json`，SHA256 `3cf778b8872ff29dbb01cb40c2af2126db2daa9d969b83cd57b7d680c75f46bb`。主驱动 SHA256 为 `246b184d0757b066e6d3e19c03fcddbb939752f90511d9b43bab1696ae451393`。

workspace 日志 `workspace-test-r03.log` SHA256 为 `4e8ec9e6f944b4bc8ba25fdcf79d6bb61fcce589330f7aeff568bdf80c313b17`。按 269 个 Cargo 顶层 Running/Doc-tests 区段各最后一条汇总计数，排除三条内部子进程重复汇总。`workspace-r03-count-check.json` 独立重算为 1741/0/7。

七项忽略为 independent_probes::crash_effect_child、restart::crash_child、restart::live_scope_child、future_disposal_regression::lifecycle_child、child_lifecycle::cleanup_failure_runner、child_lifecycle::fixture_child，以及 binaries 的可选远程 Server 场景。前六个为父测试调用的辅助入口；最后一个远程场景未执行。忽略不计通过，不据此声明远程能力已验收。

## 独立二十步

实际运行目录：

```text
/Volumes/ORICO/cayw-issue11-evidence-20261023-b/independent-r02/runs/issue11-r03-independent-20261023T140430Z-fb2166
```

运行时间为 2026-10-23 14:04:30—14:08:41 UTC。仓外 runner `independent-r02/run_independent.py` 为 131393 字节，SHA256 `8be55feb4a331bc0d0dd5f7f0e7c1190c1e7cdebfb154cf2f3721efc8eac139b`；plan 为 9732 字节，SHA256 `0b005d1bf9345ebb4cd4ad368f4406f1d8718f0f7b26ab255b001f5b6e634a04`。运行前后程序、计划和全部源码清单未变。

复跑入口：

```sh
python3 -I /Volumes/ORICO/cayw-issue11-evidence-20261023-b/independent-r02/run_independent.py \
  --repo /Volumes/ORICO/cayw-issue11-integration-20261023-b \
  --target-dir /Volumes/ORICO/cayw-issue11-target-20261023-b \
  --label issue11-r03-independent \
  --commit b14b401f37af877e03bbf11594a05dadb1289d47
```

新运行另建唯一目录。下表前十六步的命令形式为 `cargo test -p PACKAGE --test TARGET [--features fault-injection] --locked --offline`。其余四步是上表 issue1-client 的 fmt/check/test/clippy。

| 步骤 | Package / Target | 实际结果 |
| --- | --- | --- |
| 1 | cayw-runtime / scheduler_load | 3/0/0，直接 0 |
| 2 | cayw-runtime / scheduler_approval | 1/0/0，直接 0 |
| 3 | cayw-runtime / scheduler_ipc | 4/0/0，直接 0 |
| 4 | cayw-runtime / scheduler_recovery | 3/0/0，直接 0 |
| 5 | cayw-runtime / scheduler_failures | 4/0/0，直接 0 |
| 6 | cayw-runtime / scheduler_tools | 3/0/0，三个父测试，直接 0 |
| 7 | cayw-runtime / cancel_completion_order，fault-injection | 4/0/0，直接 0 |
| 8 | cayw-runtime / cancel_terminal，fault-injection | 5/0/0，直接 0 |
| 9 | cayw-runtime / crash_reconciliation，fault-injection | 3/0/0，直接 0 |
| 10 | cayw-runtime / crash_recovery_matrix，fault-injection | 2/0/0，直接 0 |
| 11 | cayw-runtime / process_cleanup，fault-injection | 22/0/0，直接 0 |
| 12 | cayw-verify-tool / access_scenarios | 2/0/0，直接 0 |
| 13 | cayw-verify-tool / access_evidence | 3/0/0，直接 0 |
| 14 | cayw-verify-tool / evidence | 30/0/0，直接 0 |
| 15 | cayw-verify-tool / sdk_cli | 4/0/0，直接 0 |
| 16 | cayw-tui / review_ipc | 2/0/0，直接 0 |
| 17 | issue1-client fmt | 直接 0，不计测试 |
| 18 | issue1-client check | 直接 0，不计测试 |
| 19 | issue1-client test | 3/0/0，两个顶层区段，直接 0 |
| 20 | issue1-client clippy -D warnings | 直接 0，不计测试 |

表内数字为通过/失败/忽略。合计 98 次测试执行通过，不与主检查重跑项相加宣称不同用例总数。每步直接码来自 subprocess，timed_out=false、source_unchanged=true。102 条检查全部为真，required_unverified=[]，没有使用 partial run。

独立程序清除继承的模型凭据和证据路径变量，再给每步设置新证据目录。报告中的 stripped_model_keys=[] 表示当时没有这些变量需要删除，未打印任何值。没有整仓重跑、真实模型或新的 UI 专项；review_ipc 仅验证原消费测试适配。

82 个收据文件全部重新计算长度和摘要，其中 tools 为 68 个文件，另有负载 3、IPC 3、恢复 8。收据索引内容摘要为 `ed025b9ef09b123684006eb0b9192ef4c61615dd2fadb9f8b84e8b3cef244408`；`receipt-index.json` 文件摘要为 `baefb7aec80633dd697ec6ecf86865ab39330364caa37396ae00bab1e4bf6680`。`report.json` 与 `conclusion.json` 字节相同，报告 SHA256 为 `906d5e1391c84e7d1213bfa1129ccdcaf53e7537e89b665c28f94dbe63c01b63`。

主线程用 `recheck_independent_r03.py` 再次核对二十份日志、二十份步骤记录、程序/计划、全部 82 文件、主/独立共 270 个不同路径的实际程序、源码 ZIP/清单及锁文件，直接返回 0。该程序 SHA256 为 `eb927a15085f490de91598b95de94d96d8d753984f76064ee9b492e45ca944bb`；结果 `main-independent-r03-recheck.json` SHA256 为 `b89a431cada411436d18aed92dd215357ad3e214ecc1c4db81b8d1ca201dbfdf`。核对还覆盖新 tools 原件的完整调用/审批/用量账本、负载上界、IPC 反例和分页，未用作者 r10 代替独立新运行。

部分测试在进程内完成其强断言，没有向独立目录另存 JSON，包括故障目标和 VerifyTool/TUI 消费目标。报告把这些证据形式明确列入 unverified，不声称存在单独 JSON。load 无 budget 字段，预算证据来自工具和模型请求账本。报告的普通未验证条目不代替八条条件的实证，必需缺口为空。

## 原失败、修正与保留

- r01 全仓直接 101。正确计数为 268 个顶层目标、1728 通过、10 失败、7 忽略；旧 1731 多计三条内部子进程成功汇总。原报告/日志不覆盖，更正单存。失败为 VerifyTool 九项和 TUI 一项仍要求安全审批占名额的旧测试。迁移消费状态，保留原身份、权限、游标和实际效果断言，并等待安全 park 提交；最终同源码全仓与独立消费测试均通过。
- 同根两个 test.run 被 Host 写保护串行，不能证明两个真实进程并发。保留该保护，使用已有受控终端路径。
- r04/r05 发现真实缺陷：后台进程仍活动时，下一审批提前归还名额。r05 永久回归直接 101；增加原 Host 五项安全检查后，r06 相同回归直接 0。r03 主全仓及独立三父测试再次通过。
- r09 只证明 A 自然退出后取消，不能当作运行中取消；其收据写 8 而实际旧脚本预算为 4096 的错误保留。r10 改为有限脚本及实际预算，增加 live 取消；最终独立运行另产新原件，没有修写 r09/r10 原件。
- issue1-client 原 locked 命令编译前直接 101。离线最小更新锁文件 17 行，补本地 path 包 cayw-providers 及现有依赖引用；没有第三方版本或来源升级，根锁和 headless-client 锁逐字未改。最终两个独立客户端的 locked/offline 检查均通过。
- r02 第一项格式检查直接 1，只有 scheduler.rs 调用链换行不合 rustfmt；其余十四项未运行。仅修换行后新建 r03，十五项全部重跑。两份清单仅该文件格式差异。
- 早期独立 shell 包装的只读变量错误、首次故障命令参数错误和准备解析器的字段/计数错误分别保留。最终使用直接 subprocess 返回码；解析实际 processes 键、按 Session 映射、audit 递增和恢复后历史 fence，没有改产品或放宽原权限/未知状态。

## 交付封存与边界

仓外 `seal_issue11.py` 使用主 `verification-r03.json` 和上述完整独立 `report.json`，核源码一致、十五加二十步直接返回码、日志与程序摘要，并只允许测试后的两个文档变化。封存使用新 label `r01`，输出 `final-local-delivery-r01.json`、`delivery-source-r01.json`、`delivery-source-r01.zip` 和 `delivery-tracked-r01.patch`。交付源码摘要与测试源码摘要分别保存；本文不包含自身摘要，避免自引用。原失败和旧封存保持原样。

本记录只覆盖上述 macOS ARM64、配置、输入和实际场景。其他 macOS 架构、机器或设备断电未验证。没有固定吞吐收益、固定毫秒公平保证或 1001 活动任务的吞吐证据。Linux、Windows、plugin、UI、远程 Device/Server 不在本项范围。

资源清理未知和模型费用未知分别记录。unknown 不证明发生了未授权效果，也不能为了验收清除它。live 组合最终保留 A 的未知名额是验收结果的一部分。没有修改 #6—#10 的既有封存。
