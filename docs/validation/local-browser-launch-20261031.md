# 本机自动启动与浏览器交接验证（2026-10-31）

本记录绑定代码提交 `3a5a82d4d949512231afd2edb86b13dac1b13eda`，分支 `feat/product-workflows-20261031`，原始上游基线 `0e2b2596a67b98ab5c048974b08523413eaf33c5`。此前产品流程提交均保留。本记录之后的提交只补验证文档。执行平台是 Linux x86_64、Rust 1.95.0、Chromium 151；不代表 macOS 验收或正式发布。

## 使用结果与调用链

```sh
cayw start --workspace /你的项目
# 不自动打开：
cayw start --workspace /你的项目 --no-open
# 离线演示，不调用付费模型：
cayw start --workspace /临时项目 --offline --demo-authorized
```

普通本机启动在服务就绪后打开系统默认浏览器，直接进入工作区；不找文件、不手输 token。刷新保持当前网关的会话。`--no-open`、SSH_CONNECTION / SSH_TTY 非空、无桌面及打开失败时，输出可点击的 `launch_url`。该链接 60 秒内一次有效；过期后重新运行原命令获取新入口。SSH 仍需用户自行做相同端口的本机转发，启动器不推断客户端地址，也不开放非 loopback。

- `crates/cayw-runtime/src/cli_start.rs`：复用或启动 Runtime 和独立 WebUI，保存私有实例描述；等待就绪后 `present` 用私有控制凭据 POST `/api/browser/launch`，仅取得短期票据。macOS 调用 `/usr/bin/open URL`，云端验证调用系统 `xdg-open URL`。打开失败保留服务和可点击入口。重复启动申请新票据；并发首次启动等待同一实例就绪。Ctrl+C/SIGTERM 继续仅清理本次拥有的进程。
- `crates/cayw-webui/src/browser_session.rs`：外层严格 Host/Origin/Sec-Fetch-Site 校验后，控制凭据才可申请票据。一次票据绑定接收 authority，验证后签发 HttpOnly、SameSite=Strict、8 小时 cookie，并以 303 跳回 `/` 清除 URL 票据。会话与票据仅在本次网关内存中存在；重启均失效。票据和会话数量有上限，超限明确拒绝。cookie 不能签发新启动票据。
- `crates/cayw-webui/src/lib.rs`：RPC 与 Panels 同时接受已验证会话或原有显式 Bearer 客户端，保持 loopback、Origin、权限、任务归属和请求结果分类。注销删除该会话。所有响应保留 no-store / no-referrer。
- `crates/cayw-webui/assets/app.js`：页面自动以 same-origin cookie 检查实际 Runtime health；成功才展示工作区，刷新只恢复读取，不重发任务。认证过期提示重新运行 `cayw start`。独立网关高级 Bearer 输入保留在折叠项中，默认启动不经过该路径。凭据不进入 localStorage/sessionStorage。

## 官方 DSH 源码依据

已实际读取官方 `deepseek-ai/deepseek-harness` 的固定提交 `ce7073d51254c26cd63b22a60835b8f948deb447`：

