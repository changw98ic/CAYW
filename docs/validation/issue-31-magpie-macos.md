# Issue #31：Magpie 真实模型补验

记录日期：2026-10-24。用户在离线首版交付后明确授权：“允许使用 magpie 做真实模型测试 预算 100m”。本记录只证明该授权下的 macOS ARM64 合成场景。此前的[离线验收](issue-31-trace-debug-macos.md)保持独立；其固定 Provider 不能作为本次真实请求证据。

后续的 [Haiku 参数错误反馈修复](issue-31-haiku-argument-recovery-macos.md) 单独记录代码修复、本地回归和原响应离线复验，不改写本文六次真实请求与旧预算。

本次实际发送 6 次生成 POST。最终使用 `opencode-go/deepseek-v4.1-flash` 的 Messages 接口完成了实际图执行、错误呈现、网页修复、分支变化、撤销，以及停用插件后的普通真实模型任务。前两条尝试路径有协议失败，原响应、失败任务和预算保留均未删除或改成成功。

## 源码、平台与调用路径

- 工作树：`/Volumes/ORICO/CAYW/.claude/worktrees/issue31-trace-debug`，分支 `plugin/trace-debug`。
- 基线：`b14b401f37af877e03bbf11594a05dadb1289d47`；产品改动仍未提交或推送，未修改远端 Issue。
- 宿主：macOS 27.0.1，Build `26A434`，ARM64；Rust 1.95.0。
- 浏览器：Playwright MCP 驱动的 Chrome 155。浏览器用户代理不能证明宿主架构。
- 插件使用离线验收已封存的二进制，SHA-256 为 `06197516bef7ac3195b325c1019e56bb014fdfe2ee5c0a70756154e70e831897`。本轮没有修改产品执行源码，也没有为了通过测试放宽参数或追踪校验。
- 最终仓外验收宿主二进制 SHA-256 为 `725ca06c209155249bca2a3d37e718f505f90f0ab98057b6cacb0c0be1f85621`。它引用本工作树的真实 Runtime、ProviderConfig、Host、Cells、IPC 和 Trace Debug 图生成器。其依赖版本与产品 `Cargo.lock` 核对一致，没有借本次测试升级产品依赖。
- 本次源码、验收程序、锁定文件、日志和响应校验清单：`/out/issue31-live-delivery/source-manifest.json`。旧离线清单保持历史原件，不改写为真实验收。

调用路径：真实 ProviderConfig → 本机只转发并记录字节的 HTTP 中继 → Magpie `127.0.0.1:3425` → 真实流式响应 → 正常 `execute_code` → CodeMode → Host → Cells 持久输出 → Runtime 事件 → 独立插件的认证 IPC → 网页。中继没有生成模型响应、改写模型参数、重试或修改响应正文。它只接受已明确选择的模型和接口，先记预算再转发。

GET `/v1/models` 曾返回 200，但它不计入这 6 次生成请求，也不单独作为真实模型通过证据。只使用文档允许的公开本机应用标识；未读取私人模型凭据文件，未在证据中保存认证头。

## 输入、权限与逐轮结果

每轮使用新建合成工作区，不指向仓库。模型只能看到 Runtime 的工具说明、本轮合成脚本及该任务的工具结果。没有发送私人源码、其他会话追踪、数据库、网页令牌或 IPC 令牌。最终核对脚本还检查了原请求正文中的本机私人路径和令牌变量名。

输入文件内容如下，实际文件尾有换行：

```json
{"label":"issue31 live synthetic input","value":7}
```

其 SHA-256 为 `ae72abb670b750b2a82f3e583931afd7565ffde27505525b58ee046f66b840b1`。初始图输入为 `{"value":7}`，初始状态为 `{"visited":0}`。输入节点实际读取 `input.json`，核对准确内容，然后通过 Host 向 `side-effect.txt` 写入一次固定内容。只预授权这两个准确参数：

```json
{"capability":"fs.read","arguments":{"path":"input.json"}}
{"capability":"fs.write","arguments":{"path":"side-effect.txt","content":"issue31 live side effect written once by the real Host"}}
```

