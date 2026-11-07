# Issue #31：Haiku 参数错误反馈修复

记录日期：2026-10-24。用户在查看 [Magpie 真实模型补验](issue-31-magpie-macos.md) 后要求修复 Haiku 参数错误直接终止任务、没有返回模型修正的问题。本记录保留原来的失败事实，单独记录后续代码与验证。工作树为 `/Volumes/ORICO/CAYW/.claude/worktrees/issue31-trace-debug`，分支为 `plugin/trace-debug`；未提交、推送或修改远端 Issue。

## 原始故障

原 `r02` 响应确实包含 `execute_code` 工具调用，结束原因是 `tool_use`。工具参数解析一次后是 JSON 字符串，不是对象。再解析字符串内部内容仍报 `Extra data: line 1 column 9806 (char 9805)`。不能通过自动解包把原参数当作有效调用。

Messages 适配器原先在 `content_block_stop` 校验参数失败后立即返回流错误。Agent 因此结束，没有生成工具错误结果，也没有下一次纠错请求。客户端已经保存的原始响应包含后续用量和 `message_stop`；旧适配器没有消费到这些事件，因此旧 Runtime 仍保留预算。

原请求 SHA-256 为 `35554120ea769a1bf69b8f9d1e7297bf8ca10c12374ac2a4c741195fdcfdc942`，原响应 SHA-256 为 `4bb610ed7bcec822b9302c3fd6cacc2fc858d4836153747d47bf7d4066bf4b3b`。原件位于 `/out/issue31-magpie-live-r02/http/request-1792807349396330000.*`。本次不改写响应、旧失败任务或其 1,100,000 token 预留。

## 修复规则

Messages 响应只有在调用编号非空且唯一、工具已声明、内容块和结束事件完整时，才能进入参数纠错路径。参数语法错误、非对象参数或不满足工具声明时，适配器保留原始参数并标记 `arguments_error`。Agent 生成同一调用编号的“未执行”错误结果，再按现有轮数、取消和 token 预算决定下一轮。

原始参数不会双重解码，不补删括号，不作为工具输入执行。历史发送必须核对原始协议内容与归一化调用一致，并有准确配对的 `is_error=true`、`outcome_unknown=false` 结果。参数有效性必须与 `arguments_error` 是否存在一致，但错误说明文字不参与判断。请求入口统一校验参数，协议内容映射只核对正文、调用编号、工具名称和原始参数，不再次编译参数声明。协议要求对象参数时，仅对发送副本生成明确标记原参数无效的对象；本地原件保持不变。

取消、资源上限、缺结束事件、截断、重复或复用编号、未声明工具继续停止。流式参数只接受开始块中的空对象 `input={}` 作为占位。非对象初始参数可以单独进入错误反馈；若它后面又出现 JSON 片段，则作为参数来源冲突停止，不能丢掉初始值而采用后续片段。独立复核提出了这个组合边界，后续测试明确覆盖 null、空字符串、整数和数组的单独输入及混合输入。Runtime 预算规则和业务未知效果规则不变。本次只修复 Messages 路径，不将结果外推到其他模型协议。

## 验证方式

所有新增测试使用本机 HTTP 服务和合成模型响应，实际运行 Runtime、Cells 和受控 Host。原始响应另由本机服务逐字节返回给修复后的适配器，后续响应使用明确的合成内容。该复验不连接 Magpie，不产生真实模型生成请求，也不打开或更改原 Runtime 数据库。

可复跑的 Runtime 回归入口：

```sh
/Users/chengwen/.cargo/bin/cargo +1.95.0 test \
  --manifest-path /Volumes/ORICO/CAYW/.claude/worktrees/issue31-trace-debug/Cargo.toml \
  -p cayw-runtime --test tool_argument_recovery --locked --offline
```

测试覆盖：

- 与原故障相同的外层字符串、内部多余括号，经错误反馈后得到新的有效调用。只有有效调用创建执行，实际文件写入一次。
- 重开同一持久会话后可以发送后续用户消息，旧错误调用和结果仍准确配对。
- 持续错误达到现有 `max_turns` 时停止，工具执行次数为零。
- token 预算耗尽后不再发出模型请求。
- 缺少 `message_stop` 时不产生工具结果，保留模型请求预算。

本机证据目录为 `/out/issue31-haiku-recovery-r01`。修复前复现日志 `before-test.log` 显示任务状态为 `Failed`，与测试要求的 `Completed` 不符。修复后 `providers-tests-r01.log` 记录提供者包 80 项测试全部通过；独立复核补充参数来源边界后，`providers-final.log` 记录最终 81 项全部通过，提供者严格 Clippy、全工作区格式及补丁检查也再次通过。`runtime-recovery-r01.log` 记录以下 4 项 Runtime 测试全部通过。

