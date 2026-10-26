# cayw-pi-compat

可选的 **Pi → CAYW** 外部兼容宿主映射（F15 首刀，SLICE S）。

提供：

- 按「插件@version × 表面」记录的**版本/兼容矩阵**（`CompatMatrix`）
- Pi 公共扩展表面族（工具 / 命令 / 会话 / 事件 / 交互 / 生命周期）到 CAYW 接缝的首刀映射
- 可选启用的 [`PiCompatHost`]：经 `cayw-plugins::PluginRegistry` 完成装 / 调 / 卸

## 首刀范围

- 协议版本：`PI_COMPAT_PROTOCOL_VERSION = 1`
- Pi ABI 钉扎：`@earendil-works/pi-coding-agent@0.84.1`（与 pi2dsh 文档基线一致）
- **两条装载路径**（fixture 保留给 CI）：
  1. 离线 fixture：`cayw-pi-fixture@0.1.0`（Native，`load_native`）
  2. **磁盘/子进程样例**：`cayw-pi-sample-ext@0.1.0`（External，`load_external`；二进制存在时装→调→卸）
- `call_tool(package, tool, …)` 按已装载 manifest 的 capability 名调用，不再写死 fixture 工具
- 拒绝路径一律记为 `incompatible` / `unavailable`，**禁止静默成功**
- `Cancelled` / `Timeout` 保持为已知终态（不抬升为 `OutcomeUnknown`）

## 仍不支持（明确未交付）

- **未修改的 npm Pi ExtensionAPI 包**直接加载 / Node ABI 桥（矩阵中钉扎包各表面 = `unavailable`）
- 完整事件总线、`SessionManager`、`registerCommand`、UIContext（commands/session/events = `unavailable`；interaction = `incompatible`）
- Host/Bridge/WebUI 默认启用该可选宿主
- 将 F15 全范围或「全插件兼容」标为完成

样例扩展只证明 CAYW external JSON 协议子进程路径；它**不是** npm `@earendil-works/pi-coding-agent` 扩展运行时。

## 快速使用

```rust
use cayw_ai::CancellationToken;
use cayw_pi_compat::*;
use serde_json::json;
use std::path::PathBuf;

# async fn demo() -> Result<(), PiCompatError> {
let host = PiCompatHost::new(true); // false = 宿主关闭，全部调用 Disabled

// CI / offline fixture
let package = PiFixturePlugin::package_id();
host.install_fixture()?;
let _ = host
    .call_tool(&package, PiFixturePlugin::TOOL, json!({"text": "hi"}), CancellationToken::new())
    .await?;
host.unload(&package)?;

// Real path beyond fixture: sample external binary when present on disk
let sample = PiSampleExtension::package_id();
let exe = PathBuf::from("target/debug/cayw-pi-sample-ext");
if exe.is_file() {
    host.install_sample_extension(&exe, CancellationToken::new()).await?;
    let _ = host
        .call_tool(&sample, PiSampleExtension::TOOL, json!({"note": "disk"}), CancellationToken::new())
        .await?;
    host.unload(&sample)?;
}

println!("{}", host.matrix_snapshot().markdown_table());
# Ok(())
# }
```

## 来源与边界

- 功能：F15（[#33](https://github.com/changw98ic/CAYW/issues/33)）
- 采用方式：设计参考（非代码移植、非 Node/pi2dsh 运行时依赖）
- 参考：[weijiafu14/pi2dsh](https://github.com/weijiafu14/pi2dsh) 的兼容层与矩阵记账形状
- 记录：[`docs/sources/f15-pi-compat-host.md`](../../docs/sources/f15-pi-compat-host.md)

## 验证

```sh
cargo test -p cayw-pi-compat --locked
cargo clippy -p cayw-pi-compat --all-targets --locked -- -D warnings
cargo run -p cayw-pi-compat --example offline_pi_compat --locked
```

默认路径无外网、无 npm 安装。样例扩展测试依赖同 crate 的 `cayw-pi-sample-ext` 二进制（由 `cargo test` 自动构建）。
