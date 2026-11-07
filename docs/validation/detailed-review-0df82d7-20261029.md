# 0df82d7 详细审查修复与验证：2026-10-29

用户提供的 61 项审查发现已完成核实、修复或既有实现复核，并完成最终源码上的检查。最终逐项结果为 **61 项通过、0 项待处理**。其中 P1 为 5 项，P2 为 32 项，P3 为 24 项。本结论只覆盖原报告列出的发现及本机实际场景，不表示全部 F01–F20 或全部 GitHub Issue 完成。

原报告基于提交 `0df82d7035a0e12895106a29f6bbab599ff2aef3`，只做了静态审查。本轮使用分支 `integrate/f01-f20-20261027`，修改尚未提交。实际平台为 macOS arm64、Darwin 27.0.0，工具链为 Rust 1.95.0 和 Node v24.4.1。其他平台、架构和未执行场景不从本记录外推。

[61 项修后行为与逐项证据](detailed-review-0df82d7-items-20261029.md)按原报告顺序列出全部编号。完整结果清单为 `.local/integration-validation/finish-search-rust-20261029/detailed-review-final-02/manifest.json`，SHA-256 为 `e46ed38f3786463793eb5b7824e1de7bd81c27e965c44217fd6e963ab1d052d0`。

## 输入与证据

以下证据路径均相对于 `.local/integration-validation/finish-search-rust-20261029/`。每轮命令保留参数、退出码、stdout、stderr、日志哈希和运行前后源码哈希。旧失败记录保留，未被后续成功覆盖。最终清单记录 910 个源码与文档文件，保留 61 份先前命令收据作为历史；历史结果不参与本轮最终通过判断。

原始审查有 61 项发现，逐文件覆盖表有 888 条记录。原始文件的 SHA-256 为：

| 文件 | SHA-256 |
| --- | --- |
| CAYW_问题与建议.csv | `2888b60f3436419896ada70d7128309b61bf410365090843404a7aca56133e57` |
| CAYW_逐文件审查覆盖.csv | `c05ddb94f2b47ffcf3f7af26fd3aac99974c342a8bf7b4c202957260b5295fa1` |
| CAYW_整合分支详细代码审查.docx | `7d546bdf2ca25a7a537e56a29accaa1a7275174b13e7e92a87325e88a61bad1e` |

解析结果位于 `supplied-review-01/`。下载内容只作为待核实的数据，没有执行其中的命令或代码。优先级统计采用原字段开头的 P1/P2/P3；逐项表保留原字段中的范围说明。

61 项按产品文件归属分为 25 组，由实施代理与独立只读复核代理并行处理。主线负责整合与实际编译、测试。AD-P03 同时保留 Vision 与 Web Access 两组归属。最后的 Artifact 修法与八处 Clippy 写法调整另有定向复核，没有覆盖旧发现记录。

`detailed-review-collection-11/collection.json` 保存实施、最新独立复核及历史引用。`detailed-review-inventory-final-02/verification.json` 核对原报告、映射和最终汇总的 61 个唯一编号及顺序，全部一致。静态复核没有代替实际运行。

## 修复后的行为

五项 P1 的结果如下：

- **AD-N07：** 图像 HTTP 后端在读取 stdout/stderr 时执行字节上限。未结束的响应一旦超限，就停止并回收子进程，不再先把整个响应收进内存再检查。
- **MC-01：** Agent 批次中首个结果未知的工具会阻止后续未启动工具执行。已启动工具进入取消和清理流程，全部调用仍有配对记录，下一模型轮保持阻断。
- **RT-01：** Artifact 保留结果未知状态与原调用身份。可信核对记录先落盘，再恢复精确匹配的 Host 待核对写入，最后更新本地动作状态。双 Runtime 场景可以恢复原 Session，新未知结果仍会再次阻断。没有 Tokio 环境的普通线程遇到锁争用时返回 `Busy`，不再崩溃。
- **RT-13：** 子 Agent 的查看、停止和插入消息统一经过 Host 的授权、调用开始和 Request 检查。测试核对实际子任务状态与消息消费。
- **WB-01：** 默认 `ownership.json` 的空父路径按当前目录处理。实际绑定成功后可以由新实例读回；真实写盘错误继续向上传递。

