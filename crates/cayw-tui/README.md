# cayw-tui

`cayw-tui` 是 CAYW 的可选本机终端界面客户端。它通过公开的认证 loopback IPC（`cayw-ipc`）连接宿主持有的 `cayw-runtime`，本身不执行模型请求、不调用隐藏工具、也不直接读写宿主数据库或工作区文件。

## 职责与边界

- **纯 IPC 消费端**：使用 `session.list`、`session.create`、`session.get`、`session.events`、`session.prompt`、`session.cancel`、`approval.decide`、`session.fork` 完成会话切换、流式回复、审批、取消和冻结会话显式 Fork。
- **只读查看视图**：提供对话（`Chat`）、文件（`Files`）、变更对比（`Diff`）和事件与执行日志（`Logs`）四个只读视图。所有内容均投影自权威快照与已消费事件，未知副作用（`outcome_unknown`）不会被当作已确认结果展示，也不会因切换视图隐藏安全提示。
- **断连不重放**：连接断开时保留本地草稿和已消费事件游标，重连后从保留游标续读并重新读取权威快照，不自动重发未确认的提交。

## 运行方式

发行包通过 `sh install.sh` 同时安装 `cayw`、`cayw-tui` 和 `cayw-client`。先在项目目录运行：

```sh
cayw serve
```

再在另一终端进入同一项目目录，打开终端交互界面：

```sh
cayw tui
```

`cayw tui` 使用 `~/.cayw`（或 `CAYW_HOME`）中当前工作区的连接记录，把本机连接设置交给同目录的 `cayw-tui`。无需手填地址或访问码，也不会启动另一份 Runtime。`--workspace DIR` 可以连接其他项目。退出界面后，独立启动的服务继续运行。

从源码构建时，用 `cargo build -p cayw-runtime -p cayw-tui --bins --release --locked` 把两个程序放入同一个构建目录，再运行 `target/release/cayw tui`。

需要直接连接显式地址的本机 Runtime IPC 时：

```sh
CAYW_IPC_ADDR=127.0.0.1:4100 \
CAYW_IPC_TOKEN=your-host-ipc-token \
cargo run -p cayw-tui --locked
```

可用 `--addr 127.0.0.1:4100` 覆盖 `CAYW_IPC_ADDR`；令牌只从环境变量 `CAYW_IPC_TOKEN` 读取，不接受命令行参数。

## 快捷键

- `Tab` / `Shift+Tab`：在会话列表、主视图和输入框之间切换焦点
- `Alt+1` / `Alt+2` / `Alt+3` / `Alt+4`：切换 `Chat` / `Files` / `Diff` / `Logs` 只读视图（焦点在主视图时也可用 `←` / `→` 或 `[` / `]`）
- `Ctrl+N`：新建会话
- `Ctrl+L`：刷新会话列表
- `Enter`：发送消息（输入框焦点）或打开选中会话（会话列表焦点）
- `Alt+Enter`：在输入框内换行
- `Ctrl+X`：请求取消当前运行
- `Alt+O` / `Alt+T` / `Alt+D`：待审批操作允许一次 / 允许本任务 / 拒绝
- `Ctrl+F`：对已冻结且副作用已结算的会话打开显式普通聊天 Fork 确认
- `PgUp` / `PgDn` / `↑` / `↓` / `Home` / `End`：滚动当前主视图
- `Ctrl+R`：重新连接并从已消费游标续读
- `Ctrl+Q` / `Ctrl+C`：退出并恢复终端状态