| 场景 | 实际结果 | 原始记录 |
| --- | --- | --- |
| 坏参数 → 错误反馈 → 新的有效调用 → 正文结束 | 任务完成；3 次本机请求结算 45 token；仅有效调用产生 1 次执行、1 次文件写入 | `runtime/recovery-first-round.json`、`runtime/recovery-requests.json` |
| 重开同一持久会话并发送后续消息 | 第二个任务完成；累计 4 次本机请求、60 token；仍只有 1 次执行 | `runtime/recovery-reopened.json` |
| 连续坏参数达到 `max_turns=2` | 任务按现有 Runtime 规则记为 `Cancelled`；2 次请求、2 个已知错误结果、0 次工具执行 | `runtime/turn-limit.json` |
| 总 token 预算为 15 | 第一轮结算 15 token 后停止；仅 1 个请求真正开始联网；后续被拒请求也留在账本；0 次执行 | `runtime/token-limit.json` |
| 响应缺少 `message_stop` | 任务失败；0 次执行、无工具结果；合成测试的 128,000 token 预留保持未知 | `runtime/incomplete-response.json` |

提供者测试还覆盖参数深度、节点数和标量长度超限，以及解析中取消；这些情况保持响应失败。首次修复还要求错误说明逐字一致，并拒绝空说明；后续精简已移除这一过严要求。当前仅在错误标记是否存在与参数有效性矛盾时拒绝，即使移除协议元数据也不能跳过该检查。签名块和调用顺序经过序列化及下一轮发送后保持一致。

## 原始响应离线复验

仓外复验程序位于 `/out/issue31-haiku-recovery-instrument-r01`。该程序的第三方依赖全部匹配产品锁定文件，共 222 个依赖身份，未升级产品依赖。首次构建因复制的锁定文件尚未包含仓外测试程序而被 `--locked` 拒绝，记录保留在 `original-response-replay.log`；离线整理并核对后，以 `--locked --offline` 构建运行成功，记录为 `original-response-replay-r02.log`。

复跑时，将最后一个参数改为不存在的新输出目录，避免覆盖原证据：

```sh
/Users/chengwen/.cargo/bin/cargo +1.95.0 run \
  --manifest-path /out/issue31-haiku-recovery-instrument-r01/Cargo.toml \
  --locked --offline -- \
  /out/issue31-magpie-live-r02/http/request-1792807349396330000.request.json \
  /out/issue31-magpie-live-r02/http/request-1792807349396330000.response.sse \
  /out/issue31-haiku-local-replay-new
```

原 `r02` SSE 字节由本机服务原样返回。修复后的适配器产生完整 `ToolUse`，保留原调用编号、`execute_code` 名称、字符串参数和错误标记，读到上游报告的 11,793 token。序列化后追加同一调用编号的错误结果，下一次本机请求成功发送。原签名块保持不变，非对象参数只在发送副本中投影。程序不执行任何工具，第二次响应明确是合成正文。自动纠错并实际写文件由前述 Runtime 测试另行证明。

复验输出为 `original-response-replay/summary.json`、`parsed-message.json`、`local-events.json` 和 `feedback-request.json`。`evidence-assertions.json` 独立核对 1 个原签名块、参数原值和错误结果；原请求与响应的 SHA-256 仍与本文记录一致。`preservation-check.json` 确认旧 `r02` 模型请求账本、`r03` 最终预算及原 #31 的 27 个源码/文档文件保持原样；另外 2 份既有文档仅补充本次规则或后续记录入口。旧失败没有改成成功，原预留没有释放。

## 工作区检查与源码绑定

本次重新执行了完整默认工作区的格式检查、编译、测试和所有目标的严格 Clippy 检查；五个检查命令的退出码均为 0，详细命令与耗时见 `workspace-results.json`。工作区测试为 1,763 项通过、0 项失败、7 项忽略。独立复核后仅新增 1 项提供者边界测试并补文档，没有再改生产逻辑；最终提供者包的 81 项通过单独记录，不冒充再次运行全工作区。原始日志分别为 `workspace-fmt.log`、`workspace-check.log`、`workspace-tests.log`、`workspace-clippy.log`、`diff-check.log`，测试汇总为 `workspace-test-summary.json`。这次重新运行与旧的 1,751 项通过记录分开保存。Cargo 仍报告既有示例 `offline` 输出文件同名提示；测试通过，严格 Clippy 无警告。

