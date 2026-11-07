# cayw-content

Device 持有的本地内容库，以及可选加密备份对象。这个 crate 不依赖 Core、Device、Runtime 或 Server，也不建立网络连接。当前 #18 本机交付由 Runtime 接入同一个本地库；可选加密、备份和远端 trait 不属于此次本机主线的验收范围。

## 可调用接口

- `Id::new(value)`：1–64 个 ASCII 字母、数字、`_`、`-`，用于记录、Device 和 key id。JSON 反序列化也运行相同检查；记录名不能是文件路径。
- `ContentStore::open(absolute_directory, device_id, limits)`：创建或打开本地库；设备标识和限额写入持久状态，重新打开时必须一致。路径的每一层都拒绝软链接。macOS 的 `/tmp` 通常是软链接，请使用 `std::env::temp_dir().canonicalize()?` 得到父目录，再创建新的库目录。
- `put(&name, expected, bytes) -> Result<Record>`：`ExpectedRevision::Absent` 表示新建；更新使用已读记录的 `record.expected()`。同时核对 `version` 和 `revision`，更新成功后本地版本加一。相同版本号、不同分支也冲突。
- `read(&name) -> Result<Option<(Record, Vec<u8>)>>`：本地读取并校验内容 SHA-256，不依赖备份端或网络。
- `try_read` / `try_put`：与对应阻塞操作共用锁、完整性检查、版本比较和提交；锁忙时返回 `Error::Busy`，不修改内容。取得锁后同步完成；取消等待由宿主处理。
- macOS `open_private(directory, device_id, limits, expected_root)`：在同一个不跟随符号链接的目录句柄上检查组/其他用户写权限及可选原 `(device, inode)`，再初始化库。已有绑定的根必须存在；身份或权限错误在创建库文件前返回。未绑定的新路径仍可按私有权限创建。
- macOS `open_private_amending_limits(directory, device_id, limits, expected_root)`：受信宿主的显式配额调整入口。仍核对原 `(device, inode)`，保留设备、记录、key/nonce 和对象；只有全部保留记录与对象仍满足新限额时才写入新限额，否则返回配额错误且不修改状态。旧的 `ContentStore` 句柄在磁盘限额变化后继续以 `StoreConfigurationMismatch` 失败，不会静默使用旧限额。
- `root_metadata()`：读取已持目录句柄的设备、inode 和当前权限，不按外部路径重开。`validate_configuration()` 在同一目录句柄和内容锁内核对持久设备与配额，不修改状态，也不重新解析配置目录。`ContentHash::from_hex` 解析固定的小写 64 位摘要，`Limits::validate` 可在打开文件前验证限额。
- `KeyHandle::from_bytes(key_id, [u8; 32])`：宿主显式提供密钥，每次加密、解密、上传都要传入。该类型没有序列化或克隆接口；`Debug` 只输出 id，Drop 清理自身的密钥副本。调用方负责清理输入副本及外部密钥生命周期。
- `register_key(&handle)` / `revoke_key(&key_id)`：只保存允许/永久撤销的 id，不保存密钥或自动找密钥。未知 id 拒绝；撤销后不能重新注册相同 id。撤销检查用于导出、恢复和上传；普通本地读取仍可用。
- `export_snapshot(&name, &handle) -> Result<EncryptedSnapshot>`：导出当前本地版本。随机 24 字节 nonce，每个导出对象唯一；本地持久保存已用 nonce 标识。
- `restore(&snapshot, &handle, expected_local, mode) -> Result<Record>`：显式提交恢复。`RestoreMode::Continue` 只接受空位置或当前分支的直接下一版；已导入来源的直接下一版也可续接。跳过版本、旧备份、不同分支均拒绝。恢复写入 Device 自己的新本地版本，并保存来源 Device、版本、分支、记录指纹和 manifest 指纹。
- `RestoreMode::Fork { name }`：用户明确恢复旧内容或不同分支时，必须指定一个不存在的新记录名，并传 `ExpectedRevision::Absent`。不能借此覆盖已有记录。
- `EncryptedSnapshot::{to_bytes, from_bytes}`：导出/有界导入 JSON 对象；先检查编码长度，再解析、检查协议、记录、内容长度和密文 hash。`Manifest::upload_id()` 将元数据和密文 hash 绑定为稳定上传 id。
- `upload_snapshot(&snapshot, &handle, &remote, expected_head)`：使用已导出的同一个 snapshot 重试；stage → 读回并校验 → commit。对本地记录和版本没有修改。`fetch_snapshot(&remote, upload_id)` 只获取并验证对象，恢复是后续单独的显式操作。

