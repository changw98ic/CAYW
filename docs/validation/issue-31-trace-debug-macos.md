# Issue #31：Trace Debug 的 macOS 本地验收

记录整理日期：2026-10-24。任务范围以 [Issue #31](https://github.com/changw98ic/CAYW/issues/31) 为准。本次交付独立插件应用、实际运行追踪、网页编辑后自动重新计算，以及可复跑的离线演示。代码保留在独立工作树，未提交、推送、合并或修改远端 Issue。

后续独立的 [Magpie 真实模型补验](issue-31-magpie-macos.md) 已完成，包含实际请求、通过场景、失败原件和预算保留。本文以下内容仍是原离线轮次的记录，不改写其 Provider 或用量。

## 源码、构建与平台

- 工作树：`/Volumes/ORICO/CAYW/.claude/worktrees/issue31-trace-debug`。
- 分支：`plugin/trace-debug`。
- 创建工作树时的 `origin/dev` 基线：`b14b401f37af877e03bbf11594a05dadb1289d47`。
- 当前改动是基线之上的本地未提交文件，不能只用基线提交号标识本次构建。逐文件校验值、命令和原始日志清单保存在 `/out/issue31-delivery/source-manifest.json`。
- 实际宿主：macOS 27.0.1，Build `26A434`，ARM64。
- Rust：`rustc 1.95.0 (59807616e 2026-04-14)`；Node：`v24.4.1`，仅用于开发检查。
- 浏览器：Playwright MCP 驱动的 Chrome `155.0.0.0`。浏览器用户代理中的 `MacIntel` / `Mac OS X 10_15_7` 是用户代理值，不是宿主架构证明。
- 原始事件时间戳与 Playwright 文件名保留采集值，部分 UTC 日期为 2026-10-23，没有为整理日期重写。

本次使用固定离线 Provider 响应。Runtime、Host 文件读写、Cells、Luau 和认证 IPC 均为真实实现。没有调用外部模型或付费服务。所有追踪原件只保存在本机；追踪含节点源码、输入、上下文和结果，不作为网页发布或提交内容。

最终演示构建 `r05` 的 SHA-256：

| 文件 | SHA-256 |
| --- | --- |
| `target/debug/examples/demo` | `e2cd4c39578b670893ec9d6b2d67089a5ffb2d960af52609d2d72723ae5b0a29` |
| `target/debug/cayw-trace-debug` | `06197516bef7ac3195b325c1019e56bb014fdfe2ee5c0a70756154e70e831897` |

`Cargo.lock` 仅增加新包的依赖清单，没有升级第三方依赖。直接开源依赖的 Cargo 许可证声明为 MIT、Apache-2.0 或二者可选，与本仓库 Apache-2.0 兼容。插件复用已有 `cayw-codemode`，没有另接第二套 Luau 执行器。Node、Python 和浏览器检查均为开发工具，不向产品 Agent 开放通用执行入口。

## 复跑命令与输入

以下命令在该工作树根目录执行。若 PATH 没有 Cargo，将 `cargo` 替换为已安装的 `/Users/chengwen/.cargo/bin/cargo`。

```sh
cargo +1.95.0 fmt --all -- --check
cargo +1.95.0 check --workspace --locked --offline
cargo +1.95.0 test --workspace --locked --offline
cargo +1.95.0 clippy --workspace --all-targets --locked --offline -- -D warnings
cargo +1.95.0 build -p cayw-trace-debug \
  --bin cayw-trace-debug --example demo --locked --offline
node --check crates/cayw-trace-debug/assets/app.js
```

启动两个独立进程。`--output` 必须是新目录：

```sh
python3 -I scripts/validate-trace-debug.py serve \
  --demo target/debug/examples/demo \
  --plugin target/debug/cayw-trace-debug \
  --output /absolute/new/trace-demo
```

脚本保存 `ready.json`、进程 ID、命令、二进制校验值和标准输出/错误。脚本启动时不创建业务任务。打开 `ready.json` 中的 `web_url`，输入公开的合成网页令牌：

```text
issue31-public-web-fixture-1111111111111111
```

脚本使用的 IPC 令牌也是公开合成数据，且与网页令牌不同。真实 Runtime 的令牌应通过可信环境提供，不能复用该演示令牌。

另开终端，选择普通故障演示或审批等待演示：

```sh
python3 -I scripts/validate-trace-debug.py task \
  --output /absolute/new/trace-demo

# 如需核对实际等待，用此命令代替上面一条。
python3 -I scripts/validate-trace-debug.py task \
  --output /absolute/new/trace-demo --wait-read
```

输入文件固定为 41 字节：

```json
{"label":"issue31 demo input","value":7}
```

实际文件尾有换行，SHA-256 为 `31acf78b389cb9de1b1e0ca067caa8267322943f40f42638f30b789f9151362b`。初始状态为 `{"visited":0}`。计划的输入与上下文来自该次真实 `fs.read` 的结构化结果。

演示只预授权 `fs.read({"path":"input.json"})`，以及向 `side-effect.txt` 写入准确固定内容的一次业务路径。第一次真实任务写入，后续任务读取现有文件而不重复写入。等待演示还会在 `input` 节点请求 `fs.read({"path":"approval-input.json"})`，该读取没有预授权。

等待演示的任务命令会保持等待。网页模块图、节点图、时间线和详情应显示“等待响应”。在另一个终端明确批准这一次固定文件读取：

```sh
python3 -I scripts/validate-trace-debug.py approve-read \
  --output /absolute/new/trace-demo
```

该命令核对能力名、准确路径和调用类型，只接受一个匹配的待审批请求，并使用 `allow_once`。演示的 IPC 请求与单次 Host 等待上限为 180 秒，须在此期间批准。插件自身只有事件读取接口，不负责批准或重提任务。

## 实际运行、编辑和撤销

最终网页验收目录：`/out/issue31-browser-r05`。`/out` 在本机解析到 `/Volumes/ORICO`。执行身份如下，原件可据此相互核对：

- Session：`f99f9249-ae71-44c4-b673-63a49ad341bc`。
- 原任务：`db96a03c-5703-464e-aa27-dc478c7b6a44`。
- 节点图执行：`d88a7385-d924-4e1f-8e24-05b33e471829`。
- Attempt：`ff41f767-1ef8-4ce1-b90c-4de529f47c5d`。
- Worker：`902eec7b-1c01-4aaf-b137-d94d6871f524`，fence 为 1。
- 实际图脚本：`sha256:53533f90b1c057d6face6576f411cd8190c7332b7c88f5f90713285fc38d0c0f`。

| 操作 | 实际结果 |
| --- | --- |
| 显式提交等待演示任务 | 页面自动选择新节点图，不手动刷新。`input` 的真实读取尚未结算，模块、节点、时间线、详情均显示“等待响应” |
| 明确批准固定文件读取 | 原任务继续。`input` 完成，`condition` 因读取不存在的 `input.amount` 而失败，图和错误摘要自动更新 |
| v1：原始记录 | 路径 `input → condition`，整轮 `failed`，最终输出为 `null`，没有把中间输出当成成功结果 |
| v2：将 `input.amount` 改为 `input.value` 并保存 | 自动求值，路径 `input → condition → high → result`，输出 `classification=high`、`threshold=5`、`value=7`、`visited_nodes=4` |
| v3：将参数改为 `{"threshold":20}` 并保存 | 自动切到 `low`，输出 `classification=low`、`rule=value <= threshold`、`threshold=20` |
| v4：撤销到 v2 | 创建新版本并自动从入口重新计算；四个步骤 `reused=false`，恢复 `high` 和阈值 5 |
| 查看 v1 和本地结果 | v1 仍是实际失败；v2–v4 明确标记本地重放；终态与本地结果不再显示等待 |

等待只是一项网页显示状态，由当前原始运行中尚未结算的节点调用推导。Rust 状态及持久格式没有新增一个虚假的执行终态。它不细分“等审批”与“等其他 Host 响应”。原始输入节点耗时约 45.7 秒，包含人工检查和批准时间；复用步骤保留原耗时，不表示保存时又花了这段时间。

错误摘要显示错误节点、原始错误的首行、关键输入、进入前状态、耗时、证据游标和建议核对位置。原始错误、完整步骤 JSON 及 `Document.events` 可展开。未用模型猜测根因。

网页中的两种图区分实际经过边和计划边。模块可以下钻；时间线可以定位节点，并标明“已捕获”“本地计算”“复用”。最终截图验证横向模块连线从右侧到左侧连接，文字位于模块间隙内。390×844 深色页面的文档和 body 宽度均为 390；宽图在自己的容器内滚动，页面没有横向溢出。

## 反例与数据边界

`r03` 浏览器验收保留在 `/out/issue31-browser-r03`。该轮早于等待显示与模块连线修正，用于下列编辑、冲突和证据操作；相应核心逻辑由最终测试再次覆盖。不能将 `r03` 当作等待响应的证据。

| 反例或操作 | 观察结果 |
| --- | --- |
| 从旧 v2 草稿提交阈值 99，而最新版本已是 v4 | HTTP 409，最新版本不变，草稿保留，不自动覆盖或重试 |
| 当前最近路径未经过 `low`，编辑 `low` | v5 为 `blocked`，步骤为空，列出 `the captured evaluation never reached the edited node`；没有借用更早 v3 的分支输入 |
| 修改条件节点以调用新的 `fs.write` | v7 为 `blocked`，显示 `UnknownCapability: fs.write`，最终输出为 `null`；没有真实文件写入 |
| 保存请求在途时继续输入 | 真实响应交付前输入新阈值，第一次完成得到 v9 低分支，新输入保留为基于 v9 的草稿；再保存得到 v10 高分支，两轮均保留 |
| 时间线键盘、模块下钻与原始证据 | 方向键后可用 Enter 选中条件节点；分支模块优先定位失败节点；游标 54 对应原始错误事件可以展开 |
| 对比 v3 与 v10 | 同时显示低/高路径、阈值 20/5 和不同最终输出 |

最终 Rust 测试还覆盖：

- Cells 输出先持久化，再调用观察接口。
- 原始执行的 Session、Attempt、Worker、fence、任务、父调用和源码校验不符时拒绝投影。
- 实际脚本须与 `script_for_plan` 的源码摘要一致；节点不能用保留追踪结构伪造节点路径，不能逃出函数体修改生成器局部变量。
- 路径顺序、分支、输入和进入前状态不符时隔离该图，原事件继续保存，不卡住 Session 游标。
- 缺输入、缺捕获响应、参数不匹配、取消、原运行未结束或未知业务效果时，明确停止并列出缺口。
- 本地计算仅使用名称、参数和顺序准确匹配的捕获响应，不持有真实 Host 写接口。
- 本地与实际脚本均拒绝非法数组指针、超过 32 层的返回数据、未知返回字段、无效图及超预算计算。
- 保存/撤销、重开、独占文件锁、`0600`、符号链接拒绝、游标连续性、重复事件一致性、认证、同源和长轮询退出。

`r04` 曾因演示默认 IPC 30 秒请求超时，在批准前取消。原始失败日志、事件、浏览器等待样本和退出码仍保留在 `/out/issue31-browser-r04`。随后只延长演示的有界观察时间，使用新目录 `r05` 重新验证；没有恢复或盲目重放超时请求。最终 `r05` 的批准、完成和清理均有独立收据。

## 业务写入与停用插件

`r05/waiting-task.log` 和 `r05/after-edits.log` 的以下字段完全相同：

| 字段 | 修改前后结果 |
| --- | --- |
| Core 最后游标 | 118 |
| Runtime 任务数量 | 1 |
| 已结算能力调用 | 3 |
| `fs.write` 次数 | 1 |
| 副作用文件 SHA-256 | `6d9f75270238497761129f5e0ccc6c071fb7deebd0a6684bfa63b817f471d0fb` |
| 副作用文件 `mtime_ns` | `1792774056999561943` |

v2–v4 的编辑、阈值变化和从入口撤销没有追加 Core 事件、创建任务或修改该文件。`r03` 的 v2–v10、冲突和新增写调用反例也有相同方式的前后收据：游标 79、任务 1、写入 1，文件内容和修改时间均不变。

最终构建还执行了：

```sh
python3 -I scripts/validate-trace-debug.py stop-plugin \
  --output /absolute/new/trace-demo
python3 -I scripts/validate-trace-debug.py task \
  --output /absolute/new/trace-demo
python3 -I scripts/validate-trace-debug.py shutdown \
  --output /absolute/new/trace-demo
```

停插件后，普通认证 IPC 任务 `688b73e4-2756-449d-975f-a79bf0f1a86d` 完成。累计游标 191，任务 2，写入仍为 1，副作用文件 SHA-256 和修改时间不变。固定离线 Provider 会在记录原始条件错误后结束任务；任务完成不表示错误节点已成功。

浏览器在停止服务前关闭。`shutdown.json` 中插件、演示 Runtime 的退出码依次均为 0，监督脚本退出码为 0。最终 `r05` 服务停止前浏览器控制台为 0 错误、0 警告。`r03` 控制台中的一条 HTTP 409 是旧草稿反例的预期拒绝，不写成零错误。

## 呈现耗时

计时依据保存的 Runtime 事件时间戳和浏览器 `performance.timeOrigin + performance.now()`。浏览器用 MutationObserver 记录 DOM 变化，再记录下一次 `requestAnimationFrame`。这是同机的 DOM/下一帧呈现近似，不证明 GPU 已完成显示，也不是服务端投影耗时。

最终 `r05` 样本：

| 路径 | 起点 | 下一帧记录 | 耗时 |
| --- | --- | --- | --- |
| 首个追踪事件到图 | 游标 44，`1792774057174` | `1792774057296.9` | 122.9 ms |
| 待结算调用到等待显示 | 游标 49，`1792774057222` | `1792774057296.9` | 74.9 ms |
| 错误到摘要 | 游标 103，`1792774102949` | `1792774103183.5` | 234.5 ms |
| 保存修复到 v2 高分支 | POST 开始 `1792774121270.6` | `1792774121345.8` | 75.2 ms |
| 保存阈值到 v3 低分支 | POST 开始 `1792774162379.2` | `1792774162437.7` | 58.5 ms |
| 撤销到 v4 新结果 | POST 开始 `1792774162454.4` | `1792774162513.5` | 59.1 ms |

早期 `r03` 样本：追踪到图 95.5 ms，错误到摘要 245.7 ms，普通保存/撤销约 48.8–67.5 ms。v9 的 81.3 ms 来自人为延后真实响应交付的在途输入反例，单列保留，不混作普通性能基准。

每轮仅为本机固定输入样本。没有据此承诺所有项目的性能，没有添加固定毫秒阈值的 CI 断言。

## 检查结果与原件

最终插件测试为 47 项：追踪投影 19、真实 Runtime 演示 3、节点求值 17、HTTP 5、存储 3；全部通过。新增真实审批等待测试验证批准前调用没有开始或结算，批准后记录真实结果，之后的入口重新计算只消费捕获响应且不增加 Runtime 事件。

默认完整 workspace 检查、测试和全部目标严格 Clippy 已运行；最终命令和逐项结果由交付清单绑定原始日志。完整默认 workspace 测试为 1,751 通过、0 失败、7 默认忽略。格式、JavaScript 语法与 Python 语法检查通过。忽略入口没有在本次运行中当作通过。

本机主要原件：

| 路径 | 内容 |
| --- | --- |
| `/out/issue31-delivery/source-manifest.json` | 基线、工作树、最终逐文件校验值、平台、二进制和验证日志清单 |
| `/out/issue31-browser-r05/ready.json` | 最终启动命令、进程和构建校验值 |
| `/out/issue31-browser-r05/browser-audit.json` | 实际 API 文档快照、保存请求、DOM/下一帧状态、错误收集 |
| `/out/issue31-browser-r05/browser-results.json` | 版本、路径、输出、等待、窄屏与耗时计算结果 |
| `/out/issue31-browser-r05/trace.json` | 插件追踪原件，含原始事件、内容版本和本地结果 |
| `/out/issue31-browser-r05/waiting-task.log`、`approval.log`、`after-edits.log` | 首任务、一次批准和编辑后只读核对收据 |
| `/out/issue31-browser-r05/task-without-plugin.log`、`shutdown.json` | 停插件后任务和进程退出结果 |
| `/out/issue31-browser-r05/waiting-modules.png`、`waiting-nodes.png` | 实际等待时的两种图 |
| `/out/issue31-browser-r05/modules-final.png`、`nodes-final.png` | 最终模块连线和高分支路径 |
| `/out/issue31-browser-r05/modules-narrow-dark.png`、`editor-narrow-dark.png` | 390×844 深色图和编辑器 |
| `/out/issue31-browser-r03/browser-audit.json`、`browser-results.json` | 早期十版本、旧草稿、在途输入和缺上下文反例 |
| `/out/issue31-playwright-audit-r04.js`、`issue31-browser-metrics.py` | 浏览器采集及离线耗时核对脚本，后者以 `python3 -I ... /out/issue31-browser-r05` 运行 |
| `/out/issue31-build-r05.log`、`issue31-plugin-tests-r06.log` | 最终演示构建和 47 项插件测试 |
| `/out/issue31-workspace-check-r05.log`、`issue31-workspace-test-r05.log`、`issue31-workspace-clippy-r05.log` | 最终完整默认工作区检查 |
| `/out/issue31-format-r05.log`、`issue31-js-r05.log`、`issue31-python-r05.log` | 最终格式和脚本语法检查 |

原件在本机保留，不将 SQLite、锁、捕获数据或实际凭据加入仓库。

## 验收边界

本次完成的是 macOS ARM64 上的首版离线验收。未验证其他 macOS 架构、真实模型或所有 Cargo feature 组合，不将这些范围写成通过。Windows 与 Linux 不属于本次首发范围。

首版支持最多 64 节点的有界无环 `GraphPlan`、Luau 节点和明确捕获的调用响应。普通未适配脚本只展示已有调用事实并列出上下文缺口，不声称已经获得完整节点图。插件不恢复任意源码行的虚拟机栈。原生 Rust 源码修改后的新二进制仍需编译，节点快速反馈不替代它。

模块图为稳定分组布局。已验证演示的三模块相邻连接。大量模块、跨行或同排跨越中间模块的连线没有进行通用避障验收，不据演示截图声称已覆盖所有布局。网页等待状态表示当前原始节点存在尚未结算调用，不是权限裁决或业务结果。
