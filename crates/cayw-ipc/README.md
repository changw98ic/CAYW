# cayw-ipc

CAYW 的版本化本机通信库，Apache-2.0。只负责认证、请求/响应配对、取消和连接资源管理；实际 Session、权限和事件持久化由宿主处理。

客户端和服务端都只接受 loopback 地址，包括 `127.0.0.1` 和 `::1`；绑定 `0.0.0.0` / `::` 的 listener 会被拒绝。远程通信必须经过独立远程模块，不能将这里的裸 TCP 转发当作带 TLS 的远程通道。宿主提供非空 token；库不读取凭据，不记录 token，`ServerConfig` 的 Debug 隐去 token。token 只用于客户端向服务端认证，不能替代系统隔离或服务端身份认证。

## 宿主接入 API

```rust
pub const PROTOCOL_VERSION: u32 = 1;

#[async_trait::async_trait]
pub trait RpcHandler: Send + Sync + 'static {
    async fn call(
        &self,
        method: &str,
        params: serde_json::Value,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<serde_json::Value, RpcError>;
}

// 可序列化、反序列化。
pub struct RpcError {
    pub code: String,
    pub message: String,
}
// RpcError::new(code: impl Into<String>, message: impl Into<String>) -> RpcError

pub async fn serve(
    listener: tokio::net::TcpListener,
    handler: std::sync::Arc<dyn RpcHandler>,
    config: ServerConfig,
    cancel: tokio_util::sync::CancellationToken,
) -> std::io::Result<()>;
```

`ServerConfig::new(token)` 构造配置，所有字段公开。`serve` 在配置和地址不合法时返回 `InvalidInput`；取消传入的 token 后等待连接清理并返回。丢弃 `serve` Future 也会取消并中止其拥有的连接任务。宿主应正常取消并等待 `serve`，从而给处理器清理机会。

`Client` 实现 `Clone + Send + Sync`，所有副本共享一个连接。

```rust
impl Client {
    pub async fn connect(
        addr: std::net::SocketAddr,
        token: impl Into<String>,
    ) -> Result<Client, ClientError>;

    pub async fn connect_with_config(
        addr: std::net::SocketAddr,
        token: impl Into<String>,
        config: ClientConfig,
    ) -> Result<Client, ClientError>;

    pub async fn request(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, ClientError>;

    pub async fn request_with_cancel(
        &self,
        method: &str,
        params: serde_json::Value,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<serde_json::Value, ClientError>;

    pub async fn close(&self);
}

// 可序列化、反序列化；同时实现 std::error::Error。
pub struct ClientError {
    pub code: String,
    pub message: String,
    pub outcome_unknown: bool,
}
```

`connect` 只在显式调用时建立连接。库从不自动重连或重发。`close` 关闭所有副本共享的连接，并等待读写任务退出；最后一个 `Client` 被丢弃时也会通知后台任务释放 socket。请求 Future 被接纳后若直接丢弃，会关闭整个连接，并将其他未完成请求标为结果未知。需要保留连接的取消应调用 `request_with_cancel`，取消其 token 后继续等待该请求的终态。

处理器必须将工作包含在返回的 Future 中，不得自行分离有副作用的后台任务。取消、超时或断连会取消处理器 token，继续轮询最多 `cancellation_grace`，然后丢弃仍未完成的 Future。阻塞线程或不可取消的外部副作用不可能由丢弃 Future 撤销；这种情况必须通过运行时持久记录核对。

## v1 行协议

每帧是一行 UTF-8 JSON，以一个 LF 字节结束。`max_frame_bytes` 不包括结尾 LF。读取使用固定大小缓冲区并在每次追加前检查长度；不会使用无限增长的 `read_line`。写入也在序列化过程中限制缓冲区大小。整帧最多嵌套 64 层对象/数组，包含最外层协议对象，因此 params/result 最多占 63 层。客户端在发送前检查本地 `Value` 深度，超出时返回 `invalid_params` 且结果已知；过深的处理器结果返回 `response_too_large_outcome_unknown`。JSON 的字符串、数字、布尔值、null 和数组不会互相转换类型。

请求必须且只能包含 `version`、`id`、`method`、`params` 四个字段。ID 必须是非空字符串，按 UTF-8 字节数限制长度，不能含控制字符。method 同样非空、不含控制字符，最大 256 字节。

```json
{"version":1,"id":"h","method":"hello","params":{"token":"host-provided-token"}}
{"version":1,"id":"h","result":{"version":1,"max_frame_bytes":1048576,"max_in_flight":64,"max_requests_per_connection":65536,"max_id_bytes":128}}
{"version":1,"id":"1","method":"session.get","params":{"session_id":"example"}}
{"version":1,"id":"1","result":null}
```

