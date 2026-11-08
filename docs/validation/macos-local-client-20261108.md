# macOS 本地唯一 Client 验收

2026-11-08（本机日期），当前未提交工作区在 macOS arm64 上完成离线回归、唯一 Client 产品流程、浏览器流程、无模型手工验收、经授权的真实模型分段验收，以及两次完整的真实浏览器 Agent E2E。本次记录所列范围通过。

本记录只证明下列源码状态、二进制、平台和输入。它不把 macOS 本地结果扩大成 Windows、Linux、云端、广泛模型质量或费用结论，也不把当前未提交状态写成已合并交付。

结构化总结果见 [JSON 记录](macos-local-client-20261108.json)。早期真实模型分段结果见 [真实模型 JSON](macos-local-client-20261108-live-model.json)。完整浏览器链路见 [真实 WebUI E2E JSON](macos-local-client-20261108-live-webui-e2e.json)、[`r5` 审批截图](macos-local-client-20261108-live-webui-r5-approval.png) 和 [`r5` 完成截图](macos-local-client-20261108-live-webui-r5-completed.png)。最终无模型页面见 [可访问性快照](macos-local-client-20261108-accessibility.md) 和 [完整页面截图](macos-local-client-20261108.png)。

## 结论

本次验证得到以下结果：

- 没有模型方案时，唯一 Client、TUI 和 WebUI 可以启动。启动不会发送模型请求。
- 没有模型方案时，WebUI 可以创建和查询 Session。
- 第一次执行任务时，服务返回 `model_not_configured` 和 `outcome_unknown: false`。这表示任务已知没有进入执行。
- WebUI 显示模型配置说明，但 Session 仍保持“就绪”。页面不会错误显示“连接结果待确认”，也不会进入待核对阻断。
- 本地首次模型配置入口是 `cayw conf`。已删除的 `cayw configure` 会被拒绝。
- 每个工作区使用自己的 `models.json`。可选 `api_key` 以普通私有文件保存；公开设置、启动信息和验收记录不回显密钥。
- 一台设备使用一个 Client。所有工作区共享 Client 的连接、重连、IPC 发现和进程监管；工作区保持独立 Runtime、SQLite、Session、模型、MCP、Pi 和架构插件状态。
- WebUI 和 TUI 是 Client 插件。工作区架构插件由 Client 按工作区启用和监管。普通架构插件不冻结普通 Session；StrongFlow 保留专用绑定合同。
- 插件清理无法确认时会保留阻断，不会盲目重启、更新、禁用或删除同名实例。
- 经当次明确授权，早期分段验收完成 7 次真实生成请求：核心 Agent 5 次、Runtime/Session SDK 1 次、认证 loopback IPC 1 次。这 7 次没有组成浏览器 E2E。
- 随后的真实浏览器验收从页面配置模型开始，完整经过 WebUI、唯一 Client、工作区 Runtime、两次真实模型生成、`execute_code`、页面审批、Host `fs.write`、最终页面答案和刷新恢复。`r4` 和 `r5` 的 8 项断言均通过。`r5` 使用完整 workspace 测试后重新构建的最终二进制。
- 浏览器验收的 `r1`、`r2`、`r3` 明确保留为失败尝试，不能算作 E2E 通过。五次浏览器尝试共发送 9 次真实生成请求。连同早期分段验收，本会话共发送 16 次真实请求；15 次已完成请求报告 18215 token，另有 1 次早期取消请求用量未知。上游费用没有独立核对，因此费用状态为未知。

## 源码与环境

| 项目 | 实际值 |
| --- | --- |
| 分支 | `workflow/macos-startup-20261020T012416Z-8f7112` |
| 基线提交 | `cb8599d0fd2e8ce73edb5545e00db57b7049a69d` |
| 远端跟踪状态 | 当前分支领先 43 个提交 |
| 工作区 | 有未提交修改和未跟踪文件；全部保留 |
| Workbench 27 文件源码清单 SHA-256 | `08ec96aea0c81edb1240b1cf12a2c7b012f0016316bdb4eed53402506f7427d5` |
| Workbench 启动时的 `git diff --binary HEAD` SHA-256 | `ec22229895bb24e84e5abebe2d1d33b5f14139c4db24b57fff50082312a2c7eb` |
| 成功真实 WebUI E2E 启动时的 `git diff --binary HEAD` SHA-256 | `a6f17a7f5f8f1aa1d9b84478c21e84592c8d2a125b810e5af743373c36c9f14e` |
| 系统 | macOS `27.0.1`，Build `26A434`，Darwin `27.0.0` |
| 架构 | `arm64` |
| Rust | `rustc 1.95.0 (59807616e 2026-04-14)`，host `aarch64-apple-darwin`，LLVM `22.1.2` |
| Cargo | `1.95.0 (f2d3ce0bd 2026-03-21)` |
| Node.js | `v24.4.1` |
| Chrome | `157.0.8103.38` |
| Playwright Core | `1.60.0` |

