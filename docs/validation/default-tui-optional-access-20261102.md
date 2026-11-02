# 默认 TUI 与可选网页访问验证（2026-11-02）

本记录取代 2026-10-31 默认网页、启动票据和手工网页 token 的当前产品说明；旧记录只说明各自历史源码。实现提交为 `640719463589c126c03a9c88b3791fdda71ed616`，验证脚本兼容更新为 `94cd3681c89a6c24c400800ce7fadb59d6d204ce`。分支 `feat/product-workflows-20261031`，前次交付 `c3b805f9606a6466000fa3590b410ca58c7b7208`，完整补丁基线仍为 `0e2b2596a67b98ab5c048974b08523413eaf33c5`。后续提交仅补本验证记录。

## 实际启动行为

```sh
cd /你的项目
cayw                                      # 默认 TUI，工作区为 cwd
cayw --workspace /另一个项目               # 显式覆盖
cayw start                                # 同一默认 TUI 入口
cayw --ui web                             # 显式选择可选网页
cayw --ui web --no-open                    # 普通 URL，免登录
cayw --ui web --lan 192.168.1.10:3080      # 未配置账号也启动并允许访问
cayw account --username admin              # 可选：交互式隐藏输入两次密码
# 重启网页启动器后加载新账户；本地回环模式始终免登录。
cayw --ui web --lan 192.168.1.10:3080
```

裸命令进入实际 TUI，不启动 WebUI 或浏览器。重复裸命令在另一终端进入 TUI，复用 Runtime；网页必须显式选择，可同时连接 TUI 所用 Runtime。`cayw tui` 保留仅连接语义，`serve/init/configure/models/plugins` 保留。每个启动器只清理自己创建的进程。

本地网页不要求登录、token、启动票据或认证 cookie；直接访问普通 URL 即可。显式 `--lan IP:PORT` 后，缺少可选账户文件时允许启动和访问，终端与页面给出非阻塞暴露提示。账户存在时使用账号密码；匿名访问和错误密码被拒绝，成功后通过 HttpOnly、SameSite=Strict 会话保持登录，退出撤销当前会话。没有 token 粘贴入口或 launch ticket。缺少文件不是错误；已有账户文件损坏或权限异常明确报错，避免静默取消用户已配置的登录。

## 调用链和边界

- `crates/cayw-runtime/src/main.rs`：无子命令或根级启动参数转到统一启动器，默认 cwd；`--help` 保留纯帮助；`account` 显式调用独立 WebUI 的账户配置程序。
- `cli_start.rs`：默认 TUI；仅 `--ui web` 启动网页。TUI 与 WebUI 分别持有启动锁，重复 TUI 连接原服务，重复网页复用原地址。网页只打印/打开普通 URL，原 `web.token` 生成和启动票据申请已删除。默认回环地址，局域网必须明确传参；内部 IPC 认证仍自动交接，不要求用户输入。
- `cayw-webui/src/main.rs` 与 `access.rs`：本地免认证；LAN 的 `None` 账户直接放行，`Some` 账户检查密码。账户命令只在交互终端隐藏读取密码，保存加盐 Argon2id 摘要，0600 原子创建，不覆盖旧账户。密码不接收命令行参数，不进入日志、URL 或浏览器存储。账户是启动时配置，变更需重启 WebUI。
- `lib.rs` 与 `assets/app.js`：默认本地自动读取实际 Runtime；LAN 已配置账户时提供账号密码表单，提交后清空密码字段。刷新不重发任务。Host/Origin/Sec-Fetch-Site、请求大小、并发、权限、工作区归属和未知结果分类保留。局域网模式不接受 Bearer header 代替密码。原显式 Rust 网关 Bearer API 保持 SDK 兼容且仅允许回环；CLI、默认网页及 LAN 不选用它。

采用 RustCrypto Argon2 0.5.3（MIT / Apache-2.0）与 rpassword 7.5.4（Apache-2.0），已核其官方文档和依赖许可证。未复制 DSH 认证实现，未改变项目许可证。

## 冻结版本运行证据

