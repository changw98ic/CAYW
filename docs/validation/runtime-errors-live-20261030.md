# Runtime 错误与 Core 写入的真实模型验证

本记录从提交 `031fe055b72fd403314615f979426d983bc1befa` 开始。验证入口为本轮工作树新增的 `runtime_error_live` 示例和 `scripts/validate-runtime-errors-live.py`。本轮没有修改 Runtime、Core、Queue 或独立客户端的生产逻辑，没有新增依赖。每轮的 `source-manifest.json` 绑定实际源码、锁文件及三个执行文件的 SHA-256。平台为本机 `macOS-27.0.1-arm64-arm-64bit-Mach-O`，其他 macOS 架构未验证。

正常链路、存储故障、Worker panic、Core 写入四类场景已完成，SDK 与 IPC 均有实际证据。先前因额外审批而取消的文件任务也已在原 Session 内完成。过程中的 HTTP 400 未知用量继续保留，不把它们记为零用量或零费用。最终汇总位于 `.local/runtime-errors-live-20261030-final/summary.json`。

## 正常链路与原文件任务完成

正常链路证据位于 `.local/runtime-errors-live-20261030-run04-normal/`。每项先创建新 Session，`prompt` 实际入队并执行；模型通过唯一的 `execute_code` 入口调用已精确授权的 `fs.read`、`fs.write`，将 49 字节验证文件完整写入 `proof.txt`，再返回文件中的随机 nonce。nonce 未出现在用户提示里。两个 Task 均 completed，工具结果与工具调用配对，两个能力均有开始和完成记录，原 Task、worker、Attempt 和 fence 一致，无待审批或未结算能力。

| 模式 | Session | Task | 模型请求 | reported token | 原事件数 |
| --- | --- | --- | ---: | ---: | ---: |
| SDK | `60eca37e-9d52-4d70-a7e0-d7ffab7ee413` | `557feff6-fdb6-438c-8f30-18890e27f44f` | 2 | 3,543 | 112 |
| IPC | `e08c2834-eaf9-496d-894f-9c1aab251ca5` | `1d0440cf-509d-4069-af45-bfcdb8d58154` | 2 | 3,671 | 128 |

SDK 使用全新进程重新读取；IPC 实际结束服务进程（exit 0），再以同一数据库启动服务并读取，第二个服务也 exit 0。前后 Session、完整 Task 和完整事件完全相同，游标从 1 连续到末尾，读取没有模型请求。SDK 查询进程和 inspect 进程未带模型凭据；IPC 服务启动仍带模型凭据，不能将客户端无模型凭据外推为服务端无须凭据。

取消过的原文件工作仍在 Session `d8cda7b9-85ad-4abd-8c48-b9b87c728d18` 中完成。原 Task `b313a782-b7f9-4155-a3cc-b3a6b2187d6d` 保留 cancelled；新的明确输入对应 Task `4a946ad5-0e6d-4c4a-bda3-5ad2c54ca265` completed。原消息和完整事件前缀不变；新 Task 未重放原 cell。`proof.txt` 与原 `source.txt` 的 SHA-256 均为 `996b1975a6c172c5b1e4d979a70e1ebfc4ef20fb58299b01ac2b4626fe32bf4e`，模型返回了原 nonce。继续执行复用原预算根，新增 2 次请求、8,157 token；整个原 Session 的 6 次请求共结算 17,626 token、预留 0、budget_unknown=false。

## 已完成的真实故障场景

Magpie 版本为 `0.1.1953`。请求通过本机 `http://127.0.0.1:3425/v1`，明确选择 `opencode-go/deepseek-v4.1-flash`。实际响应中的模型名称为 `deepseek-v4.1-flash`。转发器保留完整请求与响应字节、响应身份和用量，认证请求头不进入证据。模型只接收验证用提示和验证目录内容。

原始证据目录为 `.local/runtime-errors-live-20261030-run03-faults/`。四个场景均返回成功，合计 4 次实际模型请求、5,000 token，模型用量未知数为 0。每个请求的最终 reported 用量、响应 ID、响应模型与 Runtime 模型账本逐项相等。每个成功模型请求均无剩余预留；金额没有可信价格来源，仍为未知。