Workbench 在运行前后重新散列其 27 个明确源码文件和四个产品二进制。结果为 `changed_sources: []`、`changed_binaries: []`。这证明 Workbench 验收期间没有改动这些输入。

`git diff` 散列只覆盖 Git 已跟踪差异，不覆盖未跟踪文件。Workbench 的源码清单散列覆盖它明确列出的 27 个产品输入。下表另外固定本次使用的全部主要二进制。

## 二进制收据

下表是早期 `cayw-macos-local-20261108-r3` 产品验收冻结的二进制。

| 二进制 | SHA-256 |
| --- | --- |
| `target/debug/cayw` | `3fbb53eea76837a8ef332d8117822d43a495bdb49bd4e28a82dfc128d3d87969` |
| `target/debug/cayw-runtime-plugin-fixture` | `eac318300131438d2c71e55d96989fc30f2e8756091700f660c19a9c2beac3a7` |
| `target/debug/cayw-webui` | `6e17f03c30a168f147c2b5820a86ac284ae67de5041176666d4466f6467aa238` |
| `target/debug/cayw-tui` | `3987c5be58bc43a01411af802281ae6ddd7ea583148a9aa670a4040f2b7e7b27` |
| `target/debug/cayw-visual-editor` | `c237f33922e92e6bec907ed7d18f67ce6f64adba1f9c557e1e09deea6c31f7fe` |
| `target/debug/cayw-trace-debug` | `3d54f955d11d905bd4df96488ce8efaacd2f22373724af5c6b752bdfed678a7f` |
| `examples/headless-client/target/debug/cayw-client` | `c98f532dc819f48c6234589aa634855b694ef96b2c2dca25ef4d7fbca7565df6` |
| `target/debug/examples/magpie` | `1545f4ce2d706873706a554329a201e97536b612b613f6dff85d4dbfd5916e31` |

产品工作流、Workbench 和无模型手工浏览器验收使用前七个冻结产品二进制。早期真实模型分段验收随后从当前源码构建并运行 `magpie` example；其源码、二进制和日志另有 SHA-256 收据。

真实 WebUI E2E 在修复模型方案名称的 HTML `pattern` 后重新构建页面服务。`r4` 先完成一次完整 E2E。完整 workspace 测试随后重新链接了二进制，因此又按本地产品功能集构建，并用全新目录执行 `r5`。`r5` 是最终二进制证据。每次运行都在开始和结束时重新散列二进制；运行期间没有变化。

| 真实 WebUI E2E 二进制 | `r4` SHA-256 | 最终 `r5` SHA-256 |
| --- | --- | --- |
| `target/debug/cayw`（`plugins strongflow pi-compat`） | `3fbb53eea76837a8ef332d8117822d43a495bdb49bd4e28a82dfc128d3d87969` | `3fbb53eea76837a8ef332d8117822d43a495bdb49bd4e28a82dfc128d3d87969` |
| `target/debug/cayw-webui` | `fcc6645477999a608d208fe308386ece320a4a7439c7017365a93ef28f1a5ec3` | `8cab00d7cf8f0454ec55a4ec2846120ce3c5e29fa986213565f87ea437a8b511` |

## 自动测试

下列最终命令均退出 0：

