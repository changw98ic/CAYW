# 2026-11-17 trace 链路问题修复验收

本次修复运行检查器误报成功、本地未知写入缺少结果核对入口、默认一秒等待造成额外模型往返三处问题。真实模型回归、原 trace 重放和全工作区检查全部通过。

## 源码、环境与证据

- 起始源码：`84370a61301faaef6ff17594886dbf72d7726d8b`，与开始修复时的远端 `dev` 相同。
- 修复源码：`acdf8f2`。真实回归捕获时的 18 个代码文件摘要与该提交一致；验收文档随后单独提交。
- 本机实际平台：Darwin `27.0.0`，`arm64`；使用 Rust `1.95.0`。未验证其他 macOS 架构。
- 真实 Provider：免密本机 `http://127.0.0.1:3425/v1`，模型 `opencode-go/deepseek-v4.1-flash`。
- 最终真实模型证据：`/Volumes/ORICO/cayw-trace-fixes-20261117-183505-verified/`。
- 该次运行的可执行文件副本：同级 `cayw-trace-fixes-20261117-183505-verified-binaries/`。`invocation.json` 记录程序摘要、配置、平台、验收脚本摘要和改动文件摘要。固定副本避免并行 Cargo 检查替换 `target/debug` 程序。
- 原始 A/B trace 重放及逐步核对：`/Volumes/ORICO/cayw-product-e2e-20261117-163404/evidence/chain-analysis-20261117/`。

首次真实回归也通过，记录保留在 `cayw-trace-fixes-20261117-183505/`。最终验收使用固定程序副本的第二次运行。第一次通用读取提示中，模型曾重复诊断 Luau 空值；第二次以明确的原始 `return load(...)` 脚本验证审批和等待，未改变产品审批策略。

## 修复结果

### 检查器按最终记录更新步骤

原实现只看外层工具回执的错误标志。返回运行中结果页后，后台最终失败或结果未知不会更新原步骤。

新实现按原执行身份合并执行终态、子调用、外层工具核对和子调用核对。运行中、未知、取消、中断与失败分别保留。进程固定能力的 `success=false` 显示失败，并在说明中保留 VM 状态和子调用；不使用任意业务 JSON 的 `success` 字段猜测结果。步骤耗时仍表示该次工具调用，证据游标指向更新后的权威记录。

增量分页读取前缀以恢复身份和开始时间，输出新增或被更新的步骤。合并按步骤 ID 和最新证据游标去重。删除无依据的 200000 条检查器上限，内部仍每批读取最多 1000 条，不积累原始事件和子调用大结果。测试确认显式 `usize::MAX` 可读取合法历史。

检查器协议从 2 升为 **3**，新增状态属于检查器响应合同；Core **4** 和 IPC **2** 保持各自版本。当前完整 TUI/WebUI 检查器渲染不在此实现范围。

原 A/B trace 通过新的转换代码重放后：

- 逐一核对 **107** 个绑定步骤，包含父调用和 wait。
- **5** 次非零退出的进程结果都显示 `failed`。
- 原父调用 `call_f09c6631abad49dfa7179c94` 显示 `outcome_unknown`，证据游标为 **25440**；原来错误显示 `success`、引用 **25264**。
- 原日志保持不变；重放不发起模型或工具执行。

重放入口与核对命令：

```sh
cargo +1.95.0 run -p cayw-run-inspector --example replay_runtime_events --locked -- capture.json SESSION_ID
python3 /Volumes/ORICO/cayw-product-e2e-20261117-163404/evidence/chain-analysis-20261117/verify_inspector_replay.py
```

### 本地核对后可在原工作区继续写入

新增 `cayw reconcile --inspect SESSION` 和 `--decision FILE`，详见[使用方法](../local-reconciliation.md)。命令沿唯一 Client 路由，将本地操作者已经独立核实的精确决定保存到工作区外的私有记录。Client 的核验器读取该记录，再进入既有 Runtime/Core 核对流程。

普通 RPC 的 actor/evidence 文本仍不能授权核对。原任务身份、原始来源、执行者停止、Cells 状态和进程清理检查全部保留。SDK 和完全显式 Headless 仍默认拒绝核对，由其宿主显式注入核验器。

本地 CLI 集成检查先在旧代码失败：真实进程写入一次后超时，工作区拒绝后续写入，本地核对命令不存在。修复后完整检查通过：拒绝无私有确认的 RPC、读取原证据、追加核对、相同决定幂等重提、拒绝伪造执行身份、在同一工作区完成新写入。旧未知事件保留，原进程未重跑。

真实模型回归同样执行了一次实际写入后挂起的 Node 进程，宿主按测试 profile 的一秒超时中断。测试操作者检查文件内容、摘要、原进程已停止和原任务清理完成，再确认已核实的失败及其副作用。确认后模型在同一工作区写入 `resumed.txt`。原 `effect.txt` 始终只有一行；原进程派发只有一次；重复提交只追加一条核对事件。原任务仍为 `outcome_unknown`，新决定没有改写原任务终态或原事件。