没有开放 Shell、Node、Python、通用网络或其他业务写接口。初始条件故意读取不存在的 `input.amount`；这是被验收的真实节点错误，不是模型生成失败的替代描述。

| 运行 | 实际生成请求 | 模型与接口 | 实际结果 |
| --- | --- | --- | --- |
| `r01` | 1–2 | `opencode-go/deepseek-v4.1-flash`，Chat Completions | 第一次响应通过正常工具执行准确图源码，实际读取、写入后在条件节点失败。第二次响应有正文但结束标记为 `tool_calls`，且没有工具调用；Runtime 将任务记为失败。随后网页的 v2–v4 修复、高低分支和撤销均完成；该结果不改变原任务失败 |
| `r02` | 3 | `factory/claude-haiku-5-5`，Messages | 线上响应把工具参数编码为 JSON 字符串；Runtime 拒绝“Invalid tool call ID, name or arguments”。没有图执行、没有能力调用、没有文件写入 |
| `r03` | 4–5 | `opencode-go/deepseek-v4.1-flash`，Messages | 图通过正常工具执行，条件实际失败，随后模型正常结束任务。网页完成 v2 高分支、v3 低分支、v4 撤销恢复高分支 |
| `r03` 停插件后 | 6 | 同一明确模型与 Messages 接口 | 普通认证 IPC 任务完成，模型准确返回 `ISSUE31_LIVE_NORMAL_OK`，没有工具调用或重复写入 |

这些是分别明确启动的验证轮次，不是 Provider 自动重试或自动换模型。失败 Runtime 保持原样，新的轮次不释放它们的预算。

`r01` 的验收收集器曾按 32 条事件分页读取，返回 `response_too_large_outcome_unknown`。`task.resume` 的返回结果已保存，插件也完整采集到游标 890；因此没有重新提交业务任务。仓外收集器随后改为逐条只读获取原事件，成功补全收据。插件自身没有因该收集器问题修改协议或跳过事件。

线上原件只能证明客户端收到的响应内容；本次没有进一步定位协议问题发生在上游服务、模型还是 Magpie 转换层，不据这些失败归因到某一实现。

## 最终通过轮次

最终目录为 `/out/issue31-magpie-live-r03`，`/out` 在本机解析到 `/Volumes/ORICO`。

| 身份 | 值 |
| --- | --- |
| Session | `7e418fdb-c87a-48e1-82a6-a7acc1752ecf` |
| 实际图任务 | `16bb1997-e982-4ebe-9533-378c3898ce81` |
| 图执行 | `30734c86-d23b-43c9-a299-d1abfd1de8b9` |
| Attempt | `ce5f8fd2-5750-4a1c-ba5b-2e2050a7dbff` |
| Worker | `d7ca4672-780b-47d9-96e5-b842f139981e`，fence 1 |
| 图源码 | `sha256:26ba8705f74f55fbafa2f5256f86b3791aeefced2b0e7c8581e25ad20760cee5` |
| 停插件后任务 | `9bfe50be-a177-41b9-9823-9fc695175ace` |

真实模型工具参数中的源码摘要与 `script_for_plan` 的生成源码一致，插件 `trace_error=null`。没有把模型给出的不同源码拼成可信图。

| 网页操作 | 实际路径与输出 |
| --- | --- |
| v1 原始运行 | `input → condition`，`failed`，最终输出 `null`。错误事实含节点、实际 `value=7`、进入前 `visited=1`、原错误和证据游标 2703 |
| v2 修复 | 将条件读取改为 `input.value` 后保存；`input → condition → high → result`，`classification=high`、`threshold=5`、`value=7`、`visited_nodes=4` |
| v3 参数变化 | 阈值改为 20 后保存；`input → condition → low → result`，`classification=low`、`rule=value <= threshold`、`threshold=20` |
| v4 撤销至 v2 | 从入口重新计算，四步均 `reused=false`；恢复高分支和阈值 5。输入节点使用捕获的读取与写入响应，没有真实重做能力调用 |