执行平台为 Linux x86_64、Rust 1.95.0、Chromium；macOS 原生验收未运行。源码和固定二进制摘要见 `frozen-source-and-binaries.json`，测试使用 `/tmp/cayw-optional-frozen-binaries` 复制件。验证脚本后续改动只清理旧手工 token 登录步骤，应用输入与冻结构建一致，已记录文件清单。命令、时间、退出码与日志 SHA256 见交付包 `logs/*.json`，详细场景结果另见 `*-summary.json`。

| 检查 | 证据结果 |
| --- | --- |
| fmt、全仓 check、WebUI 严格 all-targets Clippy | 通过 |
| Runtime/WebUI plugins、pi-compat、strongflow 相关 lib/bin Clippy | 通过；仅命令级豁免基线非 macOS 的 dead_code、unused_mut、unreachable_code、useless_conversion，未修改源码告警策略 |
| WebUI Rust 测试 | 通过，含 29 项真实网关 HTTP/IPC 集成；本地免登录仍拒绝跨站/错误 Host，LAN 登录拒绝匿名、错误账号密码、跨站和 Bearer 替代，刷新会话及注销有效 |
| UI 状态、DOM、工作台和 Core 协议 Node 测试 | 101 项通过；取消、未知副作用、回执丢失和会话冻结语义保留 |
| `validate-default-entry.py` | 实际 PTY：裸命令 cwd、显式 workspace、默认 TUI、重复 TUI、TUI 期间显式网页、退出外部 Runtime 保留、兼容 start、账户两次隐藏输入、0600 摘要及已有文件保留均通过 |
| `validate-browser-launch.mjs` | 系统 xdg-open 实际打开 Chromium；本地无登录/票据/cookie、实际离线任务产物和刷新不重放；SSH/no-open/无桌面与打开失败保留普通 URL；LAN 未配置直接放行及提示；真实账户命令、错误/正确登录、刷新和退出；重复/并发网页与外部 Runtime 保留均通过 |
| `validate-product-workflows.py` | 13 组完整 CLI 流程通过，包括模型配置 A→停服→离线 B→重启、失败激活、配置中断、插件实际进程审批/失败更新/持久化不确定性、取消和重连 |
| `validate-workspace-switch-browser.mjs` | A→B→A 保留原运行 Task/Attempt/fence、审批和单次 resume；显式取消无文件副作用；B 自有服务清理、C 外部服务保留；可选工具状态正确 |
| `validate-product-settings-browser.mjs`、`model_switch` | 网页模型设置、插件审批、实际离线聊天和刷新通过；活动 Attempt 的 Provider、模型限制和账本身份保持原快照 |
| 旧故障注入/工作台验证脚本兼容更新 | 已清理 WebUI token 输入步骤并通过语法检查；未将这些旧脚本整套或 macOS 工作台测试算为本轮执行通过 |

早期验证的旧私有 token 夹具、单独 crate 的随机数 feature、独立网关输出解析、TTY 输入同步和测试目录可选二进制差异已处理；最终通过结果以冻结版本记录为准。失败尝试保存在 `earlier-attempts`，未抹去诊断。

## 限制与阻塞

- `cargo clippy --workspace --all-targets --locked --offline -- -D warnings` 返回 101，仍是未修改基线 `cayw-device` 的非 macOS unused_mut、dead_code 和 TIOCSCTTY.into 告警。相关文件逐字节与上游基线一致。
- `cargo test --workspace --locked --offline` 返回 101，本轮在大量测试链接时耗尽 32GB 云盘，出现 ENOSPC 与链接器 signal 7，未完成全仓测试。沙箱新命令启动也暂时受阻。已通过允许的执行方式删除约 2.6GB 可重建测试缓存，恢复环境；源码、完整日志、回执及固定场景测试二进制保留。未把相关测试通过外推为全仓通过。
- macOS Terminal/Safari/default browser、arm64/x86_64 构建和安装升级回滚未运行。LAN 行为在显式 LAN 模式的隔离 loopback 上测试，没有更改用户机器网卡、防火墙或真实账户，未验证实际局域网硬件。
- 当前 LAN 使用 HTTP；未增加 TLS 终止、远程账户管理或密码热加载。配置后重启 WebUI 生效，未配置时按用户要求直接放行。
- 未读取真实模型密钥，未调用真实/付费模型，未 push、创建 PR、合并、发布或部署。交付包不含截图、浏览器 profile、账户文件、测试 HOME 或用户数据。
