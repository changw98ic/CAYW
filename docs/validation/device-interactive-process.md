# Device 交互进程与共用启动关闭验证

2026-10-19，Device 的交互进程接口与共用启动关闭修复已接入主树。在 macOS ARM64、Rust 1.95.0 上，原完整 20 包工作区编译通过；Device 81 项测试与 Runtime 5 项进程清理测试通过。格式检查、Device 严格 Clippy 均退出 0；Device 文档测试命令退出 0，实际没有测试用例。

本记录只覆盖这次六文件接入。External Module 的 Device 接入正在另一个独立副本实施；本次不能证明该迁移完成，也不能代替完整工作区测试或 Issues #4、#12 的全部验收。

## 交付的行为

宿主通过 `ProcessRunner::start_interactive_in_directory_with_permit` 显式传入允许的程序、参数、打开的目录、环境、总运行期限、总输出预算、交互缓冲限额和资源凭证。Device 持有真实进程、管道与监督线程；模块或其他消费者只得到唯一的 `ManagedStdin`、`ManagedStdout` 和可克隆的 `ProcessControl`。

- 输入在检查长度后才复制，最多保留一个待写请求。写入成功表示所有字节已进入原管道；已经开始的写入 Future 被丢弃时请求停止，防止半帧后继续使用同一进程。
- 输出缓存有明确上限，满时暂停 stdout 读取，仍观察停止、截止时间、stderr 和原进程退出。stdout 与 stderr 在原管道读取处共用整个进程生命周期的预算，包含空闲输出和预读字节。交互接口不保留 stderr 内容。
- 丢弃任一输入或输出端点会请求停止。`close()` 和 Future Drop 都不证明清理已经完成。
- `ProcessControl::wait()` 可重复等待，等待 Future 被丢弃后可以再等。它只在真实监督线程退出、线程局部析构与资源处理完成后返回；只看到管道结束或进程不再运行不能替代等待。
- 工作目录通过已经打开的目录句柄设置，不按可能已被替换的外部路径重新打开。

独立审查在原第一阶段交付中发现：普通进程的清理持续被操作系统拒绝时，资源仍被占用，同一执行器却还能启动新进程。原故障测试两次实际失败，日志保留。修复把原 Scope 的共用关闭标志直接传给所有真实启动路径；未确认清理时永久关闭该 Scope 的后续普通与交互启动。即使最终清理成功，该执行器也不会自动重新开放；独立执行器仍可工作。

## 固定源码与独立复核

作者固定源码：

```text
/Volumes/ORICO/cayw-device-shared-admission-20261019-b86c2e70
```

作者证据：

```text
/Volumes/ORICO/cayw-device-admission-evidence-20261019-b86c2e70/final-manifest.json
SHA-256 ffc79825e25fd31d57db7eae301128df4861e4d3ec0f3e4cf9f2b2da534c3edb
```

原审查者针对修复版重新验证：固定源码的 81 项测试通过，另加的 3 项独立探针通过，格式、编译与严格 Clippy 退出 0。独立探针保留原失败测试体，并覆盖交互清理拒绝、两种启动 Future 被丢弃后监督线程 panic、后续启动拒绝、等待清理保持未完成及资源最终归还。两次 panic 为主动注入并由原监督路径捕获；原失败日志和修复后的日志均保留。

独立记录：

```text
/Volumes/ORICO/cayw-device-admission-independent-review-20261019-c5b69d42/review-seal.json
SHA-256 1e98a6465ad29707d04cf841aa43b8bc8e841e39bd1ed4e51c6e7999c66c94e7
```

本次独立记录的 `findings=[]` 只表示该修复输出在此次复核范围没有剩余阻断发现。原第一阶段较早的空发现结论已经被后来发现的普通启动缺陷取代，不能继续用于放行原版本。

## 主树接入与实际命令

只接入以下六文件，没有接回作者副本中裁剪过的 workspace 或依赖锁：

