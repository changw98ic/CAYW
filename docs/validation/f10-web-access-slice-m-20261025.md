# F10 验收说明（SLICE M + audit remediation）

## 范围

Issue [#33](https://github.com/changw98ic/CAYW/issues/33) F10：`cayw-web-access` 统一 `web.*` 工具面、实网搜索后端、Host catalog 注册。

分支：`feat/f10-web-access`（基于 `dev`）。

## 本机命令

```sh
cargo test -p cayw-web-access --locked
cargo clippy -p cayw-web-access --all-targets --locked -- -D warnings
cargo run -p cayw-web-access --example offline_web_access --locked
```

## 结果口径

- 单元/fixture：公开 URL 提取 → `content_ref`；search→extract；离线 `FailureClass::Offline`；拒绝 `search.query`；PDF 字面量（弱）与无字面量失败；**DDG HTML 解析**（mock transport）。
- Host：`HostConfig.web_access` 默认 `Some` → catalog 含 `web.search` / `web.fetch` / `web.extract`，经 `Executor::WebAccess` 调用。
- 实网搜索后端：**DuckDuckGo HTML**（无付费 key）。可选 Brave：`BRAVE_SEARCH_API_KEY` + DDG 软回退。
- 默认路径无 Brave 计费；live 网络可选、非 CI 门禁。

## 不做声明

- **不**宣称完整 PDF 或任何视频能力。
- **不**证明 Bridge/WebUI 全接线或 F10 全范围完成。
