# F13 持久后台命令 — 来源记录

- **功能编号**：F13（[issue #33](https://github.com/changw98ic/CAYW/issues/33)）；兼 F20 网关归属一部分
- **切片**：SLICE P / `feat/f13-background-tasks`
- **上游参考**：[ismailsaleekh/pi-background-tasks](https://github.com/ismailsaleekh/pi-background-tasks)（命名任务、日志路径、完成通知、`running|completed|failed|killed`）
- **采用方式**：设计参考（非代码移植；**不含**该包 Fusion 工作流）
- **CAYW 新增**：
  - `cayw-runtime::background`：`bg.run` / `bg.status` / `bg.list` / `bg.logs` / `bg.kill` / `bg.result`；SQLite 元数据 + 日志索引；完成时 `background_task_completed` 控制事件归所属 Session
  - 启动仅选 **Host 进程 profile**（不向 Agent 暴露自由 shell/argv）
  - `cayw-webui::OwnershipStore` + `DeviceRouter`：Session/Task → device 持久映射，并按归属选择 IPC 连接（无条目才回退默认）
- **边界（本切片不做）**：
  - 任意 shell 命令行、Fusion、跨 Runtime 进程领养 OS 子进程
  - 将 F13 / F20 全范围标为完成；设备常驻 daemon 真机验收另记
- **可复跑**：`cargo test -p cayw-runtime --lib background:: --locked`；`cargo test -p cayw-runtime --test background --test f20_background --locked`；`cargo test -p cayw-webui --locked`（含 OwnershipStore 恢复路由到已注册设备 IPC）

## 本地集成后的恢复

后台任务持久记录自己的执行者、根进程和已观察后代的完整启动身份。正常终态必须有该后台任务自己的 Device 清理确认。父模型任务的清理、单独的 PID 或探测错误都不能替代这份确认。

重开时，仍持有任务锁的执行者继续拥有后台任务；确实失去执行者的任务进入 `outcome_unknown`，不自动重放。缺少完整进程身份、日志写入失败或终态保存失败时也保留未知状态。

公开 Rust 宿主可以调用 `Runtime::background_confirm_cleanup(id, evidence, verify_stopped)`。回调收到 `BackgroundCleanupInspection`，包含后台任务、原执行者、已记录进程和身份缺失标记。宿主必须用独立记录核对停止情况；缺少身份不能靠空列表推断成功。清理确认绑定原任务归属与完整身份快照，确认本身不修改业务终态。

宿主随后调用 `Runtime::background_recover(id, status, evidence)`，显式结算已核对的未知结果。普通说明文字没有清理证明时不能解除未知状态；已经确定的终态不能被改写。这条恢复路径不会接管或重新执行原操作。
