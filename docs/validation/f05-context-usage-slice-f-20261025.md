# F05 / SLICE F 验收说明（2026-10-25）

## 范围

Issue [#33](https://github.com/changw98ic/CAYW/issues/33) F05 首刀：`cayw-context` 只读概览折叠。

分支：`feat/f05-context-usage`（基于 `dev`）。

## 本机命令

```sh
cargo test -p cayw-context --locked
cargo clippy -p cayw-context --all-targets --locked -- -D warnings
cargo run -p cayw-context --example offline_overview --locked
```

## 结果

- 单元测试覆盖：会话折叠、跨会话未知费用保持空、父子 Fleet 同屏、缓存命中率边界、异质价目合并为未知。
- 示例打印 Fleet 成员与跨会话 KPI；`cost=Unknown`。
- 无网络、无 Provider、无真实模型请求。

## Runtime 接线补充

```sh
cargo test -p cayw-runtime --test context_overview --locked
```

证明：`OfflineProvider` 真实落账 → `model_requests` → adapter → `session_context_overview`，以及 IPC `session.context_overview`。未知费用保持空。

## 不做声明

本记录不证明 F05 全范围完成，也不证明 TUI/WebUI 已接线。
