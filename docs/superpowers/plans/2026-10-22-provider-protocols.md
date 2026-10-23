# 有限 Provider 协议实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. 已有本地实施授权持续有效；不重复设计审批，不自动提交，不重启 workflow。

**Goal:** 完成 Issue #10 限定的四行服务适配、本地 HTTP 与公开 Runtime 验证，并单独保留需要当次授权的真实服务验收门槛。

**Architecture:** 复用 `cayw_ai::Provider`、现有 OpenAI/Anthropic/Gemini 适配器和 Runtime → Agent → execute_code → Host 路径。Magpie 共用 Chat Completions 适配器，但保存独立服务配置、请求路由和实际响应模型。协议事实、用量状态和持久请求预算分别记录，不用工具副作用未知状态表示未知费用。

**Tech Stack:** Rust 1.95.0、Tokio、Reqwest 0.13、Serde、SQLite、现有 Luau/Cells/Host。

## Global Constraints

- 只在 `/Volumes/ORICO/cayw-issue10-integration-20261022-a` 开发；I6/I7/I8/I9 交付保持封存。
- 所有代码搜索用主仓 CodeGraph 定位，再读取 I10 实际文件；不把主仓旧源码覆盖到 I10。
- 本机 macOS 27.0.1 ARM64；其他架构保持未验证。
- 真实服务调用、凭据读取和付费测试本项尚未授权；#6 的额度不能转用。
- 不 commit、push、创建 PR、merge、关闭 Issue 或修改远端评论。
- 不扩展 Responses、Interactions、UI、plugin、远程 Device/Server、Linux/Windows。
- 不自动重试、跳转、换服务、换模型或重放未知效果；不放宽参数、权限、调用配对和预算。
- 命令经仓外 `validate.py` 保存唯一标签的源码清单、原始输出和退出码，保留所有失败。
- 本计划的勾选只代表明确列出的本地结果；四条真实服务行全部完成前不结项、不进入 #11。

## Task 1: 拒绝歧义工具参数

**Files:** `crates/cayw-ai/src/validation.rs`、`src/providers/openai.rs`、`tests/contracts.rs`、`tests/openai.rs`。

**Interfaces:** 保持 `parse_tool_arguments(&str) -> Result<Value, ValidationError>` 和显式 `unsigned_tool_integer`。成功工具终态必须有完整且无重复字段的 JSON 对象；Length 保留截断信息并禁止执行。

- [x] 通过真实 loopback HTTP 复现重复字段被后值覆盖且返回 Done/ToolUse；原始日志 `duplicate-tool-fields-red-r01.log`。
- [x] 通过公开解析接口复现重复字段接受；日志 `duplicate-object-parser-red-r01.log`。
- [x] 使用 Serde 递归解析拒绝普通、转义同名和嵌套字段，不改变数值或空对象规则；非 Length 参数失败终止流。
- [x] `cargo test -p cayw-ai --locked --offline`，33 passed，原始日志 `duplicate-tool-fields-green-r01.log`。

## Task 2: 用量存在性与有效性

**Files:** `crates/cayw-ai/src/types.rs`、`src/providers/openai.rs`、`crates/cayw-providers/src/{anthropic,gemini,common}.rs`，对应 HTTP 测试。

**Interfaces:** `AssistantMessage.usage` 保存已有计数，`usage_status` 区分未报告、部分报告、完整字段与无效报告。部分字段跨帧覆盖累计值；不逐帧相加，不用默认零补造报告。

- [x] 本地 HTTP 矩阵覆盖 Anthropic、Gemini SSE/JSON 的缺失、null、空、只输入、只输出、全零与完整计数；既有增量测试覆盖跨帧。`usage-presence-red-r02.log` 复现，`usage-conflicts-green-r03.log` 核整个 ai/providers 共 74 passed。Anthropic 缺必填报告仍失败，用量状态保持 Unknown。
- [x] OpenAI total、缓存/推理子计数，Gemini total/工具提示计数、累计下降和溢出均有 HTTP 回归。三个 `usage-conflicts-*-red-r01.log` / `usage-tool-prompt-red-r01.log` 保留原反例；`usage-conflicts-green-r03.log` 全部通过。原字段进入 `usage_fields`，无效状态为 Invalid；持久预算仍属 Task 5，尚未完成。
- [x] 取消、断流和解析失败保留已知用量；覆盖已解析 usage-only 帧但尚无文本更新时取消的 Agent 路径。定向 HTTP 用量原件纳入 `model-ledger-boundary-final-r02` 的 10 项；最终同树仍由 Task 6 重验。