| 场景 | Session | Task | 模型 token | 最终任务状态 |
| --- | --- | --- | ---: | --- |
| 存储 SDK | `631935d3-c04f-401f-8464-41e07e7f33e0` | `5820ddb2-306c-4bc9-9d1b-71e4a7afcce4` | 1,249 | completed |
| 存储 IPC | `ef31f9e9-8377-4757-8470-7b1c121b15a2` | `ff8ff916-96d4-4e0f-9549-ed4c318842f8` | 1,249 | completed |
| panic SDK | `6b915226-9032-4282-bcd2-4ee9d4ce9e7f` | `a38b02ab-471b-4c1d-9a45-7229b0f7c140` | 1,247 | draining，保留名额 |
| panic IPC | `7388eaec-c5ab-4d53-8a22-cf1fe57df490` | `bf63d684-4e9c-4239-90c0-9ee12f1085d5` | 1,255 | draining，保留名额 |

### 存储故障

SDK 和经过认证的实际 IPC 分别先完成一个真实模型任务，再从该 Session 派生四个故障用分支。分支共用原预算树。

- 将两个独立排队记录的 `input_json`、`intent` 实际改为 `{`。`task.get`、`session.tasks`、`task.resume` 均返回 `storage_corrupt`，保留 `EOF while parsing an object at line 1 column 1`。IPC 另保留方法与任务/Session 上下文。坏记录仍为 queued、fence 为 0、没有执行者或 Attempt、未占名额；Session 和事件不变。
- 在新任务实际插入时通过 SQLite 触发器拒绝写入。错误为 `storage_error`，保留 `live validation forced queue write failure`。没有新增任务行，Session 和事件不变。
- 用另一个 SQLite 连接持有 `BEGIN IMMEDIATE` 写锁。入队实际返回 `storage_busy`，保留 `database is locked`。释放验证连接自己的写锁后，仍无新增任务行。
- 上述故障的 `outcome_unknown` 均为 false。每个场景只有最初一次真实模型请求，故障后新增模型请求为 0；没有修复、删除或重放坏记录。
- 原真实任务的 Session、Task、完整事件和模型账本在新进程读取后完全相同。损坏的分支记录继续保留。

这里实际制造的是行写入约束失败与写锁竞争，没有模拟物理磁盘损坏、拔盘或磁盘空间耗尽。

### Worker panic

两个场景都先收到真实模型的完整回复并结算用量，再在已有 `core.attempt_terminal.committed` 故障点真实 panic。该位置位于 Agent 返回后的 worker 清理阶段，经过 Runtime 的实际 panic 捕获。

- SDK 和 IPC 返回 `worker_panicked_outcome_unknown`，`outcome_unknown=true`。原因包含中文、转义符、换行和双向控制字符，返回错误及持久化任务原因保留转义文本，未带入这些实际控制字符。
- Task 保持 draining，`holds_slot=true`，原 worker、Attempt、fence 保留，health 的 running 为 1。模型已完成结算与 worker 清理结果未知是两件独立的事。
- 该提交故障点仍持有 Core 连接锁，因此原连接读取和入队返回 `session_error`，具体原因为 `The session store mutex is poisoned`。验证程序保留该错误，不清除锁中毒，也不把它写成成功。
- 新开 Runtime 后，已落盘的回复、完整历史、模型用量可读取。新任务返回 `busy`；再次 resume 原 draining 任务返回 `invalid_params`。这两个新请求的错误为已知拒绝，不会清除原 panic 的未知状态。
- 另一个全新进程再次读取时，Session、Task、完整事件和账本完全相等，任务仍保留名额，没有额外模型请求。

这证明故障被报告并保留待核对状态，不代表 panic 清理已成功或原任务已恢复完成。故障验收主动要求保留该状态；没有执行恢复或解除名额。

## 已完成的 Core 写入检查

两份已完成的真实模型历史分别来自上述存储 SDK、存储 IPC 的 baseline。先完成原任务的新进程重开核对，再关闭 Runtime，通过两个独立 `SessionStore` 连接运行写入检查。证据为 `run03-faults/real-history-writes-summary.json` 和两个存储目录中的 `real-history-writes.json`。检查没有请求模型，没有增加预算根。

