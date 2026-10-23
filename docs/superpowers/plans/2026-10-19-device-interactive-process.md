# Device 交互进程与模块清理实施计划

> 承接用户“使用 workflow 把所有功能都实现”的授权。先完成真实 Device 交互执行，再迁移 External Module；不提交、推送、发布或调用真实模型。共享读取仍有唯一 writer，第一阶段只改新的独立源码副本。

**目标：** External Module 的实际子进程、进程组、三条管道、资源凭证与监督线程都由 Device 持有；模块只保留有界通信端点。停止请求与已确认清理分开，等待者取消不能丢失清理归属。

**方式：** 复用现有 ProcessRunner Scope、启动前登记、SupervisorResources、真实 join owner 和 Unix 执行路径。先在独立的 Device 包副本实现并独立验证，再在共享读取交回固定源码后接入主树；随后迁移 Modules 并协调 Runtime 消费方。

## 持续约束

- CodeGraph 已确认 CAYW 无索引且不应重复查询。只按已知路径、明确 mod/import 和 Cargo metadata 的结构化目标读取；不用文本或文件搜索替代。
- 第一阶段不改主树 Device/Modules、Host/Cells/Runtime、根清单和锁文件。独立副本记录每个输入的原始摘要，只接受该固定基线。
- 不复制用户配置、旧项目代码或许可证；现有 Apache-2.0 与普通兼容依赖不变。
- Device 必须真正拥有进程，不能用 own_cleanup 包裹 Modules 的旧 Child 冒充统一监督。没有两套长期执行路径或旧 API 兼容实现。
- 取消只请求停止；只有真实子进程回收、受管进程组清理、管道处理、监督线程 join 和资源清理完成后才可释放凭证。
- 无法确认的清理保留失败和资源，关闭准入，不把超时或输出关闭视为清理成功。
- 明确 Windows Job Objects、PTY、宿主 OS 进程崩溃与 setsid 逃逸边界；macOS 测试不外推其他平台。

## 第一阶段：Device 的真实交互入口

准确起点：`crates/cayw-device/src/lib.rs`、`src/process.rs`、`src/scheduler.rs`、`Cargo.toml`。测试入口由 Cargo metadata 提供；已有目标包括 process、scheduler、cleanup_completion、independent_early_call、independent_last_owner_join、independent_thread_exit 和 fixtures/process.rs。

拟新增接口如下。实现者可根据现有内部结构细化私有类型，但每个公开能力必须有实际执行与测试，不能只声明类型。

```rust
pub struct InteractiveLimits {
    pub max_write_bytes: usize,
    pub max_buffered_stdout_bytes: usize,
}

pub struct InteractiveProcess {
    pub stdin: ManagedStdin,
    pub stdout: ManagedStdout,
    pub control: ProcessControl,
}

#[derive(Clone, Copy, Debug)]
pub struct ProcessExit {
    pub status: std::process::ExitStatus,
    pub termination: TerminationReason,
    pub output_complete: bool,
}

impl ProcessRunner {
    pub async fn start_interactive_in_directory_with_permit(
        &self,
        spec: DirectoryProcessSpec,
        limits: InteractiveLimits,
        cancellation: CancellationToken,
        permit: ResourcePermit,
    ) -> Result<InteractiveProcess, DeviceError>;
}

impl ManagedStdin {
    pub async fn write_all(&mut self, bytes: &[u8]) -> Result<(), DeviceError>;
}

impl ManagedStdout {
    pub async fn recv(&mut self) -> Result<Option<Vec<u8>>, DeviceError>;
}

impl ProcessControl {
    pub fn is_running(&self) -> Result<bool, DeviceError>;
    pub fn close(&self);
    pub async fn wait(&self) -> Result<ProcessExit, std::sync::Arc<DeviceError>>;
}
```

执行要求：

1. 保留原 run 接口及全部原测试语义。新增交互模式共用实际启动、身份围栏、退出观察和清理路径；不要复制另一套 PID 信号/回收实现。
2. Child、PGID、OS 管道及凭证留在监督线程。stdin/stdout 端点不克隆，control 可克隆，仅持有有界缓冲、通知和完成状态，不向 Modules 暴露裸 PID。
3. stdin 至多一个待写操作。先校验长度再复制；非阻塞分段写完才确认。写 Future 中途丢弃或端点丢失请求停止原进程，避免半帧影响下一次调用。
4. stdout 缓冲满时停止继续读取 stdout，但继续观察取消、截止时间、stderr 和 leader 退出。监督线程不能阻塞等通道空间。停止清理不等待消费者。
5. 在原始 pipe 读取处统计 stdout+stderr 的整个生命周期字节，包含握手、换行、预读和 idle 输出，不按调用重置。边界和总数用 checked 操作；stderr I/O 失败不能静默吞掉。
6. 启动 Future 返回端点时明确转移取消 guard；丢弃未完成启动仍走原清理。总生命周期 timeout 显式提供，不能用 Modules 单次调用时间推算。
7. cwd 是宿主持有的打开目录描述符，复用 fchdir 保护；改名或路径替换后仍执行在原目录，不重新按路径打开。
8. 同步 is_running 使用监督线程共同遵守的短锁与未回收身份围栏，WNOHANG/WNOWAIT 查询；回收前撤销可用性，ECHILD 撤销按数字发信号的资格。锁内不阻塞 wait、join 或 group 清理。
9. wait 可重复、多等待者、取消后再等；结果只在真正 join 和清理完成后发布。drain 与 wait 等待同一完成事实，失败不能释放 reservation。

