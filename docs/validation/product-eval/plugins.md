# 插件效果与 plain 成对对照

[返回主方案](../product-eval.md)。本页区分“代码存在”“本地产品能接入”“真实任务得到收益”。只有最后一项才是效果结论。核对基线同主方案。

## 当前 lane 资格

| Lane | 当前真实入口 | 本次能证明什么 / 不能证明什么 |
| --- | --- | --- |
| plain | 本地 `cayw serve` + `cayw-client`/TUI/WebUI | 主基线，默认 CodeMode；无可选架构插件/MCP/Pi |
| External 架构 plugin | `cayw plugins install/enable`，Client 全局 inventory + 每工作区 activation | 有真实本地接入链；须提供实际有用的可执行程序与 descriptor 并完成握手/真实调用，才可加入效果比较；仓内 fixture 不算 |
| MCP stdio | 可信配置 `mcp_servers_file`，每工作区 Registry/进程 | 有真实接入链；必须使用已安装审查过的真实 server，并验证工具确实派发、结果参与最终产物 |
| Pi | 额外 `pi-compat` 构建 + 可信 `pi_compat` 配置 | 本地加载器目前明确处理 fixture、sample extension、npm hello。不能由这些示例推导任意 Pi 插件可用或对真实任务有效；默认发行包未包含，默认 N/A |
| StrongFlow | `Runtime::bind_strongflow/checkpoint_strongflow/resume_strongflow` 等 SDK 合同 | 具有实现不等于本地用户能选择执行。当前没有核实到发行 CLI 的原生注册/会话绑定入口；本地产品效果 lane N/A，缺真实宿主与连接证据时 blocked，不编造 `--plugin strongflow` |
| JEv / Fusion / Observer | 未核实到本地可运行产品入口 | N/A，不能把同名 crate、feature 或编译成功当效果 |
| TUI / WebUI | Client plugin，`cayw tui` / `cayw --ui web` | 是真实界面 lane，使用 U01；不是架构规划插件收益的替代 |

依据：`crates/cayw-runtime/src/cli_plugins.rs`、`cli_workspaces.rs`、`plugins.rs`、`main.rs`；`crates/cayw-sources/src/config.rs`；`scripts/package-cli.sh`。发行 Runtime 启用 `plugins`，不默认启用 `pi-compat`/`strongflow`。**没有统一“全插件模式”开关。** 不要为了让矩阵全绿而补写模拟 plugin 或只更换提示词。

## External 的预检与运行

只选择用户实际准备评估、来源可信、具有任务价值的本地程序。按[本地工作流](../../local-workflows.md#两类-plugin)准备私有 descriptor，角色必须是 `workspace_architecture`、kind 为 `external`，能力及权限声明必须与真实握手一致。先保存程序版本、SHA256、descriptor 脱敏副本、来源和安全审查结果。

```sh
# 使用主手册同一 BIN/RUN/CAYW_HOME；这里的 descriptor 和名称由实际插件提供。
"$BIN/cayw" plugins install --file /absolute/private/plugin.json
"$BIN/cayw" plugins list --workspace "$RUN/workspace"
"$BIN/cayw" plugins enable ACTUAL_PLUGIN_NAME --approve-process --workspace "$RUN/workspace"
"$BIN/cayw" plugins list --workspace "$RUN/workspace"
"$BIN/cayw" doctor --workspace "$RUN/workspace"
```

`install` 只记录并核对本地程序摘要，不代表已经运行；`enable --approve-process` 是明确进程授权，会执行真实握手。只有授权运行该程序后才能执行，不能照抄占位名称。External 使用清空环境，但仍以用户身份运行，**不是沙箱**；最小权限和隔离用户仍必要。

把同一 case 在全新 plain 和 External 工作区各运行一次，再重复三组。plain 不安装/启用扩展；不得把扩展跑后的文件复制给 plain。两组可见数据必须相同。用真实事件证明：能力公开给模型、模型调用、参数、Host 审批、插件返回、产物使用该结果。仅 `plugins list` 显示 enabled 算接入预检，不能算插件被模型采用。未采用记“已启用但未使用”，不要冒充正向收益。

另测一次普通 Session 在 plugin 停用后能否继续；如调用中崩溃/结果未知，先核对原调用和清理记录，不自动重启或重放。测试副作用只用一次性 fixture。StrongFlow 冻结/Fork 例外只在真实绑定 lane 已就绪时测，不把普通 External Session 误写为必然冻结。

```sh
"$BIN/cayw" plugins disable ACTUAL_PLUGIN_NAME --workspace "$RUN/workspace"
```

停用失败/清理未知应原样记录，不能删除状态清除阻断。更新或移除前须在所有相关工作区停用；本 eval 不需要修改用户日常 plugin inventory。

## MCP 的预检与运行

当前本地 importer 接受 `mcpServers` 对象（也接受其代码声明的兼容形状），其中 stdio `command` 必须是**已存在的绝对可执行路径**。远端 URL/SSE/HTTP transport、`npx`/`uvx` 等裸命令和相对命令会被跳过，不能声称导入任意 MCP 配置就可用。

在本次私有配置中增加真实 `mcp_servers_file` 绝对路径。文件示意如下，路径/参数必须替换为审查过的真实 server，不由模型生成：

```json
{
  "mcpServers": {
    "actual-reviewed-server": {
      "command": "/absolute/path/to/installed/server",
      "args": [],
      "env": {}
    }
  }
}
```

这不是现成插件，也不包含安装命令。不允许未审查自动下载/运行软件。配置/启动前确认该程序会访问什么数据；启动、目录配置或 tools list 成功都不是具体调用权限。只批准本次 fixture 所需精确操作。不要把生产凭据塞入 `env`；如任务确需外部服务，单独取得具体数据/接收方授权和预算。

Client 打开工作区时加载配置，按工作区独立准备 Registry/MCP；修改后正常停止并重新启动 Client，不依赖不存在的热加载开关。保留 installed/skipped 及跳过原因、真实 discovery/describe/call 事件；lazy 生命周期不能仅凭未看到进程就认定不可用。

成对任务可选择 C04 的本地资料检索：plain 读取原文件，MCP lane 访问内容完全相同的只读资料副本。若 MCP 获得 plain 没有的数据，结果应称“数据接入价值”，不得称“同等信息下推理更好”。固定 echo/add server 只能用于链路冒烟，不能代表 C04 的资料质量效果。

## 比较结果写法

先在 review_notes 预先声明插件为实验变量，并列出允许变化的 plugin_identity/tools_sha256（若必须换构建，也明确对应 source/features/binaries），其余模型、输入、预算、权限、环境、测量边界等逐项人工匹配。严格 report 不忽略这些字段，因此跨插件 lane 的 incomparable 是预期提示，不代表不能做人工成对消融分析。不得伪造相同工具摘要来绕过分层；记录至少三对相同 repetition 的运行、次序、所有失败及混杂因素，再单独报告描述性差值。

每个真实扩展列：入口/版本、可执行性、是否实际使用、最终正确率、人工帮助、请求数、时延、可核对成本、失败/unknown、可恢复性。先报告完成质量，再谈效率收益；额外上下文、权限、测试工具或预算都必须列为混杂因素。

若没有已接通且有任务价值的扩展，本轮如实交付 plain 结果及上表 N/A/blocked。这比用 fixture 假装完成 StrongFlow/JEv/Fusion/Observer 全矩阵更有用。
