# Rust pi 内核验收记录

本文保留 2026-10-18 的历史冻结结果与当时缺口；其源码摘要不用于校验当前工作区。当前实现、交付和 GitHub 状态见 [项目说明](../../README.md) 与 [Issue #1 验收](issue1-sdk-ipc.md)。

日期：2026-10-18。范围：`cayw-ai` / `cayw-agent` 第一版内存内核的离线基线，参考 pi 1.3.0。结果：macOS ARM64 与 Linux ARM64 的 58 项测试全部通过，离线示例均完成。本记录未运行真实模型请求；随后按用户授权完成的 Magpie + DS Flash 测试单独记录在 [真实模型验收](rust-pi-core-magpie.md)。Windows 仍未验证。

## 源码与配置

- 分支：`pi-core-rust`，基于 `9e7dc502fdf771bb7a6e1111bc9f9c6dc3787c21`，本次代码尚未提交。
- 23 个构建输入的 SHA-256：[`rust-pi-core-sources.sha256`](rust-pi-core-sources.sha256)。清单文件 SHA-256 为 `c19915fcb090603cedbe34065379a4f93eec37f4eb1a8e0d5afd67bb6afbf524`。包括两个 manifest、源码/测试/示例及根 Cargo.toml、Cargo.lock。
- Cargo.lock SHA-256：`5f31876cd2e8048e604ec1e2583f57197f55302eb775bc58fdb483a847c94e40`。
- Rust/Cargo 1.95.0，Rust edition 2024；`cargo --locked` 固定实际依赖解析。
- 核心依赖：Tokio 1.53.3、tokio-util 0.7.20、futures 0.3.34、reqwest 0.13.5、jsonschema 0.59.0、serde 1.0.229、serde_json 1.0.154。
- jsonschema 关闭默认外部解析 feature，工具声明中的外部引用另有拒绝检查。Reqwest 默认客户端禁用重试和跳转。
- 使用固定的离线模型数据和 loopback HTTP 服务；没有读取本机认证文件、没有真实模型请求、没有计费调用。下载依赖和容器镜像不属于模型测试。

在根目录可用 `shasum -a 256 -c docs/validation/rust-pi-core-sources.sha256`（macOS）或 `sha256sum -c ...`（Linux）核对源码。

## 平台与命令

| 平台 | 环境 | 结果 |
| --- | --- | --- |
| macOS | Darwin 27.0.0 / arm64，`aarch64-apple-darwin`，rustc 1.95.0 | 58 测试、fmt、check、Clippy、示例全部通过 |
| Linux | Docker 内的 Linux aarch64，`aarch64-unknown-linux-gnu`，rustc/Cargo 1.95.0 | 58 测试和示例通过；未单独运行 Linux Clippy/fmt |
| Windows | 没有可用运行环境 | 未验证，包括编译、运行和取消行为 |

本机默认 stable 工具链没有 Clippy，但已安装的 `1.95.0` 工具链有，实际使用：

```sh
cargo +1.95.0 fmt --all -- --check
cargo +1.95.0 check --workspace --locked
cargo +1.95.0 test --workspace --locked
cargo +1.95.0 clippy --workspace --all-targets --locked -- -D warnings
cargo +1.95.0 run -p cayw-agent --example offline --locked
```

均退出 0。原始输出：[`rust-pi-core-macos-tests.log`](rust-pi-core-macos-tests.log)、[`rust-pi-core-macos-build-example.log`](rust-pi-core-macos-build-example.log)。前者包含最后一次完整测试和 Clippy；格式检查无输出，通过状态包含在后者的组合命令退出码中。

Linux 使用 `rust:1.95-slim`，固定镜像 digest：`sha256:e14e87345b4d5964ddcc3491d27ee046a0f23820f340c3c1e24da6880141f7c0`。使用稳定源码副本，而非在编译期间读取正在格式化的挂载文件：

```sh
tar -cf - Cargo.toml Cargo.lock crates | docker run --rm -i \
  --mount type=volume,source=cayw-pi-rust195-target,target=/build \
  --mount type=volume,source=cayw-pi-rust195-registry,target=/usr/local/cargo/registry \
  --env CARGO_TARGET_DIR=/build \
  rust@sha256:e14e87345b4d5964ddcc3491d27ee046a0f23820f340c3c1e24da6880141f7c0 \
  sh -c 'mkdir -p /work && tar -xf - -C /work && cd /work && cargo test --workspace --locked && cargo run -p cayw-agent --example offline --locked'
```

退出 0，原始输出：[`rust-pi-core-linux-tests-example.log`](rust-pi-core-linux-tests-example.log)。tar 对 macOS 扩展属性的提示不影响 Rust 文件内容或测试。环境原始输出：[`rust-pi-core-environments.log`](rust-pi-core-environments.log)。

## 测试分类

| 位置 | 数量 | 验证内容 |
| --- | ---: | --- |
| `cayw-ai` SSE 单测 | 2 | UTF-8 任意分片、不同换行、多行 data、尾事件、无效编码、事件大小上限 |
| `cayw-ai/tests/contracts.rs` | 4 | system/工具声明重放、严格 JSON 对象、类型校验与外部引用拒绝、序列化和用量 |
| `cayw-ai/tests/openai.rs` | 18 | 实际请求、文本/工具增量、逆序调用索引、缓存/思考用量、响应身份、HTTP/协议错误、取消与丢弃连接 |
| `cayw-agent` 工具执行单测 | 10 | 顺序/并行、完成顺序与结果顺序、校验/阻止/截断、panic、进度结算、取消与未知结果 |
| `cayw-agent/tests/agent.rs` | 13 | 多轮循环、队列、continue、等待订阅器、丢弃 Future、轮次上限、重复工具编号与错误硬退出 |
| `cayw-agent/tests/hooks.rs` | 6 | 请求/工具/轮次回调、替换结果、阻止工具、取消、队列模式和延迟插入消息 |
| `cayw-agent/tests/http_loop.rs` | 1 | 本地 HTTP → 分片流 → Rust echo 工具 → 配对结果的后续请求 → 回答 |
| `cayw-agent/tests/review_regressions.rs` | 4 | 未知副作用停止当前运行/新请求/reset，以及回调失败/取消/丢弃时保留排队输入 |
| 合计 | 58 | 两个平台均 0 失败；没有 doctest 用例 |

离线示例的实际输出：

```text
tool start: add {"a":2,"b":3}
tool result: 5
assistant: 2 + 3 = 5
completed: 5 messages, running=false
```

## 过程中发现并修正的问题

- 首次 Agent 编译出现异步准备阶段借用存活过久；缩短 Future 作用域后编译通过。
- Clippy 指出事件枚举中的大字段；将 MessageUpdate 的流事件放入 Box 后通过。
- 复查确认未知工具结果只阻止下一次运行、没有阻止同一次运行的后续请求；新增当前运行保护和回归测试。
- 复查确认下一轮回调失败会丢失已取出的排队消息；消息在提交历史前由有作用域的保护持有，失败/丢弃时放回队列。
- 新输入中的未知结果也必须在 Provider 请求前阻止；已补入口回归。
- 模型工具增量逆序到达、缓存详情晚到和响应身份变化均补测试后修正。
- 前两次 Linux 验证分别读到已移除的临时测试文件与格式化期间的半写入文件；两次都明确失败，没有计为通过。最终用稳定源码副本完成验收。

这份记录不声称所有代码都采用测试先行；基础循环部分首轮测试在实现后运行，复查发现的问题有先失败再修复的回归记录。

## 许可证核对与验收缺口

Cargo metadata 的全部解析包均有许可证表达式，无缺失项。直接依赖均为 MIT 或 Apache-2.0 兼容的宽松许可；传递依赖主要还包括 BSD、ISC、Unicode-3.0、CDLA-Permissive-2.0、Zlib 等宽松许可。含 LGPL 选项的包同时提供 MIT/Apache-2.0 选择；不选择 LGPL 路径。此为清单层面的兼容性核对，不替代将来分发二进制时汇总实际第三方 NOTICE 的工作。pi 的完整 MIT 通知保留在 `third-party/pi-MIT.txt`。

这份离线基线不覆盖真实模型服务与费用；之后的 Magpie + DS Flash 有限真实测试见 [独立记录](rust-pi-core-magpie.md)。仍未验证或未实现：Windows、Anthropic/Gemini/OAuth、完整 pi 应用、Luau CodeMode、磁盘日志、Session/Worker 恢复、版本化 IPC、审批持久化、执行围栏、进程树和资源治理。因此 #1/#6 仍不能结项，性能与模型质量也没有结论。

GitHub #1 的旧领取评论尚未同步：替换操作被自动审批拒绝，理由是需要替换该评论的明确授权。当时的更正稿仅保存在本地，未作为正式交付发布。没有关闭 Issue、勾选验收项或推送源码。
