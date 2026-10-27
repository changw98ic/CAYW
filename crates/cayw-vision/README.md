# cayw-vision

CAYW 视觉工具路由（F11，SLICE N）。

提供**本地像素工具**（裁切 / 像素差 / 主色）、可注入的 OCR / 视觉问答后端，以及按内容摘要命中的答案缓存。缓存命中**不重复计费**后端请求。

## 后端

| 能力 | 真实后端 | CI / 无依赖回退 |
| --- | --- | --- |
| OCR | `TesseractOcrEngine`（系统 `tesseract` CLI；可用 `CAYW_VISION_TESSERACT` 指定路径） | `FixtureOcrEngine`（`CAYW_VISION_OCR=fixture` 强制；未安装 tesseract 时自动选用） |
| describe | `HttpDescribeBackend`（`CAYW_VISION_DESCRIBE_URL`，可选 `CAYW_VISION_DESCRIBE_KEY`） | `CountingDescribeBackend`（离线计量 / 可 seed） |

像素算法（`crop` / `pixel_diff` / `dominant_colors`）始终在进程内运行，不依赖外部二进制或网络。

## Host / Runtime 目录

`definitions()` 注册：

- `vision.crop`
- `vision.pixel_diff`
- `vision.colors`
- `vision.ocr`
- `vision.describe`

`cayw-host` 在 `HostConfig.vision = Some(Limits)` 时把上述工具编入目录，默认不提供；Runtime 通过 `host.capabilities()` 可见。图像参数为 RGBA8 + `rgba_base64`。

Runtime 把裁切图像写入同一 Device 内容库，返回正式 `ResultHandleView`，保留尺寸、MIME 和像素摘要。后续图像输入可按引用读取，Base64 只解码一次。注入内容接口后，存放失败直接返回，不退回进程内引用。独立 Rust 使用者可选择自己的存放接口。

## 快速使用

```rust
use cayw_ai::CancellationToken;
use cayw_vision::*;
use std::sync::Arc;

let img = ImageBuffer::solid(8, 8, [255, 255, 255, 255])?;
let mut ocr = FixtureOcrEngine::new();
ocr.insert(img.sha256(), "HELLO");
let describe = Arc::new(CountingDescribeBackend::new());
let router = VisionRouter::new(Arc::new(ocr), describe);
let text = router.ocr(&img, &OcrRequest { engine_hint: None }, &CancellationToken::new())?;
```

生产 Host 侧用 `VisionService::from_env(Limits::default())`：有 tesseract 则走真实 OCR，否则 fixture。Host 取消时 `tesseract` / describe 的 `curl` 经 KillOnDrop 杀掉，不留孤儿进程。

## 来源与边界

- 功能：F11（[#33](https://github.com/changw98ic/CAYW/issues/33)）
- 采用方式：设计参考（非代码移植）
- 参考：[ysr666/dsh-vision-router](https://github.com/ysr666/dsh-vision-router)
- 记录：[`docs/sources/f11-vision-router.md`](../../docs/sources/f11-vision-router.md)

本切片**不**交付：云端故障转移链、桌面 / HTML 截图→元素定位闭环、ground/detect、SVG trace、与 F18 预览关联、将 F11 全范围标完成。

## 验证

```sh
cargo test -p cayw-vision --locked
cargo clippy -p cayw-vision --all-targets --locked -- -D warnings
cargo run -p cayw-vision --example offline_vision --locked
```

无 tesseract、无 `CAYW_VISION_DESCRIBE_URL` 时，fixture / counting 后端保证 CI 可离线通过。
