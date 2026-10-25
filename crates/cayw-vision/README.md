# cayw-vision

CAYW 视觉工具路由（F11 首刀，SLICE N）。

提供**本地像素工具**（裁切 / 像素差 / 主色）、可注入的 OCR / 视觉问答后端，以及按内容摘要命中的答案缓存。缓存命中**不重复计费**后端请求。本 crate **不**直连云端视觉 Provider、不做桌面截图、不暴露 Host/Runtime 工具名——那些在后续切片接结果句柄与模型后端。

## 首刀范围

- `ImageBuffer`：宿主规范 RGBA8 光栅 + 内容 SHA-256
- 本地：`crop` / `pixel_diff`（差比 + 8×8 最差格）/ `dominant_colors`
- `FixtureOcrEngine` + `CountingDescribeBackend`（离线 fixture / 计量）
- `VisionRouter`：统一入口；OCR / describe 走 `AnswerCache`
- 协议版本：`VISION_PROTOCOL_VERSION = 1`

本切片**不**交付：真实云端 / Ollama 视觉链、桌面 / HTML 截图、SVG trace / 抠图、Host 工具接线、与 F18 预览关联、将 F11 全范围标完成。

## 快速使用

```rust
use cayw_vision::*;
use std::sync::Arc;

let img = ImageBuffer::solid(8, 8, [255, 255, 255, 255])?;
let mut ocr = FixtureOcrEngine::new();
ocr.insert(img.sha256(), "HELLO");
let describe = Arc::new(CountingDescribeBackend::new());
let router = VisionRouter::new(Arc::new(ocr), describe);
let text = router.ocr(&img, &OcrRequest { engine_hint: None })?;
```

## 来源与边界

- 功能：F11（[#33](https://github.com/changw98ic/CAYW/issues/33)）
- 采用方式：设计参考（非代码移植）
- 参考：[ysr666/dsh-vision-router](https://github.com/ysr666/dsh-vision-router) 的视觉问答 / OCR / 定位裁切 / 像素差 / 缓存产品形状
- 记录：[`docs/sources/f11-vision-router.md`](../../docs/sources/f11-vision-router.md)

## 验证

```sh
cargo test -p cayw-vision --locked
cargo clippy -p cayw-vision --all-targets --locked -- -D warnings
cargo run -p cayw-vision --example offline_vision --locked
```

测试为本地纯函数与内存缓存，无网络、无 Provider、无真实 UI。
