# F20 / SLICE Q 组合验收烟测（2026-10-25，审计补强续）

日期：2026-10-26（America/New_York）。范围：Issue [#33](https://github.com/changw98ic/CAYW/issues/33) **F20 收口**的组合验收烟测，不是新调度器，也不重开 [#11](https://github.com/changw98ic/CAYW/issues/11) / [#18](https://github.com/changw98ic/CAYW/issues/18) 绿场。

分支：`feat/f20-combo-acceptance`（基于 `origin/dev`）。来源记录见 [f20-combo-acceptance.md](../sources/f20-combo-acceptance.md)。

## 目标与边界

| 项 | 说明 |
| --- | --- |
| SLICE | Q（仅本切片；**不**要求同 PR 合并 #37–#54） |
| 目标 | 在本分支 tip 上可证明的组合路径：Session/子代理/进程清理/MCP 资源同场 + SQLite 重开 |
| 底座（已在 tip） | #11 调度、#18 ContentStore、#4/#7 进程与恢复、`agent.run` 子代理、Bridge MCP resources |
| 仍不宣称 | F20 / #33 全范围完成；`result.release`；`bg.*`/网关 Ownership；F14 懒起/空闲停；WebUI/TUI/auth IPC 同场；多模块同树封口 |

## 组合验收清单

| ID | 场景 | 本分支状态 | 封口依赖 | 证据位 |
| --- | --- | --- | --- | --- |
| Q1 | 同 Session：Session SQLite 重开后消息/事件仍在；macOS 另跑 ContentStore `content_inspect` | **已跑** Session；ContentStore 限 macOS | — | `base_combo_session_reopen_*`；macOS `base_combo_content_*` |
| Q2 | 跨 Session：A/B 用户回合与子代理事件不串号 | **已跑** | — | `f20_combo_smoke` |
| Q3 | 同场子代理：`agent.run` 独立 child Session/Task；重开后身份仍在 | **已跑** | O 增强 Fleet 后补 | `f20_combo_smoke` |
| Q4 | 结果释放与进程清理一致 | **部分**：tip 已跑真实 `test.run` 进程 → `processes_stopped` + CapabilitySettled 重开；并诚实证明 tip **无** `result.release` 目录项（Unknown capability）。**未跑** Slice A 软释放执行路径 | 完整封口仍需 A（#40） | `process_cleanup_*`（unix）；`result_release_not_registered_on_tip` |
| Q5 | 持久后台：`bg.*` 完成后归属原 Session；Runtime/网关重开后 Ownership 可读 | **pending / ignored**（本分支无 `bg.*` / #45） | P | `pending_background_ownership_survives_reopen` |
| Q6 | MCP：与结果同 Session 可恢复 | **部分进阶**：fixture MCP list/read + Bridge 重开仍在；**新增**真实 stdio 子进程 spawn→call/read→disconnect→reaped + 结算重开。**未跑** F14 ManagedStdio 懒起/空闲停 | 完整封口仍需 R（#41） | `mcp_resources_*`；`mcp_stdio_process_lifecycle_*`（unix） |
| Q7 | WebUI/TUI/SDK 同场消费可恢复状态（认证 IPC 或公开 SDK） | **pending**（本 PR 范围外） | A+P+R 后组合轮 | 封口轮次另记 |
| Q8 | 多模块同 Session 重启恢复总表 | **pending 封口**；tip 另跑 process+MCP stdio 同 Session 组合（child+Bridge 同 Runtime 多轮在 tip 仍触发 model reserve 拒绝）。**不**合并 #37–#54，**不**宣称 Q8 封口 | A+P+R | `tip_tree_session_child_process_and_mcp_stdio_combo`（unix；非封口） |

诚实规则：`scripts/run-f20-combo-smoke.sh` 在存在 ignored / remaining_gaps 时 **`f20_combo_complete` 恒为 false**；`exit 0` / `passed=true` 只表示已启用用例通过，**不得**读成 F20 封口。`CLAIM_FULL_F20=1` 会在仍有缺口时强制非零退出。

## 新增/变更入口

| 路径 | 作用 |
| --- | --- |
| `crates/cayw-runtime/tests/f20_combo_smoke.rs` | Q1–Q3 底座 + tip Q4/Q6（含 stdio 生命周期）+ tip 同树组合；Q5 ignore；Q4 result.release 缺席证明 |
| `scripts/run-f20-combo-smoke.sh` | 烟测 + 诚实 summary（ignored ≠ seal） |
| `docs/sources/f20-combo-acceptance.md` | 来源与边界 |
| 本文件 | 清单与执行记录 |

## 本机命令

```sh
./scripts/run-f20-combo-smoke.sh .local/f20-combo-smoke/linux-r02
# 或直接：
cargo test -p cayw-runtime --test f20_combo_smoke --locked
```

## 本轮执行

| 命令 | 平台 | 结果 |
| --- | --- | --- |
| （初轮）`./scripts/run-f20-combo-smoke.sh .local/f20-combo-smoke/linux-r01` | Linux x86_64 | exit 0；1 passed / 3 ignored；**当时** `summary.passed=true` 且未区分 seal — 审计指出不足 |
| `./scripts/run-f20-combo-smoke.sh .local/f20-combo-smoke/linux-r02` | Linux x86_64 | exit 0；3 passed / 1 ignored；exercised Q1–Q4、Q6；pending Q5/Q7/Q8；`f20_combo_complete=false`；`summary.json` SHA-256 `c0e8b91b46bfa6b7536f553608a5e054d98f264d0a4830cac1d8d1a96c80a6ab`；log SHA-256 `e2e14fcf764522720f01a275f77a60f27470621896989f13b84580fb848a336e` |
| `./scripts/run-f20-combo-smoke.sh .local/f20-combo-smoke/linux-r03` | Linux x86_64 | exit 0；**6 passed / 1 ignored**；exercised Q1–Q4、Q6；pending Q5/Q7/Q8；`f20_combo_complete=false`；新增 Q4 `result.release` 缺席、Q6 真实 stdio 启停、tip process+MCP 组合；`summary.json` SHA-256 `cbfbaf6b7f49757b56723801dddd7bbfc5710938e7d180f36889f6b73e97db58`；log SHA-256 `851404315141b98895fd1c8691717d1a36c89d9192978cd4327a6506d357561c` |

## 中文结项要点（可直接转述）

1. **切片 Q 在本分支 tip 上继续补强**：Q1–Q3 仍在；Q4 进程清理+结果重开，并增加 `result.release` 缺席证明（未合并 #40）；Q6 在 fixture 资源重开之外增加真实 MCP stdio 启停/reap；tip 同树组合覆盖 process+MCP stdio（child+Bridge 同 Runtime 多轮仍受限），仍非 Q8 封口。
2. **Q5 / Q7 / Q8 与 Q4·Q6 完整封口半边仍 pending**：`bg.*`（#45）、`result.release` 执行路径（#40）、F14 ManagedStdio 懒起/空闲停（#41）、WebUI/TUI/auth IPC、同树总表 — 文档与 summary 诚实标明；**未合并**其他功能 PR。
3. **脚本不再把 ignored 静默当成 F20 成功**：`f20_combo_complete` 恒 false，直至缺口清零；可选 `CLAIM_FULL_F20=1` 强制失败。
4. **不宣称 F20/#33 完成**；本 PR 保持对 `dev` 未合并。
