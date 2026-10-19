# Workspace diff EOF 标记修复验证

本记录只验证 `code.diff_hunks` 对 `\ No newline at end of file` 的处理。旧实现会接受某一侧已经声明文件结束、随后该侧又出现正文的非法 diff，还会在 `max_hunks` 截断返回结果时报告 `validated_complete: true`。本次修复为每个文件分别保存旧侧和新侧的结束状态，跨后续 hunk 保留，读取下一组文件头时重置。删除行后的标记只结束旧侧，新增行后的标记只结束新侧，上下文行后的标记结束两侧。

另一侧仍可继续；后续 hunk 中已结束的一侧只能使用锚定在已知结尾的空范围。标记必须紧跟正文，重复标记和带额外字符的标记继续拒绝。所有输入都验证完才返回成功，包括未返回的后续 hunk。返回字段、原始字节切片、行号、输入 SHA-256 和既有格式限制保持原有含义。

## 冻结输入与证据

验证在 macOS 27.0.1 / ARM64、Rust 1.95.0 上离线进行。目录日期按本地日期 2026-10-19 记录；命令日志保存 UTC 起始时间。

- 证据目录：`/Volumes/ORICO/cayw-workspace-diff-eof-20261019.7qWJqe`
- 修复后冻结根：`/Volumes/ORICO/cayw-workspace-diff-eof-20261019.7qWJqe/frozen`
- 复跑 Cargo 入口：`/Volumes/ORICO/cayw-workspace-diff-eof-20261019.7qWJqe/frozen/Cargo.toml`
- 40 个构建输入的清单：`frozen-source-manifest.json`；逐文件记录字节数和 SHA-256。
- 清单聚合 SHA-256：`8c482920055b0ec2729ad7e89c0da6d5da0289409e51e3829a33af24fc5ed46e`。算法为：按相对路径排序，每行使用小写文件 SHA-256、两个空格、相对路径、LF，再对拼接的 UTF-8 字节计算 SHA-256。
- 修复前 `src/code.rs` SHA-256：`9efd551907c7a72dce0a92cc5b374e0515af419b72a66f6bfe006eec1ccc2151`。
- 修复后 `src/code.rs` SHA-256：`a9fde84f710e7ee4346f9592d852efd6f399e401f0e357c44ed523f9c483458e`。
- 新增 `tests/diff_eof.rs` SHA-256：`d3dd1559c4cc6516bf57afb6474990fa46f3556ca74eb953b04989f4d56f3d38`。

冻结根只包含 Workspace 及其实际本地依赖 `cayw-ai`、`cayw-agent`、`cayw-codemode` 的库源码、各自清单，以及 Workspace 的全部测试目标。根清单仅在冻结副本中收窄 workspace 成员；所有 package 和 dependency 声明保持原样。没有复制用户配置，也没有修改生产 Cargo 清单或锁文件。

第一次 `--locked` 测试在执行测试前因副本锁文件需要裁剪而拒绝；该日志仍保留为 `red-diff-eof.*.log`，不作为测试失败证据。随后使用 `cargo metadata --offline` 对隔离副本裁剪锁文件，从根锁的 394 个包保留 281 个。`lock-prune-verification.json` 核对全部保留包的版本、来源和校验值均与根锁一致。

- 捕获的完整根锁 SHA-256：见 `capture-receipt.json` 与 `inputs/Cargo.lock`。
- 实际冻结锁 SHA-256：`b49c09c514587dc3097ba9955ccbd6015fe4913d9cba74e270228b28a279c469`。

`commands.jsonl` 保存实际参数、工作目录、环境覆盖、起始时间、退出码，以及独立 stdout/stderr 日志的路径、字节数和哈希。`validation-evidence.json` 汇总结果，`live-input-comparison.json` 记录生产文件与冻结输入的核对结果，`review/` 保存这次三个交付文件的差异。

## 回归结果

新增测试通过公开的 `Workspace::execute("code.diff_hunks", ...)` 调用实际实现，未使用替身。8 个测试包含 96 次调用：76 次检查非法输入拒绝，20 次检查合法输入和返回值。

| 检查 | 实际结果 |
| --- | --- |
| 相同的最终格式测试配原始解析代码 | 4 通过、4 失败，退出码 101；非法续写被误接受 |
| 修复后的 EOF 回归目标 | 8 通过、0 失败，退出码 0 |
| Workspace 全包、全部 feature | 75 通过、0 失败、0 忽略，退出码 0 |
| 原独立检查的公开接口测试重跑 | 12 通过、0 失败，退出码 0 |
| 原独立检查的 8 个 EOF 确认案例重跑 | 8 个符合预期，mismatches 从 4 降为 0 |
| `cargo check --all-targets` | 退出码 0 |
| `cargo clippy --all-targets -- -D warnings` | 退出码 0 |
| 四成员冻结副本 `cargo fmt --all -- --check` | 退出码 0，无格式差异 |

全包 75 项包括 2 个库内测试、73 个集成测试；文档测试为 0。部分集成测试会创建子进程，原始日志额外打印了 2 次已计入目标的子进程测试通过结果；将日志所有 summary 直接相加会得到 77，不能据此宣称有 77 个不同测试。

