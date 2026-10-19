# Magpie + DS Flash 真实模型验收

本文保留 2026-10-18 原内核的历史验收结果和当时缺口。当前工作区范围见 [项目说明](../../README.md) 与 [Issue #1 验收](issue1-sdk-ipc.md)，不能将本记录中的“尚未实现”或“未提交”当作当前状态。

2026-10-18，按用户“接上 magpie 拿 ds flash 测试”的明确授权，用当前 Rust 内核发送真实模型请求。4 项检查全部通过，共 5 次模型请求；程序退出 0。只发送合成提示和进程内生成的测试值，没有发送仓库内容或读取其他 Agent 的认证文件。

## 接入与源码

- 本机 Magpie：`0.1.1285`，通过 `GET /api/hello` 确认；`GET /v1/models` 返回 111 个目录项。
- 接口：`http://127.0.0.1:3425/v1/chat/completions`。
- 请求目录 ID：`group/auto-deepseek-v4-1-flash`。
- 5 次响应中的模型名均为 `deepseek-v4.1-flash`。这是网关返回的名称；未独立核对网关背后的供应商路由。
- 实际调用：`cayw-ai::providers::OpenAiProvider` → `cayw-agent::Agent` → 显式注册的 Rust `read_probe` 工具。文本检查直接消费同一个 Provider 的流接口。
- 本次只新增 [`examples/magpie.rs`](../../crates/cayw-agent/examples/magpie.rs) 作为可复跑入口，没有修改内核协议或新增依赖。原离线基线的 23 个构建输入指纹仍全部相符。
- 当前分支 `pi-core-rust`，HEAD 为 `9e7dc502fdf771bb7a6e1111bc9f9c6dc3787c21`；Rust 实现仍在未提交工作区。
- 本次 24 个构建输入的 [SHA-256 清单](rust-pi-core-magpie-sources.sha256)，清单 SHA-256：`82321b5efe9758238edd850ff4cf22d1a488604238548f74ebbeac4c2790679b`。
- 本机执行文件 `target/debug/examples/magpie` 的 SHA-256：`d0b452ada9895127a01f79ace8f1504fee13afa88bddf4b3931379d12aeded4d`。执行文件未纳入仓库，不要求其他机器可复现相同二进制散列。

## 输入、限制与权限

本次访问本机网关使用显式 `MAGPIE_API_KEY` 环境变量。源码和验收日志不保存凭据值。没有更改 Magpie 配置、模型目录或远端仓库。

所有请求设置 `thinking_level = Low`。直接文本检查和取消检查的 `max_tokens = 1024`；工具循环和后续对话为 `2048`。直接流检查和每次 Agent prompt 各有 90 秒超时；工具/后续 prompt 每次最多 3 个模型轮次，取消 prompt 最多 1 轮。失败直接退出，没有示例自身的自动重试。Magpie 的上游重试、路由和计费策略未独立验证。

`read_probe` 工具只接受 `{"key":"cayw-rust"}`，读取 Rust 进程内生成的合成值。没有文件读写、Shell、网络或持久化副作用。预期值不会放入模型提示，只有真实执行工具才会返回给模型。

## 实际结果

| 检查 | 实际输入与断言 | 结果 |
| --- | --- | --- |
| 流式文本 | 要求只回答 `CAYW_MAGPIE_STREAM_OK`；收到 5 个非空文本增量、12 个思考增量和唯一终态，最终文本精确匹配 | 通过 |
| 模型 → Rust 工具 → 模型 | 提示调用一次 `read_probe`；Rust 执行计数为 1；调用/结果 ID 与工具名匹配；结果无错误或未知标记；模型最终精确复述 `CAYW_PROBE_1792329739711_96543` | 通过 |
| 多轮上下文 | 撤下工具后，要求只复述上一轮工具值；模型再次精确匹配，工具执行次数仍为 1 | 通过 |
| 中途取消 | 要求输出 1–200；收到首个非空文本增量 `1` 时调用 `AgentControl::abort()`；终态为 `Aborted`，Agent 空闲，无流式消息、未结工具或待核对标记 | 通过 |

真实执行时间为 UTC `2026-10-18T13:22:16.826` 至 `13:22:25.496`，合计约 8.67 秒。这是一次小样本记录，不是性能基准或延迟承诺。

[原始运行日志](rust-pi-core-magpie-live.log) 保留提示、工具参数/结果、模型名、响应 ID、终态、用量与断言结论；不记录思考正文。日志 SHA-256：`996f5b7408e04257a99f1fd4f516553532a23f17ab343e231292fe2e6d4622e7`。

### 模型报告的用量

| 请求 | 普通输入 | 缓存读取 | 输出（含思考） | 其中思考 | 总 token |
| --- | ---: | ---: | ---: | ---: | ---: |
| 文本 | 46 | 0 | 48 | 38 | 94 |
| 工具调用 | 348 | 0 | 43 | 0 | 391 |
| 工具结果后的回答 | 161 | 256 | 14 | 0 | 431 |
| 后续对话 | 165 | 0 | 56 | 42 | 221 |
| 已完成请求合计 | 720 | 256 | 161 | 80 | 1137 |
| 中途取消 | 未收到完整用量 | 未收到完整用量 | 未收到完整用量 | 未收到完整用量 | 未知 |

思考是输出的子集，不重复加入总数；缓存写入报告为 0。取消响应在本地结构中的用量为 0，表示没有收到完整用量，不能解释成零消耗或零费用。未核对供应商账单；也未证明上游服务在本地取消后立即停止生成或计费。

## 复跑

需要正在运行的 Magpie、目录中可用的模型路由，以及当次真实模型调用授权。先在环境中设置 `MAGPIE_API_KEY`，不要把真实凭据写入源码或验收文件。

在仓库根目录执行：

```sh
MAGPIE_BASE_URL=http://127.0.0.1:3425/v1 \
MAGPIE_MODEL=group/auto-deepseek-v4-1-flash \
cargo +1.95.0 run -p cayw-agent --example magpie --locked
```

`MAGPIE_BASE_URL` 和 `MAGPIE_MODEL` 的默认值就是上述配置；`MAGPIE_API_KEY` 必须显式设置。普通 `cargo test` 不运行此真实请求示例。示例对精确回答、工具执行次数和状态进行断言；模型输出不满足要求时退出非零，不能只根据 HTTP 成功判定通过。

本次实际使用的命令为 `MAGPIE_API_KEY=<本机网关凭据> cargo +1.95.0 run -p cayw-agent --example magpie --locked`，输出经 `tee` 保存；shell 启用 `pipefail`，不会把示例失败隐藏成日志写入成功。

核对本次源码：

```sh
shasum -a 256 -c docs/validation/rust-pi-core-magpie-sources.sha256
```

Linux 可用 `sha256sum -c` 核对同一清单。

## 平台与回归

| 平台 | 本次结果 |
| --- | --- |
| macOS ARM64 | 4 项真实检查、58 项离线/本地 HTTP 测试、fmt、全目标 check、全目标 Clippy 全部通过 |
| Linux ARM64 | 新示例 `cargo check --workspace --examples --locked` 通过；本次未连接真实模型。内核原基线的 58 项测试与离线示例已通过，见独立记录 |
| Windows | 未验证 |

macOS 的实际环境为 macOS `27.0.1` / arm64（Darwin `27.0.0`），rustc `1.95.0 (59807616e 2026-04-14)`，Cargo `1.95.0 (f2d3ce0bd 2026-03-21)`，目标 `aarch64-apple-darwin`。

本次 macOS 检查命令均退出 0：

```sh
cargo +1.95.0 fmt --all -- --check
cargo +1.95.0 check --workspace --all-targets --locked
cargo +1.95.0 test --workspace --locked
cargo +1.95.0 clippy --workspace --all-targets --locked -- -D warnings
```

[回归与 Clippy 输出](rust-pi-core-magpie-checks.log) 包含 58 项测试及 Clippy；fmt 和全目标 check 的退出 0 已由执行结果确认，不在该日志中重复保存。

Linux 使用稳定源码副本与固定镜像，避免读取正在编辑的文件：

```sh
tar -cf - Cargo.toml Cargo.lock crates | docker run --rm -i \
  --mount type=volume,source=cayw-pi-rust195-target,target=/build \
  --mount type=volume,source=cayw-pi-rust195-registry,target=/usr/local/cargo/registry \
  --env CARGO_TARGET_DIR=/build \
  rust@sha256:e14e87345b4d5964ddcc3491d27ee046a0f23820f340c3c1e24da6880141f7c0 \
  sh -c 'mkdir -p /work && tar -xf - -C /work && cd /work && cargo check --workspace --examples --locked'
```

退出 0，[原始输出](rust-pi-core-magpie-linux-check.log) 中的 macOS 扩展属性提示不影响 Rust 文件或编译结果。

## 仍未覆盖

这是 Magpie + 指定 DS Flash 路由的一次有限真实测试。未覆盖其他 Provider、所有思考档位、图片、长任务、真实编程任务质量、故障恢复、上游计费/取消、Linux/Windows 真实请求。工具是合成能力，不是文件或进程治理能力。

Luau CodeMode、统一权限/预算、持久化会话、版本化 IPC 和 Device 仍未实现；#1/#6 的完整交付保持未完成。独立的 [离线基线](rust-pi-core.md) 不会因本次真实测试而被改称真实模型证据。代码未提交或推送。

接入文档通过 Context7 核对：[Magpie integration](https://github.com/yetone/magpie/blob/main/docs/integrating.md) 与 [gateway routing](https://github.com/yetone/magpie/blob/main/docs/subsystems/gateway-routing.md)。