| 检查 | 结果 |
| --- | --- |
| `cargo +1.95.0 test -p cayw-runtime --test provider_cli --test serve_config --locked --offline` | 16/16 通过：`provider_cli` 3 项，`serve_config` 13 项 |
| `cargo +1.95.0 test --workspace --locked` | 最终源码通过；397 个结果块累计 2722 项通过、0 失败、11 ignored |
| `cargo +1.95.0 check --workspace --locked --offline` | 通过 |
| `cargo +1.95.0 clippy --workspace --all-targets --locked --offline -- -D warnings` | 通过 |
| `cargo +1.95.0 test -p cayw-runtime --features "plugins strongflow pi-compat" --locked --offline` | 通过 |
| `cargo +1.95.0 clippy -p cayw-runtime --features "plugins strongflow pi-compat" --all-targets --locked --offline -- -D warnings` | 通过 |
| `cargo +1.95.0 fmt --all -- --check` | 通过 |
| `git diff --check` | 通过 |
| `cargo +1.95.0 run -p cayw-agent --example offline --locked --offline` | 通过；离线 Agent 得到 `2 + 3 = 5` |
| 默认 feature、独立干净 `CARGO_TARGET_DIR` 的 `tui_cli` | 1/1 通过 |
| 启用 `plugins` 的 `tui_cli` | 2/2 通过 |
| `cargo +1.95.0 test -p cayw-webui --locked --offline` | 52/52 通过 |
| WebUI 九个明确 Node 测试模块 | 107/107 通过 |
| `python3 -I scripts/test_validate_headless_chat.py` | 7/7 通过 |
| `cargo +1.95.0 run -p cayw-agent --example magpie --locked`，当前 Magpie 路由 | 4/4 真实案例通过；5 次生成请求 |
| `cayw-client --mode sdk ... prompt`，显式无认证模型配置 | Runtime 真实请求完成；任务 `completed`；Session `idle` |
| `cayw serve` + `cayw-client --mode ipc ... prompt` | 认证 loopback IPC 真实请求完成；协议 v3；服务确认退出 |
| `node --test crates/cayw-webui/tests/dom.test.mjs` | 35/35 通过；包含全部 HTML `pattern` 的 Unicode Sets 回归测试 |
| `cargo +1.95.0 build -p cayw-runtime --features 'plugins strongflow pi-compat' --locked` | 通过；生成最终真实 E2E 使用的本地产品 Client |
| `node scripts/validate-live-webui-e2e.mjs target/debug /private/tmp/cayw-live-webui-e2e-20261108-r4` | 8/8 断言通过；2 次真实模型请求；控制台错误为 0 |
| `node scripts/validate-live-webui-e2e.mjs target/debug /private/tmp/cayw-live-webui-e2e-20261108-r5` | 完整 workspace 测试后复跑；8/8 断言通过；2 次真实模型请求；控制台错误为 0 |

最终完整测试日志见 [workspace 测试日志](macos-local-client-20261108-workspace-test.log)，SHA-256 `b9e4815f9617080d2083d7d6a6665554e3f346815b97b036679b37756233486f`。

完整 workspace 日志中有 11 个 ignored 项。它们是由父测试显式启动的子进程夹具，或需要单独输入的可选验收：

- 子进程或清理夹具：`crash_effect_child`、`crash_child`、`live_scope_child`、`lifecycle_child`、`held_runtime_process`、`configured_transport_child`、`cleanup_failure_runner`、`fixture_child`。
- 单独运行的可选验收：`server_binary_device_example_and_operator_commands_work_on_loopback`、`complete_capture_preserves_stream_and_accounting`、`local_stream_stages`。

Cargo 还报告了一个非失败警告：`cayw-agent`、`cayw-content` 和 `cayw-runtime` 都有名为 `offline` 的 example，因此 workspace 构建产物名称碰撞。测试仍退出 0。这个警告没有被写成测试通过条件，也没有被隐藏。

## r3 产品验收

正式产品证据目录为 `/private/tmp/cayw-macos-local-20261108-r3`。各脚本要求新的输出目录；复跑时不能复用该目录。

