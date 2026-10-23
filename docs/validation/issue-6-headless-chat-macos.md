# Issue #6：macOS 无界面 Chat 本地验证

本地检查日期：2026-10-21。授权真实验收与复核日期：2026-10-22；原始 UTC 日志跨 2026-10-21/22，按文件内时间保留。范围为 macOS 27.0.1（26A434）、ARM64、Rust/Cargo 1.95.0。其他 macOS 架构未验证。

Issue #6 已完成本记录所列 macOS ARM64 范围的整项本地验收。可安装 Runtime、独立 Rust 客户端、可信配置、固定响应组合场景及最终本地回归已完成。完整默认 workspace 测试为 1,480 项通过、0 项失败；独立客户端另有 9 项通过。用户另行授权本项使用 Magpie DS Flash，预算 500,000,000 token。SDK 与认证 IPC 已各完成两个真实用户回合，原始 HTTP、实际文件、持久消息、工具配对、游标续读与进程重开均经只读复核。独立控制复核与取消补跑证据已核，11 项交付条件的收据检查退出 0。改动尚未提交，GitHub Issue 未修改。

## 源码与证据位置

- 交付目录：`/Volumes/ORICO/cayw-issue6-integration-20261021-a`。
- 分支：`feat/issue6-headless-runtime`；基线提交：`d6d8977249acdd8b54a2bba3db49edbb67465651`。
- 证据目录：`/Volumes/ORICO/cayw-issue6-evidence-20261021-a`，下文证据名均相对于该目录。
- #5 最终交付清单：`/Volumes/ORICO/cayw-issue5-evidence-20261021-a/final-delivery-source.json`，内容清单摘要为 `472572d54498c2f31177a3fac2131c3cb0083311d985ad4225b1a0fd39124950`。
- 本目录包含完整未提交的 #5 交付。相对 Git HEAD 的全部变更不能统称 #6 新实现。最终收据另列与 #5 清单的差异。
- 任务条件冻结于 `issue6-contract.json`，对应 GitHub Issue #6 的 `updatedAt=2026-10-19T14:53:37Z`。功能范围以该任务为准。

每条主线程命令通过仓外 `validate.py` 保存 `.log`、`.json` 和 `-source.json`，包括完整命令、退出码、UTC 时间、工具链、锁文件、源码清单和运行前后变化。清单包含未提交文件和删除状态。代理的独立证据采用各自具名收据，不能把局部文件稳定写成全仓稳定。

## 本地交付

1. `cayw-ai` 的助手消息保存 `usage_status`：`unknown`、`partial`、`reported`。缺失、部分报告、完整零值、分块补齐、错误和取消分别验证。没有把未知费用当作零。
2. `HeadlessConfig` 为 SDK 和 `cayw serve --config` 提供同一可信入口。支持预算、宿主固定 Pipes 程序和精确权限；进程配置本身不授权。坏配置在创建数据库和监听前拒绝。
3. `examples/headless-client` 是独立 Cargo workspace，提供可安装的 `cayw-client`。两种模式调用公开 SDK/认证 IPC；输出逐行 JSON，保存连续游标，Ctrl+C 后继续观察原任务。客户端不读 Core SQL 表实现控制。
4. 跨 Runtime 审批检查原 Task、Attempt、Worker、fence、Core lease 和原执行者持有的任务锁。审批开始与完成分别记录；审批中途持久化失败时保留待核对状态，不能由撤销操作擦除。
5. 审批写入和名额封闭写入同时失败时，原执行者不走普通完成路径。名额保留到显式恢复。三条双故障测试使用同进程两个 Runtime 和独立连接，不声称跨进程双故障通过。
6. Requests 的短事务锁在既有 `sqlite_busy_timeout_ms` 内等待。未取得锁前不读时钟或记录，不重放事务、模型请求或能力。该锁与 Runtime 生命周期锁、原任务锁职责不同。

使用方法、完整宿主配置和实际命令见[无界面对话指南](../headless-chat.md)与[独立客户端](../../examples/headless-client/README.md)。

## 已安装组合场景

初版完整通过记录为 `installed-chat-v4.json` / `.log` 与 `installed-chat-v4/`。运行时间为 `2026-10-21T15:19:54Z` 至 `15:20:03Z`，退出 0，SDK/IPC 均通过。

