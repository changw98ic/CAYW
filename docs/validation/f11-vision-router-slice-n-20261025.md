# F11 / SLICE N 验收说明（2026-10-25）

## 范围

Issue [#33](https://github.com/changw98ic/CAYW/issues/33) F11 首刀：`cayw-vision` 视觉路由（本地像素工具 + fixture OCR/describe + 答案缓存）。

分支：`feat/f11-vision-router`（基于 `origin/dev`）。

## 本机命令

```sh
cargo test -p cayw-vision --locked
cargo clippy -p cayw-vision --all-targets --locked -- -D warnings
cargo run -p cayw-vision --example offline_vision --locked
```

## 结果

- 集成测试覆盖：固定样图 OCR 稳定、像素差稳定、describe 缓存命中不计费、裁切/主色本地运算、尺寸不匹配显式报错。
- 示例打印 OCR、像素差比与缓存二次 describe（`billed_second=0`）。
- 无网络、无 Provider、无真实模型请求、无桌面截图。

## 不做声明

本记录不证明 F11 全范围完成，也不证明 Host/Runtime 工具已接线或云端视觉链可用。