| 验收 | 结果 | 主要证据 |
| --- | --- | --- |
| Headless SDK / IPC | 通过 | SDK 和 IPC 两种模式；18 次合成模型请求；0 次真实模型请求 |
| 本地产品工作流 | 14 项通过 | 无模型启动、`cayw conf`、明文可选密钥与公开脱敏、模型热切换、唯一 Client、插件清理、双工作区隔离 |
| 浏览器启动 | 6 项通过 | 一个 Client 和一个 WebUI 服务两个工作区；重复和并发启动复用；本地访问无需登录 |
| A/B/C 工作区切换 | 6 项通过 | Client 路由、Runtime 复用、任务身份保持、取消不产生文件副作用、无工作区连接文件 |
| 浏览器模型与插件设置 | 3 项通过 | 模型保存、插件进程同意、真实 Host 文件效果和刷新恢复 |
| 本机与 LAN 账号设置 | 5 项通过 | 真实非回环本机接口；`macos: "validated"`；0600 Argon2id 摘要；权限和会话撤销 |
| Workbench | 6 项通过 | 真实 Runtime 记录、Artifact、Editor、Trace 和 Host 调用；2 次固定本地模型请求；0 次付费请求 |

复跑命令如下。`$OUT` 必须指向一个尚不存在的新目录。

```sh
ROOT=/Volumes/ORICO/CAYW
BIN="$ROOT/target/debug"
OUT=/private/tmp/cayw-macos-local-NEW

python3 -I "$ROOT/scripts/validate-headless-chat.py" \
  --runtime "$BIN/cayw" \
  --client "$ROOT/examples/headless-client/target/debug/cayw-client" \
  --out "$OUT/headless-chat"

python3 "$ROOT/scripts/validate-product-workflows.py" \
  "$BIN" "$OUT/product-workflows"

CAYW_PLAYWRIGHT_MODULE=/Users/chengwen/.nvm/versions/node/v24.4.1/lib/node_modules/playwright-core/index.mjs \
CAYW_CHROMIUM='/Applications/Google Chrome.app/Contents/MacOS/Google Chrome' \
node "$ROOT/scripts/validate-browser-launch.mjs" \
  "$BIN" "$OUT/browser-launch"

CAYW_PLAYWRIGHT_MODULE=/Users/chengwen/.nvm/versions/node/v24.4.1/lib/node_modules/playwright-core/index.mjs \
CAYW_CHROMIUM='/Applications/Google Chrome.app/Contents/MacOS/Google Chrome' \
node "$ROOT/scripts/validate-workspace-switch-browser.mjs" \
  "$BIN" "$OUT/workspace-switch"

CAYW_PLAYWRIGHT_MODULE=/Users/chengwen/.nvm/versions/node/v24.4.1/lib/node_modules/playwright-core/index.mjs \
CAYW_CHROMIUM='/Applications/Google Chrome.app/Contents/MacOS/Google Chrome' \
node "$ROOT/scripts/validate-product-settings-browser.mjs" \
  "$BIN" "$OUT/product-settings"

CAYW_PLAYWRIGHT_MODULE=/Users/chengwen/.nvm/versions/node/v24.4.1/lib/node_modules/playwright-core/index.mjs \
CAYW_CHROMIUM='/Applications/Google Chrome.app/Contents/MacOS/Google Chrome' \
node "$ROOT/scripts/validate-account-settings-browser.mjs" \
  "$BIN" "$OUT/account-settings"

CAYW_PLAYWRIGHT_MODULE=/Users/chengwen/.nvm/versions/node/v24.4.1/lib/node_modules/playwright-core/index.mjs \
CAYW_CHROMIUM='/Applications/Google Chrome.app/Contents/MacOS/Google Chrome' \
node "$ROOT/scripts/validate-workbench.mjs" \
  --bin-dir "$BIN" --out "$OUT/workbench"
```

## 无模型 WebUI 手工验收

Playwright MCP 使用一个全新的隔离 `HOME`、`CAYW_HOME` 和工作区启动冻结的 `cayw`。启动输出明确报告 `model_configured: false`，但 Client 和 WebUI 正常可用。页面创建了“无模型启动验收”Session。

发送合成消息后，记录到的结果为：

```json
{
  "method": "task.enqueue",
  "workspace_id": "local",
  "status": 502,
  "error": {
    "code": "model_not_configured",
    "outcome_unknown": false
  }
}
```

页面随后满足全部断言：

- Session 状态为“就绪”。
- 模型未配置说明可见。
- “会话暂时不能继续”区域隐藏。
- 页面没有“连接结果待确认”或待核对说明。
- 消息框重新可用。
- 再次输入文本后，发送按钮可用。
- 停止按钮保持禁用。
- 再次调用 `session.list` 返回 HTTP 200；该 Session 的 Runtime 状态为 `idle`。

