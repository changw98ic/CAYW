# External Module 的 Device 生命周期迁移

> 承接“使用 workflow 把所有功能都实现”的持续授权和 `2026-10-19-device-interactive-process.md` 第二阶段。一个作者在完整独立副本实施，另一位审查者验证；Main 核对差异后才接主树。无需再次询问执行方式，不提交、推送、发布或调用真实模型。

**目标：** External Module 的所有实际进程与管道都由 Device 拥有；加载、调用、卸载和清理各有明确状态。进程尚未清理时，名称、能力和实例资源仍被占用。Native 模块现有同步重入和原实例身份核对继续工作。

**起点：** 共享读取最终 v3 固定源码，加已经独立复核的 Device 六文件。Device 源码为 `/Volumes/ORICO/cayw-device-shared-admission-20261019-b86c2e70`，作者清单摘要 `ffc79825e25fd31d57db7eae301128df4861e4d3ec0f3e4cf9f2b2da534c3edb`；独立记录 `/Volumes/ORICO/cayw-device-admission-independent-review-20261019-c5b69d42/review-seal.json` 摘要 `1e98a6465ad29707d04cf841aa43b8bc8e841e39bd1ed4e51c6e7999c66c94e7`。Main 已将六文件接入主树，完整工作区依赖锁验证另存 `/Volumes/ORICO/cayw-device-live-integration-20261019-7kb2pq7x`，结果以实际记录为准。长会话优化由另一工作流单独实现，不纳入本副本。

## 必须保持的边界

- Core v3、IPC 帧 v1、模块线协议 v1 保持各自含义；不创建旧产品接口兼容实现。
- 无 CodeGraph 索引，已确认不重复查询。按固定清单、已知 mod/import 与 Cargo metadata 读取，不用文本或文件搜索替代。
- 20 包完整 workspace，原第三方版本、来源、checksum 和 feature 不升级。根锁仅为新增本地依赖更新边，不裁锁。
- 不复制用户配置、凭据或代理会话原文；不修改原固定源码、证据、主树和第三方缓存。
- 进程组不等于沙箱；Windows Job Objects、PTY、setsid 逃逸及宿主进程崩溃不由本轮证明。

## 1. 宿主显式启动输入

在 `crates/cayw-modules/src/external.rs` 定义唯一 `ExternalLaunch`。它拥有宿主提供的独立 `cayw_device::ProcessRunner`、打开的 `std::fs::File` 工作目录、非 Clone 的 `ResourcePermit`、显式总生命周期 `Duration`。不能从调用参数、模块回应或最新 Session 推导这些值。

新入口形状：

```rust
ExternalModule::spawn(config, launch, load_cancel).await
ModuleRegistry::load_external(config, launch, context, load_cancel).await
Runtime::load_external(config, launch, context, load_cancel).await
```

具体字段名由作者在交付中固定，但三层都必须实际消费同一输入。删除 `ExternalConfig::working_directory: Option<PathBuf>` 和原隐式 cwd 路径；总生命周期不从单次调用 timeout 推算。宿主为每个实例单独创建 runner Scope 与服务资源凭证。Modules 使用 Device 交互入口取得唯一 stdin/stdout 和可重复等待的 control，永远不持裸 Child/PID/OS pipe。

## 2. 有界通信与调用状态

- 队列有明确项数上限；先 `try_reserve` 槽位，再用受限 `std::io::Write` 与 `serde_json::to_writer` 构造帧。不要先 `to_vec`、再检查长度。换行与 request_id 的最大宽度都算入最终限长。
- 协议 actor 按序选择请求，并 checked 分配不复用的 request_id。发送、取消和已派发状态在一个可审查的同步边界决定，不能在“确认未发送”后又开始写。
- 未取得槽位、无效参数、超限帧、排队中取消等已知未派发情况使用明确的已知失败，不能一概标成未知。
- 一个请求开始写入后，调用取消、Future Drop、timeout、协议错误或进程退出都关闭原实例；可能的副作用保留 unknown。剩余队列请求不写入。其他独立实例不受影响。
- 单次成功调用返回时的 guard 只结束该调用，不能停止长期实例。加载取消与实例 stop token 分开，成功交付后释放加载 guard。
- stdout 分帧有界，处理换行、预读与多 chunk，空闲时意外输出终止实例。总 stdout+stderr 预算完全使用 Device 在原管道读取处的计数，不另建 budget 或 stderr reader。

Context7 已核对：Tokio `Sender::try_reserve` 不等待且返回槽位凭证，丢弃归还容量；Serde JSON `to_writer` 将序列化流写入 `io::Write` 并传播写入错误。来源分别为 docs.rs/tokio/latest/tokio/sync/mpsc/struct.Sender.html 和 docs.rs/serde_json/latest/serde_json/fn.to_writer.html。

## 3. 加载、卸载与清理归属

