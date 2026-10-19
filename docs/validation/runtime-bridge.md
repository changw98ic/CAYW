# Runtime 宿主接入验收

2026-10-19，Runtime 已把宿主现有 Bridge 接入实际 Cells/Luau → Host 调用链。本记录覆盖本地固定 Provider 与 SourceHandle、macOS ARM64、Rust 1.95.0 的局部验证。真实服务、最新 Linux/Windows 及完整 Issue #9 验收仍未完成。

## 实际接线

可信宿主通过 `RuntimeConfig.bridge: Option<cayw_bridge::Bridge>` 提供已有能力；默认无 Bridge。若同时提供 `host.external`，Runtime 在打开会话数据库之前拒绝歧义配置。Core 不创建第二份 MCP loader、连接或来源注册表。

每个外部调用从原 Core 能力记录创建独立范围，通过 `Host::scoped_external` 使用原权限和审计对象，共享 Host 的预算、未知状态、执行顺序及进程归属。目录发现使用不能执行的服务对象，真实执行必须带原调用范围。内部调用保留完整 lease/worker/fence、task、execution、parent、source hash、精确参数和 BindingSnapshot；能力申请还绑定原注册表身份，替换注册表不能继承旧授权。

准备和终态保存为 Core 的 `runtime_bridge_prepared`、`runtime_bridge_settled` Host 事件，原 lease 在提交时再次核对。准备提交后才轮询宿主来源。事件是恢复依据，不另建可变的 dispatch 表。`bridge_dispatches` SDK 按 Core 事件游标返回经核对的记录；当前每页重建完整会话前缀，尚无独立快速索引。

恢复只从原来的持久日志核对来源、请求、授权和结果，不重新调用工具或模型。只有原 Started 缺少终态、且有完全一致的已知 Bridge 结果时，才能修复 Core；明确保存过的已知或未知结果不能覆盖。只有经完整核对且属于同一 parent 或已登记 wait 的子调用，才能独立承担该结果的不确定性。缺失或损坏的证明仍保留 parent unknown。

## 实际结果

作者在一个检查前后哈希一致的 15 个本地依赖包、286 个输入文件集合上执行：

```sh
cargo +1.95.0 fmt -p cayw-runtime -- --check
cargo +1.95.0 check -p cayw-runtime --all-features --offline --locked
cargo +1.95.0 test -p cayw-runtime --all-features --offline --locked
cargo +1.95.0 clippy -p cayw-runtime --all-targets --all-features --offline --locked -- -D warnings
```

四项退出码均为 0。Runtime 完整测试为 108 passed、0 failed、0 ignored；其中 `bridge.rs` 15 项、`bridge_recovery.rs` 9 项、`runtime_bridge.rs` 21 项，合计 45 项。Main 已读取完整测试、check 和 Clippy 日志，并核对 286 个输入文件及 11 份证据的哈希一致。

断言使用实际 Luau、原 Host 授权和调用记录；独立 SQLite 读取证明准备事件在来源第一次被轮询之前已经提交。场景包括并行 Session、交错 Cells、取消/Drop/吊销/重连、完整旧 lease 拒绝、来源和 Core 记录失败、参数/结果边界，以及伪造来源、变更投影和恢复重试不重放。Host/来源权限拒绝时分发数为零。普通文件持久化失败的 unknown 行为继续由回归测试保留。

证据中的早期失败保留：初期依赖源码未完整时的编译失败；测试误用超过 64 的分页上限；两个已修正的严格检查表达式。另一次完整测试在 `approvals::expired_grant_requires_a_new_decision_before_second_dispatch` 等待审批超时，原测试孤立重跑通过。该测试使用 100ms 授权及 180ms 延迟，存在负载相关的时序依赖；这些观察不证明那次超时的唯一原因。现已只修改 `tests/approvals.rs`：用 Semaphore 持有已经获准的首次调用，仅把原授权记录的过期时间改到过去，核对其余字段完全不变，再释放首次调用。测试仍要求同一次执行、相同能力和参数范围出现新的审批，且拒绝第二次审批后来源调用总数保持为 1。没有固定毫秒延迟或耗时成功断言，5 秒等待只用于发现缺失的状态变化。修正后单项测试以及完整 108 项测试通过，生产源码和授权语义未修改。原失败、原测试、修正前单项通过与修正后记录分别保留。

## 源码绑定

修正审批过期测试后，局部验证的完整输入集合 SHA-256：

```text
4546fd1cc41672abe57e188505a7ca70f3784a73fe81aa33860a4347c9fdbc9c
```

Runtime 输入集合 SHA-256：

```text
ec6302d719ff1ac2bfc94d2c08b887f4071d73a213d33d197d1053177b67c439
```

Main 已核对修正后 286 个归档输入与清单完全一致，11 份证据哈希一致；与前一验证集合比较，唯一变化是 `crates/cayw-runtime/tests/approvals.rs`，其 SHA-256 为 `8b82ed1d2f92dbf7d9eb0a9952e315bfd3717fdf0ff94562ab9d609603dac6a0`。完整测试、单项测试、check、Clippy 及空的 fmt 日志均已读取。

根 Cargo.lock SHA-256：

```text
46e0440091aafa15a621795ccfd2560d2a2ab53baa3fb0c1044bad79be1365b3
```

本机修正后证据位于 `crates/cayw-runtime/target/bridge-validation/expiry-correction/`：

- `validation-facts.json`：平台、输入、配置、权限、命令、结果和限制；SHA-256 为 `857efce90a3aea9de5e7776cbb6a3d26ecb860cbfe7ebe311e58d1c746614a7d`。
- `receipt.json`：测试数量和检查前后输入、日志哈希。
- `closure-before.json`、`closure-after.json`：精确路径和文件哈希；两份内容一致。
- `tests-after.log`、`check-after.log`、`clippy-after.log`、`fmt-after.log`：完整输出。
- `correction.json`、`approvals-before.rs`、`failed-before.log`、`isolated-before.log`、`isolated-after.log`：唯一测试变更与修正前后的原始观察。
- `source-closure.tar`：286 个修正后输入文件，归档 SHA-256 为 `396411ff7655423f1e6dab8f63ef36dc9f7389cf193661032a5c65f4fa2862f5`。

前一验证集合位于 `crates/cayw-runtime/target/bridge-validation/final/`，完整输入哈希为 `479dbe93568dfe5e73022318e45efb3db4000f5948a71ef6e23be7ab40c52c61`；原始证据未覆盖。另有该阶段整个 workspace 的副本 `crates/cayw-runtime/target/bridge-validation/source-snapshot`，父目录的 `source-manifest.json` 收录 561 个文件、20 个包，哈希 `7b863ce5879acffb2888ccae761cdd245834dcd4151663bd430f380c18085d71`。这个副本不包含后来的审批测试修正；不同阶段和收录范围不能混用。证据目录由构建生成，其他机器不保证已有，应以源码与精确命令复跑。

本次只使用固定本地 Provider/SourceHandle，没有真实模型或付费调用。Windows/Linux、宿主真实 MCP 服务、完整 Issue #9 和后续共享读取接口不在此结果内。原始证据保留，后续源码变更必须建立自己的绑定，不能沿用这里的 108 项结果。