连接的第一帧必须是 `hello`，成功返回版本和服务端限制后才允许调用处理器。客户端将本地限制与服务端限制取较小值。`hello` 和 `rpc.cancel` 是传输层保留方法，不能通过 `Client::request` 调用，也不会分派给宿主。

响应必须且只能包含 `version`、`id` 和 `result` 或 `error` 其中之一。`result:null` 是有效成功响应。error 为 `{code,message}`。整帧 JSON 在转换为 `Value` 前先递归检查：任意对象中的重复字段都拒绝，包括 hello 的 token、取消目标、业务 params/result；字段名按 JSON 转义解码后比较，`"x"` 与 `"x"` 也算重复。同名字段位于不同对象内合法。额外协议字段、错误响应形状、无效 UTF-8、坏 JSON、尾随第二份 JSON、超大帧都拒绝。未知版本返回 `unsupported_version` 后断开；认证失败返回 `unauthorized`，先调用业务方法返回 `handshake_required`。

所有请求帧（包括 hello、取消帧和被拒绝的业务请求）的 ID 在同一连接内都不能重复。服务端保留有上限的已见 ID 表；达到 `max_requests_per_connection` 后继续发帧会关闭连接，已有请求可能变为结果未知。客户端到达该预算时会在发送前返回 `connection_limit`；宿主可以等待已有调用终结后显式关闭，再显式打开新连接。不存在自动换连接或自动重放。

hello 响应之后，如果握手读取器已缓冲其他字节（包括未结束的半帧），`connect` 在返回可用句柄前以 `protocol_error` 拒绝连接；此时没有业务请求，`outcome_unknown:false`。读取器不会被替换或清空；后到数据仍由同一读取器校验。

客户端为每个业务及取消帧生成独立的操作系统随机数（128 位），ID 由十六进制计数、`-` 和 32 位十六进制随机数组成。计数保证连接内唯一性，随机部分使观察到一个 ID 的对端无法推算后续 ID。协商后的 `max_id_bytes` 至少需要 34，过小时 `connect` 返回已知失败；计数增长超出协商宽度时返回 `connection_limit`。业务 ID 的操作系统随机源失败时在发送前返回 `id_unavailable`；取消 ID 分配失败按既有取消失败契约返回 `cancel_failed` 并关闭连接，原调用仍须核对结果。

响应只能对应本连接中已经完整写出且尚未终结的请求。仅入队或尚在部分写入的请求还不能接收响应；未知、重复、提前响应或版本/形状错误会关闭连接，并将所有仍未完成的调用返回为 `outcome_unknown:true`。已经交付的终态不会被撤回。合法连续请求及合并到一次写入的正常响应仍完整保留。

## 取消与结果未知

取消帧自身使用新的唯一 ID，其 `params.request_id` 指向本连接中曾接纳的业务请求。

```json
{"version":1,"id":"2","method":"rpc.cancel","params":{"request_id":"1"}}
{"version":1,"id":"2","result":{"cancelled":true}}
{"version":1,"id":"1","error":{"code":"cancelled_outcome_unknown","message":"request cancelled; side effects may have happened"}}
```

取消帧和原请求各有一个终态；二者的响应顺序没有要求。原请求已结束时取消返回 `cancelled:false`，不重复生成其终态。不存在或属于其他连接的目标 ID 会关闭当前连接，不能取消其他连接的工作。处理器等待期间，服务端仍读取取消和其他请求。

客户端收到原请求终态后可以返回，但内部取消确认仍有期限和数量限制。对端遗漏取消确认也会触发连接关闭，不能无限累积等待项。

`ClientError.outcome_unknown` 为 `true` 时，宿主必须先核对持久状态，不能按普通通信失败盲目重试。

| 情况 | code | outcome_unknown |
| --- | --- | --- |
| 请求尚未发送：本地参数/配置错误、容量不足、ID 预算耗尽、预先取消 | `invalid_method` / `invalid_config` / `client_busy` / `connection_limit` / `request_cancelled` 等 | false |
| 服务端没有分派：同时执行数量已满 | `busy` | false |
| 处理器明确确认失败 | 处理器提供的普通 code | false |
| 服务端取消或执行超时 | `cancelled_outcome_unknown` / `timeout_outcome_unknown` | true |
| 响应太大或处理器 panic | `response_too_large_outcome_unknown` / `handler_panic_outcome_unknown` | true |
| 客户端等待超时、写入失败、协议错误、断连、主动关闭时尚在途 | `request_timeout` / `write_failed` / `protocol_error` / `connection_lost` / `connection_closed` 等 | true |

