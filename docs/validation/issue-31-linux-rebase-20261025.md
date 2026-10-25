# Issue #31：Linux 变基与 Host 目录同步修复

日期：2026-10-25（America/New_York）。范围：将 `plugin/trace-debug` 变基到 `dev` tip `ac17893`，并在 Linux x86_64 上复跑 `cayw-trace-debug` 包测试。

## 变更

1. 将 `d0e1390` 变基到 `ac17893`。
2. 同步引入 Linux `O_PATH` 目录 `fsync` 修复（与 #30 相同的 Host/Workspace 路径），避免本机写入能力在 Linux 上因 `EBADF` 失败。不包含可视化编辑器源码。

## 本机验证（Linux x86_64，Rust 1.95.0）

```sh
cargo test -p cayw-trace-debug --locked
```

结果见本轮日志：插件相关测试全部通过（与 macOS 记录中的 47 项同量级）。

未在本环境重跑 Playwright MCP 网页八项操作与 Magpie/Haiku 线上路径；macOS 既有验收与 Haiku 重测记录仍有效。

## Issue 八项验收对照

| 标准 | 状态 | 依据 |
| --- | --- | --- |
| 1. Trace 增量驱动两图 | pass（既有 macOS） | issue-31-final-acceptance-macos.md |
| 2. 报错摘要与证据 | pass（既有 macOS） | 同上 |
| 3. 改节点后自动重算 | pass（既有 macOS + Linux 包测） | 同上；Linux engine/replay 测试 |
| 4. 新内容真实改分支 | pass（既有 macOS + Linux） | 同上 |
| 5. 无整项目编译检查 | pass（既有 macOS） | 同上 |
| 6. 对比原始/重放 | pass（既有 macOS + Linux） | 同上 |
| 7. 缺上下文缺口 | pass（既有 macOS + Linux） | 同上 |
| 8. 停插件后普通任务可用 | pass（既有 macOS） | 同上 |

