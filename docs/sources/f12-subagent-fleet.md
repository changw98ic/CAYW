# F12 来源记录：子代理委派 / FleetView（SLICE O）

## 标识

| 字段 | 值 |
| --- | --- |
| 功能编号 | F12 |
| 总规划 | [#33](https://github.com/changw98ic/CAYW/issues/33) |
| 切片 | SLICE O — subagent fleet / FleetView 首刀 |
| 采用方式 | 设计参考（非代码移植、非外部运行时依赖） |
| CAYW 实现 | `cayw-runtime`（`children` + `fleet`）+ WebUI 公开 `runtime.fleet*` |

## 参考来源

| 项目 | 链接 | 许可 | 使用范围 |
| --- | --- | --- | --- |
| pi-subagents | https://github.com/nicobailon/pi-subagents ；[官方包页](https://pi.dev/packages/pi-subagents) | MIT（上游声明） | 仅参考前台/后台委派、FleetView 状态、运行中追加指令与停止的产品形状；明确为 Nico 版本交互参考 |

未复制上游 TypeScript / 扩展源码，未引入 Pi / Node runner 依赖。调度、预算、结果与恢复归属仍接 CAYW Session/Worker（既有 `agent.run` 真子会话）。

## 本切片交付

- `agent.run` 增加 `mode=foreground|background`（默认前台，保持 Issue 5 等待语义）
- 后台 / detach：父能力立即返回句柄；父取消与 `drain` **不**停止已 detach 成员
- FleetView：`agent.fleet` 能力 + `Runtime::fleet` / IPC `runtime.fleet`
- 干预：`agent.stop` / `agent.steer` + `runtime.fleet.stop` / `runtime.fleet.steer`
- WebUI 公开方法白名单接入上述只读/干预 RPC
- 深度 / 并发 / 共享 token / `max_time` 与树绝对 deadline 仍生效
- 本源记录

## 明确未交付（后续）

- 完整 TUI FleetView 渲染、多 agent 角色目录、council / workflow 脚本
- 父结束后 detach 子的跨进程持久 runner（本刀仍在同一 Runtime 进程内）
- 将 F12 全范围或 F20 组合验收标为完成

## 验收口径（本切片）

本地 `cargo test -p cayw-runtime --test children --locked` 覆盖前台回归与后台 detach / FleetView / 父取消不影响 detach；预算与深度既有用例仍通过。不把本切片标为 F12 全范围完成。
