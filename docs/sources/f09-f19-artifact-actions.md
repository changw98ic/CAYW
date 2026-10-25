# F09 / F19 来源记录：HTML 产物与宿主动作协议（SLICE J+K）

## 标识

| 字段 | 值 |
| --- | --- |
| 功能编号 | F09、F19 |
| 总规划 | [#33](https://github.com/changw98ic/CAYW/issues/33) |
| 切片 | SLICE J（HTML 产物）+ SLICE K（动作协议）首刀合切 |
| 采用方式 | 设计参考（非代码移植、非外部运行时依赖） |
| CAYW 实现 | `crates/cayw-artifacts` |

## 参考来源

| 项目 | 链接 | 许可 | 使用范围 |
| --- | --- | --- | --- |
| dsh-visualize | https://github.com/Nagi-ovo/dsh-visualize | 按其仓库声明 | 仅参考对话 HTML 卡片、图表、独立预览与展示更新的产品形状 |
| Plannotator / 反馈路径 | https://github.com/backnotprop/plannotator 等 | 按其仓库声明 | 仅参考会话绑定的后续反馈形状；不移植批注 UI |

未复制上游 TypeScript / React 源码，未引入 DSH / Cordis 运行时。CAYW 新增统一宿主动作协议：因 dsh-visualize 现有卡片按钮尚不能直接向主对话发送 follow-up。

## 本切片交付

- `ArtifactStore`：按 Session 的 HTML/图表/预览存放、更新、预览、列表与快照恢复
- `ActionProtocol`：按钮/表单/图表动作 → `FollowUpTask` / `InvokeTool` / `UpdateState` / `ReadOnly`
- 跨 Session 拒绝；`content_version` 钉扎；动作与 UI 状态快照恢复
- 可选 `result_handle` 字段对齐 F01 句柄引用（本切片不解析 ContentStore）
- 单元测试、`offline_artifacts` 示例、本源记录与 crate README

## 明确未交付（后续切片）

- WebUI / TUI 渲染与 Runtime 真实启任务接线
- HTML 沙箱 / CSP 执行策略
- 将 Trace Debug / visual-editor 专用页自动登记为通用产物
- Cells `result_omitted` 自动进产物（依赖 F01 更深接线）
- 将 F09 或 F19 全范围标为完成

## 验收口径（本切片）

本地 `cargo test -p cayw-artifacts` 证明：工具产出 HTML 可预览；快照恢复后仍按 Session 可见；卡片按钮同 Session 产出后续任务意图；跨 Session 拒绝；刷新后动作状态仍在。不把本切片标为 F09/F19 全范围完成。