其他发现覆盖以下行为：

- Artifact follow-up 使用运行时当前的取消身份；恢复 bundle 在所有部分验证通过后才更新。结果存储在正文已提交后，只重试尚未提交的元数据。长行搜索片段包含实际命中位置。
- 后台任务跨 Runtime 取消核对原执行者，并持久化取消请求。清理完成的任务时，同时清理进程、执行者和清理确认记录。运行中和结果未知的任务仍被保留。
- Core 将 Provider 元数据纳入原有叶子限额，在序列化期间执行字节上限，并减少同一不可变记录的重复校验。流式事件与会话投影仍在同一事务中落盘。
- Provider 不再构造后丢弃同一请求内容。Responses 保留合法 token 计数详情，复用已有参数解析。Sources 的连接和出站帧只编码一次，Remote 和游标文件在读取或编码期间执行上限。
- OCR 未配置或配置错误时保留实际不可用原因。图像 handle 通过真实 Host 参数检查与 Luau 调用使用。图片输入先计算完整字节预算，再解码。
- Fleet 分页不先复制全部历史成员。问题工具区分 Task 与 Session 的身份。Plannotator 根据事件游标读取另一 Runtime 新提交的反馈。
- WebUI 明确显示读取错误；切换会话时更新面板归属并拒绝迟到响应。面板替换先验证后提交。TUI 对同一会话快照只解析一次，并接受核对后的权威状态。
- 编辑器保留 React 数值样式，正常编辑测试使用真实 companion。布局下拉读取真实计算样式。审计日志按有界行读取。Trace、HTML 和 framework 删除同一调用链中的重复解析。
- Luau `coroutine.wrap` 保留尾部 `nil` 的返回个数。Cells 与验收工具保留独立入口检查，删除同一不可变数据的重复处理。验收包核对场景和重复次数是否完整。

AD-P02、RT-03、AD-P01、RT-10 在对应实施报告中属于既有实现。本轮保留这些实现，并将其回归检查绑定到最终源码，没有把它们写成新增功能。

## 最终实际检查

最终命令结果来自 `detailed-review-final-bundle-06/verification.json`。各范围分开计数；同一测试可能在不同配置中重复运行，不将下表相加为不同测试总数。

| 范围 | 实际结果 | 原始收据 |
| --- | --- | --- |
| 全仓库格式、所有目标编译、Clippy、离线示例和差异空白检查 | 六个命令全部退出 0 | `detailed-review-workspace-03/verification.json` |
| 全仓库测试 | 2641 项通过、0 项失败、11 个默认未执行入口 | 同上，`test.stdout` |
| Runtime 定向回归，启用 `plugins,pi-compat` | 193 项通过、0 项失败 | `detailed-review-runtime-repairs-05/verification.json` |
| Runtime 特性编译 | `plugins,pi-compat` 所有目标编译通过 | `detailed-review-features-03/verification.json` |
| 插件执行、服务和生命周期，启用 `plugins,strongflow` | 14 项通过、0 项失败 | 同上，`runtime-plugins.stdout` |
| Pi 兼容集成 | 2 项通过、0 项失败 | 同上，`runtime-pi.stdout` |
| 独立 headless-client | 格式、所有目标编译、测试、Clippy 全通过；41 项测试通过 | `detailed-review-client-06/verification.json` |
| WebUI Node | 90 项通过、0 项失败 | `detailed-review-node-07/verification.json` |
| 编辑器 Node | 42 项通过、0 项失败 | 同上 |
| Remote 真实二进制本机回环 | device 示例构建通过；单独执行的回环测试 1 项通过 | `detailed-review-remote-loopback-04/verification.json` |

最终全仓库使用以下命令。完整参数以留存的 `*.command.json` 为准：

