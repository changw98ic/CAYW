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
| Q4 | 结果释放与进程清理一致 | **部分**：tip 已跑真实 `test.run` 进程 → `processes_stopped` + CapabilitySettled 重开。**未跑** `result.release` | 完整封口仍需 A | `process_cleanup_and_settled_result_survive_reopen`（unix） |
| Q5 | 持久后台：`bg.*` 完成后归属原 Session；Runtime/网关重开后 Ownership 可读 | **pending / ignored**（本分支无 `bg.*`） | P | `pending_background_ownership_survives_reopen` |
| Q6 | MCP：与结果同 Session 可恢复 | **部分**：tip 已跑 fixture MCP list/read + Bridge 终态重开。**未跑** F14 懒起/空闲停 | 完整封口仍需 R（+A） | `mcp_resources_share_session_and_survive_reopen` |
| Q7 | WebUI/TUI/SDK 同场消费可恢复状态（认证 IPC 或公开 SDK） | **pending**（本 PR 范围外） | A+P+R 后组合轮 | 封口轮次另记 |
| Q8 | 多模块同 Session 重启恢复总表 | **pending**（不要求本 PR 合并 #37–#54） | A+P+R | 封口轮次另记 |

诚实规则：`scripts/run-f20-combo-smoke.sh` 在存在 ignored / remaining_gaps 时 **`f20_combo_complete` 恒为 false**；`exit 0` / `passed=true` 只表示已启用用例通过，**不得**读成 F20 封口。`CLAIM_FULL_F20=1` 会在仍有缺口时强制非零退出。

## 新增/变更入口

| 路径 | 作用 |
| --- | --- |
| `crates/cayw-runtime/tests/f20_combo_smoke.rs` | Q1–Q3 底座 + tip Q4/Q6；Q5 ignore |
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

## 中文结项要点（可直接转述）

1. **切片 Q 在本分支 tip 上补强组合烟测**：Q1–Q3 仍在；Q4 改为真实进程清理+结果重开；Q6 改为真实 MCP 资源同 Session 重开；不是新调度器，不合并其他功能 PR。
2. **Q5 / Q7 / Q8 与 Q4·Q6 的完整封口半边仍 pending**：`bg.*`、WebUI/TUI/auth IPC、同树总表、`result.release`、F14 懒起/空闲停 — 文档与 summary 诚实标明。
3. **脚本不再把 ignored 静默当成 F20 成功**：`f20_combo_complete` 恒 false，直至缺口清零；可选 `CLAIM_FULL_F20=1` 强制失败。
4. **不宣称 F20/#33 完成**；本 PR 保持对 `dev` 未合并。