- [browser-auth.ts](https://github.com/deepseek-ai/deepseek-harness/blob/ce7073d51254c26cd63b22a60835b8f948deb447/packages/client/connection/src/browser-auth.ts)：启动入口自动交接随机进程 token，GET 验证、HttpOnly / SameSite=Strict cookie、303 跳回干净 URL，刷新使用 cookie。
- [web-app/src/index.ts](https://github.com/deepseek-ai/deepseek-harness/blob/ce7073d51254c26cd63b22a60835b8f948deb447/packages/bundle/web-app/src/index.ts)：Loader/API 就绪后才交接；SSH 或 no-open 不打开；系统打开失败后保留入口。

CAYW 独立实现了同样无需手输的本机入口体验，采用一次性短期票据和当前进程会话；没有照搬 DSH 持久签名密钥、30 天 cookie 或新增外部凭据系统。

## 运行证据

完整命令、时间、退出码和日志 SHA256 见同名 JSON 与交付包 `logs/*.json`。`frozen-source-and-binaries.json` 记录全体跟踪文件摘要、固定二进制摘要和基线相同的阻塞文件；所有最终浏览器和 CLI 流程使用 `/tmp/cayw-browser-delivery-binaries` 的复制件，后续 Cargo 命令不会覆盖它们。只上传摘要、代码及日志；没有上传截图、浏览器 profile、测试 HOME 或认证状态。

| 检查 | 实际结果 |
| --- | --- |
| `cargo fmt --all -- --check`、全仓 `cargo check --workspace --locked --offline` | 通过 |
| `cargo test -p cayw-webui --locked --offline` | 通过，包括 28 项实际网关 HTTP/IPC 测试及票据过期、authority、旧网关会话单测 |
| WebUI 严格 all-targets Clippy | 通过，无告警豁免 |
| Runtime/WebUI plugins、pi-compat、strongflow 的相关 lib/bin Clippy | 通过；仅命令行豁免原有非 macOS dead_code、unused_mut、unreachable_code、useless_conversion，未修改源码告警策略 |
| Core 协议、UI 状态、DOM 与工作台 Node 测试 | 101 项通过；未知副作用、取消和会话冻结回归仍通过 |
| `model_switch` | 通过，活动 Attempt 的 Provider、模型、限制及账本身份保持原快照 |
| `scripts/validate-browser-launch.mjs` | 系统 xdg-open 实际启动测试用 Chromium，自动登录、干净 URL、刷新、重复启动、并发首次启动通过；no-open/SSH/无桌面不唤起；打开失败仍可点击；无效/已使用/旧网关链接拒绝；注销、重启恢复、外部服务复用及所有权清理通过 |
| `scripts/validate-workspace-switch-browser.mjs` | 登记工作区 A→B→A，原运行任务的 Task/Attempt/fence 和审批不变、只 resume 一次；显式取消后没有文件副作用；B 自有服务清理、C 外部服务存活；可选 Editor/Trace 未安装入口禁用且给出路径 |
| `scripts/validate-product-settings-browser.mjs` | 模型设置、插件登记及真实进程显式审批、离线聊天产物、刷新自动重连通过 |
| `scripts/validate-product-workflows.py` | CLI 完整回归通过，包括配置中断、模型切换、在线 A→停服→离线 B→重启选择 B、失败激活、插件更新失败、持久化不确定性、取消、重连及外部服务复用 |
| 实际 PTY TUI、定制数据库/IPC 身份与离线 Agent 示例 | 通过；TUI 创建会话后退出，自有 Runtime discovery 清理；自定义身份保留普通 API 并明确拒绝不兼容的工作区管理 |

最初测试夹具仍按手工登录等待，及本轮两处 Clippy 提示已修复；失败尝试的日志一并保留，最终采用上述冻结版本证据。

## 未完成的跨平台验收与基线阻塞

- macOS `/usr/bin/open`、Safari / 系统默认浏览器、arm64 / x86_64 构建、安装和升级回滚均未运行；云环境只提供 Linux。此处无被拒绝的权限请求，属于环境能力缺失。云端系统 opener 与 Chromium 测试不作为 macOS 结果。
- `cargo test --workspace --locked --offline` 返回 101：基线 `process_cleanup.rs` 引用仅 macOS 实现的 `ProcessIdentity::observe` 与 `Runtime::confirm_lost_processes_stopped`，8 项 E0599，完整日志 `logs/workspace-tests.log`。
- 严格全仓 `cargo clippy --workspace --all-targets --locked --offline -- -D warnings` 返回 101：基线 `cayw-device` 的非 macOS unused_mut、dead_code 与 TIOCSCTTY.into 问题，完整日志 `logs/workspace-clippy.log`。阻塞文件逐字节与原始上游基线一致，摘要记录提供核对结果；未为 Linux 适配而修改它们。
- 未调用真实或付费模型，未读取真实用户模型密钥，未推送、合并、发布、部署或创建 Release。真实 macOS 验收和发布仍不在此次已验证范围。
