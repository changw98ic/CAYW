# F17 局部重算（SLICE D）实现记录

日期：2026-10-25（America/New_York）。范围：仅 `cayw-trace-debug` 的 F17 保存后局部重算跟进（Issue #33 / #31）。未做 F16 符号/文件关联。

## 行为

| 模式 | 触发 | 行为 |
| --- | --- | --- |
| `observed` | 原始 Runtime | 不变 |
| `recompute` | 节点保存 / edit | 复用修改点上游已完成步骤（`reused=true`）；从该节点捕获的 input / state_before 起局部求值并更新下游 |
| `replay` | 撤销等无 `changed_node` 路径 | 仍从入口整图本地重放；步骤不标复用；同 VM 内可重建共享表引用 |

快速路径不启动 Cargo / tsc / 整项目构建或全量测试。Host 仍只返回精确匹配的捕获响应。

## 验证

在分支上执行：

```sh
cargo test -p cayw-trace-debug --locked
```

本机 Linux 上该包测试全部通过（含 `engine`、`replay_state`、`demo_runtime`）。历史 macOS 离线验收未在本轮重跑。
