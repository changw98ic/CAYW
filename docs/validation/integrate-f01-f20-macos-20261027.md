# F01–F20 本地集成验证

日期：2026-10-27。集成分支：`integrate/f01-f20-20261027`。基底：`dev` 的 `9cc509d66dff9300333ca0bf5af2e8b7868c0541`。

本次本地集成代码已通过下列统一检查、组合场景与实际界面验证。最终结果为 `FINAL-RESULT.json`，原始日志与源码摘要已核对。结果限定于本记录的 macOS arm64、配置和场景，不把全部功能 Issue 标为完成。没有修改远端 Issue、PR 或分支状态。

## 集成范围

六组并行补丁已经进入同一工作树。Runtime、Host、RPC、启动配置与依赖清单由集成主线统一处理。已有的可视化编辑器和 Trace Debug 直接扩展当前基底，没有重复合入旧分支。证据目录的 `feature-provenance.json` 逐项记录 18 个功能分支和两组选择性 UI 输入，20 个提交对象已核对存在；该清单绑定本次导入，不假定远端分支此后不变。

| 功能 | 本次接入 |
| --- | --- |
| F01 / F02 / F03 | 正式 `result.*` 引用、自动外置、进程摘要与原文、分页读取和软释放；网页、图像与过滤原文共用 Device 内容存放 |
| F04 / F16 / F17 | 统一文件版本与符号位置；Host、Editor、Trace 可共用进程内 bridge；捕获上下文后局部重算受影响下游 |
| F05 / F06 | 用量和上下文读取原模型请求账本；运行过程读取原 Core 事件，缺失价格保持未知 |
| F07 / F08 / F09 / F19 | 工作台面板、带版本的计划批注、会话产物和公开动作入口；实际执行继续经过 Runtime/Host |
| F10 / F11 | 可选网页与图像配置、严格参数及持久结果引用 |
| F12 / F13 | 子任务与后台任务归属、日志、取消、原执行者清理核对及显式恢复 |
| F14 / F15 | 配置导入 MCP 生命周期；可选 Pi 工具适配器使用同一 Bridge 与 plugin 注册表 |
| F18 / F20 | 复用外部工程验证入口；组合测试覆盖实际进程、stdio MCP、SQLite、SDK 与认证 IPC |

旧 WebUI/TUI 的有效视图已经迁入当前接口。Core 不依赖界面、远程服务或 Node；这些组成仍按原边界启用。`dependency-boundaries.json` 从本次 Cargo 元数据核对了 33 个工作区成员：Core 没有 UI 或 plugin 的直接运行依赖，Runtime 没有 UI 或 VerifyTool 的直接运行依赖，依赖图中没有 Codex 包。可选 Pi 和远程功能仍按 Cargo feature 启用。

## 本轮修正的组合问题

- 过滤原文、网页正文与图像输出统一引用。内容写入失败和元数据冲突返回实际错误；已经释放的引用保持释放，重新生产创建新引用。
- 结果读取不扩大精确授权。显式拒绝和撤销阻止自动读取，后续可信审批可以重新授权。任务自然到期与用户撤销分开判断。
- 后台任务记录自己的执行者、进程身份与清理证据。父模型任务的进程清理不能替代后台任务清理。探测、日志或数据库失败保留未知结果，普通说明文字不能解除未知状态。
- 计划批注提交用户读到的任务、版本和内容摘要。提交与计划更新共用持久协调，旧计划的批注不会落到新计划上。
- 网关消费设备选择并连接对应 IPC。Runtime 不再接受后丢弃该字段。产物动作在取消前先核对所属会话。
- Editor 和 Trace 使用各自页面登录。工作台直接打开配置地址，不在 URL 中传递主 WebUI 或 IPC 令牌。
- Pi 允许真实 npm scoped 来源名称，工具名称保持原规则。适配层关闭未声明的额外顶层参数。Bridge 固定原 plugin 实例；旧来源不能调用或卸载替换实例。调用前取消和执行中未知结果分开处理。
- 启动配置与 Runtime 执行共用同一份能力目录。计划更新、人类提问与会话值的精确授权使用实际参数声明，额外字段和未启用能力在启动前拒绝。

## 环境与证据

实际执行平台为 macOS ARM64，Darwin 27.0.0。其他 macOS 架构未验证。Rust 命令使用本机已安装工具链；Clippy 使用 `1.95.0`。Node 为 `v24.4.1`。

本轮原始日志位于 `/tmp/cayw-integration-20261027/`。最终统一检查使用 `final-suite-03/`，每条命令分别记录退出码、日志摘要及前后源码。主线重新统计日志，核对工作区、F20 和真实终端记录；`verified-summary.json` 通过。`final-code-before-03.json` 与 `final-code-after-03.json` 的 743 份 Cargo、crates 和 scripts 文件完全相同，包含当时未跟踪的代码。仓库文档在全部检查结束后收尾。

