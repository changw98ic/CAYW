# Core 四项边界修复验收

2026-10-19，四项已确认的 Core 问题完成修复及独立复核。验证环境为 macOS ARM64、Darwin 27.0.0、Rust 1.95.0；Windows、Linux 和最新 Runtime 组合不在本记录的通过范围。本文记录局部验收，不关闭 Issue #1、#6 或 #7。

## 问题与结果

| 问题 | 修复后的可观察结果 |
| --- | --- |
| v2 迁移只核对部分来源，投影可能与日志中的终态、授权或外部来源不一致 | 按完整日志重建执行、能力和审批事实，核对完整投影和合法转换；拒绝缺失、重复及不一致事实，保留合法首次 `CapabilityStarted` 的旧记录 |
| v1/v2 可提交超过当前读取限制的 JSON | 写入前检查字节、节点和深度，随后做类型解码；迁移成功的数据可通过实际 snapshot、events、list、recover 和 reopen 读取 |
| `ProtocolEnvelope::new` 拒绝编号时，释放深层 owned JSON 可能导致栈溢出 | 公开 `ProtocolPayload`，先检查并迭代释放 JSON 叶子，再验证编号；自定义有限包装器显式转发两种 visitor |
| append-only 触发器未阻止相同键的 `INSERT OR REPLACE` | 新库、已有 v3 和迁移库均安装插入冲突检查；拒绝替换，允许正常追加。复现需要直接写 SQLite，不代表存在公开事件替换 API |

`ProtocolEnvelope::new` 要求 `T: ProtocolPayload`，`decode` 要求 `T: DeserializeOwned + ProtocolPayload`。CoreRequest、CoreResponse、Value、标量、Option、Vec、Box 和 BTreeMap 已使用这一份契约；没有保留旧的内部 JsonValues 契约。检查限制针对 JSON 叶子，不承诺允许任意递归的 Rust 包装类型。wire 字段和 Core 协议版本没有因本次修复改变。

## 验证

作者完整 Core 全目标测试为 106 passed、0 failed。保持原始字节的两组复现，在原始源码副本为 3 passed、5 failed，修复后为 8 passed、0 failed；不同源码使用独立构建目录，避免缓存混用。

独立验证者又在冻结副本上运行：

| 范围 | 结果 |
| --- | --- |
| 未修改的原始复现 | 8 passed、0 failed |
| 现有定向回归 | 24 passed、0 failed |
| 新增独立边界检查 | 5 passed、0 failed |
| Core 全目标编译 | 退出码 0 |

独立检查覆盖 29 种 v2 状态/来源/顺序变化、9 种审批变化及 4 种恢复变化；合法 active、settled、recovered、reconciled 和首次 Started 对照可迁移。v1/v2 × event/projection × depth33/nodes4097/string65537 的 12 个非法输入被拒绝，合法边界可读。迁移重写后注入 metadata 写入失败，原数据库字节、版本和触发器完整回滚。25,000 层 JSON 在 256 KiB 线程栈上，针对空、空白、超长及合法编号均能拒绝并正常返回，没有进程 abort。

作者在 live 根目录执行，退出码均为 0：

```sh
cargo +1.95.0 fmt --manifest-path /Volumes/ORICO/CAYW/Cargo.toml -p cayw-core -- --check
cargo +1.95.0 check --manifest-path /Volumes/ORICO/CAYW/Cargo.toml -p cayw-core --all-targets --offline --locked
cargo +1.95.0 test --manifest-path /Volumes/ORICO/CAYW/Cargo.toml -p cayw-core --all-targets --offline --locked
cargo +1.95.0 clippy --manifest-path /Volumes/ORICO/CAYW/Cargo.toml -p cayw-core --all-targets --offline --locked -- -D warnings
```

Cargo 实际位于 `/Users/chengwen/.cargo/bin/cargo`。独立复核使用其冻结副本的 manifest，精确命令、环境、时间、退出码和日志路径保存在下列 JSON 中。

## 源码与原始证据

独立验证的生产文件集合包括 Core、Agent、AI 的源码和 crate 清单，共 24 个文件。SHA-256 为：

```text
6fd161515355b2ccc49bd0b45b2c48edcf6f2912afad8e50542443c257a2b2b9
```

计算方式为相对路径到文件 SHA-256 的映射，按键排序后编码为无额外空白、无末尾换行的 UTF-8 JSON，再计算 SHA-256。Main 已按这一清单逐一核对当前 24 个生产文件，全部一致。不要混用作者较早清单中不同收录范围的哈希。

冻结源码、测试和根清单/锁文件的完整集合哈希为：

```text
99999845d991617e17f88c6138df78415bd91d79b9579f16b7806e50e4f8ec7e
```

根 Cargo.lock SHA-256：

```text
46e0440091aafa15a621795ccfd2560d2a2ab53baa3fb0c1044bad79be1365b3
```

本机证据位置：

- 原始问题和未修改复现：`/Volumes/ORICO/cayw-core-independent-78xn_9te/`。
- 作者的命令和红绿结果：`/Volumes/ORICO/cayw-core-repairs-wrnh7ow9/evidence/final-evidence.json`；后续整理的清单为同目录 `final-results.json`、`source-closure-final.json`。
- 独立结果：`/Volumes/ORICO/cayw-core-final-verification-86qhqhuk/evidence/final-results.json`。
- 独立精确文件及哈希：同目录 `source-closure-final.json`。
- 独立原始复现输出：同目录 `original-probes-rerun.log`。
- 现有回归原始输出：同目录 `final-scope.log`。
- 补充检查修正后输出：同目录 `final-scope-corrected.log`。

独立验证最初缺少辅助 workspace target 文件，未执行测试；补齐已声明文件后重跑。一个新增探针还误以为 SQLite 同名表会阻止创建触发器；实际触发器存在且替换已被阻止。保留该次失败和诊断，只修正新增探针后 5 项通过。`final-scope.log` 因这个探针含一次失败，不能把整条命令写成退出码 0；其中 24 项既有回归均通过。

这些证据保留在本机，不保证其他机器可直接访问；复跑需要源码、相同工具链和已缓存依赖。没有真实模型、付费服务、Git 写入或原始证据修改。完整产品、其余 Core 行为和最新 Runtime 集成仍需各自验收。