导航和普通页面加载没有控制台错误。执行预期拒绝后，Chrome 记录了一条 `Failed to load resource: 502`。这条记录对应上述有结构的 `model_not_configured` 响应。没有 JavaScript 异常；后续读取请求继续返回 200。

可访问性快照 SHA-256 为 `1565cc0e6ab7c0ac987675716ef5301be958122b5251a77f938b9207dc77243d`。截图 SHA-256 为 `435d7adbef62e75a95ca9c5be66d608b11d79f1518cf4793a7b2f6fff48f86d0`。两份证据都没有 IPC 访问码、账号密码或模型密钥。

## 真实浏览器 Agent E2E

最终通过目录为 `/private/tmp/cayw-live-webui-e2e-20261108-r5`。该目录是完整 workspace 测试和最终二进制重建后创建的全新隔离目录。脚本没有复用 `r1` 至 `r4`。较早的 `r4` 也独立通过，并保留全部原始证据。

这次验收覆盖一条完整链路：

```text
浏览器输入
→ WebUI HTTP RPC
→ 唯一 Client
→ 工作区 Runtime
→ Magpie 真实模型
→ execute_code
→ Luau 调用 fs.write
→ WebUI 显示精确权限请求
→ 浏览器点击“允许一次”
→ Host 写入文件
→ 工具结果回到模型
→ 第二次真实模型生成
→ Runtime 完成任务
→ WebUI 显示最终答案
→ 整页刷新后恢复最终答案
```

浏览器先在设置页保存并启用 `live-browser-e2e`。配置使用 `magpie`、`chat_completions`、`auto/deepseek-v4.1-flash` 和本机 `http://127.0.0.1:3425/v1`。服务无需认证，因此 `models.json` 没有 `api_key` 字段。

模型第一次生成了一个 `execute_code` 调用。调用参数中的 Luau 脚本精确为：

```lua
return cayw.call('fs.write',{path='live-e2e.txt',content='CAYW_LIVE_WEBUI_E2E_FILE'})
```

页面显示“写入文件”审批卡。卡片显示 `live-e2e.txt` 和写入内容。浏览器点击“允许一次”后，Runtime 才记录 `capability_started`。Host 写入 24 字节，文件 SHA-256 为 `ea1af8184cdb699e0da32752f570e7091d86a8a4c04a23867dc715fa7017e5bc`。工具结果为 `completed`，`is_error: false`，`outcome_unknown: false`。

模型第二次生成后，页面显示精确结果 `CAYW_LIVE_WEBUI_E2E_OK`。Task 为 `completed`。Session 为 `idle`。待审批、未解决调用和未解决能力均为 0。整页刷新后，最终答案仍然可见，任务没有重放。

两次真实请求都完整结算：

| 轮次 | 结果 | 用量 |
| --- | --- | ---: |
| 1 | `toolUse`，产生唯一 `execute_code` | 1505 token |
| 2 | `stop`，返回最终标记 | 1792 token |
| 合计 | 2 次请求 | 3297 token |

浏览器 RPC 记录包含 `settings.models.put`、`settings.models.use`、`session.create`、`task.enqueue`、`task.resume`、`approval.decide`、`session.events` 和 `session.get`，全部返回 HTTP 200。三次 `settings.plugins.list` 也全部返回 HTTP 200。浏览器控制台错误列表为空。Client 和 WebUI 均确认退出 0。

最终通过前保留了三次失败尝试：

| 尝试 | 结果 | 原因 | 真实请求 | 已知用量 |
| --- | --- | --- | ---: | ---: |
| `r1` | 失败 | 验收脚本把 `fs.write` 的页面名称误写成“修改文件” | 2 | 3245 token |
| `r2` | 失败 | 文件名定位同时匹配摘要和原始 JSON，Playwright 严格模式拒绝两个元素 | 1 | 1460 token |
| `r3` | 失败 | 核心链路已完成，但误用默认空 feature 构建，三次插件设置请求返回 502 | 2 | 3210 token |
| `r4` | 通过 | 使用本地产品功能集重新构建，8 项断言全部通过 | 2 | 3222 token |
| `r5` | 通过 | 完整 workspace 测试后重建最终二进制并全新复跑，8 项断言全部通过 | 2 | 3297 token |

