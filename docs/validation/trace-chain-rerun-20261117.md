# 2026-11-17 dev 再次验收

本次基于 `7598874622b798c59da6ec7a9936cc36f41a8c5b` 重新构建并验收。拉取时与远端 dev 一致，工作区干净。平台为本机 Darwin 27.0.0 / arm64，Rust 1.95.0。

原始证据和完整报告：`/Volumes/ORICO/cayw-rerun-20261117-201550/`。程序副本在同级 `cayw-rerun-20261117-201550-binaries/`；摘要、配置、平台和脚本记录见 `invocation.json` 与 `rerun-invocation.json`。

## 真实模型与 trace

免密本机 Magpie，模型 `opencode-go/deepseek-v4.1-flash`。允许审批、拒绝审批、批量等待、未知结果核对、同工作区继续写入、并发排队和取消后重启恢复全部通过。

- 15 个会话、2163 条连续且相互一致的事件。
- 29 次真实模型请求：28 次完成、1 次取消；11 次代码执行，全部有终态。取消请求费用仍为未知。
- 14 个实际开始的子调用全部结算，另 1 个被拒调用明确未执行。原调用身份和实参核对通过。
- 两次各约两秒的实际进程操作放在同一 cell，省略等待时间，额外 wait 为 0。
- 未知写入核对前显示 `outcome_unknown`，确认失败事实后显示 `failed`；原副作用只发生一次，相同确认只追加一条记录。
- 10 个排队任务全部受理，观察到 2 个运行、8 个排队，最后全部完成并释放名额。
- 取消后重启、显式恢复未重做原写入，后续新任务成功。
- 原 A/B trace 重新通过本次构建的检查器：107 个关联步骤、5 个非零进程结果核对通过；原误报步骤显示 `outcome_unknown`，证据游标 25440。

固定程序摘要全部一致，测试拥有的服务已停止。初始产品的前端＋协作服务＋队列使用原 trace 重放，本次没有重新生成完整产品或操作可视化编辑器网页。

## 重跑发现及修正

首次全工作区测试在 `empty_argument_fragments_do_not_impose_a_response_quota` 失败，单独运行旧测试也在 20 秒总等待到期。这个用例发送 65,537 个空参数分片后，再发送真实参数和完成事件；它验证响应长度支持，不验证固定吞吐速度。

仅修改 [Responses 测试](../../crates/cayw-providers/tests/responses.rs)：将现有 20 秒挂起保护用于等待每个新事件，保留全部分片及最终 ToolUse、唯一工具调用检查，并补充 city=Paris 参数断言。调整等待方式后单项通过，整例耗时 20.69 秒；最终全量运行中 Responses 整组 20 项全部通过。产品解析器和产品时限均未修改。

首次失败日志和退出码分别保存在 `evidence/initial-workspace-tests.log`、`responses-original-repro.log` 和 `initial-workspace-checks.json`；最终命令与退出码见 `workspace-checks.json`。实际产品程序运行在基线提交；后续差异仅为测试修正和本验收文档。

## 最终检查

以下命令全部退出 0。日志在证据目录的 `evidence/workspace-*.log`。

```sh
cargo +1.95.0 fmt --all -- --check
cargo +1.95.0 check --workspace --locked
cargo +1.95.0 clippy --workspace --all-targets --locked -- -D warnings
cargo +1.95.0 test --workspace --locked
cargo +1.95.0 run -p cayw-agent --example offline --locked
```

新增产品检查、数值限制、兜底、自动重试：无。测试复用原 20 秒挂起保护，没有引入新数值。
