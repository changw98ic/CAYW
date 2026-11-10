# 本地工作流

完整发行包包含同一源码版本构建的 `cayw`、`cayw-client`、`cayw-tui` 和 `cayw-webui`。首发范围是 macOS arm64。Core Only 可以单独安装 Runtime；UI 和 plugin 不是 Core 依赖。

## 唯一 Client

一台设备只运行一个本地 Client。所有工作区共享它的 IPC 地址、访问码、连接、重连和进程监管。工作区不保存自己的连接状态或 Runtime 地址。

每个工作区仍有独立的规范目录、SQLite、Session、模型方案、MCP/Pi 实例和架构插件启用关系。Client 在进程内打开这些工作区，并按 `workspace_id` 路由请求。一个页面选择工作区，不会改变其他页面的选择，也不会把已经提交的任务换到另一个工作区。

```sh
cd /你的项目
cayw                                  # 启动或复用唯一 Client，打开 TUI
cayw --workspace /另一个项目           # 选择初始工作区
cayw start                            # 与裸命令相同
cayw --ui web                         # 启动或复用 Client 管理的 WebUI
cayw --ui web --no-open               # 只输出普通 URL
cayw --ui none                        # 只运行 Client
cayw --ui web --lan 192.168.1.10:3080  # 显式开放局域网入口
```

并发启动命令通过全局启动锁决定谁创建 Client。其他命令等待并连接同一个健康 Client。重复 WebUI 启动复用同一个网页进程和网关。TUI、WebUI 和后续 Client 类 plugin 都由 Client 统一启动、停止和监管。

Client 只在 `<CAYW_HOME>/client.json` 发布全局发现信息。启动结果和 doctor 使用 `client_file` 指向这个文件。工作区不创建 `connection.json`，也没有旧的公开 `connection_file` 字段。doctor 只报告发现的旧工作区连接文件，不迁移或删除它们。

`cayw tui` 是只连接入口。`cayw serve` 可以显式启动 Client。退出一个复用 Client 的界面不会停止 Client；创建 Client 的前台命令收到 Ctrl+C 或 SIGTERM 后会停止它，并等待所有已打开工作区和 Client plugin 清理。清理结果不确定时会报错，不会自动重放任务。

## 工作区

工作区由规范化目录确定。状态位于 `CAYW_HOME/workspaces/<目录摘要>/`。先登记目录，再从 TUI 或 WebUI 打开：

```sh
cayw init --workspace /你的项目
```

WebUI 只提交登记后的工作区标识，不接受任意目录路径。移动项目目录会产生新的工作区身份，不会自动搬迁数据库。Client 最多打开有界数量的工作区；打开失败不会替换已经打开的工作区。

浏览器 RPC 在顶层携带 `workspace_id`。Client 内部通过 `client.workspace.call` 把请求交给对应工作区。内部 IPC 地址和访问码不会返回浏览器。刷新页面只读取已有 Session 和事件，不重新提交任务。

## 无模型启动与首次配置

Client、TUI 和 WebUI 不要求先配置模型。启动不发送模型请求，也不弹出配置向导。创建和查询 Session 可用。第一次 `prompt`、`enqueue` 或 `resume` 在任务准入前返回 `model_not_configured`，并提示运行：

```sh
cayw conf --workspace /你的项目
```

`cayw conf` 是本地产品唯一的首次模型配置命令。没有 `cayw configure` 兼容入口。它把当前工作区的 `configured` 方案写入私有 `models.json` 并选中该方案。模型字段包括服务、API、模型名称、端点、版本、输出上限和可选 `api_key`。新建方案时密钥可以留空，表示服务不需要认证。编辑已有方案时，同一服务地址下留空会保留原密钥；删除必须明确选择清除。更换服务或端点时必须填写新密钥或明确清除，旧密钥不会自动转移。填写后，密钥以明文保存在普通本地文件中。CAYW 不使用 Keychain、加密或 OAuth。

模型方案示例：

```json
{
  "service": "openai",
  "api": "chat_completions",
  "id": "local-model",
  "base_url": "http://127.0.0.1:8000/v1",
  "api_version": "v1",
  "max_output_tokens": 4096,
  "api_key": "optional-plain-text-key"
}
```

CLI 通过同一个认证 Client 管理方案：

```sh
cayw models add daily --file /path/model.json --workspace /你的项目
cayw models list --workspace /你的项目
cayw models use daily --workspace /你的项目
cayw models remove unused --workspace /你的项目
```

公开模型视图、启动输出、doctor 和 Settings RPC 只显示 `authentication: plain_file` 或 `none`，不返回密钥。切换只影响后续开始的 Task Attempt。运行中的 Attempt 保留原 Provider、模型和输出上限。