v1 继续保留原始错误。v2–v4 明确为 `replay` 本地求值，原始结果为 `observed`。保存路径未执行编译、类型检查、构建、测试或模型请求。

编辑前后收据精确一致：Core 游标 2815，任务数 1，能力调用 2，写入 1；副作用文件 SHA-256 为 `655477af4f447ef2b122b039bbefc4a2658ded14786ae47522a93e7c21c2be75`，`mtime_ns=1792807804004430236`。模型请求事件也没有增加。停插件后的普通任务使游标增至 2845、任务数增至 2；能力调用仍为 2，写入仍为 1，副作用文件内容和修改时间不变。

最终控制台在停服务前为 0 错误、0 警告，浏览器采集的 JavaScript 异常为 0。全部三轮插件和验收宿主退出码均为 0，中继没有活动请求或记录错误。`r02` 浏览器在服务关闭后产生的连接错误保留在 Playwright 日志中，不混作运行前零错误证据。

## 实测呈现耗时

`r03` 浏览器在任务开始前已连接。采用 Runtime 事件时间戳与浏览器 MutationObserver 后下一次动画帧时间计算，均为同机近似；不证明 GPU 已完成显示，不作为普遍性能承诺。

| 路径 | 实测耗时 |
| --- | --- |
| 首个图追踪事件到节点图 | 1,014.1 ms |
| 条件错误到事实摘要 | 882.0 ms |
| 保存修复到 v2 高分支 | 742.8 ms |
| 保存阈值到 v3 低分支 | 697.1 ms |
| 撤销到 v4 新结果 | 673.2 ms |

原始游标、起止时间保存在 `browser-results.json`。真实模型产生了数千条流式事件，结果不与离线少量事件的样本混作同一负载。`r01` 的浏览器连接晚于图执行，其追踪到图和错误到摘要间隔不当作实时延迟；只保留事实和保存结果。没有新增固定毫秒阈值的 CI 断言。

## 用量与预算

本次独立总上限为 100,000,000 token。单次请求先预留 1,100,000 token，并限制输出；全部轮次合计最多 12 次请求。新轮次导入此前请求和保守保留值后才允许发送，不复用其他会话或旧验收的预算。

| 请求 | 上游报告 token | Runtime 终态 | Runtime 已结算 | Runtime 保留 |
| --- | ---: | --- | ---: | ---: |
| 1 | 11,579 | completed | 11,579 | 0 |
| 2 | 13,874 | failed | 未结算 | 1,088,421 |
| 3 | 11,793 | failed | 未结算 | 1,100,000 |
| 4 | 6,593 | completed | 6,593 | 0 |
| 5 | 8,949 | completed | 8,949 | 0 |
| 6 | 8,993 | completed | 8,993 | 0 |
| 合计 | 61,781 | 4 completed / 2 failed | 36,114 | 2,188,421 |

上游用量合计包含服务报告的缓存 token。失败响应虽然带了用量报告，Runtime 仍保留预算，不能把“收到用量”写成“Runtime 成功结算”。首次中继的较宽松终态统计没有成为后续放行依据；最终跨轮总账以 Runtime 失败为依据，为请求 2、3 各保留完整 1,100,000 token。旧中继原件不改写。

最终保守放行占用为 `36,114 + 2,200,000 = 2,236,114` token；剩余放行额度为 **97,763,886 token**。保留值不是已消费 token；上游报告也不是账单。没有可信价格和金额结算，金额状态为 `unknown`，不写零费用。

Runtime 的 `input_token_bound=null`，预算预留不是服务端硬费用上限。本次通过小型合成输入、显式输出限制、有限请求数、发送前持久预留和跨轮保留失败额度约束测试；没有为了耗尽额度循环调用。

## 复跑入口与原件

普通 `cargo test` 不会调用模型。本轮验收程序放在仓外，避免将带真实调用的启动脚本混入离线演示默认入口。实际使用的完整源文件、锁定文件及校验值随本机交付清单保存。