| 最终检查 | 实际结果 |
| --- | --- |
| `cargo fmt --all -- --check` | 通过 |
| `cargo check --workspace --locked` | 通过 |
| `cargo test --workspace --locked --no-fail-fast` | 2357 项通过、0 项失败、9 项跳过 |
| `cargo +1.95.0 clippy --workspace --all-targets --locked -- -D warnings` | 通过 |
| `cargo check -p cayw-runtime --all-features --all-targets --locked` | 通过 |
| Pi 四目标：`serve_config`、`provider_config`、`pi_compat_runtime`、`pi_mcp_combo` | 21 项通过，无失败或跳过 |
| 故障注入四目标：`result_sources`、`result_externalization`、`results`、`approvals` | 36 项通过，无失败或跳过 |
| `cargo test -p cayw-content --all-features --locked` | 58 项通过，无失败或跳过 |
| 自动外置故障测试的定向 Clippy | 通过 |
| `cargo run -p cayw-agent --example offline --locked` | 通过 |
| F20 四目标、10 个要求场景 | 全部通过，无失败、跳过或缺失场景 |
| 重建 TUI 与真实 tmux 交互 | 通过，原终端记录与进程清理收据已核对 |

这些检查存在范围重叠，测试数量不相加为独立场景数量。9 项默认跳过中，8 项是由父测试启动的子进程辅助入口，1 项远程二进制测试已另行构建示例并在本机补跑；该项不代表两台设备验收。

旧失败日志继续保留。较早的 `final-checks-stable-01/summary.json` 记录以下结果：该轮可选 Pi 检查有 1 项失败，所以统一结果仍为失败。它不替代最终 `final-suite-03/`。

- `cargo fmt --all -- --check`：`format.log`，通过。
- `cargo check --workspace --locked`：`check.log`，通过。
- `cargo test --workspace --locked --no-fail-fast`：`tests.log`，2356 项通过、0 项失败、9 项跳过；统计见 `tests-summary.json`。
- `cargo check -p cayw-runtime --all-features --all-targets --locked`：`runtime-features.log`，通过。
- `cargo +1.95.0 clippy --workspace --all-targets --locked -- -D warnings`：`clippy.log`，通过。
- `cargo test -p cayw-runtime --features fault-injection --test result_sources --locked`：`result-faults.log`，12 项通过。
- `cargo run -p cayw-agent --example offline --locked`：`offline.log`，工具返回 5，保留 5 条消息，运行状态结束。
- 可选 Pi 四目标：`pi-runtime.log`，20 项通过、1 项失败。唯一失败是 npm 扩展的认证 IPC 调用返回 `cell still requires wait`。已定位为测试固定模型在执行单元仍存活时直接回答，缺少协议要求的 `wait`。测试已修正为按真实状态继续等待，生产生命周期检查保持严格。`final-pi-wait/manifest.json` 记录修复前显式暂停可稳定复现，修复后 Pi 四目标 21 项、默认配置 10 项及该 npm 场景重复 5 轮通过；定向 Clippy、格式和差异检查通过。该次只改 `tests/serve_config.rs`，前后摘要一致。后续 Runtime 修改仍需核对当前代码。

此前的补充验证还包括：

- `cargo test -p cayw-runtime --features pi-compat --test children --test collaboration --test request_host_call_budget --test serve_config --locked`：`final-runtime-catalog-tests4.log`，36 项通过。
- `cargo test -p cayw-runtime --lib background --locked` 与 `cargo test -p cayw-runtime --test background --test f20_background --locked`：后台最终收据记录 12 项单元、8 项集成和 1 项 F20 测试通过；恢复核对包括真实本机进程。
- `cargo test -p cayw-pi-compat --locked`：原实例、取消、参数错误恢复及固定 npm hello 扩展。
- `cargo test -p cayw-runtime --features pi-compat --test serve_config --test provider_config --test pi_compat_runtime --test pi_mcp_combo --locked`：20 项通过，包括真实 npm 扩展经 CLI、认证 IPC 与 Host 的调用。
- `cargo test -p cayw-host --test output_filter --locked`：5 项通过，包括原文存放错误保留未知与停止标记。
- `cargo +1.95.0 clippy -p cayw-plugins -p cayw-pi-compat --all-targets --locked -- -D warnings`。
- `cargo run -p cayw-agent --example offline --locked`：工具计算返回 5，最终保留 5 条消息且运行状态结束。
- `final-frontend-01/summary.json`：显式六个 WebUI 前端文件 100 项、Editor 源码操作 35 项通过，均无跳过；这是 Node 测试，真实浏览器流程另有记录。
- `cargo build -p cayw-remote --example device --locked` 后执行 `cargo test -p cayw-remote --test binaries --locked -- --ignored`：显式补跑 1 项本机 loopback 进程测试通过；验证真实 server/device 程序、取消与重连，不代表两台设备验收。

