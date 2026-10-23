# Runtime RPC 统一入口

这是当前实施批次的共享契约。协议外壳由 `cayw-ipc` 处理：`{version:1,id,method,params}`，响应按相同 id 返回 result 或结构化 error。界面与远程接入只能转发公开方法，不直接执行工具。

| method | params | result |
| --- | --- | --- |
| `runtime.health` | `{}` | `{status:"ok",protocol_version:1}` |
| `session.create` | `{label:string}` | SessionSnapshot |
| `session.list` | `{}` | SessionSnapshot 数组 |
| `session.get` | `{session_id:string}` | SessionSnapshot |
| `session.events` | `{session_id:string,after:u64,limit?:usize}` | EventRecord 数组，严格 after 游标；默认 limit 100 |
| `session.prompt` | `{session_id:string,text:string}` | 最终 SessionSnapshot；过程事件通过独立 `session.events` 轮询消费 |
| `session.cancel` | `{session_id:string}` | `{cancelled:bool}`，实际终态随后进入事件日志 |
| `approval.decide` | `{session_id:string,approval_id:string,decision:"allow_once"|"allow_task"|"deny"}` | 已记录的审批决定 |
| `session.fork` | `{session_id:string,label:string,confirm:bool,mode:"chat"}` | 新 SessionSnapshot；confirm 非 true 必须拒绝 |
| `capabilities.list` | `{}` | 宿主当前可发现的能力声明数组 |

SessionSnapshot 至少有 `id`、`label`、`messages:Message[]`、`cursor:u64`、`state`、当前 attempt、pending approvals 与 unresolved calls。EventRecord 至少有 `cursor:u64` 与可序列化的事件内容。其他内部字段可以添加，但所有接入必须使用同一个真实运行接口。

`session.prompt` 执行到终态才完成响应；事件读取/取消/审批必须能在同一服务的其他并发请求中继续处理。调用方断连或取消请求时，运行层必须取消本次执行并保留真实结算；不自动重发。UI 获取 events 后只在成功消费时推进 cursor，掉线重连不重发 prompt。

所有接口都在明确宿主授权上下文中运行。本地 IPC token 由宿主显式配置。可选网页使用单独 local access token，不将 IPC token 发给浏览器；浏览器提交的内容作为用户数据，不能授权协议自身不允许的权限。审批不可由模型调用 `approval.decide`。

`session.reconcile` 的具体证据形状待 Core 实现后记录；初版 UI 不提供凭空清除未知副作用的按钮。缺少真实核对结论时保留阻止状态。
