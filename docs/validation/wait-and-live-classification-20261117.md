# 默认等待与真实调用分类修复

源码修复提交：`22ea1f2`。平台为 macOS arm64，Rust 1.95.0。

## 修改

- `CellTool` 的 exec/wait 在省略 `yield_time_ms` 时默认等待 1000 ms，若宿主设置了更小的 `max_yield_time`，则使用宿主值。依据是原真实 trace 中短执行耗时 44–468 ms，默认 0 导致每次都先返回空页。示例改为省略等待参数。
- 输出、完成、主动暂停或权限等待仍可提前返回。等到默认等待时间只返回当前进度，不取消执行；显式 0 和其他合法等待值仍按原值执行。Rust `ExecRequest`/`WaitRequest` 的显式参数含义不变。
- 验收脚本先记为 `not_run`。只有 prompt trace 出现模型请求 `started` 事件才记为 `live_model`。配置失败或仅预留请求仍为未运行；请求开始后的服务失败仍计入真实尝试，不要求先收到成功回复。

## 修复前后的回归

先添加回归并实际观察失败，再修改实现：

- 短操作经真实 Agent → CellTool → CellManager → 测试 Host 执行，修复前模型需要 3 轮，断言预期 2 轮而失败；修复后 2 轮完成，Host 只执行一次。另验证宿主将等待上限设为 500 ms 时省略参数仍合法。没有使用固定耗时断言。
- 模拟整个验收 CLI 流程，配置失败和仅预留请求两例在修复前被误标为 `live_model`；修复后三例全部通过，第三例确认已开始的 Provider 失败仍标为 `live_model`。这些是合成故障测试，不是付费调用证据。

实际执行并通过：

```sh
cargo +1.95.0 test -p cayw-cells --locked
cargo +1.95.0 test -p cayw-runtime --test cell_origins --test core_bounds --locked
cargo +1.95.0 clippy -p cayw-cells --all-targets --locked -- -D warnings
python3 scripts/test_validate_live_coding.py
CAYW_EVAL_RUSTC="$(rustup which --toolchain 1.95.0 rustc)" python3 scripts/test_product_eval.py
python3 -m py_compile scripts/validate-live-coding.py scripts/test_validate_live_coding.py
cargo +1.95.0 fmt --all -- --check
git diff --check
cargo +1.95.0 build -p cayw-runtime --bin cayw --locked
```

Cells 顶层测试 102 项通过；3 个专用子进程入口按原方式由对应父测试启动，顶层标为 ignored。Runtime 两组集成测试共 10 项通过，覆盖调用来源、交错审批、完整大结果及持久化。Python 分类测试 3 项、原生 Rust 评分器测试 66 项通过。

## 真实模型复验

使用重建后的 `target/debug/cayw` 和原独立 Client，在全新工作区调用本机免密 Magpie 的 `opencode-go/deepseek-v4.1-flash`。源码为 `22ea1f2`，运行配置、二进制摘要及原始日志在：

`/Volumes/ORICO/cayw-validation-20261117-144250-wait-classification/`

21 次 exec 的首次结果为 17 次 completed、3 次 failed、1 次 running，**没有 not_started**。失败包含模型传入不支持的 `max_depth`、固定测试配置禁止的过滤器，以及一次 Luau 字符串语法错误，均如实返回。另有一次未完成参数的模型调用保留在原始轨迹中，没有作为正常 exec 统计。

运行中发生的 2 次 wait 都针对正在等待 `cells.load` 审批的同一个长执行，不是此前短操作的空页查询。模型生成的 12 项测试通过，但 `cells.load` 审批在原有 120 秒权限等待期限后失败，最终未写出 `output/change.md`。因此本次 **整题失败**，没有独立产物通过结论，也没有自动重跑。

新脚本正确记录 `execution_kind=live_model`、`machine_passed=false`。原 Task 最终 failed、holds_slot=false；Session idle，无待审批或未结调用，Client 退出 0。此次额外暴露的审批阻塞未在这两项修改中处理。

证据入口：

- `review/invocation.json`：实际构建、模型、配置。
- `review/fix-verification.json`：首次返回状态与错误、审批和最终状态汇总。
- `review/evidence/005-prompt.stdout.log`、`005-prompt.stderr.log`：完整 trace 和权限等待错误。
- `review/evidence/failed-final-state.json`、`failed-final-task.json`：失败后的权威终态。
- `review/evidence/cayw-*-fix*.log` 等：修复前失败、修复后回归、构建与 Clippy 日志。
- `review/source-snapshot/`：本次修复和回归代码副本。

该真实样本证明短操作的首次返回行为已改变，以及请求开始后的失败仍被计为真实尝试；不将它写成完整产品题、审批或可视化插件通过。

## 边界变化

新增执行上限、重试、兜底：无。没有收紧允许输入。默认首次调用在暂时没有输出时可能等待最多 1 秒，而不是立即返回空页；这是响应等待策略，超时返回不会取消工作。调用者可显式传 0 获得立即返回。

分类新增的判断依据是实际模型请求开始记录，用于避免把启动失败算成真实调用；未更改 Runtime 的请求、授权、取消或结果终态。
