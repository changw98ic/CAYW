# F08 / SLICE I 验收说明（2026-10-25）

## 范围

Issue [#33](https://github.com/changw98ic/CAYW/issues/33) F08 首刀：`cayw-plannotator` 结构化批注反馈。

分支：`feat/f08-plannotator-feedback`（基于 `dev`）。

## 本机命令

```sh
cargo test -p cayw-plannotator --locked
cargo clippy -p cayw-plannotator --all-targets --locked -- -D warnings
cargo run -p cayw-plannotator --example offline_feedback --locked
```

## 结果

- 单元测试覆盖：批注一条 → 后续 attempt 可见；版本匹配则 Applied；版本不匹配 / 缺 live 文件则拒绝静默套用；跨 Session 隔离；Deny 无评论拒绝。
- 示例打印匹配注入与版本冲突拒绝；无网络、无 Provider、无真实模型请求。

## 不做声明

本记录不证明 F08 全范围完成，也不证明浏览器 UI / Pi 扩展 / TUI 接线已交付。
