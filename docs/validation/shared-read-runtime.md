# Shared read Runtime validation

本记录覆盖 `2026-10-19-shared-read-runtime-integration.md` 的 Host、Cells、Runtime 接线。两次独立获准的 `fs.read` 可以使用同一次文件采集；每次调用仍有原 Core 计划、权限、Host 开始及终态、Cells 依赖及终态，并分别扣除预算。所有调用都经过原 `CellBackend::call`。本记录不代表 Issue #5 结项。

最终固定源码的摘要、完整命令结果和工作区回归结果已在下方绑定。原始失败日志和修复后的日志分别保留，不以旧日志代替当前源码验收。

## 运行范围

- 开发机为 macOS 27.0.1 / arm64，Rust 和 Cargo 使用 `+1.95.0`。
- Cargo 调用使用 `--offline --locked`；构建目录与 `TMPDIR` 位于 `/Volumes/ORICO`。仅运行本地固定 Provider、真实 Luau/Cells、SQLite、文件和本机进程测试，没有真实模型、付费服务或凭据调用。
- 未增加依赖，未修改根 `Cargo.toml` 或 `Cargo.lock`。本实现者未修改 Core、Device、Workspace 或 Harness 的生产及测试文件。最终副本可包含这些包各自所有者已完成的修复；文件摘要在固定源码清单中逐项记录。
- Windows 和 Linux 未运行本轮验证。macOS 通过不能外推为三平台通过。

## 实际执行与记录

`Runtime Backend::call` 固定完整原 `CellExecution`、task、attempt/worker/fence、parent、source hash、精确参数、普通 GrantRecord 和 Request receipt。Cells 序号从 1 开始，Core 从 0 开始，转换检查下溢。

Host 的普通调用和冻结读取票据共用参数验证、权限、开始审计、预算及终态流程。冻结票据不占用普通执行队列，避免写入等待读取锁时反过来挡住读取收尾。票据持有工作区共享锁，直到本次 Host/Core 终态和原 Cells 依赖及终态提交后才释放。协调器的首次读取结果也到这个确认点后才对等待者发布。确认拒绝走已提交失败的完成流程；丢弃或未知结果继续冻结后续工作。

`.cayw-host-read-fence` 的文件身份保持稳定。独立替换的状态文件记录实际目录身份、incarnation、generation 和待核对的原写入。每次协作写入在派发前持久化新 generation，即使内容相同也增加代次。文件及 Workspace 修改、预设进程、未分类的外部/模块服务走此边界。未分类服务和进程还会永久关闭该根目录后续复用，后续获准读取以 `uncached` 执行。

`runtime_shared_read` 记录原调用、完整租约、授权、原 receipt、采集来源、角色、stamp、依赖和结果摘要，在本次 Core 终态前提交。恢复逐项核对原 Core 事件与授权，不重新选择新 grant，不读取当前文件补猜旧结果，不调用 Provider 或重新执行能力。原父工具结果未知时仍保持 `NeedsReconciliation`，只有可信宿主显式核对才能继续。

## 覆盖的实际行为

