# F06 来源记录：运行检查器（SLICE G）

## 标识

| 字段 | 值 |
| --- | --- |
| 功能编号 | F06 |
| 总规划 | [#33](https://github.com/changw98ic/CAYW/issues/33) |
| 切片 | SLICE G — run inspector 首刀 |
| 采用方式 | 设计参考（非代码移植、非外部运行时依赖） |
| CAYW 实现 | `crates/cayw-run-inspector` |

## 参考来源

| 项目 | 链接 | 许可 | 使用范围 |
| --- | --- | --- | --- |
| dsh-maze（原 dsh-trace-compare） | https://github.com/lamost423/dsh-maze | MIT | 仅参考执行时间线、失败/重试恢复链、耗时轨道、会话比较、证据回跳的产品形状 |

未复制上游 TypeScript / React 源码，未引入 Cordis / DSH 运行时依赖。CAYW 实现为独立 Rust 只读折叠层，输入为与运行步骤/证据游标对齐的快照；缺失字段保持空，不编造恢复结论。

## 本切片交付

- `RetryPath` / `RunTimeline` / `DurationTrack` / `SessionCompare`
- 两次失败重试路径可区分；同任务会话 A/B 墙钟轨道 + turn 对齐
- 单元测试与 `offline_inspector` 示例
- 本源记录与 crate README

## 明确未交付（后续切片）

- 迷宫 UI、缩放回放、密度聚合徽标
- CodeMode 内部工具 span、源码/符号关联
- Trace Debug 节点重新求值接线
- TUI / WebUI 渲染与 Runtime 事件直连导出

## 验收口径（本切片）

本地 `cargo test -p cayw-run-inspector` 证明：两条失败恢复路径 `path_id`/`RecoveryKind` 可区分；同 `task_fingerprint` 的 A/B 比较产出共享 `t0` 轨道与 turn 对齐泳道。不把本切片标为 F06 全范围完成。
