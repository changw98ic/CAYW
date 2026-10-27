# cayw-pi-compat

可选的 **Pi → CAYW** 外部兼容宿主映射（F15 首刀，SLICE S）。

**诚实范围：** 本切片**只证明**钉扎 npm 包的 `hello.ts` **工具切片**
（`tools.registerTool` / `tools.invoke`）。commands / events / `sendMessage` /
session / UI **未交付**（矩阵 `unavailable` / `incompatible`；桥接显式抛
`unsupported:`，禁止静默 noop）。**不是**完整 Pi ExtensionAPI 兼容，**不是** F15 完成。

提供：

- 按「插件@version × 表面」记录的**版本/兼容矩阵**（`CompatMatrix`）
- 表面族清单（工具 / 命令 / 会话 / 事件 / 交互 / 生命周期）→ CAYW 接缝的**记账**映射（多数仍为 unavailable）
- 可选启用的 [`PiCompatHost`]：经 `cayw-plugins::PluginRegistry` 完成装 / 调 / 卸（tools 路径）

## 首刀范围

- 协议版本：`PI_COMPAT_PROTOCOL_VERSION = 1`
- Pi ABI 钉扎：`@earendil-works/pi-coding-agent@0.84.1`（与 pi2dsh 文档基线一致）
- **三条装载路径**：
  1. 离线 fixture：`cayw-pi-fixture@0.1.0`（Native，`load_native`）
  2. 自制 CAYW 协议样例：`cayw-pi-sample-ext@0.1.0`（External Rust 子进程；**不是** Pi ExtensionAPI）
  3. **主验收路径**：未修改的 npm Pi 扩展 `@earendil-works/pi-coding-agent@0.84.1` 的 `examples/extensions/hello.ts`，经 `npm/pi-npm-bridge.mjs`（Pi ExtensionAPI 适配器）+ `load_external`
- `call_tool(package, tool, …)` 按已装载 manifest 的 capability 名调用
- 不支持的扩展接口明确记为 `incompatible` / `unavailable`，不静默成功；单次参数错误只拒绝该调用，不改写扩展兼容记录
- 调用前取消保持已知未执行；已分派调用的取消、超时或卸载保留 `OutcomeUnknown`
- Bridge 固定原 plugin 的实例、代际和完整声明；旧来源不能调用或卸载同名替换实例

## 主验收：真实 npm Pi 扩展

```sh
# Node >= 22.19（Pi 0.84.1 引擎要求）
cd crates/cayw-pi-compat/npm && npm ci --omit=dev
# 可选：export CAYW_PI_NODE=$(command -v node)
cargo test -p cayw-pi-compat --locked npm_hello_extension
```

适配器（`npm/pi-npm-bridge.mjs`）在 CAYW 侧实现最小 `ExtensionAPI`：`registerTool` 实作；
`registerCommand` / `on` / `events` / `sendMessage` / UI 等**显式抛 `unsupported:`**
（不再静默 noop）。用 jiti **原样加载**官方 `hello.ts`，再把 `execute()` 结果翻译成
CAYW external JSON。npm 包本身不做任何改写。

自制 `cayw-pi-sample-ext` **不能**单独关闭 F15：验收测试要求响应含 `pi_extension_api: true` 且 `extension_path` 指向 `hello.ts`。

## 仍不支持（明确未交付）

- `registerCommand` / `pi.on` / `events.*` / `sendMessage` / `SessionManager` / UIContext
  （commands/session/events/`sendMessage` = `unavailable`；interaction = `incompatible`；
  桥接调用即抛 `unsupported:`）
- Host/Bridge/WebUI 默认启用该可选宿主
- 将 F15 全范围或「全插件兼容」/「完整 ExtensionAPI 兼容」标为完成
- 任意 Pi 扩展的全表面自动兼容（矩阵按包@version×表面记账；已证明的仅 hello 工具切片）

## 快速使用

```rust
use cayw_ai::CancellationToken;
use cayw_pi_compat::*;
use serde_json::json;

# async fn demo() -> Result<(), PiCompatError> {
let host = PiCompatHost::new(true);

// Primary: unmodified npm Pi hello.ts
let npm = PiNpmHelloExtension::package_id();
host.install_npm_hello_extension(CancellationToken::new()).await?;
let _ = host
    .call_tool(&npm, PiNpmHelloExtension::TOOL, json!({"name": "CAYW"}), CancellationToken::new())
    .await?;
host.unload(&npm)?;

println!("{}", host.matrix_snapshot().markdown_table());
# Ok(())
# }
```

## 来源与边界

- 功能：F15（[#33](https://github.com/changw98ic/CAYW/issues/33)）
- 采用方式：设计参考 pi2dsh；**依赖真实公开 npm 包** `@earendil-works/pi-coding-agent@0.84.1`（未改写）
- 参考：[weijiafu14/pi2dsh](https://github.com/weijiafu14/pi2dsh) 的兼容层与矩阵记账形状
- 记录：[`docs/sources/f15-pi-compat-host.md`](../../docs/sources/f15-pi-compat-host.md)

## 验证

```sh
cd crates/cayw-pi-compat/npm && npm ci --omit=dev
cargo test -p cayw-pi-compat --locked
cargo clippy -p cayw-pi-compat --all-targets --locked -- -D warnings
cargo run -p cayw-pi-compat --example offline_pi_compat --locked
```
