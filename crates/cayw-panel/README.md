# cayw-panel

CAYW 面板 / 文件查看器扩展 API（F07 首刀，SLICE H）。

官方模块与第三方通过**同一套** [`PanelRegistry`](src/registry.rs) 与 [`WorkspaceHooks`](src/workspace.rs) 注册侧栏 / 底部工作区面板，以及按扩展名解析的文件查看器。注册只产生宿主侧目录数据：不授予 Core 私有状态、Host 能力、审批或新的执行权。TUI / WebUI 自行决定如何展示；用户动作仍走既有公开 SDK / IPC 与权限路径。

## 首刀范围

- 区域：`sidebar`、`bottom`
- 贡献：`PanelDescriptor`（tab / 页面）、`FileViewerDescriptor`（扩展名认领）
- 钩子：按 Session 的 `open` / `activate` / `close` / `list_open`（内存态）
- 协议版本：`PANEL_PROTOCOL_VERSION = 1`

本切片**不**交付完整文件编辑、Git、交互终端、后台任务页或子代理页的产品实现；这些后续可经同一 API 注册。WebUI 已通过 `/api/panels` 与侧栏/底部工作区表面接线首刀官方与样例面板（见 `cayw-webui`）；完整编辑器/终端产品 UI 仍属后续。

## 快速使用

```rust
use cayw_panel::*;
use std::sync::Arc;

let registry = Arc::new(PanelRegistry::new());
let _files = registry.register_panel(PanelDescriptor {
    id: "files".into(),
    title: "Files".into(),
    contributor: ContributorKind::Official,
    placement: PanelPlacement::Both,
    order: 10,
    description: None,
    hidden: false,
})?;
let _viewer = registry.register_file_viewer(FileViewerDescriptor {
    id: "markdown".into(),
    title: "Markdown".into(),
    contributor: ContributorKind::ThirdParty,
    extensions: vec!["md".into()],
    priority: 10,
})?;

let hooks = WorkspaceHooks::new(Arc::clone(&registry));
let opened = hooks.open(
    "session-1",
    OpenPanelRequest {
        panel_id: "files".into(),
        zone: Some(PanelZone::Sidebar),
        path: Some("README.md".into()),
        activate: true,
    },
)?;
```

`PanelHandle` 在 Drop 或显式 `unregister` 时撤销贡献；代际不匹配视为已卸载。

## 来源与边界

- 功能：F07（[#33](https://github.com/changw98ic/CAYW/issues/33)）
- 采用方式：设计参考（非代码移植）
- 参考：[omdsh-dev/DSH-better-sidebar](https://github.com/omdsh-dev/DSH-better-sidebar) 的 `registerTab` / `registerFileViewer` / 侧栏+底部双工作台形状
- 记录：[`docs/sources/f07-panel-extension.md`](../../docs/sources/f07-panel-extension.md)

## 验证

```sh
cargo test -p cayw-panel --locked
cargo clippy -p cayw-panel --all-targets --locked -- -D warnings
cargo run -p cayw-panel --example offline_panels --locked
```

测试为本地内存注册表与钩子，无网络、无 Provider、无真实 UI。
