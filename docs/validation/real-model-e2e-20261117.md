# 2026-11-17 真实模型全链复测

本次已执行待审批、并发排队、取消后恢复、团队协作任务台原题，以及可用的可视化应用和 Runtime 接口。主要场景通过，但不能记为所有集成全部通过：进程超时后的未知写入仍缺少本地核对入口；会话主界面没有统一面板；普通代码的节点重算没有真实模型证据。

## 源码、平台与证据

- 源码起点：`d7c9ec265beb667048ade7387a30b87ba89ec11f`，开始及结束前获取的 `origin/dev` 均为该提交。
- 产品修复提交：`b20e3d1`。本报告所在提交另保存验收脚本的回复内容断言、客户端二进制摘要和本文。
- macOS arm64；Rust `1.95.0`；Node `v24.4.1`。手机验证是浏览器的 `390 × 844` CSS 视口，不代表 iOS 实机验证。
- 实际 Provider：Magpie，`http://127.0.0.1:3425/v1`，模型 `opencode-go/deepseek-v4.1-flash`；本机服务未配置密钥。
- 运行前已获得本次真实模型授权。模型累计用量、轮数和任务总时限未设上限；Host 单次调用时限沿用 `120000ms`。
- [原生场景及首轮原题证据](/Volumes/ORICO/cayw-full-e2e-20261117-154628-final/)，下文称 A。
- [应用继续实现、修复与最终证据](/Volumes/ORICO/cayw-product-e2e-20261117-163404/)，下文称 B。
- [较早失败的队列试验](/Volumes/ORICO/cayw-full-e2e-20261117-151439/)单独保留，未清除其中的未知结果。
- 两个目录的 `invocation.json` 保存当时源码差异、配置、平台和二进制摘要。B 的 `evidence/source-receipt.json`、`evidence/final-model-output-manifest.json` 保存复核时的源码及生成文件摘要。
- 本次中途修正过验收脚本。原始失败、人工释放旧测试进程、显式交接与后续修复均保留，不能称为一次全绿的无人干预运行。

## 原生运行链路

| 场景 | 实际验证 | 结果与证据 |
| --- | --- | --- |
| 待审批后允许 | 模型触发 `cells.load` 的精确待审批项，自行调用 `approval.pending`、`approval.decide`；未由脚本批准 | 通过；A `approval-result.json`、`evidence/approval.events.json`；决策者 `model-auto` |
| 待审批后拒绝 | 模型自行拒绝原读取，再等待原 cell；不重新读取 | 通过；A `deny-result.json`；原调用仅一次，终态 `not_executed` |
| 并发与排队 | 先持久化受理 10 个任务；独立 HTTP 请求恢复这些任务；透明转发真实模型响应，在观察屏障处暂留实际响应字节 | 通过；A `queue-blocked.json` 为 2 个占名额、8 个排队；`queue-result.json` 中 10 个真实上游响应均 200、10 个任务全部完成，回复逐一等于各自的 `CAYW_QUEUE_0` 至 `CAYW_QUEUE_9` |
| 取消、重启、继续 | 模型经 Host 写入 `checkpoint.txt` 后，中断下一次真实模型流；按完整任务身份取消；重启 Client，显式 `recover`；同一会话新一轮读取并写出恢复文件 | 通过；A `cancel-result.json`、`evidence/recovery-final.events.json`；旧任务保持取消，重启核对未重放模型请求，原文件写入恰好一次 |

排队和取消中的本机转发器不生成模型响应，也不重试上游。它只转发 Magpie 的实际响应。取消后客户端断开导致的转发器 BrokenPipe 日志保留，不改写为成功。

取消恢复这一项证明“已完成写入 + 等待模型响应时取消”的恢复。它不证明进程副作用结果未知时可以继续写入。后者见下文缺口。

## 原题与独立验收

