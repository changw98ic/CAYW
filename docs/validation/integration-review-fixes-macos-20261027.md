# 集成复核四项修复：macOS arm64

本记录覆盖用户指出的网页响应大小、产物取消、网关未知结果分类和上下文逐轮展示四项问题。基底提交为 `17656badb4e96c7e440eb4a013b9684cffcb24c2`，分支为 `integrate/f01-f20-20261027`。修复后的源码尚未提交或推送；下述摘要绑定本次实际工作树。

## 源码、环境与证据

- 平台：macOS 27.0.1，arm64，Darwin 27.0.0。
- Rust 1.95.0，Node 24.4.1，Chrome 156.0.8078.48。
- 本地证据目录：`/tmp/cayw-review-fixes-20261027/`。
- 最终联合检查：`checks-02/`；最终重建浏览器：`browser-after-03/`。
- 两组运行前后代码一致，且使用同一份源码。已跟踪代码补丁 SHA-256 为 `fd5b43d41c12a6a04680813a0a58095f0d7e2eec9916adaf1329b50ab4343c9b`。新增 `crates/cayw-web-access/tests/stream_limits.rs` 的 SHA-256 为 `61ccc2612541678fcdcf6f861b3c99eba584aa0c01f2bb612771d28f77b8a515`。
- `verification-02.json` 核对了9份命令日志、当前源码、实际页面返回、四个程序摘要和四份退出记录。`FINAL-RESULT-02.json` 记录解析自原始日志的数量。完整补丁另含本验证文件；文档最终补充不改变上述代码摘要。

## 根因与修复

### 网页响应在读取时落实上限

原通用 HTTP 和 Brave 路径先调用 `response.bytes()` 完整分配正文，再检查大小。响应没有结束时，读取阶段不会执行已有的字节上限。

两条路径现在共用 `read_bounded_body`：声明长度已超限时先拒绝；实际正文通过 `response.chunk()` 逐块累计，超限块不追加到结果缓冲，释放响应并返回 `BodyTooLarge`。保留超时和网络错误分类，不增加重试或依赖。此修复限制正文累计，不声称进程总内存严格等于 `max_bytes`；底层传输块、缓冲和分配容量也占内存。

回归使用本机 HTTP，覆盖分块、缺失长度、冲突长度、声明超限、刚好边界及请求取消。超限服务端保持未结束，客户端返回后才由通道释放并回收线程。取消测试在服务端确认请求到达后丢弃原 Future，并断言原连接关闭。时间只作宽裕防挂上限，不使用固定毫秒性能断言。Brave 的本机端点注入位于包内测试，没有新增公开配置接口。

### 产物工具执行与取消分开取锁

原 Runtime 在 `ActionProtocol::dispatch` 执行 Host 工具期间持有全局产物锁，取消入口也等待这把锁。现在短暂取锁获得同一个共享动作协议，执行与审计在外层锁之外进行。回归通过受控 Host 的启动信号确认工具已经运行，再检查无关产物读写和原取消令牌。

释放总锁后，还复现了旧快照删掉在途动作的问题。`ActionProtocol` 现在用进程内计数跟踪尚未返回的原 dispatch 和 cancel 调用。恢复快照时保留这些原动作记录，包括同名冲突、“已取消但派发桥尚未返回”和“派发已返回但取消桥尚未返回”的区间。两类调用均退出后释放登记；正常恢复仍可替换已结束记录。快照仍导出 `Accepted`，不通过丢弃未结算动作求一致。取消只向原执行发信号，不代表外部副作用回滚。

Runtime 取消由 `cancel_for_session` 在同一动作锁内检查归属并固定原记录，避免恢复恰好发生在先读取归属、后取消的中间。`HeldDispatch` 的两项通道测试分别暂停派发桥和取消桥返回，覆盖双向交错、同名冲突、迟到结果、错误会话拒绝以及全部返回后的正常替换。真实 Runtime 回归另行覆盖原 Host、精确授权、取消令牌、审计和恢复。

### 网关保留是否可能已执行的判断

副作用分类补齐六个产物变更方法：`artifacts.put`、`artifacts.update`、`artifacts.action`、`artifacts.action_cancel`、`artifacts.recovery_save`、`artifacts.recovery_load`。同一次分类核对还补齐 `runtime.background.kill`、`runtime.background.recover` 和 `plannotator.submit`。

网关保留 IPC 的 `outcome_unknown` 判断。连接准入前的已知拒绝不能仅因错误码为 `connection_closed` 被改成未知。已准入写操作在超时、断连、写失败或协议损坏后丢失回执时保持未知。只读操作的传输失败不引入副作用未知；Runtime 直接报告的领域未知仍完整保留。

