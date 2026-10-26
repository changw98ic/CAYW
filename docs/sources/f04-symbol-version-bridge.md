# F04 来源记录：符号 / 文件版本桥接（SLICE C）

## 标识

| 字段 | 值 |
| --- | --- |
| 功能编号 | F04 |
| 总规划 | [#33](https://github.com/changw98ic/CAYW/issues/33) |
| 切片 | SLICE C — symbol/version bridge（宿主语言服务最小接入首刀） |
| 采用方式 | 设计参考（非代码移植、非外部运行时依赖） |
| CAYW 实现 | `crates/cayw-codeintel` |

## 参考来源

| 项目 | 链接 | 许可 | 使用范围 |
| --- | --- | --- | --- |
| pi-lens | https://github.com/apmantza/pi-lens ；https://pi.dev/packages/pi-lens | MIT（上游声明） | 仅参考「语言服务反馈 + 符号/文件版本供 Agent 与编辑面共用」的产品形状；不复制 LSP 连接池、impact cascade、word index、MCP server 或 TypeScript 源码 |

未引入 Node / npm 依赖，未嵌入 tree-sitter WASM。CAYW 实现为独立 Rust 内存桥接：宿主喂入 outline 快照，桥接负责版本身份、definitions 与带回链的 diagnostics。

## 本切片交付

- `FileVersion` / `SymbolId` / `SourceLocation` / `Diagnostic` 共享类型
- `SymbolVersionBridge::publish_file`：内容摘要变化则 version bump；同摘要保持同一版本
- **Producer**：`publish_bytes` / `publish_path` / `publish_outline_json` 读取工作区文件字节或 `code.outline` 结果并写入桥接（轻量 Rust outline，非 LSP）
- `definitions`：同索引定义查询（供 Agent / 编辑器 / Trace 共用）
- `report_diagnostic` + `diagnostics`：错误必须绑定版本化源码位置；过期版本拒绝静默套用
- **共享实例**：`SymbolVersionBridge` 经 `Clone`/`Arc` 共享；`HostConfig::codeintel`、`EditorConfig::codeintel`、`serve_with_codeintel` 接同一句柄
- **Host 工具**：`codeintel.refresh` / `codeintel.definitions` / `codeintel.file_version` / `codeintel.symbols`
- **Trace 入口**：`POST /api/codeintel/definitions`、`POST /api/codeintel/file-version`
- 单元测试、Host 集成测试、`offline_bridge` 示例、本源记录与 crate README
- 验收笔记 `docs/validation/issue-33-f04-symbol-version-bridge.md`

## 明确未交付（后续切片）

- 跨文件影响诊断、依赖图
- 实况 LSP / linter / formatter
- Trace 自动关联工程符号（SLICE E）

## 验收口径（本切片）

本地 `cargo test -p cayw-codeintel` 证明：改文件 → 版本 bump；多消费者拿到同一 symbol/file 版本；诊断回链版本化位置；过期版本拒绝。不把本切片标为 F04 全范围完成。