| 已安装文件 | SHA-256 |
| --- | --- |
| `/Volumes/ORICO/cayw-issue6-runtime-install-20261021-b/bin/cayw` | `b46af7f9e0cf6ed7f8a91fda695184d1c02f5c7a4e0dbee021124b2a8f649526` |
| `/Volumes/ORICO/cayw-issue6-client-install-20261021-c/bin/cayw-client` | `1fd40d47209ceca907aae9ce1baed3f2deb5e849aeb1e8fbb0977bf69ad5f59e` |

安装命令与构建清单分别保存在 `runtime-install-b.json`、`client-install-c.json`。两者均为 macOS ARM64 debug 安装，Runtime 使用 `--no-default-features`，锁文件固定且没有联网解析依赖。安装时源码清单为 `252d614261edbac2ad37a22cc4cfe32086423ba4925423caf26a46736856f4b5`。

模型响应由自启 loopback HTTP 服务合成。请求路由为 `group/auto-deepseek-v4-1-flash`，实际响应模型明确标为 `offline-served-deepseek-fixture`。假 key 为 `offline-http-fixture`。Runtime 没有启用内存 OfflineProvider，而是通过真实 HTTP/SSE 适配器消费分片响应。一次完整组合共 18 次合成模型请求，真实请求 0。SDK 和 IPC 的两回合成功任务各有 4 次模型请求，不能写成 4 个用户回合。

| 场景 | 实际核对 |
| --- | --- |
| SDK/IPC 各两个用户回合 | 随机标记只在输入文件；Luau 调用实际 `fs.read`/`fs.write`，模型消费结果后回答；第二回合使用上一回合事实 |
| 文件与身份 | `round1.txt` 等于原文件，`round2.txt` 等于 `continued:` 加原内容；两任务完成、不同 Attempt、原 Worker/fence、四个子能力终态与原父调用绑定 |
| 审批 | 每个精确文件调用分别允许；批准前无对应执行/写入；可信拒绝时没有文件效果；模型声称批准和迟到审批不能放行 |
| 模型流取消 | 已收到部分流后取消；原任务 cancelled；部分消息保留，用量 unknown |
| 工具执行取消 | 观察到自有 `/bin/sleep 30` 执行后再取消；原任务保留 outcome_unknown；Device drained 审计在原任务终态之前；再次 prompt 被拒绝且没有重放 |
| 事件续读 | 保存前三条事件游标，再取同会话后缀；前缀加后缀等于完整持久事件，任务数不变 |
| 进程重开 | 同一数据库、配置和工作区重开后，原 Session、消息和事件保持 |
| IPC 边界 | 未认证读取、错误 token、未知版本、未知字段、未来游标、limit=0、未知方法均明确拒绝 |
| 执行连接断开 | 原任务 cancelled，不重连重发；观察原任务而不是创建替代任务 |
| 审批前退出 | 杀掉原 Runtime 后显式恢复；同一原任务 interrupted、fence 增长；迟到审批不能修改原终态 |

`tool-cancel-result.json` 分别保存请求时间、取消确认、原进程与任务、清理和终态。外部 `ps` 只观察 PID、父 PID 和秒级启动时间。Device 内部完整身份由具名 Rust 测试验证；不能把外部观察精度扩写为微秒身份验收。`drained` 证明该执行者的清理完成，不证明业务效果撤销。

独立执行者随后运行同一安装脚本，证据为 `installed-chat-independent-v1/` 和 `installed-chat-independent-wrapper-20261021T152336834195Z/`。完整命令退出 0。独立读取实际文件、97 条连续事件、原任务、HTTP 记录和清理顺序，确认两种模式各有两个用户回合。18 次请求仍全部是本机合成响应。脚本及两个二进制前后摘要一致。

独立复核另外发现：未知会话再次请求时，SDK 和 IPC 均拒绝且持久任务仍为未知，但 IPC 的错误 `outcome_unknown=false`。该补充检查确实失败，保存在 `independent-artifact-review.json`；不能把它抹去，也不能把原脚本退出 0 写成已覆盖该字段。后续新增字段断言后，用原安装版复跑得到 `reconciliation-installed-red-v1` 退出 1，错误为 `rejected prompt lost the original unknown outcome`，16 次合成请求、真实 0。修复后的安装与结果另列。