两份正常文件任务也完成同样的写入检查，证据为 `run04-normal/sdk-normal/writes.json`、`run04-normal/ipc-normal/writes.json`。SDK 保留 6 条消息，cursor 从 112 增至 117；IPC 保留 6 条消息，cursor 从 128 增至 133。最终又用全新 inspect 进程读取，确认原 Task、模型账本、消息和原事件前缀不变，每份只新增 5 条显式验证事件。

两份历史各有 4 条消息，初始 cursor 为 32。检查显式开启一个验证 Attempt 后，分别对以下两条写入路径制造延迟外键错误，使 SQLite 在 COMMIT 时实际失败：

- `append_host_event` 的轻量游标更新路径，失败目标 cursor 为 34。
- `finish_attempt` 的普通 persist/mutate 路径，失败目标 cursor 为 37。

两种失败都返回 `FOREIGN KEY constraint failed`。失败后，本连接与独立连接读取的完整 snapshot 和完整事件均等于失败前状态；普通结束路径未提前清除 Attempt。移除验证触发器后，显式写入一次，事件只新增一次。Host 事件先于 control 事件，游标连续；最终 cursor 为 37，验证 Attempt 已结束，原消息完全相同。过期游标、伪造 lease 和空 actor 均被拒绝，未修改历史。

这直接证明提交失败后没有对外发布错误状态。没有公开缓存计数器，因此没有直接观测内部缓存项是否移除，也不声称该入口直接证明了其他 Session 缓存项保留。后者由本轮已执行的 Core 单元测试另行覆盖。

## 过程失败与预算

用户授权本轮真实模型验证总上限为 100,000,000 token。

首轮 `.local/runtime-errors-live-20261030-run01/` 的验证转发器重复发送 `host` 和 `Host` 两个请求头。6 次请求均收到 HTTP 400，没有真实模型回复或用量报告。使用只连接本地接收器的检查复现了重复头，修正后请求只带一个 Host。原始请求、响应和数据库未删除。

6 个首轮 Session 的公开模型账本已用新进程逐个读取，均保留 1,000,000 token 未知预留，合计 6,000,000。`settled_tokens=0` 只表示尚未结算，不能解释为实际用量或费用为零。首轮任务没有重放，也没有释放这些预留。原始身份和账本见 `run01/failed-run-ledgers.json`，原因及本地复现见 `run01/relay-diagnosis.json`。

第二轮 `.local/runtime-errors-live-20261030-run02/` 已收到 4 次真实模型响应，实际 reported 总用量为 9,469 token。模型先做工具发现，之后 `fs.read` 已成功；脚本又调用 `store("src", r, 0)`，该帮助函数实际映射为 `cells.store` 能力。宿主只预授权了精确的 `fs.read`、`fs.write`，因此 `cells.store` 进入审批等待。15 秒 exec 与 20 秒 wait 都在等待同一审批，并非文件读取过慢。第 5 次 TurnStart 超过配置的 4 轮限制后触发取消。

取消记录将该 `cells.store` 明确结算为 `not_executed`，原因是 `capability cancelled before permission`，并关闭待处理审批。新进程读取确认模型 4 次请求已结算 9,469 token、预留为 0、budget_unknown 为 false。Task 为 cancelled。这一结果不能记为正常任务通过。

验证提示随后改为在单个 cell 中使用局部变量，把 `fs.read` 返回的 `content` 直接交给 `fs.write`。权限和轮数没有扩大。原 Session 的继续执行前提是确认无 pending approval、无未结算能力、无未知模型预留；不会重放原取消任务，也不会用新 Session 代替原工程任务的完成。

后续独立故障场景使用新数据库，继续计入同一 100m 总预算。首轮 6,000,000 token 未知预留没有因新场景或新 Session 被抵扣或覆盖。历史其他任务的未知预算不在本轮授权内，也未打开、修改或解除。

## 最终用量与证据核对

最终离线审计重新打开了正常 SDK、正常 IPC、存储 SDK、存储 IPC、panic SDK、panic IPC 及原文件任务的 7 份 Session。所有新进程检查均成功，没有新增模型请求。源码 26 个明确路径、三个实际二进制的指纹与运行时清单相等。各轮原始报告、源码清单和辅助执行脚本的哈希收录在最终 `summary.json`。

