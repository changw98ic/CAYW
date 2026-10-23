# 长会话重复读取开销实施计划

> 承接“使用 workflow 把所有功能都实现”的持续授权。使用独立完整副本实现、复核后再接入主树；不提交、推送、发布或调用真实模型。此前诊断的独立验证因上游 403 中断，不将其称为通过，不改变登录、模型或权限以绕过它。

**目标：** 减少 Runtime 检查期间对旧事件的重复解析，并在检查耗时超过定时间隔后停止补跑积压次数；保持现有数据库、协议、授权、执行者和未知结果的校验。

**起点：** `/Volumes/ORICO/cayw-shared-read-source-20261019-final-v3/source`，394 个文件、4,874,007 字节，清单聚合摘要 `712334fa3b8443cfa43c5594e9c991e6a21c387d7bc722a859a3aa77dca6fc82`。固定副本不改，复制时按父目录 `closure.json` 的准确条目核对。根清单与锁、第三方版本不变。

**诊断依据：** `/Volumes/ORICO/cayw-history-investigation-20261019.nsfz4zve`。Main 已重新核对 141 项证据、旧快照 561 项输入以及 8 组原始日志与摘要，记录位于 `/Volumes/ORICO/cayw-history-evidence-check-20261019-wt6vsqiy/verification.json`。各组从相同的 20 轮历史开始；组合实验减少了检查次数及解码次数。单因素耗时波动大；不同类别的计时重叠，不可相加。实验缓存只计算原始键长，不可直接用于生产。

## 必须保持的行为

- Core 协议和存储仍为 v3，IPC 帧仍为 v1，不增加历史兼容路径或诊断环境开关。
- 每次 `SessionStore::events` 都读取当前事务内的 SQL 行及当前 projection，执行现有 wait-log 和完整执行者校验。不能缓存 snapshot、lease、权限、approval、binding 或最终授权结论。
- 原始行任一字节发生变化就重新执行原 `checked_input(serde_json::from_str::<EventRecord>())`；每次命中也检查请求 session、Core 版本、连续 cursor、缺行和当前 projection。
- 缓存是一个 SessionStore 实例及其克隆拥有的私有资源，独立打开的 store 不共享。没有全局静态缓存。缓存返回独立值，调用者修改返回值不能改变缓存。
- 仅保存已经成功读取、通过校验且完成事务的事件；不使用缓存掩盖失败、改变提交前发布规则或恢复未知副作用。
- 保留 `load`、`validate_wait_log`、`check_values`、`encode_readable` 等既有检查，不用删除安全检查换速度。
- 缓存同时有条数及实际保留内存上限。计数包含键、拥有的记录、字符串/Vec 容量、嵌套 JSON 与容器开销；不能只计算编码后的长度。超过单项或总额时执行原正常解码但不保留该项。内存估计必须有明确保守依据并检查算术溢出，不能未经说明乘一个常数就声称已限额。
- 不缓存无法可靠界定占用的事件变体也可以，但必须保留完整正确读取，并在操作次数证据中明确实际覆盖。
- Timer 固定设置 `MissedTickBehavior::Skip`；保留原立即检查、取消分支、绝对截止时间和每个真正调用前的当前身份检查。Context7 核对的 Tokio 文档说明 Skip 跳过漏掉的 tick 后沿原时间基准继续，Burst 默认补跑。

## 实现范围

- 修改 `crates/cayw-core/src/store.rs`：实例拥有缓存、在原 events 路径按实际行字节命中或解码，完成事务后才保留新项。
- 按职责增加 `crates/cayw-core/src/store/event_cache.rs`：所有权、条数/内存预算、独立返回值、必要的占用测量。只增加实际使用的私有类型。
- 修改 `crates/cayw-runtime/src/execution.rs`：原 25ms fence interval 明确 Skip，不增加可选旧行为开关。
- 增加有意义的缓存安全/资源回归，优先复用 Core 现有测试模块和 fixture；不增加生产公共诊断 API。读取明确 mod/import 与 Cargo metadata 给出的测试路径，不用搜索命令替代 CodeGraph。

## 验证

1. 在新副本保留未修复的操作次数或定向行为对照，再修改并记录实际差异。
2. 测试缓存预热后篡改协议、session/cursor、JSON 深度/节点/字节，删改事件或改变 projection，均走当前校验并拒绝错误；合法控制组仍可读。
3. 独立打开 store 恢复并替换 lease 后，旧 worker 仍被拒绝。修改返回值不污染后续读取。多会话、并发 store 克隆、超限事件、淘汰与内存计数溢出都有实际检查。
4. 保留 module 卸载冻结、StrongFlow、unknown、wait 配对和取消回归；不把减少定时次数当成省略真正执行前的检查。
5. 在完全相同起始数据库的独立副本中做有界离线 Runtime 对照，包含原 Request authority sidecar。保留源码/构建/二进制/配置/命令/日志与操作次数，不要求跨机器固定毫秒数。旧 121 次吞吐失败继续保留，未重新验收前不改写。
6. Rust1.95.0、macOS arm64、独立 ORICO target/tmp、offline/locked；格式、Core/Runtime 全 target/feature check/test/doc 和严格 Clippy。复制完整 20 包及原锁，不裁剪或升级。
7. 独立复核精确固定输入、安全和资源边界；Main 核对真实差异后再接主树，随后验证与 Device/Modules 的新组合。副本通过不能替代主树组合。

## 交回与未验范围

交回新源码、前后清单、实际最小差异、原始失败与修复后日志、命令/退出码/计数、占用上限依据、独立复核和可复跑入口。原固定副本与证据不改。Windows/Linux、真实 Provider、最新浏览器组合和整个 Issue 验收仍需各自证据。
