# Issue #1：公开 Rust SDK 与 IPC 验收记录

2026-10-19，独立 Rust 客户端、Session/事件契约和跨运行实例的取消修复已接入主工作区。**按当前仅 macOS 的首发范围，Issue #1 的本地功能验收完成，执行证据覆盖 macOS ARM64 离线环境。** 定向验证通过 284 项相关产品测试、2 项客户端测试和六个 CLI 场景；随后完整 workspace 测试通过 1,185 项、失败 0 项、忽略 5 个入口，另有 7 项启用全部 Runtime feature 的模块生命周期测试通过。严格 Clippy、20 包所有目标/feature 编译及离线示例通过。这几组测试有重叠，不相加。原先“取消已接受但任务记录为完成”的失败已保留，并由固定执行顺序的回归测试覆盖。

权威任务：[GitHub Issue #1](https://github.com/changw98ic/CAYW/issues/1)。公开接口和行为见 [SDK/IPC 契约](../issue1-sdk-ipc-contract.md)，独立客户端和复跑命令见 [issue1-client](../../examples/issue1-client/README.md)。2026-10-19 按用户要求关闭 #1，并新建 [#19：Linux 与 Windows 支持及原生验收](https://github.com/changw98ic/CAYW/issues/19)。[关闭评论](https://github.com/changw98ic/CAYW/issues/1#issuecomment-6262365157) 已说明验收范围，以及关闭当时的代码尚未提交或推送；未替换原有评论或改写任务正文、勾选项。用户随后明确要求提交并推送，本文、产品实现和独立客户端一同交付，实际提交身份及远端位置以 Git 历史为准。此次 GitHub 操作及文档差异另存于 `/Volumes/ORICO/cayw-issue1-close-issue19-20261019-5la73iu2/`，不改写此前测试证据。

## 当前首发范围

2026-10-19 用户明确要求：“不需要处理 windows  首发为 macos  linux 都不需要考虑”。因此当前只实现和验收 macOS，Windows/Linux 的适配、探针与环境排查停止，不再作为 #1 或首发交付的前置条件。下文原平台研究保留为历史记录，不转写为通过，也不要求在首发前补齐。当前执行证据只覆盖 macOS ARM64，不能外推到其他 macOS 架构。

已核对 Issue #1 的两项具体交付和三项可执行验收：公开 SDK/IPC 客户端、行为契约、真实本机请求与执行中事件订阅/取消、版本错误、保存游标续读、结果配对、审批边界、Session/Worker 分离及持久化恢复均有上次独立复核和主树复跑证据。按新平台范围，#1 本地验收完成。模型回复使用固定数据，SQLite、TCP、Host 文件读写及进程退出使用真实路径；持久化 Runtime 的真实模型全链归 #6，不因 #1 完成而声称已验收。后文完整 workspace 的其他可选场景缺口仍按各自范围保留。

首发范围调整时只更新了交付状态，没有修改产品代码或重新运行产品测试。该次六份说明文档的修改前备份、精确差异及原测试输入核对保存在 `/Volumes/ORICO/cayw-macos-launch-scope-20261019-wr04nyi1/`，之前的测试源码、日志与封存记录保持原样。随后为实际提交并推送，另从 Git 暂存区导出源码并完成下节的新验证。

## 实际交付

独立客户端是根目录之外的嵌套 Cargo workspace，有自己的锁文件，通过公开 SDK 和认证的本机 TCP IPC 消费同一 Runtime 实现。原六文件交付为：

```text
docs/issue1-sdk-ipc-contract.md
examples/issue1-client/Cargo.toml
examples/issue1-client/Cargo.lock
examples/issue1-client/README.md
examples/issue1-client/src/lib.rs
examples/issue1-client/src/main.rs
```

随后取消修复修改了以下八个路径，其中 `cancel_terminal.rs` 为新增文件，其余为更新：

```text
crates/cayw-runtime/src/execution.rs
crates/cayw-runtime/src/lib.rs
crates/cayw-runtime/src/queue.rs
crates/cayw-runtime/src/requests.rs
crates/cayw-runtime/tests/cancel_terminal.rs
docs/issue1-sdk-ipc-contract.md
examples/issue1-client/README.md
examples/issue1-client/src/main.rs
```

取消与完成在写入持久任务终态时判断先后。取消先提交时，已知结果的任务、唯一审计记录和 Requests 权限记录采用同一取消结果；完成先提交时，后来的取消返回 false，不能改写原终态。未领取的排队任务可以直接取消；已有原执行者的任务仍由原执行者收尾。迟到的审批不能复活旧任务，显式下一任务需要新的权限、执行者、attempt 和更高的执行围栏序号。

Requests 在原任务和进程清理期间保持 Closing，禁止继续接纳能力调用。已开始且结果不明的效果仍是 `outcome_unknown`，不能改成“已完成”或“已取消”。清理或持久化失败时保持 draining 并占用原执行名额。Requests 和 Queue 是独立数据库；若 Requests 已结束而 Queue 最终提交失败，取消会返回需要显式恢复的错误，不能确认另一个相冲突的终态。回归测试通过真实的提交失败验证这一边界，没有声称两个数据库之间存在原子事务。

完整 workspace 严格检查还发现 `crates/cayw-runtime/tests/module_lifecycle.rs` 的一个条件导入错误：`Message` 只在 StrongFlow 子模块使用，却放在仅要求 `modules` feature 的外层。补充修复只把这个导入移入 `strongflow` 子模块，随后运行完整测试与全部 Runtime feature 的七项模块测试；没有改变生产逻辑、feature 定义或依赖。

根 Cargo 清单、根锁文件、客户端清单和客户端锁文件未改。客户端 299 个第三方锁条目的包名、版本、来源及校验值都存在于根锁的 374 个第三方条目中。Runtime 默认不启用可选 feature，其实际依赖图没有 UI、可选 Server、VerifyTool、Codex 或代码索引运行依赖。Core/存储/事件协议仍是 v3，外层 IPC 帧仍是 v1。

## 提交前独立副本验证

2026-10-19，按用户“把 issue1 对应的代码提交并推送”的要求，从实际 Git 暂存区导出 385 个具名交付文件，其中 384 个相对原 HEAD 有变化，原有 `LICENSE` 未变。此前远端只有文档，因此首次源码提交包含根清单已声明的完整 20 包工作区、依赖锁文件、独立客户端及说明；只提交客户端会缺少实际运行依赖。其他包一同交付不表示对应 Issue 已完整验收。隔离中的长会话优化、External Module 迁移、本地工具脚本、构建目录和未发布评论草稿未纳入。

新证据根为 `/Volumes/ORICO/cayw-issue1-commit-20261019-ci3opucs/`，构建目录为其中的 `target/`，执行目录为 `staged-source/`。平台为 macOS ARM64，Rust/Cargo 1.95.0；使用新建的短 `TMPDIR`、`CARGO_NET_OFFLINE=true` 和 `RUST_BACKTRACE=1`，移除模型凭据变量。对暂存副本实际执行：

```sh
cargo +1.95.0 fmt --all -- --check
cargo +1.95.0 fmt --manifest-path examples/issue1-client/Cargo.toml -- --check
cargo +1.95.0 test --workspace --offline --locked --message-format=json
cargo +1.95.0 clippy --workspace --all-targets --offline --locked -- -D warnings
cargo +1.95.0 check --workspace --all-targets --all-features --offline --locked
cargo +1.95.0 test -p cayw-runtime --test module_lifecycle --all-features --offline --locked --message-format=json
cargo +1.95.0 test --manifest-path examples/issue1-client/Cargo.toml --all-targets --offline --locked --message-format=json
cargo +1.95.0 clippy --manifest-path examples/issue1-client/Cargo.toml --all-targets --offline --locked -- -D warnings
cargo +1.95.0 run --manifest-path examples/issue1-client/Cargo.toml --offline --locked --message-format=json -- all /Volumes/ORICO/cayw-issue1-commit-20261019-ci3opucs/scenarios
cargo +1.95.0 run -p cayw-agent --example offline --offline --locked --message-format=json
```

十条命令均退出 0，运行前后 385 个输入未变。完整默认 workspace 测试通过 1,185 项、失败 0 项、忽略 5 个入口；186 个父级汇总对应 166 个测试程序与 20 组文档测试，三个嵌套子进程通过不重复计入。七项模块生命周期测试、两项客户端测试和六个客户端场景分别通过，不与完整测试相加。六场景结果逐一与 `acceptance.json` 核对；unknown 恢复仍保持无自动重放，重开宿主的 Provider 和服务调用数均为 0。离线示例输出 `completed: 5 messages, running=false`。全目标/全 feature 编译不等于所有 feature 已执行；此前五个默认忽略入口及远程二进制场景的边界不变。

完整测试仍有四条同名 `offline` 示例输出冲突警告，原始日志保留。严格 Clippy 通过。保留的历史 `rust-pi-core-magpie-checks.log` 末尾空行未改写，因此源码与文档的 `git diff --cached --check` 排除原始 `docs/validation/*.log` 后通过。暂存副本中 67 个本地 Markdown 链接有效。没有运行真实模型或付费服务，也没有补做 Windows/Linux。

```text
selected-source-manifest.json
SHA-256 8adf7cf8a8665a7e304ef0f6fa6fb1a6f2529e1f7d58213ad6cdb977cd56b9bf

validation.json
SHA-256 c7aeeec75f8a929b758408dd0d86c76efba48faae2c364dfd1e8aac388821ca4

result-verification.json
SHA-256 34791b17d10350be9e1163ec5fc39d3a0c83d07130cce5b8f62f6ad7489d89b7

scenarios/acceptance.json
SHA-256 0fd7641239139542ae32ca02edd7f6e5ac17dc63b72d048dd2e215c58eea5f74
```

本节及上文的历史时点措辞在测试完成后补入。最终提交相对已测副本仅更新本验收文档，产品源码、测试、清单、锁文件和其余交付文件保持一致；最终字节身份与提交树另记在本地交付封存中。原始测试记录不覆盖。

## 主工作区复跑

平台为 macOS 27.0.1 ARM64，Rust/Cargo 1.95.0，debug 构建。模型回复、审批选择和中断服务效果明确使用固定合成数据；SQLite、TCP、Luau、Host 分发、文件读写和子进程退出走真实产品路径。未调用真实模型或付费服务。

修复后新构建证据根：

```text
/Volumes/ORICO/cayw-issue1-cancel-main-20261019-h6x460x1
```

从 `/Volumes/ORICO/CAYW` 执行，环境覆盖为：

```sh
export CARGO_TARGET_DIR=/Volumes/ORICO/cayw-issue1-cancel-main-20261019-h6x460x1/target
export TMPDIR=/Volumes/ORICO/cayw-issue1-cancel-main-20261019-h6x460x1/tmp/
export CARGO_NET_OFFLINE=true
export RUST_BACKTRACE=1
```

验证进程还移除了环境中以 `_API_KEY`、`_AUTH_TOKEN`、`_ACCESS_TOKEN` 结尾的变量，只记录移除的变量名。实际命令：

```sh
cargo +1.95.0 fmt --all -- --check
cargo +1.95.0 fmt --manifest-path examples/issue1-client/Cargo.toml -- --check
cargo +1.95.0 run --manifest-path examples/issue1-client/Cargo.toml --offline --locked --message-format=json -- all /Volumes/ORICO/cayw-issue1-cancel-main-20261019-h6x460x1/scenarios
cargo +1.95.0 test --manifest-path examples/issue1-client/Cargo.toml --all-targets --offline --locked --message-format=json
cargo +1.95.0 test -p cayw-core -p cayw-ipc -p cayw-runtime --all-targets --no-default-features --offline --locked --message-format=json
cargo +1.95.0 clippy --manifest-path examples/issue1-client/Cargo.toml --all-targets --offline --locked -- -D warnings
cargo +1.95.0 clippy -p cayw-core -p cayw-ipc -p cayw-runtime --all-targets --no-default-features --offline --locked -- -D warnings
cargo +1.95.0 check --workspace --all-targets --all-features --offline --locked
cargo +1.95.0 --version --verbose
rustc +1.95.0 --version --verbose
```

十条命令均退出 0。相关产品测试为 284 项父级测试，客户端另有 2 项，均无失败或忽略；嵌套子进程的 1 项汇总不重复计入 284。新增的 13 项回归包括八项任务写入测试和五项公开 Runtime/认证 IPC 测试。六个 CLI 场景单列，不加到测试数量中。20 包所有目标、所有 feature 的编译通过，不将编译检查称为所有 feature 的执行验收。

上述十条定向命令前后，Main 核对 404 个具名输入一致，核原始日志、63 份保留程序和 82 个明确场景输出。403 个原输入加一个新的取消回归测试构成这份清单；不是仓库全部未跟踪文件的枚举。本验收记录和 `.local` 验证脚本另行绑定，不属于 404 个交付输入。随后补充测试导入修复，只改变了同一清单中的 `module_lifecycle.rs`；下节完整 workspace 验证使用新的 `workspace-source-manifest.json`，不能将两份源码清单当作相同字节。

根清单和锁保持：

```text
Cargo.toml  SHA-256 76baed65b2c40b24ff5d92f2830ab88daceb2a95de1a4ffc86addcf9080d8874
Cargo.lock  SHA-256 46e0440091aafa15a621795ccfd2560d2a2ab53baa3fb0c1044bad79be1365b3
```

主树证据身份：

```text
handoff-audit.json
SHA-256 6934e871e8ccc2a7380042d5801b7fac7566d22130c925fbc2c82065d5435957

integration.json
SHA-256 9c0678f7c7f96254b9ff6329f53eb753f8a0695b8f7b4c0f983fcf9f6c9eef46

live-source-manifest.json
SHA-256 b00285017a7d590659f17a0e0cc984a744aaae118c00777a16fcfb8aafd1489a

validation.json
SHA-256 34516574966c558f23bc766a6d743cada17a73fcbc0657c802dae8705dc8a8b1

main-verification.json
SHA-256 defac6b4737dcbb9781a0fa772a3781770e47b0ca6170f661f8d3fd274ebbb89
```

## 完整 workspace 回归

因取消修复改动共用运行层，Main 在同一主工作区另跑完整测试。第一次沿用较长的证据目录作为 `TMPDIR`，`cayw-host/tests/shared_read.rs` 的 Unix socket 测试失败，错误为 `path must be shorter than SUN_LEN`。只将临时目录缩短到同一磁盘后，未改源码的完整测试通过；随后严格 Clippy 报出上文的条件导入问题。修正该测试导入后，以下六条命令全部重新执行并退出 0：

```sh
export TMPDIR=/Volumes/ORICO/c1-07o4ub3s/
cargo +1.95.0 fmt --all -- --check
cargo +1.95.0 test --workspace --offline --locked --message-format=json
cargo +1.95.0 clippy --workspace --all-targets --offline --locked -- -D warnings
cargo +1.95.0 test -p cayw-runtime --test module_lifecycle --all-features --offline --locked --message-format=json
cargo +1.95.0 check --workspace --all-targets --all-features --offline --locked
cargo +1.95.0 run -p cayw-agent --example offline --offline --locked --message-format=json
```

其余环境覆盖仍采用上节的 `CARGO_TARGET_DIR`、`CARGO_NET_OFFLINE` 和 `RUST_BACKTRACE`，同样移除模型凭据变量。复跑时使用新建的短临时目录；不要依赖上述历史目录仍存在。

完整测试的 186 个父级汇总与 Cargo 启动的 166 个测试程序和 20 组文档测试相符：1,185 项通过、0 项失败、5 个入口忽略。三个嵌套子进程通过汇总不重复计入；九个由父测试故意中断的子进程启动没有自己的完成汇总，单独保留，不计作额外通过项。启用全部 Runtime feature 的 `module_lifecycle` 另有 7 项通过、0 失败、0 忽略。离线示例实际输出 `completed: 5 messages, running=false`。

默认忽略的五个入口分别是：

| 入口 | 本次范围 |
| --- | --- |
| `crash_effect_child` | 由 `independent_crash_effect_is_durable_unknown_without_replay` 在独立子进程中调用。 |
| `crash_child` | 由 `abrupt_process_exit_recovers_unknown_without_replay` 在独立子进程中调用。 |
| `live_scope_child` | 由 `live_process_scope_lock_is_not_stolen` 在独立子进程中调用。 |
| `lifecycle_child` | 由对应父测试在独立进程中调用。 |
| `server_binary_device_example_and_operator_commands_work_on_loopback` | 需先构建 Device 示例再显式运行的远程二进制场景；本次未运行，保持未验证。 |

完整测试 stderr 仍有四条 Cargo 输出文件名冲突警告，涉及不同示例共用 `target/debug/examples/offline` 和 `offline.dSYM`，原日志保留。严格 Clippy 命令退出 0，不能据此把完整测试日志写成零警告。

最终六条命令前后均核对 404 个具名输入未变；封存核对覆盖原始日志和 194 份保留程序。这个程序数量属于完整 workspace 记录，与定向验证中的 63 份有重叠，不相加。新源码清单相对上节清单只改变一个测试文件，生产取消修复仍与独立复核补丁逐字节一致。

```text
workspace-source-manifest.json
SHA-256 a742cacf11276f802626ea9876e5c2c819679dca1f279c7fbb303d2ce7d5caa0

workspace-validation-final.json
SHA-256 20a615f93a92700ed84969017cfc391ebd0a38b83de3a7cd317583b5b2e47166

workspace-main-verification.json
SHA-256 0d607daa5c1e275eba91a009360d4fea4940000279686be0b1013f1cd1dc20dd
```

第一次路径长度失败保存在 `workspace-validation.json` 与 `workspace-commands/`，缩短路径后的完整测试通过和条件导入检查失败保存在 `workspace-validation-shorttmp.json` 与 `workspace-commands-shorttmp/`；最终成功记录使用 `workspace-commands-final/`。`workspace-runner-initial.py` 保留初次脚本，`supplemental-before-module_lifecycle.rs` 保留测试导入修复前的文件。没有覆盖早期失败记录。

测试完成后，本验收记录、README、AGENTS 与路线图才更新进度。证据根的 `documentation-before.json` 保留四份文档修改前的字节身份；`post-validation-documentation.json` 记录前后摘要并核对 404 个交付输入中仅三份根说明文档发生变化，其他 401 个仍与最终测试清单一致。本验收记录在该文件中另行绑定。这些文档更新不代表在更新后的文档字节上重跑了产品测试。

## 逐项验收

| Issue #1 要求 | 修复后证据与结果 |
| --- | --- |
| 独立 SDK 和认证 IPC 创建 Session、发请求、读取执行中的新事件、取消 | 两条客户端路径通过；IPC 使用真实 TCP 认证和 Runtime 分发。独立探针先观察订阅等待，再放行模型回复并读取新事件。 |
| 跨实例取消与终态一致 | SDK 经另一份 Runtime，IPC 经另一认证服务取消。旧任务为 `cancelled`，`cancel_requested=true`，且只有一个 `run_cancelled`。独立固定顺序探针还核 Requests 为 Ended/Cancelled，并只有一次 `task_ended`。 |
| 完成先提交、排队取消、重复收尾、旧执行者 | 完成先提交后取消返回 false，任务、审计、Requests 不变；未领取和已领取排队取消均只有一次终态；错误执行者、旧围栏和重复收尾被拒绝。 |
| IPC 版本、错误和有界资源 | 不兼容帧版本明确返回 `unsupported_version`。Core v3 与 IPC v1 区分；产品测试覆盖响应配对、容量拒绝、慢消费者、帧大小、取消期限和断连不重放。 |
| 游标保存后断线、准确续读 | 文件及父目录同步后才断开，从磁盘读回游标并读取严格后缀；前缀加后缀等于原持久事件，没有重复、缺口或 prompt 重放。 |
| 工具结果不能错配或重复消费 | 公开 Core 拒绝错误 ID、名称、参数、跨 attempt/Worker 和旧执行凭据，拒绝不推进游标。独立探针核乱序完成仍按声明顺序唯一消费，保留原调用来源和身份。 |
| 审批不能绕过 | 允许前没有文件效果；单次写权限不扩展到随后读取。伪造、重复和迟到审批被拒绝；旧审批不能用于下一任务，下一任务拒绝后没有文件效果。 |
| Session 与 Worker/进程解耦 | 客户端停止自己持有的原始子进程，并 wait 确认退出后才 reopen。同一 Session 保留，新显式 task/attempt/Worker 不同，Core 围栏更高。 |
| 开始前中断与已开始未知效果恢复 | 开始前中断记录未执行、不重放。真实 Host 分发并同步合成效果后中断，保留原身份 unknown、拒绝新工作及 Fork；只凭原收据显式核对。重开宿主的 unknown 场景中 Provider 与服务调用数均为 0。 |
| 取消不能清除 unknown 或掩盖清理失败 | unknown 保留，Requests 以 failed 结束；Device 已退出但清理记录失败仍保留 draining、Closing 和容量。原持有者重试并显式恢复后才释放。 |
| 两个持久库部分提交失败 | 真实 Queue COMMIT 故障发生在 Requests end_task 成功后；保留容量、拒绝冲突取消，显式恢复不改写已结束的权限终态。 |
| Windows | 不在当前 macOS 首发范围，适配与验证停止；之前发现的问题保留为历史记录，不阻塞本次交付。 |
| Linux | 不在当前 macOS 首发范围，环境排查与验证停止；之前未运行的事实保留，不阻塞本次交付。 |
| 真实/付费 Provider | 未验证；不从固定离线模型外推。 |

主树两条取消路径的原始输出位于 `scenarios/sdk/cancel-peer.json` 和 `scenarios/ipc/cancel-peer.json`，对应的 `cancel-tasks.json`、`cancel-audit.json`、`cancel-replacement-tasks.json` 保留旧终态及新任务。两条路径中原 Core 围栏为 1，下一 attempt 为 2。Runtime task 自己的围栏与 Core attempt 围栏属于不同记录，不能混为一个序号。

## 独立复核与证据核对

修复作者与独立审查分别使用：

```text
/Volumes/ORICO/cayw-issue1-cancel-repair-20261019-a2zjn9lt
/Volumes/ORICO/cayw-issue1-cancel-review-20261019-qIy6NOfl
```

独立审查重新构建并运行相关 284 项产品测试、2 项客户端测试、六个场景，以及十项独立探针（原五项交错探针、四项固定顺序探针、一项 Device 清理探针）。格式、严格 affected Clippy、20 包所有目标/feature 编译通过。该次矩阵 23 行本地要求通过、三行未验证（Windows、Linux、真实 Provider），该次取消修复复核没有确认的遗留缺陷；后续平台复核另确认了下节的两处 Windows 阻断。

Main 核对作者 470 份、独立审查 568 份具名证据的长度和 SHA-256，九条作者最终命令、34 条独立命令记录及其中 16 条必需最终检查。作者程序清单 67 条由 61 份保留程序和六个编译工具组成；审查程序清单 73 条由 67 份保留程序和六个编译工具组成。Main 还从原 403 文件独立回放补丁，逐字节重建最终 404 文件，再接入主工作区。

保留的非零日志分清用途：原源码固定顺序探针退出 101，证明缺陷可复现；diff 退出 1 表示预期代码差异；其余七条非零命令是审查辅助脚本的目录、字段或依赖缓存路径错误，修正后相应检查通过。Device 独立探针原日志保留一条未使用导入警告。它们没有被改成“零错误日志”，也没有被当作最终产品检查通过的替代。

```text
作者 evidence/final-02-after-validation.json
SHA-256 348497f364c5a462b807ad0994ce675395cf5cfd05105bdafb21c56f99afb9a2

作者 evidence/issue1-cancel-final.patch
SHA-256 40ec3e3e1b5e1b27e21d4739ef0b483bb52d706dbf2c5eea5ac6270969289b04

作者 evidence/evidence-seal-final.json
SHA-256 b5b40f8702c53bbbbc51d2b84bce4c4a67f986f531c7885f50f6a500a8a914c8

审查 evidence/review-manifest.json
SHA-256 961e2aea541850ce01661c4096ff3ef74a83158f9993f5c849e875c6465b8f46
```

## 历史平台研究（已停止，不属首发范围）

本节记录平台范围调整前已做的工作，不是待执行计划。2026-10-19 的用户决定已取消本阶段 Windows/Linux 工作；以下问题和未运行命令均不阻塞 macOS 首发，也不继续实施。

以下工作发生在上述 macOS 验收和首次文档封存之后，不改写当时的源码清单或通过范围。只读源码复核及独立反证确认两处 Windows 阻断；没有在 Windows 或 Linux 执行产品，也没有交叉编译产品。

| 必经位置 | 已确认的问题及影响 |
| --- | --- |
| `crates/cayw-cells/src/journal.rs` 的 `SqliteJournal::open` | 使用普通 `File::open` 打开父目录后同步。Windows 普通文件打开没有目录所需标志，因此六个客户端场景共用的 Runtime 初始化入口会失败。客户端 `save` 也有相同操作；只修客户端不足以解决。 |
| `crates/cayw-workspace/src/lib.rs` 的 `open_dir_inner` | 明确拒绝非 Unix 平台。允许审批后的真实 `fs.write` 必经此入口，即使关闭可选 Workspace 工具也不能绕过。Host 在此前还会同步变更记录，因此初始化、Host、Workspace 和证据保存都要覆盖。 |

第二次方案研究与独立核实没有建立可直接交付的完整修复。当前候选是从原先持有的目录取得文件身份，用 `OpenFileById` 重开同一对象、核对卷及文件 ID、申请所需写权限，再执行目录刷新。所查官方文档分别支持这些组成部分，但尚缺普通用户、本地 NTFS 和现有共享限制下的组合运行证据。仅增加目录打开标志、复制句柄或使用不增加权限的 `cap-fs-ext::Reopen` 不能证明目录刷新完成。不能忽略同步错误、按外部路径重新找回根目录，或把需要管理员权限的刷卷当作替代。副作用可能已发生后的失败仍须保留 `unknown` 和恢复要求。

这表示方案还未验证，不表示 Windows 无法实现。独立原生探针已生成，用来记录权限、共享模式、原目录身份、实际刷新及错误结果。作者与独立复核在 macOS 上通过 42 条辅助函数断言、三组 JSON 解码、Windows x64/ARM64 目标文件编译及 x64 静态分析；两轮相同检查不相加。未编译原生 CRT 入口，未链接 Windows 可执行程序，也未解析或执行 PowerShell。Windows 原生执行次数为 0。探针准备或目标文件编译不计作产品修复、Windows 验收或断电持久性证据。当前产品源码未作 Windows 修复，隔离副本仍保留 405 个具名冻结输入。

独立复核发现，原 `run-native.ps1` 在 Ctrl+C 展开时可能跳过 `catch`，令子进程未停止且退出记录未保存。随后将终止原 Process 对象、限时等待、输出收集和记录保存移入 `finally`，并让 JSON 文件写入不依赖已关闭的输出管道。这是按已确认控制流程修正的脚本；本机缺少 PowerShell，不能声称正常、超时或 Ctrl+C 行为已实测。原作者清单、失败复核和本地编译日志保持原样，修正后的文件与差异单独绑定。

探针文件在以下独立目录，不属于产品源码：

```text
/Volumes/ORICO/cayw-issue1-windows-candidate-20261019-3vu6mn8b/probe/

probe.c                         原生调用与辅助函数
run-native.ps1                  Windows 构建及限时运行脚本
README.txt                      环境、命令及未验证范围
main-repair/                    中断清理修正与后续核对
```

范围调整前准备的 Windows 命令如下，仅作历史记录，当前不执行：

```powershell
pwsh -NoProfile -File .\run-native.ps1 -Parent 'C:\probe-parent' -Architecture x64 -TimeoutSeconds 90
```

父目录须已存在且位于本地 NTFS；ARM64 使用对应工具链并改为 `-Architecture arm64`。脚本只创建全新的测试子目录，保留原始输出及测试数据。该命令从未执行；原生编译、链接、正常完成、超时和 Ctrl+C 清理均保持未验证，本阶段不再补做。根目录位移若被原共享模式阻止，记录未执行，不能改共享模式凑出通过。

Linux 环境检查确认 Docker CLI 当前使用 `/Users/chengwen/.docker/run/docker.sock`；`/var/run/docker.sock` 指向同一个端点。限时的 `/_ping` 与 `/version` 请求均未收到响应，`curl` 退出 28。`docker desktop start --timeout 25` 只报告 `Docker Desktop is already running`，不能证明引擎可用。没有重启引擎、改配置、下载镜像或创建容器，也没有 Linux 产品测试。源码复核未发现必经的 Linux 平台拒绝，仍不能据此判定场景通过。

本次证据位置：

```text
主树平台复核：
/Volumes/ORICO/cayw-issue1-cancel-main-20261019-h6x460x1/windows-portability-review.json
SHA-256 74dc35529a0b36e8e3e1d20432f6057c58f100ab59a2c11a008336a9d0f57d80

方案研究与独立核实：
/Volumes/ORICO/cayw-issue1-windows-candidate-20261019-3vu6mn8b/evidence/windows-durability-research.json
SHA-256 3b15f662edffa3ed03333cd1ca4ec545826b466532b23a5f0c6290338d84ecb5

隔离源码清单：
/Volumes/ORICO/cayw-issue1-windows-candidate-20261019-3vu6mn8b/evidence/baseline-manifest.json
SHA-256 15e6f649aacdd7e0953963adcb86dba21ae4938f6ad5c5e252f5d7537789adb1

平台环境及各次 Docker 原始结果入口：
/Volumes/ORICO/cayw-issue1-windows-candidate-20261019-3vu6mn8b/evidence/execution-environment.json
/Volumes/ORICO/cayw-issue1-cancel-main-20261019-h6x460x1/portability-environment.json
```

保存研究时核对主树及隔离副本各 405 个具名输入均与清单一致。本节随后修改本验收文档，前后字节与其余输入核对另记在隔离证据目录的 `portability-documentation-before.json` 和 `portability-documentation-change.json`；首次 `post-validation-documentation.json` 保持原样。环境记录含各自检查时点的状态，当前方案核实已结束，以本节及保存的研究结果为准。

## 保留的初次失败

修复前的六文件示例交付和主树九命令记录保存在：

```text
/Volumes/ORICO/cayw-issue1-live-20261019-g9tw6480
```

当时 271 项产品测试、2 项客户端测试和六个原场景通过，403 个具名输入、62 份保留程序、74 个明确输出由 Main 核对。但首次独立复核发现未覆盖的取消竞态，矩阵为 18 行通过、一行失败、两行未验证。后来的某一轮 36 例全部通过没有撤销已确认失败。

原失败仍位于：

```text
/Volumes/ORICO/cayw-issue1-review-20261019-Skh6tg/evidence/cancel-terminal-reproduction/peer-ipc-9/

cancel-response.json  cancelled: true
tasks.json            state: completed, cancel_requested: true
audit.json            kind: run_finished
```

取消只写了共用任务表，原 Worker 最终却只读取本实例的内存取消令牌；Queue 最终事务没有重读持久取消，Requests 又过早独立选择结束结果。旧的排队取消还允许重复收尾，追加重复审计。修复分别以固定执行顺序、唯一终态、提交失败及清理失败测试覆盖，没有靠增加等待时间或反复碰调度代替回归。

旧主树 `main-verification.json` 的 SHA-256 为 `a7ab12a7e8f9e105d7debcfcab7a574480592db81377d835635d3a4108ac024c`；Main 初次独立失败核对记录 `independent-review-audit.json` 为 `986d6c864e0cca14b15bc13c38c37658fa3421896fe175c6cd8cc5708a52eb3d`。这些历史文件没有覆盖。

## 结论边界

本记录证明主树在 macOS ARM64、固定离线模型下的 Issue #1 本地交付、取消修复和默认 workspace 回归。Windows/Linux 不在当前首发范围，其适配与验证已停止。真实模型、突然断电持久性、任意并发 Requests 锁可用性、完整 workspace 所有 feature 的运行及上表远程二进制场景未验证；这些事实不因平台范围调整而改变，也不自动并入 #1 的最小 SDK/IPC 验收。七项模块测试启用全部 Runtime feature，不等于完整 workspace 所有 feature 已执行。子进程停止并等待只证明进程退出后的恢复。作者、独立审查与主树重复执行的测试不相加。

完整 workspace 默认测试、严格 Clippy、所有目标/feature 编译及离线示例均已通过，最终结果绑定 `workspace-validation-final.json` 和 `workspace-main-verification.json`。后续说明文档更新单独记录，不改写测试时的源码清单。按用户最新确定的 macOS 首发范围，Issue #1 本地功能验收完成，并已按后续明确指令关闭 GitHub #1、建立 Linux/Windows 支持任务 #19；对应实现、独立客户端和本文随本次提交交付，原始运行产物仍保存在上述本地证据目录。Issues #2、#6 和其他 Issue 没有因此完成，继续按各自 macOS 交付和可复跑证据推进。
