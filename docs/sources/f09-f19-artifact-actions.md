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

- `ArtifactStore`：按 Session 的 HTML/图表/预览存放、更新、列表与快照恢复
- `preview` → `ArtifactPreview` 渲染投影（`html` + `content_type`），不是存储 `ArtifactRecord`
- `ActionProtocol` + `HostActionDispatch`：按钮/表单/图表动作 → `FollowUpTask` / `InvokeTool` 实际调用宿主桥 / `UpdateState` / `ReadOnly`
- 产品默认消费者：`cayw-runtime::RuntimeHostDispatch`（`FollowUpTask` → `Runtime::enqueue`；`InvokeTool` → 已授权 `Host::call`）；`RecordingHostDispatch` 仅测试/离线双
- 跨 Session 拒绝；`content_version` 钉扎；桥错误 → Rejected；动作与 UI 状态快照恢复
- 失败的 `update` 不先改 `title`（校验全部通过后再原子写入）
- 统一审计：`Runtime::dispatch_artifact_action` 写入 `artifact.action.*` 到 Runtime audit journal；`list_artifact_actions` / `get_artifact_action` 暴露协议记录
- 取消：`Runtime::cancel_artifact_action` 取消 in-flight Host 工具令牌或 follow-up 队列任务（`ActionStatus::Cancelled`）
- 磁盘恢复：`{root}/artifacts/recovery.json`（不经 ContentStore；`result_handle` 仍为不透明引用）
- 可选 `result_handle` 字段对齐 F01 句柄引用（本切片不解析 ContentStore）
- 单元测试、`offline_artifacts` 示例、本源记录与 crate README

## 明确未交付（后续切片）

- WebUI / TUI 外壳接线（渲染投影与 Runtime/API 审计/取消已具备；页面壳/面板未接）
- HTML 沙箱 / CSP 执行策略
- 将 Trace Debug / visual-editor 专用页自动登记为通用产物
- Cells `result_omitted` 自动进产物（依赖 F01 更深接线）
- 将 F09 或 F19 全范围标为完成

## 验收口径（本切片）

本地 `cargo test -p cayw-artifacts` 与 `cargo test -p cayw-runtime --test artifact_dispatch` 证明：工具产出 HTML 的 `ArtifactPreview` 可渲染投影；同 Session follow-up 进入 Runtime 任务队列；InvokeTool 走已授权 Host::call；跨 Session 与版本不匹配拒绝且不调桥；`{root}/artifacts/recovery.json` 磁盘往返后仍按 Session 可见且保持 RuntimeHostDispatch。不把本切片标为 F09/F19 全范围完成（缺 WebUI 外壳）。
