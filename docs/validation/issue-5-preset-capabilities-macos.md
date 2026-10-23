# Issue #5 预置 CodeMode 能力验收

日期：2026-10-21。范围：[Issue #5](https://github.com/changw98ic/CAYW/issues/5) 的八组预置能力及公开 Rust SDK、认证本机 IPC 共用的受控执行链。

本地交付与验收已完成。最终格式、编译、工作区测试、严格 Clippy 和离线示例全部退出 0；独立执行者已复跑关键越权、分页、批次失败与取消对照。本次没有提交、推送、创建 PR、合并或关闭 Issue，远端状态未同步。本记录不表示后续 #6 的真实模型全链或 #7/#8/#9 的扩展验收已完成。

逐能力参数、返回、权限、资源范围、预算、取消、失败和入口见[能力契约矩阵](../issue5-capabilities.md)。下文记录本次实际交付、命令、输入和证据边界。

## 1. 源码、构建与环境

| 项目 | 实际值 |
| --- | --- |
| 交付目录 | `/Volumes/ORICO/cayw-issue5-integration-20261021-a` |
| 分支 | `feat/issue5-preset-capabilities` |
| 基线 HEAD | `d6d8977249acdd8b54a2bba3db49edbb67465651`；本次改动尚未提交，HEAD 本身不能代表交付源码 |
| 冻结 Issue | `/Volumes/ORICO/cayw-issue5-contract-20261021-a.json`，正文 `updatedAt=2026-10-19T14:53:32Z` |
| 最终命令绑定的源码清单 SHA-256 | `be365d6de0f683d8c51531e625b5068c195f21b7978bd73336fdbf1b5699e4d5` |
| Cargo.lock SHA-256 | `b3329220076fc60e7e240096eccf9e279886e33d218cbf27104c20c2e9871060` |
| 系统 | macOS 27.0.1，Build 26A434，Darwin 27.0.0 |
| 实际架构 | arm64，`aarch64-apple-darwin` |
| 工具链 | Rust/Cargo 1.95.0，`1.95.0-aarch64-apple-darwin` |
| Rust 编译器 | `rustc 1.95.0 (59807616e 2026-04-14)` |
| 构建 | debug；`CARGO_INCREMENTAL=0` |
| 主 target | `/Volumes/ORICO/cayw-issue5-target-20261021-a` |
| feature | 最终五条命令使用默认 feature，未指定 `--all-features` 或 `--no-default-features`；Clippy 使用 `--all-targets` |
| 故障测试 | Host 的开发依赖启用 Workspace `fault-injection`；最终工作区日志确实执行 `recovery.rs` 的全部 11 项；另有显式启用该 feature 的两包验证 |
| 真实模型与付费请求 | 0；本项使用固定离线 Provider、本机测试宿主和本机适配器 |

每个最终命令均生成 `.log`、`.json` 和 `-source.json`。逐文件清单包括 Git 跟踪文件、未跟踪交付文件和删除记录，并保存类型、模式、字节数及 SHA-256。五个命令前后的清单都相同，`source_unchanged_during_command=true`。这绑定了本次本地源码和锁文件，不宣称已经追踪每一个外部构建输入；VerifyTool 缺少完整构建资料时继续保持 pending。

最终命令结束后，只更新本记录、能力矩阵、实施计划和路线图。收尾清单与逐项对照保存为仓外 `final-delivery-source.json` 和 `final-delivery.json`；它们核对产品、测试、Cargo 清单及锁文件未变。旧运行清单不包含后来文档的最终内容，不回写旧日志。

本次只增加 Runtime 对锁文件中既有 `base64 0.22.1` 的直接依赖，没有新增注册表版本。该版本采用 MIT OR Apache-2.0，与项目 Apache-2.0 兼容。

## 2. 最终命令与原始日志

证据根目录为 `/Volumes/ORICO/cayw-issue5-evidence-20261021-a`。下表名称均相对该目录。

| 命令 | 记录前缀 | 实际结果 |
| --- | --- | --- |
| `cargo fmt --all -- --check` | `final-fmt-v3` | exit 0 |
| `cargo check --workspace --locked` | `final-check-v2` | exit 0 |
| `cargo test --workspace --locked` | `final-test-v2` | exit 0；221 个顶层目标汇总 1453 passed、0 failed、7 ignored |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | `final-clippy-v2` | exit 0；无 Rust warning/error |
| `cargo run -p cayw-agent --example offline --locked` | `final-offline-v2` | exit 0；工具 `add` 返回 5，模型输出 `2 + 3 = 5`，最后 `5 messages, running=false` |

| 日志 | SHA-256 |
| --- | --- |
| `final-fmt-v3.log` | `e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855`（空输出） |
| `final-check-v2.log` | `46b76ec99e65ec846ff6a4f138faedaf2645519e4c0309511cc8493c6c4f4e31` |
| `final-test-v2.log` | `6f0c7612beca5973bfe3036f0ff21285f6d5c16bb0a98f35f28aa70569b80afd` |
| `final-clippy-v2.log` | `65f01dfe05965ec03c4488d99e36213fb96c5fbd6960ebd1cd5478a30b6ef5b1` |
| `final-offline-v2.log` | `64400b97d772d2746700cfdac8ed375abc457b42a7e97916d84d59496e54faeb` |

顶层总数按每个 Cargo `Running` / `Doc-tests` 目标的最后一条结果统计，包含两项预期编译失败的文档测试。原日志有 224 条结果汇总；其中 3 条是父测试启动的子进程重复输出，合计多计 3 passed 和 36 filtered out。1453 不是互不重复的 Issue #5 场景数，也不把其他 workspace 包的通过当成其所属 Issue 的验收。

7 个默认忽略入口已逐一核对：Cells 的 3 个崩溃/活进程子程序、Data 的 1 个深层输入释放子程序、VerifyTool 的 2 个清理/子进程子程序，由父测试分别启动；另外 1 个是 `cayw-remote/tests/binaries.rs` 的独立二进制示例，本轮没有执行，不属于 #5。没有把这些忽略入口写成单独通过。

测试命令开始时，Cargo 提示 Agent、Content、Runtime 的 `offline` 示例输出文件名相同。测试仍退出 0。随后通过 `-p cayw-agent` 单独编译并运行指定示例，输出与上表一致。本记录保留该提示，不把严格 Clippy 通过描述为所有命令完全没有提示。

## 3. 输入、权限与运行方式

测试中的 Provider 只决定脚本、模型事件和最终回答。执行使用真实 Luau VM、Cells、Host、Workspace/Data、Git、Device、Core 持久记录与本机 TCP；不是用 Provider 的自述替代文件、仓库或进程结果。

每条场景的完整输入、精确授权和预算写在清单绑定的测试源码中。典型授权形状为 `ScopedPermission::exact("fs.read", {"path":"note"})`；能力名和规范化后的完整参数必须相等。文件写入授权还绑定内容和适用的预期摘要。测试分别配置允许、拒绝、过期、撤销、调用数、字节、输出页、时间与进程上限，断言效果发生前后的真实状态。

模型只看到 `execute_code`。exec/wait 自身管理 cell，不为脚本整体授予子能力权限。每次 `cayw.call`、`cells.store`、`cells.load` 均独立验证和记录。未知结果、结果丢失或清理失败继续冻结执行，最终回答不能替它们结算。

纯本地输出、发现和 JSON 计算没有外部变更效果。它们的参数和计算预算失败是已知失败；实际 Host 的记录失败仍按持久执行契约保留未知状态。对有副作用操作，必须区分效果开始前拒绝和效果发生后的无法确认，不能把后者降级为普通失败。

## 4. 八组交付与实际断言

下表所有具名测试均见 `final-test-v2.log`；完整参数与更多对照见能力矩阵。测试源码链接相对本记录。

| 组 | 实际实现与执行证据 | 结果与意义 |
| --- | --- | --- |
| 1. Luau 单元 | [Cells 场景](../../crates/cayw-cells/tests/cells.rs)、[预算与等待](../../crates/cayw-cells/tests/hardening.rs)、[会话值](../../crates/cayw-runtime/tests/values.rs)、[完成核对](../../crates/cayw-runtime/tests/completion.rs) | Cells 全包 90 passed；CodeMode 全包 79 passed；Runtime values 3、completion 6。exec/wait 精确分页；扩大预算与重复 wait 不重复文件效果；活 VM、未读输出、非零 cell exit 不能报完成；store/load 的版本、会话隔离和当前任务授权均有对照；媒体、通知、自省有大小与参数对照 |
| 2. 共享只读 | [原身份与预算](../../crates/cayw-runtime/tests/shared_read.rs)、[原授权重查](../../crates/cayw-runtime/tests/shared_read_authority.rs)、[失效](../../crates/cayw-runtime/tests/shared_read_invalidation.rs)、[恢复](../../crates/cayw-runtime/tests/shared_read_recovery.rs) | 同快照 `fs.read` 合并与复用；每个原调用独立扣预算、授权、记录和确认；相同写入仍推进代际；撤销使缓存失效；原授权不能由新授权顶替；恢复不重新捕获或猜测字节。只有显式登记的 `fs.read` 启用共享 |
| 3. 文件与变更 | [Runtime 文件与批次](../../crates/cayw-runtime/tests/preset_workspace_data.rs)、[Host 补丁](../../crates/cayw-host/tests/host.rs)、[Workspace 恢复](../../crates/cayw-workspace/tests/recovery.rs)、[Workspace 边界](../../crates/cayw-workspace/tests/workspace.rs) | `host_write_patch_tree_and_committed_batch_are_real_runtime_paths` 检查真实字节和调用结果；Host 直接 patch 的缺失文件/无匹配失败不改磁盘且不冻结；语法检查、固定根、不跟随链接、原子单文件替换、批次逐项安装与显式恢复均运行；首次删除后失败保留 unknown，重开不自动继续；不宣称多文件同时原子可见或删除可回滚 |
| 4. 检索与结构 | [Runtime 分派](../../crates/cayw-runtime/tests/preset_workspace_data.rs)、[读取边界](../../crates/cayw-workspace/tests/read_capabilities.rs)、[搜索](../../crates/cayw-workspace/tests/search.rs)、[diff](../../crates/cayw-workspace/tests/code.rs) | 正则/多行、嵌套 ignore、glob、单个 Rust 文件大纲和 diff 块实际通过 Host。越界、非法正则、坏语法、坏 diff、行数/字节上限、预取消与扫描中取消都有已知失败对照；跨文件语义索引仍由宿主持有 |
| 5. 数据 | [数据能力](../../crates/cayw-data/tests/capabilities.rs)、[数据边界](../../crates/cayw-data/tests/boundaries.rs)、[归档](../../crates/cayw-workspace/tests/archives.rs)、[Runtime 组合](../../crates/cayw-runtime/tests/preset_workspace_data.rs) | Data 全包 48 passed、1 ignored；Workspace 全包 80 passed。五种格式、查询、转换、diff、两种摘要、Base64/Hex 与 ZIP/TAR 只读实际运行。未知算法/输入种类、多余字段、深度/节点/输出、预取消、diff 运行中取消及恶意归档路径/条目/膨胀限制均有对照 |
| 6. Git | [Host Git](../../crates/cayw-host/tests/git.rs)、[私有快照](../../crates/cayw-host/tests/git_snapshot.rs)、[Runtime Git](../../crates/cayw-runtime/tests/git.rs) | Host git 12、git_snapshot 10，Runtime git 1。真实合成仓库核对 HEAD/index tree/分支/工作树注册；拒绝越界路径、失配摘要、脏工作树、外部 gitdir、危险配置；hooks/filter/凭据与网络旁路不执行；写入中断保留 unknown，不自动重试 |
| 7. 工具链与进程 | [固定 profile](../../crates/cayw-runtime/tests/toolchain.rs)、[Host 终端](../../crates/cayw-host/tests/terminal.rs)、[Runtime 终端](../../crates/cayw-runtime/tests/terminal.rs)、[Device macOS](../../crates/cayw-device/tests/macos_lifecycle.rs) | Device 93 passed，Host terminal 14、Runtime terminal 4、toolchain 5。五种宿主 profile、精确授权、拒绝程序/argv/env 注入、真实 PID 回收、Pipes/PTY、幂等输入、连续原始输出、寿命预算与原 supervisor join 均运行；未确认清理保留原身份和名额 |
| 8. 人机与宿主 | [协作](../../crates/cayw-runtime/tests/collaboration.rs)、[认证 IPC](../../crates/cayw-runtime/tests/preset_ipc.rs)、[图像输入](../../crates/cayw-runtime/tests/input.rs)、[本机子任务](../../crates/cayw-runtime/tests/children.rs)、[资源](../../crates/cayw-bridge/tests/resources.rs) | collaboration 9、baseline 1、preset_ipc 4、input 7、children 10、Bridge resources 8、Runtime resources 3。问题只能由宿主答复且不产生授权；计划按版本更新；图像入队前校验；真实子 Session/Task 共享受限预算并独立授权；资源固定来源、URI/MIME 与原 dispatch，取消/断连/迟到结果保留边界 |

### 关键执行差异

- Host 等待与 VM 计算分别计时。默认计算 5 秒，单次 `host_wait` 60 秒。`two_host_waits_do_not_spend_the_vm_calculation_budget`、`calculation_clock_resumes_after_a_host_reply`、`store_wait_is_paused_and_bounded_by_host_wait_not_vm_timeout` 与 Runtime 三项 `host_wait` 测试已执行。等待回答不消耗 VM 计算时间，但不取消 Host 或 Runtime 原绝对时限。
- Runtime 总时间在等待 `user.ask` 时到期，精确结果为 `Err(Budget("time"))`、Task Failed、`cancel_requested=false`，并保留 `task exceeded its time budget`。该无外部效果场景不能用来推断写入或进程超时没有 unknown。
- `final_response_cannot_complete_a_yielded_vm`、`final_response_cannot_discard_unread_output`、`skipping_an_oversized_first_item_cannot_hide_unread_output` 与重复 wait 对照按每个实际交付 cursor 核对，不仅记录最大 cursor。
- `invalid_arguments_escape_and_tree_bound_fail_before_effect` 的三个场景各保留一条已结算能力记录并报告已知失败。它们证明零文件效果，不能写成零 Core 记录。非法目录 schema 在更早阶段被拒绝的其他测试另有零调用断言。
- `recursive_remove_failure_and_cancellation_keep_partial_effects_unknown` 使用开发 feature，在第一次删除前/后分别注入错误和取消。删除前文件完整且失败已知；删除后保留已删条目和 unknown；重开后不继续删除。注入入口不在模型目录中，普通构建为 no-op。
- `unconfirmed_child_cleanup_keeps_the_handle_slot_and_never_claims_finished` 与 Device 清理失败场景保留原句柄和容量。进程已经停止并不自动证明业务效果已知。

## 5. 独立关键复跑

独立执行者使用不同 target，在 macOS arm64 上实际执行：

```sh
RUSTUP_TOOLCHAIN=1.95.0-aarch64-apple-darwin \
CARGO_INCREMENTAL=0 \
CARGO_TARGET_DIR=/Volumes/ORICO/cayw-issue5-target-independent-20261021-a \
cargo test -p cayw-runtime --test completion --test preset_workspace_data --test toolchain --locked
```

原始日志：`/Volumes/ORICO/cayw-issue5-evidence-20261021-a/issue5-independent-rerun-20261021T102230Z.log`。

结果为 completion 6/6、preset_workspace_data 13/13、toolchain 5/5，共 24 passed、0 failed。实际检查越权前零效果、跳过输出游标不能隐藏未读项、扩大预算和重复 wait 不重写、批次中断恢复/第三方阻断、运行中取消和原 PID 回收。

该日志保留首轮 Cargo 输出。Cargo 完成后，zsh 包装器使用保留变量名 `status` 失败。执行者追加环境与源码摘要，并用同一命令再次执行得到 Cargo exit 0；没有修改或删除首轮输出。此包装器错误不冒充 Cargo 失败，也不隐藏。

独立日志记录三个测试文件及完成检查、执行、进程归属等源码 SHA-256。该轮早于后续 Git 布尔配置修复；它证明上述独立场景，不证明当时尚未加入的 parser 修改。最终 E-FT 已另外验证后续 Git 修复和最新完整树，不给独立复跑追加所有文件必须一致的新门槛。

## 6. 历史失败与修复证据

旧日志保留在原目录。以下为实际解决的失败，不修改旧日志使其变绿。

| 原失败 | 根因与改动 | 后续实际证据 |
| --- | --- | --- |
| Cells 小 `item_bytes` 初始化失败 | 静态会话值目录被错误计入单项输出上限。目录初始化保留结构/深度校验，实际 Definition/Search 返回继续受单项上限限制 | `cells-hardening-repair-20261021T105546Z/cayw-cells-full.log`（位于 `/Volumes/ORICO/issue5-evidence`），90 passed；E-FT 再次通过，含 `small_item_budget_initializes_and_still_bounds_introspection_results` |
| 旧 Host 等待测试超出测试看门狗 | 旧测试仍把计算 timeout 当作 Host 等待上限。按新契约显式设置 host_wait，核对原调用启动、取消被观察、宽限后仍未结算及持久 unknown | 同上；`host_wait_bounds_started_host_hang_and_records_unknown` 通过；没有改回旧计时行为 |
| VerifyTool 找不到已完成任务 | 产品已经统一 `TaskInput`，验证器仍读 `task.text`。改为核对 `task.input.text` 和空 images；身份、页、cursor、终态核对不放宽 | `verify-tool-sdk-validation-green.log`、`verify-tool-lib-green.log`、`verify-tool-sdk-cli-green.log`；E-FT 完整重跑 |
| Git 显式空 boolean 被当作 true | 区分隐式键与显式空值：无等号为 true，`key=` 为 false | `git-bool-host-suite.log` 150 passed；E-FT 的 git_snapshot 10 项通过 |
| 新递归删除故障测试最初返回完整删除成功 | 现有删除计数能区分部分效果，但新增故障点尚未接入循环。将开发故障点接到真实删除前/后 | `remove-partial-red-v1.log` 保留；`remove-partial-green-v1.log` 通过；E-FT recovery 11 项通过。不是修复了原有 removed>0 判断 |
| 最终格式检查 v2 失败 | recovery 测试一处换行不符合 rustfmt | `final-fmt-apply-v2` 应用格式；`final-fmt-v3` 检查通过 |
| 最早组合编译读取旧 `task.text` | 两个 VerifyTool 消费者未随 TaskInput 迁移 | `final-check-v1` 保留；`final-check-v2` 通过，最终测试与 Clippy 也通过 |

Workspace/Data 显式故障 feature 两包验证另存 `/Volumes/ORICO/cayw-issue5-evidence-20261021-b/workspace-data-final-20261021T110521Z.log`：128 passed、0 failed、1 ignored，且两包 `--all-targets -- -D warnings` 通过。它与最终 E-FT 分别绑定自己的轮次，不合成为额外的去重测试数量。

## 7. 复跑入口与保留边界

在交付目录执行：

```sh
export PATH="/Users/chengwen/.rustup/toolchains/1.95.0-aarch64-apple-darwin/bin:$PATH"
export RUSTUP_TOOLCHAIN=1.95.0-aarch64-apple-darwin
export CARGO_INCREMENTAL=0
export CARGO_TARGET_DIR=/Volumes/ORICO/cayw-issue5-target-20261021-a

cargo fmt --all -- --check
cargo check --workspace --locked
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo run -p cayw-agent --example offline --locked
```

显式执行文件/Data 故障对照：

```sh
cargo test -p cayw-workspace -p cayw-data --features cayw-workspace/fault-injection --locked
```

证据记录器 `/Volumes/ORICO/cayw-issue5-evidence-20261021-a/validate.py` 为每条命令接收新 label，拒绝覆盖已有证据。复跑生成新文件，不删除旧 target 或清理原工作区。

- 本轮没有真实模型调用。固定离线 Provider 足以验证 Issue #5 的执行契约；它不提供模型质量、真实用量或 #6 验收结论。
- 图像和音频校验 MIME、规范 Base64、字节大小及原 cell/任务归属，没有完整媒体解码器。认证 IPC 组合使用完整 1×1 PNG；部分编码边界测试使用任意字节，不能当作真实媒体格式解码通过。
- 五类工具链 profile 的成功对照实际运行 `/usr/bin/printenv`，验证宿主固定程序、参数、环境与完整分派；非零对照运行 `/usr/bin/false`，取消对照运行本机进程 fixture。不声称模型任务已经逐个执行 Cargo 五种命令。项目自身的 Cargo 检查见第 2 节。
- `capabilities.list` 有真实认证 TCP 对照。`session.grants`、`session.reconcile`、`capability.reconcile` 是对 Rust SDK 的薄映射，没有各自独立 IPC 专项；权限与核对行为由真实 SDK 场景覆盖。`tests/input.rs` 中直接 RpcHandler 调用与 `preset_ipc.rs` 的真实认证 TCP 分别记录。
- MCP/资源桥使用宿主注入的真实本机连接适配器和固定 fixture，不代替后续 #9 对真实来源与连接生命周期的完整验收。Core 不新增连接、Skills loader 或持久代码索引。
- SHA-256/reftable/sparse 等当前 Git 快照无法正确表示的布局明确拒绝；不把支持完整对象 ID 参数写成支持所有仓库布局。
- 只声明实际执行的 macOS arm64 场景；macOS 其他架构未验证。不把本次结果外推到 Windows/Linux。
- 仍保留未知效果、原执行者、清理确认和执行名额边界。不自动重跑丢失 VM，不因最终回答、取消或重开把 unknown 改成成功。