### 省略等待时间不再默认一秒交回模型

省略 `yield_time_ms` 时等待输出、审批、脚本主动让出、完成或取消。有宿主 `max_yield_time` 时使用该上限。显式 `0`、宿主最大值及完整可表示的毫秒值继续受支持，没有另加执行时限。

可控时钟回归先复现旧行为：跨过一秒的操作需要三次模型请求。修复后只需模型→工具→模型两次。检查还覆盖宿主四秒最大值、显式 `0`、完整 `u64` 值、审批通知和取消，Host 操作均只执行一次。

真实模型把两次各约两秒的实际进程操作放在同一 cell 中，省略等待时间。一次工具结果直接返回完成，额外 `wait` 为 **0**。这是本次实测场景，不是对任意长任务的固定延迟承诺。

## 实际验证

```sh
cargo +1.95.0 test -p cayw-cells --locked
cargo +1.95.0 test -p cayw-run-inspector --locked
cargo +1.95.0 test -p cayw-runtime --test run_inspector --locked
cargo +1.95.0 test -p cayw-runtime --test local_cli --locked
cargo +1.95.0 test -p cayw-trace-debug --test http --locked run_inspector
python3 scripts/validate-trace-fixes.py --out /Volumes/ORICO/cayw-trace-fixes-20261117-183505-verified
python3 /Volumes/ORICO/cayw-trace-fixes-20261117-183505-verified/evidence/verify-evidence.py
```

针对性测试通过：Cells 全套；检查器 **18** 项；Runtime 检查器 **4** 项；本地 CLI **21** 项；Trace HTTP 检查器 **2** 项。

核心真实回归包含允许审批、拒绝审批、跨旧一秒边界的批量操作、进程未知结果核对、幂等重提和原工作区继续写入：4 个会话、1368 条事件、15 次真实模型请求、9 次代码执行。

随后用同一组固定程序和原验收脚本补跑并发排队及取消恢复。10 个任务全部进入队列，观察屏障下为 2 个运行、8 个排队，最终全部完成并释放名额。写入完成后取消模型请求，原任务保持取消；重启与显式恢复没有新增模型请求或重做原写入，后续读取和新写入成功。调用入口、程序路径和源码记录在 `evidence/lifecycle-followup.py` 与 `lifecycle-followup-invocation.json`，结果为 `queue-result.json`、`cancel-result.json` 和 `lifecycle-followup-result.json`。取消时转发器的 BrokenPipe 日志对应被取消的响应连接，原请求的取消终态和后续恢复均已核对。

合并核对 **15** 个会话、**2124** 条连续且相互一致的事件、**30** 次真实模型请求、**12** 次代码执行。模型请求为 29 次完成、1 次取消；请求和代码执行全部有终态。取消请求的费用仍为未知。**13** 个实际开始的子调用全部结算，另 **1** 个被拒调用明确未执行。固定程序副本的摘要全部一致。测试拥有的服务已停止。

本轮没有重新生成最初的完整产品，也没有重新操作可视化编辑器网页。该产品原 trace 的状态投影已重放；本次新执行证明上述三个修复场景，不替代原产品全部功能验收。

## 新增检查和限制

- 本地命令参数和确认文件使用严格类型校验，依据是外部输入边界；错误参数明确拒绝。
- 私有确认记录须与原调用身份、固定 RPC actor 和结果逐字匹配，依据是核对权限边界；原有私有文件、链接和原子保存检查复用，不另定大小上限。没有确认或凭据损坏的请求会被拒绝，合法精确确认可通过和幂等重提。
- 检查器遇到缺失的 Core 前缀返回 `CorruptStore`，依据是追加日志的游标一致性；正常完整历史不受影响，不再把缺失日志当成完整成功视图。
- 新增生产数值限制、兜底、自动重试：**无**。没有收紧原本合法的产品输入。测试中的两秒进程与一秒故障 profile 是显式场景配置，不是产品默认限制。

## 工作区检查

- `cargo +1.95.0 fmt --all -- --check`：通过。
- `cargo +1.95.0 check --workspace --locked`：通过。
- `cargo +1.95.0 clippy --workspace --all-targets --locked -- -D warnings`：通过。
- `cargo +1.95.0 run -p cayw-agent --example offline --locked`：通过。
- `cargo +1.95.0 test --workspace --locked`：通过，进程退出码 0。

完整工作区日志保存在最终证据目录的 `evidence/workspace-tests.log`、`workspace-check.log`、`workspace-clippy.log`；离线示例日志为 `offline-example.log`。`evidence/command-results.json` 记录上述命令的实际退出码。
