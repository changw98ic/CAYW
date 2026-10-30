# Runtime 错误分类与 Core 追加写入复核

本记录对应 2026-10-30 的后续清理。源码基于 `dc849aa1f3a3e9758858d19cf83a67c94a7dac6c`，检查在提交前的工作树上运行，源码校验值见本文末尾。运行平台为本机 macOS arm64，使用 Rust/Cargo 1.95.0。没有调用真实模型或付费服务。

## 修改范围

- Runtime 删除重复的 TaskIntent 规则。公开 Queue 入口继续调用 `TaskIntent::validate`；Requests 独立校验自己的授权输入。
- `persist` 返回已经写入的投影字符串。`mutate` 直接复用该字符串，删除写入后的整份投影查询，事务提交成功后才放入缓存。
- Host/control 事件使用专用的追加路径。原租约、预期游标、输入和等待记录校验均保留。事件和投影游标在同一个 IMMEDIATE 事务内写入，提交后回填缓存。
- Runtime 错误和分类集中在 `src/error.rs`。Rust API 与 RPC 共用 `code()`；`storage_failure()`、`worker_failure()` 和 `outcome_unknown()` 供调用方读取类型和状态。数据库忙、磁盘容量、I/O、损坏、worker panic/丢失和 Agent 错误有不同分类，原始错误来源及恢复清理错误继续保留。RPC 增加有界的操作名和已提供的会话/任务身份上下文，不截断原始原因。
- Requests 权威存储故障保留原有未知状态，错误码同时带具体原因和 `_outcome_unknown`。内容存储的 `CommitOutcomeUnknown` 也继续保持未知。恢复错误递归核对原因与清理错误，外层 `false` 不能覆盖内层未知。没有新增自动重试。

SQLite 的 `json_set` 仍处理完整投影 JSON。读取缓存前仍读取并比较完整 JSON。该改动没有把追加写入变为常数开销，也没有修改 Core v3、IPC v1、WAL 或 FULL 配置。

## 已执行检查

在仓库根目录执行：

```sh
cargo +1.95.0 test -p cayw-core --locked
cargo +1.95.0 test -p cayw-core --lib ordinary_write_commit_failure_preserves_history_and_allows_retry --locked
cargo +1.95.0 test -p cayw-queue --locked
cargo +1.95.0 test -p cayw-runtime --test input --test request_boundaries --test session_view --locked
cargo +1.95.0 test -p cayw-runtime --test runtime --test cancel_terminal --test scheduler_failures --locked
cargo +1.95.0 test -p cayw-ipc --locked
cargo +1.95.0 check --workspace --locked
cargo +1.95.0 check -p cayw-runtime --all-targets --features plugins,pi-compat,strongflow,remote --locked
cargo +1.95.0 clippy -p cayw-core -p cayw-queue --all-targets --locked -- -D warnings
cargo +1.95.0 clippy -p cayw-runtime --all-targets --locked -- -D warnings
cargo +1.95.0 fmt --all -- --check
```

以上检查均已完成并返回 0。Core 全包完成后，原有 COMMIT 失败测试又补入普通 `finish_attempt` 写入场景，随后单独复跑通过。该测试现在分别验证轻量追加与完整投影写入提交失败时，历史和事件保持不变，该会话缓存不发布，移除故障后重试成功。Queue 为 39 项；Runtime 上述六个测试目标合计 35 项；IPC 为 46 项。

大历史回归在原有流式测试中增加 Host/control 写入，核对旧游标被拒、事件顺序和执行身份正确、消息历史不变、独立连接重新解码后的会话与缓存相同。原有跨连接写入、恢复后旧租约拒绝、同游标投影损坏、等待登记与日志一致性、追加日志保护测试继续通过。

最终错误实现又执行了以下检查，均返回 0：

```sh
cargo +1.95.0 test -p cayw-runtime --locked
cargo +1.95.0 test -p cayw-core --features fault-injection --locked
cargo +1.95.0 check --workspace --locked
cargo +1.95.0 check -p cayw-runtime --all-targets --features plugins,pi-compat,strongflow,remote --locked
cargo +1.95.0 clippy --workspace --all-targets --locked -- -D warnings
cargo +1.95.0 fmt --all -- --check
```

Runtime 全包的 7 项错误回归均通过。全包有 4 项 ignored：`held_runtime_process` 与 `configured_transport_child` 是由父测试显式调用的子进程辅助入口；`complete_capture_preserves_stream_and_accounting` 和 `local_stream_stages` 需要外部 fixture 输入，本轮未运行，不能计为通过。日志中的子进程执行结果与父测试可能重复计数，因此本记录不将简单汇总数当作独立测试数量。

## 现有基准的结果核对

复用已有 `refactor_persistence_bench`，release 构建后串行运行中档普通写入（1 会话）与中档混合写入（2 会话）。数据库位于工作树所在的外置卷，独立结果目录为本机 `.local/core-append-20261030.UFtDl0/`。没有增加基准框架或生产诊断接口。

```sh
cargo +1.95.0 build -p cayw-core --release --example refactor_persistence_bench --locked
target/release/examples/refactor_persistence_bench --size medium --category normal --sessions 1 --db <新目录>/normal.sqlite --out <新目录>/normal.json
target/release/examples/refactor_persistence_bench --size medium --category mixed --sessions 2 --db <新目录>/mixed.sqlite --out <新目录>/mixed.json
```

普通写入完成 3,589 条事件、1,024 条消息，投影 277,005 B。混合写入共 2,186 条事件，每会话 128 条消息，合计投影 170,584 B。所有会话游标、消息数、数据库行数和 Idle 终态核对通过，存储格式仍为 `cayw-core/3`，SQLite 为 3.53.2。这两次运行只核对写入结果；没有同输入的改动前对照，因此不据此报告提速比例。

## 过程中的失败

- 调整 `persist` 返回值后，首轮编译指出 waits 和工具来源绑定两处仍期望单个事件；已改为解包元组。
- 新增旧游标断言最初期望 `InvalidInput`，现有接口实际返回 `InvalidEvent`；已按原契约修正，Core 全包复跑通过。
- 错误来源链首轮测试有一项失败。修正恢复错误暴露的具体来源后，6 项错误测试复跑通过。随后补入 Requests 存储未知状态和内容提交未知状态回归，最终 Runtime 全包结果另列。

Core 最后修改的 `store.rs` SHA-256 为 `13fbcbbafb96ef1d998d46c143c7e3e286cfd6d5cdf359cbb4cae3461ebb6cef`；大历史回归文件 `streaming_projection.rs` 为 `cab42c15075759e3450b39b01998f4133dec3f868e9d639401c579ad6c01eca4`。最终错误模块 `error.rs` 为 `a03cf2ac439b64f09d590f464ed6456cc165c57a2620f982db8816ce4050e8c3`，RPC 边界 `rpc.rs` 为 `412b41446d845fc9fd9fffeed35d958e01f89bccf4e3696436d6bedc5d374698`。本轮原始测试日志（包括首轮失败）保留在上述本机 `.local/core-append-20261030.UFtDl0/`，文件名对应会话的后台任务 ID。

本记录只覆盖上述本机离线场景，不证明其他 macOS 架构、真实模型或远端 CI 已通过。
