# cayw-output-filter

Issue [#33](https://github.com/changw98ic/CAYW/issues/33) **F02** / SLICE B 首刀：可选确定性命令输出过滤，保留错误、关键变化与摘要，并提供完整原文双入口。

## 能力

- `apply_filter`：开/关过滤；内置 `generic` / `cargo_like` / `git_status_like` / `passthrough`
- `dual_entry`：同一 `original_digest` 下取 `summary` 与 `original`
- `compare_filter_toggle`：同任务开/关对比；**输出减少量与 wall/cost/重读成本分列**，禁止把压缩率当作任务总收益

## Host 接线

`cayw-host` 的 `HostConfig::output_filter`（默认启用）在 process/git 结果呈现时调用本 crate；agent 侧 `stdout`/`stderr` **只含摘要**，并附 `output_filter` 元数据（digest/reduction）。完整原文不内联进 capability JSON（否则无真实减量）；`git.blame`/`log`/`show` 走 Passthrough 以免拆坏记录。`dual_entry` 仍供离线对比。

## 不做（本切片）

- 终端流式输出过滤
- 与 F01 结果句柄持久化对齐
- 移植 RTK TOML 规则或压缩引擎
- 将 F02 全范围标为完成

## 来源

设计参考 [rtk-ai/rtk](https://github.com/rtk-ai/rtk) 的 rtk-oss 命令过滤形状，**非代码移植**。见 `docs/sources/f02-output-filter.md`。

## 本地命令

```sh
cargo test -p cayw-output-filter --locked
cargo clippy -p cayw-output-filter --all-targets --locked -- -D warnings
cargo run -p cayw-output-filter --example offline_filter --locked
```
