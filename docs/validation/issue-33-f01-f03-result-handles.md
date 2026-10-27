# Issue #33 SLICE A — F01 + F03 result handles

日期：2026-10-26（America/New_York）。分支：`feat/f01-f03-result-handles`。基线：`dev`。

## 目标

扩展现有 CodeMode / Runtime 工具入口（不另建执行器），提供与 ContentStore / #18 对齐的统一结果句柄：存放、按引用分页取回、批量元数据、按查询返回片段、软释放。

## 落地

| 能力 | 工具 / API | 说明 |
| --- | --- | --- |
| 外置存放 | `result.store` | UTF-8/JSON 正文写入 Device `ContentStore`；返回 `handle` + hash + bytes |
| 分页取回 | `result.get` | `offset`/`limit` 按 UTF-8 字节边界分页 |
| 批量 | `result.batch_get` | 最多 16 个 handle；可选附带首页 |
| 检索片段 | `result.search` | 字面量子串，按行返回 offset/line/snippet |
| 释放 | `result.release` | 元数据 `released=true`；后续 get/search 拒绝。ContentStore 无删除 API，正文对象可仍占配额（#18） |
| 宿主检查 | `Runtime::result_inspect` | 含已释放句柄 |

实现：`crates/cayw-runtime/src/results.rs`。目录注册与 `content.*` 相同，仅在配置了 `ContentConfig` 时出现。Session/Task 作用域与 `content.read`/`content.put` 一致（记录名哈希绑定 device/session/task/handle）。

## 自动外置（本轮补齐）

Host/tool 成功返回值在进入 Cells 结算前，若已配置 `ContentConfig` 且 JSON UTF-8 字节数超过
`RuntimeConfig::auto_externalize_threshold_bytes`（默认 **32 KiB**；`0` 关闭），则经与
`result.store` 相同的 ContentStore 路径外置，并向模型返回带 `externalized=true` 的句柄桩；
完整正文仍用 `result.get` / `result.search` 取回。`result.*` / `content.*` 自身不二次外置。

## 仍非范围

- Workspace `fs.*`（含 frozen shared-read）大结果自动改写为句柄。
- ContentStore 物理删除 / GC。
- F02 命令输出过滤。

## 来源记录（#33 §六）

- 功能：F01、F03；issue #33。
- 来源：T-Auto/dsh-ops（native PTC）、mksglu/context-mode — **设计参考**，非代码移植。
- 采用方式：CAYW 自行实现，复用 `cayw-content` ContentStore。
- 许可：本仓库 Apache-2.0；未引入上游代码文件。

## 复跑

```sh
cargo test -p cayw-runtime --lib results:: --locked
# macOS（ContentStore open_private / 集成）：
cargo test -p cayw-runtime --lib results:: --locked
cargo test -p cayw-runtime --test results --locked
```

本执行环境为 Linux；ContentStore 本机私有打开与 #18 一样以 macOS 为主线验收。Linux 上已跑通分页/检索纯函数单测；ContentStore 路径测试以 `#[cfg(target_os = "macos")]` 门控。

## 验收勾选（SLICE A）

- [x] 统一结果句柄存放到 ContentStore（与 #18 同库）。
- [x] 按 handle 分页取回正文。
- [x] 批量 handle 元数据（及可选首页）。
- [x] 按查询返回相关行片段。
- [x] 软释放与后续拒绝；无独立执行器。
- [x] Host/tool 超阈值结果自动外置为句柄（阈值可配，默认 32 KiB）。
- [ ] Workspace 大读自动句柄化（后续切片）。
