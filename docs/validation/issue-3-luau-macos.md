# Issue #3：macOS Luau 原型与测量方法

本项选择 Luau 作为本机 CodeMode 的脚本运行方式。有限脚本通过公开的 `cayw_codemode::CodeMode` 调用受控 Rust Host，权限决定、调用开始和结算沿用 Host 的实际路径。选择依据是这条路径能够运行真实的计算和文件组合，并对错误、预算、取消与到期保留可复核结果；不以语言微基准证明整个 Agent 更快。

本文说明技术选择和复跑方法，不代替某次运行的接受决定。范围只包括实际执行记录所列的 macOS 主机、架构、源码和程序。全部十一项主线的最终同源码真实 Provider 验收另行执行。

## 固定清单与实际结果

[脚本与期望](../../crates/cayw-verify-tool/src/cases.rs)定义十八个场景。每个样本保存完整脚本及摘要、有限工具的完整签名、输入、精确授权、各层限制和预期结果。[执行与独立核对](../../crates/cayw-verify-tool/src/runner.rs)从同一个实际 Host 捕获签名，读取工作目录前后状态，并核对原调用、授权决定和同步保存的开始/结算记录。

| 场景 | 检查内容 |
| --- | --- |
| `cold_process_first_success` | 新的子进程首次输出和结果；保存原进程退出与清理记录。 |
| `warm_process_first_success` | 同进程、同 CodeMode 对象先执行 warmup，再执行目标脚本。 |
| `file_pipeline` | 真实 UTF-8 文件的写、读、列目录和状态查询，独立核对内容和摘要。 |
| `syntax_failure` | 原语法错误在 Host 分派前停止，没有文件效果。 |
| `syntax_repair_success` | 在原工作目录执行固定修正，保存与原失败的身份配对。 |
| `loop_budget` | 长循环达到指令检查点预算后停止。 |
| `cancel_pending_host` | 原 Host Future 已进入后主动取消，等待同一调用结算并保留 unknown。 |
| `host_permission_denied` | 未授权写入被拒绝，没有调用开始或文件效果。 |
| `sandbox_globals` | 执行检查不可用的脚本全局对象和实际注册的目录。 |
| `unknown_capability` | 不存在的工具在 Host 分派前被拒绝。 |
| `invalid_arguments` | 数字路径在 Host 分派前被拒绝。 |
| `file_calculation` | 真实写入并读取 1 至 10，脚本计算偶数总和；独立期望为 30、5 项、21 字节。 |
| `capability_signatures` | 在 VM 内读取四个文件工具的完整名称、说明和参数结构，与同一 Host 核对。 |
| `output_budget` | 完成一次写入与小输出后触发输出超限，后续已授权写入未执行。 |
| `memory_budget` | 完成一次真实写入后，在 1 MiB VM 限制下申请 8 MiB，后续已授权写入未执行。 |
| `host_call_budget` | 两次真实调用完成后拒绝第三次，即使第三次已获授权。 |
| `active_timeout` | 长循环到期，返回明确 Timeout，且无 Host 效果。 |
| `timeout_pending_host` | 执行到期取消原待结算 Host，等待原工作停止，保留 unknown 和对应的原恢复记录。 |

这里的 `passed` 表示实际结果符合该场景的期望。语法错误、拒绝、取消和超限仍保留自己的实际错误种类，不能据此写成普通任务成功。`unknown` 不因测试知道 fixture 没有网络而被清除。

任务文件保存为报告包的实际附件；工作目录快照另列 Host 自有的锁和恢复内容。成功写入要求原恢复状态存在且没有 pending；被中断的外部调用要求 pending 中的工作区、代次、原 audit ID、能力名和参数摘要一致。读取器检查保存字节与样本的一致性，并重新计算固定场景条件。它不会重新运行脚本，也不以可修改的外层摘要代替结果检查。

## 测量边界

每个场景至少采样三次。同一次正式报告使用同机、同一固定程序、同一套能力适配和每个场景固定的权限/预算。所有样本按执行顺序保存，包含错误、到期、取消和修正，不挑选最快结果。

- 冷启动创建新的子进程。`parent_spawn_to_exit_ns` 包含加载程序、参数处理、Tokio、Host/CodeMode 准备、首次执行、序列化和退出；不清空操作系统缓存。
- 暖启动复用当前父进程。warmup 和下一次脚本复用同一个 CodeMode 对象；每次 `execute` 都新建 VM，没有 VM 缓存复用的结论。
- `host_setup_ns` 与 `codemode_setup_ns` 分别测公开构造函数；后者尚未创建 VM。
- `first_execute_ns` 包含 VM 创建、编译、脚本运行和 VM 销毁。公开接口不能分开这些内部阶段，所以 `vm_setup_only_ns` 保持 null。
- `case_execute_ns` 测目标执行；`case_total_ns` 包含 fixture 准备与结果检查；`scenario_total` 还包含报告和附件工作。暖样本另存首轮 warmup，不能把下一轮当作首执行。
- 时间使用单调时钟，单位为纳秒。汇总保留每阶段的最小值、中位数、p95 和最大值，没有固定毫秒通过门槛。