| 用例 | 观察与含义 |
| --- | --- |
| 两个独立 Luau Cell 等待同一实际 OS 锁，各自 AllowOnce | 两个原 Core Started 均先存在，释放锁后两个独立终态、audit 和依赖对应一次 leader 采集。第二个角色随调度为 follower 或 ready；这个 Runtime 用例不强制声称 follower。 |
| Cells 协调器中的确定性 follower | 原 leader 的 Cells 确认前无法取得结果；取消一个 waiter 不影响 leader。leader 取消/丢弃唤醒等待者，不提升等待者、不重试、不留下后台采集。 |
| 禁止合并、允许以后复用的策略 | 第二个同键读取独立 uncached 执行，不能替换尚未确认的原 leader。其后 ready 来源仍为首次读取。 |
| 不同 Cell/parent/序号跨独立 Runtime 写入 | `shared_read_generations` 同时保留两个 Cell 的不同原序号；独立连接的相同内容写入令 generation 增加，后续重新采集。`shared_read` 另以不同数据库验证相同根目录的代次一致性。 |
| 预算 | ready 结果分别消费 Host 和 Runtime 调用、参数及返回字节预算。复用不会赠送调用或响应额度。 |
| 普通授权等待与拒绝 | 原 AllowOnce/AllowTask 过期或撤销时，即使插入一个新的同范围 grant，等待中的原读取仍失败，不能替换授权。已准备 ready 值后的已知拒绝提交自己的失败，后续独立授权读取仍可使用合法原采集。 |
| Request receipt | 两次 request 工具调用只触发评估，审批由可信 SDK 提供。保护读取保留精确原 receipt；在实际文件锁等待中自然过期或撤销后不交付字节。此路径因 request 工具使用未分类 Host service，当前明确为 uncached。 |
| 工作区路径被移走或替换 | 文件读取、能力目录和 Request 审批继续使用已打开目录；替换旧路径不能切换根目录。链接、特殊文件、无效 UTF-8、超长文件和伪造内容摘要被拒绝。 |
| 调用取消与丢弃 | Runtime 两个原读取等待实际锁时取消或丢弃 prompt，原调用停止，零采集事件；重新打开和恢复不会调用替代 Provider。 |
| Host 审计失败或中断 | 活票据继续挡住写入；丢弃后保留 exact 原 read fault，其他连接拒绝继续派发。原 Host 的永久停止不会被恢复清除。 |
| Core 开始、采集证据或终态写入失败 | 实际 SQLite trigger 拒绝对应写入；未把没有完成的读取记作成功，父调用继续待核对；删除当前文件后恢复仍不重读或重放。 |
| Cells 原成功写入失败 | Core 已知成功保留；票据及候选值不会提前确认。重开后精确补回原 Cells 依赖及结果；当前文件已删除或被外部改变，修复仍使用原记录。父调用保持未知，另一用例再以显式宿主核对解除它。 |
| 改动恢复证据 | 固定 append-only Core SDK fixture 改动租约、task、parent、source、序号、参数、授权、依赖、stamp、结果摘要、receipt、角色或完整原 GrantRecord；恢复在改变标准历史前拒绝。该 fixture 没有 VM、采集或真实队列，不将它描述成真实中断验收。 |
| generation 和故障记录 | 非十六进制摘要、超长标识、计数耗尽、错误或链接故障记录均拒绝。选定状态替换窗口保留原 mutation；实际 read fault `create_new` 失败时独立完整原记录可恢复 exact slot。 |

## 故障证据及修复

以下均为本地原始日志，失败和后续通过分开保留。

- `/Volumes/ORICO/cayw-shared-read-red.log`：实际写入曾越过活读取锁；Host 统一代次锁修复后由独立 Host/Runtime 写入测试覆盖。
- `/Volumes/ORICO/cayw-shared-read-runtime-root-red.log`：根目录改名后能力目录曾报 `No such file or directory`。Runtime 的目录、Request 操作及审批改用原 held Host，后续用例通过。
- `/Volumes/ORICO/cayw-shared-read-corrupt-generation-red.log`：非十六进制 incarnation 曾被接受；现在严格验证摘要形状和原标识边界。
- `/Volumes/ORICO/cayw-shared-read-grant-evidence-red-2.log`：恢复曾接受改变的过期时间。现在记录并比较完整原 GrantRecord；有效形状但不同的过期时间或模式也拒绝。
- `/Volumes/ORICO/cayw-shared-read-visible-state-red.log` 与 `/Volumes/ORICO/cayw-shared-read-failed-storage-red.log`：选定状态可见窗口曾丢失原 pending，或 read fault 创建失败后只剩不可恢复记录。完整原 Mutation/Read marker 保留 exact 身份，后续 `/Volumes/ORICO/cayw-shared-read-storage-fixed-2.log` 通过。
- `/Volumes/ORICO/cayw-shared-read-storage-fixed.log`：一个测试误用已经观察未知的 Host，产生原有永久停止。修正测试保留该断言，在核对后新建 Host；没有放宽生产停止规则。
- `/Volumes/ORICO/cayw-shared-read-known-rejection-red.log`：新增 `ReadCandidate::reject` 接口前的编译失败；这不是一次运行时行为失败。其生命周期和 Runtime ready 拒绝用例均已执行通过。
- `/Volumes/ORICO/cayw-shared-read-noncoalescing-red.log`：第二个同键调用曾为 Leader，期望 Uncached。当前协调器禁止替换已有键；`/Volumes/ORICO/cayw-shared-read-noncoalescing-fixed.log` 通过。
- `/Volumes/ORICO/cayw-shared-read-clippy-1.log`：测试持有互斥锁跨 await；缩小测试锁的代码块，后续严格检查通过。
- `/Volumes/ORICO/cayw-shared-read-final-owned-live/tests.log`：清理重复调用遇到短暂 `Busy`。测试只对明确允许的所有权占用作有界重试，保留原两次 drain、仍占容量和唯一 `processes_stopped` 断言；后续完整三包回归通过。