`Record` 包含 `name/version/content_hash/content_len/device_id/lineage/parent_revision/restored_from/revision`。`revision` 是带格式标识的元数据 SHA-256；`lineage` 的随机起点让独立创建的相同内容也属于不同分支。这里没有仅按整数版本强制覆盖的入口。

## 本地存储与故障边界

库目录包含 `.lock`、`state.json` 和 `objects/<sha256>`。所有进程都对独立打开的 `.lock` 文件获取排他锁，读状态、核对预期版本、检查限额和提交处于同一次锁定中。锁是操作系统的建议锁；其他程序必须遵守它。锁句柄在返回或异常退出后释放。

先写固定的临时内容文件并 `sync_all`，再移动到内容寻址位置并同步相关目录；随后写临时状态、同步、原子替换 `state.json` 并同步目录。权威记录只在状态提交时改变；半写入临时文件不能成为权威。成功返回前完成同步。替换状态后目录同步失败会返回 `CommitOutcomeUnknown`，调用方必须重新读取实际状态，不能假设失败就没有提交。

`.lock` 同时持有初始化标记。已有库丢失/损坏 `state.json` 会报错，避免静默清空记录或撤销表。首次创建中断后若已有内容对象却没有状态，也拒绝自动初始化。已提交内容对象不会因远端故障而被替换。

路径操作使用 `cap-std` 目录句柄和 `cap-fs-ext` 的 nofollow 打开方式；所有生成的相对文件名固定或来自验证过的 hash。拒绝根目录/祖先、对象、状态、临时文件和锁文件的软链接，不只做字符串前缀检查。Unix 新目录按 `0700`、新文件按 `0600` 创建；已有目录权限不自动修改。当前首发只验收 macOS。

宿主必须提供可信、私有、稳定的本地目录。本库不抵抗可直接修改目录的同一用户、管理员、恶意硬链接、整库回滚/删除、损坏硬盘或忽略文件锁的程序。SHA-256 检查内容一致性，不能对这类本机攻击提供真实性保证。不要把底层锁、替换和同步语义未验证的网络文件系统当作已支持平台。

`Limits` 默认限制单条内容 16 MiB、所有保留内容对象合计 256 MiB、1024 条记录、16384 个 key/nonce 表项。构造时限制配置范围，持久状态编码最多 32 MiB。总内容限额按去重后的不可变对象字节计费，包括旧版本和中断后留下的孤立对象；不会无限替换旧对象后漏计配额。临时内容最多另占单条上限，临时状态另占 32 MiB；文件系统块、目录项、锁和日志空间不属于内容字节限额。没有自动垃圾回收、删除记录或历史回滚接口；配额调整只经显式 `open_private_amending_limits` 进行。

## 加密和恢复模型

默认开启 `encryption` feature；`default-features = false` 可只构建本地内容库。

加密使用 RustCrypto `chacha20poly1305` 0.10.1 的 XChaCha20Poly1305，带 16 字节认证标签。AEAD 附加认证数据包含协议、key id、完整 Record、nonce 和密文长度，因此 Device、记录名、版本、内容 hash、分支或来源信息被替换时解密失败。密文另有 SHA-256 校验，整个 manifest 决定上传 id。元数据是公开的：远端可见记录标识、版本、长度及内容 hash；本实现不隐藏访问模式，也不提供抗离线猜测的内容 hash。

key id 是宿主命名的外部密钥身份；宿主必须对同一个秘密使用同一个 id，禁止回收 id 或为同一个秘密注册不同别名。注册只保存 id。输入 key handle 不匹配 manifest、未知 key id、撤销 key id、错误密钥、损坏密文及不支持的协议都拒绝，不进行 fallback。导出 nonce 和已导入 nonce 均持久记账；导出表将 nonce 绑定到确切的 upload id，替换对象后不能沿用旧 nonce 上传或恢复。已用 nonce 的新导出/重复恢复拒绝；成功恢复后重放不能再创建另一个 Fork。

撤销只阻止遵守本地 registry 的后续操作。其他 Device 的撤销传播、外部密钥销毁、密钥分发和用户确认由宿主负责；不能撤回已经交付给对方的明文或密钥。示例密钥只在内存中存在，不适合保留后等待恢复。

恢复不能让备份端自动成为新权威：先验证对象、认证解密，再在本地锁内重新核对 expected revision、分支和 nonce；最后以本地状态原子提交权威引用及已消费 nonce。失败的认证、冲突或配额检查不会修改权威版本。备份端给出旧 head 也只能拿到旧数据，不能跳过本地恢复规则。

## 宿主远端接口

`RemoteObjectStore` 由宿主实现，包含 `stage/read_staged/commit/read_committed/head`。本库没有默认网络客户端、服务端、URL、凭据发现或后台同步线程。

