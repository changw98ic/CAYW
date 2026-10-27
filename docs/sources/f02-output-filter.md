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

## Host 接线（本分支后续提交）

- `HostConfig::output_filter`：默认 `None`。宿主可显式配置过滤；Runtime 未配置内容库时不启用缩减
- `build.run`/`test.run`/`check.run`/`lint.run`/`format.run` 与 `git.*` 读结果经 `process_value` 调用 `apply_filter` + `dual_entry`
- 启用时 JSON 中 `stdout`/`stderr` **仅为**过滤摘要；**不**再内联 `stdout_original`/`stderr_original`（否则无真实上下文减量）
- 当过滤实际减量（`reduction.changed`）时，先把完整原文写入 Host/Runtime 共用的 Device 内容库，再返回摘要与 `output_filter.*.original_handle`
- `original_handle` 使用正式 `ResultHandleView`，包含 Session、scope、内容摘要、字节数、编码与释放状态；`original_digest` 只表示内容摘要，不是另一条裸摘要读取入口
- 元数据同时保留 reduction / `original_inline: false`；存放错误保留原始未知状态和停止标记
- ContentStore 未配置却需要外置时 **fail closed**（明确报错），绝不静默丢弃唯一原文副本
- `git.blame` / `git.log` / `git.show` 自动 `Passthrough`（`max_lines=0`），避免 Generic head/tail 拆坏 porcelain 记录；未减量时无需 handle
- 设为 `None` 或 `enabled: false` 时正常返回 stdout/stderr
- 模型通过正式 `result.get` 分页读取原文；宿主的 `Host::read_output_original(handle_id)` 使用相同持久引用校验
- 不测量任务 wall/cost（仍由调用方传入 `compare_filter_toggle`）
- 终端流式 `terminal.read` 字节游标语义不变，本切片不改

## 明确未交付（后续）

- 终端会话流式输出过滤
- 可配置 TOML 规则包与社区过滤器导入
- 将 F02 全范围标为完成

## 验收口径（本切片）

本地 `cargo test -p cayw-output-filter --locked` 覆盖：同一 shell 捕获开/关过滤，摘要与原文均可取；记录输出减少量；对比报告声明减少量 ≠ 任务总收益。不把本切片标为 F02 全范围完成。