### 修复后的最终安装版

`runtime-install-c` 与 `client-install-d` 在全新目录安装修复后的两个程序，均退出 0。命令分别为：

```sh
cargo install --path crates/cayw-runtime \
  --root /Volumes/ORICO/cayw-issue6-runtime-install-20261021-c \
  --bin cayw --no-default-features --locked --offline --debug
cargo install --path examples/headless-client \
  --root /Volumes/ORICO/cayw-issue6-client-install-20261021-d \
  --bin cayw-client --locked --offline --debug
```

| 最终安装文件 | SHA-256 |
| --- | --- |
| `/Volumes/ORICO/cayw-issue6-runtime-install-20261021-c/bin/cayw` | `b5d2d3a866e86cf70c725d3aee1b082527ed0b8b146f3e5f694b6b2b7382f8ba` |
| `/Volumes/ORICO/cayw-issue6-client-install-20261021-d/bin/cayw-client` | `91dd7f14057c3904408020cb14ade011c59c58b5440716721020154e4cf71ef3` |

`installed-chat-v5` 使用以上两个程序和增加未知错误字段断言后的脚本，完整复跑前述场景。实际时间为 `2026-10-21T15:45:06Z` 至 `15:45:16Z`，退出 0。SDK、IPC 均通过；18 次本机合成请求，真实请求 0，端点为 `http://127.0.0.1:54205/v1`。

主线程另从磁盘复核两种模式的 `round1.txt`、`round2.txt`、两个原任务及各 97 条连续事件。两种模式取消工具后均保存原 Worker/Attempt；`processes_stopped` 的 drained 游标为 21，原任务未知终态游标为 23，自有进程已不存在。再次 prompt 均非零退出，错误为 `code=needs_reconciliation`、`outcome_unknown=true`，没有新增任务或工具取消场景请求。安装收据、二进制摘要、完整日志与七个独立源码文件的核对结果保存在 `local-evidence-review-v1.json`。

安装、完整工作区检查和该组合场景共同绑定源码清单 `968f94b748afafa7ab0b935682da52badc8a234037c44c1e7975297514cb71fe`，各条命令运行期间源码未变。此后仅修改交付文档，最终收据逐条列出差异。

## 已完成的独立与定向检查

| 检查 | 结果 | 证据 |
| --- | --- | --- |
| 独立客户端修复后全部目标 | 9 项通过，无失败、忽略或过滤；严格 Clippy / fmt 均退出 0；原日志及七文件摘要已核 | `client-reconciliation-fix-20261021T153623847827Z/` |
| Runtime 修复后原生与 IPC 回归 | 24 + 5 = 29 项通过，无失败、忽略或过滤，退出 0 | 同目录的 `green-runtime-lib-ipc.log` |
| Runtime 审批、IPC、可信配置 | 10 + 5 + 7 = 22 项通过，退出 0；原日志与当前八文件摘要已核 | `runtime-approval-ipc-config-20261021T152008251666Z/` |
| Requests 完整包回归 | 46 项通过，Clippy / fmt 通过；含测试辅助入口和 compile-fail 文档测试 | `requests-contention-delivery.json` |
| VerifyTool 审批证据与边界场景 | 3 + 2 = 5 项通过，包级严格 Clippy / fmt 退出 0 | `access-audit-green-v2`、`access-audit-clippy-v1`、`access-audit-format-check-v1` |

独立客户端锁文件的 299 个 registry 包按名称、版本、来源和 checksum 与根锁一致。独立锁文件无需包含根 workspace 的所有依赖。

Runtime 的 normal/features 依赖树由 `runtime-dependency-tree-v1` 实测，命令为：

```sh
cargo tree -p cayw-runtime --no-default-features --locked --offline \
  --target aarch64-apple-darwin -e normal,features
```

实际 Runtime 图不含 TUI、WebUI、plugin、remote、VerifyTool 或 Codex。主线程核过原依赖日志摘要，并确认当前 29 个 Cargo 清单、锁文件及构建配置输入与该记录一致，见 `final-dependency-review-v1.json`。整个 workspace 的检查会编译其其他成员；这不表示安装的 Runtime 启用了这些组件。

