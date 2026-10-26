# F11 来源记录：视觉工具路由（SLICE N）

## 标识

| 字段 | 值 |
| --- | --- |
| 功能编号 | F11 |
| 总规划 | [#33](https://github.com/changw98ic/CAYW/issues/33) |
| 切片 | SLICE N — vision tools / vision router |
| 采用方式 | 设计参考（非代码移植） |
| CAYW 实现 | `crates/cayw-vision` |

## 参考来源

| 项目 | 链接 | 许可 | 使用范围 |
| --- | --- | --- | --- |
| dsh-vision-router | https://github.com/ysr666/dsh-vision-router | MIT（上游声明） | 仅参考视觉问答 / OCR / 定位与裁切 / 像素差 / 答案缓存的产品形状；明确「视觉模型是眼睛、主对话模型是大脑」的工具化拆分 |

未复制上游 TypeScript / Cordis / sharp / tesseract 源码。CAYW 实现为独立 Rust 路由层：本地像素运算在进程内完成；OCR 经系统 `tesseract` CLI（可回退 fixture）；describe 可选 HTTP（`CAYW_VISION_DESCRIBE_URL`）否则 counting/fixture；缓存按图像与提示摘要命中后不再计费。

## 本切片交付

- `ImageBuffer` / `Region` / 内容 SHA-256 身份
- 本地 `crop` / `pixel_diff` / `dominant_colors`（真实像素算法）
- `TesseractOcrEngine` + `FixtureOcrEngine`；`HttpDescribeBackend` + `CountingDescribeBackend`
- `AnswerCache`、`VisionRouter`、`VisionService`
- Host 目录：`vision.crop` / `vision.pixel_diff` / `vision.colors` / `vision.ocr` / `vision.describe`（Runtime 经 Host capabilities 可见）
- 单元 / 集成测试与 `offline_vision` 示例
- 本源记录与 crate README；验收笔记 `docs/validation/f11-vision-router-slice-n-20261025.md`

## 明确未交付（后续切片）

- 云端 / Ollama / LM Studio 视觉故障转移链
- `vision_ground` / `vision_detect`、桌面 / HTML 截图、SVG trace、抠图、长图 OCR
- 结果句柄深度对齐（F01/F03）与 F18 页面预览关联
- 将 F11 全范围标为完成

## 验收口径（本切片）

本地 `cargo test -p cayw-vision` 证明：固定样图 OCR / 像素差输出稳定；describe 缓存命中不增加后端计费次数；目录含五项 `vision.*` 工具；有 tesseract 时真实 OCR 后端可调用，无依赖时 fixture 回退可用。不把本切片标为 F11 全范围完成。
