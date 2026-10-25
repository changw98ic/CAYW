# cayw-run-inspector

CAYW 运行检查器（F06 首刀，SLICE G）：通用**重试/恢复路径**、**耗时轨道**与**会话间比较**。

从原生运行步骤快照折叠只读视图，**不编造结局**、不调用 LLM、不改写持久证据；缺耗时或证据游标时保持空值。补齐 Trace 时间线之外的失败链与跨会话对齐比较。

## 首刀范围

- 重试/恢复路径：`RetryPath`（`SameParamsRetry` / `ChangedParams` / `ChangedTool` / `Unrecovered`）
- 耗时轨道：`DurationTrack`（会话本地或墙钟对齐）
- 会话比较：`SessionCompare`（同 `task_fingerprint` 时按 turn 对齐）
- 单会话时间线：`RunTimeline`
- 协议版本：`RUN_INSPECTOR_PROTOCOL_VERSION = 1`

本切片**不**交付：迷宫 UI / 回放、CodeMode 内部工具 span、源码关联、节点重新求值接线、TUI/WebUI 渲染。

## 快速使用

```rust
use cayw_run_inspector::*;

let timeline = build_run_timeline(&snapshot_a);
let compare = compare_sessions(&[snapshot_a, snapshot_b])?;
assert!(compare.same_task);
```

## 来源与边界

- 功能：F06（[#33](https://github.com/changw98ic/CAYW/issues/33)）
- 采用方式：设计参考（非代码移植）
- 参考：[lamost423/dsh-maze](https://github.com/lamost423/dsh-maze)（原名 dsh-trace-compare）的执行时间线 / 失败恢复链 / 耗时轨道 / 多会话对比产品形状
- 记录：[`docs/sources/f06-run-inspector.md`](../../docs/sources/f06-run-inspector.md)

## 验证

```sh
cargo test -p cayw-run-inspector --locked
cargo clippy -p cayw-run-inspector --all-targets --locked -- -D warnings
cargo run -p cayw-run-inspector --example offline_inspector --locked
```

测试为本地纯函数折叠，无网络、无 Provider、无真实 UI。
