# Issue #31：Haiku 线上纠错重测

记录日期：2026-10-24。用户在[当前源码八项验收](issue-31-final-acceptance-macos.md)后要求“重测”。本次沿用此前 Magpie 真实模型测试的独立 100,000,000 token 总授权，并继承全部六次旧请求和旧失败保留额。没有使用其他会话预算。

当前代码上的两条路径均通过：全新 Haiku 任务完成实际工具执行；用旧坏响应触发参数错误后，错误实际返回线上 Haiku，Haiku 生成有效对象参数并完成一次真实执行。第二条路径的首个响应来自历史原件，随后两次响应来自真实模型。它不证明本轮模型自然重现了旧坏参数。

本次只新增仓外验收工具和本文，没有修改产品执行代码。工作树为 `/Volumes/ORICO/CAYW/.claude/worktrees/issue31-trace-debug`，分支为 `plugin/trace-debug`，基线为 `b14b401f37af877e03bbf11594a05dadb1289d47`。没有提交、推送、合并或修改远端 Issue。

## 构建、模型与执行范围

实际环境为 macOS 27.0.1 ARM64、Rust 1.95.0、默认 feature。模型明确选择 `factory/claude-haiku-5-5`，Messages 接口经本机 Magpie `127.0.0.1:3425` 转发，四次实际响应报告模型为 `claude-haiku-5-5`。没有自动换模型、重试或更改真实响应字节。

仓外测试程序位于 `/out/issue31-haiku-live-instrument-r01`。它引用当前工作树的 Runtime、ProviderConfig、Host、Cells、CodeMode、IPC 和 Trace Debug 图生成器，构建使用独立 `target`，成功后复制固定二进制。锁定文件中的 303 个第三方依赖身份与产品锁定文件一致，没有升级依赖。

| 固定二进制 | SHA-256 |
| --- | --- |
| `/out/issue31-haiku-live-instrument-r01/bin/issue31-live-runtime` | `076a0a7829d99f0267567ac0cd64668847d85313fe46cb4d9be000503ce3abd3` |
| `/out/issue31-final-acceptance-r01/bin/cayw-trace-debug` | `b704e87ffd33ced62b31ebc39a50935eb8bea5006da7a9038a4e9dc67c601e21` |

构建命令及退出码见 `build-result.json`。`binary.json` 记录固定副本摘要。构建前工具自动审批服务曾不可用，几次调用没有执行；没有因此发生模型请求。服务恢复后构建与重测正常执行。

输入、预设条件失败和权限与此前真实演示相同：合成输入 `value=7`，条件故意读取不存在的 `input.amount`；只允许准确的 `input.json` 读取及向 `side-effect.txt` 写入固定内容。实际读取结果和文件摘要由 Host 返回。模型只收到本轮合成脚本和工具记录，没有私人源码、真实业务追踪、数据库或访问令牌。中继仅使用此前已验证的公开本机应用标识，不读取私人凭据配置，也不记录认证头。

## 第一轮：全新真实 Haiku 任务

目录：`/out/issue31-haiku-live-r01`。Session `3adee78b-5d98-487b-a1a2-ccd3dbf0968f`，任务 `26e462d4-5694-4812-a129-fdbf72e0c74b`，图执行 `cfb609fe-bdc7-4f73-862a-6c28042a0efc`。

本轮没有注入或回放模型响应。两次生成 POST 均为真实 Haiku：

1. 请求 7 返回一个有效 `execute_code` 调用，参数为对象。源码逐字匹配本轮图生成器输出。真实 Host 读取一次、写入一次，然后图在预设的缺失字段条件处失败。
2. 请求 8 接收真实工具结果后，以 `end_turn` 正常结束任务。任务 completed 不表示预设失败节点通过。

浏览器在任务前连接。真实 trace 自动显示条件失败。随后通过 Playwright MCP 完成 v2 修正字段走 high、v3 阈值 20 走 low、v4 撤销恢复 high。路径、输出和错误对比与持久记录一致，v1 原始失败保持不变，后三轮明确为本地重放。

修改前后 Runtime 游标均为 128，任务 1，能力调用 2，写入 1。副作用文件 SHA-256 均为 `655477af4f447ef2b122b039bbefc4a2658ded14786ae47522a93e7c21c2be75`，`mtime_ns` 均为 `1792832074078484681`。页面错误收集为空，浏览器控制台为零错误、零警告。

本轮说明有效参数下的真实闭环正常，没有触发原来的参数格式错误，不能用它单独证明错误反馈后的纠正。

## 第二轮：原坏响应触发，真实 Haiku 继续纠错

目录：`/out/issue31-haiku-live-r02`。Session `540a5829-871e-4a04-846e-c07cb60ff998`，任务 `424af8f1-593b-45c9-911c-8d4bbd5c21e6`，有效调用创建的执行 `88cc9abd-0732-4a3a-aed8-3a4665a65984`。

为稳定触发原故障，本机中继对本轮第一个请求逐字节返回原 r02 响应，SHA-256 为 `4bb610ed7bcec822b9302c3fd6cacc2fc858d4836153747d47bf7d4066bf4b3b`。这一步没有向 Magpie 发生成 POST，不产生新的模型消费。原 r02 失败任务和原预留没有打开或修改。

实际链路：

