# F15 来源记录：Pi 兼容宿主（SLICE S）

## 标识

| 字段 | 值 |
| --- | --- |
| 功能编号 | F15 |
| 总规划 | [#33](https://github.com/changw98ic/CAYW/issues/33) |
| 切片 | SLICE S — 可选 Pi→CAYW 外部宿主映射 + 版本/兼容矩阵首刀 |
| 采用方式 | 设计参考（非 pi2dsh 代码移植）+ **真实公开 npm Pi 扩展依赖（未改写）** |
| CAYW 实现 | `crates/cayw-pi-compat` |

## 参考来源

| 项目 | 链接 | 许可 | 使用范围 |
| --- | --- | --- | --- |
| pi2dsh | https://github.com/weijiafu14/pi2dsh | 上游仓库声明为准 | 仅参考「Pi 公共扩展 ABI → 另一宿主」的兼容层形状、表面族划分与逐插件版本矩阵记账；不移植其 Node/Cordis 引擎或 shim |
| Pi coding-agent ABI + hello 扩展 | `@earendil-works/pi-coding-agent` **0.84.1**（`examples/extensions/hello.ts`） | 上游声明为准 | ABI 钉扎；**原样加载**该包发布的 `hello.ts` 作为主验收扩展（经 CAYW 侧 `pi-npm-bridge` 适配） |

未复制/改写上游扩展源码。适配器仅在 CAYW 侧翻译 Pi ExtensionAPI ↔ CAYW external JSON。Rust Core 继续独立。

## 本切片交付（诚实口径）

**已证明：** 钉扎 npm 的 `hello.ts` **工具切片**（`tools.registerTool` / `tools.invoke`）。**不是**完整 Pi ExtensionAPI / F15 完成。

- `CompatMatrix`：插件@version × 表面 → `full` / `partial` / `unavailable` / `incompatible`
- 表面族**记账**映射：tools / commands / session / events / interaction / lifecycle（多数仍 unavailable）
- 可选 `PiCompatHost`（默认可构造为关闭）：经 `cayw-plugins` 装 / 调 / 卸
- 离线 `cayw-pi-fixture@0.1.0`（`load_native`）烟测
- 自制样例 `cayw-pi-sample-ext@0.1.0`（`load_external`；**非** Pi ExtensionAPI，保留作协议烟测）
- **主验收**：`@earendil-works/pi-coding-agent@0.84.1` 未修改 `hello.ts` → Node `npm/pi-npm-bridge.mjs`（仅 `registerTool` 实作）→ `load_external` → `hello` 工具调用
- `call_tool` 按 capability 名调用
- 本源记录、crate README、验收说明

## 版本 / 兼容矩阵（首刀基线）

| 侧 | 版本 |
| --- | --- |
| Pi ABI 钉扎 | `@earendil-works/pi-coding-agent@0.84.1` |
| CAYW plugin 协议 | `PLUGIN_PROTOCOL_VERSION`（当前 1） |
| cayw-pi-compat 协议 | `PI_COMPAT_PROTOCOL_VERSION = 1` |
| Node 引擎 | ≥ 22.19（上游 package engines） |

矩阵行由 `CompatMatrix::first_cut_baseline()` 生成。要点：

- `cayw-pi-fixture@0.1.0`：tools / lifecycle = **partial**（`load_native`）
- `cayw-pi-sample-ext@0.1.0`：tools / lifecycle = **partial**（自制 CAYW 协议；非 Pi ABI）
- `@earendil-works/pi-coding-agent@0.84.1`：`tools.registerTool` / `tools.invoke` / `lifecycle.install|unload` = **partial**（真实 npm + 仅 registerTool 的适配器；**hello 工具切片**）
- 上述包：`commands` / `events` / `session` / `session.sendMessage` = **unavailable**；`interaction.ui.select` = **incompatible**
- 桥接对不可用表面**抛 `unsupported:`**，禁止静默 noop

## 适配器工作方式

1. Host 解析 Node ≥22.19，必要时 `npm ci` 安装钉扎依赖
2. 子进程：`node npm/pi-npm-bridge.mjs --extension <abs>/hello.ts --package-name … --package-version …`
3. Bridge 用 jiti **原样 import** `hello.ts`，把最小 `ExtensionAPI`（`registerTool` 实作；commands/events/sendMessage/UI 等抛 `unsupported:`）交给默认导出工厂
4. 收集到的工具成为 CAYW `PluginManifest.capabilities`；握手走 CAYW external JSON
5. `Call(hello, {name})` → `tool.execute(...)` → 返回 `{ text, details, pi_extension_api: true, extension_path, protocol: "pi-ExtensionAPI", … }`

## 明确未交付（后续）

- 完整事件总线 / SessionManager / `registerCommand` / `sendMessage` 作为 CAYW capability / UIContext
- 任意第三方 Pi 扩展的全表面自动兼容（需按包@version 扩矩阵与适配面）
- Host/Bridge/WebUI 默认启用该可选宿主
- 将 F15 全范围或「全插件兼容」/「完整 ExtensionAPI 兼容」标为完成（当前仅 hello 工具切片）

## 验收口径（本切片）

- `cargo test -p cayw-pi-compat --locked` 含 **`npm_hello_extension_install_call_unload`** 与 **`acceptance_requires_pi_extension_api_markers`**
- 后者在仅有 Rust sample 时失败（缺少 `pi_extension_api` / `hello.ts` 路径）
- 需要 Node ≥22.19 与 `crates/cayw-pi-compat/npm` 的 `npm ci`
- 不把本切片标为 F15 全范围完成
