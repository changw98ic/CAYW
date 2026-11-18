# 会话排队、产物恢复和长历史读取验收

平台为 macOS arm64，Rust 1.95.0。真实模型使用本机 `127.0.0.1:3425/v1` 和 `opencode-go/deepseek-v4.1-flash`。验收目录为 `/Volumes/ORICO/cayw-dev-review-weoz_9qh/phase2`。各次运行的 `invocation.json` 或二进制摘要绑定实际构建。

## 真实模型

- 空目录整题生成了可运行的团队协作任务台。模型末次 `test.run` 通过 23 项。宿主在源码副本上独立运行 `npm test`，也通过 23 项。浏览器以演示成员提交任务，收到实时进度和 200,000 行的完成结果。可视化编辑器修改标题、保存、撤销并再次保存后，HTML 摘要恢复到修改前。记录见 `fresh-node-tests.log`、`fresh-browser-verification.json` 和 `fresh-model-taskdesk.tar.gz`。
- 整题 Trace 中 32,213 个游标连续。56 次工具调用、44 次执行、67 次能力调用及 9 次审批均有终态。3 次代码测试失败由模型在后续修改中处理，1 次文件写入能力失败保留在原始记录。执行耗时 1,305.113 秒，57 次模型请求均有终结消息。服务报告 185,019 输入、130,180 输出和 5,734,400 缓存读取 token。记录见 `chain-audit.json` 和 `real-model/evidence/product.events.json`。
- 模型创建、更新原生卡片后，Client 重启读回同一版本与正文。随后在原题会话记录版本绑定批注，经真实模型后续任务读取批注并更新原交付卡。重复提交同一请求编号返回同一任务。浏览器打开版本 2 并收到验证标记。记录见 `real-model/evidence/artifact-reopen-and-queue.json`、`feedback-result.json` 和 `fresh-browser-verification.json`。
- 另一隔离工作区使用较新 Runtime 复测。真实模型自动允许和明确拒绝均有审批回执。十个后续任务全部接收，当时两项占用名额、八项排队，最终十项各完成一次。取消与重启恢复记录显示原始写入一次，恢复步骤没有重放模型请求。记录见 `final-lifecycle/results.json`、`final-lifecycle/queue-result.json` 和 `final-lifecycle/cancel-result.json`。

空目录整题、批注往返和生命周期复测使用各自记录的构建。后两项覆盖后续提交前的变更，源代码检查另记。

## 历史与性能

- 修复后的既有任务台通过 17 项 Node 测试。实际浏览器从最新 200 条继续向前读，最终显示了 555 条及最早的“已提交”。注入 HTTP 500 后，界面显示保存失败并保留原草稿。记录见 `taskdesk-browser-result.json` 和 `team-taskdesk-fixed.tar.gz`。
- 在同一份 50,695 条事件的 Trace 副本上分别使用 54 和 540 个项目。每组追加并重开 10 次。追加中位数分别为 1.29 和 1.19 毫秒，按最近秩取 P95 为 2.08 和 1.96 毫秒。两组最后均重开得到 50,705 条，空闲状态响应均为 157 字节。这是本机样本，记录见 `trace-append-benchmark.json`。
- 流式文本片段由聊天页按事件更新，空闲会话可返回空变更。消息提交、审批、执行状态转换及重新同步仍会读取权威会话快照。长会话的状态转换流量、初始峰值内存和整题模型耗时仍需独立验证改进幅度。

## 新增行为与检查

- 后续任务提交编号校验来自持久任务关联。同一编号关联两个不同输入时返回冲突。已有任务的执行并发由同一调度器负责。
- 模型产物更新使用期望版本。较旧版本提交需要先读取当前版本。会话预览隔离模型返回的 HTML，页面允许用户打开正文和查看版本。
- 新增业务数量上限、自动重试及静默回退均为无。旧显式恢复包仍由恢复入口导入。历史里缺少可验证正文的旧产物仍需核对证据。

## 源码检查

- 全工作区 Rust 测试记录 3,137 项通过、11 项忽略和 2 项 TUI 失败。TUI 修复后，Rust 定向回归 46 项全部通过，其中包括两条 TUI 用例，以及只配置一个执行名额时提问等待允许另一会话运行。
- WebUI JavaScript 测试 101 项全部通过。记录见 `workspace-tests.log`、`final-regressions.log` 和 `webui-tests.log`。
- 最终源码的 `cargo fmt --all -- --check`、`cargo check --workspace --locked` 和 `cargo clippy --workspace --all-targets --locked -- -D warnings` 均通过。
