# Trace Debug

Trace Debug 是独立、可选的本机调试插件。它通过认证 IPC 读取 Runtime 事件，在网页里显示模块架构图、节点路径、输入、状态、错误和原始证据。保存节点修改后，插件用捕获的输入和前置状态自动重新计算，并显示新的分支和输出。

普通 Runtime 不依赖此包。关闭插件不会取消原任务。插件不直接打开 Core 或 Cells 数据库，也不持有业务任务写入接口。

## 启动

在仓库根目录构建一次：

```sh
cargo +1.95.0 build -p cayw-trace-debug --bin cayw-trace-debug --locked --offline
```

通过可信环境提供已有 Runtime 的 `CAYW_IPC_ADDR`、`CAYW_IPC_TOKEN`，并提供独立的 `CAYW_TRACE_WEB_TOKEN`。两个令牌必须不同，长度为 32..4096 个不含空白的 ASCII 字符。令牌不放在命令行或 URL 中。

```sh
target/debug/cayw-trace-debug \
  --store /absolute/path/to/trace.json \
  --session SESSION_ID \
  --web-addr 127.0.0.1:0
```

启动后打开标准输出中的 `web_url`，输入网页令牌。`--session` 可重复指定，最多观察 16 个会话。只能绑定本机地址。按 Ctrl+C 停止插件。

追踪文件保存运行源码、输入、上下文、原始事件和全部内容版本。新文件权限为 `0600`，同一文件只允许一个插件进程打开。文件达到 32 MiB、十万条事件、256 次执行或单次执行 256 个版本时，插件明确提示改用新文件，不静默删除历史。

## 实际离线演示

演示使用固定离线模型响应，但真实执行 Runtime、Host 文件读写、Luau 和认证 IPC。它不调用外部模型。演示服务与插件是两个进程。

```sh
cargo +1.95.0 build -p cayw-trace-debug \
  --bin cayw-trace-debug --example demo --locked --offline
python3 -I scripts/validate-trace-debug.py serve \
  --demo target/debug/examples/demo \
  --plugin target/debug/cayw-trace-debug \
  --output /absolute/new/trace-demo
```

`--output` 必须是新目录。脚本会保存进程、命令、构建文件校验值和日志。另开终端显式启动一次任务：

```sh
python3 -I scripts/validate-trace-debug.py task \
  --output /absolute/new/trace-demo
```

打开 `ready.json` 中的 `web_url`。网页令牌是脚本内公开的合成测试数据：

```text
issue31-public-web-fixture-1111111111111111
```

选中含五个节点的运行。原始条件读取不存在的 `input.amount`，所以失败。点击 `condition`，把节点源码中的 `input.amount` 改成 `input.value`，保存后自动重新计算。参数 `{"threshold":5}` 走 `high` 分支；改为 `{"threshold":20}` 走 `low` 分支。最终输出随分支变化。原始失败记录仍可查看，撤销会创建新的内容版本并自动重新计算。

模拟不会创建 Runtime 任务，也不会重做演示中的文件写入。

要观察节点实际等待 Host 响应，在另一个新演示目录启动服务，再显式运行：

```sh
python3 -I scripts/validate-trace-debug.py task \
  --output /absolute/new/trace-demo --wait-read
```

任务会在 `input` 节点请求读取 `approval-input.json`。该读取没有预授权，因此任务命令会等待。演示的 IPC 请求和单次 Host 等待均限定为 180 秒，须在此期间完成批准。网页的模块图、节点图、时间线和详情都会显示“等待响应”。在另一终端批准这一次固定文件读取：

```sh
python3 -I scripts/validate-trace-debug.py approve-read \
  --output /absolute/new/trace-demo
```

批准后原任务继续执行，原始条件仍会因 `input.amount` 不存在而失败。保存节点修改后的本地计算只使用已捕获的读取响应，不会再次申请或执行文件读取。“等待响应”根据原始运行中尚未结算的节点调用显示，不把历史结果或本地重放标成等待。

独立验证停用行为：

