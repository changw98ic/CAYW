# F20 / SLICE Q 组合验收骨架（2026-10-25）

日期：2026-10-25（America/New_York）。范围：Issue [#33](https://github.com/changw98ic/CAYW/issues/33) **F20 收口**的组合验收**骨架与底座烟测**，不是新调度器，也不重开 [#11](https://github.com/changw98ic/CAYW/issues/11) / [#18](https://github.com/changw98ic/CAYW/issues/18) 绿场。

分支：`feat/f20-combo-acceptance`（基于 `origin/dev`）。来源记录见 [f20-combo-acceptance.md](../sources/f20-combo-acceptance.md)。

## 目标与边界

| 项 | 说明 |
| --- | --- |
| SLICE | Q（仅本切片） |
| 目标 | Cells 结果生命周期 + MCP/子代理/后台 + WebUI/TUI/SDK **同场**消费可恢复任务/进程/结果归属；先落清单与底座烟测 |
| 底座（已在 tip） | #11 调度、#18 ContentStore、#4/#7 进程与恢复、`agent.run` 子代理 |
| 封口依赖 | 至少 A（结果句柄）+ P（后台/网关归属）+ R/F14（MCP 生命周期）；O（Fleet）可选 |
| 不宣称 | F20 / #33 全范围完成；WebUI 真机浏览器关断全链；真实 Provider 付费请求 |

## 组合验收清单（封口时逐项填证据）

| ID | 场景 | 底座状态 | 封口依赖 | 证据位 |
| --- | --- | --- | --- | --- |
| Q1 | 同 Session：Session SQLite 重开后消息/事件仍在；macOS 另跑 ContentStore `content_inspect` | Linux 底座已覆盖 Session；ContentStore 限 macOS（Linux O_PATH/`EBADF` 已知缺口） | — | `base_combo_session_reopen_*`；macOS `base_combo_content_*` |
| Q2 | 跨 Session：A/B 用户回合与子代理事件不串号 | 底座烟测已覆盖 | — | `f20_combo_smoke` |
| Q3 | 同场子代理：`agent.run` 产生独立 child Session/Task，父不串提示词；重开后身份仍在 | 底座烟测已覆盖 | O 增强 Fleet 后补 | `f20_combo_smoke` |
| Q4 | 结果释放与进程清理一致：`result.release` 后不可取回；对应进程 cleanup 已确认 | **pending** | A + 进程路径 | 忽略测 `pending_result_release_aligns_with_process_cleanup` |
| Q5 | 持久后台：`bg.*` 完成后归属原 Session；Runtime/网关重开后 Ownership 可读 | **pending** | P | 忽略测 `pending_background_ownership_survives_reopen` |
| Q6 | MCP：懒起/空闲停经 CodeMode，与结果句柄同 Session 可恢复 | **pending** | R/F14 + A | 忽略测 `pending_mcp_lifecycle_shares_session_results` |
| Q7 | WebUI/TUI/SDK 同场消费可恢复状态（认证 IPC 或公开 SDK） | **pending** | A+P+R 后组合轮 | 封口轮次另记 |
| Q8 | 多模块同 Session 重启恢复总表（Cells + content + 子代理/后台 + MCP） | **pending** | A+P+R | 封口轮次另记 |

未合入 A/P/R 前，Q4–Q8 保持 pending / `#[ignore]`，不得改写为已通过。

## 新增入口

| 路径 | 作用 |
| --- | --- |
| `crates/cayw-runtime/tests/f20_combo_smoke.rs` | 底座组合烟测 + pending 忽略桩 |
| `scripts/run-f20-combo-smoke.sh` | 一键跑烟测并写 `.local/f20-combo-smoke/` 摘要 |
| `docs/sources/f20-combo-acceptance.md` | 来源与边界 |
| 本文件 | 清单与执行记录 |

## 本机命令

```sh
./scripts/run-f20-combo-smoke.sh .local/f20-combo-smoke/linux-r01
# 或直接：
cargo test -p cayw-runtime --test f20_combo_smoke --locked
```

`--include-ignored` 仅用于确认 pending 桩仍存在且文档化；忽略测失败不得计为底座通过。

## 本轮执行

| 命令 | 平台 | 结果 |
| --- | --- | --- |
| `./scripts/run-f20-combo-smoke.sh .local/f20-combo-smoke/linux-r01` | Linux x86_64，Rust 1.95.0 | exit 0；1 passed / 3 ignored；`summary.passed=true`；`summary.json` SHA-256 `bd98f00a8f4e151827c233ed2a01268a614b625c1322fe0417df3b57022cc272`；log SHA-256 `edeca53ddfb7c9d7b9218c75278f4259d9271063c9a4f029885550fa3a4e465d` |

## 中文结项要点（可直接转述）

1. **切片 Q 只交付 F20 组合验收骨架**：清单 + 底座烟测 + 来源说明；不是新调度器，不重开 #11/#18。
2. **底座烟测已覆盖**：同 Session SQLite 重开、跨 Session 隔离、同场 `agent.run` 子代理身份不串号；ContentStore 重开另限 macOS（Linux 已知 O_PATH/`EBADF`）。
3. **封口仍等 A/P/F14（及可选 O）**：结果释放对齐进程清理、后台/网关归属、MCP 同 Session、WebUI/TUI/SDK 同场 — 现为 pending/`#[ignore]`。
4. **不宣称 F20/#33 完成**：本 PR 保持对 `dev` 未合并，直至封口轮次有同源码证据。
