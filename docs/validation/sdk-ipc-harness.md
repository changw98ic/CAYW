# SDK / IPC 离线验证记录

> 2026-10-19 起测试工具改名为 VerifyTool，代码包 `cayw-harness` 改为 `cayw-verify-tool`，`cayw-modules` 改为 `cayw-plugins`。本记录保留当时的原始命令和路径，复跑时请换用新包名。

2026-10-19，`cayw-harness` 已通过公开 Rust SDK 和需要认证的本地 TCP IPC 执行离线场景。本记录只覆盖冻结源码、macOS ARM64、Rust 1.95.0、debug 构建和固定本地 Provider；不代表 Issue #2、三平台或真实模型任务已经验收。

## 实际执行路径

固定 Provider 产生 Luau 工具请求，由实际 Runtime、Cells、CodeMode 和 Host 完成文件写入与读取。测试使用明确的固定授权，不把测试配置称为用户批准。第二次写入同时提供 `overwrite=true` 和原文件内容 SHA-256，仍经过正常的内容核对。

IPC 场景使用真实客户端与服务端，并保存经过本地透明转发的请求/响应帧。记录包含连接、原请求 ID、取消目标和认证结果。错误的公开测试 token 必须被拒绝；该 token 不是用户凭据。

七项 SDK / IPC 场景分别验证：

| 场景 | 实际检查 |
| --- | --- |
| `cold-sdk` | 新进程启动、首次 SDK 执行、退出及重开会话 |
| `sdk-lifecycle` | 同一 Runtime 首次与后续执行，重开后历史和文件一致 |
| `ipc-lifecycle` | 认证连接上的两次实际 Luau 文件往返 |
| `ipc-reconnect` | Provider 等待期间断开，用新客户端从磁盘保存的 Session / 游标继续读取事件 |
| `ipc-cancel` | `rpc.cancel` 指向原请求；保留原响应，同时确认任务已取消、会话空闲及无工具副作用 |
| `host-restart-before-dispatch` | 确认原宿主退出后恢复；脚本和工具均未执行，恢复不重放 |
| `host-restart-unknown` | 本地合成副作用及就绪标记同步到磁盘后终止宿主；恢复保留原调用的未知结果、拒绝新任务并且不重放 |

取消的原 IPC 响应可以保守地保留 `outcome_unknown`。任务已取消和会话空闲是另行检查的事实，不能用来改写原响应。终止宿主场景是进程退出验证，不是断电持久性验证；恢复前已等待原进程实际退出。

## 同一版本的结果

所有结果均来自 `final-lint-corrected` 的冻结副本和唯一构建目录，没有把不同版本的通过记录拼在一起。

| 检查 | 结果 |
| --- | --- |
| 包格式检查、所有 target 编译和构建 | 退出码 0 |
| Harness 测试 | 22 passed，0 failed，0 ignored |
| 所有 target 的严格 Clippy，包含依赖 | 退出码 0，`-D warnings`，无依赖排除 |
| SDK / IPC：7 场景 × 3 次 | 21 passed，退出码 0 |
| 原 Luau：11 场景 × 2 次 | 22 passed，退出码 0 |
| 下游方式实现的公开 `Scenario` 示例 | 1 passed，退出码 0 |
| 错误脚本对照 | 退出码 1，拒绝 `actual_pipeline_effect`，并拒绝不匹配的能力/授权/终态记录 |
| 跳过游标对照 | 退出码 1，拒绝 `saved_cursor_resume_is_exact` |

两项故意制造错误的对照单独记录，不计入成功任务样本。22 项包测试由 6 项库测试、8 项可执行程序测试、4 项原 CLI 测试、1 项扩展测试和 3 项 SDK CLI 测试组成；嵌套启动的样本不重复加入这个数量。

完整命令、环境、起止时间、退出码和日志哈希保存在 `commands.json`。主要 Cargo 检查为：

```sh
cargo +1.95.0 fmt --manifest-path "$SNAPSHOT/crates/cayw-harness/Cargo.toml" -- --check
cargo +1.95.0 check --manifest-path "$SNAPSHOT/Cargo.toml" -p cayw-harness --all-targets --offline --locked
cargo +1.95.0 build --manifest-path "$SNAPSHOT/Cargo.toml" -p cayw-harness --all-targets --offline --locked
cargo +1.95.0 test --manifest-path "$SNAPSHOT/Cargo.toml" -p cayw-harness --offline --locked
cargo +1.95.0 clippy --manifest-path "$SNAPSHOT/Cargo.toml" -p cayw-harness --all-targets --offline --locked -- -D warnings
```

