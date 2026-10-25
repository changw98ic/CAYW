# F20 组合验收 — 来源记录

- **功能编号**：F20（[issue #33](https://github.com/changw98ic/CAYW/issues/33)）— 子任务与后台执行模块的**组合层**
- **切片**：SLICE Q / `feat/f20-combo-acceptance`
- **性质**：**组合验收**，不是新调度器，也不重开已关闭的 #11 / #18 绿场
- **上游底座（已在 `dev`）**：
  - [#11](https://github.com/changw98ic/CAYW/issues/11) 多 Session 调度 / 公平 / 恢复
  - [#18](https://github.com/changw98ic/CAYW/issues/18) 本机资源与 Device `ContentStore`
  - [#4](https://github.com/changw98ic/CAYW/issues/4) 进程身份与清理、[#7](https://github.com/changw98ic/CAYW/issues/7) 崩溃恢复围栏
  - Issue #5 本地 `agent.run` 子代理路径
- **封口依赖（并行切片，未合入前不宣称组合封存）**：
  - SLICE A — F01/F03 统一结果句柄
  - SLICE P — F13 持久后台命令与网关归属
  - SLICE R / F14 — MCP 生命周期（兼 #9）
  - 可选 SLICE O — F12 FleetView / 后台子代理
- **本切片交付**：验收清单文档 + 可复跑底座烟测骨架；不把 F20 / #33 复选框标完成
- **可复跑**：`./scripts/run-f20-combo-smoke.sh`；`cargo test -p cayw-runtime --test f20_combo_smoke --locked`
- **平台备注**：Linux 底座烟测不含 ContentStore 写入（已知 O_PATH/`EBADF`）；macOS 另启用 content 重开用例
