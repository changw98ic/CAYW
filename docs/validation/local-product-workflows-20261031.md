# 本地产品工作流：2026-10-31 开发验证

本记录证明下面列出的源码与开发场景，不是 macOS 首发验收或正式发布声明。没有推送、PR、合并、Release 或部署操作。

## 源码与环境

- 最新基线核对为 `0e2b2596a67b98ab5c048974b08523413eaf33c5`，独立工作副本 `/workspace/CAYW`，分支 `feat/product-workflows-20261031`。
- 功能提交 `27d7951478a43a3435f81fa61df89501202c94e3`。本记录与 JSON 为随后单独的文档提交；运行对象的源码及程序摘要均绑定前一功能提交。
- 执行环境 Linux x86_64，Rust 1.95.0、Node 24.19.0、系统 Chromium；仅作开发证据。未改动 WinWinCode 源码，使用环境已安装的 Rust/Node 工具链。
- Runtime 构建启用 `plugins`。另对 `plugins,pi-compat,strongflow` 组合做编译/Clippy 检查，没有运行真实 Pi 包或付费模型。
- 最终运行前把四个二进制复制到 `/tmp/cayw-product-frozen-final`，防止 Cargo 后续构建覆盖运行对象。验证后逐项重算源码、冻结程序和日志 SHA256，确认一致。

详细命令、UTC 起止时间、退出码与日志 SHA256 见[机器可读摘要](local-product-workflows-20261031.json)。完整轻量交付目录为 `/workspace/CAYW-delivery-20261031`，含 `verification-summary.json`、`frozen-source-and-binaries.json`、`logs/`、`screenshots/` 和完整补丁。测试生成的令牌文件及配置数据未放进交付包。

## 用户路径与实际结果

| 路径 | 实际运行与断言 |
| --- | --- |
| Web 统一启动 | `cayw start --offline --demo-authorized --workspace DIR` 启动真实 Runtime 和 WebUI；只输出 URL 与私有令牌文件路径。重复启动返回原 URL，不生成第二个服务。错误令牌被拒绝。 |
| 无界面与 TUI | `start --ui none` 启动后用 SIGTERM 清理，服务发现记录移除。真实 PTY 中 TUI 连上所拥有的 Runtime、打开新建会话对话框并输入名称、Ctrl+Q 退出；确认所拥有服务记录移除。PTY 辅助脚本响应终端光标查询，不替代 macOS Terminal.app 验收。 |
| 复用服务 | 先独立 `serve --offline`，再 `start`；返回 `owned_runtime:false`。停止启动器后原服务仍可通过认证 IPC 查询，再由原测试所有者停止。 |
| 工作区 | 切换到另一个目录，新工作区的模型方案和会话列表为空，已登记目录列表包含两处；根目录不在已有会话下被替换。 |
| 真实离线任务 | 浏览器及 HTTP 都创建会话并发送任务，经过实际 Luau/Host 写入并读回 `cayw-demo.txt`，内容精确等于 `headless runtime demo`。重连读取原会话，不重发 prompt。 |
| 模型管理 | HTTP/CLI 保存、列出、选择方案；选择立即改变 health 模型，不重启，不发送模型请求。非法模型更新保留原方案，当前方案不能直接删除，离线服务拒绝选择真实模型。CLI configure 保存后即时应用，EOF 和放弃保存保留原字节。 |
| 运行中模型切换 | 专项测试在旧 Provider 首个回合暂停时切换模型，恢复后旧任务的工具回合与最终回复继续使用旧 Provider/模型/输出限制，账本保留旧模型身份；新任务使用新模型。 |
| 插件完整生命周期 | 通过 CLI 登记本地 External 描述，未批准启用被拒；明确批准后实际进程握手成功，可绑定空闲会话。停用冻结原会话，重启及重新启用后仍冻结；已启用实例不能直接更新。不兼容协议更新保留原登记；实际无法握手的本地进程留下具体错误、停用状态及 unknown 分类。恢复正确登记后可重新批准启用，重启恢复批准实例；卸载后登记列表与已加载插件列表均为空。 |
| 管理持久化故障 | 只破坏测试目录中的管理锁文件类型，停用返回 `settings_outcome_unknown`，状态仍明确显示实例启用且可用。修复测试锁后由测试显式重试，停止成功；没有自动重试。 |
| 任务取消 | 真实离线任务在等待文件审批时，以当前 Task、Attempt、worker 和 fence 取消；最终状态 `cancelled`，未发生文件写入。 |
| 实际网页操作 | Chromium 登录、切服务时自动更新 API/版本默认值、保存模型方案、要求插件进程复选批准、启停实际插件、创建会话及发送离线任务、刷新重连。页面无 JS 异常。保存桌面与 390px 移动端截图，并检查设置窗口不超过视口宽度。 |

主链脚本是 [validate-product-workflows.py](../../scripts/validate-product-workflows.py) 和 [validate-product-settings-browser.mjs](../../scripts/validate-product-settings-browser.mjs)，复跑参数见[使用文档](../local-workflows.md)。所有服务均使用临时 HOME/CAYW_HOME 和清空后的环境；仅模型配置切换测试注入固定合成密钥。没有读取用户真实模型密钥，没有外部模型请求。

## 检查结果与限制

| 检查 | 结果 |
| --- | --- |
| `cargo fmt --all -- --check` | 退出 0 |
| `cargo check --workspace --locked` | 退出 0；保留已有非 macOS 分支告警 |
| Runtime `model_switch` / `provider_config` / `runtime` | 分别 1 / 6 / 7 passed，退出 0；涵盖模型身份、配置校验、审批、取消、持久化和预算阻止副作用 |
| WebUI gateway | 27 passed，退出 0；含凭据保护、所有权持久化、控制通道、超时和 unknown 分类 |
| WebUI JS/DOM | 101 passed，退出 0 |
| 最终冻结程序的主链 / 浏览器 / PTY | 退出 0，结果与具体断言见上表和 JSON |
| `sh -n scripts/install-cli.sh scripts/package-cli.sh` | 退出 0；仅语法检查 |
| 产品库/二进制 Clippy，含 plugins/Pi/StrongFlow 组合 | 退出 0。命令明确允许既有 `dead_code`、`unused_mut`、`unreachable_code`、`clippy::useless_conversion`；不等于严格全仓通过。 |
| 严格全仓 Clippy `--all-targets -- -D warnings` | 退出 101，原有 `cayw-device` 的非 macOS 分支出现 unused_mut、dead_code、useless_conversion。相关文件未修改。 |

全仓测试与 macOS-only `local_cli`、`model_setup` 测试没有在 macOS 执行；另一次包含 lib tests 的检查遇到原有 `results.rs` 非 macOS 测试缺失导入，保留在 earlier-attempts 日志中。该代码在基线中相同，不在本次扩展 Linux 支持。开发中发现并修复的本次测试借用问题和 Clippy 问题，其早期失败也被保留，最终结果不混用旧失败轮次。

## 交付范围与未完成验收

基础版本已连接统一启动、工作区选择、日常模型配置与切换、External 插件管理，以及会话/任务/审批/取消/重连工作流。网页切换另一项目目前需要 `cayw start --workspace DIR`，没有网页内启动其他工作区进程的界面。Editor、Trace 与 Pi 仍是独立可选组件，不在四程序基础发行包内；其已有工作台入口保留。

macOS arm64 原生构建、真实安装、升级替换失败后的四程序回滚和实际终端行为均未运行，仍需在对应平台验收；本环境无法提供这项证据。未运行真实付费模型或官方服务矩阵，也不据此宣称编码质量、性能、稳定性或正式发布成熟度已达到 DSH。