完整工作区测试首轮 `final-workspace-tests1.log` 已结束：2350 项通过、1 项失败、9 项跳过。唯一失败是 `result_sources::produced_handles_keep_encodings_separate_and_released_filtered_originals_stay_released`，读取共享结果时返回内容库忙。该失败已定位并修复，随后冻结源码的完整工作区测试通过；首轮仍保留为失败记录。9 项跳过中，8 项是由父测试显式启动的子进程辅助入口，1 项是需要另行构建示例并显式运行的远程二进制验证，随后已按要求在本机单独补跑通过。共享结果组随后在冻结源码下重复出现相同忙错误，并用握手控制的子进程回归确定根因：其他线程启动子进程时，子进程在执行新程序前继承了事务锁文件的打开描述。原事务只关闭父进程的文件描述符，继承者仍可暂时保留锁。修复由成功获取锁的事务守卫在结束时主动解锁，不增加写入重试，也不改变未知写入结果的语义。确定性回归同时检查真实事务期间仍返回忙，以及事务完成后在子进程仍暂停时可以写入。原实现稳定失败的记录为 `results-fork-lock-before2.log`。Trace 首页跨端口导航原先返回 403，已限定允许用户点击打开的无查询首页导航，接口的 Host、Origin、来源与 Bearer 校验仍保留。真实 HTTP 回归 8 项通过。

浏览器完整重跑 `final-workbench/run-09/summary.json` 已通过 6 组断言。锁修复后重新构建四个服务，`final-workbench/run-10/summary.json` 再次通过全部 6 组断言。它实际启动 Runtime、WebUI、Editor 和 Trace，完成 Session 创建、计划旧批注拒绝与重新审阅、用量和产物读取、Editor 独立登录读取磁盘项目，以及 Trace 独立登录读取真实 Session、完成状态和 Host 调用。run-10 使用 Chrome `156.0.8078.48`、2 次本机固定模型响应、0 次付费模型请求。浏览器没有脚本错误，四个服务均以退出码 0 正常结束。选定源码和四个程序的摘要在浏览器运行前后未变化；这只绑定脚本列出的文件与程序，不能作为全仓构建输入收据。失败的 run-01 至 run-08 记录保留。Trace 没有运行图 manifest 的项目如实显示图缺口，不声称该次验证了节点图。

F20 最终运行 `f20-stable-01/summary.json` 已通过全部 10 个要求场景：会话重开与子任务隔离、内容重开、进程清理与结果重开、结果读取与释放、后台归属、MCP 资源、stdio 进程清理、空闲回收、子任务与 MCP 组合，以及 SDK 与认证 IPC 对照。没有失败、跳过或缺失场景。前后源码收据一致，包含当时未跟踪的测试和浏览器脚本；日志摘要另经核对。它证明这些实际组合场景，不等于全部 F01–F20 Issue 验收。

锁修复补充收据为 `final-results/manifest-lock-fix.json`。其中内容库全功能测试 58 项、Runtime 结果与权限测试 28 项通过。结果来源测试按默认并行方式连续运行 20 轮，每轮 12 项，全部通过；这 240 次是重复验证，不是 240 个不同场景。两条定向 Clippy、格式和差异检查通过。主线已核对收据中的 9 个源码摘要、全部日志摘要及前后收据一致。旧 `final-results/manifest.json` 只保留作锁修复前的历史记录。

最终复核修复了自动外置的未知提交缺口。普通调用和共享读取都只外置一次。Core 和 Cells 保存原 Host 成功值；交给 Luau 的引用单独保存。正文与元数据写入前，Runtime 持久记录原调用、完整执行身份、Task、Device、引用、摘要和字节数。存放已知失败返回原值；存放未知和缺失终态保留独立核对义务，阻止当前及重开后的继续、恢复和 Fork。宿主使用 `pending_result_storage`、`inspect_result_storage` 与 `reconcile_result_storage` 核对原事实、原进程清理、实际内容和独立证据；核对同步当前目录，不重放原工具或存放，不覆盖原终态。真实文件副作用一次、共享读取一次、目录同步失败和实际子进程退出的回归均通过；核对后可以恢复、重开并显式执行新任务。`final-results/manifest-externalization.json` 记录 Runtime 定向46项、Content58项、Cells95项和Core等待归属8项通过；Cells的3项跳过由父测试显式调用。主线对11个源码、5份构建输入和其余日志收据的25处摘要核对全部匹配。

