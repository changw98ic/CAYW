# memory/context/work 与 remove-over-defense 的 dev 整合检查

日期：2026-11-10。本文只记录本次离线检查，不代表 Rust 工作区、macOS 产品或真实模型已通过验收。

## 输入与合并方式

- `dev`：`9cc509d66dff9300333ca0bf5af2e8b7868c0541`
- `feat/remove-over-defense`：`523981db44f9ad2de44d3c05c3fd64af62cbc442`
- `feat/memory-context-work`：`c55559d96c5fd734ecf5fb26b8920e64866082c7`
- 两功能分支共同祖先：`dffc4c23d47c207a06f585c1d3703ee404548a07`

在独立工作树中以 remove-over-defense 为第一父提交，合并 memory-context-work。dev 是第一父提交的祖先；不重写分支历史。合并后重新核对三个远端 head，再对 dev 做普通快进更新。

## 语义处理

- 保留移除过度防御分支的可选预算语义：未设置预算就是没有该预算，不重新补固定上限。
- 主 session 的记忆输入接入原有请求记账，不把输入、输出上限或实际用量字段互相代填。忘记过滤在未设置上下文窗口的路径也生效，保持工具调用/结果配对及原 unknown 状态。
- 本地 Client 使用 ModelAuto。模型通过 CodeMode 审批当前 Task 的原待决操作，审批工具不会再次等待人工审批。`approval_id` 与 capability request 的 `request_id` 保持独立。SDK 默认 Explicit。
- Git 恢复点用于误删误改恢复，未覆盖的路径不再因此失去操作授权。未添加通用 shell 执行入口；移除 WorkItem 的直接命令式验收，不映射成其他验收类型。
- 不加入旧版本模型配置迁移。设备共享模型列表，工作区独立保存选用；显式 Headless 配置仍有自身语义。
- 工作派发返回实际 Task 的 session/work/key 身份。重复 dispatch key 不能把其他 WorkItem 或 session 的 Task 伪装成当前派工结果。

## 已运行的检查

环境为 Linux 云工作区，Node 24、Python 3.12。以下测试均未连接真实模型。

| 命令/检查 | 结果 | 范围 |
| --- | --- | --- |
| `node --test crates/cayw-webui/tests/*.test.mjs` | 122 通过，0 失败 | WebUI 状态、DOM、审批及中断历史回归 |
| `python3 scripts/test_product_eval.py` | 63 项中 56 通过、7 跳过 | 离线评分器；7 项依赖 Rust 的测试未运行 |
| `python3 scripts/test-install-cli.py` | 21 通过 | 合成程序/故障测试，不是 macOS 安装验收 |
| `python3 scripts/test_install_cli.py` | 1 通过 | 安装提示文本 |
| `python3 scripts/test_validate_headless_chat.py` | 7 通过 | 本地离线证据记录器 |
| `python3 scripts/test-generate-third-party-notices.py` | 17 通过，1 失败、2 错误 | 3 项均因缺少 cargo，无法运行真实依赖 metadata |
| scripts 中 Python 编译、shell 语法；Cargo TOML 解析 | 通过 | 40 个 Cargo.toml、6 个 shell 脚本；不等于 Rust 编译 |
| `git diff --check`、未解决合并项与冲突标记检查 | 通过 | 差异与合并完整性 |

## 未验证

当前环境没有 cargo、rustc 或 rustfmt；没有为本次检查安装工具链。因此以下均未执行：

- `cargo fmt --all -- --check`
- `cargo check --workspace --all-targets --locked`
- `cargo test --workspace --locked`
- `cargo clippy --workspace --all-targets --locked -- -D warnings`
- 新增模型 auto 审批、记忆过滤、派工身份及设备模型目录的 Rust 回归测试。
- macOS TUI、打包、真实安装、Git/PTY 及真实 Provider 验收。

仓库 CI 当前只在 main push 或 pull request 触发；本次直接更新 dev，不修改 CI 触发条件，也不创建 PR。不能将 dev 更新视为 CI 通过。
