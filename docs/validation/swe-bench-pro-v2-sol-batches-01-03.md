# SWE-bench Pro V2 第 1–3 批执行记录

GPT‑6.2 Sol 通过 CAYW 处理了选题清单的前 15 道 Go 题，每批五题。官方评分脚本确认 **14/15** 道通过。第三批结束后停止，第四批未启动。

| 批次 | 题号 | 通过 |
|---|---:|---:|
| 1 | 1–5 | 5/5 |
| 2 | 6–10 | 4/5 |
| 3 | 11–15 | 5/5 |

| 题号 | 上游仓库 | 官方高难 | 评分结果 | 通过的必要测试 |
|---:|---|---|---|---:|
| 1 | `navidrome/navidrome` | 是 | 通过 | 3/3 |
| 2 | `gravitational/teleport` | 是 | 通过 | 14/14 |
| 3 | `flipt-io/flipt` | 是 | 通过 | 10/10 |
| 4 | `flipt-io/flipt` | 是 | 通过 | 5/5 |
| 5 | `gravitational/teleport` | — | 通过 | 114/114 |
| 6 | `flipt-io/flipt` | 是 | 通过 | 6/6 |
| 7 | `flipt-io/flipt` | — | 通过 | 23/23 |
| 8 | `flipt-io/flipt` | 是 | 通过 | 3/3 |
| 9 | `future-architect/vuls` | — | 通过 | 97/97 |
| 10 | `gravitational/teleport` | — | 没有补丁 | — |
| 11 | `navidrome/navidrome` | — | 通过 | 3/3 |
| 12 | `future-architect/vuls` | — | 通过 | 9/9 |
| 13 | `navidrome/navidrome` | — | 通过 | 2/2 |
| 14 | `flipt-io/flipt` | — | 通过 | 47/47 |
| 15 | `gravitational/teleport` | — | 通过 | 16/16 |

## 运行与评分

- 本机是 macOS arm64。主会话经本地 WebUI 的 RPC 提交任务，再由 CAYW 把每道题派到各自的工作区子会话。主会话和子会话都选用 `codex/gpt-6.2-sol`。
- 用独立的 `linux/amd64` 官方镜像和官方评分脚本验证原补丁。模型只收到公开题面及基础源码。复用的镜像层均按官方摘要校验。下载报错按环境故障记录，恢复后用相同补丁补评。
- 第 1、3 批的模型任务均完成并保存非空补丁，执行记录中没有 `test.run` 调用。第 2 批没有完整的批次结束记录，逐题保留了可核对的任务状态与补丁结果。
- 第 7 题的原进程退出时，任务记录仍为运行中。保存的部分补丁通过了 23/23 项必要测试。原进程退出原因未知。
- 第 9 题第一次评分通过 90/97 项。Go 编译器在编译标准库时崩溃，其余七项未运行。在全新官方容器里用同一补丁重评，97/97 项通过。
- 第 10 题任务显示完成，导出的补丁为空。

## 证据范围

- CAYW 构建的源码基线为 `5fcd012cff86f01d6df2ba8f938bc828341f28f1`，构建工作区含未提交改动。JSON 保存了实际使用的二进制摘要。
- 这次是本地 CAYW 运行，完成条件与官方排行榜的固定时间和网络规则不同。成绩适用于本次运行。
- [逐题数据](swe-bench-pro-v2-sol-batches-01-03.json)保存实例 ID、基础提交、补丁摘要、镜像摘要和必要测试数。
- [官方数据集](https://huggingface.co/datasets/ScaleAI/SWE-bench_Pro/tree/2d52cb3df914a3fcf80c7f66738b3a88ae37fc50) · [评分程序版本](https://github.com/scaleapi/SWE-bench_Pro-os/tree/66f92766bba642462d4bbe5479e83f91f9211862/v2)
