# 参与贡献

感谢关注 [CAYW](README.md)。本仓库用 Rust workspace 开发；首发面向 macOS，CI（`.github/workflows/ci.yml`）在 `macos-latest` 上跑与 `AGENTS.md` 一致的检查。

## 构建与测试

需要 **Rust 1.95+**（含 `rustfmt`、`clippy`）。在仓库根目录：

```sh
cargo fmt --all -- --check
cargo check --workspace --locked
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
```

单个包或示例见 [AGENTS.md](AGENTS.md)。Luau（`mlua-sys`）需要本机 C++ 编译器（如 `g++` / Xcode CLI）。

真实模型示例 `cargo run -p cayw-agent --example magpie --locked` 需当次授权与环境变量，默认不要跑。

## 命名

- 产品名：**VerifyTool**（大小写固定，不要写成 `verifyTool`）。
- Rust crate 名保持：`cayw-verify-tool`。
- StrongFlow / JEv / Fusion / Observer 及第三方扩展统称 **plugin**（Native / External），包名 `cayw-plugins`。

## Pull Request 期望

- 改动组件边界或执行语义前，先读 [docs/architecture.md](docs/architecture.md) 与 [AGENTS.md](AGENTS.md)。
- 交付以可复跑验收为准；不要把局部测试写成全平台或全 Issue 通过。
- 路线图与依赖图同步见 [docs/development-map.md](docs/development-map.md)；Issue 范围以 GitHub Issue 为准。
- 不要为占位创建空 crate；不要引入 Codex；许可证为 Apache-2.0。
- PR 描述写清改了什么、如何验证；CI 应通过上述 fmt/check/test/clippy。

更完整的架构约束、证据规则与开发命令见 **[AGENTS.md](AGENTS.md)**。
