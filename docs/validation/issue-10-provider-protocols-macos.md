# Issue #10 模型协议验证记录

记录日期：2026-10-23。对应 [Issue #10](https://github.com/changw98ic/CAYW/issues/10)。

**按用户批准的 Magpie 四格式范围，macOS ARM64 整项本地验收完成。** 用户于 2026-10-23 明确选择 Magpie 四格式作为 #10 的验收范围；GitHub 标题和正文已按批准的清单更新并回读确认，Issue 仍为 OPEN。原 r02 运行共 15 次请求，结果仍为三行通过、Haiku 失败、整体 exit 1；修复后的 r03 程序另用新会话完成 Haiku 两个用户回合，共 6 次请求。累计 21 次真实请求分属两份源码，不能写成同树一次 4/4。原失败请求的 126857 token 预留保留，金额和币种未知。原件位于 `/Volumes/ORICO/cayw-issue10-live-20261023-a`。官方三服务直连和旧 Magpie group 路由保留为“未验证”，按本次决定移出关闭范围。#6 的记录和额度保持封存。尚未进入 #11。

## 源码、平台和证据

- 开发目录：`/Volumes/ORICO/cayw-issue10-integration-20261022-a`，分支 `feat/issue10-providers`。
- 基础 HEAD：`d6d8977249acdd8b54a2bba3db49edbb67465651`。目录还有前项继承的未提交工作；不能把相对 HEAD 的全部差异归入 #10。
- 原件目录：`/Volumes/ORICO/cayw-issue10-evidence-20261022-a`。下文日志名称均相对该目录。
- 本机：macOS 27.0.1，Build 26A434，Darwin 27.0.0，ARM64。
- 工具链：Rust/Cargo 1.95.0，`aarch64-apple-darwin`，`CARGO_INCREMENTAL=0`，Debug 构建。
- 其他 macOS 架构未验证。Linux 和 Windows 不在首发验收范围。
- 现行任务范围：GitHub #10 与本记录的四行矩阵。用户选择和远端回读保存在新目录 `issue10-approved-scope-update-r02.json`；现行结构化清单为新目录 `active-acceptance-scope.json`。旧目录 `issue10-contract.json`、`acceptance-scope.json` 保留为范围调整前的历史原件，不再充当当前关闭清单。资料原件 `protocol-references-r01.json`、`protocol-fields-r01.json`、`google-generate-content-schema-r01.json` 只作协议依据，不证明产品通过。

仓外 `validate.py` 使用唯一标签保存命令、退出码、完整原日志、工具链和命令前后源码清单。源码清单包含受版本控制及未忽略的新文件、文件模式和 SHA-256。原始失败、中断和旧结果不覆盖。新增 `--evidence-dir` 用于保存指定合成输出目录的逐文件摘要；早期未带该选项的 receipt 不具有输出文件逐项封存能力。

仓外 `run_final_local.py --attempt rNN` 顺序运行完整检查、独立客户端、五种协议、两个独立安装目录及 SDK/IPC 脚本；每步核源码摘要不变，失败即停并保留原件。第四轮已完成全部 15 步；具体结果见下文。不同源码清单上的定向通过，不能合称一次最终全仓通过。后续源码不能沿用第四轮通过声明，须核对应新轮次的具名原日志、receipt 与 `final-local-suite-rNN.json`；失败轮次不覆盖。

`final-local-suite-r01.json` 已实际运行：整仓和独立客户端格式、workspace check 通过；workspace test 在编译 `cayw-core/tests/execution_wait_recovery.rs` 时因一处 `AssistantMessage` 构造遗漏新增 `usage_fields` 字段而 exit 101，没有执行测试。该轮源码稳定，后续步骤未运行。已只补空原字段映射，保留 Unknown 用量和原恢复断言；`core-wait-usage-field-green-r01` 八项通过且源码稳定。`final-local-suite-r02.json` 的两项格式和 check 仍通过，但 test 编译又发现 `cayw-core/tests/recovery_bounds.rs` 同样遗漏 `usage_fields`，exit 101，尚未执行测试。该处也只补空原字段映射并保持 Unknown。两轮原件继续保留，随后先集中编译全部测试目标。`all-test-targets-compile-r01` 错把 `--keep-going` 用于 `cargo test`，参数被拒绝，不是源码编译结果；核对 Cargo 文档后改用 `cargo build --workspace --tests --keep-going --locked --offline`。`all-test-targets-compile-r02` 又发现 `cayw-core/tests/execution_waits.rs` 同样漏字段，已只补空映射。`all-test-targets-compile-r03` 全部测试目标编译通过且源码稳定，它没有执行测试。完整检查须在新轮次重跑。

`final-local-suite-r03.json` 的两项格式和 check 通过；workspace test 实际执行完毕，exit 101，源码清单首尾一致。按每个 Cargo 测试目标最后一条结果计数，为 1624 passed、33 failed、7 ignored，9 个测试目标失败；早先把三条内部子进程成功汇总重复相加，得到 1627 passed，另存 `r03-top-level-count-correction-r01.json` 说明计数差异，没有改原日志或旧报告。汇总包含仓内已有可选包，不表示本项通过。后续 Clippy、安装和 SDK/IPC 组合步骤未执行。两项曾超过 60 秒的 CodeMode 测试后来均通过，没有挂起。原始失败和计数另保存在仓外 `final-r03-failure-triage.json`、`r03-failure-counts.json`，未覆盖前三轮原件。

该轮暴露的调用方迁移包括：取消已持久接受后，Agent 可见回复为 Aborted，原模型 Done/Stop 另由账本保留；旧测试却始终期望 Stop。系统说明测试仍读取 System 的旧 content，未读取当前有效 section。分页测试的首个成功合成回复未报告用量，原预留阻止下一次 Provider 调用。VerifyTool 在 helper 已返回 Reported 4/4/8 后，把 total 改为 1，因用量矛盾被实际账本拒绝，五个验证目标因此不能生成成功基线，尚未进入证据破坏后的 reader 检查。对应修改保留全部权限、取消、分页和证据断言，不放宽账本。

这些调用方迁移已实际定向复验：`cancel-terminal-migration-green-r01` 为 4 项取消定序和 7 项 Runtime 测试通过；`tui-page-migration-r01` 为 2 项真实 loopback IPC 测试通过，原 220 片和分页检查保留；`verifytool-usage-migration-r01` 的五目标共 43 项通过，成功基线及证据破坏检查均实际执行。三次运行均 exit 0，且绑定同一稳定源码清单。主线程已读原日志并复算摘要，取消四场景 648 份输出均逐文件核摘要；关键原账本、身份、历史、迟到控制和重开事实另由主线程与独立只读复核核对。原件入口为 `r03-migrations-verified-r01.json`。这些结果证明相应测试消费入口及合成数据迁移，不是产品新缺陷 red→fix，也不是最终整仓结果。

`streaming-isolated-red-r01` 在单独运行时复现原 512 片回复的 `Budget("time")`，exit 101 且源码稳定。源码核对显示，每片观察无条件重建模型账本，反复读取不断增长的 Agent 历史；这是实际存在的重复工作路径，没有逐函数计时来分配扫描、解码和磁盘提交各自耗时。最小产品修改仅合并请求内连续相同的完整规范化用量观察；事件类型、用量状态、计数、原字段、响应身份、模型、结束原因或脱敏状态变化均另存，A→B→A 不跨变化合并，Done/Error 始终保存。重复分支仍在原协调锁内核持久取消并显式核 Core lease，接收数量上限、逐片用量检查、失败预留和终态保护不变。全部正文事件仍按原 Agent 路径持久化。

修改前 `streaming-observation-boundaries-red-r01` 的 12 项中 5 项实际失败，`streaming-history-red-r01` 同样因原期限失败。修改后 `streaming-observation-boundaries-green-r01` 12 项通过；`streaming-accounting-green-r01` 的原分片测试和新增完整历史测试各 1 项通过，测试段分别为 2.77 秒和 2.99 秒。原 `streaming.rs` 文件未改，仍为 512 片、8192 字符、30 秒任务期限及 Unknown 用量；时间记录不是固定性能保证。新增测试逐页核所有 512 条 TextDelta 的顺序、原 delta、累积正文、原身份、最终消息及重开不重放；原预留和未知金额保留。`streaming-model-boundary-regression-r01` 的取消定序、说明、公开账本边界、HTTP 期限和用量五目标共 24 项通过。三次修改后运行绑定同一稳定源码清单；最终整仓仍须新轮次完成，独立复核与最终封存以仓外具名原件为准。

## 第四轮同一源码结果

`final-local-suite-r04.json` 的 15 个步骤均 exit 0，运行源码清单首尾为 `720109b65d681479f9e8c2129dd0fe3c8786b1a366d78df54c0b60cc759a9118`，538 个文件。实际命令时间为 UTC 2026-10-22 20:05:31 至 20:17:39。整仓格式、独立客户端格式、check、Clippy、离线示例、独立客户端三项检查、五配置协议、配对检查、七项脚本自测、两个程序独立安装及 SDK/IPC 组合运行均通过。

整仓按 260 个 Cargo 测试目标的最终结果计为 1665 passed、0 failed、7 ignored；三条内部子进程汇总未重复相加，顶层没有过滤项。独立客户端另有 13 passed、0 failed。六个 ignored 项是由父测试调用的子进程辅助入口；另一个二进制集成测试明确 ignored，本轮没有单独执行。Cargo 提示多个已有 `offline` 示例同名输出冲突；显式 `cayw-agent` 示例和两个全新安装目录均各自运行通过。该命令没有启用所有可选 feature。原计数及范围见 `final-command-counts-r04.json`，不声称所有可选包组合均已验收。

`verify_final_local.py --attempt r04` 已 exit 0，回读核对全部命令、原日志、源码字节与模式，以及 160+45+3+3+721=932 份输出文件的摘要。核对结果为 `final-local-suite-r04-verification.json`。实际 Runtime 以 `--no-default-features --debug` 安装，独立客户端依赖同样关闭 Runtime 默认 feature；两者均未启用 plugin/remote。程序 SHA-256 和原 Cargo 安装信息保存在 `final-build-binding-r04.json`。实际组合运行保存 18 次合成 HTTP 请求、SDK 和认证 IPC 两种模式；真实请求仍为 0。

第四轮之后已补齐原生协议结束原因的 HTTP 断言。`native-stop-http-r01` 实际执行 39 项测试，全部通过，运行期间源码清单首尾为 `ebde2948ed98c15fff0e015a1d580e18bc83500cbb0ddbec6c0af171a79d916d`。新增两个参数化测试共七个场景：Anthropic 的 `stop_sequence`、`max_tokens`、`model_context_window_exceeded`；Gemini SSE/JSON 分别验证纯文本 `MAX_TOKENS` 和含完整函数调用的 `MAX_TOKENS`。检查唯一终态、正文、已收到的用量更新、原计数字段、响应 ID，以及请求模型与实际返回模型。前者按已有规则映射 Stop/Length；Gemini 含任何函数调用时返回 Error，不能产生 Done。`ToolCallEnd` 只表示参数解析完成，不能据此认定工具执行成功。README 已写明这个范围。产品分支未改，这是补覆盖，没有产品缺陷 red→fix。`native-stop-format-check-r01` 仅因一个断言需换行而退出 1；修正格式后 r02 通过，原件均保留。

第四轮的两组五配置原件和实际安装 SDK/IPC 原件均已完成独立只读复核。在各自已核范围内，没有发现具体产品错误。详细结果、亲核数量及局限分别保存在 `r04-protocol-independent-review-r01.json`、`r04-installed-independent-review-r01.json`。这些复核仍只证明第四轮源码和原件。新增测试和本记录修改后，最终本地结果须另核 `final-local-suite-r05.json` 及对应 verification、delivery、seal；文件未生成、步骤失败或源码变化时，不得宣称新轮次通过。

## 服务、API 与真实任务矩阵

下表是用户于 2026-10-23 批准的当前关闭范围。网关固定为 `http://127.0.0.1:3425`。每行只证明自己的精确模型、配置、源码与实际传输。路径和证据目录均为实际运行记录。

| 服务 / API | 固定请求模型 | 路径 / 公开传输 | 已完成的真实任务与原件 |
| --- | --- | --- | --- |
| Magpie / Chat Completions SSE | `opencode-go/deepseek-v4.1-flash` | `/v1/chat/completions`；SDK | r02，两个用户回合、4 请求；`magpie-four-protocol-live-r01/chat/row-result.json` |
| Magpie / Responses SSE | `codex/gpt-6-luna` | `/v1/responses`；认证 IPC | r02，两个用户回合、4 请求；`magpie-four-protocol-live-r01/responses/row-result.json` |
| Magpie / Anthropic Messages SSE，版本 `2023-06-01` | `factory/claude-haiku-5-5` | `/v1/messages`；SDK | r03 新会话，两个用户回合、6 请求；`magpie-haiku-retry-r01/messages/row-result.json` |
| Magpie / Gemini SSE | `antigravity/gemini-3.8-flash` | `/v1beta/models/antigravity/gemini-3.8-flash:streamGenerateContent?alt=sse`；认证 IPC | r02，两个用户回合、4 请求；`magpie-four-protocol-live-r01/gemini/row-result.json` |

每行均保存配置、请求模型与响应模型、原请求和响应 body、完整事件、精确审批、文件结果及会话重开记录。SDK/IPC 基础覆盖由 #6 和本项本地组合测试另证，不把矩阵解释为每种格式都已运行全部传输组合。原 Haiku 失败行继续保留，不能由成功重试覆盖。

OpenAI、Anthropic、Gemini 官方服务直连，以及旧 Magpie `group/auto-deepseek-v4-1-flash` 路由，均为“未验证”；本次明确移出 #10 的关闭范围。网关结果不能转称官方直连通过。没有读取私人模型凭据文件。品牌或 API 名称不代表全部模型能力均支持。

下表集中列出每行的能力、限制和证据链接。每个“输入/配置/运行”链接均包含实际 `model_configuration`、两次用户输入 `round1.task.input` / `round2.task.input`、nonce 源文件摘要、审批、完整工具和模型记录。相邻 `evidence/` 保存原命令、事件和 relay 请求/响应 body，`workspace/` 保存实际文件。请求先送本机记录中继，再由中继转发到固定 Magpie 网关；中继端口随行记录，不把它当成另一模型服务。

| 已验行 | 本地协议能力和明确限制 | 输入、配置与实际运行 | 源码 / 构建 |
| --- | --- | --- | --- |
| Chat / DS Flash | 文本、分片工具参数、多调用配对、结束原因、用量、取消和错误；本行真实任务为文本文件操作。Magpie 无签名 reasoning 按显式服务配置回传；非空原生元数据拒绝。真实图片和其他模型未验证 | [输入/配置/运行](/Volumes/ORICO/cayw-issue10-live-20261023-a/magpie-four-protocol-live-r01/chat/row-result.json)；[调用原件目录](/Volumes/ORICO/cayw-issue10-live-20261023-a/magpie-four-protocol-live-r01/chat/evidence)；[授权计划](/Volumes/ORICO/cayw-issue10-live-20261023-a/authorization-and-plan-r01.json) | [r02 源码](/Volumes/ORICO/cayw-issue10-live-20261023-a/source-r02.json)；[r02 构建](/Volumes/ORICO/cayw-issue10-live-20261023-a/final-build-binding-r02.json) |
| Responses / GPT-6 Luna | 文本、交错参数分片、原 item/call 身份、加密 reasoning 回传；仅合法 `response.completed` 成功，未完成/拒绝明确失败；不发布明文 reasoning，真实图片和服务端工具未验收 | [输入/配置/运行](/Volumes/ORICO/cayw-issue10-live-20261023-a/magpie-four-protocol-live-r01/responses/row-result.json)；[调用原件目录](/Volumes/ORICO/cayw-issue10-live-20261023-a/magpie-four-protocol-live-r01/responses/evidence)；[授权计划](/Volumes/ORICO/cayw-issue10-live-20261023-a/authorization-and-plan-r01.json) | [r02 源码](/Volumes/ORICO/cayw-issue10-live-20261023-a/source-r02.json)；[r02 构建](/Volumes/ORICO/cayw-issue10-live-20261023-a/final-build-binding-r02.json) |
| Messages / Haiku | 文本、JSON 参数分片、多工具配对、原 thinking/signature/redacted 块顺序回传；空签名或缺终态失败。真实覆盖空 thinking 签名，非空 thinking/redacted 只经本地测试。显式 thinking 控制、非 direct caller、服务端工具拒绝 | [输入/配置/运行](/Volumes/ORICO/cayw-issue10-live-20261023-a/magpie-haiku-retry-r01/messages/row-result.json)；[调用原件目录](/Volumes/ORICO/cayw-issue10-live-20261023-a/magpie-haiku-retry-r01/messages/evidence)；[单次重试授权](/Volumes/ORICO/cayw-issue10-live-20261023-a/haiku-retry-authorization-and-plan-r01.json) | [r03 源码](/Volumes/ORICO/cayw-issue10-live-20261023-a/source-r03.json)；[r03 构建](/Volumes/ORICO/cayw-issue10-live-20261023-a/final-build-binding-r03.json) |
| Gemini SSE / Gemini Flash | 文本、完整 `functionCall.args`、多调用配对、原 Part 和不透明签名回传；未启用可选参数分片流。含函数调用的 `MAX_TOKENS` 失败；明文思考、grounding、服务端执行及 Vertex 认证拒绝。非流式模式只有本地测试 | [输入/配置/运行](/Volumes/ORICO/cayw-issue10-live-20261023-a/magpie-four-protocol-live-r01/gemini/row-result.json)；[调用原件目录](/Volumes/ORICO/cayw-issue10-live-20261023-a/magpie-four-protocol-live-r01/gemini/evidence)；[授权计划](/Volumes/ORICO/cayw-issue10-live-20261023-a/authorization-and-plan-r01.json) | [r02 源码](/Volumes/ORICO/cayw-issue10-live-20261023-a/source-r02.json)；[r02 构建](/Volumes/ORICO/cayw-issue10-live-20261023-a/final-build-binding-r02.json) |

所有行的本地协议能力由 [Provider 能力矩阵](../../crates/cayw-providers/README.md#protocol-capability-matrix)和各行固定 HTTP 测试证明，不能当作本次真实任务逐个触发了全部反例。关闭连接不证明上游停止计费，缺用量和失败请求保留预留，金额均未知。

## 已实现的有限协议范围

OpenAI 与 Magpie 共用一份 Chat Completions 实现。Responses、Anthropic Messages 和 Gemini 分别实现原生协议。Magpie 可显式选择四种格式；服务始终记为 `magpie`。Responses 保存原 output 条目、item ID、call ID 和加密 reasoning；Gemini 保存原 parts、签名和标准化调用 ID 映射。回放前核对原件与标准化内容一致，不伪造协议身份。`ProviderConfig` 是无界面启动的唯一模型配置对象，明确指定服务、API、模型、端点、版本、输出上限及凭据环境变量名称。加载和验证不读取凭据、不联网；构造选中适配器时才读取所选的一项凭据。旧三项 `CAYW_PROVIDER_*` / `CAYW_MODEL` 不再选择或覆盖 `cayw serve` 的模型。

请求创建是惰性的。默认客户端不重试、不跟随跳转、不自动换模型。取消检查覆盖联网前、响应头等待、HTTP body、JSON 扫描和事件之间；丢弃流会释放待完成的网络操作。连接关闭不能证明服务已停止处理或计费。

支持文本、宿主明确启用的用户图片，以及文本加 `structured_content` 的工具结果。图片只校验 MIME 声明和标准 base64，不验证解码后的真实图片格式。工具图片明确拒绝。Anthropic 当前修复将 thinking、signature 和 redacted_thinking 保存在 `provider_metadata["anthropic"].content`，按原块顺序回传，不作为普通思考或最终正文。思考文本可以为空；完整块需保留非空签名，签名 delta 按官方累积规则替换而非拼接。Gemini 的明文思考仍拒绝；不透明签名保存在原 Part 并原样回传。缺原 parts 的 Gemini 工具历史在联网前拒绝，本地生成的调用 ID 不会写成服务 ID。Responses 的加密 reasoning 保存在原 output 条目，不发布为明文思考。Chat Completions 拒绝思考历史；显式选择 `model.provider="magpie"` 时，无签名思考按原序写入 `reasoning_content`，该兼容服务行为以本轮实际网关验收为准。Chat Completions 拒绝非空协议元数据，其他原生协议只接受自己的元数据。Anthropic 修复已在 r03 完成本地完整检查和独立真实重试；原 r02 失败结果保持不变。

失败、取消、Pending、空 Assistant 历史及非法消息顺序在发 HTTP 前拒绝，不静默删除失败观察。请求需以用户消息或完整配对的工具结果结束。历史工具调用需保留原 ID、名称、对象参数和合法结束原因；存在参数解析错误、缺结果、重复 ID、名称不匹配或未知效果时拒绝继续。原生协议还按历史时刻的工具声明校验参数；OpenAI 的实际工具执行仍由公开 Agent 的已注册工具和工具自有参数转换后校验。

OpenAI/Magpie 与 Anthropic 拼接协议中的 JSON 参数片段，再决定调用是否完整。Gemini 本项有限实现接收完整 `functionCall.args` 对象，不启用可选的不完整参数流。TCP 分段不等同协议参数分片，也不保证消费者以同样大小读取。

适配器设有明确的字节、帧、内容片段、工具调用、JSON 节点和深度限制。共享严格 JSON 解析器只有 `cayw-ai::json::parse_response` 一份；重复字段、越界或不完整数据明确失败。单个非字符串标量最多 4 KiB。JSON 每 4 KiB 或 64 次结构处理让出执行；单次有界字符串转换、Schema 校验和消息复制不属于可逐指令中断的操作。具体限制见 [Provider 说明](../../crates/cayw-providers/README.md)。

## 用量和持久预算

用量状态区分 `Unknown`、`Partial`、`Reported`、`Invalid`。只有输入和输出都明确报告才是 `Reported`，明确零与缺失不同。计数错类型、累计下降、缓存/推理子计数矛盾、总量不一致和溢出不能产生有效报告。`usage_fields` 保留收到的已知计数字段；`UsageUpdate` 在最终协议事件前发布已收到的用量，使取消和后续错误仍保留这些观察。

Runtime 的模型请求账本保存原 Session、Task、完整 Attempt、模型请求 ID、请求序号、服务和模型。每次请求先预留同一持久预算树的全部剩余额度，Reserved 和 Started 写入成功后才调用底层 Provider。子会话、Fork、新 Task、数据库重开和提高配置不重置历史欠账。同一预算树只允许一个未结算请求。

仅成功 Done、合法结束、未取消、完整有效用量、未超预留且终态提交成功时结算。失败、取消、缺终态、流被丢弃、无效/缺失用量和记账失败都继续占用原预留。没有可信输入 token 上界，`input_token_bound=None`；输出上限会按余量收紧，但这不是服务端硬费用上限。金额没有可信价格，`cost_status=Unknown`，金额和币种为空。Reported 不代表账单结清，预留也不是已发生费用。

持久取消与 Started/Finished 在同一协调范围内定序。取消先于 Started 提交时，不准入 Provider；Started 已先提交的请求可能已发生费用，迟到的 Done 不能释放取消后的预留。模型费用未知和工具 `outcome_unknown` 分别保存，不能相互替代或清除。

## 已保存的反例与定向回归

这些记录只证明各自源码和输入。最终同树结果另行记录。

| 发现与实际反例 | 修复或新增检查 | 已有原件与准确范围 |
| --- | --- | --- |
| 重复 JSON 字段被后值覆盖 | 严格解析拒绝歧义对象 | `duplicate-tool-fields-red-r01`、`duplicate-object-parser-red-r01`、`duplicate-tool-fields-green-r01` |
| 缺失/矛盾用量被当成有效总数 | 四种状态、原字段、累计及溢出检查 | `usage-presence-red-r02`、`usage-conflicts-*-red-r01`、`usage-tool-prompt-red-r01`、`usage-conflicts-green-r03` |
| Anthropic 终止后的坏字节随网络分块改变结果 | 每次只消费到一帧，终止后立即释放 body | `network-lifecycle-green-r02` 保存 1+37+3=41 项通过；失败原件继续保留 |
| 图片/思考/结构化结果被静默改写或省略 | 按显式能力拒绝或完整保留 | `request-content-red-r02`；后续请求内容回归纳入 `history-sequence-green-r01` |
| Error/Aborted 历史被静默丢弃，非法消息顺序仍联网 | 联网前校验历史，不构造修复后的假历史 | `history-sequence-red-r02` 逐项跑 OpenAI/Magpie 的失败/取消、顺序、结束、空消息和参数错误；`history-sequence-green-r01` 共 102 项通过，但全树运行中有修改，不能作最终同树证据 |
| 记录满后仍调用 Provider、无关根会话受其他根容量影响、SDK 接受零输出、持久取消后仍结算迟到 Done | 预算树内投影及写前容量检查、预留终态空间、原身份取消检查、SDK 零输出拒绝 | `model-ledger-boundary-red-r02`：6 项中 4 失败；`model-ledger-boundary-cancel-red-r01`：Started 守卫失败；`model-ledger-boundary-final-r02`：库 29、公开边界 6、HTTP 用量 10，共 45 项通过且源码稳定；同一清单上的定向 Clippy 和四文件格式检查通过。主线程及独立只读复核已核原日志与源码绑定；Runtime 全包和最终同树仍待运行 |
| 公共固定成功回复未明确报告用量，或错误覆盖总量为 7 | 仅明确成功分支补合成 4/4/8、Reported，取消/失败/panic 保持原语义 | 原 `runtime-fixture-consumers-r01`：16 个目标、45 项失败；分别通过 main 23、process 22、requests 14、bridge 53 项。112 项来自不同源码清单，不能合成全包通过 |
| 现有多工具测试未自动核流索引，结果两来源值相同 | 逐个核 start/delta/end 的原索引/ID/参数，文本及结构化结果使用不同值 | `protocol-pairing-r02`：一个 Rust 测试依次跑五配置并通过。属于补测试覆盖，产品直接通过，没有产品缺陷 red→fix |

记录器新增输出摘要后，`recorder-boundaries-red-r01` 实际运行 2 项测试并报告 3 个失败：输出根被替换成目录链接仍可扫描，以及命令退出 0/7 后扫描异常导致缺少 receipt。修复后 `recorder-boundaries-green-r01` 两项通过，源码稳定。内部两份 receipt 分别保留命令原退出码 0/7，证据完整状态均为 false；外层分别返回 125/7。该检查只使用新建的合成目录。

模型账本的 4096 条容量数据是测试合成的存量记录，不是 4096 次真实请求。事件容量使用生产同一守卫路径及内部测试限额 4；线上限额仍为 262144，没有声称完整线上上限已实际跑完。Fork 检查覆盖分支再 Fork 与重开，不外推未跑的 Fork/child 组合。模型写失败保留工具未知标记的单元守卫不等于真实 Host 先发生未知后再失败的完整组合任务。

多工具测试实际写十个临时文件。第一调用等待第二调用的结束事件后再写，因而确实逆序完成。第二次 HTTP 请求和内存历史仍须恢复原调用顺序，分别保留文本与结构化结果。该项使用公共 Agent，不代替 Runtime 授权验证。

`provider-deadline-local-r01` 新增两项公开 Runtime + OpenAI 适配器的真实数字 loopback HTTP 用例：收到完整请求但不返回响应头；收到明确 11/7/18 用量且 `UsageUpdate` 已持久化后保持流未结束。两项均由 5 秒任务期限结束，60 秒 watchdog 只防挂起，没有毫秒速度断言，也不用 sleep 定序。实际结果均为 `RuntimeError::Budget("time")`、Task Failed，用户 token 和 `cancel_requested` 均未取消。原 1000 token 预留保留、未结算、金额 Unknown；第二项保留 Reported 计数及原字段。重开后原 Task 和完整账本相同，新用户任务不能清除预留或发第二次 HTTP 请求。两项首次直接通过，源码稳定；主线程和独立只读复核已核源码及原 log/receipt/source。这是补覆盖，没有产品 red→fix。它只证明这两个 OpenAI 本机用例，不外推其他协议或真实服务。

`provider_protocols.rs` 另通过公开 Runtime、Cells 和真实 Host 跑五配置的两个用户回合，每配置两写三读；核原参数/ID、工具结果、磁盘、后续请求、游标、Completed Task 及重开不重放。这些模型响应均由本机 HTTP 服务合成。

## 公开 SDK / IPC 和系统说明

旧 `config-final-loopback-script-r01-results` 保存 18 次合成请求，以及 SDK/IPC 各两个用户回合、逐次精确审批、拒绝、流取消、工具取消、事件分页、重开、认证错误、执行断连和审批前退出恢复。独立只读复核核对了原文件、事件和工具身份。该次程序构建不是最终同一源码，部分原始 IPC 帧、OS 观测和断连/崩溃后完整记录缺失，不能将旧摘要当完整最终证据。

旧请求还证实第二用户回合会重复拼接整段 CodeMode 系统说明，834 字节增长为重复两份的 1670 字节。`instruction-red-r01` 实际断言数量 2 而期望 1；固定命名 section 后，`instruction-final-test-r01` 的新 Session 两回合检查通过。旧匿名说明还需归入同一有效来源：`instruction-legacy-red-r01` 在持久历史关闭重开后实际出现 3 份，期望 1。修复仅在给 Agent 的历史副本中迁移全文严格相等的 Runtime content，不改数据库原事件，不删除相似宿主文字。`instruction-legacy-final-test-r01` 两项通过，`instruction-legacy-clippy-r02` 通过且没有警告；原日志、receipt、源码绑定已由主线程和独立只读复核核对。原宿主 content、其他 section、tools_removed、持久 messages/events 前缀、精确审批、真实 `fs.read` 结果和原工具 ID 保留。tools_added 的保留来自源码检查，该测试没有非空 tools_added；Fork 未测。这些测试使用合成 Provider，不是 HTTP 或真实模型。

脚本记录原 IPC 帧、hello 认证关联、关闭时间、指定受管进程的 OS 命令原件，以及断连/崩溃前后完整事件检查点。`config-evidence-red-r01` 和 `config-evidence-checkpoints-red-r02` 分别保留三项和两项实际失败；`config-evidence-green-r01` 五项脚本自测通过。独立复核另发现终态查询未传入原 PID/启动时间，可能漏掉已经换父进程的原 sleep。`config-evidence-original-pid-red-r01` 两个真实 OS 反例均复现；最小修复保存并复查本次观测的原身份，`config-evidence-original-pid-green-r02` 七项通过。首轮复验误选未排序的旧 ps 收据而失败，修正测试排序后通过，没有放松进程存活断言。主线程和独立只读复核已核源码及原件。自测的父进程变化和 sleep 为真实 OS 行为，Scenario 状态及恢复控制为合成；crash 新测试在首个检查捕获原进程，其余阶段接点来自源码核对。OS 启动时间只有秒精度，PID 复用未测。这是脚本证据缺口修复，不能称为 Runtime 回归，也不能代替真实两个程序的完整运行。

第四轮组合运行已使用重新构建的两个程序和全新 `final-installed-chat-r04-results` 目录。独立复核逐项核了 721 份输出摘要，并只读核两份本次合成数据库。SDK 的 301 条和 IPC 的 364 条 Core 事件共 665 条；客户端 stdout 保存的 2715 条事件记录均与对应数据库事件相等。SDK 和 IPC 两用户回合均到 cursor 156，八次真实文件能力调用都保留精确审批与原身份。流取消的两份 prompt 原输出各覆盖 1–24，工具取消各覆盖 1–53，终态 after、公开 cursor 和数据库一致。因此本次没有取消终态事件或模型记录缺失，也没有额外模型请求。

流取消和断连保持 Unknown 用量、原 8192 token 预留及未结算状态。工具取消前，原模型 ToolUse 已按明确 41/9/50 结算；随后真实进程停止，工具能力结果保持 `outcome_unknown`，原 Task 为 `outcome_unknown`，会话为 `needs_reconciliation`，重复任务被拒。这里公开 `frozen` 字段为 false；不能将其写成 true。原进程定向查询及清理审计在任务终态前完成，OS 的启动时间仍只有秒精度，不能外推任意 PID 复用或全部后代清理。

断连的 19→24 条事件与审批前退出恢复的 37→37→39→39 条事件均核原前缀；原模型身份和已完成用量不被改写，HTTP 次数不增加。五份 IPC 帧日志共 21 个实际协议帧，认证、请求关联和拒绝边界均已核。检查点仍只代表本 Session 的 Core `host/model_request` 原事件，不是 SDK 预算树或金额投影。脚本没有自动断言每条取消路径的全部事件、HTTP 次数和恢复前缀；本次这些事实由原输出和数据库独立核实。原 HTTP 请求文件是解析后的 JSON，准备的 SSE 和 `transport.complete` 不能单独证明消费者已读完，本次另由持久 Assistant 和模型终态核对。详细范围见 `r04-installed-independent-review-r01.json`，不外推真实服务。

## 本轮四格式准备与本地检查

本节原件均位于 `/Volumes/ORICO/cayw-issue10-live-20261023-a`，与旧封存目录中同名轮次分开。`run_bound_suite.py --attempt rNN` 在每步前后核完整源码清单；全部 16 步还包括原生 Responses HTTP 原件、独立客户端、两个全新安装程序及公开 SDK/IPC 组合运行。完整结果须以该目录实际生成的 suite、verification 和 build binding 为准；缺文件、失败或源码变化时不称通过。

新目录 `final-local-suite-r01.json` 的格式及 workspace check 通过。workspace test 实际执行完毕，exit 101，源码清单首尾均为 `dd26e75ad0583c1552b851ff8a28a7d6b5256fe9d0548145bda45933438dbd75`。按 262 个 Cargo 顶层目标的最终汇总计为 1688 passed、1 failed、7 ignored；内部子进程汇总未重复相加。唯一失败为 `provider_cli` 的合成 Responses `response.created` 缺 `object:"response"`。仅补齐这份合成响应后，`provider-cli-fixture-green-r01` 的 3 项通过。未放宽产品解析规则；原失败日志和 `workspace-r01-failure-counts.json` 保留。

只读独立复核另复现两处 Responses 产品缺陷：合法 `reasoning_text` content part 被拒绝，以及非零 `cache_write_tokens` 被重复计入普通输入。复现原件在 `protocol-review-r01/reasoning-text-harness/run-r04.log`。修复后按 item ID、output/content index、片段顺序与完成内容核对 reasoning；不发布可见思考，原加密条目经序列化后在第二请求回放。普通输入扣除缓存读取和写入两项，并检查相加溢出、超过输入及缺失父计数，原 usage 字段保留。`responses-review-red-r02` 先有 4 项失败；`responses-review-green-r04` 的 18 项、`responses-review-all-r03` 的整包 65 项、定向 Clippy 和两文件格式检查均通过。主线程回读原日志并复算摘要；这些结果只证明各自定向范围。只有 `response.completed` 可产生成功终态的原约定保留；`response.incomplete` 继续失败。

本轮 Gemini 3.8 文档明确不支持 `candidateCount`。`gemini-candidate-red-r01` 先在本机 HTTP 请求中复现该字段仍被发送；移除无条件参数后，`gemini-candidate-green-r01` 的网关 4 项与原生 HTTP 39 项通过。该结果是合成回归，不证明实际 Magpie 接受请求。保持原签名、原调用 ID、缺失 ID 的原表示及 4096 输出上限；真实返回的结束原因、用量和调用身份须核原响应。

四格式真实任务使用公开客户端、精确的一次性文件审批和本轮新安装程序。调用脚本及其纯合成自测保存于新目录，源码和程序摘要由主线程另核。脚本核精确模型、各协议输出上限、prompt 退出码、两回合各自回答及失败行请求计数；失败账本与子进程清理状态单独记录。主线程追加 `runner-final-answer-red-r01`，复现把元数据、工具参数、思考或此前回答当作最终正文的 4 个错误；限定到最后一条助手消息的文本片段后，`runner-final-answer-green-r01` 的 7 项和 `runner-selftest-main-r01` 的 20 项合成自测通过。失败请求必须计入总数；费用保持未知。自测、模型目录 GET 和本机 HTTP 测试均不算真实生成任务。实际真实请求数与两用户回合结果只以新目录具名 `report.json`、逐行 `row-result.json`、原 HTTP body、公开事件和文件结果为准。

## 本轮实际网关结果与 Anthropic 修复

新目录的 `final-local-suite-r02.json` 已完成 16 步，全部 exit 0。源码清单为 `a1ce74163974ee0375d3b00fa4e0da8a29ca297d2301ec78cea860d423e1bf99`，541 个文件、8726555 字节。整仓按 262 个顶层目标计为 1693 passed、0 failed、7 ignored；独立客户端为 13 passed、0 failed。定向的 4+1+18 项属于已有目标重跑，不重复加入整仓计数。`final-local-suite-r02-verification.json` 已核 1123 份输出；`final-build-binding-r02.json` 绑定 ARM64 Debug 程序，Runtime 关闭默认 feature。安装使用同一源码产物复用，不是从空 target 重建。源码归档为 `source-r02.tar.gz`。

`magpie-four-protocol-live-r01.json` 记录实际运行时间 UTC 2026-10-23 04:15:15 至 04:16:15，源码、runner 和两个程序前后未变。实际结果：

| Magpie 格式 / 模型 | 用户回合 | 模型请求 | 结果 |
| --- | --- | --- | --- |
| Chat Completions / `opencode-go/deepseek-v4.1-flash` | 两个均完成 | 4 | 通过 |
| Responses / `codex/gpt-6-luna` | 两个均完成 | 4 | 通过 |
| Messages / `factory/claude-haiku-5-5` | 第一个任务未完成 | 3 | 失败 |
| Gemini SSE / `antigravity/gemini-3.8-flash` | 两个均完成 | 4 | 通过 |

15 次请求全部有原 HTTP body 和摘要，持久 Started 数与实际转发数均为 15。三条通过行都核对精确一次性审批、114/124 字节的实际文件、最终可见正文中的随机 nonce，以及后续请求中的原工具身份和结果。所有行收尾均完整，受管子进程停止。独立只读复核原件为 `live-independent-review-r01.json`，SHA-256 `2890a7f3487dfcf1388bfef455229c365dc35ecac8a575a992a49cf99e60fc44`。独立复核确认旧源码等值的时间在本次 Anthropic 修复之前；它不证明新源码已通过。

Haiku 的第三次请求仍属于第一个用户 Task。网关返回 HTTP 200；原 SSE 含空 thinking 文本和非空 signature_delta。本地适配器以 `Anthropic thinking, signatures and non-text/tool blocks are unsupported` 拒绝并断开。因此保存的响应没有 message_stop，不能据此断言上游自己截断。前两次请求结算 1938 和 2277 token；第三次失败，原 126857 token 预留仍保留。工具写入已完成，工具 outcome_unknown 为 false；模型费用仍为 unknown，二者分别记录。整体报告 exit 1，不将部分成功改为全通过。

本地修复先以 `anthropic-thinking-red-r01` 复现：7 项中 4 项失败，包含相同的 thinking/signature 拒绝。修复后 `anthropic-thinking-green-r01` 的 7 项通过。补强反例和取消检查后，`anthropic-providers-all-r02` 的整包 72 项通过、0 项失败，包含全部 7 项新增回归；命令前后源码一致，93 份新增输出均已逐文件核摘要。修复保存原 content 块、签名和加密数据，回传前核正文及工具 ID/名称/参数，拒绝错类型、错索引、缺签名和缺终态，不把思考数据放进最终正文。`anthropic-runtime-green-r01` 使用合成 HTTP 响应、实际 Runtime/Cells/Host 文件能力，完成两个用户回合，并核对会话重开后原消息、事件和签名回传。四份源码和测试的独立只读复核未确认到可复现缺陷；报告为 `anthropic-source-independent-review-r01.json`，SHA-256 `62696099a996cb6832d140294e70e9e645ef0043342dab3cb9bb0b6d1a0f76f6`，只证明所列文件的静态检查，不代替执行结果。这些是本地合成检查，不是再次真实调用。

## r03 完整本地检查与 Haiku 真实重试

新目录 `final-local-suite-r03.json` 已完成全部 16 步，均 exit 0。源码清单 `85cc39d0318873ba1457cca805c7d60704d3c7a995645b5ded3b6101cedc82a6` 包含 542 项、8766125 字节。整仓 263 个顶层目标的最终汇总为 1700 passed、0 failed、7 ignored；独立客户端三个目标另有 13 passed。三条内部子进程汇总未重复计算。定向重跑的 Runtime 4 项、配对 1 项、Responses 18 项，以及整仓内的 Provider 72 项与新增 thinking 7 项，不再叠加成“整仓测试总数”。六个 ignored 项是父测试调用的辅助入口，另一个可选二进制场景未单独运行。

`final-local-suite-r03-verification.json` 已核原命令、日志和 1126 份输出。`final-build-binding-r03.json` 保存两个 ARM64 Debug 程序：Runtime SHA-256 `a382b69ce51a2e6df092a0d055cd56423eb69471e436930ccb334b4940fed1c7`；客户端 `8b15dd876a863341c43248cb9c1d6273280c5d068b4dc66901e99f0ee7454225`。Runtime 关闭默认 feature，未启用 plugin/remote；安装复用同源码产物，不是空 target 重建。SDK/认证 IPC 的 18 次合成请求另有原件，不能算真实模型请求。修复交付入口为 `anthropic-fix-delivery-r01.json`。

用户随后明确要求“重试 haiku”。`magpie-haiku-retry-r01.json` 记录 UTC 2026-10-23 06:11:33 至 06:11:47 的一次独立运行，整体 exit 0。新 Session `3b6e40e3-0a6f-4d1d-96d9-5461bf867122` 完成两个用户回合，各 3 次请求。6 次 POST 均返回 HTTP 200，原流均有合法 `message_stop`。源码、原 runner 和两个程序在运行前后等值。

真实响应第 2、3、4 次各有一个空 thinking 块及非空原签名。三个块在后续请求中原样回传，包含跨用户回合回传；全部先前 Assistant content 共 15 次回传均与原流重建内容在 JSON 值层面一致。签名只作原值核对，不声称密码学验证。原工具块已经带 `caller:{"type":"direct"}`；独立报告中称该字段由 SDK 增加的观察不准确，`haiku-retry-delivery-r01.json` 明确更正，原报告未覆盖。

本次四次实际文件能力调用均先获得精确的一次性审批。114/124 字节文件、后续工具结果和两次最终可见回答中的随机 nonce 均已核对。公开事件连续 216 条，续读 3+213=216，不重不漏；十条保存的恢复命令已核 stdout、receipt 与 stderr，会话重开前后消息和 Task 相等。该行为 SDK，不声称 IPC 进程重启。

六次请求均按原身份保存 Reserved、Started、Finished，合法用量结算为 `1938+2334+2955+3369+3701+4245=18542` token，新会话未结算预留为 0。金额和币种仍未知。原失败会话的数据库和账本摘要未变，126857 token 预留继续保留。主线程核对入口为 `haiku-replay-review-r01.json`，独立复核为 `haiku-retry-independent-review-r01.json`，交付为 `haiku-retry-delivery-r01.json`。本次单项重试已执行完毕，不构成额外真实调用或可复用预算授权。

## 证据适用范围与最终交付

`approved-scope-source-impact-r01.json` 比较 r02/r03 的两份清单及全部归档成员，只发现六个路径变化：两份文档、两份测试、Anthropic 实现及 common 中仅接受 Anthropic 原生元数据的一行分支。Chat、Responses、Gemini 及其 Runtime/传输路径的源码未变。三行 r02 真实记录可按此影响边界继续引用，但实际执行仍为 r02 程序；不声称三行已在 r03 重跑，不声称两个轮次二进制相同。报告中跨命令相加的 1739 不作整仓计数；整仓仍为 1700，客户端另 13，重复目标和脚本自测分开记录。

原目录第五轮 `final-local-suite-r05.json` 的 15 步、1667 passed/0 failed/7 ignored、客户端 13 passed，以及 926 份输出和 5510 份最终证据仍只证明其原源码 `1321e6f02c34b75cd3ff9f3d8c89a7fe70688d48db7c1ccd28b28d980215b994`。新目录 r02 的原 3/4、r03 修复、独立 Haiku 成功和本次范围更新均另立记录，不改写旧 seal。

逐项复核 `approved-scope-requirements-review-r01.json` 确认两项交付和八项条件中，行为条件均已有证据，唯一缺口为四格式能力/限制矩阵和逐行证据链接。补齐四份文档后，`approved-scope-documentation-review-r01.json` 确认该缺口消除；主线程重新核对该报告全部 16 份输入及既有交付中的 1666 份原件摘要，均一致。`approved-scope-documentation-main-check-r01.json` 保存最终状态修改前的文档身份。

最终交付入口为新目录 `approved-scope-final-delivery-r01.json`，现行完整源码清单及归档为 `source-approved-final-r01.json` / `source-approved-final-r01.tar.gz`。与实际执行的 r03 相比，只修改本记录、Provider README、无界面启动说明和路线图四份文档；产品代码、锁文件、构建和测试输入均未改。完整树身份随文档变化另存，不沿用旧全树摘要。本地整项完成后按主线进入 #11，#10 的远端 Issue 继续保持 OPEN。

范围更新已按用户批准写入 GitHub #10 并回读确认。除此之外没有提交、推送、PR、合并或关闭 Issue。官方直连、旧 group 路由、其他模型、真实图片及其他 macOS 架构仍未验证。
