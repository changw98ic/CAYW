# F09 / F19 / SLICE J+K 验收说明（2026-10-25，含审计补强）

## 范围

Issue [#33](https://github.com/changw98ic/CAYW/issues/33) F09 HTML 产物 + F19 宿主动作协议首刀：`cayw-artifacts`。

分支：`feat/f09-f19-artifact-actions`（基于 `dev`）。

## 本机命令

```sh
cargo test -p cayw-artifacts --locked
cargo clippy -p cayw-artifacts --all-targets --locked -- -D warnings
cargo run -p cayw-artifacts --example offline_artifacts --locked
```

## 结果

- 单元/集成覆盖：`ArtifactPreview` 渲染投影、内存与 `{root}/artifacts/recovery.json` 磁盘恢复、只读/状态动作、同 Session follow-up/tool **实调** `HostActionDispatch`、桥错误 Rejected、跨 Session / 版本不匹配拒绝且不调桥、外 Session 更新拒绝。
- 示例打印 preview `content_type`、follow-up/tool Applied 与 bridge call 计数、跨 Session Rejected，以及磁盘恢复后产物/动作数量；无网络、无 Provider、无真实模型请求。

## 不做声明

本记录不证明 F09/F19 全范围完成，也不证明 WebUI 外壳或生产 Runtime adapter 已合入主路径；证明的是协议层桥被真实调用，以及磁盘恢复路径可用。
