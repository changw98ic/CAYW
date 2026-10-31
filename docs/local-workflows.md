# 本地工作流

完整发行包包含同一源码版本构建的 `cayw`、`cayw-client`、`cayw-tui`、`cayw-webui`。首发范围仍为 macOS arm64。Core Only 可单独安装 Runtime，UI 和 plugins 不是 Core 依赖。

```sh
cayw start --workspace /你的项目          # 默认网页
cayw start --workspace /你的项目 --ui tui
cayw start --workspace /你的项目 --ui none # 仅服务
```

`start` 在前台管理自己启动的进程。网页启动结果为一行 JSON：`url` 为本机地址，`web_token_file` 为私有令牌文件路径；读取该文件，将内容粘贴到网页登录框。令牌自动生成，与 IPC 令牌独立，不进入 URL、HTML 或浏览器持久存储。刷新网页后重新输入同一枚网页令牌即可读取原会话，不自动重发任务。退出本次启动后令牌失效，下次启动生成新令牌。

工作区由规范化后的目录路径确定，数据库位于 `CAYW_HOME/workspaces/<目录摘要>/`。网页设置展示当前目录与已登记目录；切换目录使用 `cayw start --workspace DIR`，不会把已有会话或执行器换根。当前版本没有在网页内直接启动另一工作区的进程。移动项目目录后会产生另一工作区身份，不自动搬迁数据库。

重复运行同一工作区的 `start` 会返回现有启动信息；正在初始化时返回 `starting: true`。若 `serve` 已独立启动，`start` 复用该服务，退出界面保留它。已有服务不能通过第二条启动命令修改 `--offline`、`--config` 等启动参数。Ctrl+C / SIGTERM 停止本次拥有的 UI 与 Runtime，并等待原有任务取消和持久化清理；若清理未确认会报错，不自动重放任务。

## 模型日常管理

启动不调用模型；保存方案也不调用模型。可先启动无模型服务，再在网页“工作区 / 模型 / 插件”添加并使用方案。填入服务、API、模型名称、端点、版本、输出限制及密钥**环境变量名称**。密钥值不进入配置，也不由网页收集。使用方案时，服务只读取选中变量；服务启动时必须已经有该环境变量。新增环境变量需要重新启动进程，已有变量对应的模型切换不需要重启。

CLI 通过相同的认证 IPC 接口管理：

```sh
cayw models add daily --file /path/model.json --workspace /你的项目
cayw models list --workspace /你的项目
cayw models use daily --workspace /你的项目
cayw models remove unused --workspace /你的项目
```

`model.json` 是原 `config.json` 的 `model` 对象，不包含预算或权限。最多保存 64 个方案，名称允许字母、数字、`._-`。方案保存在当前工作区的 `models.json`，选中方案优先于旧 `config.json.model`。旧配置仍可启动；`cayw configure` 支持替换已有模型，取消或输入中断保留原文件，保存后尝试在当前工作区的运行服务中应用名为 `configured` 的方案，结果中的 `activation` 明确区分已保存与已应用。配置预算、工具权限及进程 profile 仍通过启动配置选择，模型管理不修改它们。

切换对后续开始的 Task Attempt 生效，包括尚未派发的任务。每个运行中的 Attempt 固定 Provider、模型名称和输出上限，后续工具回合继续使用同一快照，持久账本记录实际模型身份。切换不增加预算，不清除已消费或未知预留。不能删除选中的方案；先选择另一方案。`--offline` 明确使用合成 Provider，拒绝选择真实模型方案。

## 本地 External 插件

发行包的 Runtime 使用 `plugins` feature。单独的无插件构建仍可使用普通聊天；插件设置接口会明确返回不支持。可信 Native 插件继续通过 Cargo feature 静态链接，不支持任意 npm、URL、Rust 动态库安装或在线更新。

安装来源是本机 External IPC 描述 JSON：

```json
{
  "executable": "/absolute/path/my-plugin",
  "argv": [],
  "manifest": {
    "name": "my-plugin", "version": "1.0.0", "protocol_version": 1,
    "kind": "external", "capabilities": [], "permissions": {}
  }
}
```

登记时检查本机可执行文件、清单协议版本、能力 schema 和参数边界，记录可执行文件 SHA256；不启动程序，也不下载或复制源码。描述文件不接受环境变量映射或 shell 命令。External 进程使用清空的环境，不继承模型凭据。进程隔离不是安全沙箱，插件仍以本机用户身份运行；启用前必须审阅路径、参数、能力与权限，并明确批准执行。

```sh
cayw plugins install --file /path/local-plugin.json
cayw plugins list
cayw plugins enable my-plugin --approve-process
cayw plugins bind my-plugin --session SESSION_ID
cayw plugins disable my-plugin
cayw plugins update my-plugin --file /path/new-local-plugin.json
cayw plugins remove my-plugin
```

各命令可添加 `--workspace DIR`。网页提供相同操作、实际可用状态和错误原因；启用必须勾选进程批准，成功后清空勾选。启用会核对已登记可执行文件的摘要并运行真实协议握手；失败保留安装登记与错误，保持停用，不自动重试。摘要只覆盖可执行文件，不覆盖 argv 指向的脚本、解释器加载的模块或其他资源，批准前须自行审阅这些来源。

停用先持久化停止意图，再经 Runtime checkpoint/freeze 卸载；停止意图保存失败时保留当前实例并返回错误。进程握手或持久化边界的结果不明确时返回 `settings_outcome_unknown`，先读取实际状态，不自动重试。

更新必须先停用，保持同名，验证失败保留原登记。更新只登记本地新版本，成功后仍需重新批准启用。卸载只删除登记和停止对应实例，不删除用户的本机可执行文件。已批准启用的插件在 Runtime 重启时按同一路径、参数及摘要恢复；恢复失败转为停用并记录错误。

启用不自动绑定所有会话。绑定通过 Runtime 原有接口检查会话空闲和插件身份；能力调用仍经过 Host 精确权限、预算和持久化。停用、卸载或替换会先走 Runtime 原有 checkpoint/freeze 路径，已绑定会话保持冻结，重新启用不会静默换绑。用户可在原有网页流程中明确确认 Fork 为普通聊天；未知副作用仍需先核对。

## 打包和验证

在 macOS arm64 源码目录运行 `sh scripts/package-cli.sh /新的输出目录`，构建四个程序和启用 plugins 的 Runtime，生成 `BUILD.json`、`SHA256SUMS` 与安装脚本。不会安装、推送或发布。包内 `install.sh` 对四个程序检查摘要、暂存文件和 `--help`，替换失败时尝试恢复备份。可视化编辑器、Trace 调试与 Pi 兼容仍是独立可选组件，不随这个基础包安装；工作台保留其已有入口和独立认证。

复跑入口：

```sh
cargo fmt --all -- --check
cargo check --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test -p cayw-runtime --features plugins --test model_switch --locked
cargo build -p cayw-runtime --features plugins --bin cayw --bin cayw-runtime-plugin-fixture -p cayw-webui --bin cayw-webui --locked
python3 scripts/validate-product-workflows.py target/debug /新的证据目录
CAYW_PLAYWRIGHT_MODULE=/绝对路径/playwright/index.mjs node scripts/validate-product-settings-browser.mjs target/debug /另一个新证据目录
```

这些验证只用合成 Provider、离线 Fixture、本机进程与临时配置，不加载用户模型配置或真实密钥，不调用外部付费模型。平台和实际运行范围见对应验证记录，其他平台的开发结果不构成 macOS 验收。
