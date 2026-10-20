# Issue #2：macOS 离线验证基线

本次交付可复用的 VerifyTool 场景、故障注入、报告读取和比较接口，并提供独立消费示例。场景使用实际 Runtime、SQLite、Host 和经过认证的本机 IPC；固定离线 Provider 不访问真实模型。VerifyTool 是测试消费者，不是 Runtime 的运行依赖。

范围按 [Issue #2](https://github.com/changw98ic/CAYW/issues/2) 的十二条离线条件核对。该任务明确不等待 #6 的真实模型完整任务或 #7 的全部崩溃矩阵。本文与对应运行记录只证明记录中的源码、程序、配置和主机，不代表十一项主线最终接受。

## 已接受的 v11 交付

- 平台：macOS 27.0.1（26A434），ARM64；Rust/Cargo 1.95.0，debug 构建。
- 原工作分支基线：`6bdb9f93a9523671f499978708c583138a6d9a8d`，另有保存的未提交改动。
- 具名输入：374 项；源码标识 `f839955951f45e2ed1815400dd05735fd15c2a91cac40276ee9ed1879392c053`。
- 原源码清单摘要：`51954c841b3c9f366bdc5e7cf87dc5d1cf8cbad239c161bbf67cf48d8d9afe31`。
- 验证程序摘要：`82d519db1d55a757020927a273df3996104cb22910d694396237057c4d6be1bf`。
- 独立消费程序摘要：`b78271dc822f0edc024ca3a0ae8c1475ea3c035e6d5f17747f1f6d0e5fd78e3b`。
- 实际执行 43 条正式命令；工作区日志有 195 个测试结果组，0 个失败组。结果组数不是测试项数。
- 独立复跑 58 条作业，另有四条公开 API 补充作业、九个样本；六个没有任务结果检查的样本保持未验证，三个实际结果检查样本通过。
- 主会话核对了独立命令的 116 份输出流、410 个列明输出，以及保存的源码和程序。原材料保存副本共 2,273 项，最终复核另存 597 项；保存后重新核对摘要。
- 本轮真实 Provider 调用为 0。未知模型用量和费用保持空值，不解释成零费用。

十二条条件对应的实际检查如下。命令名是原证据目录 `commands.jsonl` 及独立索引中的唯一标签。

| 条件 | 交付与实际检查 |
| --- | --- |
| 可复用 SDK/IPC 验证入口 | `sdk-three-final-v11`、两个独立消费程序及公开自定义场景实际运行 Runtime/SQLite/Host。 |
| 基础离线故障样本 | SDK、认证 IPC、事件连接断开、执行连接丢失、取消、分派前后进程退出分别保留原样本。 |
| 测量与修复记录 | 冷暖、首次/后续执行、取消、进程内存观察及原语法错误/修正成对保存；用量、费用未测保持空值。 |
| 独立客户端与拒绝路径 | `issue1-client-all-v11`、`sdk-a3`、`sdk-b3` 核对审批、取消、查询和认证/版本/帧/参数错误，没有启动未授权效果。 |
| 事件续读与执行断连分开 | 原事件内容、顺序、游标、请求与执行身份均核对；断开执行连接不重发 prompt。同步改内容并重算外层摘要的反例仍被拒绝。 |
| 每个常规场景至少三次 | 作者 SDK 两组各 30 样本、Luau 两组各 33 样本；独立 SDK 两组各 30、Luau 33。单独 IPC 探针不冒充三次采样。 |
| 原任务与未知副作用 | 取消、重开、故障后核对原执行者和调用结果；未知副作用不自动重放，未确认清理不释放名额。 |
| 验证器失败对照 | 无真实效果、事件/分页缺失、调用错配、非终态和内容矛盾不能判任务通过。无 `task_check` 的公开自定义场景保持未验证。 |
| 比较规则 | 实际比较报告；已知条件不同为 `incomparable`，缺输入为 `pending`，损坏/矛盾为 `invalid`，两侧身份分别保存。 |
| 报告与构建输入 | 缺附件、内容修改、路径越界、错误程序/根目录/构建记录均有拒绝对照；运行时传入摘要不能替代编译时实读文件。 |
| 原始子进程清理 | 原 child 的正常、失败、超时、Future 丢弃路径保留原句柄等待和有界双路输出；清理不明停止后续采样。 |
| 独立消费与复核 | `examples/verify-tool-client` 独立 workspace/锁文件复用公开接口，实际创建、关闭、重开并查询会话，错误期望必须失败。 |

这里的“原 child”指原始子进程；它的退出与清理记录不以新进程或单次 PID 查询替代。进程退出注入只覆盖具名位置，不是断电测试或完整 Runtime 崩溃矩阵。

## 入库前复跑

入库使用独立分支，34 个交付文件逐字节对应已接受的 v11 副本。首轮复跑基线为 `814ce00b546a80f21cb12d8d61cf0f5a72a177cb`；随后纳入 `dev` 的构建配置更新，最终复跑基线为 `ae3c609db15eef8d04ae3b754260854eaa5a896c`。最终具名输入清单另外包含 `.cargo/config.toml` 和 `scripts/sweep-target.py`；脚本没有在本次复跑中执行。#3 的新增 Luau 场景和未完成接受记录不在本次补丁中。原开发目录不回退、不清理。

原验收记录及入库复跑使用不同目录：

- 原验收：`/Volumes/ORICO/cayw-issue2-continuation-20261020-thNXYL/issue2-offline-accepted-v11.json`，摘要 `731a41ebf1f7f488551e09c342c3f5489b062c32c12e49c84ac9b626ef97997a`。
- 原材料：同目录中的 `commands.jsonl`、`preserved-v11/manifest.json`、`preserved-v11-final-review/manifest.json`，以及接受记录逐项列出的独立作业。
- 入库源码：`/Volumes/ORICO/cayw-issue2-integration-20261020-a/`。
- 入库复跑：`/Volumes/ORICO/cayw-issue2-integration-evidence-20261020-a/`；最终基线原记录位于其 `on-dev-ae3c609/` 子目录。每条命令保存 argv、退出码、输出/错误流及具名输入前后摘要，两轮汇总见根目录的 `integration-result.json`。

最终基线上实际执行 16 条命令，退出码全部符合预期。格式、工作区编译、测试、Clippy 和离线 Agent 示例通过；工作区原日志的 195 组测试汇总合计 1,244 次通过、0 次失败、7 次跳过，其中包含父测试启动的子进程结果，不作为独立测试去重数量。最终一轮两个子进程的汇总输出交错，按完整计数字段及终态数量核对，未丢弃这两组结果。七个跳过入口中六个是子进程夹具，另一个远程 Device 示例不在本次范围。测试原错误流有四条已有的 `offline` 示例输出文件重名警告；Clippy 没有警告或错误。

SDK/IPC 十个场景各三次，共 30 个样本通过；Luau 十一个场景各三次，共 33 个样本通过；独立客户端三个持久会话样本通过。错误期望的三个样本全部判任务失败，执行退出 1，而读取有效失败报告退出 0。自比较因两侧均缺完整构建记录而返回 `pending`、退出 1。两轮分别重算 32 份输出流、32 份具名源码快照及 255 个样本附件的摘要。最终 369 项具名输入在全部命令前后相同；该清单不包含本文这份验证说明，不是整个提交树或所有编译器输入的完整追踪。

最终 VerifyTool 程序摘要为 `8c72e8a2606ca321d04d7c650a31b3d2168c13d9ced904236e6e58f5458531d2`，独立消费程序摘要为 `e70b3c915dc462a0fd09346bcdb77f3c7ccf28d01095000fcd7313d2f6169bf4`；最终具名输入快照摘要为 `87da8490990272fafec88c03aafffdff8db72193cc19e34164e63bde6d36200e`。独立锁文件额外解析的 `windows-sys 0.59.0` 已按注册表校验值核对，许可证为 `MIT OR Apache-2.0`；macOS 实际构建没有编译该包。未运行其他平台测试。

这些绝对路径只定位本机记录。其他开发机应使用新的目录实际复跑：

```sh
cargo +1.95.0 fmt --all -- --check
cargo +1.95.0 check --workspace --locked --offline
cargo +1.95.0 test --workspace --locked --offline
cargo +1.95.0 clippy --workspace --all-targets --locked --offline -- -D warnings
cargo +1.95.0 run -p cayw-agent --example offline --locked --offline
cargo +1.95.0 build -p cayw-verify-tool --locked --offline

# 使用实际构建目录中的程序；输出目录必须是新的。
cayw-verify-tool sdk-ipc --samples 3 --out /tmp/new-sdk-report
cayw-verify-tool read /tmp/new-sdk-report
cayw-verify-tool luau --samples 3 --out /tmp/new-luau-report
cayw-verify-tool read /tmp/new-luau-report

cargo +1.95.0 build --manifest-path examples/verify-tool-client/Cargo.toml --locked --offline
verify-tool-client run /tmp/new-client-report
verify-tool-client read /tmp/new-client-report
verify-tool-client wrong-result /tmp/new-wrong-result
verify-tool-client read /tmp/new-wrong-result
verify-tool-client compare /tmp/new-client-report /tmp/new-client-report
```

`wrong-result` 预期退出 1；其 `read` 仍可退出 0，因为报告有效不等于任务通过。没有完整构建输入的 `compare` 预期退出 1 并返回 `pending`。命令名需替换为本次构建程序的实际路径；短 `TMPDIR` 可避免本机 socket 路径过长。

## 保留边界

具名文件实读、前后摘要、公开 Cargo JSON 和 `.d` 文件不证明所有编译器、构建脚本、宏、原生工具和链接器输入已完整追踪。`build_input_completeness` 仍为 `pending`，不得输出完整来源、可比性能或速度优势结论。

本次未验证其他 macOS 架构、真实模型完整任务、模型生成/修复能力和全部崩溃窗口。后续任务在最终同一份源码上补足各自真实 Provider 证据。提交、推送、合并与 GitHub Issue 关闭分别以实际 Git/GitHub 状态为准。
