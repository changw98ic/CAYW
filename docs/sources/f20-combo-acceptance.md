# F20 组合验收 — 来源记录

- **功能编号**：F20（[issue #33](https://github.com/changw98ic/CAYW/issues/33)）— 子任务与后台执行模块的**组合层**
- **切片**：SLICE Q / `feat/f20-combo-acceptance`
- **性质**：**组合验收**，不是新调度器，也不重开已关闭的 #11 / #18 绿场；**不要求**本 PR 同树合并 #37–#54
- **上游底座（已在 `dev` / 本分支 tip）**：
  - [#11](https://github.com/changw98ic/CAYW/issues/11) 多 Session 调度 / 公平 / 恢复
  - [#18](https://github.com/changw98ic/CAYW/issues/18) 本机资源与 Device `ContentStore`
  - [#4](https://github.com/changw98ic/CAYW/issues/4) 进程身份与清理、[#7](https://github.com/changw98ic/CAYW/issues/7) 崩溃恢复围栏
  - Issue #5 本地 `agent.run` 子代理路径
  - Bridge MCP `mcp.resources.*` fixture 路径（非完整 F14 生命周期）
- **本分支 tip 已证明**：Q1 Session 重开、Q2 跨 Session、Q3 `agent.run` 子代理、Q4 进程清理+CapabilitySettled 重开（unix）、Q6 MCP resources 同 Session 重开
- **仍 pending（诚实缺口）**：
  - SLICE A — `result.release`（Q4 完整封口半边）
  - SLICE P — F13 `bg.*` + 网关 Ownership（Q5）
  - SLICE R / F14 — MCP 懒起/空闲停（Q6 完整封口半边）
  - Q7 WebUI/TUI/SDK 认证 IPC 同场；Q8 多模块同树总表
  - 可选 SLICE O — F12 FleetView
- **可复跑**：`./scripts/run-f20-combo-smoke.sh`；summary 中 `f20_combo_complete` 在仍有 ignored/gaps 时为 false
- **平台备注**：Linux 底座烟测不含 ContentStore 写入（已知 O_PATH/`EBADF`）；macOS 另启用 content 重开用例；Q4 进程测 `cfg(unix)`
