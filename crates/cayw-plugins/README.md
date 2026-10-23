# CAYW Plugins

`cayw-plugins` 是可独立运行的宿主 plugin 库，依赖 `cayw-ai` 的工具声明、严格参数校验与取消令牌。它没有接入尚未稳定的 Core、Device 或 CodeMode，也不创建模型连接。

## plugin 注册

- `PluginManifest` 声明名称、版本、协议版本、Native / External 类型、工具参数和逐工具权限。`PluginContext::new` 的能力与权限只能由宿主提供，默认不授权。
- `Plugin` 是可信 Rust 实现的异步调用接口。`PluginRegistry::load_native`、`load_external`、`describe`、`list`、`call`、`unload` 提供实际注册和执行。plugin 名称和工具名称均拒绝冲突；参数不做隐式转换；schema 外部引用不可读取文件或访问网络。
- 每次调用取得 plugin 代际快照；卸载取消旧代际中的请求，返回结果前再次检查代际。注册表不持锁等待 plugin 代码，不同 plugin 可以并行运行。同一个 External plugin 顺序处理协议请求。
- plugin 返回的 JSON 仅是数据，不会变成权限或用户审批。宿主必须保持这条边界；恶意 Native Rust 代码和 External 进程本身都不能靠此库获得安全隔离。
- 取消、超时、卸载、传输失败或 plugin 明确报告未知结果时，`PluginError::outcome_unknown()` 为真。终止等待或杀进程不能证明副作用已回滚，宿主必须核对再重试。原生 plugin 应主动响应取消，不能在异步函数中阻塞线程或使 `shutdown` 阻塞。

## External 协议 v1

`ExternalConfig` 使用绝对 executable、独立 argv、宿主显式环境变量和可选绝对工作目录；不经 shell，默认清空继承环境。宿主声明完整预期 manifest，plugin 握手必须逐字段一致。握手和调用都有时限，协议行和 stdout + stderr 生命周期累计输出均有字节上限。stderr 仅计数并丢弃，不存储可能含凭据的日志。

每条消息是一行 JSON，以 LF 结束。请求编号从 1 递增，响应必须匹配当前请求，未知 envelope 字段和主动输出都导致终止。示例：

```json
{"type":"handshake","protocol_version":1}
{"type":"handshake","manifest":{"name":"fixture","version":"1.0.0","protocol_version":1,"kind":"external","capabilities":[{"name":"fixture.echo","description":"Echo offline JSON","parameters":{"type":"object"}}],"permissions":{}}}
{"type":"call","request_id":1,"method":"fixture.echo","arguments":{"n":2}}
{"type":"response","request_id":1,"result":{"status":"ok","data":{"n":2}}}
{"type":"response","request_id":2,"result":{"status":"error","message":"known failure","outcome_unknown":false}}
```

一个请求恰好对应一个响应。取消、超时、格式错误、错配、输出超限、未知副作用或卸载均使进程终结并拒绝继续调用；普通已知失败可以继续。丢弃正在等待的 Future 也传播取消。Unix 新建进程组，正常和异常释放都向整个组发送 SIGKILL，随后回收直接子进程；主动脱离该进程组的后代不受此机制保证。Windows 只保证直接子进程终止，尚未实现 Job Object 进程树隔离。独立进程不是安全沙箱。

`cayw-plugin-fixture` 是测试用真实 Rust 子进程，可模拟握手、往返、卡住、崩溃、错编号、超大输出、伪审批字段和 Unix 后代进程，不连接网络。

## StrongFlow

默认 Cargo feature `strongflow` 提供 `StrongFlow`：宿主调用 `create_plan` 创建含依赖和验收检查项的计划，循环、重复 ID 和缺失依赖被拒绝。`ready` 只返回前置步骤全部通过的待执行项，`start_step` 检查状态后开始。`submit_evidence` 只在宿主 API 上提供；它要求 source SHA-256、构建 ID、配置、平台、输入、执行命令、实际结果和已执行检查项，覆盖全部声明项才通过，执行失败使步骤失败。同一步的证据必须对应同一源码、构建和配置。宿主负责核实证据真实性，模型说“通过”不能经 plugin 工具入口提交验收。

