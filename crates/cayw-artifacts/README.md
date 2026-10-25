# cayw-artifacts

CAYW HTML 产物展示与宿主动作协议（F09 + F19 首刀，SLICE J+K）。

按 **Session** 存放通用 HTML 卡片 / 图表 / 独立预览正文，并提供统一的宿主动作协议：按钮、表单、图表选择等交互变成结构化意图（同 Session 后续任务、已授权工具调用、UI 状态更新、只读确认）。跨 Session 动作拒绝；内容版本可钉扎；快照支持进程内「重启恢复」烟测。

本 crate **不**渲染 UI、不调用模型、不授予新 Host 能力、不自行启动 Runtime 任务。WebUI / TUI / Runtime 通过既有公开 SDK / IPC 解释已接受的意图。

## 首刀范围

- 产物：`HtmlCard` / `Chart` / `StandalonePreview`
- 存储：`put` / `update` / `preview` / `list_by_session` + `content_version` + 可选 `result_handle`
- 动作：`FollowUpTask` / `InvokeTool` / `UpdateState` / `ReadOnly`
- 归属：跨 Session 拒绝；版本不匹配拒绝
- 恢复：`RecoveryBundle` 导出 / 恢复产物与动作状态
- 协议版本：`ARTIFACT_PROTOCOL_VERSION = 1`

**不在本切片**：WebUI/TUI 接线、真实 Runtime 启任务、HTML 沙箱、把 Trace / visual-editor 专用页冒充通用产物展示、将 F09/F19 全范围标完成。

## 快速使用

```rust
use cayw_artifacts::*;
use std::sync::Arc;

let ws = ArtifactWorkspace::new();
ws.store.put(PutArtifactRequest {
    artifact_id: "card-1".into(),
    session_id: "sess-1".into(),
    kind: ArtifactKind::HtmlCard,
    title: "Summary".into(),
    body: "<button>Retry</button>".into(),
    at_ms: 1,
    result_handle: None,
})?;

let outcome = ws.actions.dispatch(ActionRequest {
    action_id: "act-1".into(),
    artifact_id: "card-1".into(),
    session_id: "sess-1".into(),
    kind: ActionKind::Button,
    intent: ActionIntent::FollowUpTask { prompt: "Retry".into() },
    expected_content_version: Some(1),
    requested_at_ms: 2,
})?;
assert_eq!(outcome.record.status, ActionStatus::Accepted);
```

## 来源与边界

- 功能：F09 + F19（[#33](https://github.com/changw98ic/CAYW/issues/33)）
- 采用方式：设计参考（非代码移植）
- 参考：[Nagi-ovo/dsh-visualize](https://github.com/Nagi-ovo/dsh-visualize) 的卡片/图表/预览形状；Plannotator 的会话反馈路径。dsh-visualize 当前卡片按钮尚不能直接向主对话发 follow-up，故 CAYW 自建宿主动作协议。
- 记录：[`docs/sources/f09-f19-artifact-actions.md`](../../docs/sources/f09-f19-artifact-actions.md)

## 验证

```sh
cargo test -p cayw-artifacts --locked
cargo clippy -p cayw-artifacts --all-targets --locked -- -D warnings
cargo run -p cayw-artifacts --example offline_artifacts --locked
```

测试为本地内存存储与动作协议，无网络、无 Provider、无真实 UI。