```sh
cargo +1.95.0 fmt --all -- --check
cargo +1.95.0 check --workspace --all-targets --locked --keep-going
cargo +1.95.0 test --workspace --locked --no-fail-fast
cargo +1.95.0 clippy --workspace --all-targets --locked -- -D warnings
cargo +1.95.0 run -p cayw-agent --example offline --locked
git diff --check
```

bundle05 在完成格式、193 项定向测试和全仓库检查后，因脚本引用不存在的 `--test plugins` 而停止。该失败发生在测试启动前。脚本改为真实目标 `plugin_execution`，并启用其需要的 `strongflow`，避免只选到零测试目标。bundle06 先核对原成功收据、每条命令的退出码、日志哈希及完整源码清单，再复用这三个已完成阶段，执行剩余检查。没有重写 bundle05 的失败结果，没有把复用写成重新运行。

最终结果清单再次核对七个必需检查范围、21 条命令、当前源码、日志、浏览器相关输入、最新独立复核和全部 61 项证据。格式化与 193 项定向测试直接纳入清单，不只作为历史引用。`detailed-review-resume-binding-01/verification.json` 又逐条核对三个复用阶段的原命令参数、标签、数量、工作目录、日志和代码，并单列文档变化；该核对没有运行产品命令。汇总期间代码没有变化。最终文档生成后的源码检查单独记录于 `detailed-review-final-report-01/verification.json`。

## 11 个默认未执行入口

`detailed-review-ignored-final-01/verification.json` 将最终全仓库输出与真实调用关系逐条核对。这 11 个入口本身都没有计入 2641 项通过：

- 8 个是父测试启动的子进程辅助入口。它们对应的父测试在最终全仓库运行中全部通过。
- 1 个是 `server_binary_device_example_and_operator_commands_work_on_loopback`。构建 device 示例后，已单独使用 `--ignored --exact` 实际运行并通过，结果单独计数。
- 2 个需要显式输入，本轮未执行：`complete_capture_preserves_stream_and_accounting` 需要 `CAYW_REPLAY_CAPTURE` 与新的 `CAYW_REPLAY_OUTPUT`；`local_stream_stages` 需要 `CAYW_STREAM_FIXTURE` 与新的 `CAYW_STREAM_OUTPUT`。两者没有被改记为通过。

## 浏览器验证

真实 Chrome 版本为 `156.0.8078.48`。使用本机现有 Playwright Core 与 Chrome，没有联网安装依赖。环境没有可用的 Playwright MCP，因此本记录不声称通过 MCP 执行。

编辑器完整链使用真实 Rust 二进制、companion 和 Vite，对 React、Vue 各执行编辑、重启、草稿冲突、保存冲突，共 8 个场景，全部通过。原始记录为 `browser-review/acceptance-run-02/summary.json`。留存二进制 SHA-256 为 `f5930d9492dd27afb7f19c3baa55ffb560864a72467a3a916b2daf62e61a5acc`。

WebUI 使用真实 Chrome、生产页面资源和受控本机 RPC，覆盖局部读取错误、403 断连、会话切换、面板归属和迟到响应。记录为 `webui-browser-review/result.json`。这部分只证明页面行为；Rust 网关由对应集成测试验证。

`detailed-review-browser-binding-04/verification.json` 绑定已有浏览器结果、留存二进制及 25 个相关输入。完整链的 22 个输入另留存在 `detailed-review-browser-inputs-01/`。最终汇总重新核对相关源码与日志哈希，全部一致。这是已有运行与当前相关输入的核对，没有声称最后一轮重新运行浏览器。首次完整链因输入目录不存在而失败的记录仍保留。

## 实际失败与补修记录

第一轮全仓库测试为 2629 项通过、10 项失败、11 个默认未执行入口，见 `detailed-review-workspace-01/verification.json`。失败涉及结果存储、Artifact 取消、Fleet、图片预算、用量 fixture 和插件测试。该轮在测试失败后停止，没有执行后续 Clippy 和示例。