### 使用文档与配置复核

独立执行者核对指南、客户端说明、JSON 示例与实际公开入口，并用原已安装 Runtime b 离线加载 `host.example.json`。启动摘要与七个预算值、固定 `hold`、`pipes` 和零条预授权一致；SIGINT 后退出 0，stderr 为空。没有连接客户端、提交任务、执行 profile 或请求模型。命令、二进制和 20 个具名文件摘要见 `headless-doc-review-v1-rpuysqos/receipt.json`，静态结论见同目录 `review.json`。该旧安装仅证明所记配置的接受与启动；不替代最终 Runtime c/客户端 d 组合证据。

复核指出两处指南表述过强，已按实际源码修正：`max_capability_bytes` 同时计算能力参数和结果封装的 JSON 字节；`max_total_tokens` 在每次模型响应结束后累计报告用量，累计值大于阈值才停止，末次响应可能使总量超出阈值。没有借文档收尾修改预算实现或扩大本次范围。

## 失败与修复记录

- 安装版早期 v2 的进程观察失败。macOS 中文 `ps lstart` 日期少一个空白字段，脚本把完整命令拆错。自有 `/bin/sleep 30` 复现先退出 1；只给该 `ps` 调用设 `LC_ALL=C` 后退出 0。父 PID 和精确命令检查保留。证据为 `process-locale-red-v1` / `process-locale-green-v1`。该失败没有证明 Device 未启动；旧场景未及时发取消，最终 HTTP 固定响应失败和未知终态均保留。
- v3 场景断言通过，但取消 HTTP 连接后，Python handler 继续读下一请求，产生 ConnectionResetError 栈。固定响应服务每请求关闭连接后，v4 完整场景退出 0，日志没有这段噪声。
- `final-test-v1` 完整 workspace 命令退出 101。唯一已观察失败为 `approval_reader_rejects_missing_or_changed_original_execution_evidence`：验证器仍要求第二次审批暂停时 audit 为空，终态也只允许旧的两条收尾记录。真实 Runtime 新增审批开始/完成记录后，旧验证器错误拒绝原始报告。TaskInput 没有丢失。
- 原失败已通过 `access-reader-repro-v1` 独立复现。最小修复精确验证两对开始/完成记录，以及原有授权撤销和任务完成记录。新增 24 类损坏证据，涵盖删除、重复、错序、错误身份/字段/游标/授权和暂停前缀不一致；原有权限、原任务、Provider 消费、IPC 原帧及实际文件检查保留。`access-audit-green-v2` 5 项通过。
- IPC 未知错误字段的差异来自 Runtime RPC 错误码映射。IPC v1 的 `RpcError` 只有 code/message，`ClientError` 按 `_outcome_unknown` 后缀识别未知。原 Runtime 仅返回 `needs_reconciliation`，导致 IPC 布尔值为 false。修复后唯一运输层错误码为 `needs_reconciliation_outcome_unknown`；示例客户端对外显示 `needs_reconciliation` 并原样保留运输层未知标志。不新增 wire 字段，不改协议版本，不保留旧 wire 别名。`red-client.log` 两项和 `red-runtime.log` 一项在修复前确实失败，退出 101；修复后客户端全 9 项、Runtime 24 + 5 项通过。新增测试实际启动固定 `/bin/sleep 30` 后取消，再核 SDK/IPC 与原生 RPC 的拒绝、原任务身份、完整任务列表和请求数不变。已知 Busy 错误仍为 `outcome_unknown=false`，没有把所有错误一律标未知。

所有旧失败原日志和退出码保留。修复不能把本机固定响应记录变成真实模型证据。

## 最终工作区检查

以下命令在最后一次 Rust 修改整合后执行，全部退出 0。共同源码清单为 `968f94b748afafa7ab0b935682da52badc8a234037c44c1e7975297514cb71fe`，根 `Cargo.lock` SHA-256 为 `b3329220076fc60e7e240096eccf9e279886e33d218cbf27104c20c2e9871060`。每条命令运行前后清单一致。