## Task 3: 网络结束、错误、期限和能力边界

**Files:** `crates/cayw-ai/src/providers/openai.rs`、`providers/sse.rs`、`crates/cayw-providers/src/transport.rs`、`src/lib.rs`，对应 HTTP 测试。

- [ ] 服务端保持 HTTP body 未结束，消费者保留 stream；验证成功、解析失败和取消终态之前连接已释放。
- [ ] 同一协议终止后的数据不因网络分块方式改变结果；缺终止、坏 JSON、错误参数、截断和非法顺序不产生工具成功。
- [x] 302/429/503 保留单请求证据，见 `network-lifecycle-green-r02`；`model-ledger-boundary-final-r02` 的 HTTP 用量测试覆盖主动取消，`provider-deadline-local-r01` 两项覆盖等待响应头和已收到用量后的 Runtime 期限耗尽。分别核 Failed/Cancelled、预留及重开不重发；最终同树仍待 Task 6。
- [ ] 合成敏感标记不得通过远端 error 对象进入错误历史。
- [ ] 多调用交错参数、高序号先到、逆序结果在实际 HTTP 往返中保持原 ID 和顺序。
- [ ] 不支持媒体/思考/签名等能力明确失败，或仅在显式配置下按可见规则降级。

## Task 4: 一个明确的服务配置入口

**Files:** `crates/cayw-runtime/src/main.rs`、`src/launch.rs`、新 `src/provider.rs`、`Cargo.toml`、启动测试、`docs/headless-chat.md`。

**Interfaces:** 宿主明确选 service/API/model/base URL/输出上限和凭据环境变量名称；只在实际启动选中服务时读取其凭据。`Runtime::open(..., Arc<dyn Provider>, Model, RuntimeConfig)` 保持通用注入入口。

- [ ] 未知或矛盾配置在联网前失败；迁移现有 CAYW_PROVIDER_* 约定到一个规范配置路径，不维护双轨产品契约。
- [ ] OpenAI/Magpie 分别具名，Anthropic 与 Gemini 显式选择适配器，Gemini 模式不隐式切换。
- [ ] 使用仅本机固定假密钥的进程测试验证实际 endpoint、模型和配置错误；不读取用户模型凭据。

## Task 5: Runtime 请求记录与本地完整工具任务

**Files:** `crates/cayw-runtime/src/execution.rs`、`src/lib.rs`、新请求记账模块，Provider 传输观察入口，`tests/provider_protocols.rs`、`tests/provider_usage.rs`。

**Interfaces:** 每次模型请求绑定 Session/Task/完整 Attempt 和轮次；持久保存准入预留、用量可用性、金额是否已知、终态及未结算部分。实际 HTTP 原字段和内部事件有可复核关联，认证头和凭据不入记录。

- [x] 五种配置通过公开 Runtime 完成同一 Session 两个用户回合：OpenAI、Magpie、Anthropic、Gemini SSE/JSON。每轮 exec → yield → wait → read，真实 Host 合计两写三读；核磁盘、原参数/ID/结果、历史、游标、Completed Task 和后续 HTTP 回答。`runtime-protocol-r03.log` 4 tests passed，运行中源码未变化；当前后续产品改动仍须最终重验。
- [ ] 保存真实 HTTP 请求/响应、协议结束/用量事实与内部事件；固定数据明确标记，不冒充模型服务。
- [ ] 请求开始、流中、工具等待期间取消；核时间预算、失败、修复请求和重开后仍保留未知用量/金额与原预留。
- [ ] 迟到响应不能复活旧任务，费用未知不能清除工具 outcome_unknown，也不能自动重放。

## Task 6: 独立复核、完整检查和真实服务门槛

