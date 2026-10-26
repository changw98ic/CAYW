# F15 / SLICE S 验收说明（Pi 兼容宿主）

日期：2026-10-25（npm ExtensionAPI 路径补强：2026-10-26）

## 命令

```sh
cd crates/cayw-pi-compat/npm && npm ci --omit=dev
cargo test -p cayw-pi-compat --locked
cargo clippy -p cayw-pi-compat --all-targets --locked -- -D warnings
cargo run -p cayw-pi-compat --example offline_pi_compat --locked
```

需要 **Node.js ≥ 22.19**（可用 `CAYW_PI_NODE` 指定二进制）。

## 主验收

| 测试 | 断言 |
| --- | --- |
| `npm_hello_extension_install_call_unload` | 装载未修改 `hello.ts`，调用 `hello`，响应含 `pi_extension_api`、`protocol=pi-ExtensionAPI`、`extension_path` 以 `hello.ts` 结尾、`Hello, CAYW!` |
| `acceptance_requires_pi_extension_api_markers` | Rust sample 无 Pi 标记；npm 路径必须成功且带标记（禁止仅用 sample 关闭项） |

## 诚实缺口

- commands / session / events 仍 unavailable；interaction incompatible
- 适配器 UI/exec/session 为 stub
- 未声明「全插件兼容」或 F15 全范围完成