如果先前模型请求已经发出，但 Runtime 没有收到完整用量，该请求按有界预留只扣记一次并立即释放占用，记录保留用量未知标志，不把未知写成零。未配置 token 上限时，这笔扣记不阻塞后续请求；配置了上限时，只要已用量仍小于上限，发送下一条消息仍可直接获准。TUI 和 WebUI 都使用普通发送路径，按 `task.enqueue`、`task.resume` 的顺序执行，不预览、不申请也不续期模型预算。配置的上限用完时，下一次请求返回 `budget_exceeded`，界面显示一句明确的预算耗尽说明，保留草稿且不自动重发。

本地 `config.json.model` 不是长期配置源。发现旧字段时，Client 把它单向迁移到当前工作区的 `models.json`，然后删除旧字段。已有选中方案优先，损坏的旧字段不会覆盖它。

完整显式 Headless 是另一条系统边界：同时提供 `--db`、`--token-env` 和 `--config` 时，不读写默认数据根，`--config` 中的 `model` 是唯一模型来源，本地 Settings 接口不可用。它仍使用相同的可选明文 `api_key` 字段。

## 本地网页和可选账号

本地 WebUI 只监听回环地址。它没有登录、网页 token 或 launch ticket。普通 URL 可直接访问。`--no-open`、SSH 环境或无桌面环境会跳过系统浏览器；服务仍保持可用。

`--lan IP:PORT` 必须显式选择 WebUI。未配置账号时，LAN 请求直接放行，终端和页面显示暴露提示。Host、Origin 和 Sec-Fetch-Site 仍会校验，RPC 参数和凭据不能放进 URL。

局域网启动同时提供两个入口：

- LAN URL 面向网络访问。
- `local_url` 只允许真实回环连接，用于本机管理。

账号配置在本机 WebUI 的“设置 → 局域网访问账号”中完成。保存后立即应用于当前 LAN 入口，并撤销旧登录会话。本机入口始终免登录。远端访客不能配置账号，也不能利用 Forwarded 或 X-Forwarded-For 伪装成本机。

账号文件是 Client 级 `CAYW_HOME/web-account.json`。它保护当前网页服务中的所有工作区，只保存用户名和加盐 Argon2id 摘要，使用 0600 原子写入。CLI 备用入口为：

```sh
cayw account --username admin
cayw account --username admin --file /私有目录/account.json
```

CLI 不覆盖已有账号。LAN 当前使用 HTTP，只适合可信网络。本阶段不增加 TLS、远程账号管理或多用户权限系统。

## 两类 plugin

Client 根据 plugin 类型管理生命周期。

- Client plugin：TUI、WebUI 等设备级入口。它们连接唯一 Client，不能按工作区保存连接状态。
- WorkspaceArchitecture plugin：StrongFlow、JEv、Fusion、Observer 和本地 External 架构 plugin。它们按工作区启用，但进程仍由唯一 Client 启动、停止和监管。

本地 External descriptor 必须声明 `role: "workspace_architecture"` 和 `kind: "external"`：

```json
{
  "executable": "/absolute/path/my-plugin",
  "argv": [],
  "manifest": {
    "name": "my-plugin",
    "version": "1.0.0",
    "protocol_version": 1,
    "role": "workspace_architecture",
    "kind": "external",
    "capabilities": [],
    "permissions": {}
  }
}
```

安装清单是 Client 全局状态。启用关系属于工作区：

```sh
cayw plugins install --file /path/local-plugin.json
cayw plugins list --workspace /你的项目
cayw plugins enable my-plugin --approve-process --workspace /你的项目
cayw plugins disable my-plugin --workspace /你的项目
cayw plugins update my-plugin --file /path/new-local-plugin.json
cayw plugins remove my-plugin
```

安装和更新只校验本地文件并记录可执行文件 SHA256，不运行程序。启用需要明确的进程批准，并核对摘要和真实协议握手。清单中的程序后来缺失时，Client 仍可启动；`plugins list` 和 doctor 把该条标为不可用，其他 plugin 与普通会话继续工作。移除清单不要求缺失程序重新出现。External 进程使用清空的环境，不继承模型密钥。进程隔离不是安全沙箱；它仍以本机用户身份运行。

普通 WorkspaceArchitecture plugin 不绑定普通 Session。它被停用、卸载或崩溃时，普通 Session 不冻结，后续对话仍可继续。进行中的调用若无法确认结果，返回 `outcome_unknown`；Client 不自动拉起 plugin，也不重放旧调用。External plugin 的退出确认有时间上限。终止、等待或超时无法确认清理时，Client 保存原 PID、启动时间、命令和进程组，阻止同一实例被当成已停止。重试只重新观察原进程，不启动新进程；PID 被复用、任意说明文字或缺少身份都不能清除阻断。只有精确原进程和进程组确认停止后，才允许清除对应记录并再次启用。