macOS 的 `getrusage(RUSAGE_SELF).ru_maxrss` 以字节记录进程生命周期峰值，包含初始化和更早样本。它不是单脚本分配量、不是前后差值，也不是硬限制。VM 分配器的 1 MiB 限制由实际超限路径验证；本项没有测单脚本分配峰值。输出和调用次数限制分别通过实际超限及“后续已授权操作没有执行”核对。

原 Host Future 与执行截止时间在执行期间保留，不启动替代调用；CodeMode 的停止结算保留同一个 shutdown deadline。主动取消读取原调用者令牌为已取消；到期路径读取该令牌仍未取消，并同时要求原 Host 子令牌观察到取消、调用已同步结算和实际 Timeout。读回与重新等待不创建新的执行预算。Cells 分页暂停和完整 Runtime 的恢复矩阵属于后续对应任务的验收。

## 同机复跑与来源

在仓库根目录准备新的输出目录，使用 Rust/Cargo 1.95+：

```sh
CARGO_TARGET_DIR=/absolute/new-target \
CAYW_SOURCE_MANIFEST_PATH=/absolute/source-manifest.json \
cargo +1.95.0 build -p cayw-verify-tool --locked --offline --message-format=json

/absolute/new-target/debug/cayw-verify-tool luau \
  --samples 3 --out /absolute/new-luau-report
/absolute/new-target/debug/cayw-verify-tool read /absolute/new-luau-report

cargo +1.95.0 test -p cayw-verify-tool --locked --offline -- --test-threads=1
```

源码清单格式和构建 receipt 的检查边界见 [VerifyTool 说明](../../crates/cayw-verify-tool/README.md)。保存实际 Cargo 参数、退出码、完整双流、锁文件、feature、编译器、macOS 版本、架构和二进制摘要。具名源码及公开 Cargo/`.d` 观察能够核对已列输入，但不追踪每个编译器、宏、原生工具或链接器的全部输入；完整构建输入仍标 pending。不能因此发布完整来源或性能优势结论。

2026-10-20 续接作业的原始材料位于本机独立目录 `/Volumes/ORICO/cayw-issue3-continuation-20261020-a/`，与 #2 v11 的固定程序和证据分开。读取这些记录时先核对其实际存在、摘要和命令终态，不将本文当作运行已经完成的声明：

- `commands.jsonl` 与各 label 的 `.stdout`、`.stderr`、`.source-before.json`、`.source-after.json`：每条实际命令及具名输入的前后观察。
- `measurements-red-v1`、`measurements-green-v1`、`measurements-green-v2`：缺失场景、Host 元数据分区和固定计算脚本的原失败及修复过程。
- `bundle-red-v1`、`bundle-green-v1`：实际结果文件附件与修改内容后重新核对的反例。
- `recovery-red-v1`、`recovery-green-v1`：成功写入后删除或损坏原恢复状态的反例。
- `deadline-origin-red-v1` 和后续包测试：原调用者取消状态的缺失与实际观察。
- `final-build-v1`、`formal-luau-v1/`、`formal-read-v1`：正式程序、每场景三次的原样本与独立读回；`formal-luau-v1/meta.json` 保存完整运行配置，`samples.jsonl` 和各样本附件保存全部输入及结果，`summary.json` 只从这些样本推导统计。
- `independent/`：另一执行者使用同一固定程序保留的命令、双流和内容反例，完成情况以其索引为准。
- `issue3-offline-accepted-v1.json`：逐条条件的最终本地决定；不存在时不能声称本项已接受。

这些绝对路径是开发机的证据位置，不是其他机器的可用入口。迁移证据时保留相对目录、原 bytes/hash 和固定程序，重新执行仍需使用新的目录。

## 取舍和未验证范围

Luau 沿用现有 Rust/Host 路径，能够表达分支、循环和多调用组合，并在一份报告中保留限额和副作用结果。每次新 VM 提供隔离，也带来每次初始化与编译成本；不能把进程已经暖启动解释成没有这些成本。

Monty 没有本次同机对照数据。真实模型生成脚本的成功率、自动修复成本、完整 Agent 延迟、真实 Provider 使用量、其他 macOS 架构和全部崩溃窗口都未由这些离线场景证明。未测 token 和费用保持 null，不写零费用。fixture 的默认拒绝与精确授权配置不等于产品用户审批。
