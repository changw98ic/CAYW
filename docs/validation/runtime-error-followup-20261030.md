# Runtime 错误分类的三处后续修正

本记录对应 2026-10-30 在 `810c5e04a585c9f5b368e323fccfa8c27401eb46` 基础上的工作树修改。验证平台为本机 macOS arm64（`uname -sm` 为 `Darwin arm64`），工具链为 `rustc 1.95.0 (59807616e 2026-04-14)`。没有调用真实模型或付费服务。本文只证明列出的源码、配置和本机离线场景，不外推到其他 macOS 架构或远端 CI。

## 修正及实际覆盖

- 独立 `headless-client` 的 SDK 模式直接使用 `RuntimeError::code()`、`to_string()` 和 `outcome_unknown()`。IPC 模式不再把 `needs_reconciliation_outcome_unknown` 改成旧名称。单元测试覆盖存储分类、嵌套原因、清理错误和两种模式的一致性；原有独立进程测试也核对协调拒绝时的错误码一致、身份保留和不重放。
- `StorageFailure::from_sqlite` 识别 `FromSqlConversionFailure` 内具体的 `serde_json::Error`，分类为 `storage_corrupt`，保留原始错误对象和原因。回归实际修改临时队列数据库的 `input_json`、`intent` 两列，再通过 `Runtime::task`、`tasks` 和真实 RPC handler 读取。测试核对具体来源、分类、上下文、未知标志不被添加、坏记录不被修改、Provider 未执行。非 JSON 转换错误没有被扩大为该分类。
- Runtime worker 的 `catch_unwind` 保留 `&str`、`String` 原因，并转义控制字符和不可打印字符。非字符串原因使用明确的 `non-string panic payload`。测试复用已有 `core.attempt_terminal.committed` 故障点，真实执行队列任务并在 Agent 返回后的 worker 清理阶段 panic。三种原因均经过实际捕获路径；返回值、RPC 错误转换和持久化任务记录保留原因，任务仍为 `draining`、保留执行名额、报告未知状态并拒绝新任务。

没有新增错误体系、重试机制、故障控制器或生产故障点。客户端为测试直接引用已在其依赖图中的同版本 `rusqlite = 0.40.1`，只增加开发依赖及锁文件中的直接依赖边，没有升级第三方包。Core v3、IPC v1 和错误消息结构没有改变。

## 已完成的检查

以下命令从工作树根目录运行，均返回 0。独立客户端命令使用 `CARGO_TARGET_DIR="$PWD/target"` 复用构建目录。

```sh
cargo +1.95.0 test -p cayw-runtime --test input --lib --locked
cargo +1.95.0 test -p cayw-runtime --features fault-injection --test worker_panic --locked
cargo +1.95.0 test -p cayw-runtime --test runtime --test cancel_terminal --test scheduler_failures --test process_cleanup --locked
cargo +1.95.0 test --manifest-path examples/headless-client/Cargo.toml --all-targets --locked --offline
cargo +1.95.0 clippy --workspace --all-targets --locked -- -D warnings
cargo +1.95.0 clippy -p cayw-runtime --all-targets --features fault-injection --locked -- -D warnings
cargo +1.95.0 clippy --manifest-path examples/headless-client/Cargo.toml --all-targets --locked --offline -- -D warnings
cargo +1.95.0 fmt --all -- --check
cargo +1.95.0 fmt --manifest-path examples/headless-client/Cargo.toml --all -- --check
git diff --check
```

Runtime lib 86 项、input 13 项通过；worker panic 为 1 项测试内的 3 种原因；runtime 7 项、cancel_terminal 5 项、scheduler_failures 4 项、process_cleanup 24 项通过。独立客户端为 17 项单元测试及 28 项集成测试，共 45 项通过，上述目标均无 ignored。`worker_panic` 只在 `fault-injection` 配置中运行，普通构建不包含该测试。此处没有重新运行整个工作区测试矩阵，也不以 Clippy 检查代替未运行的测试。

本机原始日志保留在 `.local/error-followup-20261030.4gKSpx/`：

| 日志 | 结果 |
| --- | --- |
| `runtime-input-errors.log` | Runtime lib 与 input 通过 |
| `worker-panic.log` | 真实 worker 捕获处的三种原因通过 |
| `runtime-lifecycle.log` | 取消、调度和进程清理相关回归通过 |
| `headless-client-all-final.log` | 独立客户端全包通过 |
| `workspace-clippy.log`、`runtime-fault-clippy.log`、`headless-client-clippy.log` | 三种检查配置均通过 |
| `workspace-fmt.log`、`headless-client-fmt.log` | 格式检查通过，成功时没有输出 |

## 复现和过程失败

- 分类修正前运行真实队列坏记录回归，实际得到 `Some(Other)`，预期为 `Some(Corrupt)`。日志为 `queue-json-before.log`。增加具体来源分支后通过。
- worker 回归最初使用取消时 Drop panic 的 Provider。Agent 的内部捕获先处理了该 panic，实际返回的是执行记录错误，未到达目标 worker 捕获处。日志为 `worker-panic-before.log`、`worker-panic-diagnostic.log`。该测试夹具已移除，改用现有的 worker 清理提交故障点，最终通过。
- 独立客户端首轮单元测试通过，集成测试仍断言旧的 `needs_reconciliation`。修正 `client.rs` 的两种模式断言后，全包又指出 `event_paging.rs` 的同一旧断言。分别保留在 `headless-client.log`、`headless-client-final.log`。同步为 Runtime 规范错误码后，最终全包通过。

这些失败日志未删除或改写。

## 最终源码 SHA-256

| 文件 | SHA-256 |
| --- | --- |
| `crates/cayw-runtime/src/error.rs` | `56fa1bfd78556a2f8c4fbf2bc8b5e109689018398e6c4947c21dc7186b1808fa` |
| `crates/cayw-runtime/src/execution.rs` | `befb7cc6437647d1db5aa35c4edb9ca36254a472af0259323352e3f6f2d4479a` |
| `crates/cayw-runtime/tests/input.rs` | `f65f21a4ddc8eca723a50ec8d6ba47b9bc88a50460bbfd22632e5d4795bf1da4` |
| `crates/cayw-runtime/tests/worker_panic.rs` | `02ac228322d1004624f28c927382b1e12c03611d65edb9866c78066e4c6468d0` |
| `examples/headless-client/src/main.rs` | `075f1751bb30273e881713ea347943e866fdda6ba82d70f641f35ac8106863ae` |
| `examples/headless-client/tests/client.rs` | `6edfbcddf0e4fe2a9309293d6ef243aa262b2d11a615fa9354819c36975ced2d` |
| `examples/headless-client/tests/event_paging.rs` | `68b0ce6c03b957710219bbb0e878a72d7a0ace2629fa26f118d77e9f48e0a211` |
| `examples/headless-client/Cargo.toml` | `ad83ed402cd1b40ca1a017d632cc22b1d9b47b6b2a8d549e4cb1bf743a78cc47` |
| `examples/headless-client/Cargo.lock` | `bf16c0eb38482b976472a3a2b01143db16afc43347abbbf71784e3b835229ae7` |