| 命令 | 结果 | 证据 |
| --- | --- | --- |
| `cargo fmt --all -- --check` | 退出 0 | `final-fmt-v2` |
| `cargo check --workspace --locked --offline` | 退出 0 | `final-check-v3` |
| `cargo test --workspace --locked --offline` | 1,480 项通过，0 失败，7 个默认忽略入口；223 个顶层目标 | `final-test-v2` |
| `cargo clippy --workspace --all-targets --locked --offline -- -D warnings` | 退出 0，无告警 | `final-clippy-v2` |
| `cargo run -p cayw-agent --example offline --locked --offline` | 实际 add 返回 5，助手回答 `2 + 3 = 5`，5 条消息且 running=false；退出 0 | `final-offline-v2` |

完整测试为 `2026-10-21T15:43:31Z` 至 `15:53:06Z`。原日志共有 226 组 result，其中 3 组是父测试启动子进程后重复输出的结果；计数按每个顶层目标的最后一组 result 汇总，没有重复计数。7 个默认忽略入口中，6 个是由父测试单独运行的子程序入口；remote 独立二进制入口未执行，不属于本次无远程交付。没有运行 `--all-features`。独立客户端是嵌套 workspace，9 项单列，不混入根 workspace 的 1,480 项。

`final-test-v2` 仍有既存的多个 `offline` 示例输出文件同名提示。该命令实际退出 0；Agent 离线示例另行明确指定包并执行成功。此记录不将有提示的完整测试写成“无告警”。修复前 `final-test-v1` 的失败与其他历史记录原样保留。

最终源码、与 #5 的具名差异、各成功命令后的文档变化及日志摘要绑定于 `final-local-delivery-v1.json`、`final-local-delivery-v1-source.json` 和 `final-local-delivery-v1-changes.json`。文档链接、配置解析、脚本语法与 `git diff --check` 的正式结果保存为 `final-document-check-v1`。这些文件在仓外证据目录，不写入工作区源码清单自身。

## 授权真实模型验收

本节记录之后执行的真实阶段。前面的固定响应、0 次真实请求和 `final-local-delivery-v1.json` 仍是当时本地阶段的历史结果，原收据没有覆盖改写。

真实证据根目录为 `/Volumes/ORICO/cayw-issue6-live-20261022-a`，本节相对路径均以此为根。`authorization.json` 保存用户原文“允许验收 使用 magpie DS flash 预算 500m”，额度独立于旧 #2/#3/#4 的 100M。授权覆盖本项验收与必要诊断，没有扩展到提交、推送、合并或关闭 Issue。

网关通过 `GET /api/hello` 实测为 Magpie `0.1.1308`。实际上游是 `http://127.0.0.1:3425/v1`，请求路由保持 `group/auto-deepseek-v4-1-flash`；全部 106 次实际 HTTP 响应均为 200，原流中实际模型均为 `deepseek-v4.1-flash`。模型目录虽已使用新的同模型名称，本次冻结路由实际可用，未自动切换。认证采用 Magpie 文档允许的专用本机应用标识，没有读取已有模型密钥或其他 Agent 配置，没有修改网关共享、网络或路由配置。该规则不外推到远端服务。

仓外记录器 `runner/live_gateway.py` 在每次上游请求前持久预留 1,100,000 token，并限制请求正文为 1 MiB、每轮运行 48 次请求、总账本 256 次请求。它逐字节转发请求正文与响应实体正文，但替换认证头并改变 HTTP 分块，不将它表述为完整网络传输完全原样。它不生成模型回答、不重试、不改模型。全部真实任务使用上列 Runtime c 与客户端 d，二进制散列一致。产品源码在真实阶段没有变化；收尾仅更新说明文档。

初始每任务阈值为 8,192 token、8 次模型循环。`two-turn-v2` 在真实发现能力过程中用尽该阈值后，从 `two-turn-v3` 起改用 65,536 token、20 次模型循环；其他共同配置为 180 秒总时间、24 次能力调用、524,288 能力字节、120 秒审批/单次 Host 等待，`permissions=[]`。变更写入 `configuration-change-after-two-turn-v2.json`，没有修改初始授权原件或产品预算语义。65,536 是每次响应后检查的停止阈值，独立 500M 账本负责真实调用的保守额度准入。