验证用真实子进程和明确门闩/marker，超时只防挂：

- 多次 echo、argv/env 原样、打开 cwd 后改名/替换、写入限长在入队前拒绝。
- 不读 stdin、stdout 消费暂停、stderr 不断输出；缓存有界，取消/超时/预算仍收敛；精确输出预算与多一字节、跨往返总预算。
- leader 退出且后代持有/关闭管道；close、端点 Drop、启动 Future Drop、runner close 都清理原 group；permit 在真实 wait/drain 结束前保留。
- 复用已有 signal/观察/线程启动/join/panic/TLS 退出故障注入；持续故障不能伪报成功，已回收身份不能再信号旧 PGID。
- 原早退清理、last-owner、真实 join、current-thread Tokio 非阻塞回归。

交回：实现差异、固定输入及前后摘要、实际命令/退出码/原始日志、独立复核结果。副本通过不等于主树已接入。

## 第二阶段：External Module 迁移

在第一阶段独立通过、共享读取已交回完整冻结依赖后开始。准确文件为 Modules src/lib.rs、external.rs、registry.rs、Cargo.toml，以及 metadata 声明的 tests/external.rs、registry.rs、fixtures/external.rs、相关 example。消费迁移按编译诊断的准确路径协调原 owner。

- 新 External 启动接收宿主提供的独立 ProcessRunner、打开 cwd、ResourcePermit 和显式总生命周期 timeout。移除可选路径 cwd 的旧隐式执行入口。
- 一个实例使用独立 runner/Scope 与生命周期凭证。跨 Session 服务进程的归属由宿主明确登记，不能临时归给最新调用任务；每调用原 Core/Cells/Request 事实另行保留。
- load 取消、call 取消和实例停止分开。单次成功返回时的 CancelOnDrop 不能杀长期进程。
- 排队但未写入的调用取消只影响自身；已经开始写入后的取消/超时/协议错误终止原实例，已发送调用保留 unknown，其余排队调用不得再写入，其他实例继续可用。
- 先取得有界队列槽位，再用有限 serializer 构造线帧；checked 分配 request_id，不复用。队列满明确未派发失败，不能无限等待并持有大帧。
- Registry 在 await 启动前预留完整实例身份、名字和能力；Loading、Loaded、Retiring 都有归属，关闭后不能迟到发布 Loaded。
- 同步 unload 只请求停止，External 进入 Retiring，直到 await 原实例 drain 成功才释放名字与能力。等待 Future Drop 后仍可再等；旧清理或旧结果不能影响新 generation。
- 保留 Native 同步卸载重入及 call_exact 的 registry/generation/完整 manifest 核对。Native 无需伪造 Device 清理证明。
- External 协议 actor 的 JoinHandle 有稳定 owner，等待多个消费者时不丢失。没有 lifetime 自引用。删除旧 Child、ProcessGuard、ProcessWatcher、SIGCHLD、stderr task 和独立输出预算实现。
- Modules 增加本地 cayw-device 依赖；Main 在根锁 cayw-modules 条目增加该依赖，不升级包。生产 tokio process/signal/io-util 可移除；nix 测试依赖移至 Unix dev-dependencies 并保留锁中原版本。

验收：握手失败/取消/drop/重复加载不遗留归属；A 已发 B 排队的独立取消；实例隔离；retiring 名称保留；多等待者与等待 Drop；即时退出同步检查；Tokio 销毁后 Device 仍清理；原 registry/native/strongflow 测试与 no-default-features。Runtime 任务名额释放的集成必须另有实际证据，不在库通过时提前宣布完成。

## 第三阶段：主树集成与证据

- Main 核对副本基线与主树输入；若他人已修改，先合并再验证，不覆盖。
- 按包运行 fmt、全 target/feature check/test/严格 Clippy，Rust1.95、offline、locked，独立 ORICO 构建目录。
- 协调消费方迁移后运行最新全 workspace 的同组检查；保留所有首次失败，不用此前冻结通过替代当前结果。
- 冻结完整依赖并独立复核，记录源码、构建、平台、输入、身份、权限、预算和实际清理。
- 未验证平台与 Issue4/12 其余缺口保持开放；不提交、推送、关闭 Issue 或发布。
