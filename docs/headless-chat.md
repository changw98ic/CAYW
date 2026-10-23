# macOS 无界面对话

CAYW 可以安装为独立的 `cayw` Runtime 和 `cayw-client` Rust 客户端。客户端通过公开 Rust SDK 或认证本机 IPC 管理持久会话。模型通过 Luau 调用实际 Rust 能力；宿主逐次检查权限、预算、取消和结果。

当前启动入口通过一份显式配置选择模型服务与 API。Magpie 的四种格式已分别在 macOS ARM64 完成两个真实用户回合：Chat Completions / `opencode-go/deepseek-v4.1-flash`、Responses / `codex/gpt-6-luna`、Messages / `factory/claude-haiku-5-5`、Gemini 流式 / `antigravity/gemini-3.8-flash`。Chat、Responses 和 Gemini 的成功记录来自 r02，Haiku 来自修复后的独立 r03 重试；每行只证明自己的源码、配置与实际传输，见[四格式矩阵及证据](validation/issue-10-provider-protocols-macos.md)。官方服务直连和旧 group 路由未纳入本次验收。此前 #6 的[首个真实闭环记录](validation/issue-6-headless-chat-macos.md)仍只证明当时的源码和场景。

## 安装

需要 macOS、Rust 1.95+ 和仓库源码。以下命令在仓库根目录执行，把两个程序装入同一指定目录：

```sh
export CAYW_INSTALL_ROOT="$HOME/.local/cayw-headless"
cargo +1.95.0 install --path crates/cayw-runtime --root "$CAYW_INSTALL_ROOT" --bin cayw --no-default-features --locked
cargo +1.95.0 install --path examples/headless-client --root "$CAYW_INSTALL_ROOT" --bin cayw-client --locked
"$CAYW_INSTALL_ROOT/bin/cayw" --help
"$CAYW_INSTALL_ROOT/bin/cayw-client" --help
```

Runtime 默认 feature 为空。此安装不启用 UI、plugin 或 remote，也不需要 VerifyTool 服务。客户端是独立 Cargo workspace，单独保留锁文件。`--root` 指定安装目录，程序位于它的 `bin/` 下。验证记录使用仓外独立安装目录和 `--debug --offline`；上面的日常安装采用 Cargo 默认的 release 构建。

## 准备工作区和可信配置

工作区目录和数据库父目录必须已存在。以下例子使用一个专用目录：

```sh
export CAYW_CHAT_ROOT="$HOME/cayw-headless-example"
mkdir -p "$CAYW_CHAT_ROOT/workspace" "$CAYW_CHAT_ROOT/state"
cp examples/headless-client/host.example.json "$CAYW_CHAT_ROOT/host.json"
```

[完整配置示例](../examples/headless-client/host.example.json)设定每个任务最多 8 次模型循环、45 秒总时间、16 次能力调用，以及 256 KiB 能力参数和结果封装的累计 JSON 字节额度。8192 token 是持久会话预算树的额度；新任务、子会话和 Fork 共用这笔额度。Runtime 在模型请求开始前预留全部剩余额度，只有完整成功且用量有效时才按实际报告结算。宿主等待及单次 Host 调用各为 40 秒。示例中的 `hold` 是固定的 `/bin/sleep 30` 测试程序，用于在执行期间取消。注册这个程序不会启动它，也不会授予执行权限。

配置的顶层字段如下。宿主字段省略时采用 Runtime 默认值；真实模型启动必须提供完整 `model` 对象。未知字段、错误类型、零预算、无效程序或无效权限会被拒绝。

| 字段 | 含义 |
| --- | --- |
| `model` | 单一模型服务配置；包含服务、API、模型、端点、版本、输出限制和凭据环境变量名称 |
| `budgets.max_turns` | 一个任务内的模型循环次数；不等于用户回合数 |
| `budgets.max_total_tokens` | 持久会话预算树的 token 额度；请求前预留剩余额度，成功后按有效报告结算，未结算预留继续占用 |
| `budgets.max_run_time_ms` | 一个任务的总执行时间 |
| `budgets.max_capability_calls` | 能力调用次数上限 |
| `budgets.max_capability_bytes` | 能力参数和结果封装的累计 JSON 字节额度 |
| `budgets.host_wait_ms` | 一次宿主等待上限，包括审批；必须为正且不超过一小时 |
| `budgets.host_call_timeout_ms` | 一次 Host 调用上限 |
| `profiles` | 宿主固定的程序、参数、环境、时间和输出限制；当前 JSON 入口只支持 `pipes` |
| `permissions` | 宿主预先允许的精确能力名和完整 JSON 参数；空数组表示调用需要审批 |
| `requests` | 可选的任务能力申请策略和限额；列出精确可申请操作，不直接批准申请 |

