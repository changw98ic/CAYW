# F10 来源记录：Web 搜索 / 获取 / 提取（SLICE M）

## 标识

| 字段 | 值 |
| --- | --- |
| 功能编号 | F10 |
| 总规划 | [#33](https://github.com/changw98ic/CAYW/issues/33) |
| 切片 | SLICE M — web search / fetch / HTML·PDF extract 首刀 |
| 采用方式 | 设计参考（非代码移植、非外部 npm 运行时依赖） |
| CAYW 实现 | `crates/cayw-web-access` |

## 参考来源

| 项目 | 链接 | 许可 | 使用范围 |
| --- | --- | --- | --- |
| pi-web-access | https://github.com/nicobailon/pi-web-access ；[官方包页](https://pi.dev/packages/pi-web-access) | MIT（上游声明） | 仅参考网页搜索、URL 获取、正文/PDF 提取与来源引用的产品形状；不移植其 Provider 回退链或视频管线 |

未复制上游 TypeScript / 扩展源码，未引入 Pi / Node runner 依赖。CAYW 实现为独立 Rust Provider/工具面；能力名固定为 `web.search` / `web.fetch` / `web.extract`，**不**覆盖本地 `search.query`（`fs.grep`）。

## 本切片交付

- `WebAccess` 门面 + `SearchProvider` / `HttpTransport`
- 离线 fixture 与可诊断 `FailureDiagnostic`（含 `offline_mode`）
- HTML / 最小 PDF 正文提取与 `Citation` / `ContentRef`
- 单元测试与 `offline_web_access` 示例
- 本源记录与 crate README

## 明确未交付（后续）

- 视频内容理解 / 帧提取
- Exa / Brave / SearXNG 等实网搜索 Provider 与回退链
- Host catalog / Bridge / CodeMode 注册接线
- 与 F01 结果句柄、持久 `cayw-content` 对齐
- 将 F10 全范围标为完成

## 验收口径（本切片）

本地 `cargo test -p cayw-web-access --locked` 覆盖：公开 URL fixture → 正文引用可取回；离线失败可诊断；`search.query` 被拒绝且能力表不含该名。不把本切片标为 F10 全范围完成。
