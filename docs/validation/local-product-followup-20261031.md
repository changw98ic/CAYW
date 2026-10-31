# 本地产品工作流补充验证：网页工作区与配置权威

本记录延续 [第一阶段记录](local-product-workflows-20261031.md)，保留 `27d7951` 与 `f604a06` 两个原提交。当前分支仍为 `feat/product-workflows-20261031`，基线 `0e2b2596a67b98ab5c048974b08523413eaf33c5`。本轮功能提交 `dea7a48`，配置修复 `b4691d0`，本机选项修复 `9ad1a5a`，最终执行源码 `92fee2297742c5efd90332fd16e43202345a98dd`；随后仅提交本验证文档。

执行环境仍为 Linux x86_64、Rust 1.95.0、Node 24.19.0、Chromium 151；这些是开发验证，不是 macOS 首发验收。无付费模型请求，不读取真实用户模型凭据，不发布、推送、开 PR 或合并。

## 实际调用链

网页侧栏选择登记目录 → `/api/rpc` 调用 `settings.workspaces.open` → 网关固定转发给原 CLI 宿主 → `cli_workspaces::Workspaces` 按 opaque ID 查私有登记表 → 认证已有 Runtime 或启动同版本独立 Runtime → 网关登记内部 IPC 路由并从公开回执移除 IPC 地址 → 页面后续请求携带所选 `workspace_id`。

没有按网页提供的路径打开任意目录。新项目须先通过本机 `cayw init --workspace DIR` 登记。每个 Runtime 的根目录保持固定，网关按原有 Session/Task 所有权拒绝跨工作区误发请求；切换页面选择不改变网关的全局默认目标，也不停止旧工作区。已提交 enqueue/resume 保存原工作区并保留请求连接，不重发。宿主只持有和清理由自己创建的 Child，复用的外部服务由原所有者管理。

使用自定义数据库或显式 IPC 令牌的服务没有共享本机发现身份，会明确拒绝工作区启动功能，普通认证会话 API 保持可用。原有远端 Device 路由保留；网关回归用显式 `workspace_id:local` 验证既有远端创建路径。

## 已运行用户路径

- A 的真实浏览器任务进入文件审批等待，切到 B。B 无旧会话，原 A 的 Task、worker、Attempt、fence 和状态均未改变，cancel_requested 为 false；没有第二次 resume。
- B 由原宿主启动。重复打开 B 复用实例。以 B 作用域请求 A 的 Session 或 resume A 的 Task，被网关在转发前拒绝。
- 在 B 输入执行设备 `local` 创建会话，仍属于所选 B；A、B 的会话列表各自隔离。
- 切回 A，恢复原会话与审批，显式取消同一 Task 后得到 cancelled，没有文件写入。
- 切到外部独立启动的 C，复用其服务。关闭原宿主后，由它创建的 B 已停止，外部 C 仍存活且能通过认证 IPC 查询。
- 默认界面读取 Editor/Trace 的实际同目录安装元数据。未安装入口禁用，并显示源码构建或同前缀安装命令、启动帮助；Pi 显示未编译、未启用或已启用。安装状态可刷新。外部服务地址未配置时，打开按钮禁用，不把主网页或 IPC 令牌传给可选工具。
- 自定义数据库/令牌场景：普通认证 session.create 成功；工作区管理明确拒绝不兼容发现身份，未启动子工作区。

## 已修复配置遮蔽

原调用链中，离线 configure 只更新 `config.json.model`，旧 `models.json` 的已选 A 可能继续遮蔽新保存 B。本轮没有再增加选择优先级：在线、离线 configure 都通过共享读写函数原子更新同一工作区方案库并选中 configured；在线服务随后从该库 reload，不再执行另一组 put/use 写入。

实际回归：在线 configure A → 停服务 → 无密钥的离线元数据保存成功 → 离线 configure B → 重启 → health 和已选 configured 均为 B。全程未发送模型请求。在线配置缺失环境变量时返回非零，具体变量名称保留在错误中，当前模型仍为 B；恢复有效的合成变量配置后可正常应用。输入中断同时保留兼容配置与模型方案库原字节。失去激活回执时保留 outcome_unknown，不宣称当前模型一定未改变。

## 检查与证据范围

[JSON 摘要](local-product-followup-20261031.json) 记录命令、UTC 起止、退出码、日志 SHA256、源码和冻结程序身份。最终冻结 Runtime 启用 plugins；原有 14 项 Runtime、27 项网关、101 项网页单元/DOM 回归、全仓 check、浏览器模型/插件/聊天及 PTY 路径在 `9ad1a5a` 通过。`92fee22` 仅增加主 CLI 与工作区管理器的发现身份限制，其他文件逐项核 SHA 相同；因此没有重跑未改变的回归。

最终 `92fee22` 冻结程序重新运行了工作区浏览器主链、完整产品主链（含 A→B 配置回归）、自定义身份反例、fmt 及产品库/二进制 Clippy，全部退出 0。Clippy 明确允许既有 dead_code、unused_mut、unreachable_code、useless_conversion 平台告警，不等同严格全仓通过。严格全仓 Clippy 仍由未修改的 Device 非 macOS 分支三项错误阻塞，原退出 101 与日志保留。

早期验证脚本曾把 waiting_approval 误按 running 比较，并过度要求审批拒绝后任务必须 failed；最终使用显式取消检查取消语义，没有为迎合断言修改 Core。相关早期失败日志作为历史证据保留。

最终交付包只包含完整源码、完整补丁、命令日志、摘要和辅助脚本。浏览器截图保留本地，不上传、不收入包。macOS arm64 原生构建、安装、升级失败回滚和 Terminal.app 行为仍未执行；Editor/Trace/Pi 仍是独立可选组件。