HTTP/IPC 回归用后端信号确认请求确已到达，再检查超时或断连结果。该后端为透明网关夹具，不声称它执行了真实产物业务写入；真实产物派发由 Runtime 回归覆盖。

### 逐轮展示读取实际 usage 字段

原页面读取不存在的 `input_tokens`、`output_tokens` 和 `delta_tokens`，所以每轮显示0。现读取 `evolution[].usage.input`、`output` 和 `total_tokens`，没有保留旧字段别名。总量卡继续读取顶层 `tokens`。

真实浏览器使用两轮不同的本机固定用量：12输入、7输出、总量19；17输入、11输出、总量28。页面完整文字与 Runtime 数据逐项一致，总量47，费用仍为 `unknown`。

## 实际命令与结果

以下命令均退出0。完整参数、开始与结束时间、日志摘要见 `checks-02/summary.json` 和 `browser-after-03/summary.json`。

| 命令 | 实际结果 |
| --- | --- |
| `cargo fmt --all -- --check` | 通过 |
| `cargo check --workspace --locked` | 通过 |
| `cargo test -p cayw-web-access -p cayw-artifacts -p cayw-webui --locked --no-fail-fast` | 89通过，0失败，0跳过 |
| `cargo test -p cayw-runtime --test artifact_dispatch --test f20_combo_smoke --test integrated_ipc --locked` | 20通过，0失败，0跳过；其中派发12、组合7、IPC1 |
| `cargo +1.95.0 clippy --workspace --all-targets --locked -- -D warnings` | 通过 |
| 六份 WebUI Node 测试 | 100通过，0失败、取消、跳过或待办 |
| `git diff --check HEAD` | 通过 |
| 四服务 `cargo build --bins --locked` | 通过 |
| `scripts/validate-workbench.mjs` | 六组实际浏览器流程通过，无页面脚本错误，四服务均退出0 |

Node 命令为：

```sh
node --test crates/cayw-webui/tests/core-protocol.mjs crates/cayw-webui/tests/dom.test.mjs crates/cayw-webui/tests/state.test.mjs crates/cayw-webui/tests/ui.test.mjs crates/cayw-webui/tests/workbench.test.mjs crates/cayw-webui/tests/workbench_dom.test.mjs
```

浏览器复跑命令为：

```sh
cargo build -p cayw-runtime -p cayw-webui -p cayw-visual-editor -p cayw-trace-debug --bins --locked
CAYW_PLAYWRIGHT_MODULE=/absolute/path/to/playwright/index.mjs node scripts/validate-workbench.mjs --bin-dir /absolute/target/debug --out /new/evidence/browser
```

当前环境没有 Playwright MCP，本次使用本机 Playwright 与 Chrome。浏览器执行了真实 Runtime、SQLite、IPC、HTTP、Luau、Host 文件写入与审批，以及 Editor 和 Trace 的独立登录。模型响应和用量是本机固定输入。没有付费模型调用或真实 Brave 请求。

## 原失败与中途结果

- `web-body/regression_unpatched.log`：原网页实现1通过、4失败，超限读取等不到结束或错误分类不符。
- `artifact-cancel/original_failure.txt`：原锁实现使无关产物操作等待超时。
- `gateway-effects/pre-fix-test-failure.txt`：原产物写请求超时错误返回 `outcome_unknown=false`。
- `browser-before/receipt.json`：确认原四程序摘要与旧构建一致。新断言抓到原 UI 第一轮显示0，浏览器退出1，四服务退出0。原失败发生在单独保存 context JSON 之前，因此该轮没有独立 `context-overview.json`，不将它列为留存文件。
- `artifact-mainline-precheck-failed.log`：主线一次定向测试11通过、1失败。测试误将 ExternalHost 的 release 信号当作取消后仍未返回的证明；真实 Host 取消可丢弃该 Future。精确交错后改用 HeldDispatch 桥测试，真实 Runtime 回归保留原令牌和审计检查。最终联合结果均通过。
- `checks/`、`browser-after/` 和 `browser-after-02/` 是较早的通过结果。最后只读复核发现取消桥尚未返回的逆向窗口，补充计数保护和第二项桥测试后，重新执行 `checks-02/` 和 `browser-after-03/`，均通过。较早结果保留，不覆盖。

本记录不把本次测试数量解释为独立功能数，也不把旧集成的2357项全工作区结果外推到新修复。其他 macOS 架构、真实外网搜索和真实付费模型场景未在本轮验证。