题目原文已固定在 [scripts/prompts/team-taskdesk.txt](../../scripts/prompts/team-taskdesk.txt)。应用由 CAYW 的真实模型通过受控 CodeMode 写入，采用原生 HTML/CSS/JS、Node HTTP、SQLite 和两个执行者。最终有 21 个文件，含设计、源码、运行文档和测试；见 B `evidence/final-model-output/`。

首轮使用 A 的原始新会话。该轮在写出应用与 23 项通过测试后，误选宿主残留的旧测试入口，最终任务为 `outcome_unknown`。原记录没有改成成功。核对进程退出后，按逐文件摘要将 18 个已生成文件交给 B 的干净工作区，显式新建继续实现任务；没有重放原任务。B `evidence/source-handoff.json` 记录来源和摘要。

独立验收脚本为 B [evidence/taskdesk-check.py](/Volumes/ORICO/cayw-product-e2e-20261117-163404/evidence/taskdesk-check.py)。它使用实际 HTTP、独立只读 SQLite 连接和真实 Node 进程，不采用模型自报结论。最终 B `taskdesk-independent-result.json` 的 7 组检查全部通过：

1. 10 次提交均返回 202，并立即可从另一数据库连接读到；观察到 2 个执行、8 个排队；重复幂等键返回原任务。
2. 三名成员均可评论；另一项目成员的读取、修改、取消、核对、评论和事件访问均被拒绝；同版本并发修改得到一个 200、一个 409。
3. 10 个任务全部成功，事件重建的同时执行峰值为 2，每个任务仅执行一次；逐个核对计算的总和、平均值。
4. 第三名成员通过真实 SSE 收到另一名成员新提交的评论。
5. 实际执行者进入失败状态并保存原因；运行中取消进入 `canceled`，进度未写成 100%。
6. 已写出本地产物后对 Node 进程发送 SIGKILL；以原数据库重启，任务进入 `interrupted`，未自动重放；显式核对恢复后完成，本地产物仍只有原唯一身份。
7. 历史超过一页后，从游标 0 重新连接，收到全部 301 条项目事件，没有重复或遗漏。

这里的产物与通知是应用自己的 SQLite outbox，不代表验证了第三方外部系统的恰好一次投递。

独立脚本记录执行前后的后端文件摘要；最终页面修复后再次核对，后端字节未变。没有为前端改动重复整套已通过的后端检查。

## 浏览器与可视化组合

| 组件 | 实际接入与验证 | 边界 |
| --- | --- | --- |
| 计划批注 | 对实际计划的任务、版本和内容摘要提交 `deny_with_feedback`；模型从系统批注读取唯一标记 `UI-CHECKPOINT-20261117`，按其中的浏览器失败修复现有源码 | B `plannotator-result.json`；旧版本批注不用于替代新版本批准；反馈不是操作授权或用户验收 |
| 产物 | 同一会话实际保存 HTML 卡片和独立预览记录，并读取渲染投影逐字核对 | B `artifact-result.json`、`artifact-final-result.json`；未声称会话界面已嵌入产物 |
| 运行检查器 | 经公开 Runtime RPC 扫描完整事件日志，超过 1000 条仍继续读取 | B `run-inspector-result.json`；`has_more=false`，最终扫描到事件 29081 |
| Trace Debug | 独立应用通过认证 IPC 从游标 0 捕获同一产品会话；网页登录并查看实际调用 | A 捕获到游标 25797；B 捕获到游标 29081，与当时 Core 相同；B 有 61 个调用项目，追踪文件约 18.9 MB；原始 JSON 和截图保留 |
| 可视化编辑器 | 打开实际 `web/index.html`；预览用 `?api=` 连接真实协作服务，后端仅放行该显式本机 Origin；选择原元素、修改文字、应用、保存；再通过撤销和保存恢复原文件摘要 | B `editor-result.json`、`evidence/editor-save-undo.mp4`；不是直接改磁盘来冒充编辑器保存 |
| 面板登记 | 同一会话同时打开 files、editor、trace、artifacts、plannotator、tasks、usage 7 项，并读取打开列表 | B `panels-result.json`；只证明登记接口，不证明主会话里有完整面板界面 |