- Registry 在任何 await/spawn 前校验完整 manifest/context，并原子预留 registry_id、generation、名字和全部能力；状态为 Loading。重复或冲突加载不先产生一个未登记进程。
- Loading、Loaded、Retiring 都保留原身份与能力冲突占用。unload 或加载 Future Drop 后不能迟到发布 Loaded。
- `unload(name)` 对 Native 保持同步取消、移除、锁外 shutdown，支持 Native 在 call/shutdown 内同步查看 registry、卸载自身和加载替代。
- `unload(name)` 对 External 同步关闭准入并进入 Retiring。新增按完整 `ModuleDescription` 等待原实例清理的 async 入口；成功后才精确释放原项。等待者 Drop 不丢所有权，多个等待者可再等；旧实例的完成不能删除新 generation。
- 启动失败或加载取消时，已经实际启动的 Device Scope 仍有稳定所有者。不能因加载调用返回 Err 就丢失 Retiring 记录。
- 协议 actor JoinHandle 由实例内稳定 owner 持有。多个等待者轮流等待同一真实 join，Drop 某次等待不能 detach 或 abort 后伪称完成；避免 actor 捕获其 owner 形成 Arc 自引用。
- 清理成功必须同时确认 actor 已结束、原 Device control.wait / runner.drain 的实际完成。无法确认则保留 Retiring、凭证和失败，不自动重开或重放。
- 同步 check_available 使用 Device 原 control.is_running，保留执行器尚未调度时观察已退出 leader 的能力。
- Registry Drop/执行 Tokio 被销毁时仍请求 Device 停止，真实 OS 清理由 Device 的监督及 join owner 继续拥有；不依赖新建 Tokio runtime 代回收旧 Child。

## 4. Runtime 消费迁移

准确文件：`crates/cayw-runtime/src/modules.rs` 与 metadata 声明的 `tests/module_execution.rs`。增加宿主启动输入，并提供按原完整描述等待 External 清理的宿主入口。已有 `unload_module` 必须先持久化绑定冻结/StrongFlow checkpoint，再请求停止；不能跨 await 持 coordination 锁，也不能靠新 generation 自动解冻旧 Session。

保留 `ModuleCatalog` 捕获原描述、`call_exact` 在首次 poll 原子核 registry/generation/完整 manifest、Host 授权和 Core/Cells 原调用配对。Native 的 `tests/module_lifecycle.rs`、`tests/optional.rs` 等行为不得改弱。

本阶段的服务实例资源与 Runtime task 是不同归属。不能临时将长期服务移入最新任务。涉及任务名额是否应等原已派发模块调用收敛的问题，必须在实际 Runtime 集成场景验证；做不到时明确留作未验范围，不能从 Device 或 Modules 库测试外推。

## 5. 删除旧执行路径与依赖迁移

修改 Modules `src/lib.rs`、`external.rs`、`registry.rs`、`Cargo.toml` 及必要私有实现文件；测试入口为 `tests/external.rs`、`tests/registry.rs`、`tests/fixtures/external.rs`，消费方按编译诊断和明确模块引用迁移。

删除旧 `tokio::process::Child/Command`、ProcessGuard、ProcessWatcher、SIGCHLD 订阅、stderr task、独立 OutputBudget 和无 owner 的 spawn。Modules 增加本地 cayw-device 依赖；tokio 生产 process/signal/io-util 若已无实际用途则移除，nix 只作 Unix dev-dependency。Runtime 只有实际使用 Device 类型或测试 fixture 才增加本地依赖。不得保留旧 spawn 签名的转接实现。

## 6. 验收和交付

用真实离线子进程与 marker/门闩验证，超时只防挂：

1. 握手失败/超时、加载取消/Drop、重复加载与能力冲突没有未登记执行者；已启动但待清理时名称保留。
2. A 已派发、B 排队；取消 B 后 A 和下一合法调用正常。取消/丢弃 A 后关闭原实例，B 不写入，另一实例继续。
3. 队列满与超限序列化在派发前失败；帧与缓存有实际内存边界；checked request_id 不复用。
4. External Retiring 保留名称/能力；多等待者、丢弃等待、重等、晚到的旧清理不影响替代实例。
5. 同步退出观察、后代保管道、stdout 暂停与 stderr 超预算、总生命周期 timeout、Tokio 销毁后原 Device 仍实际清理。
6. Native 同步重入、name-call 首次 poll、call_exact 的旧描述/foreign registry/schema/permission/Drop 回归保持；StrongFlow 卸载冻结与显式 Fork 保持。
7. Rust1.95/macOS arm64/独立 ORICO target/tmp/offline/locked；fmt、完整 workspace check、Modules 全 target/feature 与 no-default-features 测试、Runtime 模块/冻结/清理相关测试、严格 Clippy。计数以原始父级测试日志为准，不加子摘要。
8. 保存完整前后源码清单、最小 patch、第三方依赖不升级证明、真实命令/环境/退出码/原始失败与成功日志/构建产物摘要；独立审查挑战生命周期、未知结果和资源归属。

不把本地离线测试写成全平台、真实 Provider 或 Issue12 整项完成。Main 集成后再验证与长会话优化的组合。