五次浏览器尝试共发送 9 次真实请求，全部有完整上报用量，共 14434 token。失败尝试没有被删除，也没有改写为成功。`r1` 和 `r2` 清理时出现的本地 `aborted` 消息没有对应 `model_request started` 记录，因此不另算上游请求，也不把其中的零值解释为零费用。

可复跑命令如下。输出目录必须尚不存在。`cayw` 必须先按本地产品功能集构建。

```sh
cargo +1.95.0 build -p cayw-runtime \
  --features 'plugins strongflow pi-compat' --locked
cargo +1.95.0 build -p cayw-webui --locked

CAYW_PLAYWRIGHT_MODULE=/Users/chengwen/.nvm/versions/node/v24.4.1/lib/node_modules/playwright-core/index.mjs \
CAYW_CHROMIUM='/Applications/Google Chrome.app/Contents/MacOS/Google Chrome' \
node scripts/validate-live-webui-e2e.mjs \
  target/debug /private/tmp/cayw-live-webui-e2e-NEW
```

持久化证据：

- [结构化 E2E 结果](macos-local-client-20261108-live-webui-e2e.json)，SHA-256 `49fb8ceebd524aea479719ab6da7c996834d86e01091d62a4de7e776212e9eb1`
- 失败摘要：[r1](macos-local-client-20261108-live-webui-e2e-r1-failure.json) `d41fc7f76dd71db843029a16f7c16972648248dfe1a8382aec57cc22b54e7f2d`；[r2](macos-local-client-20261108-live-webui-e2e-r2-failure.json) `d491926db8feb15f8e8c2dab5ff3bb12443b09b53a988f2f7482c76f73528a60`；[r3](macos-local-client-20261108-live-webui-e2e-r3-failure.json) `a5b2b68af29e4a56785b6efda270d54b60d8ca5eee5c32545070650178561a84`
- `r4`：[运行摘要](macos-local-client-20261108-live-webui-e2e-run.json) `d6393cde773635cd6e589c93541bf20b693eff551525a740778556cbc5f70296`；[Runtime 与事件](macos-local-client-20261108-live-webui-e2e-runtime.json) `a22a202f2d75734b8b7b578ffb8c7c0fbd1cd81345b117a929479a9bd10fc210`；[浏览器 RPC](macos-local-client-20261108-live-webui-e2e-rpc.json) `200cc0ccd95746592e4a53502196f30510b1295d923279cb0c99b7048637e233`；[审批截图](macos-local-client-20261108-live-webui-approval.png) `d29892dc3b1f8b40a7322079d38bd84b3a1c58d9d3fbb9f80056a79027a49d79`；[完成截图](macos-local-client-20261108-live-webui-completed.png) `8bdb660904869268f8244f1d2ff9be53f231a75fb02d09caf7d51984710f2359`
- `r5`：[运行摘要](macos-local-client-20261108-live-webui-e2e-r5-run.json) `334b70f881d3e10044896e1b4e4e47ee21111b9a38ef51555966d7d5f60d6037`；[Runtime 与事件](macos-local-client-20261108-live-webui-e2e-r5-runtime.json) `77396da4f08cf392015f823b699688d5e2afe7b237db3cb740b0a6cc2adbf25a`；[浏览器 RPC](macos-local-client-20261108-live-webui-e2e-r5-rpc.json) `200cc0ccd95746592e4a53502196f30510b1295d923279cb0c99b7048637e233`；[源码收据](macos-local-client-20261108-live-webui-e2e-r5-source-receipt.json) `37c649432a0d2f9eb8ea709ca728bb6282266746318f28cfa9aacebf4010e40c`；[模型配置](macos-local-client-20261108-live-webui-e2e-r5-model-config.json) `cd2e264d118dd09b031674d8fc079c5b4e946de2494d6086b595cb2ed9c7cc01`；[浏览器错误](macos-local-client-20261108-live-webui-e2e-r5-browser-errors.json) `37517e5f3dc66819f61f5a7bb8ace1921282415f10551d2defa5c3eb0985b570`；[审批截图](macos-local-client-20261108-live-webui-r5-approval.png) `91a6b050cb04a05e635bcc696ec32c99e620cacdf6f8c6694dce27b42aaabb5f`；[完成截图](macos-local-client-20261108-live-webui-r5-completed.png) `1bf1cebbd17c30bc83397102904e352af9d881ae0f7a3449137fbe75bb7b05f0`
- [完整 workspace 测试日志](macos-local-client-20261108-workspace-test.log)，SHA-256 `b9e4815f9617080d2083d7d6a6665554e3f346815b97b036679b37756233486f`

