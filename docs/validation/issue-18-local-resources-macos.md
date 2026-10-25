# Issue #18 macOS 本机资源与权威内容验收

2026-10-23 完成下述本机行为检查，2026-10-24 补齐资源矩阵与证据映射。本记录分别绑定 r01 完整检查和 r02 成对恢复补充，不把两轮写成同一份源码的一次运行。源码尚未提交或推送；本项没有创建 PR、合并或更新远端状态。保存的 GitHub [#18](https://github.com/changw98ic/CAYW/issues/18) 原件为 OPEN，updatedAt 为 `2026-10-19T14:54:05Z`。

## 范围与源码身份

交付本机资源声明准入、进程输出与时间预算，以及由本机 Device 持有的权威内容。固定 Provider 提供模型响应；文件、SQLite、进程、Pipes/PTY、SIGKILL 和认证回环 IPC 均实际执行。本项真实模型与付费请求为 0；既有真实模型授权、未知用量及未结算预留保持原账本，不重新填满，不使用 #6 的独立预算。

以下 `E` 表示仓外证据根 `/Volumes/ORICO/cayw-issue18-evidence-20261023-a`，`C` 表示控制程序目录 `/Volumes/ORICO/cayw-issue18-control-20261023-a`。表中的日志、JSON 和收据路径均相对于 E。源码根为 `/Volumes/ORICO/cayw-issue18-integration-20261023-a`，分支为 `feat/issue18-local-resources`，基线 HEAD 为 `b14b401f37af877e03bbf11594a05dadb1289d47`。HEAD 不包含本次所有改动。

| 身份 | 文件数 | 完整清单 SHA256 | 适用证据 |
| --- | ---: | --- | --- |
| #11 最终交付基线 | 550 | `376cdf4050b55970fa2bb1b399f7c4cb286f5f2197006e2ebffcc969eccba747` | `baseline-source.json`、`baseline.json`；逐文件继承 #11 已封存交付，包含其未提交代码 |
| #18 r01 | 560 | `9c2c7c9d67f1e914e4a85b2f2552c17ab55a301b8646a395af9803ae85172dab` | `final/source-inventory-r01.json`；完整十九步及独立 r01 |
| #18 r02 | 560 | `aacccbefb1bfe2ae03207bca848c000824edd20be62d4f64dba4895dcca87b89` | `supplemental-r02/source-inventory.json`、`independent-r02/source-inventory.json`；主/独立成对内容恢复，格式和故障 Clippy |

清单按 Git 列出的已跟踪及未忽略新文件逐一记录路径、长度和 SHA256，再用固定 JSON 编码计算摘要。每条验证命令前后核完整清单。`source-change-scope-r01.json` 记录相对 #11 的 21 个修改、10 个新文件，无删除或缺失。r01 到 r02 仅在 `crates/cayw-runtime/tests/content_recovery.rs` 末尾追加四个测试入口；原函数、断言、产品实现及锁文件均未改。精确补丁为 `supplemental-r02/source-delta-r01-to-r02.patch`，主线程审计还核对当前文件以 r01 ZIP 原字节为前缀。本文及路线图的后续文档收尾另存最终交付清单，不回写上述测试清单。

#11 交付封存原件为 `/Volumes/ORICO/cayw-issue11-evidence-20261023-b/final-local-delivery-r01.json`，SHA256 为 `b593a351028e981324aa8d1bdd1d479ac68aea1256d5d511e6f04703e2d6b376`。本项不重写其测试、收据或封存记录。

| 构建条件 | 实际值 |
| --- | --- |
| 系统 | macOS 27.0.1，Build 26A434，Darwin 27.0.0 |
| 架构 | ARM64，`aarch64-apple-darwin` |
| 硬件 | Mac16,1，Apple M4，16 GiB 内存，10 个物理 CPU、10 个逻辑 CPU；`supplemental-r02/hardware-r02.json` 保存实际 sysctl 输出 |
| 工具链 | rustc 1.95.0，commit `59807616e1fa2540724bfbac14d7976d7e4a3860`；Cargo 1.95.0；LLVM 22.1.2 |
| profile | debug/dev/test，`CARGO_INCREMENTAL=0` |
| 主 target | `/Volumes/ORICO/cayw-issue18-target-20261023-a` |
| 独立 target | `/Volumes/ORICO/cayw-issue18-target-content-20261023-a` |
| 缓存 | target 使用已有缓存的 APFS 复制，不是干净构建。baseline 中最初的空目录描述不代表后续实际构建方式 |
| feature | workspace 默认配置；另测本地内容无默认 feature、内容默认 feature 与显式 `fault-injection`；没有执行全仓 all-features |
| 网络与凭据 | 测试命令使用 offline；独立 runner 另设 `CARGO_NET_OFFLINE=true`。清除继承模型及 CAYW 配置变量，只记录变量名，不记录凭据值 |

三个锁文件在 r01/r02 均相同：

| 文件 | SHA256 |
| --- | --- |
| `Cargo.lock` | `f165171c1f130e16db9eaaa121048b8554642c105cb56c026945c55226fcac46` |
| `examples/headless-client/Cargo.lock` | `3390364f8738caa5436139ecf075a481dfb0a4cc1d8131488c795658230a0272` |
| `examples/issue1-client/Cargo.lock` | `dbcaaebd31be5f3a80c552bd10f4138c36ac80df0e754ebad0bb77b01cc22736` |

`dependency-locks-r01.json` 核对已有 registry 依赖身份均未改变或删除。根锁没有新增 registry 依赖；两个客户端补入根锁已有的 cap-fs-ext/cap-primitives/cap-std 3.4.6、io-extras 0.18.4、rand_core 0.6.4。沿用 Apache-2.0/MIT 等兼容许可，没有 registry 版本升级。默认内容库中的加密与 MemoryRemote 只作已有 fixture 回归，不据此验收真实远程服务。

## 资源矩阵与逐行证据

公开 `Runtime::resources()` 与认证 IPC `runtime.resources` 返回同一个 `ResourceView`，含策略、当前声明占用与七行矩阵。矩阵字段为 resource、limit_type、status、mechanism、scope、error_behavior；本机证据路径由本文维护，不写入产品 API。

类型含义：硬限制需要操作系统或相应执行机制强制约束；软件准入只控制任务能否取得执行声明；软件预算在运行中触发取消和清理；仅观察不执行限制；明确不支持表示请求该保证即拒绝；未验证表示缺少当前平台的实际结果。本轮没有“仅观察”或“未验证”伪装成支持的行，也没有声称已实现 CPU、实际内存、GPU 或 I/O 带宽硬限制。

后文独立命令表列出 I1— I10，资源矩阵使用其中 I1— I5。每项命令的直接退出码均为 0，对应 JSON 另存命令、完整清单、日志摘要和实际测试程序摘要。

| resource | 类型 / 状态 | 实际机制与范围 | 错误、耗尽或等待行为 | 对应实际证据 |
| --- | --- | --- | --- | --- |
| `declared_memory` | `software_admission` / `implemented` | SQLite IMMEDIATE 事务在同一数据库所有 Runtime 连接之间检查声明总额，并把预留与原 Task 的 holds_slot 一起提交；不测量实际内存 | 超总额在受理前拒绝；已选队首暂时不足则等待，小声明不能越过；审批安全等待归还执行声明，恢复时重新准入；Draining 保留 | I1 `resource-sdk-ipc.log` 记录测试名和 9 passed；其中 `resource_ipc.rs:76` 测试的原断言核 SDK 80、IPC 60、总额 100，80 获准、60 等待、101 拒绝，随后 80→60→0，具体值见该文件第 83、114、147、182—201、220—238 行；I2 `queue-resource-unit.log` 记录十项通过，覆盖并发连接、队首、审批、取消、提交失败和重开；I3/I4 核实际进程退出及原身份 |
| `actual_memory` | `hard_limit` / `unsupported` | 没有 RSS（进程实际驻留内存）强制限制或采样；受管进程及后代没有此保证 | 任意 `Some(hard_memory_limit_bytes)`，包括 0，受理前拒绝；无测量值不能报告为 0 | I1 四项 resource_policy 测试及 SDK/认证 IPC 拒绝断言；非法请求不创建 Task、不预留声明。没有声称执行了 RSS 越限试验 |
| `cpu` | `hard_limit` / `unsupported` | 没有 CPU quota 或 CPU 时间采样；墙钟时间预算不是 CPU 限制 | 任意 `Some(cpu_quota_percent)`，包括 0，受理前拒绝；不输出虚构 CPU 用量 | I1 `unsupported_guarantees_are_rejected_even_when_the_requested_number_is_zero` 及认证 IPC 拒绝；无 Task、无预留 |
| `gpu` | `hard_limit` / `unsupported` | 本机基线没有 GPU 分配、隔离或采样 | 非零 `gpu_units` 受理前拒绝；0 表示没有申请，不表示测得 GPU 用量为 0 | I1 resource_policy 及 resource_ipc 实际拒绝；没有以机器存在 GPU 推断隔离支持 |
| `io_bandwidth` | `hard_limit` / `unsupported` | 没有磁盘或网络带宽限制器；输出字节预算不能替代带宽限制 | 任意 `Some(io_bandwidth_bytes_per_sec)`，包括 0，受理前拒绝 | I1 SDK/认证 IPC 与零值边界断言；未执行带宽限速或硬越限试验 |
| `process_output` | `software_budget` / `implemented` | Device 对受管 Pipes/PTY 的 stdout、stderr 原始字节累计生命周期预算 | 耗尽后停止原进程组，沿 Host 记录终态或效果未知；清理及持久终态确认前不归还执行声明 | I3 `chain_matrix_records_and_cleans_every_generation_per_trigger_point` 实际父/子/孙链，Pipes 与 PTY 各触发 OutputBudget，配置 4096 字节；主 `final/runtime-faults-r01.log` 的 CHAIN_CELL 各记 OutputLimit、stdout_bytes=4096、NeedsReconciliation 和清理后释放；I5 另独立执行两个 Device 输出预算测试 |
| `elapsed_time` | `software_budget` / `implemented` | Runtime 总运行期限、Device 进程 lifetime timeout 向原执行者取消；覆盖原 Task 和受管进程存活时间，不是 CPU 时间 | 超时走原 Host/Device 清理和结算；清理不明保留 Draining；业务效果未知仍待核对 | I3 独立日志记录两个具名测试通过；精确配置和结果见主 `final/runtime-faults-r01.log` 的 CHAIN_CELL 与 RESOURCE_RUN_DEADLINE：链矩阵 Device 期限 4 秒，Pipes/PTY 均为 TimedOut；另 `runtime_deadline_stops_the_real_chain_before_resource_release` 为 Runtime 10 秒、进程配置 60 秒，返回 Budget("time")；原链停止后才归还 80，Task OutcomeUnknown、Session NeedsReconciliation，禁止重放 |

默认 `max_declared_memory_bytes` 为 1 GiB，合法范围 1..=i64::MAX。`declared_memory_bytes=0` 表示没有申请声明，不表示实际占用为 0。所有加法和 SQLite 有符号转换检查边界；同数据库打开不同策略失败。准入范围是同数据库，不是整台机器所有数据库，也不是各 task_host 独立内存账本。

I3 的真实三代链先保存并重新从内核读取 PID、启动时间、父 PID、进程组，再触发正常结束、取消或输出；4 秒 Device 期限是真实期限，没有用测试屏障延后期限。每种 I/O 均覆盖 Normal、Cancel、Timeout、OutputBudget。正常场景 Idle，其余场景的副作用保持 NeedsReconciliation。`processes_stopped` 必须早于运行终态；新 Runtime 的 active_count=0 不能代替原执行者停止证据。进程组不是安全沙箱，脱离该组的进程不在此链矩阵的保证内。

## 独立命令、日志与直接结果

独立 r01 使用单独 target，未参与产品实现。以下均以 r01 清单运行，原始 JSON/日志位于 `independent-r01/`。命令省略的 offline 环境仍由 runner 固定；原 argv 保留如下。

| ID | 实际命令 | 日志 / 同名 JSON | 直接退出码与实际结果 |
| --- | --- | --- | --- |
| I1 | `cargo test -p cayw-runtime --test resource_policy --test resource_ipc --test resources --locked --offline` | `resource-sdk-ipc.log` | 0；4+2+3=9 passed，包含原 MCP 资源三项回归 |
| I2 | `cargo test -p cayw-runtime --lib resources_tests --locked --offline` | `queue-resource-unit.log` | 0；10 passed |
| I3 | `cargo test -p cayw-runtime --features fault-injection --test process_cleanup --locked --offline` | `process-cleanup-chain-matrix-terminal-deadline.log` | 0；24 passed，包含链矩阵、Runtime 期限、清理失败、持久终态失败与宿主强杀 |
| I4 | `cargo test -p cayw-runtime --features fault-injection --test authority_process_recovery --locked --offline` | `authority-process-recovery.log` | 0；3 passed，包含辅助 child |
| I5 | `cargo test -p cayw-device --test process output_budget --locked` | `device-output-budget.log` | 0；2 passed |
| I6 | `cargo test -p cayw-runtime --test content --test content_boundaries --locked --offline` | `content-runtime-normal-boundaries.log` | 0；3+20=23 passed |
| I7 | `cargo test -p cayw-content --test root_metadata --test local --test recovery --test concurrent_open --locked --offline` | `content-library-normal.log` | 0；6+10+6+3=25 passed |
| I8 | `cargo test -p cayw-content --features fault-injection --test commit_windows --test nonblocking --locked --offline` | `content-library-fault-windows.log` | 0；4+4=8 passed；文件名 windows 指提交窗口 |
| I9 | `cargo test -p cayw-content --lib --locked` | `content-library-unit.log` | 0；9 passed |
| I10 | `cargo test -p cayw-runtime --features fault-injection --test content_recovery --locked --offline -- --nocapture` | `content-recovery-sigkill.log` | 0；r01 5 passed，四个真实强杀父场景和一个辅助 child |

I1 日志 SHA256 为 `5c1b963d5ba258178ca9d35393f51882c6453fc314081855564adb56f3aefeb0`，I2 为 `2ac3133db4e4e63df8c64f6ddb2d731d95541fe23925d68a1c8c36e498d8c5be`，I3 为 `7cff2433e12d0d261b7c4e60bb50c0432e651a0db59f7378f1e7dbc4628f7c91`，I6 为 `23f50c02c854e6c20e1fc81da1fb743659a07b72558bc685b34964f26b2da7ae`。其余摘要均由各 JSON 和 `supplemental-r02/coordinator-audit-r02.json` 绑定。

另两条 `device-process-list` 与 `list-process-cleanup` 仅执行 `--list`，直接退出码 0；不计作通过测试。独立 r01 共十二条报告，十条测试报告合计 118 次 passed、0 failed，包含辅助 child 和相互重叠的覆盖，不能称为 118 个不同业务场景。`independent-r01/review-r01.json` SHA256 为 `445cc04b5f83c85c8706eb092aa77405debbe0878558b93672d84bb799b9d2ef`。

独立复跑入口为 `C/independent18/run_independent_step.py`，SHA256 `6588efd2adf0ac8d5b6e87dfd882308bfaa2f18f9b27a85c483121ae3865b181`。例如在源码根运行，使用新的输出目录和 label：

```sh
python3 -I /Volumes/ORICO/cayw-issue18-control-20261023-a/independent18/run_independent_step.py \
  --root /Volumes/ORICO/cayw-issue18-integration-20261023-a \
  --out /absolute/new-evidence-directory \
  --target /absolute/separate-target --label resource-sdk-ipc \
  -- cargo test -p cayw-runtime --test resource_policy --test resource_ipc --test resources --locked --offline
```

## 本地权威内容与权限

Runtime 可选配置 `ContentConfig`，持有同一个 `ContentStore`。内容目录和模型 workspace 互不包含。Device、Session、可选 Task、逻辑名一起计算物理记录名；默认 session 范围，也可选 task 范围。模型只能提供逻辑名、正文、范围与预期版本，不能提供 owner、绝对路径或物理名。即使逻辑名恰为 64 位 hex，仍按当前归属重新计算物理名。

`content.read`、`content.put` 经过原 `Host::call_service`、权限、Requests、CallScope、planned/started/settled、预算和取消链。可信 SDK `content_inspect` 和认证 IPC `content.inspect` 只读核查实际内容，不自动结算未知调用。给定 Task 必须属于给定 Session。I6 实测未授权 put 不创建记录、没有 started call；跨 Session/Task、错误 token、模型 owner/path 注入、路径逃逸和符号链接替换均拒绝。某段正文的精确授权不能授权另一段正文。

I6 的 `local_content_survives_reopen_and_updates_compare_full_revision` 实际创建、读取、修改、关闭后重开同一 Session，并核正文、version、revision、内容 hash 及归属。I7 的独立进程和并发比较验证直接库行为。`ExpectedRevision` 同时检查 version 和 revision；相同整数版本、错误 revision 不能覆盖。冲突返回实际版本；损坏、缺失、配额耗尽和持久化失败明确报错，不回退旧缓存或静默新建空库。

库实际结构为 `.lock`、`state.json`、`objects/<sha256>`。对象不可变。对象临时写入、同步、rename、目录同步后，再写临时 state、同步、替换 state.json、同步目录。状态替换后的同步失败返回 `CommitOutcomeUnknown`，必须重新核实际状态。对象和 state、Core、Cells 不在一个原子事务中。历史和孤立对象计入总内容配额；文件系统块、目录项等不属于正文限额。没有自动回收或 last-writer-wins 覆盖。

Runtime 先在 SQLite IMMEDIATE 事务核持久 binding，再在同一不跟随符号链接的目录句柄上核 mode/device/inode，最后初始化内容文件。已有 binding 的根必须存在；disabled/null binding 不允许隐式启用，配置、Device、限额或根身份变动拒绝，不接触候选库。binding 可能早于整个 Runtime::open 成功提交；后续打开失败不会自动撤销该绑定。

`try_read`/`try_put` 与阻塞接口共用真实锁和校验。Runtime 遇 Busy 异步等待，并重新检查原权限和取消；取得锁后同步提交到已知结果，中间没有 await，也没有脱离 Task 的后台写。锁等待取消可能观察到已知 Cancelled 或 OutcomeUnknown，取决于取消与提交观察顺序；不为通过测试清 unknown。初始化仍可能同步等待，未实现 try_open；磁盘提交不能承诺被强制中断。

直接库 API 的 1,048,577 字节正文可创建并校验。模型 content.put 限制 64 KiB UTF-8 字节，这不是直接库的输入上限。默认 IPC 帧无法容纳大 ContentView 时，实际返回 `response_too_large_outcome_unknown`、`outcome_unknown=true`，提示核对 Runtime；随后 health 仍 ok，state/cursor 不变。没有扩大帧或增加分块读取，也不承诺大内容 SDK 与 IPC 有相同载荷范围。I6 的具名大内容测试覆盖此行为。

## 四窗口与两恢复入口

r01 每窗口只使用一种恢复入口。独立复核提出同窗口成对比较缺口后，r02 追加四个反向测试入口，形成四窗口 × SDK/认证 IPC。初始 child 写入共同使用 SDK；transport 开关选择后续 inspect、recover、reconcile 及新 Task 的入口，不能解释成初始写入走过两套传输。

固定输入为 `return cayw.call('content.put',{name='note',text='committed text',expected_version=0})`；Device 为 `original-device`，资源总额 100、原声明 80、并发 1、容量 2，Runtime 总期限 120 秒。put/read 均精确授权原参数。父进程核 ready.pid 等于 child.pid 后实际 SIGKILL，wait 必须 signal 9，不用注入错误冒充进程死亡。

| 中断点 | SDK / 认证 IPC | 实际内容与恢复结果 |
| --- | --- | --- |
| `content_state_staged` | 主各一次，独立各一次 | 权威 record 为 null、state.records 空；原 put 未提交，调用效果保持 unknown，不自动重放 |
| `content_state_renamed` | 主各一次，独立各一次 | 实际 version 1，state、对象字节、hash、revision 一致；Core 结果仍 unknown，须可信核查 exact 内容后结算 |
| `content_state_synced` | 主各一次，独立各一次 | 与 renamed 相同的已提交/待核对分类；保留原版本，不重复 put |
| `core.capability_terminal.committed` | 主各一次，独立各一次 | exact Core 终态已提交，直接修复投影，不重写内容 |

每份收据保留原 Session/Task/Attempt/Worker/fence、授权和原调用。恢复前原 Task Running、holds_slot=true、占用 80；确认原进程 signal 9 后 recover，原 Worker/Attempt 保留，fence 1→2，Task Interrupted、holds_slot=false，声明字段仍为 80，同库当前持有合计为 0。业务效果 unknown 与执行者已停止是不同事实；可确认资源归还不等于副作用已知。

renamed/synced 两窗口的两入口各保存一次后续只读 continuation。没有 verifier 时，证据字符串不能结算；伪造 Task 或结果 version 2 被拒绝；exact 结算重复调用幂等。原父 execute_code 是已知中断错误：is_error=true、outcome_unknown=false，正文为 `Worker lost the tracked VM response; inspect its independent execution and capability records`。不能改成父成功，也不额外伪造父工具结算。

只读 SQLite 核对原 cell source/hash、parent_call_id、身份和唯一子调用。原 Cell Interrupted，原 content.put Succeeded；Cells sequence=Core sequence+1。后续新 Task 只有一次 content.read，仍为 version 1、相同 revision/hash，没有第二次 put。每个恢复阶段 Provider 调用为 0；后续新只读 Task 使用固定 Provider，两次请求，不是真实模型请求。

| r02 检查 | 直接结果 | 报告与日志 |
| --- | --- | --- |
| 主成对窗口：`cargo test -p cayw-runtime --features fault-injection --test content_recovery --locked --offline -- --nocapture` | 0；9 passed、0 failed，其中 8 个强杀父场景、1 个辅助 child | `supplemental-r02/content-recovery-eight-windows.json/.log`，日志 SHA256 `83a99d9df215d97db13bfd5f8856ea86d318aa2022b1edb2e57312e7fad894ea` |
| 独立同九项：同命令去掉 `-- --nocapture` | 0；9 passed、0 failed | `independent-r02/content-recovery-eight-windows-r02.json/.log`，日志 SHA256 `c6610f45be6dcff789b509501b7120a7ca1cac006c483d5ea86a0c20e89ba153` |
| `cargo fmt --all -- --check` | 0 | `supplemental-r02/workspace-format.json/.log` |
| `cargo clippy -p cayw-runtime --features fault-injection --all-targets --locked --offline -- -D warnings` | 0 | `supplemental-r02/runtime-fault-clippy.json/.log`，日志 SHA256 `4c8c7217059da9e3e7666d2d3a3d341cd3b6c837b8ace933eb0d6b1b2805f1dd` |

主收据目录为 `supplemental-r02/content-receipts/`，独立目录为 `independent-r02/content-receipts-r02/`。主/独立各八份 signal 9 原件、各四份 continuation。两轮随机身份、revision 及程序摘要分别保留，不能混用。

独立 `review-r02.json` SHA256 为 `2670eede2a19f840b52635a4b6fade8ff8c0ee4c17fffcc4580e6e66be23c580`；它确认条件 2—8，保留条件 1 的正式矩阵映射缺口。本表及独立命令表补齐该映射，文档最终核对另存新报告，不覆盖 r01/r02。r02 中“4 秒期限”对应 Device 链矩阵；Runtime 总期限测试为 10 秒，两个测试不能合成同一个期限值。

原主审计报告 `supplemental-r02/coordinator-audit-r02.json` SHA256 为 `8a62a16924d1d98f385413ce0809cb3c44e751855f3d8a6843cc3622900a8c40`，保存 passed=true、十六条命令报告、十六份强杀收据、八份 continuation、SQLite、对象字节、完整清单及四入口追加的核对结果。原进程没有单独保存 stdout 或退出码回执，不能仅由报告写入推断直接退出码。

为补齐可复跑记录，本次实际执行 `python3 -I C/recheck_issue18_receipts.py --auditor C/audit_supplemental.py --evidence E --out E/coordinator-recheck-r03/recheck.json`，直接退出码 0，命令及起止时间见 `coordinator-recheck-r03/command.json`，原日志见 `coordinator-recheck-r03/recheck.log`。新脚本只调用原审计的只读报告/收据函数，核十六条报告、十六份强杀收据、八份 continuation 与原报告完全一致，并核当前源码前后不变；没有重跑产品测试，也没有为旧进程补写退出码。新报告 SHA256 `fb6f1edd4e15e8228a5a4342674c1e5e4628608c60844e5e1277a2e915030580`，日志 SHA256 `7912c806630193378d1ff077dc7079bff131abe0be093ee2575861628111bf0d`。原脚本独占创建输出，不能原样复跑覆盖旧报告或补丁；新复算也须使用新的输出位置。

## r01 十九步完整检查

完整入口 `C/verify_issue18.py` SHA256 为 `9085deecd42554597d926cbe6628193f3bf7d3d5b90fe094d00b88394174fcc0`。原命令：

```sh
python3 -I /Volumes/ORICO/cayw-issue18-control-20261023-a/verify_issue18.py \
  --root /Volumes/ORICO/cayw-issue18-integration-20261023-a \
  --out /Volumes/ORICO/cayw-issue18-evidence-20261023-a/final \
  --target /Volumes/ORICO/cayw-issue18-target-20261023-a --round r01
```

该 round 已存在，复跑必须使用新 round。运行时间为 2026-10-23 17:10:04—17:24:30 UTC。完整报告 `final/verification-r01.json` SHA256 为 `57adfac94dfb064cf642f85007714b143ebb841435ef02cf4bc96efb569757e2`；源码 ZIP `final/source-r01.zip` SHA256 为 `7a7301056e6cc7dbfff5ae87bbd58fe6a0d960f93f8c6c4a2eda5d760f3fb408`。下列十九步全部直接退出 0、每步前后源码不变。日志名为 `final/<label>-r01.log`，完整 argv 与摘要在报告中。

| label | 实际检查 | 结果 |
| --- | --- | --- |
| workspace-format | `cargo fmt --all -- --check` | 0 |
| headless-client-format | 独立 headless-client manifest 格式检查 | 0 |
| issue1-client-format | 独立 issue1-client manifest 格式检查 | 0 |
| workspace-check | `cargo check --workspace --locked --offline` | 0 |
| workspace-clippy | `cargo clippy --workspace --all-targets --locked --offline -- -D warnings` | 0 |
| workspace-test | `cargo test --workspace --locked --offline --no-fail-fast` | 1793 passed、0 failed、7 ignored，277 个顶层目标 |
| offline-example | `cargo run -p cayw-agent --example offline --locked --offline` | add=5，历史 5 条，running=false |
| headless-client-check | 独立 manifest、all-targets、locked/offline check | 0 |
| headless-client-test | 独立 manifest、all-targets、locked/offline test | 13 passed、0 failed |
| headless-client-clippy | 独立 manifest、all-targets、locked/offline、-D warnings | 0 |
| issue1-client-check | 独立 manifest、all-targets、locked/offline check | 0 |
| issue1-client-test | 独立 manifest、all-targets、locked/offline test | 3 passed、0 failed |
| issue1-client-clippy | 独立 manifest、all-targets、locked/offline、-D warnings | 0 |
| script-selftest | `python3 -I scripts/test_validate_headless_chat.py` | 7 passed |
| content-local-faults | cayw-content，无默认 feature，fault-injection，locked/offline | 41 passed、0 failed |
| content-default-faults | cayw-content，默认 feature + fault-injection，locked/offline | 56 passed、0 failed |
| runtime-faults | cayw-runtime，fault-injection，下面七个具名目标，locked/offline、nocapture | 46 passed、0 failed |
| content-local-clippy | cayw-content，无默认 feature + fault-injection，all-targets、-D warnings | 0 |
| runtime-fault-clippy | cayw-runtime，fault-injection，all-targets、-D warnings | 0 |

Runtime 七目标为 content_recovery、crash_recovery_matrix、crash_reconciliation、process_cleanup、authority_process_recovery、cancel_terminal、cancel_completion_order。嵌套 child 输出不重复计入顶层测试摘要；不同命令中的重叠测试不相加成不同业务场景。默认忽略的七项不改称通过。workspace 编译已有可选组件不表示本机 Runtime 启用了 UI、plugin 或远程服务。

`C/audit_primary.py` 实际独立复算完整 r01 清单、ZIP 每个文件、三锁、十九份原日志和 285 条实际程序记录。成功原件 `final/primary-audit-r01.json` 记录 passed=true、19 步、285 个唯一程序路径。原始未带 hash 的 `debug/examples/offline` 被后续 Cargo 复用，首次审计 exit 1 保留在 `primary-audit-alias-failure-r01.json/.log`；随后以 `program-alias-map-r01.json` 明确指向保留的 `offline-a60163f222160f77`。该文件长度 18,215,880、SHA256 `6f9348d21e9944bd94ec6ab41e911feadd7f98cdbf665d4807542f16318d07bb`，与原运行记录完全相同。没有改写原测试报告或把失败删除。

该 285 记录审计发生在 r02 追加测试之前。独立 r01 的 content_recovery 同名程序后来被 r02 重编：旧摘要 `75db6e2963a15a80b08b2a8acea20286b1cc27027277ae73c06fcbc6b29440a9`，当前独立 r02 摘要 `9153bbc933f2abdf832e06203be9c02f010fa933004b67798171513d9a26f5e4`。补充审计明确记录 `old_bytes_verified_now=false`；当前字节只能证明 r02 程序，不能倒推旧字节仍在。主 r02 程序摘要为 `f385937566f0da411d158bac5fcee2f938679a30499f7b2cb55bcfa7ebd5f8c0`。r01 十九步没有在 r02 清单上重跑；r02 已执行的范围限于成对内容恢复的主/独立各一条命令，以及格式和故障 Clippy 两条命令。

## 八条严格验收映射

权威原件 `issue18-authority.json` SHA256 为 `187727c531d2e982ffe2b2461a8c02610a703de9e6bc21b92dab4334f2a83cfb`。硬依赖 #4、#7、#11；无 #15 条件依赖。

| 条件 | 交付与实际证据 | 结果及范围 |
| --- | --- | --- |
| 1 逐行类型、机制、范围、错误与证据 | 本文七行矩阵、I1— I5 原命令与日志，公开 ResourceView | 每行可追到实际执行；三行已实施软件准入/预算，四行明确不支持并验证拒绝，没有硬限制支持声明 |
| 2 正常、超限/等待、耗尽、总额与原归属 | I1 100/80/60/101；I2 并发事务及队首；I3 三代链、4096 输出、4 秒 Device 与 10 秒 Runtime 期限 | 正常完成；超限无 Task；等待不越队；耗尽经原取消、清理与持久终态后释放，业务 unknown 保留 |
| 3 硬保证或执行前拒绝 | I1 包括 Some(0)；TaskResources::validate | 未声明的硬限制不执行；明确要求不支持保证时 SDK/IPC 拒绝，不静默降为观察 |
| 4 取消、宿主/Worker 退出、清理及持久化失败 | I2/I3/I4；r02 主/独立八强杀；原 Task、lease 和声明收据 | 清理或终态持久化失败保留 80 与 holds_slot；重开不靠新 Host 空计数释放；确认原执行者停止后显式恢复；不伪造 CPU/GPU 用量 |
| 5 本地创建、读、改、同 Session 重开 | I6/I7，r02 实际 state、对象字节与只读 SQLite | 不依赖 Server/同步；逐项核正文、归属、version、revision、hash，Device 保持权威 |
| 6 权限、隔离、路径、损坏、冲突与写失败 | I6/I7/I8/I9；四个先写后拒反例及最终 GREEN | 未授权、跨范围、symlink、错误根身份拒绝；缺失/损坏不当空库；冲突不覆盖新版本，持久化未知要求核查 |
| 7 中断分类、不重复、不盲重放 | r02 四窗口×两入口，十六份真实 signal 9 收据，八份只读 continuation | staged 未提交；renamed/synced 已提交但调用 unknown；Core 终态已知可修复；不重放 put；不声称多文件原子或断电安全 |
| 8 SDK/认证 IPC 配置、查询、允许、拒绝、恢复与修复成本 | I1/I6、r02 同窗口成对配置/输入/精确授权、后文失败表和独立审计 | 两入口共享同一 Runtime 路径；大内容帧边界明确；保留失败及返工，无固定毫秒收益断言或性能比较 |

## 失败原件、修复与核查成本

以下是早期源码的实际失败，不是 r01/r02 当前失败。所有原件保留。没有用编译失败冒充行为反例，也没有把文件名含 green 的报告自动当成功。

| 问题 | 原件与实际退出码 | 修复及后续证据 |
| --- | --- | --- |
| 资源公开接口和恢复配置未接齐 | `resource-ipc-red-r01.log` 的 Model::text 编译错误先修为 Model::new；`resource-ipc-green-r01.json` 101 为锁未同步；green-r02 101 为恢复仍走 Queue::open | 两个恢复打开点改为 open_with_resources；green-r03 0；I1、I2 与完整 workspace 再次验证 |
| 内容接口与强杀测试尚未接齐 | `content-runtime-red-r01/r02.json` 均 101；`content-runtime-crash-r01.json` 101；crash-r02 0、r03 101、r04 0 | 修正接口与测试编译问题；读到原父已是已知中断错误后移除多余父结算，不清子调用 unknown；最终完整 r01 与主/独立 r02 验证 |
| 内容目录先初始化后拒绝，真实产品缺陷 | `content/runtime-content-open-rejection-red-a.json/.log` 直接 101，0 passed、4 failed；disabled、配置变化、unsafe mode、inode 变化四场景可在拒绝前创建 .lock/objects/state 或替换 canary | 先核持久 binding，再在同一 nofollow 句柄核权限/dev/ino，再初始化；绑定根缺失不重建；最终 I6 二十边界测试、I7 root_metadata 六项与主 workspace 均通过 |
| 私有打开接口及错误文案 | `private-bound-open-red-r01.json` 101 为缺 open_private；green-r01/r02 均 101，identity differs 文案没有连续匹配 | open/open_private 共用初始化；修正文案，未削弱断言；最终 I6/I7 和 r01 通过 |
| 库提交窗口、锁忙与根身份接口 | `content/commit-windows-red.json`、`nonblocking-red.json`、`root-metadata-red.json` 均 101，各自 green 直接 0 | 实现对应库行为；编译或接口失败与四个真实目录行为 RED 分开记录；最终 I7/I8/I9 复核 |
| 原 MCP 资源回归被实现参与者误覆盖 | 恢复原 resources.rs 后 `resource-regressions-restored-r01.json` 0 | 从 #11 精确恢复原三项；新增四项移到 resource_policy.rs；I1 同时执行三组，9 passed |
| 全量离线 metadata 缺缓存 | `metadata-lock-r02.json` 101，arbitrary 1.5 缓存缺失；r01 no-deps 0 | 最小更新本地依赖锁，保留原 registry 身份；最终 locked/offline 十九步 0，不把 metadata 失败当产品测试失败 |
| 审计路径复用 | `final/primary-audit-alias-failure-r01.json/.log` 1；独立 r01 content_recovery 旧路径被 r02 重编 | offline 找到原 hash 程序后明确 alias 并审计 0；content_recovery 保留两摘要，明确当前未核旧字节 |
| 独立交付缺口与复核纠正 | review-r01 条件 1/8 partial；曾误称输出耗尽只独立测到 Device，已在 corrections 撤回 | I3 原日志证明 Runtime/Host 全链输出耗尽已执行；追加四入口、主/独立各九项后闭合条件 8；本文逐行矩阵补齐条件 1，另作文档独立核对 |

目录缺陷的四个 RED 产生在旧源码，日志 SHA256 `d69d354f61034e640af49c76b4b42c8ebb6125bc5a9ada1ee4b0802e1a122635`；最终 GREEN 使用 r01 清单，不把旧源码失败与新源码通过混成一次结果。修复成本以保留的失败轮次、源补丁、直接命令和报告 UTC 起止体现；没有可靠的人时或公平基准，故不编造工时、加速或成本下降比例。进程测试的注入 Provider panic 是预期反例并核清理，最终测试通过，不称日志完全没有 panic。

## 未验证与使用边界

只证明本机 macOS ARM64 的具名场景。其他 macOS 架构、发布 profile、干净构建、网络文件系统、真实模型与付费请求均未在本项执行。Linux/Windows 不在本次范围；UI、plugin、Server、远程绑定、两机同步/备份和密钥分发不在本项交付。

内容目录必须可信、私有、稳定；锁是建议锁。不能防可直接写目录的恶意同用户、管理员、恶意硬链接、整库回滚/删除或坏盘。摘要用于一致性核查，不是针对这类本机攻击的真实性保证。SIGKILL 不等于断电，fsync 结果不外推为硬件断电安全；对象/state/Core/Cells 没有多文件同时原子可见承诺。

清理确认、内容提交已知、模型账单已知是不同事实。测试不清业务 unknown，不改写已完成原调用，不删除 Host 锁，不扩大权限，不把未测得用量填为零。本记录不替代最终十一项在同源码上的组合核对，也不自动授权提交、推送或关闭 Issue。
