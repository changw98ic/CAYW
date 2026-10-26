# cayw-artifacts

CAYW HTML 产物展示与宿主动作协议（F09 + F19 首刀，SLICE J+K）。

按 **Session** 存放通用 HTML 卡片 / 图表 / 独立预览正文，并提供统一的宿主动作协议：按钮、表单、图表选择等交互变成结构化意图（同 Session 后续任务、已授权工具调用、UI 状态更新、只读确认）。跨 Session 动作拒绝；内容版本可钉扎；快照支持内存与磁盘「重启恢复」。

`FollowUpTask` / `InvokeTool` 通过注入的 [`HostActionDispatch`](src/dispatch.rs) 桥真正交给 Runtime / Host（或测试用的 `RecordingHostDispatch`），不再停在 pending JSON。本 crate **不**依赖 `cayw-runtime` / `cayw-host`，不授予新 Host 能力，不沙箱 HTML；生产侧实现该 trait，转发到既有 `Host::call` / Session follow-up 路径即可。

## 首刀范围

- 产物：`HtmlCard` / `Chart` / `StandalonePreview`
- 存储：`put` / `update` / `preview` / `list_by_session` + `content_version` + 可选 `result_handle`
- `preview` 返回渲染投影 `ArtifactPreview`（`html` + `content_type`），不是存储行 `ArtifactRecord`
- 动作：`FollowUpTask` / `InvokeTool`（走 host 桥）/ `UpdateState` / `ReadOnly`
- 归属：跨 Session 拒绝；版本不匹配拒绝；桥错误 → Rejected
- 恢复：`RecoveryBundle` 内存导出/恢复；磁盘路径 `{root}/artifacts/recovery.json`
- 协议版本：`ARTIFACT_PROTOCOL_VERSION = 1`

**不在本切片**：WebUI/TUI 外壳接线、HTML 沙箱、把 Trace / visual-editor 专用页冒充通用产物展示、解析 ContentStore 上的 `result_handle`、将 F09/F19 全范围标完成。

## 快速使用

```rust
use cayw_artifacts::*;
use std::sync::Arc;

let bridge = Arc::new(RecordingHostDispatch::new());
let ws = ArtifactWorkspace::with_dispatch(bridge);
ws.store.put(PutArtifactRequest {
    artifact_id: "card-1".into(),
    session_id: "sess-1".into(),
    kind: ArtifactKind::HtmlCard,
    title: "Summary".into(),
    body: "<button>Retry</button>".into(),
    at_ms: 1,
    result_handle: None,
})?;

let preview = ws.store.preview("card-1")?;
assert_eq!(preview.content_type, ArtifactPreview::HTML_CONTENT_TYPE);

let outcome = ws.actions.dispatch(ActionRequest {
    action_id: "act-1".into(),
    artifact_id: "card-1".into(),
    session_id: "sess-1".into(),
    kind: ActionKind::Button,
    intent: ActionIntent::FollowUpTask { prompt: "Retry".into() },
    expected_content_version: Some(1),
    requested_at_ms: 2,
})?;
assert_eq!(outcome.record.status, ActionStatus::Applied);
```

生产接线：实现 `HostActionDispatch`，`follow_up_task` 走同 Session 的 Agent/Runtime follow-up；`invoke_tool` 走已授权的 `Host::call` / Runtime request-tool 路径（与现有 ExternalHost / request tools 相同入口）。

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

测试覆盖 host 桥实调、磁盘恢复与渲染投影；无网络、无 Provider、无真实 UI。