## 早期真实模型分段验收（不是 E2E）

用户在当前会话中明确授权真实模型调用。本次使用本机 Magpie `0.1.2777`。模型目录有 23 项；旧请求 ID `group/auto-deepseek-v4-1-flash` 已不在目录中，因此请求使用当前 ID `auto/deepseek-v4.1-flash`。网关响应中的模型名仍为 `group/auto-deepseek-v4-1-flash`。

本机网关按无认证模式运行。核心示例仍要求 `MAGPIE_API_KEY` 非空，因此运行时只传入一个非秘密的本地占位值来通过旧参数检查。Runtime SDK 验收直接使用不含 `api_key` 的显式模型配置，公开运行信息显示 `authentication: "none"`。

这组早期分段验收产生 7 次生成请求：

| 路径 | 请求数 | 实际结果 |
| --- | ---: | --- |
| `cayw-agent` + `cayw-ai::OpenAiProvider` | 5 | 流式文本、Rust 工具循环、后续上下文和中途取消四项全部通过 |
| 当前 `cayw-client --mode sdk` + Runtime/Session | 1 | 请求账本完整结束；精确返回 `CAYW_RUNTIME_REAL_OK`；Task 为 `completed`；Session 为 `idle` |
| `cayw serve` + 认证 loopback IPC + Headless 客户端 | 1 | IPC v1 / Core v3；精确返回 `CAYW_IPC_REAL_OK`；请求、任务和 Session 全部结算；服务确认退出 |

核心 Agent 的四项结果：

- 流式文本收到 2 个文本增量、9 个思考增量和唯一终态；最终文本精确等于 `CAYW_MAGPIE_STREAM_OK`。
- 模型调用 `read_probe` 一次。工具调用和结果 ID、名称配对；`is_error` 和 `outcome_unknown` 均为 false；模型精确返回进程内生成值。
- 撤下工具后，模型在下一轮仍精确返回同一值，工具执行次数保持 1。
- 取消场景在第一个非空文本增量后中止。终态为 `aborted`，Agent 回到空闲，没有待处理工具调用或待核对状态。

核心 Agent 的四个已完成请求报告输入 1012 token、输出 291 token，其中思考 206 token，总计 1303 token。取消请求没有返回完整用量；其零值不能解释为零消耗或零费用。

Runtime SDK 的一次请求报告输入 1230 token、输出 10 token、总计 1240 token。请求账本依次记录 `reserved`、`started`、流事件、`usage_update`、`done` 和 `finished: completed`，结算 token 为 1240。Session 没有待审批、未解决调用或未解决能力。

认证 loopback IPC 的一次请求报告输入 1229 token、输出 9 token、总计 1238 token。Runtime 健康检查报告 Core 协议 v3、模型配置正确。模型精确返回 `CAYW_IPC_REAL_OK`；请求终态、Task 状态均为 `completed`；Session 回到 `idle`；服务收到 SIGINT 后确认清理并退出。

这组早期分段验收的已知完成请求合计 3781 token。上游账单和取消后是否立即停止生成没有独立核对，所以费用状态仍为未知。

两次前置启动曾因 `MAGPIE_API_KEY` 未设置而在 Provider 创建前退出。它们没有发送模型请求，也没有产生重复模型费用。确认本机网关按无认证模式运行后，第三次核心运行通过。

证据文件：

