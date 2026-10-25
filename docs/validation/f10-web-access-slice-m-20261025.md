# F10 / SLICE M 验收说明（2026-10-25）

## 范围

Issue [#33](https://github.com/changw98ic/CAYW/issues/33) F10 首刀：`cayw-web-access` 统一 `web.*` 工具面与引用。

分支：`feat/f10-web-access`（基于 `dev`）。

## 本机命令

```sh
cargo test -p cayw-web-access --locked
cargo clippy -p cayw-web-access --all-targets --locked -- -D warnings
cargo run -p cayw-web-access --example offline_web_access --locked
```

## 结果

- 单元测试覆盖：公开 URL fixture 提取 → `content_ref` 可取回；search→extract 管线；离线 `FailureClass::Offline`；拒绝 `search.query`；PDF 字面量提取；能力名稳定。
- 示例打印 fixture 搜索命中与引用。
- 默认路径无外网、无真实搜索 Provider 计费请求。

## 不做声明

本记录不证明 F10 全范围完成，也不证明 Host/Bridge/WebUI 已接线或视频能力已交付。