编辑器和 Trace 本次以已有的独立可选应用启动，不是伪造 External plugin 安装，也不证明 Client 已统一监管它们。普通 Luau 调用没有追踪声明，网页如实显示没有节点图；未将调用记录当成节点重算通过。

浏览器使用 T3 preview，在 macOS 上执行。保留操作录像、页面截图和数据核对：

- 首轮编辑器预览中登录、提交任务；另一浏览器身份看到该任务并发表评论。
- 最终页面在执行中显示真实进度；第二步失败后，自动显示已完成的 `scan_shards` 和 queued/started/step/failed 事件，未再显示假空步骤。
- 实时更新期间，尚未提交的评论草稿保持原文。
- 实际取消前经接口核对为 `running`，点击真实取消按钮后，接口和页面均为 `canceled`、进度 0，并显示取消事件。
- 桌面 `1280 × 900` 和手机视口 `390 × 844`；手机文档宽度为 390，无文档横向溢出。
- 停止协作服务后，页面显示“看板刷新失败”“详情刷新失败”和实时断开，未将错误显示为查询成功的空列表。

最终页面证据：B `ui-result.json`、`evidence/desktop-real-steps.png`、`evidence/mobile-final.png`、`evidence/mobile-service-failure.png`、`evidence/browser-final.mp4`。

首次浏览器取消操作因页面实时重绘，旧元素引用失效，没有发出取消请求，该任务最终成功。保留该次记录；后续新建显式任务，使用实际 DOM 中稳定的 `data-action=cancel-task` 再测通过。未把第一次操作计为取消成功。

## 发现、修复和保留的失败

| 发现 | 处理与证据 |
| --- | --- |
| 模型收到“仍在运行”，却不知道原 cell 正等待审批 | Runtime 在 ModelAuto 工具结果附上当前任务的实际待审批项及处理步骤；不自动授予权限。回归先失败后通过，4 项审批测试通过，真实允许/拒绝通过 |
| 网页入口中间层在 2 秒后拒绝已受理的排队任务 | prompt/resume 等待入口名额时不再套中间层排队时限；关闭信号、认证、入口队列容量和控制通道保留。31 项 gateway 测试通过，真实 10 任务完成 |
| 用同一工作区的阻塞进程来测试并发，实际被工作区写入锁串行处理 | 判为验收设计错误；旧环境的 10 个未知任务保留。改为透明转发真实模型响应的观察屏障，不用文件修改来占执行者 |
| 队列、取消的旧进程测试 profile 残留，原题模型误选后超时 | 两次人工释放均记录；其中取消进程已先超时，原题保持未知。脚本删除这些入口，干净环境只有 node-tests；逐文件交接另开继续任务 |
| 应用 SSE 补发静默停在 200 条 | 原始 301 条只收到 200 条的失败保留；模型修复游标分页和前端续页，新增回归，独立复测 301 条完整 |
| 应用只更新任务状态，详情步骤和时间线保持旧快照 | 通过版本绑定的真实批注交给模型；修复整份重取、旧响应覆盖、失败提示与草稿保留；最终 30 项 Node 测试和实际浏览器复测通过 |
| Rust 全工作区首次运行出现 macOS `WouldBlock` | 测试监听使用非阻塞模式，接受的连接仍可能继承该模式；测试服务显式将连接设为阻塞后使用原有读取超时。仅修改测试辅助服务；完整工作区重新运行通过 |

模型在修复中也有失败测试，原始 `test.run` 输出全部保留。最终通过不抹去这些记录。B `model-request-summary.json` 记录两个最终环境的 140 次前台实际模型请求：139 次完成、1 次取消，均有终态；不含旧失败环境和后台上下文维护调用。取消的未知用量、未提供的价格均未写成零费用。

## 实际执行的仓库检查

