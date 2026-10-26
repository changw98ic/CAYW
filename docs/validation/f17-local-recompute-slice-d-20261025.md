# F17 局部重算（SLICE D）实现记录

日期：2026-10-25（America/New_York）；局部性补强 2026-10-26。范围：仅 `cayw-trace-debug` 的 F17 保存后局部重算跟进（Issue #33 / #31）。未做 F16 符号/文件关联。

## 行为

| 模式 | 触发 | 行为 |
| --- | --- | --- |
| `observed` | 原始 Runtime | 不变；`started` 现附带 `context_before` 与共享表 `seed` |
| `recompute` | 节点保存 / edit | 上游步骤 `reused=true` 且**不重新执行**其 Luau 函数体；VM 从编辑节点用捕获的 `seed`（context + 表身份）起算，只跑编辑点及受影响下游；`executed_nodes` 列出实际执行的节点 |
| `replay` | 撤销等无 `changed_node` 路径 | 仍从入口整图本地重放；步骤不标复用；`executed_nodes` 含全路径 |

快速路径不启动 Cargo / tsc / 整项目构建或全量测试。Host 仍只返回精确匹配的捕获响应。

## 局部性如何保证

1. `script_for_local_recompute` 的 `__current` 设为编辑节点，上游函数体不进入 VM 循环。
2. `__encode_seed` / `__decode_seed` 在 JSON 上保留 input/state/context 的 Luau 表身份与 context 变更，避免「整图预热」作弊。
3. 测试用 `executed_nodes`（及 Host 捕获未消费）证明上游未重跑，而不仅依赖 `reused` 展示字段。

## 已知缺口

- 更旧的、没有 `seed` 的捕获在局部重算时回退为分列 JSON 快照：无共享身份时结果仍正确；依赖共享表身份的图在缺 seed 时可能不等价（新 observed 路径都会写 seed）。
- F16 符号/文件关联仍未做。

## 验证

```sh
cargo test -p cayw-trace-debug --locked
```

本机 Linux 上该包测试全部通过（含 `engine` 的 `executed_nodes` 证明、`replay_state` 的 9/18 与共享身份、`demo_runtime`）。
