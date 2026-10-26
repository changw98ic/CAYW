# F15 / SLICE S 验收说明（2026-10-25，补丁 2026-10-26）

## 范围

Issue [#33](https://github.com/changw98ic/CAYW/issues/33) F15 首刀：可选 Pi→CAYW 兼容宿主与版本/兼容矩阵。

分支：`feat/f15-pi-compat-host`（基于 `dev`）。

时区：America/New_York。

## 本机命令

```sh
cargo test -p cayw-pi-compat --locked
cargo clippy -p cayw-pi-compat --all-targets --locked -- -D warnings
cargo run -p cayw-pi-compat --example offline_pi_compat --locked
```

## 结果

- 单元/集成测试覆盖：宿主关闭拒绝；`cayw-pi-fixture@0.1.0` 装→调→卸（`load_native`）；
  **`cayw-pi-sample-ext@0.1.0` 从磁盘装→调→卸（`load_external`）**；缺失可执行文件记 **unavailable**；
  `interaction.ui.select` 记 **incompatible**；钉扎 npm 包行 **unavailable**；
  `Cancelled` 保持已知终态（非 OutcomeUnknown）；矩阵 Markdown 含 fixture、sample 与 ABI 钉扎。
- 示例打印协议版本、fixture/sample 调用结果与矩阵表。
- 默认路径无外网、无 npm install；样例为 CAYW external JSON 协议子进程，非 npm ExtensionAPI。

## 不做声明

本记录不证明 F15 全范围完成，也不证明未修改的 npm Pi 插件可在 CAYW 上直接运行，或 Host/Bridge/WebUI 已默认启用该可选宿主。commands/session/events/UI 映射仍为说明级。