`SNAPSHOT` 指下述冻结源码目录。实际构建使用独立 `CARGO_TARGET_DIR` 和 `TMPDIR`，构建时提供真实的 `CAYW_SOURCE_MANIFEST_SHA256`。普通新构建必须为自己的实际输入生成并保存清单，不能把本记录的哈希套在后续源码上。仅在启动已编译程序时设置这个变量不能改变构建绑定。CLI 用法和所有场景的边界见 [VerifyTool 说明](../../crates/cayw-verify-tool/README.md)。

## 源码与证据

本机证据根目录：

```text
/Volumes/ORICO/cayw-harness-evidence-8abef5923844408b93c92d3d579c4b93/final-lint-corrected
```

主要文件：

- `acceptance.json`：本次范围和结果入口。
- `source-manifest.json`、`snapshot/`：561 个冻结输入及完整 workspace 清单和锁。
- `frozen-closure.json`、`dependency-closure.txt`：实际启用的 14 个本地依赖包、版本、feature 和目标信息。
- `commands.json`、`logs/`：12 条命令的退出码、环境及完整输出；其中两条对照命令预期退出码为 1。
- `executables/`：保留的 Harness 和自定义场景可执行文件及哈希。
- `sdk-positive/`、`luau-positive/`、`custom-scenario/`：原始输入、逐项检查、事件、文件和测量结果。
- `sdk-wrong-script/`、`sdk-skipped-cursor/`：预期失败的原始观察。
- `evidence-integrity.json`：调用方检查脚本的 736 项检查，0 项失败。
- `evidence-seal.json`：397 个保留证据文件的字节数与 SHA-256。

| 对象 | SHA-256 |
| --- | --- |
| 源码清单 | `a74d9ead7ec61b08ebbc5009bc31c9fd204966029701ee97fbf2389ec069f31b` |
| Harness 可执行文件 | `d065296ca284dc68bdbfcb0791354c4f9a2d4d8868091335ddd7757dd54bd814` |
| 自定义场景可执行文件 | `e9267c22496acbb0dba3faf51294bf6c998de03dd801142fce21ffd106d5846a` |
| Cargo.lock | `46e0440091aafa15a621795ccfd2560d2a2ab53baa3fb0c1044bad79be1365b3` |
| 证据清单 | `1401434e243f89ef9571cf2fda95e68cf10ad2853dfd80a93aae1500a98c9918` |

Main 已读取完整测试、check、build、严格 Clippy 和 CLI 输出，并逐一核对 561 个冻结输入、397 个证据文件以及 12 份命令日志的哈希；当前 Harness 的 144 个清单输入与冻结副本一致。Main 另从原始场景报告核对取消、执行前终止和未知副作用恢复的逐项断言。736 项检查的执行者是调用方脚本，不能写成 Harness 自行验证了所有源码。

最初公共副本比较了复制前、复制内和复制后的文件集合及哈希。最终副本只修正 Harness `children.rs` 的严格检查表达式；417 个非 Harness 输入保持相同。没有宣称复制过程是文件系统原子快照。来源绑定始终由调用方提供，二进制只记录构建时给定的哈希，不自行证明源码来源。

## 保留的失败与边界

早期记录没有覆盖：缺少原内容哈希的后续写入；把原取消响应错误要求为已知结果；旧共享构建目录产生的二进制来源不匹配；复制活动 `target/` 造成的清单差异；最后一个 `cmp_owned` 严格检查失败。最终验证修正相应测试或 Harness 表达式，重新绑定源码与二进制，并重跑本表结果。旧副本及失败记录保留，不能当作本次通过证据。

测量保存原始纳秒，包含同步写入证据的时间，没有固定毫秒性能门槛。冷启动是新进程启动，未清空操作系统缓存。debug 构建不能用于承诺性能优势。macOS 内存值来自进程生命周期的峰值 RSS，单位为字节，不是某个任务的分配量或内存增量。

本次 Core 协议为 v3，IPC 帧协议为 v1，两者是不同层的版本。最新共享读取等后续 Host / Cells / Runtime 变更不在这个冻结集合内，需要另行验证。没有真实模型调用；真实服务、真实任务与模型修复成本、Linux 和 Windows 均未验证。Issue #2 仍未完成，所有 Issue 状态保持原样。本机生成目录不保证在其他机器存在，应保留源码与精确命令复跑。