```sh
python3 -I scripts/validate-trace-debug.py stop-plugin \
  --output /absolute/new/trace-demo
python3 -I scripts/validate-trace-debug.py task \
  --output /absolute/new/trace-demo
```

第二条命令仍通过普通 IPC 执行任务。最后在 `serve` 终端按 Ctrl+C 结束演示 Runtime。

## 节点接入

可独立重新计算的节点由 `GraphPlan` 声明。每个节点提供 Luau 函数体，以及 `params`、`input_pointer`、模块名称和实现位置。函数参数为 `input, state, params, context`，返回值为：

```lua
return {
  output = {value = input.value},
  state = {visited = (state.visited or 0) + 1},
  branch = "high"
}
```

`output` 和 `state` 必须存在，嵌套最多 32 层；JSON 空值使用 `cayw.null`。`branch` 可省略，选择默认边；提供时必须匹配该节点的分支边。首版支持最多 64 个节点的无环图。`input_pointer` 是进入节点前原始输入的 JSON 路径，例如 `/payload/value`；空字符串表示整个输入。

调用 `engine::script_for_plan(&plan)` 生成完整首轮脚本。真实执行通过 `cayw.emit` 输出 `cayw.trace.v1` 的 manifest、started、completed。Cells 先保存输出，Runtime 再发布带执行、任务、源码和输出游标的 `trace_node` 事件。插件核对 Core 的实际源码校验值与生成脚本一致，再核对节点顺序、分支、输入和进入前状态。节点自己的 `cayw.emit` 与 `_G.cayw.emit` 拒绝使用保留的追踪结构；普通 JSON 输出仍可用。追踪结构有误时，该图显示缺口，原始事件继续保存。原始输出不构成额外执行授权。

普通未接入节点声明的脚本仍显示能力调用与执行状态，但会明确列出缺少节点计划、输入和状态快照的部分；不能把普通调用日志当成完整可重新计算的节点图。

## 保存、对比与边界

保存会先持久化新内容版本，再在插件内复用 `cayw-codemode::CodeMode` 求值。**节点保存**走局部重算（`recompute`）：复用修改点上游已完成步骤，从该节点捕获的输入与进入前状态起重新求值，并只更新受影响的下游；步骤可标为 `reused`。**撤销**等无修改点路径走入口整图本地重放（`replay`），步骤不标复用。外部调用仍只读取准确匹配的捕获响应。输入映射、参数和条件都参与本轮计算。保存路径不会启动 Cargo、类型检查、构建或全量测试。

每次结果明确标记：`observed` 是原始 Runtime 记录；`replay` 是从入口的本地整图重放；`recompute` 是从修改点起的局部重算。后两者都没有真实 Host 执行句柄。外部能力只能返回原始捕获且名称、参数、顺序一致的响应。缺少响应时停止并显示具体缺口。原运行未结束或存在未知业务效果时，不自动重算。

原始事件时间戳用于近似显示真实节点和调用耗时。本地计算使用单调时钟计时。网页更新还受到事件传输和长轮询周期影响，不能把服务端投影耗时当成浏览器呈现耗时。

编辑的是可解释执行的节点内容。任意 Rust 等原生源码修改仍需编译；本地节点结果不表示新原生二进制已经运行。插件不恢复丢失的虚拟机调用栈。缺少原始输入、进入前状态或执行适配时，页面继续允许查看和编辑，但不会伪造成功。

接口为 `GET /api/state`、`POST /api/projects/{id}/edit` 和 `POST /api/projects/{id}/undo`。API 仅接受网页 Bearer 令牌。保存包含 `expected_version`，旧版本提交返回冲突，避免覆盖新内容。两种图、时间线和对比都以选中的执行及结果版本为准。

可复跑验收见 [macOS 离线记录](../../docs/validation/issue-31-trace-debug-macos.md)。另有独立的 [Magpie 真实模型补验](../../docs/validation/issue-31-magpie-macos.md)，记录实际请求、网页修复、分支变化、停用后普通任务，以及原始失败与预算保留。普通测试和上述离线演示不会触发真实模型请求。
