# cayw-webui

`cayw-webui` 是 CAYW 的可选本机浏览器界面与 loopback 轻网关。它通过公开的认证本机 IPC（`cayw-ipc`）转发请求，不依赖可选远程 Server，也不直接访问 Core 数据库或执行隐藏工具。

## 职责与边界

- **本地轻网关**：仅绑定 loopback 地址，打开网页直接连接本机工作空间，无需手动输入网页令牌；网关严格校验 `Host`、`Origin`、`Sec-Fetch-Site`，并在本机持有 IPC 令牌（`CAYW_IPC_TOKEN`）防止向浏览器泄露。
- **会话与恢复安全语义**：支持流式对话、工具输出折叠查看、审批决定、取消请求、游标续读，以及未知副作用（`outcome_unknown`）与冻结会话的显式确认 Fork。
- **多项目管理与插件管理**：支持在同一本机网关下配置和切换多个本地 Runtime 项目（各项目会话、事件游标与未决写入锁彼此隔离），并提供插件管理页面查看会话绑定的 plugin、版本代际、公开能力与冻结转换入口。暂不包含多设备管理。

## 运行方式

单项目运行：

```sh
CAYW_IPC_ADDR=127.0.0.1:4100 \
CAYW_IPC_TOKEN=fedcba9876543210fedcba9876543210 \
cargo run -p cayw-webui --locked
```

多项目运行（通过 `--project` 或 `CAYW_PROJECTS` 附加本机项目）：

```sh
CAYW_IPC_ADDR=127.0.0.1:4100 \
CAYW_IPC_TOKEN=fedcba9876543210fedcba9876543210 \
cargo run -p cayw-webui --locked -- --project docs=文档项目@127.0.0.1:4101
```

## 验证命令

```sh
node --test crates/cayw-webui/tests/state.test.mjs crates/cayw-webui/tests/ui.test.mjs crates/cayw-webui/tests/dom.test.mjs crates/cayw-webui/tests/core-protocol.mjs
cargo test -p cayw-webui --all-targets --locked
```