结果存储测试改用每次操作独有且只执行一次的测试钩子，真实持有锁验证正文提交后只重试元数据。图片预算按实际字段长度计算。用量 fixture 保留中途已报告的观察，但最终错误独立标为未知。插件成功场景补齐注册表自身的精确授权；无效参数在 CodeMode 准入前拒绝，测试不再声称其进入 Host。

Fleet 测试原先挂起子模型请求，占用共享预算。修正后子模型先正常结算，子任务停在真实问题工具边界。分页测试逐个完成三个子任务；停止测试逐个确认 Running 到 Cancelled、无活动 Attempt 和未回答问题，再验证重复停止。没有增加预算或超时，也不声称三个问题工具同时运行。

Artifact 的实际诊断见 `detailed-review-artifact-diagnostic-02/verification.json`。第一次文件写入成功，但 10 字节结果预算使响应成为未知，Host 正确保留待核对写入。旧核对流程只追加 Artifact 决定，没有恢复这条 Host 记录，所以第二次写入仍被第一条阻断。修复按原调用 ID、工具和参数摘要恢复精确记录，顺序为“核对决定落盘 → Host 恢复 → 本地动作更新”。重试只补恢复，不重放原工具。测试验证原 Session 完成、第二次真实文件效果、新未知再次阻断，以及两个写调用各只启动一次。

独立第 11 轮又发现普通同步线程遇到 Host 锁争用时缺少 Tokio 环境。`detailed-review-artifact-sync-repro-01/verification.json` 实际复现 19 项通过、1 项失败，错误为 `there is no reactor running`。修复复用已有专用 Host 线程，在其运行环境内只轮询一次。`detailed-review-artifact-sync-fixed-01/verification.json` 的 32 项随后通过；最终定向及全仓库检查覆盖最后源码。第 11 轮发现仍完整保留，第 12 轮只追加修法复核。

第二轮全仓库测试已有 2641 项通过、0 项失败，但 Clippy 因内部返回类型过于复杂而失败，见 `detailed-review-workspace-02/verification.json`。后续补修使用内部类型别名，合并等价条件，并调整测试配置初始化。没有屏蔽 lint。多轮原失败保留；最终全仓库 Clippy 已通过。

内容取消、Artifact 取消和崩溃恢复继续区分“请求取消”“真实停止”“副作用已核对”。未确认的结果保持未知并阻断后续执行，没有通过增加输出预算、延长超时、放宽授权或清除未知状态使检查通过。

## 可复跑入口与范围

统一驱动为 `run-detailed-review-checks.py`。每次 `--name` 使用新的目录。主要 phase 为 `workspace`、`runtime-repairs`、`features`、`client`、`node`、`remote-loopback`。例如：

```sh
python3 -I .local/integration-validation/finish-search-rust-20261029/run-detailed-review-checks.py --name rerun-workspace-01 --phase workspace
python3 -I .local/integration-validation/finish-search-rust-20261029/run-detailed-review-checks.py --name rerun-features-01 --phase features
```

驱动限制 Cargo 编译并发为 2，移除本轮测试中的真实 Vision 后端环境配置，只记录变量名，不记录凭据。计数收据只核对已有日志中的数量和哈希，不作为产品测试成功的替代证据。

本轮新增真实模型和付费服务调用为 **0**。模型行为使用 fixture，本机文件、SQLite、Host、进程、文件锁、HTTP 和二进制使用真实组件。没有进行性能基准测量；删除重复处理的结论来自源码复核和功能回归，不表示已测得固定加速比例。早前搜索与原 Rust Session 恢复的真实模型证据见 [搜索与 Rust 恢复记录](f01-f20-search-rust-20261029.md)，它不替代本轮源码检查。

本轮没有修改原 Rust Session 数据库，没有将未知模型用量记为零，也没有释放原未知预留。没有更改许可证、仓库权限、网站或外部自动化，没有提交、推送或发布。
