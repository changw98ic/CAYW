# Issue #31：审阅问题修复与重新验收

记录日期：2026-10-24 至 2026-10-25。本轮处理交付审阅确认的五项问题，并重新执行 [Issue #31](https://github.com/changw98ic/CAYW/issues/31) 的八项验收流程。平台为 macOS 27.0.1（26A434）ARM64，Rust/Cargo 1.95.0，默认 feature。网页由 Playwright MCP 实际操作。

工作树为 `/Volumes/ORICO/CAYW/.claude/worktrees/issue31-trace-debug`，分支为 `plugin/trace-debug`，基线为 `b14b401f37af877e03bbf11594a05dadb1289d47`。本轮修改生产代码和回归测试。未提交、推送、合并或修改远端 Issue。

本轮没有新增真实模型请求。网页演示使用固定离线 Provider，但 Runtime、Host 文件读写、Cells、Luau、认证 IPC 和插件均实际执行。大页进程验证使用禁止模型请求的 `NoRequests` Provider。此前真实模型结果仍见 [Magpie 记录](issue-31-magpie-macos.md) 和 [Haiku 重测](issue-31-haiku-live-retest-macos.md)，不重复计入本轮。

## 五项问题及修复

| 审阅问题 | 修复 | 本轮验证 |
| --- | --- | --- |
| 上游对 `context` 的修改在重新计算时丢失 | 真实执行和本地计算统一使用 `script_for_plan`。从该版本入口，在同一个 Luau 执行环境内重新计算整条路径 | 原始输出 9；只加注释后仍为 9；参数翻倍后为 18；撤销后恢复 9 |
| `output` 与 `state` 共享同一个表时，分开还原 JSON 破坏对象关系 | 删除逐节点启动执行环境、分别注入输入和状态、复用前缀的旧路径。前置节点也重新计算，以重建共享对象 | 同时覆盖直接共享 `input/state` 和通过 `/payload` 映射的嵌套 `state/context` 引用；编辑和撤销结果均正确，原始结果与事件保持不变 |
| 合法 `CapabilityReconciled` 没有活动 attempt，插件将其拒绝 | 接受 Core 在原尝试结束后记录的核对事件。同一未知调用可更新为确定结果；不同调用身份或替换已有不同确定结果仍拒绝 | 完成、失败、未执行三类结果均覆盖；同页后续事件能继续保存，关闭并重新打开追踪文件后游标和事件仍一致 |
| 固定 32 条事件超过 IPC 1 MiB 限制后反复卡在同页 | 仅对只读 `session.events` 的明确超限错误减半页大小。失败页不推进游标，成功后恢复 32 条。单条本身超限则显示具体限制并保留位置 | 集成测试及最终插件独立进程均连续读完游标 1–25，保留全部 24 条 48 KiB 合成事件，无连接错误 |
| 保存 A 期间输入 B，切换节点后成功回包删除 B | 成功回包比较原节点自己的当前草稿与已发送内容。保留较新的草稿，并更新它的保存版本 | Playwright 实际执行“保存 A → 输入 B → 切节点 → A 返回 → 切回 → 保存 B”。B 保留且再次保存为 v8，计算成功 |

三项共享数据回归先在旧实现运行，均稳定失败：结果为 1，预期为 9。原日志保留为 `replay-before-fix-r02.log`，没有用修复后的结果覆盖。

外部调用仍按节点、能力名称、参数和节点内顺序匹配捕获响应。前置节点重新计算不会重做业务读写。每个步骤均标记 `reused=false`。另外补测节点普通 `cayw.emit` 输出可以继续使用，以及两个节点不能互借捕获响应或证据编号。

本轮删除了已由同一个执行脚本完成的重复分支检查，没有增加执行快照格式、额外重试框架、第二份草稿缓存或新的工具入口。Core 已完成的核对事实不在插件重复展开校验。取消、调用身份、捕获匹配、资源限制和未知业务效果的约束仍保留。

## 当前构建与复跑入口

最终验收目录为 `/out/issue31-fix-acceptance-r02`，运行目录为 `/out/issue31-fix-browser-r02`。本机 `/out` 对应 `/Volumes/ORICO`。固定程序如下：

| 程序 | SHA-256 |
| --- | --- |
| `bin/demo` | `02d81523b2bb472a84a7263b05784cfcd967923171f6ed663a80f528dd5cfc92` |
| `bin/cayw-trace-debug` | `1a855e80e88c2d2a29993acb9aff6ff5c9bca452025e26ece410dbd793871deb` |

在工作树执行：

```sh
cargo +1.95.0 test -p cayw-trace-debug --locked --offline
cargo +1.95.0 test --workspace --locked --offline
cargo +1.95.0 fmt --all -- --check
cargo +1.95.0 clippy --workspace --all-targets --locked --offline -- -D warnings
cargo +1.95.0 build -p cayw-trace-debug --bin cayw-trace-debug --example demo --locked --offline
```

这些命令用于开发者测试和构建验收程序。网页保存、撤销路径自身不调用这些命令。

启动新演示：

```sh
python3 -I scripts/validate-trace-debug.py serve \
  --demo /out/issue31-fix-acceptance-r02/bin/demo \
  --plugin /out/issue31-fix-acceptance-r02/bin/cayw-trace-debug \
  --output /absolute/new/issue31-review-fixes
```

输出目录必须不存在。读取 `ready.json` 的网页地址。通过 Playwright MCP 在导航前执行 `browser-audit.js`，登录后执行 `browser-actions.js`。网页令牌是脚本内公开的合成测试数据 `issue31-public-web-fixture-1111111111111111`。

页面连接后，显式提交任务：

```sh
python3 -I scripts/validate-trace-debug.py task \
  --output /absolute/new/issue31-review-fixes --wait-read
```

执行 `browser-waiting.js`，确认两图和时间线显示输入节点等待。在 180 秒内批准一次准确的合成文件读取：

```sh
python3 -I scripts/validate-trace-debug.py approve-read \
  --output /absolute/new/issue31-review-fixes
```

选择 `condition`，实际展开错误卡的原始事件 JSON，并记录原始失败。`browser-full-replay.js` 执行字段修复、阈值修改、撤销和缺上下文流程；`browser-draft-race.js` 执行保存期间继续输入并切换节点的回归。后者仅在浏览器暂扣一次保存请求，随后放行到真实插件 HTTP 接口。

保存浏览器原件后关闭页面，停用该次插件，再执行普通任务。`headless_after_plugin.py` 保存本轮准确方法；用于新目录时需更改其中的目录常量。最后使用 `validate-trace-debug.py shutdown --output NEW_DIRECTORY` 关闭该次 Runtime。

## 八项验收及网页结果

| Issue 标准 | 本轮实际结果 | 原件 |
| --- | --- | --- |
| 1. 两图随实际追踪增量更新，关联同一运行，无需刷新 | 任务前页面为空且已连接。输入节点等待读取批准时，两图和时间线显示等待；批准后更新为条件失败。始终只有一次页面导航 | `browser-audit.json`、`browser-validation.json` 的 `waiting`、`failed` |
| 2. 错误展示位置、关键输入、摘要与原始证据 | 识别 `condition` 对不存在字段的比较错误，显示 `value=7`、前置状态和游标 87；实际展开 `execution_finished` 的原始 JSON | `failed.rawEvidenceOpened=true` |
| 3. 保存后自动重新计算并推动状态流转 | `input.amount` 改成 `input.value` 后，仅一次保存产生 v2。全部前置与下游节点在同一执行环境重新计算 | `high`、保存请求及返回文档 |
| 4. 新内容改变分支或输出，图与记录一致 | v2 阈值 5 走 high，v3 阈值 20 走 low；路径与最终分类同步改变。撤销创建 v4 并恢复 high | `high`、`low`、`undo` |
| 5. 自动流转没有项目编译前置，无需另点运行 | 保存与撤销进入本地 Luau 计算，只回答捕获响应。7 次网页 POST 均返回 200；运行收据没有新增任务、事件、能力调用或写入 | `save-path-review.json`、修改前后收据 |
| 6. 可对比路径、输出、错误，区分原始与重放 | v1 保留为 observed 失败；后续轮次标记 replay。实际查看错误变化和高低分支、输出对比；撤销不改写原失败 | `high.comparison`、`low.comparison`、`undo.oldFailedPreserved` |
| 7. 缺上下文显示具体缺口，区分事实和推断 | 当前 high 路径未经过 low，保存 low 产生 v5 blocked，步骤与最终输出均为空，明确指出未到达该编辑节点。撤销后 v6 恢复 high | `missingContext`、`restored` |
| 8. 插件停用后普通 IPC 和无界面任务仍可用 | 确认插件进程已退出后提交第二个普通任务，任务完成；新读取执行和图执行均 completed，业务写入仍为一次 | `headless-assertions.json`、`headless-after-events.json` |

Session 为 `17a75261-a7e1-4431-92ef-310175331a87`，首任务为 `61e5cc56-4470-4d51-9961-618be76d1277`，图执行为 `477307e0-af88-4140-a58f-866e4ac28930`，Attempt 为 `a0b68a43-20e7-4d02-abff-efa38b540e72`。图脚本校验值为 `sha256:53533f90b1c057d6face6576f411cd8190c7332b7c88f5f90713285fc38d0c0f`。

输入为 `{"label":"issue31 demo input","value":7}`，文件末尾有换行，共 41 字节，SHA-256 为 `31acf78b389cb9de1b1e0ca067caa8267322943f40f42638f30b789f9151362b`。初始状态为 `{"visited":0}`。只预授权准确的 `input.json` 读取和固定内容的 `side-effect.txt` 写入；节点内的 `approval-input.json` 读取经显式一次批准。

网页保存 v2–v8 前后，Runtime 最后游标为 102，原任务数为 1，追踪事件为 4，已结算能力调用为 3，业务写入为 1。副作用文件 SHA-256 为 `6d9f75270238497761129f5e0ccc6c071fb7deebd0a6684bfa63b817f471d0fb`，修改时间为 `1792857902939282836 ns`，两者均未变化。插件停用后的第二个任务完成后，累计游标为 180、任务为 2，业务写入仍为 1。插件、Runtime 和监督脚本退出码均为 0。

## 时延、检查与证据绑定

| 路径 | 本轮耗时 |
| --- | --- |
| 首次节点追踪到两图下一帧记录 | 95.9 ms |
| 待读取批准事件到等待显示 | 45.9 ms |
| 条件失败到错误摘要 | 83.9 ms |
| 保存字段修复到 v2 high | 60.3 ms |
| 保存阈值到 v3 low | 51.7 ms |
| 撤销到 v4 high | 57.8 ms |
| 保存未经过节点到 v5 缺口 | 49.8 ms |
| 撤销缺口改动到 v6 high | 52.8 ms |
| 草稿回归保存 A 到 v7 | 88.3 ms |
| 保留的 B 再次保存到 v8 | 57.1 ms |

计时使用同机 Runtime 时间戳及浏览器 `performance.timeOrigin + performance.now()`，取 DOM 变化后的下一次动画帧。它近似表示下一帧呈现，不证明 GPU 完成绘制，也不是固定毫秒级 CI 门槛。

最终插件测试为 54 项通过、0 失败、0 忽略。默认工作区完整测试为 1,773 项通过、0 失败、7 忽略，退出码为 0；这 54 项插件测试包含在工作区总数中，不重复相加。7 个忽略项是仓库已有的子进程专用入口和需显式构建示例的远程二进制测试，不写成全部场景通过。格式检查、全目标严格 Clippy 和固定程序构建均通过。`verify_results.py` 已读取本轮原件并生成 `acceptance.json`，确认八项标准及五项修复回归通过。

首轮严格 Clippy 曾因四项元组触发 `type_complexity` 而失败。本轮改用带字段名称的 `ObservedTrace` 结构，重新运行通过。初始反例日志、并行修改期间的编译失败和首轮 Clippy 日志保留在 `/out/issue31-fix-acceptance-r01`，不算最终通过结果。浏览器控制台为 0 错误、0 警告，页面错误收集为空。

大页独立进程证据在 `/out/issue31-fix-page-r01`。准确的 32 条读取先确认返回 `response_too_large_outcome_unknown`，同一最终插件随后自动缩页并读到 25，完整保留 24 条大事件。原件为 `run/result.json`、`run/trace.json`、构建及运行日志。没有修改 Core/IPC 的通用错误含义。

`source-build-input.json` 在最终测试和构建前保存 43 个交付源码及文档快照。`source-manifest.json` 将这些快照、固定程序、环境、网页原件、反例及进程日志关联起来，并在结尾核对源码没有变化。本文是单独新增的修复记录，旧审阅和验收原件不改写。`verify_results.py` 可重新核对已有原件。追踪、数据库和本机访问配置只保留在本机，不发布、不加入仓库。

结论只适用于本次 macOS ARM64、默认 feature 和已适配的有界无环 Luau 图。未适配脚本只展示实际调用与缺口；任意 Rust 等原生源码修改后运行新程序仍需编译。其他 macOS 架构、全部 feature 和更大复杂图的通用布局未在本轮验证。
