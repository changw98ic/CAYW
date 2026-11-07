# cayw-codeintel

Shared **file / symbol version bridge** for CAYW Agent、编辑器与 Trace（F04 / SLICE C 首刀）。

## 职责

- 按路径跟踪文件版本：单调 `version` + `content_sha256`
- 接收宿主已有的 outline / 符号快照（例如 `code.outline` 结果），生成绑定到该文件版本的 `SymbolId`
- 同索引内的 **definitions** 查询（非完整 LSP 客户端）
- **diagnostics** 必须携带版本化 `SourceLocation`，错误路径可回链源码证据
- Agent / 编辑器 / Trace 可注入同一个进程内桥接对象，读取一致的 file/symbol 版本；独立进程需要显式接入

本 crate **不**启动语言服务器、不把 CodeGraph / 依赖图塞进 Core。宿主可喂入
`code.outline` 快照；也可用 [`publish_bytes`] / [`publish_path`] 从工作区文件
字节做轻量 Rust outline 并写入同一桥接（闭环，非完整 `syn` 复刻）。

## 非本切片

- 跨文件影响级联、依赖图 / `/lens-map`
- 实况 LSP 连接池、linter / formatter runners
Trace 已接入版本化符号挂载与查询。Editor 成功保存后发布对应文件版本。该接线不增加完整 LSP、宏展开或类型推导能力。

## 快速使用

```rust
use cayw_codeintel::*;

// Shared handle (Clone = same index) for Host / editor / Trace.
let bridge = SymbolVersionBridge::new();
let editor = bridge.clone();

// Producer: read workspace bytes → digest + lightweight outline → publish.
let file = publish_bytes(&bridge, "src/lib.rs", src_bytes, "rust")?;

let hits = editor.definitions(DefinitionQuery {
    name: "hello".into(),
    path: None,
    kind: Some("function".into()),
    file_version: Some(file.version),
})?;
```

Host tools (when `HostConfig::codeintel` is set): `codeintel.refresh`,
`codeintel.definitions`, `codeintel.file_version`.
协议常量：`CODEINTEL_PROTOCOL_VERSION = 1`。

## 来源

设计参考 [apmantza/pi-lens](https://github.com/apmantza/pi-lens)（非代码移植）。记录见 `docs/sources/f04-symbol-version-bridge.md`。

## 测试

```bash
cargo test -p cayw-codeintel --locked
cargo run -p cayw-codeintel --example offline_bridge --locked
```