| 步骤 | 核对结果 |
| --- | --- |
| 当前适配器接收原坏响应 | 保留调用编号 `toolu_017Pk6hviSvr6NWpZKNEEgEA`、字符串参数和参数错误标记，完整消费响应终态 |
| 当前 Agent 处理坏参数 | 产生同编号 `is_error=true`、`outcome_unknown=false` 的错误结果；坏调用没有创建执行，也没有触达 Host |
| 请求 9 实际发给线上 Haiku | 原参数保存在明确的无效参数发送对象中；错误结果按调用编号配对；一个原始签名块原样保留 |
| Haiku 接收反馈后返回 | 新调用编号 `toolu_01DC1tLBYjXC593mer2Pvpsq`，参数为有效对象，源码逐字匹配本轮图脚本 |
| 当前 Runtime 执行有效调用 | 只创建 1 个执行，实际读取 1 次、写入 1 次；条件按演示设计失败 |
| 请求 10 接收实际工具结果 | 真实 Haiku 以 `end_turn` 正常结束任务，没有再次执行或写入 |

第二轮没有做网页修改。自动纠正发生在正常 Agent、Provider 和 Runtime 路径中，没有手工添加工具结果、修改参数或直接调用 Host。原响应用于触发已知错误，纠正后的调用和最终正文由真实 Haiku 生成。

`verify_live.py` 核对坏调用没有对应执行、调用与结果配对、原参数、签名保留、后续有效源码、真实能力调用和一次写入。签名及思考块仅在本机内存中比较，不在报告中展示正文。

## 新增用量与旧预算

本次新增四次真实生成 POST，全部 HTTP 200、完整 `message_stop`，且 Runtime 均完成结算。

| 总账请求编号 | 场景 | 上游报告 token | Runtime 终态 |
| --- | --- | ---: | --- |
| 7 | 全新任务：有效调用 | 11,643 | completed |
| 8 | 全新任务：工具结果后结束 | 15,025 | completed |
| 9 | 接收原坏参数反馈并纠正 | 18,424 | completed |
| 10 | 纠正后工具结果并结束 | 21,798 | completed |
| 本次合计 | 四次真实请求 | 66,890 | 四次 completed |

第二轮 Runtime 另外记录了本机回放原响应所带的 11,793 token。这是历史响应的用量字段，已单独标记并从新增真实请求数和消费中排除，没有重复计费或释放原失败预算。

结合旧六次请求：实际生成 POST 累计 10 次，上游报告累计 128,671 token；Runtime 实际生成请求已结算累计 103,004 token。旧失败 Runtime 保留 2,188,421 token，跨轮仍按完整 2,200,000 token 保守保留。当前保守占用为 `103,004 + 2,200,000 = 2,303,004` token；剩余放行额度为 **97,696,996 token**。

所有新请求在发送前都持久记录 1,100,000 token 预留。原先设定的累计最多 12 次真实请求仍保留，本次没有为了耗尽额度继续请求。预留不是实际消费，上游报告不是账单，金额状态仍为 unknown。

## 复跑入口与本机证据

真实请求只从独立启动命令和显式任务命令发出，普通测试不连接模型。新的真实复跑应继续使用尚未耗尽的授权和最新总账，不把旧账本视为新的 100M 授权。再次执行时输出目录必须不存在。

```sh
# 已锁定依赖的独立构建；不调用模型。
/Users/chengwen/.cargo/bin/cargo +1.95.0 build \
  --manifest-path /out/issue31-haiku-live-instrument-r01/Cargo.toml \
  --locked --offline \
  --target-dir /out/issue31-haiku-live-instrument-r01/target

# 全新真实响应；serve 只启动，不创建任务。
python3 -I /out/issue31-haiku-live-instrument-r01/runner.py serve \
  --output /absolute/new/haiku-run \
  --previous /out/issue31-haiku-live-r02/accounting.json \
  --runtime /out/issue31-haiku-live-instrument-r01/bin/issue31-live-runtime \
  --plugin /out/issue31-final-acceptance-r01/bin/cayw-trace-debug

# 对新目录显式提交一个合成任务。
python3 -I /out/issue31-haiku-live-instrument-r01/runner.py task \
  --output /absolute/new/haiku-run

# 需要稳定触发原错误时，在新的 serve 命令中明确加 --seeded。
# 此模式仅首个响应来自原件，后续仍是真实请求，必须单独标注。

python3 -I /out/issue31-haiku-live-instrument-r01/runner.py shutdown \
  --output /absolute/new/haiku-run
```

本轮原件：

| 路径 | 内容 |
| --- | --- |
| 两轮 `verification.json` | 自动核对真实调用、错误配对、执行和写入 |
| 两轮 `accounting.json` | 真实请求与 Runtime 结算交叉核对，包含继承的旧请求 |
| 两轮 `http/request-*.request.json`、`.response.sse`、`.result.json` | 真实 HTTP 正文、摘要和用量，不含认证头 |
| 第二轮 `http/seeded-original-response.*` | 原响应字节、本机触发请求及明确的历史来源标记 |
| 第一轮 `browser-audit.json`、`graph-final.png`、`console.txt` | 网页原始文档、实际操作与最终页面 |
| 两轮 `runtime/model-requests.json`、任务结果与事件收据 | Runtime 持久模型账本和真实执行记录 |
| 两轮 `shutdown.json`、`http/close.json` | 插件、Runtime 均以 0 退出，中继无活动请求或记录错误 |
| `/out/issue31-haiku-live-instrument-r01/source-manifest.json` | 当前源码、固定构建、工具和证据的 SHA-256 绑定 |

本次没有再运行全部离线测试；产品生产源码与此前 556 项相关回归及 47 项插件测试一致。真实重测、旧测试和旧失败各自保留，不改写成同一次执行。未验证官方直连、其他 macOS 架构、全部 feature，亦不据本轮声称模型每次都能纠正任意错误。