`profiles` 的 `kind` 为 `build`、`test`、`check`、`lint` 或 `format`。程序、参数和环境由宿主提供，模型不能用工具参数替换。配置文件最多 1 MiB。加载配置不发模型请求，不启动程序；启动 Runtime 时会按同一配置检查实际工作区。

例如，预先允许一次精确范围的读取可以写成：

```json
{"capability":"fs.read","arguments":{"path":"source.txt"}}
```

把它放进 `permissions` 即由可信宿主授予该精确范围的长期配置权限。需要逐次确认时保持 `permissions: []`。每次 `cayw.call` 仍独立经过实际 Host 路径。脚本或模型文字不能替代宿主授权。

需要模型先申请，再由可信宿主确认的项目测试，可在同一配置增加以下内容。`project-test` 必须已经是 `profiles` 中的固定测试程序：

```json
{
  "requests": {
    "permission_policy_id": "local-project-tests-v1",
    "operations": [{
      "id": "project-test",
      "capability": "test.run",
      "arguments": {"profile": "project-test"},
      "permission_binding": "fixed-project-test"
    }],
    "limits": {
      "repeated_valid_threshold": 3,
      "request_lifetime_ms": 90000,
      "evaluation_timeout_ms": 15000,
      "max_grant_lifetime_ms": 45000
    }
  }
}
```

`requests` 是额外的任务授权检查。普通 `permissions` 或逐次审批仍需通过。`operations` 中的参数必须完整匹配；改变 profile 或增加参数不会退回普通权限路径。`limits` 省略字段采用同一个 `RequestConfig` 默认值。次数只决定何时可评估，不决定批准。策略和限额随申请库持久保存，同一数据库重开时必须匹配。

宿主通过 SDK 的 `enqueue_with_intent` 或认证 IPC 的 `task.enqueue_with_intent` 记录用户明确的项目测试需求。模型只提交用途、能力缺口、操作编号和参数。可信控制端读取申请后，通过 SDK 的评估票据作决定，或调用 IPC 的 `request.decide`。批准、拒绝、评估不可用、到期、撤销和任务结束各有可查询的状态与原因。IPC 的参数和错误码见 [SDK / IPC 契约](issue1-sdk-ipc-contract.md)。当前 `cayw-client` CLI 没有这些申请控制命令；Rust 宿主或 `cayw_ipc::Client` 直接使用公开接口。

仓内 `task_project_test` 示例演示固定本地 Provider 如何申请运行现有项目测试。它接受项目根目录、绑定 Cargo 编译记录的程序收据，以及全新结果目录。它分别运行 SDK 和认证 IPC，保存配置、原申请、批准记录、Core/Host 事件、实际程序输出和重开结果。编译收据必须绑定实际测试程序的摘要；固定输出本身不能证明测试程序来自当前源码。该示例使用合成模型回复与宿主裁决，实际运行 Host/Device，不属于真实模型证据。完整复跑命令随 #8 验证记录交付。

`max_total_tokens` 由持久模型请求账本执行。每次请求先写入原会话、任务、Attempt 和模型身份，再预留同一预算树的全部剩余额度。同一预算树同时只允许一个未结算模型请求。请求的输出上限会按剩余额度收紧；当前没有可信的输入 token 上界，因此 `input_token_bound` 为 `None`。服务报告的实际输入仍可能使总用量超过预留，此时停止并保留预留。这个设置不构成模型服务端的硬费用上限。

缺失用量记为 `unknown`，部分字段记为 `partial`，输入和输出都明确报告才记为 `reported`。明确的零与缺失不同。错类型、累计下降、计数矛盾或溢出记为 `invalid`。只有成功的 `Done`、合法结束原因、未取消、完整且有效的报告、用量未超预留，并且终态成功写入后，才能按报告结算并释放余量。失败、取消、流被丢弃、缺终态或记账失败都保留预留。新任务、子会话、Fork、重开数据库或提高配置额度都不能清除历史欠账。