宿主实现必须满足 trait 的持久性和并发契约：

1. 以 `Manifest::upload_id()` 作为稳定上传 id，部分 stage 不可见于 committed 读取；相同 id 重试可补齐/替换未完成 stage。
2. commit 重新检查对象与 manifest，按 `(device_id, record_name)` 对调用方提供的 expected head 原子比较并替换。
3. 已提交 id 是永久幂等回执。同一 id 再提交返回成功，不能把较新的 head 倒退到旧回执；同 id 不同内容拒绝。
4. 有界读取，错误/中断明确返回；清理过期 stage、传输认证、权限和服务端持久化由宿主管理。

库在上传前及最后 commit 边界检查本地撤销状态。stage/read 在锁外运行；最终 commit 在本地排他锁内运行，以便与撤销排出先后。宿主必须限制 commit 耗时，且不能在 commit 回调中再次进入同一个 ContentStore，否则会等待自己持有的锁。没有内置异步取消或网络超时。远端服务不遵守 trait 时无法保证它的 head CAS 或回执正确，但获取/恢复仍验证对象，远端仍不是本地权威。

## 复跑与实际验证

在已更新 workspace 依赖锁文件后，可从仓库运行：

```sh
/Users/chengwen/.cargo/bin/cargo +1.95.0 test -p cayw-content --locked
/Users/chengwen/.cargo/bin/cargo +1.95.0 test -p cayw-content --no-default-features --locked
/Users/chengwen/.cargo/bin/cargo +1.95.0 clippy -p cayw-content --all-targets --locked -- -D warnings
/Users/chengwen/.cargo/bin/cargo +1.95.0 clippy -p cayw-content --all-targets --no-default-features --locked -- -D warnings
/Users/chengwen/.cargo/bin/cargo +1.95.0 run -p cayw-content --example offline --locked -- /absolute/new/private-directory
```

`offline` 示例创建真实磁盘记录、重新打开读取、导出加密对象、拒绝旧内容覆盖新版本、显式 Fork 恢复、撤销后拒绝导出。目录须为尚未存在的绝对路径且不能经过软链接。示例不联网、不调用模型、不保存密钥。

2026-10-18 的实际验证环境是 macOS Darwin 27.0.0 / Rust 1.95.0。为了不改并行开发中的根 manifest/lock，验证使用复制此 crate 的独立 workspace `/tmp/cayw-content-validation.ALkLnX`，其中的 Cargo.lock 固定实际依赖。`cargo check --all-targets`、两种 feature 配置的 Clippy `-D warnings`、默认/无加密测试及离线示例均在该 workspace 运行。默认配置共 30 个测试通过，关闭加密共 15 个通过（两种计数都包含两个子进程测试入口）；所有实际检查退出码为 0。离线示例实际库为 `/private/tmp/cayw-content-validation.ALkLnX/offline-verified`，输出验证重开读、37 字节密文、旧版覆盖拒绝、Fork 恢复及撤销。根 workspace 的统一锁文件由集成方更新。

测试覆盖本地重启读、独立进程退出后读、6 个进程竞争 CAS、同整数版本不同分支、非法 ID/JSON、软链接逃逸、大小与配额、损坏对象、丢失/损坏权威文件、半写临时文件、加密往返、错误密钥、密文/AAD 替换、撤销重开、nonce 重放、旧远端与分支冲突、直接续接与跳版拒绝、半上传、提交成功但响应丢失后的同 id 重试、远端 head CAS、读回校验后提交。

`tests/encrypted.rs` 的 `MemoryRemote` 明确是宿主 trait 的内存 fixture，用于证明库的对象传输顺序和重试约束。它不证明真实 Server、网络故障、两机加密同步或真实远端持久性。上述 2026-10-18 记录没有验收资源准入或逐点强杀；当前 Runtime 的资源与内容检查须使用各自绑定源码的新证据。断电/硬件持久性、其他 macOS 架构仍未验证；Windows、Linux 和真实两机同步不属当前本机交付范围。

依赖许可证通过实际 `cargo metadata --locked` 核对：直接依赖选用 Apache-2.0 或 MIT 许可分支；其余包含 BSD-3-Clause、Unicode-3.0 和 Apache-2.0 WITH LLVM-exception，均可与项目 Apache-2.0 使用。没有复制旧项目代码，也没有 Codex 依赖。主要实际版本：cap-std/cap-fs-ext 3.4.6、fs2 0.4.3、chacha20poly1305 0.10.1、sha2 0.10.9、rand_core 0.6.4、zeroize 1.9.1、tempfile 3.27.0。
