# F11 来源记录：视觉工具路由（SLICE N）

## 标识

| 字段 | 值 |
| --- | --- |
| 功能编号 | F11 |
| 总规划 | [#33](https://github.com/changw98ic/CAYW/issues/33) |
| 切片 | SLICE N — vision tools / vision router 首刀 |
| 采用方式 | 设计参考（非代码移植、非外部运行时依赖） |
| CAYW 实现 | `crates/cayw-vision` |

## 参考来源

| 项目 | 链接 | 许可 | 使用范围 |
| --- | --- | --- | --- |
| dsh-vision-router | https://github.com/ysr666/dsh-vision-router | MIT（上游声明） | 仅参考视觉问答 / OCR / 定位与裁切 / 像素差 / 答案缓存的产品形状；明确「视觉模型是眼睛、主对话模型是大脑」的工具化拆分 |

未复制上游 TypeScript / Cordis / sharp / tesseract 源码，未引入 Node / DSH / 云端免费视觉链依赖。CAYW 实现为独立 Rust 路由层：本地像素运算在进程内完成；OCR / describe 经可注入后端；缓存按图像与提示摘要命中后不再计费。

## 本切片交付

- `ImageBuffer` / `Region` / 内容 SHA-256 身份
- 本地 `crop` / `pixel_diff` / `dominant_colors`
- `FixtureOcrEngine`、`CountingDescribeBackend`、`AnswerCache`
- `VisionRouter` 统一入口
- 单元 / 集成测试与 `offline_vision` 示例
- 本源记录与 crate README
- 验收笔记 `docs/validation/f11-vision-router-slice-n-20261025.md`

## 明确未交付（后续切片）

- 云端 / Ollama / LM Studio 视觉后端与故障转移链
- `vision_ground` / `vision_detect` 模型定位、桌面 / HTML 截图、SVG trace、抠图、长图 OCR
- Host / Runtime / CodeMode 工具名暴露与结果句柄（对齐 F01/F03）
- 与 F18 页面预览 / 编辑产物关联
- 将 F11 全范围标为完成

## 验收口径（本切片）

本地 `cargo test -p cayw-vision` 证明：固定样图 OCR / 像素差输出稳定；describe 缓存命中不增加后端计费次数。不把本切片标为 F11 全范围完成。
