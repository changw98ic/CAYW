# F09 / F19 / SLICE J+K 验收说明（2026-10-25，含审计补强）

## 范围

Issue [#33](https://github.com/changw98ic/CAYW/issues/33) F09 HTML 产物 + F19 宿主动作协议首刀：`cayw-artifacts`。

分支：`feat/f09-f19-artifact-actions`（基于 `dev`）。

## 本机命令

```sh
cargo test -p cayw-artifacts --locked
cargo test -p cayw-runtime --test artifact_dispatch --locked
cargo clippy -p cayw-artifacts -p cayw-runtime --all-targets --locked -- -D warnings
cargo run -p cayw-artifacts --example offline_artifacts --locked
```

## 结果

- 单元/集成覆盖：`ArtifactPreview` 渲染投影、内存与 `{root}/artifacts/recovery.json` 磁盘恢复、只读/状态动作、Recording 桥实调、**RuntimeHostDispatch** 产品路径（follow-up → Runtime 任务队列；InvokeTool → 真实 Host::call）、桥错误 Rejected、跨 Session / 版本不匹配拒绝且不调桥、外 Session 更新拒绝。
- 示例打印 preview `content_type`、follow-up/tool Applied 与 bridge call 计数、跨 Session Rejected，以及磁盘恢复后产物/动作数量；无网络、无 Provider、无真实模型请求。

## 不做声明

本记录不证明 F09/F19 全范围完成，也不证明 WebUI 外壳已接；证明的是协议层桥被真实调用、产品默认消费者为 RuntimeHostDispatch（非 Recording-only），以及磁盘恢复路径可用。

## Audit remediation (2026-10-26)

- `ArtifactStore::update` validates title/body/version bump before mutating; failed updates leave title untouched (`failed_update_does_not_mutate_title`).
- Runtime exposes `list_artifact_actions` / `get_artifact_action` / `cancel_artifact_action`; dispatches append `artifact.action.*` rows to the Runtime audit journal.
- Follow-up cancel path: `cancel_artifact_action` cancels the queued Runtime task and marks `ActionStatus::Cancelled`.
- WebUI chrome still out of scope on this branch (no artifact panel hooks on `feat/f09-f19-artifact-actions`).