StrongFlow 是专用例外。StrongFlow Session 保留版本化 binding、checkpoint、冻结和恢复合同。StrongFlow 缺失时，原 Session 保持冻结。用户只能显式确认 Fork 为新的普通 Session；原 Session 和证据不变。

更新或移除前，必须先在所有工作区停用该 plugin。摘要变化或恢复失败会保留启用意图和错误状态，不静默改成成功。

## MCP 和 Pi

MCP 与 Pi 配置由 Client 在打开每个工作区时准备。每个工作区获得独立 Registry、MCP 进程和 Pi 实例。相同 source ID 不会让两个工作区共享进程或调用计数。

如果后续 Pi、Runtime 或架构 plugin 初始化失败，Client 会关闭已经启动的 MCP/Pi 资源。Client 正常退出时也会清理所有已打开工作区的这些资源。无法确认清理时返回错误，不把它当作已停止。

## doctor

`cayw doctor` 是只读检查。它报告本地状态、模型方案、Client、Client plugin、架构 plugin 和旧状态文件，但不会启动进程、重写配置、删除清理记录或迁移数据。

```sh
cayw doctor --workspace /你的项目
```

旧模型迁移发生在正常初始化或启动路径，不发生在 doctor。

## 打包和验证

在 macOS arm64 源码目录运行：

```sh
# 冷缓存构建先准备两个锁定 workspace 的依赖（此步可能联网）
cargo fetch --locked --target aarch64-apple-darwin
cargo fetch --locked --target aarch64-apple-darwin --manifest-path examples/headless-client/Cargo.toml
sh scripts/package-cli.sh /新的输出目录
```

安装包包含四个真实程序（`cayw`、`cayw-client`、`cayw-tui`、`cayw-webui`），其中 `cayw-client` 从独立的 `examples/headless-client` workspace 构建。固定目标为 `aarch64-apple-darwin`，Runtime 只启用 `plugins`，不是所有 feature/plugin 的发行包；不包含 `pi-compat`、`strongflow` 或其他可选组合的验收承诺。

安装包生成 `BUILD.json`、`THIRD-PARTY-NOTICES`、`SHA256SUMS` 和 `install.sh`，并附带需要单独分发的第三方许可文本。许可清单按锁定依赖和实际打包 feature 离线生成；许可证不在允许范围或缺少所需文本时，打包失败。上述文件都进入 `SHA256SUMS`。构建要求干净的已提交源码，并拒绝构建过程中已提交版本或跟踪源码发生变化。`BUILD.json` 留在包中，不会复制为安装目录的版本注册表。安装不联网、不使用 sudo、不修改 shell 配置。升级须先正常退出全部 CAYW；运行中拒绝、回滚和强制中断后的人工恢复限制见[安装指南](headless-chat.md#安装)。

真实发行安装验收（仅 macOS arm64，输出目录必须不存在）：

```sh
python3 scripts/validate-release-install.py /发行包目录 /新的证据目录
```

该入口使用全新临时 HOME 和 CAYW_HOME，运行安装后的真实程序，覆盖无模型默认 TUI 的真实 PTY 启动/连接/Ctrl+Q 退出、WebUI/Client 启动、独立 IPC 客户端创建/查询 Session、执行任务明确拒绝、同目录及另一安装目录的运行中 Client 阻止升级、正常停止/重启、不同文件的替换拒绝与强制替换、持久 Session 保留，以及无需模型的固定响应离线文件任务。旧程序替换部分使用合成可执行文件，不是历史 release 或跨版本数据库迁移证据。TUI 只覆盖启动、连接及退出，不代替完整交互终端验收；不包含浏览器自动化。

证据目录仅保存命令日志、构建清单、二进制摘要和 `summary.json`；临时 IPC 凭据与数据库不放入 CI artifact。任何失败或未确认清理都不能记为通过。Actions 中的发行 artifact 是候选包，不是已发布 release。

安装器故障注入可独立复跑：

```sh
python3 scripts/test-install-cli.py
```

此脚本显式模拟 uname、进程列表和可执行文件，覆盖校验/替换拒绝、TERM 中断、安装后自检失败、完整回滚、SIGKILL 留存备份及失败回滚后的再次安装拒绝。这些是合成 shell 流程测试，不是 macOS 产品通过证据；真实包验收仍需上面的 macOS 命令。

主要复跑入口：

```sh
cargo +1.95.0 fmt --all -- --check
cargo +1.95.0 check --workspace --locked
cargo +1.95.0 test --workspace --locked
cargo +1.95.0 clippy --workspace --all-targets --locked -- -D warnings
cargo +1.95.0 run -p cayw-agent --example offline --locked
```

浏览器验收使用 `scripts/validate-browser-launch.mjs`。它在临时私有 HOME 中启动唯一 Client、两个工作区和真实浏览器，使用离线 Provider 和回环网络。普通测试不读取用户模型配置，不调用真实或付费模型。真实模型测试仍需要当次明确授权。