**Files:** 新 `docs/validation/issue-10-provider-protocols-macos.md`、服务/API/精确模型能力矩阵、仓外验收原件。只有整项证据完成后才能更新路线图完成状态。

- [ ] 独立复核重点解析、错误和取消反例，主线程核源码差异和原始日志。
- [x] 第四轮 `final-local-suite-r04.json` 已依次完成格式、workspace check/test/Clippy、offline 示例、独立客户端、五协议、两个独立安装和实际 SDK/IPC 组合，共 15 步；source `720109b65d681479f9e8c2129dd0fe3c8786b1a366d78df54c0b60cc759a9118`。主测试结果为 1665 passed、0 failed、7 ignored，独立客户端 13 passed；核对器已复算 932 份输出。后续测试和说明修改另绑新完整轮次，不沿用旧清单。
- [ ] 真实运行前准备可审查的精确配置、输入、权限、次数/token/金额上限、未知用量预留和脱敏规则。
- [ ] OpenAI 官方真实任务，具体模型须当次授权。
- [ ] Anthropic 官方真实任务，具体模型须当次授权。
- [ ] Gemini 官方真实任务，具体模型须当次授权。
- [ ] Magpie `group/auto-deepseek-v4-1-flash` 真实任务，保存实际响应模型，须当次授权。
- [ ] 全部完成后保存最终源码归档、交付文件并回读；不自动提交或同步 GitHub。

## 本地追加验证

- [x] 新 Session 两用户回合只发送一份有效 Runtime 说明；旧匿名完整说明在 Agent 的历史副本归入同名 section，原数据库 messages/events 与宿主规则保留。`instruction-legacy-red-r01` 实际 3!=1，`instruction-legacy-final-test-r01` 两项及 `instruction-legacy-clippy-r02` 通过；未测 Fork。
- [x] 脚本终态查询保存原 PID/启动时间，父进程变化后仍核原 sleep。`config-evidence-original-pid-red-r01` 两个实际 OS 反例失败，`config-evidence-original-pid-green-r02` 七项通过；完整安装程序链待统一运行，秒级启动身份不证明 PID 复用。
- [x] `provider-deadline-local-r01` 两项公开 Runtime + OpenAI loopback HTTP 用例覆盖等待头部和用量已持久化后的期限耗尽。核时间失败、非用户取消、1000 token 原预留、已知用量、金额 Unknown、重开和新任务不重发。产品直接通过，属于补覆盖；主线程和独立只读复核已核源码与原件，最终同树结果另核。
- [x] 第四轮之后已补 Anthropic stop_sequence/max_tokens/model_context_window_exceeded，以及 Gemini SSE/JSON 的文本 MAX_TOKENS 和含完整函数调用拒绝，共两个参数化测试、七个场景。`native-stop-http-r01` 39 项通过；`native-stop-format-check-r02` 通过，均绑定稳定源码 ebde2948ed98c15fff0e015a1d580e18bc83500cbb0ddbec6c0af171a79d916d。实现分支未改，属于补覆盖；README 已明确任何函数调用均拒绝。新增检查的独立复核另以具名报告为准。
- [x] 第四轮五协议与安装组合的原件独立复核均完成，分别见 `r04-protocol-independent-review-r01.json`、`r04-installed-independent-review-r01.json`。流取消/工具取消原 prompt 输出分别完整到 cursor24/53，已与本次合成数据库及18个实际HTTP逐项核，未发现本次窗口的记录缺失或重发。脚本自动断言范围有限，与本次原件事实分开记录。
- [ ] 源码停写后运行 `run_final_local.py --attempt r05` 全15步；通过后核新日志、命令前后源码及输出摘要，再核新五配置和实际安装组合原件。第四轮事实不代替第五轮；独立复核完成后才生成第五轮本地归档和seal，四行真实服务仍单列未完成。

## 已知资料与边界

权威原文及七条件位于 `/Volumes/ORICO/cayw-issue10-evidence-20261022-a/{issue10-contract,acceptance-scope}.json`。协议资料和官方 Gemini v1beta discovery 的原件、摘要及 API 范围位于同目录的 `protocol-references-r01.json`、`protocol-fields-r01.json`、`google-generate-content-schema-r01.json`。资料只定义参考语义，不证明产品通过。
