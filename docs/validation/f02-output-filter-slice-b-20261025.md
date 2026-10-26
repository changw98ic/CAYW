# F02 / SLICE B 验收说明（2026-10-25）

## 范围

Issue [#33](https://github.com/changw98ic/CAYW/issues/33) F02 首刀：`cayw-output-filter` 可选确定性命令输出过滤与原文双入口。

分支：`feat/f02-output-filter`（基于 `dev`）。

时区：America/New_York。

## 本机命令

```sh
cargo test -p cayw-output-filter --locked
cargo clippy -p cayw-output-filter --all-targets --locked -- -D warnings
cargo run -p cayw-output-filter --example offline_filter --locked
```

## 结果

- 单元测试覆盖：开/关过滤双入口、输出减少量、cargo/git profile、digest 不一致拒绝、disclaimer。
- 示例打印同一捕获的 summary 与 compare disclaimer。
- 默认路径无外网、无真实模型计费请求。

## 不做声明

本记录不证明 F02 全范围完成，也不证明 Host/Runtime/终端已接线或 F01 句柄已对齐。输出压缩率不得写成任务总收益。

## Host 接线补记

`HostConfig::output_filter` 默认启用；`cayw-host` 的 process/git 结果路径调用 `apply_filter`/`dual_entry`，agent/UI 看到的 `stdout`/`stderr` 为摘要并保留 `*_original`。

```sh
cargo test -p cayw-host --test output_filter --locked
cargo test -p cayw-output-filter --locked
```

不声明终端流式过滤或 F01 句柄对齐已完成。
