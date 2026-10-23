# Issue #2、#3、#4：macOS 交付检查

本次交付从 `dev` 的 `12621d859758c28c6caa239b0bdc458570843900` 建立独立分支。按 51 个明确路径导入已验收内容，并核对每个已有文件的共同基线和实际摘要。没有把原工作区中的构建产物、其他未提交工作或配置带入交付；远端已合入的 plugin 拆分保持不变。

#2 的验证基线已由 [PR #27](https://github.com/changw98ic/CAYW/pull/27) 合并到 `dev`。#3 的 Luau 场景与测量方法见 [Luau 验收](issue-3-luau-macos.md)。#4 的进程身份、取消与清理范围见 [进程验收](issue-4-macos-process-lifecycle.md)。各自的原始记录继续绑定各自实际使用的源码、程序、平台、输入和权限。

## 本次完整检查

2026-10-21 在本次交付目录执行以下五条命令，全部退出码为 0。环境为 macOS 27.0.1（26A434）、arm64，Rust/Cargo 1.95.0，默认 feature、debug/test 构建，关闭增量编译。原始命令未设置固定毫秒通过门槛。

| 实际命令 | 结果 |
| --- | --- |
| `cargo fmt --all -- --check` | 通过。 |
| `cargo check --workspace --locked` | 通过。 |
| `cargo test --workspace --locked` | 通过；原始汇总为 201 组、1,306 passed、0 failed、7 ignored、30 filtered。 |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | 通过。 |
| `cargo run -p cayw-agent --example offline --locked` | 通过；实际工具计算 `2 + 3 = 5`，结束时 `running=false`。 |

测试汇总保留 Cargo 原输出，含父测试启动的子程序，不将重复场景当作互相独立的验收项。源码检查覆盖完整已跟踪差异和 52 个具名文件；五条命令期间这些输入均未变化。这不表示追踪了编译器的全部输入，也不扩展到其他架构。

结果、命令原文、完整标准输出与标准错误、退出码、具名文件前后摘要和工具链记录保存到本机 `/Volumes/ORICO/cayw-issue234-delivery-evidence-20261021-a/validation-v1/`。`record.json` 的 SHA-256 为 `3736971c0bc931ef644000ba6426dfb882c0e93965d854d785e52157e704a767`，源码观察文件为 `source.json`。提交前还核对了原集成目录的 51 个导入文件保持原样。

## 真实模型补充及修复

2026-10-21 的既有授权真实调用使用 Magpie 的 DeepSeek V4.1 Flash，记录在 `/Volumes/ORICO/cayw-live-234-20261021-a/`。独立的十二项选定场景包括 #2 的公开 SDK 与认证 IPC 读取、#3 的直接 Agent/Luau 计算与错误修复、#4 的 Pipes/PTY 各四项进程场景。#4 的 24 个原进程全部确认停止；异常场景继续保留业务效果不明。

持久化 Runtime 另有后续修复和两项真实运行。`execute_code` 只在自己的准备参数步骤转换 `cursor`、`yield_time_ms`、`max_output_tokens` 中适用的数字字段。数字字符串必须精确表达非负整数；已经解析的浮点数还须处于安全整数范围。转换后仍执行原范围校验。Provider、原模型历史、Core 调用及 wait 记录保留原参数。

后续 v9 的普通计算和实际错误修复均完成，11 次实际请求共报告 28,115 token。普通计算的六次原始数字字符串调用全部进入实际执行。独立检查核对 231 项，包含实际文件、持久事件、工具结果配对、原始响应与模型参数相等、权限及执行身份。检查没有调用模型。原失败记录保留，不被后续成功改写；这些有限任务也不用于推算总体性能或模型成功率。

| 原材料 | SHA-256 |
| --- | --- |
| `final-real-model-report-v1.json` | `0dd204cac58b6479303d300ba2c215dac5ba45f9c44b9149e2da0692a7f55343` |
| `fixes-20261021/numeric-repair-final-v1.json` | `ea7b063d4d0c5e996fd6824d412937d8c7ce9d9d9615a8ba429358470429a101` |

token 是 Provider 报告的用量，不是货币费用。原共享授权为 100,000,000 token；本次交付检查不新增真实模型请求。上述本机路径不是公开下载链接。

## 关闭范围

#2 不等待 #6 的完整真实任务或 #7 的全部崩溃矩阵。比较缺少关键构建资料时保持 pending，不能写成比较通过。#3 的 Monty 对照是可选项。#4 只接受具名拓扑和实际执行过的 macOS ARM64 场景。#5、#6 及后续任务的完整验收不由本次交付代替。