早期错误 fixture 曾尝试修改 append-only Core 事件或在已结束租约上追加事件；修正为原 SDK 的固定事件输入，没有弱化 Core 约束。上述测试记录不将合成存储窗口称为真实磁盘断电或 fsync 故障注入。

## 公共 Rust 接口变化

- `CellBackend::call(&CellExecution, sequence, name, arguments, cancel)` 现在返回 `Result<BackendResponse, CapabilityError>`。`BackendResponse::owned` 持有结果、可选 `DependencySnapshot` 和非 Clone 的 `BackendCompletion`。`acknowledge(self)` 只在原 Cells 提交之后发布/释放资源，不承担尚未完成的持久化写入。
- 旧 `ReadBackend` / `ReadAuthorization` 及注册读取旁路已移除。读取通过显式 `ReadPolicy`、每次独立 `ReadSource`、`ReadOrigin { execution, sequence }` 和 `ReadStamp` 进入原协调器。没有保留旧兼容执行路径。
- `ReadCandidate::acknowledge(self)` 确认成功；`reject(self, reason)` 仅完成已经提交的已知拒绝。丢弃仍冻结。
- `HostCallSettlement` 新增精确可选 `dependencies`。`StoppedScope` 保持原执行者所有权，修复不能改写已经确认的结果和依赖。
- `Host::task_host(gate, audit)` 从原目录句柄派生任务状态。`Host::admit_frozen_read` 返回 `FrozenReadTicket`，顺序为 capture/accept、settle、原 Cells 提交、acknowledge。
- `Host::pending_mutation` / `pending_read_faults` 与 `recover_mutation(FenceRecovery, cancel)` / `recover_read_fault(ReadFenceRecovery, cancel)` 是可信宿主接口，核对原记录、停止者、Device 清理和标准终态；模型没有 clear/reset 能力。

现有 Cells 测试构造器已迁移。工作区编译用于发现真实 Rust 调用者，不为旧接口保留另一路实现。根锁与任何新依赖不属于这次变更。

## 固定源码与最终命令

最终验收输入为 `/Volumes/ORICO/cayw-shared-read-source-20261019-final-v3/source`，于 `2026-10-19T00:08:09.226747+00:00` 复制冻结。`closure.json` 位于其父目录，记录 20 个工作区包、原根 `Cargo.toml` / `Cargo.lock`、第三方许可说明及选定文档，共 394 个文件、4,874,007 字节。每个文件记录大小和 SHA-256；按路径排序并以无额外空白的 JSON 编码这些记录后，清单摘要为 `712334fa3b8443cfa43c5594e9c991e6a21c387d7bc722a859a3aa77dca6fc82`。复制前、复制后及副本三份摘要相同，`before-verification.json` 在 `00:08:18.153238+00:00` 再次确认与 live 源码一致。

该副本排除 `target`、`.git`、`.local`、`.playwright-mcp`、`.runs`、`.validation`、`.diagnostic*` 及工具配置目录。初次 v2 封存复核发现遗漏排除包内浏览器/测量产物，已停止其验证并保留 `/Volumes/ORICO/cayw-shared-read-source-20261019-final-v2/validation-status.json`；不采用其任何结果。首版 `/Volumes/ORICO/cayw-shared-read-source-20261019/source` 和既有 `/Volumes/ORICO/cayw-shared-read-source-20261019-final/source` 也保持原样，均不作为本轮最终 Verify 输入。

固定输入中的 Core 两份路径修复由 Main 完成；`independent_edge_probe.rs` 的摘要为 `73d0871c22b2573381e5e03247e2c72f642c82aed0153764ca6de7732c53ef44`，`independent_probe.rs` 为 `dc16990eac861b664f5f1af75739f9a6285b2ab53bf3ca9d3b2ead5b7cd6011d`。复制前已逐一核对 Main 的稳定记录。根清单摘要 `76baed65b2c40b24ff5d92f2830ab88daceb2a95de1a4ffc86addcf9080d8874`、根锁摘要 `46e0440091aafa15a621795ccfd2560d2a2ab53baa3fb0c1044bad79be1365b3`、Workspace `code.rs` 摘要 `a9fde84f710e7ee4346f9592d852efd6f399e401f0e357c44ed523f9c483458e` 和原 `approvals.rs` 摘要 `8b82ed1d2f92dbf7d9eb0a9952e315bfd3717fdf0ff94562ab9d609603dac6a0` 均保持不变。

