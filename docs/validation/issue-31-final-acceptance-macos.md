# Issue #31：当前源码的八项验收复核

记录日期：2026-10-24。范围以 [Issue #31](https://github.com/changw98ic/CAYW/issues/31) 的八条验收标准为准。在 Haiku 参数反馈修复和后续精简之后，本轮重新测试插件、构建程序，并通过 Playwright MCP 完成网页操作。八条标准在 macOS 27.0.1 ARM64、Rust 1.95.0、默认 feature 和下述五节点演示范围内均通过。

工作树为 `/Volumes/ORICO/CAYW/.claude/worktrees/issue31-trace-debug`，分支为 `plugin/trace-debug`，基线为 `b14b401f37af877e03bbf11594a05dadb1289d47`。本轮验收未改变生产代码。代码仍未提交、推送或合并；远端 Issue 仍为 OPEN，正文与复选框未同步。

本轮使用固定离线 Provider。Runtime、Host 文件读写、Cells、Luau 和认证 IPC 均实际执行。本轮真实模型生成请求为 0。此前六次真实请求及其成功、失败、预算保留见 [Magpie 记录](issue-31-magpie-macos.md)；修复后的 Haiku 线上自行纠错仍未重测。

## 当前构建与复跑入口

验证目录为 `/out/issue31-final-acceptance-r01`；独立的运行数据目录为 `/out/issue31-final-browser-r01`。本机 `/out` 解析到 `/Volumes/ORICO`。构建成功后已复制固定程序，避免共享 `target` 中的文件变化影响证据。

| 固定程序 | SHA-256 |
| --- | --- |
| `bin/demo` | `2451aefaca9a3cf319c50982f7180317373b788e0cd9426862894d8d7f8efdbc` |
| `bin/cayw-trace-debug` | `b704e87ffd33ced62b31ebc39a50935eb8bea5006da7a9038a4e9dc67c601e21` |

在工作树中执行：

```sh
/Users/chengwen/.cargo/bin/cargo +1.95.0 test \
  -p cayw-trace-debug --locked --offline
/Users/chengwen/.cargo/bin/cargo +1.95.0 build \
  -p cayw-trace-debug --bin cayw-trace-debug --example demo --locked --offline
python3 -I scripts/validate-trace-debug.py serve \
  --demo /out/issue31-final-acceptance-r01/bin/demo \
  --plugin /out/issue31-final-acceptance-r01/bin/cayw-trace-debug \
  --output /absolute/new/issue31-acceptance
```

输出目录须不存在。先打开新目录 `ready.json` 中的网页地址，再输入公开演示令牌 `issue31-public-web-fixture-1111111111111111`。需要重新采集网页计时时，在导航前通过 Playwright MCP 执行 `/out/issue31-playwright-audit-r04.js`。本次审计脚本的快照保存在验证目录的 `playwright-audit.js`。

页面连接后，另开终端发起任务：

```sh
python3 -I scripts/validate-trace-debug.py task \
  --output /absolute/new/issue31-acceptance --wait-read
```

实际输入为 `{"label":"issue31 demo input","value":7}`，文件尾有换行，共 41 字节，SHA-256 为 `31acf78b389cb9de1b1e0ca067caa8267322943f40f42638f30b789f9151362b`。初始状态为 `{"visited":0}`。只预授权准确的 `input.json` 读取及固定内容写入 `side-effect.txt`；`approval-input.json` 需显式单次批准。

看到输入节点等待后，在 180 秒内批准这一次读取：

```sh
python3 -I scripts/validate-trace-debug.py approve-read \
  --output /absolute/new/issue31-acceptance
```

网页随后按下表操作。只点“保存并重算”或撤销按钮，不另发运行任务。结束后先关闭网页，再停插件。仓外 `headless_after_plugin.py` 记录了本轮随后通过认证 IPC 提交普通任务的方法；若用于新目录，将其中的运行目录常量改为新目录。最后用 `validate-trace-debug.py shutdown --output NEW_DIRECTORY` 清理该次进程。

以上 Cargo 命令用于开发者构建本次验收程序。网页保存和撤销路径本身不运行这些命令。

## 八项标准与实际结果

| Issue 验收标准 | 本轮结果 | 证据 |
| --- | --- | --- |
| 1. 实际执行时，两图增量更新并关联同一运行，无需刷新 | 任务前页面为空且已连接。输入节点等待读取审批时，模块图、节点图和时间线同步显示等待；批准后更新为条件失败。浏览器始终只有一次导航 | `browser-audit.json`；`browser-validation.json` 的 `waiting`、`failed` |
| 2. 错误显示位置、关键输入、摘要和原始证据入口 | 条件节点显示不存在的输入导致比较失败；展示 `value=7`、进入前状态、错误首行和游标 93。实际展开原始事件 JSON | `browser-validation.json` 的 `failed` |
| 3. 修改保存后，自动用对应输入和状态计算下游 | 将 `input.amount` 改成 `input.value`，一次保存产生 v2；输入前缀复用，条件及其后继实际重新求值 | `high` 的步骤、原始保存请求与返回文档 |
| 4. 新内容真实改变分支或输出，图与记录一致 | v2 走 high，v3 阈值 20 走 low；路径、步骤和最终 `classification` 同步改变。撤销到 v2 创建 v4，从入口重新计算并恢复 high | `high`、`low`、`undo`；`trace.json` 的版本记录 |
| 5. 自动流转不调用整项目检查、构建或全量测试，无需另点运行 | 保存与撤销直接调用插件的 Luau 求值器，只注入捕获响应。只读复核未发现编译或测试调用，也无真实 Host 执行句柄。运行收据确认没有新增任务、事件或能力调用 | `save-path-review.json`；修改前后 IPC 收据；五次网页 POST 均成功 |
| 6. 能对比路径、输出、错误，区分原始运行与重放 | 原始 v1 失败保留为 observed；v2–v6 为 replay。页面实际比较 v1/v2 的错误与路径，以及 v2/v3 的高低分支和输出；撤销创建新版本 | `browser-validation.json` 的 `high.comparison`、`low.comparison`、`undo` |
| 7. 缺上下文显示具体缺口，区分事实和推断 | 当前 high 路径未经过 low，编辑 low 产生 v5 blocked，步骤为空、最终输出为空，指出 `the captured evaluation never reached the edited node`。错误卡只报告事实并注明不推断根因；本轮没有模型推断 | `missingContext`；缺口区与节点详情；v6 撤销后恢复 high |
| 8. 停插件后普通 IPC 和无界面任务仍能执行 | 确认插件进程已退出后，提交第二个普通任务；任务完成，新建的读取执行和图执行都 completed，且没有新增业务写入 | `headless-assertions.json`、`headless-result.json`、`headless-after-events.json` |

本轮身份：Session `d5087efb-1d9a-4277-8d7b-df26f0e0a34e`；首任务 `66f57aba-0e5f-4ed3-b519-0769ef83c974`；图执行 `4040ed36-d8da-4142-a8a4-bc05fbbf6c9a`；Attempt `d1f4b793-f602-42e2-b3d8-62d470295560`。图脚本 SHA-256 为 `53533f90b1c057d6face6576f411cd8190c7332b7c88f5f90713285fc38d0c0f`。首任务由离线 Provider 在记录图失败后正常结束；这不把条件失败改成成功。

## 写入、时延和检查结果

从 v1 到 v6 的三次保存、两次撤销前后，以下实际 Runtime 数据保持一致：最后游标 118，任务 1，trace 事件 4，已结算能力调用 3，业务写入 1。副作用文件 SHA-256 为 `6d9f75270238497761129f5e0ccc6c071fb7deebd0a6684bfa63b817f471d0fb`，`mtime_ns` 为 `1792821494642921235`，均未变化。

停插件后的第二个任务为 `4f6bac69-6f19-4838-a33f-69380d1648d8`，用时约 1.81 秒。新读取执行和新图执行都成功；累计游标 196、任务 2、能力调用 4，业务写入仍为 1，文件内容和修改时间不变。插件、演示 Runtime 和监督脚本均以 0 退出。

| 路径 | 起点事件或请求 | 耗时 |
| --- | --- | --- |
| 首个节点追踪到两图下一帧记录 | 游标 44 | 215.1 ms |
| 待审批读取到等待显示 | 游标 49 | 160.1 ms |
| 条件失败到错误摘要 | 游标 93 | 201.6 ms |
| 保存修正字段到 v2 high | edit 请求开始 | 57.7 ms |
| 保存阈值 20 到 v3 low | edit 请求开始 | 59.0 ms |
| 撤销到 v4 high | undo 请求开始 | 62.9 ms |
| 保存未经过节点到 v5 缺口 | edit 请求开始 | 117.6 ms |
| 撤销缺口改动到 v6 high | undo 请求开始 | 65.4 ms |

计时来自同机 Runtime 时间戳、浏览器 `performance.timeOrigin + performance.now()`、DOM 变化和下一次动画帧。它近似表示下一帧呈现，不证明 GPU 完成绘制，不是通用性能承诺，也未作为固定毫秒阈值的 CI 断言。输入节点约 23 秒的原始耗时包含人工查看和批准时间；保存时复用该步骤不会再次等待这段时间。

当前源码的插件测试重新运行，47 项通过、0 失败、0 忽略。此前同一生产源码的 AI、Providers、Agent、Runtime 四包回归为 556 项通过、0 失败、0 忽略；全工作区格式和所有目标严格 Clippy 均通过，见 [参数反馈修复记录](issue-31-haiku-argument-recovery-macos.md)。这些是分次执行的检查，未写成一次完整工作区测试。浏览器控制台为 0 错误、0 警告，页面错误收集为空。

## 证据绑定与范围

验证目录中的 `acceptance.json` 保存逐项结论和实际时延；`verify_acceptance.py` 可重新核对已有原件；`build-results.json` 保存测试、构建命令及固定程序校验值；`browser-audit.json` 保存 15 个文档快照、20 次页面状态记录和五次保存/撤销请求；`browser-validation.json` 保存各轮输入、路径、输出、对比和原始证据查看结果；`current-ui.png`、`current-ui.yml` 保存最终页面。运行目录保留原始事件、单次批准、修改前后检查、停插件后的任务及退出码。

`source-manifest.json` 将本次源码快照、固定构建、平台、输入、验收依据和本机证据校验值对应起来。它另行引用精简回归清单；旧离线和真实模型记录不覆盖、不重写。追踪、数据库及访问配置保留本机，不发布，也不加入仓库。

结论只适用于上述 macOS ARM64、默认 feature 和最多 64 节点的有界无环 Luau 图执行范围。未适配的任意项目脚本不能凭空得到完整节点图；缺少适配或捕获上下文会显示缺口。任意 Rust 原生源码改动后运行新二进制仍需编译。其他 macOS 架构、全部 feature 和大量模块的通用连线避障未在本轮验证。旧记录中的窄屏、草稿冲突等额外场景见 [原离线验收](issue-31-trace-debug-macos.md)，不冒充本轮重新执行。
