# cayw-web-access

CAYW 网页搜索 / URL 获取 / HTML·PDF 正文提取（F10 首刀，SLICE M）。

统一工具面使用 **`web.search` / `web.fetch` / `web.extract`**。现有宿主 `search.query` 仍是本地 Workspace `fs.grep`，本 crate **拒绝**占用该名称。

## 首刀范围

- 可插拔 [`SearchProvider`] + [`HttpTransport`]
- 离线 [`FixtureTransport`] / [`FixtureSearch`]（失败带 [`FailureDiagnostic`]）
- 实网 [`ReqwestTransport`]（HTTP/HTTPS）
- HTML 正文：`html5gum`；PDF：字面量字符串首刀提取（非完整 PDF 引擎）
- 进程内 [`ContentStore`] + [`Citation`] / [`ContentRef`]，供后续工具按引用取正文
- 协议版本：`WEB_ACCESS_PROTOCOL_VERSION = 1`

本切片**不**交付：视频理解、多 Provider 搜索回退链、Host/Bridge 目录注册、与 F01 结果句柄/`cayw-content` 的持久对齐。

## 快速使用

```rust
use cayw_web_access::*;

# async fn demo() {
let transport = FixtureTransport::new()
    .insert_html("https://example.com/", "<html><body><p>Hi</p></body></html>");
let search = FixtureSearch::new().insert(
    "hi",
    vec![hit(1, "https://example.com/", "Example", "Hi")],
);
let access = WebAccess::with_fixtures(transport, search);
let page = access.fetch_and_extract("https://example.com/").await.unwrap();
assert!(page.citation.content_ref.is_some());
# }
```

## 来源与边界

- 功能：F10（[#33](https://github.com/changw98ic/CAYW/issues/33)）
- 采用方式：设计参考（非代码移植）
- 参考：[nicobailon/pi-web-access](https://github.com/nicobailon/pi-web-access) 的搜索/获取/提取产品形状
- 记录：[`docs/sources/f10-web-access.md`](../../docs/sources/f10-web-access.md)

## 验证

```sh
cargo test -p cayw-web-access --locked
cargo clippy -p cayw-web-access --all-targets --locked -- -D warnings
cargo run -p cayw-web-access --example offline_web_access --locked
```

默认测试走 fixture，无外网。`ReqwestTransport` 仅在宿主显式选用时发请求。