初次清单引用共享 `target` 路径，之后只读复查发现其中 2 个构建输出的字节改变，记录为 `mutable-build-observation.json`。该 `source-manifest.json` 保留为中间失败证据，不作为最终交付清单。复制到 `/out/issue31-haiku-recovery-build-r02` 后，提供者取消测试、签名历史测试与 Runtime 纠错测试重新通过；最终 HTTP 测试固定副本在 `/out/issue31-haiku-recovery-build-r03`，45 项通过。

`r02` 中同名原响应复验程序虽然退出码为 0，输出实际属于独立复核的 null/空串组合复现，没有生成要求的原响应结果目录。因此 `sealed-validation-results.json` 中首条记录无效，不用作原响应复验成功证据。同名仓外包共用了构建目标目录；最终改用 `/out/issue31-haiku-recovery-build-r04/target` 独立构建，并检查固定副本实际生成的目录和 `summary.json`。该次原响应复验通过，收据为 `isolated-replay-result.json`，输出为 `sealed-original-response-replay`。最终清单只引用有效固定副本；中间日志保留。

包含修复的 Runtime 程序也已按 `cargo +1.95.0 build -p cayw-runtime --bin cayw --locked --offline` 构建并执行 `--help` 成功。固定副本及收据位于 `/out/issue31-haiku-recovery-build-r01`，二进制 SHA-256 为 `2401323f275ce49c2a2d2104e201a1429168ed33e4b77daa42f8b48e5e9da5bb`。实际纠错执行由上表 Runtime 测试证明。

`source-manifest-final.json` 记录首次修复封存时的源码、验证程序、固定测试二进制、日志和结果文件 SHA-256，并引用旧 `/out/issue31-live-delivery/source-manifest.json`。旧真实封存保持原样。2026-10-24 后续精简已改变当前源码，这份旧清单及其 Runtime 二进制仅证明精简前版本，不能作为当前源码的检查结果。后续精简的命令、结果与源码绑定单独记录在 `/out/issue31-haiku-simplify-r01`。

本记录仅面向 macOS 27.0.1 ARM64、Rust 1.95.0 和默认 feature。其他 macOS 架构、全部 feature、官方直连以及修复后的 Haiku 线上自行纠错均未验证。本次新增真实生成请求为 0。

## 2026-10-24 后续精简

用户要求移除过度防御并重新测试。本轮取消 `arguments_error` 说明文字的全等比较，仅检查参数有效性与错误标记是否存在一致；移除发送副本及协议元数据映射中的重复参数校验；删除局部重复的 `outcome_unknown` 判断，保留函数入口的统一阻断。原始协议内容仍按正文、调用编号、工具名称和参数逐项核对。

共享 JSON 解析器改为返回明确的 `ParseError` 类别。Messages 只把 `InvalidSyntax` 和 `TrailingData` 作为可反馈的参数错误，取消和资源超限仍停止。OpenAI 的三处改动仅把明确错误转换成现有流错误文字。没有保留另一份解析器或旧格式分支。

新增 `anthropic_argument_error_descriptions_do_not_change_history_validity` 覆盖语法错误和声明不符两种参数，并分别带有或移除协议元数据。空说明、详细说明和中文说明共 12 组下一轮请求均需成功发送，原始参数和元数据保持原样。既有联网前拒绝测试继续覆盖错误标记缺失、有效参数带错误标记、未配对或成功结果、未知效果以及原始调用编号、名称、参数不一致。

本轮日志单独保存在 `/out/issue31-haiku-simplify-r01`。新增测试在精简前实际失败，`before-description.log` 记录收到 `Error`，而预期为 `Stop`；该失败记录保留。初次 JSON/提供者回归为 146 项通过，Agent/Runtime 回归为 410 项通过，均无失败或忽略。最后按最终生产源码重新运行 `cargo +1.95.0 test -p cayw-ai -p cayw-providers -p cayw-agent -p cayw-runtime --locked --offline`，97 个测试组共 556 项通过、0 失败、0 忽略，包含文档测试。这是四个相关包的回归结果，不是重新运行全工作区测试。

全工作区格式检查、全部目标的严格 Clippy（`--workspace --all-targets --locked --offline -- -D warnings`）以及 `git diff --check` 均通过。命令、耗时和日志路径见 `final-tests.json`、`test-summary.json`、`workspace-fmt.json`、`workspace-clippy.json`、`diff-check.json`；当前源码快照与检查结果由本轮 `source-manifest.json` 单独绑定。全部验证在 macOS ARM64、本机合成响应和默认 feature 下执行。本轮新增真实模型请求为 0，旧失败任务和预算记录未修改。
