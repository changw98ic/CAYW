# F02 来源记录：命令输出过滤（SLICE B）

## 标识

| 字段 | 值 |
| --- | --- |
| 功能编号 | F02 |
| 总规划 | [#33](https://github.com/changw98ic/CAYW/issues/33) |
| 切片 | SLICE B — command output filter 首刀 |
| 采用方式 | 设计参考（非代码移植、非外部运行时依赖） |
| CAYW 实现 | `crates/cayw-output-filter` |

## 参考来源

| 项目 | 链接 | 许可 | 使用范围 |
| --- | --- | --- | --- |
| RTK（rtk-oss 命令过滤部分） | https://github.com/rtk-ai/rtk ；[Editions](https://www.rtk-ai.app/docs/resources/editions/) | Apache-2.0（oss 过滤部分；完整版另有 BUSL 引擎，不采用） | 仅参考「确定性过滤规则 + 保留完整原文入口 + 输出减少与任务收益分列」的产品形状；不移植 TOML 规则引擎、hook 改写或压缩引擎 |

未复制上游 Rust/TOML 源码，未引入 RTK 二进制或 hook。CAYW 实现为独立 Rust 库，内置少量确定性 profile（`generic` / `cargo_like` / `git_status_like` / `passthrough`）。

## 本切片交付

- `apply_filter` / `dual_entry` / `compare_filter_toggle`
- ANSI 剥离、错误/变化行保留、head/tail/`max_lines`
- 同任务开/关对比记录（wall / 可选 cost / 失败重读）与明确 disclaimer
- 单元测试、`offline_filter` 示例、本源记录与 crate README

## 明确未交付（后续）

- Host 终端/`exec_command` 自动接线
- F01 ContentStore 结果句柄持久化对齐
- 可配置 TOML 规则包与社区过滤器导入
- 将 F02 全范围标为完成

## 验收口径（本切片）

本地 `cargo test -p cayw-output-filter --locked` 覆盖：同一 shell 捕获开/关过滤，摘要与原文均可取；记录输出减少量；对比报告声明减少量 ≠ 任务总收益。不把本切片标为 F02 全范围完成。