从该固定副本运行的三包及完整工作区结果、UTC 起止、逐条 argv 和原始日志分别写入父目录的 `owned/` 与 `workspace/`，每一组以 `results.json` 和 `toolchain.txt` 记录环境与退出码。独立构建目录为 `/Volumes/ORICO/cayw-shared-read-fixed-v3-target`，`TMPDIR=/Volumes/ORICO`，并显式设置 `CAYW_SOURCE_MANIFEST_SHA256` 为上面的清单摘要。该构建目录在本轮之前未使用。Cargo 使用 `+1.95.0 --offline --locked`；第三方包来自本机已有 Cargo 离线缓存，副本没有复制用户 Cargo 配置。

本验证记录在固定源码之外单独封存，以免把引用源码摘要的文档纳入同一个摘要而形成循环。`owned/results.json` 记录固定副本验证从 `2026-10-19T00:08:29.137517+00:00` 到 `00:10:09.480758+00:00` 完成，fmt、all-targets/all-features check、tests 和严格 Clippy 四项退出码均为 0。`owned/test-counts.json` 汇总 Cells 80、Host 113、Runtime 131，共 324 通过、0 失败、3 个 ignored 子进程入口；这些入口由通过的父测试显式调用，不另重复计数。实现者已读取整份 894 行测试原始日志。`after-owned-verification.json` 在 `00:10:41.637560+00:00` 确认副本未变，live 清单仍逐项一致。

同一固定副本的完整工作区验证从 `00:10:23.701238+00:00` 到 `00:13:23.246736+00:00` 完成。`workspace/results.json` 中的五条命令全部退出 0：

```sh
cargo +1.95.0 fmt --all -- --check
cargo +1.95.0 check --workspace --all-targets --all-features --offline --locked
cargo +1.95.0 test --workspace --all-targets --all-features --offline --locked
cargo +1.95.0 test --workspace --doc --all-features --offline --locked
cargo +1.95.0 clippy --workspace --all-targets --all-features --offline --locked -- -D warnings
```

`workspace/test-counts.json` 以每个 Cargo 测试目标的最终父进程摘要计数，共 1,152 通过、0 失败、5 ignored；另有 `doc-test-counts.json` 中 2 项文档测试通过。Core 为 106、Workspace 为 75，子进程打印的 Core 1 次及 Workspace 2 次成功摘要不重复计入；初次包含这些子进程摘要的 1,155 计数保留为 `test-counts-including-child-summaries.json`，已经废弃。Cells 的 3 个和 Data 的 1 个 ignored 入口由通过的父测试显式调用；Remote 的 1 个二进制专项集成测试仍未运行，不能据此声称已覆盖。

已逐段读取完整工作区测试日志 2,370 行及其他检查日志。`after-workspace-verification.json` 于 `2026-10-19T00:15:31.995587+00:00` 再次核对，固定副本未变化，live 的同一清单逐项匹配，清单摘要仍为 `712334fa3b8443cfa43c5594e9c991e6a21c387d7bc722a859a3aa77dca6fc82`。`cargo-target-closure.json` 另外核对 20 个包的所有本地依赖根和 Cargo 声明目标均在固定清单内。工作区编译没有要求迁移范围外的 Rust 调用者。

实现者回归只采用本节所列 `final-v3/owned/`、`final-v3/workspace/` 的命令与日志；下节的独立复核有自己的命令和原始日志。live 脚本以及其他副本的自动完成通知不替代它们。副本中的少量既有包级日志按原字节保留，但不计为本轮命令证据。本轮没有执行性能吞吐或浏览器测试，旧网页记录也不能证明最新组合已完成浏览器验收。

## 最终 v3 副本的独立复核

独立复核于 `2026-10-19T00:46:45.861542+00:00` 封存，唯一源码是上述 `final-v3/source`。运行前后均逐项核对 394 个文件、4,874,007 字节和清单摘要 `712334fa…`，没有源码或作者证据变化。旧 `final/source` 的复核单独保留，不计入本节结果。独立入口为 `/Volumes/ORICO/cayw-sr-v3-verify-20261019-codex/final-evidence.json`，文件 SHA-256 为 `d39687f4c29db863a18ea550884be70e3da96b85814adc971c50efefa3741b37`；`commands.jsonl` 保存完整命令、环境、UTC、退出码和日志摘要，`integrity-before.json` 与 `integrity-after.json` 保存前后核对。

