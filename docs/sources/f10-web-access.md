# F10 来源记录：Web 搜索 / 获取 / 提取

## 标识

| 字段 | 值 |
| --- | --- |
| 功能编号 | F10 |
| 总规划 | [#33](https://github.com/changw98ic/CAYW/issues/33) |
| 切片 | SLICE M + audit wave：实网搜索后端 + Host catalog 注册 |
| 采用方式 | 设计参考（非代码移植、非外部 npm 运行时依赖） |
| CAYW 实现 | `crates/cayw-web-access` |

## 参考来源

| 项目 | 链接 | 许可 | 使用范围 |
| --- | --- | --- | --- |
| pi-web-access | https://github.com/nicobailon/pi-web-access ；[官方包页](https://pi.dev/packages/pi-web-access) | MIT（上游声明） | 仅参考网页搜索、URL 获取、正文/PDF 提取与来源引用的产品形状；不移植其 Provider 回退链或视频管线 |

未复制上游 TypeScript / 扩展源码，未引入 Pi / Node runner 依赖。CAYW 实现为独立 Rust Provider/工具面；能力名固定为 `web.search` / `web.fetch` / `web.extract`，**不**覆盖本地 `search.query`（`fs.grep`）。

## 本波交付

- `WebAccess` 门面 + `SearchProvider` / `HttpTransport`
- **实网搜索后端**：DuckDuckGo HTML（默认，无 key）；可选 Brave（`BRAVE_SEARCH_API_KEY`）软回退 DDG
- 离线 fixture 与可诊断 `FailureDiagnostic`（含 `offline_mode`）
- HTML 正文提取；**PDF 仅字面量扫描（弱，非完整引擎）**
- `definitions()` + Host `Executor::WebAccess`；由宿主显式配置 `HostConfig.web_access` 启用
- Runtime 注入同一 Device 内容库，网页正文返回正式结果引用，保留 URL、状态和来源；`result.get` 可读取，注入接口失败不回退进程内成功结果
- 独立 SDK 未注入内容接口时可使用进程内 `ContentStore`
- 单元测试与 `offline_web_access` 示例

## 明确未交付 / 不宣称

- **视频**内容理解 / 帧提取 — **不支持**
- **完整 PDF**（字体、ToUnicode、版面）— **不支持**；仅 parenthesis/literal 弱提取
- Exa / SearXNG 等多 Provider 长回退链
- Bridge / CodeMode 以外的深层接线（Host catalog 已接）
- 将 F10 全范围标为完成

## 验收口径（本波）

本地 `cargo test -p cayw-web-access --locked`：DDG HTML 解析 + fixture 管线；`definitions` 含 `web.*` 不含 `search.query`；Host 显式配置后目录可注册。不把本波标为 F10 全范围完成。