```sh
# 仓外 r03 程序的依赖已与产品 Cargo.lock 核对一致。
/Users/chengwen/.cargo/bin/cargo +1.95.0 build \
  --manifest-path /out/issue31-live-instrument-r03/Cargo.toml \
  --locked --offline \
  --target-dir /Volumes/ORICO/CAYW/.claude/worktrees/issue31-trace-debug/target

# 每次付费复跑仍需该次授权，并保留此前预算。
# r03 中继明确读取 r02 的原始最终收据，导入请求 1–3 后再接收新请求。
python3 -I /out/issue31-live-instrument-r03/runner.py serve \
  --output /absolute/new/issue31-real-run \
  --runtime /Volumes/ORICO/CAYW/.claude/worktrees/issue31-trace-debug/target/debug/issue31-live-runtime \
  --plugin /out/issue31-delivery/bin/cayw-trace-debug

# 先打开 ready.json 中的网页并输入公开合成网页令牌，再显式提交任务。
python3 -I /out/issue31-live-instrument-r03/runner.py task \
  --output /absolute/new/issue31-real-run

# 网页完成修改、分支变化和撤销后，只读导出收据。
python3 -I /out/issue31-live-instrument-r03/runner.py inspect \
  --output /absolute/new/issue31-real-run
python3 -I /out/issue31-live-instrument-r03/runner.py stop-plugin \
  --output /absolute/new/issue31-real-run
python3 -I /out/issue31-live-instrument-r03/runner.py ordinary \
  --output /absolute/new/issue31-real-run
python3 -I /out/issue31-live-instrument-r03/runner.py shutdown \
  --output /absolute/new/issue31-real-run
```

上述代码是本次验收工具的复现入口；再次调用需建立新账本并带入所有已发生请求，不能将旧账本当作新的 100M 授权。浏览器采集脚本为 `/out/issue31-playwright-audit-r04.js`，结果整理与交叉核对脚本为 `/out/issue31-live-instrument-r03/summarize_browser.py`、`audit_final.py`。

| 本机路径 | 内容 |
| --- | --- |
| `/out/issue31-live-delivery/source-manifest.json` | 产品文件、仓外工具、构建与本轮证据校验清单 |
| `/out/issue31-magpie-live-r03/acceptance.json` | 六次请求与 Runtime 终态、报告用量、保留额度、业务收据交叉核对 |
| `/out/issue31-magpie-live-r01`、`r02`、`r03` | 各轮准确输入、配置、命令、原始 HTTP、任务结果、追踪和退出记录 |
| 各轮 `http/*.request.json`、`*.response.sse`、`*.result.json` | 原请求正文、原响应正文及用量摘要，不含认证头 |
| 各轮 `runtime/model-requests.json` | 通过公开 `Runtime::model_requests` 导出的持久账本，含每请求身份与未知保留 |
| `/out/issue31-magpie-live-r03/browser-audit.json`、`browser-results.json` | 保存请求、文档与画面状态、各版本结果及呈现耗时 |
| `/out/issue31-magpie-live-r03/error-facts.png`、`modules-final.png`、`nodes-final.png` | 实际错误摘要及最终模块图、节点图 |
| `/out/issue31-live-task-r03.log`、`issue31-live-after-edits-r03.log` | 修改前后精确一致的业务收据 |
| `/out/issue31-live-without-plugin-r03.log` | 停插件后的普通真实任务收据 |
| 各轮 `shutdown.json`、`http/close.json` | 全部进程退出及请求清理结果 |

产品执行源码未因本轮新增改动；原有 47 项插件测试和 1,751 通过、7 忽略的完整默认工作区测试仍属于离线记录中的那次运行，不冒充本轮重新执行。本文与入口链接完成后只核对文档链接、源文件校验及 `git diff --check`。

本次未验证官方服务直连、其他 macOS 架构、任意未适配脚本或所有 Cargo feature 组合。真实模型补验没有改变节点图和本地求值的既有能力边界。