```text
crates/cayw-device/src/lib.rs
crates/cayw-device/src/process.rs
crates/cayw-device/src/process/interactive.rs
crates/cayw-device/tests/fixtures/process.rs
crates/cayw-device/tests/interactive.rs
crates/cayw-device/docs/interactive-validation.txt
```

接入前保存原字节，`patch --dry-run` 与实际应用均退出 0，随后核对每个接入文件与作者固定字节一致。证据根目录：

```text
/Volumes/ORICO/cayw-device-live-integration-20261019-7kb2pq7x
```

从 `/Volumes/ORICO/CAYW` 实际执行：

```sh
export CARGO_TARGET_DIR=/Volumes/ORICO/cayw-device-live-integration-20261019-7kb2pq7x/target
export TMPDIR=/Volumes/ORICO/cayw-device-live-integration-20261019-7kb2pq7x/tmp
export CARGO_NET_OFFLINE=true
export RUST_BACKTRACE=1

cargo +1.95.0 fmt --all -- --check
cargo +1.95.0 check --workspace --all-targets --all-features --offline --locked
cargo +1.95.0 test -p cayw-device --all-targets --all-features --offline --locked --message-format=json
cargo +1.95.0 test -p cayw-device --doc --all-features --offline --locked
cargo +1.95.0 clippy -p cayw-device --all-targets --all-features --offline --locked -- -D warnings
cargo +1.95.0 test -p cayw-runtime --all-features --test process_cleanup --offline --locked --message-format=json
```

六条命令均退出 0。Device 的 81 项为 24 项单元测试、16 项交互测试、41 项其他集成测试；Runtime 另有 5 项进程清理测试，覆盖取消、丢弃 prompt、Provider panic、持久化清理记录失败与宿主进程退出后的恢复约束。主树记录为 `commands/<序号>-<名称>.json/.stdout/.stderr`，包含实际参数、工作目录、环境覆盖、时间、退出码、输入摘要和构建产物摘要。该工作区编译命令不是完整工作区测试。

主树验证输入清单含共享读取 v3 的 394 个明确路径与 3 个新增 Device 路径，共 397 个；每条命令前后逐项比对一致。这是具名输入核对，不是主树全部新文件的枚举。

Main 后续重新核对六条命令、原始日志计数、397 个输入与 14 份可执行文件，结果保存在：

```text
main-verification.json
SHA-256 eaa378b0710ec7899a2137c5f1d9795c0cd8de8a1c19eeeb89c19c8d2e2f7975
```

日志与字节核对由 `.local/seal_device_live_validation_20261019.py` 完成；该脚本使用排他创建保存结果，不能直接覆盖已有 `main-verification.json`。复核者可按 `validation.json` 与各命令记录中的 SHA-256 校验原始日志，再按 `live-source-manifest.json` 校验具名输入。需要重新运行功能测试时，应另设新的 `CARGO_TARGET_DIR` 和 `TMPDIR`，保留新的命令与日志，避免覆盖上述产物。

原根文件未变：

```text
Cargo.toml  76baed65b2c40b24ff5d92f2830ab88daceb2a95de1a4ffc86addcf9080d8874
Cargo.lock  46e0440091aafa15a621795ccfd2560d2a2ab53baa3fb0c1044bad79be1365b3
```

作者 81 项、独立 81 项和主树 81 项是重复覆盖，不能相加为新的测试数量；独立 3 项探针单列。此前 55 项和第一阶段 80 项也不加入本次数量。此次文档在主树验证之后新增，不属于上述 397 个输入或先前冻结记录。

## 验证限制

本次只在 macOS ARM64 实际运行，没有真实 Provider 或付费请求。最新 Linux、Windows 组合未验证；Windows 执行仍明确返回不支持。Windows Job Objects、PTY、主动 `setsid` 逃逸控制与宿主崩溃后的完整系统级治理尚未由这次交付证明。Unix 进程组不是安全沙箱，资源凭证只核算声明的任务数和内存，不是操作系统硬限制。

本记录不覆盖后续长会话优化、External Module 迁移及其与网页的组合，不改变旧压力测试失败、早期模型记录或各 Issue 的验收状态。