### 两种模式的两个真实用户回合

| 公开路径 | 成功证据 | 用户回合 | 模型请求 | 父工具调用 | 实际 Rust 子能力 | 连续事件 |
| --- | --- | --- | --- | --- | --- | --- |
| SDK | `runs/two-turn-v5/sdk`；`sdk-independent-v3/verification.json` | 2 | 10 | 8 | 10 | 633 |
| 认证 IPC | `runs/two-turn-ipc-v6/ipc`；`ipc-independent-v2/verification.json` | 2 | 7 | 5 | 7 | 808 |

SDK 原 Session 为 `746c202c-f147-4566-a952-611437b770bb`，IPC 原 Session 为 `a792e6cb-beca-4e28-8d7c-7042629d8ca1`。每种模式使用独立工作区、数据库和随机标记。标记只在 `source.txt`，没有放入用户提示。第一回合通过 Luau 调实际 Rust 文件能力，把完整内容写入 `round1.txt` 后回答；第二回合在同一 Session 读取原文件，写入严格等于 `continued:` 加完整原内容的 `round2.txt`，再次报告原标记。两个用户 Task 均为 completed，使用不同 Attempt 和 Worker。

`runner/verify_two_turns.py` 调用已安装客户端的 get/tasks/events/audit，并逐条核对原 HTTP 与持久记录。复核期间模型端点禁用为 `http://127.0.0.1:9/v1`，没有提交 prompt。复核确认：每个父调用和结果按 ID/工具名配对；每个结果实际进入后续模型请求；子能力关联原 Task、完整 Core lease、父调用和 Luau 源码散列；精确审批来自 sdk-user 或 rpc-user，发生在能力开始之前；第二回合请求保留上一回合事实。

复核还确认用量拆分与原流一致：非缓存输入、缓存读取、缓存写入之和等于上游 prompt_tokens，输出与推理计数分别对应。不能把非缓存 input 单独与包含缓存的 prompt_tokens 比较。Task 的 fence 按单 Task 计数，Core AttemptLease 的 fence 按 Session 递增；二者作用域不同，核对各自原记录，不要求数字相等。

每种模式另由新读取进程保存前三条事件的游标，再读到空页；前缀加后缀与完整原持久事件相等，没有新增任务。重开后，Session、消息、任务、审批、事件和审计均一致。上述独立复核分别退出 0，新增模型请求均为 0。SDK 成功来源所在 `two-turn-v5` 的总 summary 仍为 false，因为同轮 IPC 第二回合失败；不能把该历史总结果改成通过。

### 可信审批、取消、重开和 IPC 边界

原控制场景位于 `runs/controls-v1`。旧 IPC 工具取消的补跑在 `runs/controls-ipc-tool-v2`。补齐 HTTP 取消记录与原 PID 外部观测的最终取消运行在 `runs/controls-cancel-v3`，四项场景均通过，运行脚本退出 0，共 12 次真实请求；原 prompt 按取消或未知终态非零退出。原始控制场景和补跑分别由独立执行者复核，报告保留为 `control-independent-review-v1.json`、`control-independent-review-v2.json`；主线程的可复跑只读核证为 `cancellation-record-verification-v1.json`。

