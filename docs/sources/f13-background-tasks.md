# F13 持久后台命令 — 来源记录

- **功能编号**：F13（[issue #33](https://github.com/changw98ic/CAYW/issues/33)）；兼 F20 网关归属一部分
- **切片**：SLICE P / `feat/f13-background-tasks`
- **上游参考**：[ismailsaleekh/pi-background-tasks](https://github.com/ismailsaleekh/pi-background-tasks)（命名任务、日志路径、完成通知、`running|completed|failed|killed`）
- **采用方式**：设计参考（非代码移植；**不含**该包 Fusion 工作流）
- **CAYW 新增**：
  - `cayw-runtime::background`：`bg.run` / `bg.status` / `bg.list` / `bg.logs` / `bg.kill` / `bg.result`；SQLite 元数据 + 日志索引；完成时 `background_task_completed` 控制事件归所属 Session
  - 启动仅选 **Host 进程 profile**（不向 Agent 暴露自由 shell/argv）
  - `cayw-webui::OwnershipStore`：Session/Task → device 持久映射，补内存 `HashMap` 重启丢失缺口
- **边界（本切片不做）**：
  - 任意 shell 命令行、Fusion、跨 Runtime 进程领养 OS 子进程
  - 将 F13 / F20 全范围标为完成；设备常驻 daemon 真机验收另记
- **可复跑**：`cargo test -p cayw-runtime --lib background:: --locked`；`cargo test -p cayw-runtime --test background --locked`；`cargo test -p cayw-webui ownership --locked`
