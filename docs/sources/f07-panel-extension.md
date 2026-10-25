# F07 来源记录：面板 / 文件查看器扩展 API（SLICE H）

## 标识

| 字段 | 值 |
| --- | --- |
| 功能编号 | F07 |
| 总规划 | [#33](https://github.com/changw98ic/CAYW/issues/33) |
| 切片 | SLICE H — panel extension API（侧栏 / 底部工作区钩子首刀） |
| 采用方式 | 设计参考（非代码移植、非外部运行时依赖） |
| CAYW 实现 | `crates/cayw-panel` |

## 参考来源

| 项目 | 链接 | 许可 | 使用范围 |
| --- | --- | --- | --- |
| DSH-better-sidebar | https://github.com/omdsh-dev/DSH-better-sidebar | MIT（上游声明） | 仅参考 `registerTab` / `registerFileViewer`、官方与第三方同一服务面、侧栏 + 底部工作区双区域的产品形状 |

未复制上游 TypeScript / React 源码，未引入 Cordis / DSH 运行时依赖。CAYW 实现为独立 Rust 注册表与内存工作区钩子，符合架构文档「UI 贡献不取得 Core 私有状态或新的执行权」。

## 本切片交付

- 统一 `PanelRegistry`：官方 / 第三方同一 `register_panel` / `register_file_viewer`
- `WorkspaceHooks`：Session 作用域的 sidebar / bottom `open`·`activate`·`close`·`list_open`
- 扩展名文件查看器解析（优先级 + id 平局）
- 单元测试与 `offline_panels` 示例
- 本源记录与 crate README

## 明确未交付（后续切片）

- 完整文件编辑、Git、交互终端、后台任务、子代理页面产品 UI
- TUI / WebUI 实际渲染接线
- 布局持久化、跨会话排队打开、原生 tab keepMounted 等上游细节

## 验收口径（本切片）

本地 `cargo test -p cayw-panel` 证明注册、冲突、区域约束、查看器解析与工作区钩子行为。不把本切片标为 F07 全范围完成。