| 条件 | 实际核对与证据 |
| --- | --- |
| SDK/IPC 等待、可信允许与拒绝 | 两回合每次精确文件调用分别审批；拒绝场景只申请 `fs.write {path:denied.txt, content:DO_NOT_CREATE}`，可信决定前没有开始，拒绝后无文件，子能力 not_executed；拒绝结果确实进入后续模型请求 |
| 模型流期间取消 | 最终 SDK/IPC 均在收到非空 thinking_delta 后取消，原 Task cancelled，部分消息 aborted，原身份不变；记录器分别记录取消发现与上游 socket 关闭时间。此证据覆盖思考流取消，不扩大为答案正文每个时点 |
| 工具期间取消与 Device 清理 | 固定 `test.run {profile:hold}` 经可信批准后启动 `/bin/sleep 30`，记录原 PID、父 PID、进程组及秒级启动时间；取消后不依赖父 PID，按原 PID/进程组核对已消失，保存筛选后的原始 ps 行；Device drained 先于原 Task 的 outcome_unknown |
| unknown 不重放 | SDK/IPC 后续 prompt 均被 needs_reconciliation / outcome_unknown=true 拒绝，未新增 Task 或 HTTP；进程重开仍保留同一未知会话，迟到审批被拒绝。drained 不能证明业务效果撤销 |
| 审批前进程退出 | 原执行者退出后显式 recover；原 Task interrupted，原 Task fence 增加，审批由恢复者拒绝，能力 not_executed；不新增模型请求，迟到审批不能反转终态 |
| IPC 执行连接断开 | 原连接只 enqueue/resume 各一次；真实非空流后断开，原任务 cancelled，部分消息保留，上游取消记录存在；不自动重连重发 |
| IPC 认证和参数边界 | 未认证读取、错误 token、外帧 v2、多余 actor 字段、未来游标、limit=0、未知方法均明确拒绝；原空会话、Task 和模型请求不变。部分精确请求需结合冻结脚本，脱敏 token 不用于区分正误值 |
| 本机监听边界 | `runs/listen-boundaries-v1/result.json`：已安装 Runtime 拒绝 0.0.0.0，已安装客户端拒绝非回环地址；均在配置/参数阶段退出 1，无监听和新数据库，未提供模型环境或请求模型 |

最终取消补跑中，SDK 固定程序原 PID/进程组为 1091，IPC 为 1638；两者取消后均不存在。各自清理审计 cursor 5 早于原未知终态 cursor 7。SDK 工具场景使用 7 次模型请求，IPC 使用 3 次；各流取消各 1 次。SDK 在实际能力调用前纠正过数次脚本或参数错误，七次 HTTP 对应九个脚本执行，其中五个已知失败；最终只有一次真实 test.run 开始，取消后没有重放。外部观察只证明本次固定 Pipes 程序的原 PID/进程组已消失，不能泛化为任意后代进程拓扑。进程清理失败、持久化失败及旧结果反转的定向回归仍以上述具名 Rust 测试为证，不把审批前崩溃扩写为 #7 的完整矩阵。

流取消只证明记录器执行了本地上游 socket shutdown，不证明 Provider 远端停止计算或停止计费。SDK 这次 shutdown 晚于运行取消审计约 33.5 毫秒；取消确认与任务终态不能解释为同步关闭上游。该时间是本次观测值，不是固定时限保证，缺失用量继续保留未知。

### 全部请求、未知用量与失败成本

`budget-independent-verification-v2.json` 逐条读取原 SSE、HTTP 200 记录、请求/响应散列和预算账本。共 106 条请求均已结算其记录状态；其中 5 条缺少完整用量，仍保持未知并保留额度。没有把中断时数值字段的零当作零费用。

| 运行 | 实际 HTTP 响应 | 已报告 token | 未知用量保留 |
| --- | --- | --- | --- |
| route-probe-v3 | 1 | 1,090 | 0 |
| two-turn-v1 | 8 | 14,286 | 0 |
| two-turn-v2 | 8 | 17,592 | 0 |
| two-turn-v3 | 7 | 13,944 | 0 |
| controls-v1 | 29 | 50,061 | 3,300,000 |
| two-turn-v4 | 15 | 40,463 | 0 |
| controls-ipc-tool-v2 | 3 | 4,903 | 0 |
| two-turn-v5 | 16 | 44,114 | 0 |
| two-turn-ipc-v6 | 7 | 20,579 | 0 |
| controls-cancel-v3 | 12 | 24,385 | 2,200,000 |
| 合计 | 106 | 231,417 | 5,500,000 |

已报告与保守预留合计 5,731,417 token，小于独立授权 500,000,000；未结算请求为 0。未知账本 ID 为 32、45、50、95、103，每条保留 1,100,000 token。已报告数不是服务商账单；保留额度也不是已发生费用。本次额度不挪用旧账本。

失败与修复全部保留：

