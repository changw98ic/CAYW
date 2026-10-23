# CAYW Fusion External plugin

`cayw-fusion-plugin` 是可选的 External plugin：独立子进程按 [External 协议 v1](../cayw-plugins/README.md#external-协议-v1) 接入宿主，对外只提供一个能力 `fusion.collect`。它不是主链依赖：`cayw-runtime` 默认不开启 `plugins` feature，任何默认构建的 crate 都不依赖本包，普通 Chat 不需要它。参考设计见 [#17](https://github.com/changw98ic/CAYW/issues/17)。

进程只做协议与 provider 配置适配。Fusion 语义直接复用 `cayw_plugins::fusion::collect_fusion`，不另写一份：3–16 个不同 provider ID、相同输入并发盲答、不传递其他候选的回答或身份、cayw-ai 严格 schema 校验、按排序 key 的 JSON 计算 SHA-256 摘要、`disagreements` 只列分歧不投票。独立进程不是安全沙箱。

## 启动与 manifest

```sh
cargo build -p cayw-fusion-plugin --locked
target/debug/cayw-fusion-plugin --print-manifest
```

宿主用绝对路径、空 argv 启动，环境由 `ExternalConfig` 清空。`ExternalConfig::expected_manifest` 使用库函数 `cayw_fusion_plugin::manifest()`（与 `--print-manifest` 输出一致），握手时逐字段比对。可选参数：

- `--max-line-bytes N`：单条响应行上限，默认 65536，应与宿主 `ExternalConfig::max_line_bytes` 相同。报告超限时返回已知错误，进程继续服务。请求行另有 16 MiB 内存上限，宿主自身的行上限仍然先生效。
- `--allow-fault-injection`：开启离线 `panic` / `crash` 两种故障注入，仅用于测试，生产配置不要传。

## `fusion.collect`

参数（manifest 中的 schema 由宿主先校验，进程内再以 `deny_unknown_fields` 严格解析）：

```json
{
  "input": {"question": "...", "canonical_context": {}, "constraints": ["..."], "answer_schema": {"type": "object"}},
  "providers": [
    {"provider_id": "p0", "kind": "offline", "behavior": {"mode": "answer", "value": {"n": 1}}},
    {"provider_id": "p1", "kind": "offline", "behavior": {"mode": "fail", "message": "..."}},
    {"provider_id": "p2", "kind": "offline", "behavior": {"mode": "hang"}}
  ],
  "per_provider_timeout_ms": 20000
}
```

成功时 `data` 是序列化的 `FusionReport`：`input`、`input_digest`、按 `providers` 顺序排列的 `candidates`（每条带 `provider_id`、`input_digest` 和 `outcome`：`answer` / `invalid_answer` / `failed` / `timed_out` / `cancelled`）以及 `disagreements`（provider ID 对）。

- 每个 provider 独立受 `per_provider_timeout_ms`（1–600000）约束。宿主的 `call_timeout` 必须大于它，否则宿主按协议终止进程。
- provider 超时、失败、回答不合 schema 或 panic 都只记入对应候选，不使整次调用或进程失败。panic 隔离在独立 task 中，记为 `failed`。
- 未知方法、参数无效、provider ID 重复、未知 `kind`、未开启故障注入却请求 `panic` / `crash` 等，返回 `status: error`、`outcome_unknown: false`，进程继续服务。无法解析且没有 `request_id` 的行只写 stderr，不回复。
- 协议 v1 没有取消消息。宿主取消、超时或卸载时按协议终止整个进程组；宿主关闭 stdin 时，进程取消正在进行的收集并退出，不写响应。请求顺序处理。

## Provider 扩展点

当前只提供 `kind: "offline"`：确定性本地行为（`answer` / `fail` / `hang`，以及需显式开启的 `panic` / `crash`），不访问网络，用于 fixture 与测试。

真实模型 provider 尚未接入：现有 HTTP provider 需要凭据，而本进程默认不继承环境，也不应自行读取本机密钥。接入时在 `ProviderSpec` 增加一个 `kind` 变体，在 `ProviderSpec::build` 中构造实现 `FusionCandidateSource` 的来源（例如包装 `cayw_ai::Provider`），凭据经宿主显式传入的 `ExternalConfig::environment` 提供，并确保 usage 和失败都按实际返回记录。provider 的独立性与成本预算仍由宿主负责。

## 可复跑验证

```sh
cargo test -p cayw-fusion-plugin --locked
cargo clippy -p cayw-fusion-plugin --all-targets --all-features --locked -- -D warnings
```

- `tests/protocol.rs`：通过 `PluginRegistry::load_external` 启动真实子进程，比对进程报告与进程内 `collect_fusion` 完全一致；超时、panic、无效调用、报告超限后进程仍可继续调用；直接驱动 stdin/stdout 验证错误响应；关闭输入取消进行中的收集；错误协议版本被拒绝。
- `tests/runtime_host.rs`：`cayw-runtime`（`plugins` feature）加载并绑定本进程，经 CodeMode 调用 `fusion.collect` 并核对报告；能力名不与宿主目录冲突、同名重复加载被拒；卸载后已绑定 Session 冻结、普通 Chat 照常完成；调用中途进程崩溃记为 `outcome_unknown` 且 Session 冻结，宿主与其他普通 Session 不受影响；死实例可卸载并重新加载、绑定新进程，但原未知结果核对前，宿主的 owner-recovery 守卫继续拒绝同一工作区内新的 plugin 调用，重载不能绕过该守卫。

所有测试只用离线 provider，不访问网络、不需要凭据。`runtime_host.rs` 经 Luau 执行能力，属于仓库现有的 macOS 范围；Linux 上 Runtime 的能力执行路径本身存在已知 EBADF 失败（与本包无关，见 [#19](https://github.com/changw98ic/CAYW/issues/19)），未在 Linux 验证。