| 项目 | 实际值 |
| --- | ---: |
| 用户授权总上限 | 100,000,000 token |
| HTTP POST 总数（含首轮拒绝） | 20 |
| 有完整 reported 用量的响应 | 14 |
| 已报告并结算 token 总数 | 29,840 |
| 用量未知的请求 | 6 |
| 仍保留的未知预留 | 6,000,000 token |
| 已报告用量加未知预留 | 6,029,840 token |
| 按上述数值保留后的剩余额度 | 93,970,160 token |
| 历次独立预算根 | 13，每根配置 1,000,000 token |

13,000,000 是历史累计配置额度，不是实际消耗。实际预算核对始终同时保留 29,840 已报告 token 与 6,000,000 未知预留。金额仍为未知。首轮未知预留和 panic 的 worker 未知状态是不同义务，均未解除。用户先前其他 Session 的未知预留未纳入本轮额度，也未修改。

最终正常与故障场景通过，不会覆盖首轮 HTTP 400 或第二轮取消的事实。本记录不证明物理磁盘满、真实磁盘 I/O 损坏、其他 macOS 架构或完整 Issue 的所有验收条件。

## 执行入口

从本工作树根目录构建三个执行文件后，可在新的空输出路径运行完整验证。该命令会发出真实模型请求，必须具有当次明确授权。脚本默认不重试；有任何场景失败就停止后续真实场景。

```sh
python3 -I scripts/validate-runtime-errors-live.py \
  --runtime "$PWD/target/debug/cayw" \
  --client "$PWD/target/debug/cayw-client" \
  --probe "$PWD/target/debug/examples/runtime_error_live" \
  --out "$PWD/.local/runtime-errors-live-new-run" \
  --token-budget 100000000 \
  --base-url http://127.0.0.1:3425/v1 \
  --model opencode-go/deepseek-v4.1-flash
```

本次分段执行使用以下本地辅助脚本，原文及哈希已保存。分段是为了保留已取得的真实结果，不重复调用已成功的模型请求。

```sh
python3 -I .local/runtime-errors-live-20261030-preparation/inspect-run01.py
python3 -I .local/runtime-errors-live-20261030-preparation/run-faults.py
python3 -I .local/runtime-errors-live-20261030-preparation/verify-real-history-writes.py
python3 -I .local/runtime-errors-live-20261030-preparation/run-normal-completion.py
python3 -I .local/runtime-errors-live-20261030-preparation/final-audit.py
```

`run03-faults/verifier-at-run.py` 保存该轮实际加载的完整验证脚本，哈希与该轮 source-manifest 相同。之后脚本只改过失败日志的展示、正常提示和失败后停止逻辑；生产源码与三个二进制在这些分段运行中均未变化。最终脚本指纹见 run04 的清单。

## 离线检查与入口自检

Rust 工具链为 `1.95.0`。下列命令已成功执行，原始日志在 `.local/runtime-errors-live-20261030-preparation/checks/`。

```sh
cargo +1.95.0 build -p cayw-runtime --bin cayw --locked --offline
cargo +1.95.0 build --manifest-path examples/headless-client/Cargo.toml --locked --offline --target-dir target
cargo +1.95.0 build -p cayw-runtime --example runtime_error_live --features fault-injection --locked --offline
cargo +1.95.0 clippy -p cayw-runtime --example runtime_error_live --features fault-injection --locked --offline -- -D warnings
cargo +1.95.0 test -p cayw-runtime --features fault-injection --test worker_panic --test input --locked --offline
cargo +1.95.0 test -p cayw-core --locked --offline
cargo +1.95.0 fmt --all -- --check
```

Runtime 的 input 与 worker_panic 合计 14 项通过；Core 本次执行汇总 134 项通过。它们属于离线回归，不能替代真实模型场景。

本地模拟 HTTP 仅用于验证新入口本身，证据在 preparation 的 `probe-smoke/`、`probe-smoke-panic-r2/`。最初 panic 自检因直接读取原中毒连接而失败；修改验证入口保留原错误并新开连接后，SDK/IPC 的存储和 panic 四项自检均成功。模拟请求从未连接模型网关。空 Session 的 writes 自检也单独保留在 `preparation/smoke/`，不作为真实模型写入证据。