- route-probe-v1 错读 create 返回层级，v2 记录器误查 max_tokens；都在真实上游请求前失败，真实请求为 0。v3 通过。
- two-turn-v1 审批字段读错层级，v2 任务阈值耗尽，v3 与 v4 的脚本只读范围漏列模型合理的回读；失败任务和成本保留。补齐的是逐次精确读取范围，写入参数没有放宽。
- two-turn-v5 的 SDK 两回合成功；IPC 第二回合在 `continued:` 后申请多写一个空格，被可信审批拦住，错误文件未写。提示明确“无额外字节”后，只补跑 IPC，two-turn-ipc-v6 成功。
- controls-v1 一次读取 459 条事件时，完整 RPC 响应为 1,339,283 字节，超过默认 1 MiB；实际 JSON 深度 16，小于限制 64。`runs/event-page-diagnosis-v1/diagnosis.json` 在原会话禁用模型端点后复现超限，再以每页 25 条读完，与 SDK 全量一致。没有新增 prompt、任务或真实请求。此后仓外脚本连续分页，不放大协议限制。
- controls-v1 的 SDK 流取消已确认本地任务取消，但记录器留下 ConnectionResetError，未能分类上游中断；旧 run 总 summary=false 保留。异常分支还漏填派生 served_models，但原 SSE 中有实际模型和响应 ID。仓外记录器在本机合成检查中先失败、修复后成功，现于异常路径也从已保存正文提取模型/用量。真正上游错误仍保留。controls-cancel-v3 另行真实补证，未覆盖旧记录。
- 旧工具取消只按父 PID 查 sleep，父进程退出可能漏掉改换父进程的程序；这不是发现实际泄漏。仓外观测改为按原 PID/进程组，先用自有改换父进程的 sleep 复现旧盲区，再验证新检查，真实取消补跑后均已消失。
- 两回合只读核证曾错误要求 Task fence 等于 Core fence、成功审计名为 run_completed、非缓存 input 等于上游全部输入。核实际源码与原记录后修正核证，没有改产品迎合断言，没有因此重发成功任务。取消只读核证另有一次误从单项结果读请求计数；改读总记录后退出 0，未新增模型请求。

### 交付边界与复跑入口

真实任务、控制脚本和记录器都在仓外 `runner/`；每次 run 保存当时脚本副本、二进制散列、实际配置、命令/退出码、完整原始请求与响应、事件、审计、持久数据库和外部文件。真实调用没有依赖 UI、plugin、远程服务、VerifyTool 或 Codex。

只读复核入口如下。每次 label 与输出文件必须使用新名称，不能覆盖历史证据。两个正常任务复核的 Provider 均指向禁用端点，只读原 Session，不提交 prompt：

```sh
python3 -I /Volumes/ORICO/cayw-issue6-live-20261022-a/runner/verify_two_turns.py \
  --run two-turn-v5 --modes sdk --label reader-check-001
python3 -I /Volumes/ORICO/cayw-issue6-live-20261022-a/runner/verify_two_turns.py \
  --run two-turn-ipc-v6 --modes ipc --label reader-check-001
python3 -I /Volumes/ORICO/cayw-issue6-live-20261022-a/runner/verify_budget.py \
  --output /Volumes/ORICO/cayw-issue6-live-20261022-a/budget-reader-check-001.json
```

交付条件检查的首份成功收据为 `live-acceptance-check-v1.json`，退出 0，11 项条件通过，绑定 12,134 个真实阶段证据文件；当时源码清单为 `e18a6e0d06fc1d9b38574d5762e3a9abae0def3a1c45b0477c5886774d2429a2`。更新完成状态后，最终真实收据为 `final-live-delivery-v1.json`，另有同前缀的 `-source.json`、`-changes.json`、`-evidence.json`，分别绑定当前源码、与 #5 的 37 个具名变化路径、安装/检查日志、106 条请求和全部失败原件。该收据不包含自身散列，避免自引用；各命令原日志另由本地记录器保留。

最终文档检查为真实目录的 `document-check-final-v1.json`，命令日志位于本地证据目录的 `live-document-final-v1.log`；核对本地链接、示例 JSON、11 个脚本语法及 `git diff --check`。旧 `final-local-delivery-v1.json` 保留真实请求 0 的历史事实。GitHub 状态未同步；没有将 UI、plugin、remote、Linux 或 Windows 纳入本次交付。#7 的完整崩溃矩阵由后续任务单独验收。
