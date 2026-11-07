# F20 组合验证来源与运行入口

- 功能：[Issue #33](https://github.com/changw98ic/CAYW/issues/33) 的 F20。
- 采用方式：CAYW 自行实现的组合测试。复用 Core、Runtime、Host、Device、Cells、ContentStore、Bridge 和认证 IPC。
- 原始分支：`feat/f20-combo-acceptance`，`d9733fd18fea36768ac285a6a63254b3fa850acd`。
- 当前集成底座：`dev`，`9cc509d66dff9300333ca0bf5af2e8b7868c0541`。
- 文件：`crates/cayw-runtime/tests/f20_combo_smoke.rs`、`f20_mcp_lifecycle.rs`、`f20_background.rs`、`integrated_ipc.rs`，以及 `scripts/run-f20-combo-smoke.sh`。
- 平台：当前首发 macOS。每次实际执行的平台与结果保存在该次 `summary.json`。

运行 `./scripts/run-f20-combo-smoke.sh`。脚本使用新目录保存 Cargo 原始日志和机器可读结果。摘要记录测试名称、跳过和失败项、源码 HEAD、已跟踪补丁摘要、未跟踪源码逐文件摘要及具体覆盖场景。脚本在运行前后核对同一份源码。每个要求场景都实际通过且运行期间源码未变时，脚本才返回成功。

组合场景包括会话隔离、子任务归属、结果读取和释放、后台任务重开、MCP 进程生命周期、SDK 与认证 IPC 对照。子任务、Host 进程和 MCP 的组合保留原 Task 与调用终态，重开后检查没有重复执行。模型使用本地固定响应，工具和 SQLite 操作实际执行。合成用量仅用于检查记账，不能用作真实费用证据。

浏览器操作、TUI 交互和真实模型任务分别记录验证结果。本脚本的成功只证明它列出的场景，不单独代表 F01–F20 全部需求完成。历史分支验证记录仍只证明其注明的源码和范围。
