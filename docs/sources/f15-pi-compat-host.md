# F15 来源记录：Pi 兼容宿主（SLICE S）

## 标识

| 字段 | 值 |
| --- | --- |
| 功能编号 | F15 |
| 总规划 | [#33](https://github.com/changw98ic/CAYW/issues/33) |
| 切片 | SLICE S — 可选 Pi→CAYW 外部宿主映射 + 版本/兼容矩阵首刀 |
| 采用方式 | 设计参考（非代码移植、非外部 npm 运行时依赖） |
| CAYW 实现 | `crates/cayw-pi-compat` |

## 参考来源

| 项目 | 链接 | 许可 | 使用范围 |
| --- | --- | --- | --- |
| pi2dsh | https://github.com/weijiafu14/pi2dsh | 上游仓库声明为准 | 仅参考「Pi 公共扩展 ABI → 另一宿主」的兼容层形状、表面族划分与逐插件版本矩阵记账；不移植其 Node/Cordis 引擎或 shim |
| Pi coding-agent ABI | `@earendil-works/pi-coding-agent` **0.84.1**（pi2dsh 文档钉扎） | 上游声明为准 | 仅作矩阵中的 ABI 版本钉扎；本切片不加载该 npm 包 |

未复制上游 TypeScript / 扩展源码，未引入 Pi / Node runner 依赖。CAYW 实现为独立 Rust 可选宿主；Rust Core 继续独立。

## 本切片交付

- `CompatMatrix`：插件@version × 表面 → `full` / `partial` / `unavailable` / `incompatible`
- 表面族映射：tools / commands / session / events / interaction / lifecycle
- 可选 `PiCompatHost`（默认可构造为关闭）：经 `cayw-plugins` 装 / 调 / 卸
- 离线 `cayw-pi-fixture@0.1.0`（`load_native`）烟测与 `offline_pi_compat` 示例
- **真实路径（超出 fixture）**：`cayw-pi-sample-ext@0.1.0` 经 `load_external` 从磁盘装载子进程样例；缺失可执行文件记 `unavailable`
- `call_tool` 按 capability 名调用（不再写死 fixture 工具）
- 本源记录、crate README、验收说明

## 版本 / 兼容矩阵（首刀基线）

| 侧 | 版本 |
| --- | --- |
| Pi ABI 钉扎 | `@earendil-works/pi-coding-agent@0.84.1` |
| CAYW plugin 协议 | `PLUGIN_PROTOCOL_VERSION`（当前 1） |
| cayw-pi-compat 协议 | `PI_COMPAT_PROTOCOL_VERSION = 1` |

矩阵行由 `CompatMatrix::first_cut_baseline()` 生成；运行 `offline_pi_compat` 可打印完整 Markdown 表。要点：

- `cayw-pi-fixture@0.1.0`：`tools.registerTool` / `tools.invoke` / `lifecycle.unload` = **partial**（`load_native`）
- `cayw-pi-sample-ext@0.1.0`：同上三表面 = **partial**（`load_external` 磁盘/子进程；二进制存在时）
- 上述两包：`commands` / `events` / `session` = **unavailable**；`interaction.ui.select` = **incompatible**
- 真实 npm `@earendil-works/pi-coding-agent@0.84.1` 各表面族通配 = **unavailable**（本切片不加载；非 sample 子进程路径）

## 明确未交付（后续）

- 未修改 npm Pi 扩展的 ExtensionAPI 对象与事件总线 / Node ABI 桥（sample 子进程 ≠ npm 扩展运行时）
- UIContext / SessionManager / registerCommand 完整语义（族映射仅为说明）
- 将 F15 全范围或「全插件兼容」标为完成
- Host/Bridge/WebUI 默认启用该可选宿主

## 验收口径（本切片）

本地 `cargo test -p cayw-pi-compat --locked` 覆盖：关闭宿主 → `Disabled`；fixture 装→调→卸；**sample 从磁盘装→调→卸**；缺失可执行文件 → `Unavailable`；交互表面探测记 `incompatible` 非静默；真实 npm 行 `unavailable`；`Cancelled` 保持已知终态。不把本切片标为 F15 全范围完成。