| 独立实际运行 | 通过 / 失败 / 忽略 |
| --- | --- |
| Host：`shared_read`、`read_fence_hardening` | 23 / 0 / 0 |
| Cells：`shared_read_lifecycle` | 11 / 0 / 0 |
| Runtime：6 个共享读取目标及 `process_cleanup`，全部 feature | 28 / 0 / 0 |
| 新编写的独立探针 | 14 / 0 / 0 |

前三行共 62 项原始定向测试，第四行为另写的 14 项，不与实现者完整工作区的 1,152 项重复相加。四条 Cargo 命令实际退出码均为 0。构建使用 Rust 1.95.0、独立 `target`、短临时目录 `/Volumes/ORICO/c3v-oct19`、`--offline --locked`，并传入上述固定源码摘要。独立复核没有重跑完整工作区。

独立探针验证了原 Backend 及 Cells 提交后才确认结果、每次调用的原授权与审计、禁止合并时不替换原 leader、独立 Host 写入相同字节仍增加代次。通过第二个真实 RequestStore 连接申请并批准替代授权后，旧等待调用仍因原 receipt 失效而拒绝，新调用才可使用新 receipt。原 Cells 写入失败时，已知 Core 成功和执行容量保留；删除源文件并重开后使用保存的结果恢复，没有重新读取或调用 Provider。九种错误的原身份、授权和结果证据在改写标准历史前被拒绝。

针对原 `process_cleanup` 测试的 Busy 重试，额外探针用真实 leader/leaf 连续运行 8 轮。明确持有原任务 OS 锁时，Busy 不改变审计、Core 或容量；释放锁后直接调用两次 `drain_task_processes`，没有重试循环，两次均成功且只保存一次原 worker 的 `processes_stopped`。drain 后仍占容量，显式 recover 才释放。该结果未显示原有界重试掩盖问题，但不外推到所有调度时序；合成的待清理记录与此真实进程探针分别标明。

首次 Host 验证因测试创建 Unix socket 的临时路径太长而退出 101（22 通过、1 失败），保留 `logs/focused-0.log`；只缩短独立环境的 TMPDIR 后，原测试 23 项通过，未改原测试。新增替代 Request 探针首次缺少 trait import，编译退出 101，保留 `logs/probes-all-attempt-2.log`；仅修独立探针后，14 项通过。它们没有计为生产缺陷或通过结果。独立锁文件与原锁的共同包 checksum 相同，另有 Windows 专用 `windows-sys 0.59.0`，本次 macOS 不覆盖该依赖的运行。

Main 逐项重新核对了 394 个固定源码、49 份作者证据和 25 份独立封存文件，并从四份原始日志重新计数，均与记录相符。该核对及本文修改前副本保存在 `/Volumes/ORICO/cayw-shared-read-doc-review-20261019-s1bf7lhy/before.json`；它是已有证据核对，不冒充 Main 又重跑了一遍测试。本文修改在固定副本之外记录，原副本与封存保持不变。

## 限制

复用是显式启用的协作 Host generation 冻结视图。任意外部编辑器或不遵守 Host 锁的进程可能改变文件；本实现不自动检测这种编辑，也不以 size/mtime 冒充内容快照。这不是全工作区事务快照。

未知父调用、未知副作用或未确认 Device 清理继续阻止恢复后的执行。恢复不自动重试、不提升 follower、不重新打开当前内容猜旧答案。Request 工具和未分类外部服务当前保守禁用复用，授权的 uncached 读取仍可执行。

已覆盖选定审计/SQLite 写入错误、选定 generation 可见窗口及实际 read fault 创建失败。没有穷举所有文件系统持久化错误、同时多个 fallback 写入失败或主记录与 fallback 同时完全不可写的情形；所有持久化都失败时仅能确认进程内永久停止，不能声称磁盘上一定保留恢复证据。

独立复核覆盖上述最终 v3 的 macOS 离线定向范围，没有发现该范围内的生产缺陷；它不覆盖后续 Device 交互进程、Modules 迁移、长会话性能、最新网页组合、断电或 fsync 穷举。真实 Provider 全链、Windows/Linux 和 Issue #5 其余验收仍未完成。