F18 入口的 `final-f18-entry/receipt.json` 已完成。两个脚本稳定后，React/Vue 合成工程副本的构建、HTTP 打开、编辑、Host 保存和新进程重开通过，框架源文件测试 35 项通过。两种工程各有 1 条已知成功的 `fs.write`，未保存草稿没有写盘。故意启动失败、启动超时和 SIGINT 中断均留下预期失败或中断结果，所有记录均完成原进程清理。Shell 入口不再自动加载模型凭据。本次使用已有二进制，没有重建当前 Rust 源码；使用重建程序的结果另记。输入带 `FIXTURE.md`，不作为外部成熟仓库或浏览器验证证据。

自动外置修复后，`final-workbench-build-02/` 重建 Runtime、WebUI、Editor 和 Trace，`final-workbench/run-11/` 的6组真实 Chrome 流程全部通过。无浏览器脚本错误，四服务均正常退出0。随后使用同一重建 Editor 验证 React/Vue 构建、草稿未写盘、各一次Host保存、新进程和新状态目录重开，全部通过，清理完成。构建和运行期间代码摘要一致。`receipt-verification.json` 已独立核对这些日志、程序和结果。Trace没有运行图manifest，仍只证明会话与调用观察。

`final-suite-02/verified-summary.json` 保留另一轮完整检查的失败：默认工作区2354项通过、2项失败、9项跳过。两项失败都在后台锁释放后仍返回忙；同轮Pi21项、结果故障36项、Content58项以及格式、编译、Clippy和离线示例通过，F20的10个场景全部通过。真实TUI在成功工具调用后被下一轮模型预算检查拒绝。该轮743个Cargo、crates和scripts文件前后完全相同。随后用握手控制子进程 fork 至 exec 窗口，稳定复现继承者暂时保留后台锁；只在成功取锁后创建锁守卫，最后持有者主动解锁。`final-background-lock/` 保存原失败及修复后的回归、Runtime 单元 103 项、后台 8 项和 F20 后台 1 项、Clippy 通过。运行中和持久化失败时原持有者继续保留锁；独立只读复核未发现提前释放问题。这些结果不改原轮失败结论。

TUI 根因是固定测试模型成功结束时未报告用量。真实 Core 记录显示上轮用量未知，下一轮因未结算预算被拒绝。只改共享测试 Provider 的成功响应与终端证据脚本：成功响应报告合成输入 2、输出 2、总量 4；取消仍保留未知用量，生产预算规则保持。`final-tui-repair/` 记录 TUI 包 81 项和真实 tmux 20 个阶段通过，首任务两轮共结算 8、预留 0、预算确定，价格仍未知。原宿主和唯一 tmux 均清理。最终 `final-suite-03/tui/` 再次重建并完成实际交互，原终端记录、源码和进程退出已核对。

最后两处修复后，`final-workbench-build-03/` 重新构建四服务，`final-workbench/run-12/` 的 6 组真实 Chrome 流程全部通过。浏览器无脚本错误，四服务均以退出码 0 正常结束。同一重建 Editor 的 React/Vue 构建、草稿未写盘、各一次已知成功的 Host 保存，以及新进程和新状态目录重开全部通过。构建和运行期间代码未变，原日志、源码、程序和清理收据已核对。React/Vue 输入仍是带 `FIXTURE.md` 的合成工程；Trace 没有运行图 manifest，此次不证明图编辑或局部重算。

`final-repair-code-delta.json` 对照失败轮与最终轮输入：只有后台锁两份生产文件和 TUI 两份测试文件变化，其余 739 份代码相同。最终 `final-suite-03/verified-summary.json`、四服务收据和完整代码前后对照均通过。

## 验证边界

模型响应与用量来自本地固定响应和合成输入。本轮没有真实付费模型调用。进程、Luau、MCP stdio、SQLite、Host 和 IPC 在对应测试中实际执行。

Pi npm 证据仅覆盖 `@earendil-works/pi-coding-agent@0.84.1` 发布的未修改 `hello.ts` 工具切片。它不证明完整 Pi ExtensionAPI 或任意第三方扩展兼容。内置 fixture 和自制样例另作协议测试。

文件符号能力是轻量大纲和显式挂载，不是完整 LSP 或跨文件语义索引。共享 bridge 只在同进程内共享。F17 使用捕获响应和可求值节点重算，不执行真实 Host 写操作，也不代表 Rust 新二进制已经运行。

F20 脚本仅证明其列出的组合场景。F18 的历史记录仍绑定各自源码与平台。源码变换、HTTP 接口和浏览器交互分别记录，不能互相替代。

Rust 元数据核对了 376 个外部包的许可证声明，npm 锁文件核对了 Pi 的 239 个包和 Editor 的 77 个包；均没有缺失声明。四个 Pi 直接固定依赖的安装包声明为 MIT。这是依赖声明核对，不是完整的源码和 notice 审计。安装时 npm 报告 Pi 4 项、Editor 2 项已知依赖问题，未自动升级或改写锁文件，仍需单独处理依赖更新。
