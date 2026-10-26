# F06 / SLICE G 验收说明（2026-10-25）

## 范围

Issue [#33](https://github.com/changw98ic/CAYW/issues/33) F06 首刀：`cayw-run-inspector` 只读运行检查折叠。

分支：`feat/f06-run-inspector`（基于 `dev`）。

## 本机命令

```sh
cargo test -p cayw-run-inspector --locked
cargo clippy -p cayw-run-inspector --all-targets --locked -- -D warnings
cargo run -p cayw-run-inspector --example offline_inspector --locked
```

## 结果

- 单元测试覆盖：两条失败重试路径可区分、A/B 同任务 turn 对齐与墙钟轨道、异任务跳过 turn 对齐、未恢复路径、比较输入校验。
- 示例打印 A 的恢复路径与 A/B 对齐轮次；无网络、无 Provider、无真实模型请求。

## 不做声明

本记录不证明 F06 全范围完成，也不证明迷宫 UI / Trace 接线已交付。


## 续：Runtime / Trace 接线（审计补齐）

- 翻译：`crates/cayw-run-inspector/src/from_runtime.rs`
- 入口：`Runtime::run_inspector` + RPC `session.run_inspector`；Trace `GET /api/run-inspector`
- 证明：`cargo test -p cayw-run-inspector --test from_runtime --locked`
- 证明：`cargo test -p cayw-runtime --test run_inspector --locked`
