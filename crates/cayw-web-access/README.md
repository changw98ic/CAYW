# cayw-web-access

CAYW 网页搜索 / URL 获取 / HTML·PDF 正文提取（F10）。

统一工具面：`web.search` / `web.fetch` / `web.extract`。宿主本地 `search.query` 仍是 Workspace `fs.grep`，本 crate **拒绝**占用该名称。

## 本波交付

- 可插拔 [`SearchProvider`] + [`HttpTransport`]
- 离线 [`FixtureTransport`] / [`FixtureSearch`]（失败带 [`FailureDiagnostic`]）
- 实网 [`ReqwestTransport`]（HTTP/HTTPS GET）
- **实网搜索**：[`DuckDuckGoHtmlSearch`]（HTML 结果页，**无需 API key**）；可选 [`BraveSearch`]（`BRAVE_SEARCH_API_KEY`，空结果/失败时回退 DDG）
- Host/Runtime 目录：[`definitions`] → `Executor::WebAccess`（`HostConfig.web_access`，默认开启）
- HTML 正文：`html5gum`
- 进程内 [`ContentStore`] + [`Citation`] / [`ContentRef`]（Host 持有 `Arc<WebAccess>`，供同进程后续工具按引用取正文）

## 明确不支持 / 弱支持

| 能力 | 状态 |
| --- | --- |
| **视频** | **不支持**（无帧提取、无 AV 理解） |
| **PDF** | **弱**：仅扫描 PDF 字面量 `(...)` 字符串；**不是**完整 PDF 引擎（无字体/ToUnicode/xref 修复/版面） |
| 多 Provider 自动回退链 | 仅 Brave→DDG 软回退；不做 Exa/SearXNG 链 |
| 与 F01 / 持久 `cayw-content` 对齐 | 后续；本波为进程内 ContentStore |

## 快速使用

```rust
use cayw_web_access::*;
use std::sync::Arc;

# async fn demo() {
// Host 默认：`WebAccessOptions::default()` → DDG HTML + Reqwest
let access = WebAccessOptions::default().build().unwrap();

// 离线 / 单测：fixture 搜索 + 传输
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
- 参考：[nicobailon/pi-web-access](https://github.com/nicobailon/pi-web-access)
- 记录：[`docs/sources/f10-web-access.md`](../../docs/sources/f10-web-access.md)

## 验证

```sh
cargo test -p cayw-web-access --locked
cargo clippy -p cayw-web-access --all-targets --locked -- -D warnings
cargo run -p cayw-web-access --example offline_web_access --locked
# optional live DDG (network):
# CAYW_WEB_LIVE=1 cargo test -p cayw-web-access --locked -- --ignored
```

默认单测走 fixture / 解析单元测试，无外网、无 Brave 计费。
