# Issue #4：macOS 进程、取消与清理验收

本项交付公开 Runtime、Host、Device 路径上的进程归属、pipe/PTY、统一取消、原始执行预算和清理确认。验收主机为 macOS 27.0.1（26A434）、arm64，使用 Rust/Cargo 1.95.0。其他 macOS 架构未验证。

## 实际交付

Device 以 PID 和启动时间识别原进程，保留原 `Child`、进程组、工作目录句柄、监督线程和执行名额。正常结果必须等待原进程及声明受管后代停止、输出排空、句柄释放和线程真正退出。已观察到的进程即使后来离开原进程组，也必须先确认停止；Device 不向逃逸后的其他进程组发信号。

Host 的 `Profile` 显式选择 `ProcessIo::Pipes` 或 `ProcessIo::Pty { columns, rows }`。PTY 合并标准输出和标准错误，并把回显、换行转换计入同一份输出预算。Requests 将模式和终端尺寸纳入配置身份，旧授权凭据不能授权修改后的配置。

Runtime 把原进程身份关联到 Session、Task、attempt、Worker、执行和调用记录。取消、超时和输出超限沿用同一条 Host/Device 路径。登记、清理或持久记录失败时保留待核对状态和原执行名额。旧调用结果必须通过原执行身份检查，不能改写取消终态或后来的任务。

进程停止只证明资源已经停止，不证明文件写入等业务效果已撤销。业务效果不明继续保留 `outcome_unknown`。宿主强制退出后，可信宿主必须核对原进程并显式确认，不能只因重开或 Future 被丢弃就释放名额或重放任务。

## 已运行场景

| 条件 | 实际核对 |
| --- | --- |
| 任务与进程身份 | Pipes、PTY 均记录父/子/孙的 PID、启动时间、父进程、进程组及原任务和执行者。 |
| 正常、取消、超时、输出预算 | 公开 Runtime 运行八个组合；取消走公开入口，超时沿用原 Profile，输出在身份记录完成后释放。 |
| 启动中取消 | Device 在实际 pre-exec 启动关口取消并回收原 child；这时后代尚未创建。 |
| Worker 失败、宿主退出 | 注入 Provider panic、真实宿主正常退出、真实宿主 SIGKILL；后者核对原进程并通过可信宿主确认恢复。 |
| 排空、句柄和真实退出 | 超出保留上限后仍排空两路管道；PTY 计入原始字节；保留线程析构和工作目录句柄来确认真正退出后才释放名额。 |
| 身份替换 | 可控替换 PID 或启动时间，原身份不匹配时拒绝数字信号权限；无关存活进程组保留。 |
| 竞态与失败 | 自然退出/取消、启动前登记失败、启动后记录失败、记录回调 Err/panic、清理拒绝、查询错误、持久化失败。 |
| 迟到结果 | 旧租约和调用键提交真实成功值时返回 `StaleLease`；新任务的持久状态、进程身份和执行名额保持原归属。 |
| 认证本机 IPC | 实际 `session.prompt` / `session.cancel` 覆盖 Pipes 与 PTY，每个模式核对三代原进程。 |

本地最终接受记录的九项条件均为 `passed_for_declared_offline_scope`。Device 主执行和独立执行各有 93 项通过；Runtime 主执行和独立执行各有 23 项通过，各自核对 69 条原进程身份记录。重复运行不相加，身份记录数量不作为测试数量。

2026-10-21 另行使用 Magpie / DeepSeek V4.1 Flash 运行 Pipes、PTY 各四项正常/取消/超时/输出场景。八个场景满足各自预期，24 个原进程全部确认停止。取消、超时和输出超限场景保留业务效果不明状态，不记为普通任务成功。这轮真实模型记录与离线注入记录分别绑定自己的源码和程序。

## 复跑入口

在 macOS 的仓库根目录执行；命令本身不会调用真实模型：

```sh
cargo +1.95.0 test -p cayw-device --locked
cargo +1.95.0 test -p cayw-host --locked
cargo +1.95.0 test -p cayw-requests --locked
cargo +1.95.0 test -p cayw-runtime --test process_cleanup --locked
cargo +1.95.0 test -p cayw-runtime --test ipc_process_cleanup --locked
```

[Device 真实生命周期场景](../../crates/cayw-device/tests/macos_lifecycle.rs)、[Runtime 场景](../../crates/cayw-runtime/tests/process_cleanup.rs)和[认证 IPC 场景](../../crates/cayw-runtime/tests/ipc_process_cleanup.rs)定义实际进程程序、触发点和检查。原验收还保存了固定程序、构建输入、命令双流、注入位置、进程身份和独立复核。新运行应使用自己的输出目录，不覆盖原记录。

本机证据目录为 `/Volumes/ORICO/cayw-issue4-integration-evidence-20261020-a/`：

| 文件 | SHA-256 |
| --- | --- |
| `issue4-final-offline-accepted-v1.json` | `973c49cce88cdaba78a9774bc6f79a308859248b9b1f2441efa5d3df2a9a5e74` |
| `criterion-evidence-map-v5.json` | `31ca02c3393c87771673a2b7175d9b1d717d18a5c1c02667dd61b99920966e00` |

真实模型汇总为 `/Volumes/ORICO/cayw-live-234-20261021-a/final-real-model-report-v1.json`，SHA-256 为 `0dd204cac58b6479303d300ba2c215dac5ba45f9c44b9149e2da0692a7f55343`。这些路径只说明原材料在开发机的位置，不是公开下载链接。

## 管理边界与未验证范围

- Unix 进程组不是安全沙箱。观察前逃逸或在两次观察之间启动并退出的后代不在已记录覆盖范围内。
- Pipes 的可控改组成功；PTY session leader 的 `setpgid` 被系统拒绝，原失败记录保留，不声称 PTY 改组成功。
- Worker 失败由受管 Worker 中的 Provider panic 触发；没有声称另一个独立 Worker 操作系统进程崩溃已被验证。
- 宿主 SIGKILL 场景先显式释放测试夹具，再核对原进程并由可信宿主确认；没有自动处理宿主死亡的结论。
- 单项进程查询失败已覆盖，两种查询同时失败没有注入。同步记录回调不能被取消打断，宿主须为自己的存储调用设置时限。
- PID 复用通过可控身份替换验证，不依赖系统随机复用。停止进程不证明撤销外部效果。

交付分支的完整工作区回归另见 [#2/#3/#4 交付检查](issue-234-macos-delivery.md)。每份历史证据只证明其实际记录的源码、程序、平台与场景。
