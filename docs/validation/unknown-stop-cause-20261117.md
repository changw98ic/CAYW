# 未知结果停止原因与取消恢复验证（2026-11-17）

本次修复未知副作用导致停止时的说明，并合并取消恢复验收任务中的重复读取。结果只覆盖下述源码、平台和场景。

## 源码与环境

- 源码基点：`664191568724e6a91ef613477ca94277a6f0a12c`，叠加本记录所在提交的代码修改。
- 平台：macOS / Darwin 27.0.0，arm64；Rust `1.95.0`。
- 真实模型：`opencode-go/deepseek-v4.1-flash`，通过本机 `http://127.0.0.1:3425/v1` 调用；本机服务免密。
- 原始证据目录：`/Volumes/ORICO/cayw-stop-cause-20261117-220206`。
- 冻结程序目录：`/Volumes/ORICO/cayw-stop-cause-20261117-220206-binaries`。
- `invocation.json` 保存源码差异摘要、文件摘要、模型、配置、平台和程序摘要。`evidence/final-source.json` 核对真实模型测试使用的文件与交付文件相同；真实模型运行后的修改只涉及压力测试和本文档。

## 修复与针对性回归

Runtime 原先通过普通取消停止未知副作用后的运行。任务保留了 `outcome_unknown`，但最后一条助手消息却是 `aborted` / `Agent run cancelled`。调用方因此会把“结果未知”和“主动取消”混淆。

`AgentControl::stop_with_error` 现在传递宿主的实际停止原因。Runtime 在未知结果已记录后调用它，最后一条助手消息变成 `error`，并明确说明原操作结果未知、重放前需核对。任务、子调用和原始记录仍保留未知结果。主动取消继续使用 `abort`。原因只属于本次运行，后续运行不会继承；后到的清理说明不会覆盖首个原因。

测试先复现原问题，再验证修复：

```sh
cargo +1.95.0 test -p cayw-runtime --test capability_recovery real_host_unknown_terminal_pairs_in_core_and_allows_follow_up_after_reopen --locked -- --nocapture
cargo +1.95.0 test -p cayw-agent --locked
cargo +1.95.0 test -p cayw-runtime --test capability_recovery --test cancel_terminal --test model_approvals --locked
```

第一条命令修改前失败：实际 `Aborted`，预期 `Error`；修改后通过。其余命令通过。日志保存在证据目录的 `evidence/` 下。

取消恢复验收任务现在把读取 `checkpoint.txt`、写入 `recovered.txt`、读取新文件核对放在同一段 CodeMode 脚本中。断言这三次能力调用各执行一次，并属于同一个执行单元。该修改优化这个验收任务，不提供通用文件缓存，也不阻止模型因新任务需要再次读取文件。

## 真实模型与执行记录复核

测试使用重新构建并冻结的程序。实际执行命令：

```sh
cargo +1.95.0 build -p cayw-runtime -p cayw-webui -p cayw-trace-debug -p cayw-visual-editor -p cayw-tui --features cayw-runtime/pi-compat,cayw-runtime/strongflow --locked
cargo +1.95.0 build --manifest-path examples/headless-client/Cargo.toml --locked
cargo +1.95.0 build -p cayw-run-inspector --example replay_runtime_events --locked
python3 -u /tmp/cayw-rerun-driver.py /Volumes/ORICO/cayw-stop-cause-20261117-220206
```

驱动已保存为 `evidence/driver.py`。本机复跑时可用该副本和新的证据目录；脚本依赖其中写明的历史 A/B 记录及原验证器。

| 场景 | 实际结果 |
| --- | --- |
| 待审批操作允许、拒绝 | 两项通过；拒绝的能力未执行 |
| 两个约 2 秒的短操作，省略等待参数 | 原调用等待完成，无额外模型查询 |
| 实际子进程写入后超时，结果未知 | 副作用只发生一次；助手明确报告未知原因 |
| 核对原操作后继续新任务 | 原任务仍为未知；核对记录独立保存；后续任务完成 |
| 一次提交 10 个任务，并发执行数配置为 2 | 观察到 2 个执行、8 个排队；最终 10 个各完成一次 |
| 主动取消、重启 Client、恢复后复制并核对 | 原任务为取消；恢复未重放模型请求和原写入；原文件仅读取一次 |
| 原始前端、协作服务、任务队列 A/B 记录回放 | 检查器保持历史未知结果，原证据游标仍为 `25440` |

这轮没有重新生成原始整题产品，也没有重新操作可视化编辑器页面。A/B 项是用本次程序回放历史执行记录，不能替代新的整题或浏览器验收。

真实模型运行共有 15 个 Session、2,156 条去重后的 Core 事件、29 次模型请求（28 次完成、1 次主动取消）、12 次 CodeMode 执行、14 次模型工具调用、13 次实际能力调用及 1 次未执行的拒绝记录。`evidence/verification.json` 核对调用起止配对与冻结程序摘要，通过。

独立复核脚本及结果位于 `evidence/trace-review-fixed/`。它核对了 2,738 项记录关系，失败项为 0。`fixed-findings.json` 保存两个修复点的原始片段：

- 未知结果后的助手终态为 `error`，原因包含 `outcome is unknown`；数据库原任务仍是 `outcome_unknown`，不占执行名额，后续任务是 `completed`。
- 恢复任务的读取、复制和核对只有三个能力调用，共用一个执行单元；重复读取列表为空。

测试自有的 Client 和界面服务已停止，结果见 `evidence/cleanup.json`。没有改写历史消息、手动解锁数据库或自动重放未知操作。

## 工作区回归

第一轮全量测试在 Anthropic 的 65,536 个空思考分片用例中触发测试助手的 20 秒总时限。该用例单独运行只需 3.84 秒，说明负载会影响这个计时条件。原始失败结果保存在 `evidence/initial-workspace-checks/`，没有覆盖。

该用例现在直接消费有限的本机模拟 HTTP 响应。保留全部分片、签名和终态检查。私有思考分片本身不会产生公开事件，不能沿用按公开事件等待的保护。这里只移除该压力用例的吞吐计时条件；小型用例原有的等待保护仍在。修改后 `cargo +1.95.0 test -p cayw-providers --test anthropic_thinking --locked` 的 9 项测试通过，耗时 5.40 秒。

工作区最终命令如下，五项退出码全部为 0。完整测试从 `2026-11-17T14:34:08Z` 运行至 `14:52:15Z`。命令、退出码、起止时间和原始日志见 `workspace-checks.json` 与 `evidence/workspace-*.log`。

```sh
cargo +1.95.0 fmt --all -- --check
cargo +1.95.0 check --workspace --locked
cargo +1.95.0 clippy --workspace --all-targets --locked -- -D warnings
cargo +1.95.0 test --workspace --locked
cargo +1.95.0 run -p cayw-agent --example offline --locked
```

## 检查与限制变更

产品新增检查、限制、兜底、自动重试：无。没有收紧原本允许的操作，也没有新增数值上限。真实模型场景使用的并发数、外部调用时限和制造未知结果的子进程时限属于本轮测试配置，见 `invocation.json`。新增的测试断言用于检查实际停止原因、取消隔离和恢复调用次数。