`Runtime::model_requests` 可读取请求身份、预留、每次用量观察和终态。金额没有可信价格来源，保持 `Unknown`，金额和币种均为空；`reported` 不代表账单已结清。模型费用未知与工具的 `outcome_unknown` 分别记录，模型结算不能清除工具未知。真实验收仍需当次授权、单独限定请求数并核对服务账单。

## 连接模型服务和本机 IPC

`cayw serve` 只通过 `--config` 的一个 `model` 对象选择模型服务。将以下内容加入前述宿主配置；配置只保存凭据的环境变量名称，不保存凭据值：

```json
{
  "model": {
    "service": "magpie",
    "api": "chat_completions",
    "id": "opencode-go/deepseek-v4.1-flash",
    "base_url": "http://127.0.0.1:3425/v1",
    "api_version": "v1",
    "max_output_tokens": 4096,
    "api_key_env": "MAGPIE_API_KEY"
  }
}
```

所有模型字段都必填。`max_output_tokens` 是正整数，限制单次模型输出，不是任务总费用上限。服务与 API 组合固定如下。这张表说明配置选择；实际通过的服务、模型、传输和源码见四格式矩阵，官方直连仍未验证：

| `service` | `api` | `api_version` | 非本地 `base_url` |
| --- | --- | --- | --- |
| `openai` | `chat_completions` | `v1` | `https://api.openai.com/v1` |
| `openai` | `responses` | `v1` | `https://api.openai.com/v1` |
| `anthropic` | `messages` | `2023-06-01` | `https://api.anthropic.com/v1` |
| `gemini` | `stream_generate_content` | `v1beta` | `https://generativelanguage.googleapis.com/v1beta` |
| `gemini` | `generate_content` | `v1beta` | `https://generativelanguage.googleapis.com/v1beta` |
| `magpie` | `chat_completions` | `v1` | 宿主明确指定的 HTTPS 服务地址，路径为 `/v1` |
| `magpie` | `responses` | `v1` | 同上，路径为 `/v1` |
| `magpie` | `messages` | `2023-06-01` | 同上，路径为 `/v1` |
| `magpie` | `stream_generate_content` | `v1beta` | 同上，路径为 `/v1beta` |
| `magpie` | `generate_content` | `v1beta` | 同上，路径为 `/v1beta`；仅有本地固定数据覆盖 |

Anthropic 的版本通过 `anthropic-version` 请求头发送，其余版本体现在 URL 路径。当前入口只接受表中版本；其他版本不会被静默替换。模型名称必须明确；Gemini 接受模型名称或 `models/名称`。本地固定响应测试可使用数字回环地址的 HTTP 或 HTTPS，路径仍需为相应 API 根路径。官方服务配置不能改成其他网关或自定义路径；回环测试不能当作官方服务证据。URL 禁止携带用户名、密码、查询参数或片段。未知字段、矛盾组合和无效输出限制会在读取模型凭据及联网前失败。

加载配置不读取模型凭据。启动真实适配器时，只读取 `api_key_env` 指定的一项模型凭据；缺失、空值或无效请求头值会导致启动失败。宿主仍单独读取 `--token-env` 指定的 IPC 令牌。`--offline` 要求省略 `model`，同时提供两者会被拒绝。

旧的 `CAYW_PROVIDER_BASE_URL`、`CAYW_PROVIDER_API_KEY`、`CAYW_MODEL` 不再选择 `cayw serve` 的模型，也不会覆盖 `model`。迁移时把端点和模型移入该对象，将凭据移到 `api_key_env` 指定的环境变量，再删除旧的三项启动设置。Rust 宿主可调用 `HeadlessConfig::model()` 和 `ProviderConfig::build()`，将得到的适配器和模型传给原有 `Runtime::open`；宿主也可用 `build_with_environment` 提供自己的凭据读取函数。通用 `Runtime::open` 的注入接口保持不变。

真实模型请求需要本任务的明确授权与预算。以下命令只展示已授权后使用的入口。请由当前进程环境提供凭据，不把凭据放入配置文件、命令参数或输出：

```sh
: "${MAGPIE_API_KEY:?请先在当前环境提供已授权的模型凭据}"
: "${CAYW_IPC_TOKEN:?请先在当前环境提供至少16字节的IPC令牌}"
"$CAYW_INSTALL_ROOT/bin/cayw" serve \
  --db "$CAYW_CHAT_ROOT/state/ipc.sqlite" \
  --workspace "$CAYW_CHAT_ROOT/workspace" \
  --listen 127.0.0.1:4000 --token-env CAYW_IPC_TOKEN \
  --config "$CAYW_CHAT_ROOT/host.json"
```