- [真实模型结构化结果](macos-local-client-20261108-live-model.json)，SHA-256 `0b7f68dac1d9ec2948dad8565544c478b10463d6bf77376850af9b54ac628658`
- [核心 Agent 成功日志](macos-local-client-20261108-live-model-attempt3.log)，SHA-256 `6c1c621b03db1c8c6aab1c37d8a8f92be7773bfd3b5363e7f166ef0759da5065`
- [Runtime SDK 成功日志](macos-local-client-20261108-live-runtime.log)，SHA-256 `c864352b5e244308130c9745dbe313dd41a4f7709e42ab83dfd14d1d6ed0e0fd`
- [Runtime IPC 成功日志](macos-local-client-20261108-live-ipc.log)，SHA-256 `e97f0c5978087d27a4f9364521556b75f4d5c4336526b7cc302b240fc7443328`
- [当前源码、二进制和日志收据](macos-local-client-20261108-live-model-sources.sha256)，SHA-256 `11899ced61d8a9a7cfab56e5882f67faa48863f41b85207c2d33267770bb54f8`

源码收据来自本次 `magpie.d` 的实际 Rust 编译输入，并额外包含 Cargo 清单、锁文件、核心示例二进制、Runtime 二进制、Headless 客户端二进制和三份成功日志。`shasum -a 256 -c` 全部通过。日志不包含真实密钥；输入只包含合成提示和进程内合成工具值。

## 验收中发现并修复的问题

最终通过前，验收发现并修复了以下真实问题：

- 浏览器启动脚本在工作区目录异步加载完成前读取选择值。脚本现在等待选择值实际更新。
- WebUI 曾把 `model_not_configured` 这种明确的已知失败错误当成未知写结果。修复后，`outcome_unknown: false` 会释放本地写锁，Session 不再错误冻结。
- 浏览器会请求 `/favicon.ico`，原服务返回 404。现在该路由返回 204，普通页面加载不再产生这条错误。
- 默认 feature 的 `tui_cli` 曾被 workspace feature 合并掩盖，实际依赖插件夹具。插件测试和导入现在只在 `plugins` feature 下编译。
- Pi 外部插件卸载原来没有等待可确认的进程退出。卸载路径现为异步等待，并在结果无法确认时保留 unknown 分类。
- 模型方案名称的 HTML `pattern` 在 Chrome 的 Unicode Sets 规则下没有转义连字符，页面会产生正则错误。现在使用转义后的连字符，并增加了遍历全部 `pattern` 属性的回归测试。
- 真实 E2E 脚本最初使用了错误的审批名称和过宽的文件名定位。现在精确匹配页面显示的“写入文件”和“文件：live-e2e.txt”。失败路径也会保存浏览器 RPC 和控制台记录。
- 一次重建误用了 `cayw-runtime` 的默认空 feature，导致插件设置请求返回 502。最终 E2E 明确使用 `plugins strongflow pi-compat` 本地产品功能集；`r3` 仍保留为失败证据。
- 完整 workspace 测试重新链接了产品二进制。验收没有把较早 `r4` 的哈希套到新文件上，而是重建最终二进制并用全新目录完成 `r5`。

曾运行 `node --test crates/cayw-webui/tests/*.mjs`。该通配符还选中了 `runtime-browser.mjs`。这个文件是需要二进制参数并持续运行的验收服务，不是 `node:test` 测试模块，所以该命令显示 107 项通过、1 项失败。随后使用九个明确测试模块复跑，107/107 通过。前一次结果是命令选择错误，不是产品测试失败。

## 模型请求、隐私和限制

- Headless 离线验收报告 18 次合成请求。
- Workbench 报告 2 次固定本地模型请求。
- 早期 Magpie 分段验收发送 7 次真实请求：核心 Agent 5 次、Runtime SDK 1 次、认证 Runtime IPC 1 次。已完成请求报告 3781 token；其中一次取消请求用量未知。
- 五次真实 WebUI E2E 尝试又发送 9 次真实请求，全部完整上报用量，共 14434 token。`r4` 和最终二进制上的 `r5` 都是完整通过的 E2E。
- 本会话合计发送 16 次真实请求。15 次已完成请求报告 18215 token，1 次早期取消请求用量未知。真实费用和上游取消行为没有独立核对。
- 真实模型输入只包含合成提示和进程内合成工具值，没有发送仓库内容。
- 验收日志、JSON、截图和快照不保存模型密钥、IPC 访问码或账号密码。
- 本次只验证 macOS arm64 本地产品。Windows、Linux、云端、SSO、MDM、Homebrew、IDE、Desktop、Keychain 和插件市场均未验证，也不构成本次阻塞项。
- 本次没有重置、提交、推送或创建 PR。原有未提交修改和未跟踪文件均保留。
