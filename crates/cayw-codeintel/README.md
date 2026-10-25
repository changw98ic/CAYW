# cayw-codeintel

Shared **file / symbol version bridge** for CAYW Agent、编辑器与 Trace（F04 / SLICE C 首刀）。

## 职责

- 按路径跟踪文件版本：单调 `version` + `content_sha256`
- 接收宿主已有的 outline / 符号快照（例如 `code.outline` 结果），生成绑定到该文件版本的 `SymbolId`
- 同索引内的 **definitions** 查询（非完整 LSP 客户端）
- **diagnostics** 必须携带版本化 `SourceLocation`，错误路径可回链源码证据
- Agent / 编辑器 / Trace 通过同一桥接对象拿到一致的 file/symbol 版本

本 crate **不**解析源码、不重复建设 `code.outline`、不把 CodeGraph / 依赖图塞进 Core，也不启动语言服务器进程。

## 非本切片

- 跨文件影响级联、依赖图 / `/lens-map`
- 实况 LSP 连接池、linter / formatter runners
- Host / Runtime / CodeMode 工具接线与 IPC 暴露
- Trace 节点自动挂符号（属 SLICE E）

## 快速使用

```rust
use cayw_codeintel::*;

let bridge = SymbolVersionBridge::new();
let file = bridge.publish_file(
    "src/lib.rs",
    &content_sha256_hex,
    "rust",
    vec![/* SymbolInput from outline */],
)?;
let hits = bridge.definitions(DefinitionQuery {
    name: "hello".into(),
    path: None,
    kind: Some("function".into()),
    file_version: Some(file.version),
})?;
```

协议常量：`CODEINTEL_PROTOCOL_VERSION = 1`。

## 来源

设计参考 [apmantza/pi-lens](https://github.com/apmantza/pi-lens)（非代码移植）。记录见 `docs/sources/f04-symbol-version-bridge.md`。

## 测试

```bash
cargo test -p cayw-codeintel --locked
cargo run -p cayw-codeintel --example offline_bridge --locked
```
