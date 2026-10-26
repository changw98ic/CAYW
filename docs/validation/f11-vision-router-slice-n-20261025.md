# F11 / SLICE N 验收说明（2026-10-25，补强 2026-10-26）

## 范围

Issue [#33](https://github.com/changw98ic/CAYW/issues/33) F11：`cayw-vision` 视觉路由（本地像素工具 + 真实 OCR/可选 HTTP describe + fixture 回退 + 答案缓存 + Host/Runtime 目录接线）。

分支：`feat/f11-vision-router`（基于 `origin/dev`）。

## 本机命令

```sh
cargo test -p cayw-vision --locked
cargo clippy -p cayw-vision --all-targets --locked -- -D warnings
cargo run -p cayw-vision --example offline_vision --locked
```

可选：安装 `tesseract-ocr` 后 `TesseractOcrEngine::available()` 为真，单元测试会调用真实 CLI。CI 无该依赖时走 `FixtureOcrEngine`（可设 `CAYW_VISION_OCR=fixture`）。

## 结果

- 集成测试覆盖：固定样图 OCR 稳定、像素差稳定、describe 缓存命中不计费、裁切/主色本地运算、尺寸不匹配显式报错。
- Host catalog：`vision.crop` / `vision.pixel_diff` / `vision.colors` / `vision.ocr` / `vision.describe` 经 `definitions()` 注册；`HostConfig.vision` 默认启用。
- 示例打印 OCR、像素差比与缓存二次 describe（`billed_second=0`）。
- 无 `CAYW_VISION_DESCRIBE_URL`、无 tesseract 时仍可离线测试（fixture / counting）。

## 不做声明

本记录不证明 F11 全范围完成，也不证明桌面截图链或云端故障转移可用。