`red-final/` 使用最终格式的相同测试和其余相同构建输入，只把 `src/code.rs` 换成原始源码。`red-final-source-manifest.json` 可与修复后清单逐项比较；唯一内容差异就是该解析文件。最初的失败记录 `red-diff-eof-locked.*.log` 和最终失败记录 `red-diff-eof-final.*.log` 均保留，失败源于实际误接受，无编译错误。

覆盖内容包括：旧侧删除行或上下文行续写、新侧新增行或上下文行续写、上下文标记后的任一侧续写、跨 hunk 续写、合法另一侧 hunk 之后再次非法续写、越过已知 EOF 的空范围、合法另一侧续写、普通文件头与 Git 文件头的下一文件重置、重复/错位标记，以及 CRLF、多字节内容和输入末行没有换行时的精确字节与行号。每个拒绝用例还在前置有效文件后重跑，并分别使用 `max_hunks: 1` 和 `1000`，确认截断后仍验证后续内容。

原始独立证据位于 `/Volumes/ORICO/cayw-issue5-native-read-independent-20261019.EAVACh`。重跑仅复制其中 `public_api_probes.rs` 和 `diff_eof_confirmation.rs`，替换输出目录常量，保留所有断言及输入；源码变换和链接产物记录在 `independent-replay-inputs.json`、`independent-linked-artifacts.json`。新输出写入本次证据目录的 `probes/`。原证据文件及原始冻结解析文件的哈希在验证前后核对一致。四个合法 EOF 案例的完整结果与原结果相同，四个非法案例现在返回 `outcome_unknown: false` 的错误，详见 `independent-case-comparison.json`。

## 独立复核

修复交回后，另一名检查者只使用同一份 40 文件的只读冻结输入，在新的目录 `/Volumes/ORICO/cayw-workspace-diff-eof-independent-20261019.gGtrvC` 构建和执行。`independent-validation.json` 保存结论，`commands.jsonl` 保存实际命令、退出码及日志哈希，`source-closure-audit.json` 核对 Cargo 与编译器实际使用的四个本地包、23 个生产 Rust 源文件。全部 40 个输入的前后哈希均相同。

独立编写的 467 组输入分别使用 `max_hunks: 1` 和 `1000`，共调用公开接口 934 次：162 组合法输入接受 324 次，305 组非法输入拒绝 610 次，结果偏差为 0。检查包含两侧 EOF、跨多个后续 hunk、空范围锚点、截断后非法后缀、下一文件重置、CRLF、多字节正文、末行无换行，以及完整返回坐标和输入摘要。独立重跑 Workspace 全包仍为 75 通过、0 失败；check、严格 Clippy 和四包格式检查的退出码均为 0。

最初的独立探针有 8 次偏差，来自 4 组使用了接口明确拒绝的非 ASCII 路径的输入。检查者另存修正后的 `probes/cases-portable.json`，只修路径及对应预期偏移，保留多字节正文和第一次失败记录。上述 934 次、零偏差的结果来自修正后输入；没有把第一次失败隐藏或计作通过。Main 已直接读取完整 75 项测试日志及探针源码，并核对 14 个命令的全部 28 份输出日志哈希。

## 复跑

先核对 `frozen-source-manifest.json` 的全部哈希，使用冻结根复跑；不要用仍在共同修改的生产树替代。冻结输入设为只读，编译缓存和临时目录独立放在 ORICO。需要本机已经具备锁定版本的 Cargo 缓存及相应原生编译工具。

```sh
export PATH="/Users/chengwen/.cargo/bin:$PATH"
export TMPDIR=/Volumes/ORICO/cayw-workspace-diff-eof-20261019.7qWJqe/tmp
export CARGO_TARGET_DIR=/Volumes/ORICO/cayw-workspace-diff-eof-20261019.7qWJqe/target-verifier
export CARGO_NET_OFFLINE=true

cargo +1.95.0 test --manifest-path /Volumes/ORICO/cayw-workspace-diff-eof-20261019.7qWJqe/frozen/Cargo.toml -p cayw-workspace --all-features --offline --locked --test diff_eof -- --test-threads=1
cargo +1.95.0 test --manifest-path /Volumes/ORICO/cayw-workspace-diff-eof-20261019.7qWJqe/frozen/Cargo.toml -p cayw-workspace --all-features --offline --locked -- --test-threads=1
cargo +1.95.0 check --manifest-path /Volumes/ORICO/cayw-workspace-diff-eof-20261019.7qWJqe/frozen/Cargo.toml -p cayw-workspace --all-features --all-targets --offline --locked
cargo +1.95.0 clippy --manifest-path /Volumes/ORICO/cayw-workspace-diff-eof-20261019.7qWJqe/frozen/Cargo.toml -p cayw-workspace --all-features --all-targets --offline --locked -- -D warnings
cargo +1.95.0 fmt --manifest-path /Volumes/ORICO/cayw-workspace-diff-eof-20261019.7qWJqe/frozen/Cargo.toml --all -- --check
```

## 范围限制

Linux、Windows 未验证；本记录不能关闭 Issue #5，也不能证明 Host、Cells、Runtime、Device、Harness、WebUI 的集成验收或整个根 workspace 通过。本次仅使用离线合成输入，没有真实 Provider、模型或付费调用，没有网络操作、Git 写入或发布。全局指令引用的两份沟通规则文件在指定路径不存在，读取结果已确认；没有为此读取其他用户配置。
