# F08 / SLICE I 验收说明（2026-10-25）

## 范围

Issue [#33](https://github.com/changw98ic/CAYW/issues/33) F08 首刀：`cayw-plannotator` 结构化批注反馈。

分支：`feat/f08-plannotator-feedback`（基于 `dev`）。

## 本机命令

```sh
cargo test -p cayw-plannotator --locked
cargo test -p cayw-runtime --test plannotator_feedback --locked
cargo clippy -p cayw-plannotator --all-targets --locked -- -D warnings
cargo run -p cayw-plannotator --example offline_feedback --locked
```

## 结果

- 单元测试覆盖：批注一条 → 后续 attempt 可见；版本匹配则 Applied；版本不匹配 / 缺 live 文件则拒绝静默套用；跨 Session 隔离；Deny 无评论拒绝；Applied → inject section。
- Runtime 集成：`plannotator.submit` 持久化并可 reopen；下次 `prompt` 携带 `cayw-plannotator-feedback` SystemMessage section。
- 持久化一致性：Core 落盘成功后再 commit 内存；落盘失败时 list/inject 均不可见（SQLite trigger 模拟）。
- 示例打印匹配注入与版本冲突拒绝；无网络、无 Provider、无真实模型请求。

## 不做声明

本记录不证明 F08 全范围完成，也不证明浏览器批注 UI / Pi 扩展 / 完整 TUI 编辑器已交付。