处理器遇到无法确认的副作用，应返回 `RpcError::new("outcome_unknown", message)`，或使用以 `_outcome_unknown` 结尾的 code。客户端通过这一规则识别已收到的未知结果。普通 `Err` 只能表示已知失败。传输层取消/超时保守地返回未知，即使处理器随后在清理期间返回了普通错误。

## 有界资源与期限

服务端默认值如下。

| ServerConfig 字段 | 默认值 | 作用 |
| --- | --- | --- |
| `max_frame_bytes` | 1 MiB | 输入与输出 JSON 长度上限，允许 256 字节到 64 MiB |
| `max_in_flight` | 64 | 每连接执行中的处理器 Future 上限 |
| `max_connections` | 128 | 所有连接任务上限，包含尚未认证的连接 |
| `max_requests_per_connection` | 65536 | 每连接已见 ID 总数上限，至少 2 |
| `max_id_bytes` | 128 | 单个 ID 的字节数上限，最多帧上限的一半 |
| `response_queue_capacity` | 64 | 每连接等待写出的响应数上限 |
| `request_timeout` | 30 s | 单个处理器运行期限，不含取消清理时间 |
| `handshake_timeout` | 5 s | 完整 hello 收发期限 |
| `frame_timeout` | 60 s | 收齐下一帧的期限，包含两帧之间的空闲时间 |
| `write_timeout` | 5 s | 从响应入队到完整写出的一次期限 |
| `cancellation_grace` | 250 ms | 取消后继续轮询处理器的最长时间 |

客户端 `ClientConfig::default()` 使用同名的帧、同时执行数、ID 数量和 ID 长度默认值；`write_queue_capacity=64`、`connect_timeout=5s`、`request_timeout=30s`、`write_timeout=5s`。connect 期限覆盖 TCP 建连和整个 hello；request 期限覆盖本地接纳后到终态。所有待回应 ID，包括取消确认，都有后台期限。未终结 ID 数最多为协商后 `max_in_flight` 的两倍。计数和期限不能为零，也拒绝会使 Tokio 容量或时间运算溢出的配置。

服务端不建立无限业务队列；超出处理器上限返回 `busy`。客户端容量不足时在发送前返回已知失败。响应队列满时立即关闭连接；写不出去或排队超过期限也会关闭连接，并取消该连接所有处理器。缓冲区在数量和单帧字节数上均有界；宿主应结合实际响应大小调小这些限制。

断开后如果需要接续事件，由宿主在新连接上显式轮询 `events` 方法，并携带运行时保存的事件游标。这个 crate 不实现事件存储、Session 恢复或推送频道，不能把内存响应队列当成持久事件记录。

## 示例与可复跑验收

在仓库根目录，终端一启动演示服务端；token 为本地演示值，真实宿主应自行安全生成并传递。

```sh
export CAYW_IPC_TOKEN='local-demo-token'
/Users/chengwen/.cargo/bin/cargo +1.95.0 run -p cayw-ipc --example ipc_server --locked -- 127.0.0.1:0
```

服务端输出实际分配的 loopback 地址。终端二使用同一个 token，将示例端口替换成输出值。

```sh
export CAYW_IPC_TOKEN='local-demo-token'
/Users/chengwen/.cargo/bin/cargo +1.95.0 run -p cayw-ipc --example ipc_client --locked -- 127.0.0.1:PORT
```

`fixtureEcho` 仅在示例/测试处理器中实现，库内没有演示业务方法。`tests/fixtures/client.rs` 是独立子进程客户端，集成测试会运行实际可执行文件，验证认证和往返。

```sh
/Users/chengwen/.cargo/bin/cargo +1.95.0 fmt -p cayw-ipc -- --check
/Users/chengwen/.cargo/bin/cargo +1.95.0 check -p cayw-ipc --all-targets --locked
/Users/chengwen/.cargo/bin/cargo +1.95.0 test -p cayw-ipc --all-targets --locked
/Users/chengwen/.cargo/bin/cargo +1.95.0 clippy -p cayw-ipc --all-targets --locked -- -D warnings
```

测试使用真实本机 listener/client、协议异常对端和独立客户端进程，覆盖认证/版本、并发配对、取消和断连、ID 生命周期、半帧、超大帧、恶意响应、慢消费者、读写期限、不自动重连重发及后台任务释放。期限测试配置短超时，但只验证结果和释放信号，不断言固定毫秒性能。当前 macOS 已运行；Windows 和 Linux 未验证。未调用真实模型服务。

直接依赖使用 workspace 中的 `async-trait`、`futures`、`serde`、`serde_json`、`tokio` 和 `tokio-util`，另以 `getrandom 0.3` 从操作系统生成请求 ID 随机数，均使用 MIT / Apache-2.0 许可选项；依赖元数据中 `unicode-ident` 还包含 Unicode-3.0，均可与本项目 Apache-2.0 共存。
