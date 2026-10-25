# F09 / F19 / SLICE J+K 验收说明（2026-10-25）

## 范围

Issue [#33](https://github.com/changw98ic/CAYW/issues/33) F09 HTML 产物 + F19 宿主动作协议首刀：`cayw-artifacts`。

分支：`feat/f09-f19-artifact-actions`（基于 `dev`）。

## 本机命令

```sh
cargo test -p cayw-artifacts --locked
cargo clippy -p cayw-artifacts --all-targets --locked -- -D warnings
cargo run -p cayw-artifacts --example offline_artifacts --locked
```

## 结果

- 单元测试覆盖：HTML 预览与 Session 列表、重启快照恢复、只读动作、同 Session follow-up 意图、跨 Session 拒绝、版本不匹配拒绝、动作状态刷新恢复、外 Session 更新拒绝。
- 示例打印预览字节、follow-up Accepted 与跨 Session Rejected，以及恢复后产物/动作数量；无网络、无 Provider、无真实模型请求。

## 不做声明

本记录不证明 F09/F19 全范围完成，也不证明 WebUI 渲染或 Runtime 真实启任务已交付。
