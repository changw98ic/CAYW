# F14 MCP 生命周期 — 来源记录

- **功能编号**：F14（[issue #33](https://github.com/changw98ic/CAYW/issues/33)）；推进 [#9](https://github.com/changw98ic/CAYW/issues/9)
- **切片**：SLICE R / `feat/f14-mcp-lifecycle`
- **上游参考**：[nicobailon/pi-mcp-adapter](https://github.com/nicobailon/pi-mcp-adapter)（懒启动、空闲回收、外部 `mcpServers` 导入、prompts）
- **采用方式**：设计参考（非代码移植）
- **CAYW 新增**：
  - `cayw-sources::lifecycle`：`ManagedStdio` / `LifecycleMode` / 空闲回收与连接代次
  - `cayw-sources::config`：外部 `mcpServers` JSON 导入（仅绝对路径本地 stdio）
  - `cayw-sources::install`：产品入口 `install_imported` / `install_mcp_servers_file`，完成 import → `ManagedStdio::prepare` → `Registry::register`
  - Bridge/Host：`mcp.prompts.list` / `mcp.prompts.get`；stdio 发现 `prompts/list`
  - Headless：`HeadlessConfig.mcp_servers_file` → `cayw serve` 将导入源挂入 Runtime 使用的 Bridge
- **剩余限制（本切片明确不做）**：
  - 仅绝对路径本地 stdio：`npx`/`uvx`/相对路径、HTTP/SSE/远程 MCP、OAuth 一律跳过或拒绝
  - 非 stdio 传输与服务端 elicitation / 交互表单 UI
  - WebUI 直持连接改写（仍应经 Registry/CodeMode；WebUI 接线另切片）
  - 将 F14 或 #9 全范围标为完成
- **可复跑**：`cargo test -p cayw-sources --locked`（含 `lifecycle`、`config`、`install`、既有 stdio）；`cargo test -p cayw-runtime --test host_source_import --locked`