服务只监听本机回环地址。连接仍需 `hello` 认证。启动输出的 `model_configuration` 包含选中的服务、API、模型、端点、版本、输出限制和凭据变量名称，不包含凭据值。预算摘要和进程配置名称仍单独输出。`--listen 127.0.0.1:0` 可由系统分配端口，再使用启动输出的实际地址。

启动宿主、创建或读取会话不发模型请求。`prompt` 才启动任务。以下 IPC 客户端只需要地址和令牌；模型配置和凭据由 Runtime 进程持有。独立 `examples/headless-client` 的 SDK 启动器使用相同的 `model` 配置。每次 SDK 调用在客户端进程中打开 Runtime，因此只读命令也要求提供该配置与选中凭据。

在另一终端定义一个方便重复调用的函数；该终端需有相同的安装目录和 IPC 令牌：

```sh
cayw_chat() {
  "$CAYW_INSTALL_ROOT/bin/cayw-client" --mode ipc \
    --address 127.0.0.1:4000 --token-env CAYW_IPC_TOKEN "$@"
}
cayw_chat create --label 'Two user turns'
```

从 `result` 行取得实际会话 `id`，设置 `CAYW_SESSION_ID`。客户端输出为逐行 JSON；每行的 `type` 区分事件、任务、结果和错误。完整字段见[客户端说明](../examples/headless-client/README.md)。

## 两个用户回合与审批

先在工作区准备 `source.txt`，内容包含本次随机产生的标记。该标记不提前放进用户提示。第一回合示例：

```sh
cayw_chat prompt --session "$CAYW_SESSION_ID" \
  --text '通过 Luau 读取 source.txt，将完整内容写入 round1.txt，再根据工具结果报告其中的标记。' \
  --cursor-file "$CAYW_CHAT_ROOT/state/ipc-events.cursor.json"
```

客户端会输出原任务和事件。出现 `approval_requested` 后，核对其能力、完整参数和原任务。用另一终端的同一函数读取并决定：

```sh
cayw_chat get --session "$CAYW_SESSION_ID"
cayw_chat approve --session "$CAYW_SESSION_ID" \
  --approval "$CAYW_APPROVAL_ID" --decision allow_once
```

`CAYW_APPROVAL_ID` 必须来自实际待审批记录。`allow_once` 只允许该次精确操作。`allow_task` 在原任务内复用相同精确范围；不会授权其他参数或下一任务。`deny` 拒绝调用。模型说“已批准”不会产生授权。

第一回合结束后核对 `round1.txt` 的实际字节和模型回答。第二回合继续使用相同会话与游标文件：

```sh
cayw_chat prompt --session "$CAYW_SESSION_ID" \
  --text '继续上一回合：读取 round1.txt，把 continued: 与原文件完整字节直接拼接后写入 round2.txt。冒号后不加空格、换行或其他字节。报告上一回合观察到的同一标记。' \
  --cursor-file "$CAYW_CHAT_ROOT/state/ipc-events.cursor.json"
```

同样逐次核对审批。最后检查两个原任务均完成、两个实际文件和第二回合回答。两个用户回合可能产生多次模型请求。应分别统计，不能把请求数当回合数。

独立 `cayw-client --mode sdk` 的执行者在客户端进程中。另建 SDK 数据库与游标文件，每次调用使用相同的宿主及 `model` 配置，并提供该配置选中的凭据：

```sh
cayw_chat_sdk() {
  "$CAYW_INSTALL_ROOT/bin/cayw-client" --mode sdk \
    --db "$CAYW_CHAT_ROOT/state/sdk.sqlite" \
    --workspace "$CAYW_CHAT_ROOT/workspace" \
    --config "$CAYW_CHAT_ROOT/host.json" "$@"
}
cayw_chat_sdk create --label 'SDK two user turns'
```

随后用 `cayw_chat_sdk` 执行上述 `prompt`、`get`、`approve` 和检查命令。SDK 的每次调用必须使用相同数据库、工作区和宿主配置。独立客户端只使用公开接口，不用 SQL 表控制 Core。

## 取消、事件续读与重开

执行中的 `prompt` 收到 Ctrl+C 后保留原 Task ID。客户端读取该 Task，等到认领后的 Worker、Attempt 和队列围栏可用，再发送完整取消目标。客户端继续等待原任务并读取末尾事件。从另一进程取消时，必须把原 `task` 输出中的 ID 设为 `CAYW_TASK_ID`：