plugin 工具仅暴露 `strongflow.describe` 与 `strongflow.ready`。`WorkflowSnapshot` 保留完整计划、状态和证据，可序列化。

`SessionBinding` 独立记录 Session ID、普通 / StrongFlow 模式、绑定 plugin 版本、注册表实例 UUID 和代际、冻结原因及来源 Session。新注册表即使重用同一个代际数字也不能恢复旧绑定。Runtime 必须在恢复、每次使用和卸载通知后调用 `check_available`；不匹配会冻结，重新加载相同版本也不能解冻。`request_ordinary_fork(new_id, user_confirmed)` 只在明确确认且 ID 不同时返回 `ForkDecision`，保留原绑定和来源，不会直接转换原 Session。Runtime 仍需核对未知副作用、原子预留新 ID、复制持久历史并保存真实 Fork；本库没有声称已完成 Core 集成。

## Observer

默认 feature `observer` 提供 `Observer::channel`。宿主同步调用 `publish`，内部只用 `try_send`；慢读者队列满时丢弃并计数，读者关闭或失败不阻塞主执行。事件数量与单条脱敏后体积均有上限，过大或无效事件有独立计数。`ObservationReader` 单独消费或关闭通道。plugin 只提供 `observer.stats`，卸载后停止接受发布。

每个字段必须标记 Public / Private / Thinking / Credential。默认仅公开 Public 字段，递归删除常见凭据和思考字段；Credential 永不输出。可以明确启用私有内容或思考，并增加脱敏 key。脱敏不能从任意自由文本推断秘密，宿主必须正确标记字段；事件名称和字段名称只允许短标识符。

## JEv 与 Fusion

默认 feature `evaluation` 提供真实离线适配和收集算法，不提供假模型分数，不运行付费调用。

- `JevEvaluator` 由宿主明确注册；`PluginJevEvaluator` 可连接已授权的 Native / External 工具。没有评估器返回 Unavailable；概率要求有限、介于 0–1 且总和为 1。`JevReport` 永远是 provisional，不能替代 StrongFlow 执行证据。
- `FusionCandidateSource` 接收相同 `FusionInput`，包含 question、canonical context、constraints、answer schema。`collect_fusion` 要求 3–16 个不同 provider ID，并发收集盲答；不会把其他回答传给候选源。宿主负责 provider 身份和独立性。
- 输入按递归排序 JSON object key 的确定性编码计算 SHA-256。它是本库定义的摘要格式，不宣称实现 RFC 8785 的所有数值规范。每条候选保留相同摘要、原回答或失败、可用的 usage，以及严格 schema 验证失败、取消或超时。无法获得的用量记为未知。
- `FusionReport.disagreements` 只列出不同的有效答案供后续调查，不投票认定事实。冲突调查、真实模型服务、成本预算和高层产品接入未包含在本库。
- 可选的 External 形态见 [`cayw-fusion-plugin`](../cayw-fusion-plugin/README.md)：独立进程按协议 v1 提供 `fusion.collect`，内部直接调用 `collect_fusion`，语义不变。它不是本库或 Runtime 默认构建的一部分。

## 可复跑验证

在仓库根目录运行（仅检查此 crate，不格式化并行工作的其他 crate）：

```sh
/Users/chengwen/.cargo/bin/cargo +1.95.0 test -p cayw-plugins --locked
/Users/chengwen/.cargo/bin/cargo +1.95.0 test -p cayw-plugins --no-default-features --locked
/Users/chengwen/.cargo/bin/cargo +1.95.0 clippy -p cayw-plugins --all-targets --locked -- -D warnings
/Users/chengwen/.cargo/bin/cargo +1.95.0 run -p cayw-plugins --example offline_plugins --locked
```

测试都使用本地 fixture 或宿主回调，没有真实 Provider、凭据或网络访问。开发机为 macOS；Windows / Linux 行为在本机未验证，不能据此标为三平台验收完成。新增直接依赖沿用 Apache-2.0 / MIT 兼容依赖；Unix `nix` 为 MIT，`sha2` 与 `uuid` 为 MIT OR Apache-2.0，Tokio 为 MIT。