```sh
cargo +1.95.0 fmt --all -- --check
cargo +1.95.0 test -p cayw-runtime --test model_approvals --locked
cargo +1.95.0 test -p cayw-webui --test gateway --locked
cargo +1.95.0 test --workspace --locked
cargo +1.95.0 clippy --workspace --all-targets --locked -- -D warnings
python3 -m py_compile scripts/validate-live-suite.py
```

上述最终结果均通过。首次 workspace 失败和修复后完整日志分别为 B `evidence/workspace-tests-initial.log`、`evidence/workspace-tests-final.log`；Clippy 为 `evidence/clippy-workspace.log`。真实模型调用不属于普通 `cargo test`。

## 复跑入口

需重新获得对应真实调用授权，并提供可用的本机 Magpie。先构建当前四个应用和示例客户端，再按顺序执行；`RUN` 必须是新目录。

```sh
cargo +1.95.0 build -p cayw-runtime -p cayw-webui -p cayw-visual-editor -p cayw-trace-debug --bins --features cayw-runtime/plugins --locked
cargo +1.95.0 build --manifest-path examples/headless-client/Cargo.toml --locked
RUN=/Volumes/ORICO/cayw-new-live-suite
python3 scripts/validate-live-suite.py setup --out "$RUN"
python3 scripts/validate-live-suite.py approval --out "$RUN"
python3 scripts/validate-live-suite.py deny_approval --out "$RUN"
python3 scripts/validate-live-suite.py queue --out "$RUN"
python3 scripts/validate-live-suite.py cancel --out "$RUN"
python3 scripts/validate-live-suite.py prepare_product --out "$RUN"
python3 scripts/validate-live-suite.py start_trace --out "$RUN"
python3 scripts/validate-live-suite.py product --out "$RUN"
python3 scripts/validate-live-suite.py start_editor --out "$RUN"
python3 scripts/validate-live-suite.py stop --out "$RUN"
```

这些命令不会自动完成独立应用验收、浏览器操作或关闭独立编辑器/追踪进程。接口验收脚本针对本次生成应用的真实合同，见 B；新的模型实现若合同变化，须先审阅实际接口再调整，不能为使结果通过而放宽断言。关闭独立应用前，按 `private/*-process.json` 的 PID 和进程启动身份核对归属。

## 仍未通过或未覆盖

1. **本地未知进程副作用的核对入口缺失。** 旧进程退出与设备清理不等于操作结果已知。`recover` 可以保留未知结果并继续普通对话，但未知写入的工作区锁仍需可信的原结果核对；本机产品本次没有完成这类核对并恢复写入。不能删除该锁或直接将未知改为失败/成功。本次成功的取消恢复不替代这项验收。
2. **统一会话工作台未接齐。** 面板登记、产物接口、独立编辑器和追踪网页分别可用；会话主界面尚无整套可交互的统一面板。不能称为所有可视化插件的界面组合通过。
3. **真实模型普通代码的节点重算未覆盖。** 本次捕获到真实调用，但这些普通 cell 没有追踪声明。工作区离线测试中的图重算不能代替该真实模型场景。

应用模型另明确说明：核对说明输入框的草稿不在本次标题/评论草稿保留范围。该输入在其他成员触发详情重绘时仍可能被清除；未测试或宣称它通过。

## 新增检查、限制、兜底、重试

产品新增限制、兜底和重试：**无**。审批提示只增加事实和操作说明；只展示当前任务的审批，保留原权限边界。入口排队时限被移除，不收紧原来允许的输入。工作区锁、认证、项目隔离、外部调用时限和未知结果核对要求保留。

验收断言中的 2 个执行者和 10 次提交来自题目要求；301 条事件用于越过已复现的 200 条分页缺口。转发器的 120 秒外部读取时限沿用此次 Host 单次调用时限；它不对 CAYW 增加任务总时限。测试断言不会拦截正常产品操作。

所有测试服务和标签页的清理结果保存在两个目录的 `cleanup-result.json`。保留日志、数据库、未知结果、生成源码、截图和录像，供后续核对。