```sh
cayw_chat cancel --session "$CAYW_SESSION_ID" --task "$CAYW_TASK_ID"
cayw_chat tasks --session "$CAYW_SESSION_ID"
cayw_chat audit --session "$CAYW_SESSION_ID" --after 0 --limit 25
```

取消请求、取消确认和 Device 清理是三件不同的事。`cancelled: true` 不能证明进程已回收。需核对原任务、原 Worker 和 `processes_stopped` 的 `drained` 记录。清理成功也不能证明外部业务效果已撤销。工具执行期间取消可能保留 `outcome_unknown`；会话进入 `needs_reconciliation` 后拒绝新任务，不自动重做。

事件通过有界页面读取。只重接观察时，不再次提交 `prompt`：

```sh
cayw_chat events --session "$CAYW_SESSION_ID" --limit 25 \
  --cursor-file "$CAYW_CHAT_ROOT/state/ipc-events.cursor.json"
```

客户端先核对整页游标连续，写出并刷新 stdout 后，再原子保存 `{session_id, after}`。每次 `events` 只读一页；重复调用直到页面为空，才能确认已取完当前后缀。不同会话的游标文件在新任务入队前被拒绝。输出或游标保存失败后，显式重开可能重复显示已经打印的一页，但不会静默跳过未完整处理的一页。

页大小同时受条数和字节限制。IPC 默认单条消息最多 1 MiB；`--limit 1000` 的条数合法，也可能因事件内容较大而超限。本次真实记录中，459 条事件的完整响应为 1,339,283 字节；改为每页 25 条后可读完原事件。25 不是任意内容都能装入一页的保证。遇到 `response_too_large_outcome_unknown` 时，在同一已保存游标减小 `--limit` 继续读，不重新提交任务。`audit` 也应按最后一条审计游标分页；同一会话的审计游标可以有间隔，事件游标则必须连续。

IPC 的执行连接丢失会取消它承载的原执行。客户端报告错误，不自动重连或重发。另起客户端读取原会话、任务与事件。只读事件连接的关闭不会替另一个连接重发任务。

Runtime 进程退出后，用原数据库、工作区和配置重开。`get` 不接管旧执行者。需要恢复时显式调用：

```sh
cayw_chat recover --session "$CAYW_SESSION_ID"
```

恢复先核对原执行者和清理记录。审批前退出可把原任务记为 `interrupted`；可能已执行的操作仍保留未知。`recover` 不清除业务未知，也不重放 prompt。需要核对的会话应由可信宿主通过公开核对接口补充实际证据；此示例 CLI 没有提供通用的核对命令。

## 固定响应回归

从仓库根目录运行。两个路径必须指向实际安装的程序，`--out` 必须是全新目录：

```sh
python3 -I scripts/validate-headless-chat.py \
  --runtime "$CAYW_INSTALL_ROOT/bin/cayw" \
  --client "$CAYW_INSTALL_ROOT/bin/cayw-client" \
  --out "$CAYW_CHAT_ROOT/offline-run-001"
```

脚本通过同一 `model` 对象为 SDK 和 IPC 配置本机 HTTP 固定响应服务，覆盖两个用户回合、实际文件、逐次审批和拒绝、流式取消、执行中的固定程序取消、连续游标、进程重开，以及 IPC 的认证、版本、参数、执行断连和审批前退出恢复。它只向子进程提供专用假模型凭据和测试 IPC 令牌，不继承本机模型环境，也不调用 Magpie。预期为 18 次合成模型请求，真实请求为 0。服务/API 选择的本地回归还见 `crates/cayw-runtime/tests/provider_config.rs` 和 `provider_cli.rs`。固定数据、回环 HTTP 与真实服务验收分别记录。

每次结果保存 HTTP 请求与固定响应、实际配置、客户端命令、逐行事件、原任务、文件摘要、取消时间及 Device 清理记录。固定模型响应验证产品执行链；它不能证明真实模型会正确规划任务。真实验收还须绑定实际网关版本、授权和请求预算、完整输入、响应模型、消息与工具配对、外部结果、用量及失败成本。

首个协议的入口参考：[Magpie 集成说明](https://github.com/yetone/magpie/blob/main/docs/integrating.md)、[Cargo 安装命令](https://doc.rust-lang.org/cargo/commands/cargo-install.html)。这些文档不能替代本次实际网关版本和真实运行记录。
