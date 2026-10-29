# 投影缓存诊断的复跑材料

这里保存三个完成诊断的独立副本的原始插桩补丁和源码身份。按会话计数副本使用修正读数时机后的本机 `diag-bounded-02`；第一次编译失败的副本另存本机，不作为结果来源。产品不提供这些诊断接口。正式性能比较使用没有插桩的 release 二进制。

- `diag-before`：冻结起点 `e738dae1af73bf0ad3fe15f729a4557a2d9f4d24`，加本轮同源基准；记录实际命中分支和耗时阶段。
- `diag-reuse`：普通写入复用后的单槽缓存；记录实际命中分支和耗时阶段。
- `diag-bounded`：有界按会话缓存；只记录实际命中分支和缓存保留的 JSON 字节，没有分段计时。

每个目录含 `store.patch`、`bench.patch` 和 `source-identity.json`。`patches.sha256.json` 绑定这些文件。补丁保留本次原件的路径头。复跑时在单独的新副本应用，不能应用到交付工作树。

## 准备源码

`diag-before` 从上述旧提交的新副本开始，复制本轮 `crates/cayw-core/examples/refactor_persistence_bench.rs`。基准原文件 SHA-256 为 `7efc151061a8552e82650ad03dc5a7efcf9f6a8f5a2f60fbd74438377a86ce97`。

`diag-bounded` 从交付源码的新副本开始，核对 `refactor-source-20261029.json` 中的产品文件哈希。

`diag-reuse` 需要中间的单槽实现。本机原件为 `.local/refactor-20261029/single-slot-source/`，完整文件清单在其 `FILES.sha256.json`。普通写入复用后的原 `store.rs` SHA-256 为 `610e23e252157dd456a8aa31d9d7abf94fdb41963fa2c5f1725137d9d853710f`。本目录另外保留 `ordinary-reuse.patch`（冻结 store → 单槽复用 store）与 `bounded-cache.patch`（单槽 → 最终有界 store），以及 `optimization-patches.json` 中的前后哈希。可在交付源码的新副本中反向应用 `bounded-cache.patch` 重建中间 store，再核对该哈希；这只重建这个文件，不宣称全部中间文件逐字节相同。

在准备好的对应副本根目录，对本目录补丁的绝对路径先执行检查，再应用：

```sh
git apply --check -p1 /absolute/diag-bounded/store.patch
git apply --check -p1 /absolute/diag-bounded/bench.patch
git apply -p1 /absolute/diag-bounded/store.patch
git apply -p1 /absolute/diag-bounded/bench.patch
```

三个目录的路径头均使用 `-p1`。应用后，按各自 `source-identity.json` 核对两个文件的插桩后哈希。不要通过修改补丁或产品源码强行取得相同结果。

## 运行

在副本中编译，并使用独立 target。下面使用新的数据库目录：

```sh
CARGO_TARGET_DIR=/absolute/new-diagnostic-target cargo +1.95.0 build --release --locked -p cayw-core --example refactor_persistence_bench
/absolute/new-diagnostic-target/release/examples/refactor_persistence_bench --size medium --category mixed --sessions 8 --db /absolute/new-evidence/sessions.db --diag-out /absolute/new-evidence/diagnostics.json --out /absolute/new-evidence/bench.json
```

命中次数来自真实分支。workload 包含 `finish_attempt`，setup 和最终读取另记。前两个副本的耗时阶段有父子关系，只合计互不重叠的子阶段。COMMIT 含持久化等待和可能的自动 checkpoint，没有分离文件同步耗时。计数副本不提供阶段耗时，不使用其零值作时间判断。

本轮运行只证明 macOS ARM64、本机 SQLite 与顺序轮转输入。报告中的原始命中次数、结果哈希和源码清单保留本次身份，复跑的新时间不会与其相同。
