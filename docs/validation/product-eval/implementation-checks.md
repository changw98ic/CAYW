# 本 PR 工具检查，不是产品效果结果

基线为 PR55 `43a44949c5a3eae05fc125763921adef19ad83f7`。本 PR 只加 eval 方案、任务生成/独立评分脚本、离线自测与贡献入口；没有调用模型、运行付费测试、改变生产逻辑或合并 PR。

## 初始 head fbb7405 的本地执行

2026-11-09，在无 Rust/Cargo 的执行环境，实际执行：

```sh
python3 scripts/test_product_eval.py
python3 -m py_compile scripts/product_eval.py scripts/product_eval_cases.py scripts/test_product_eval.py
git diff --check
node --test crates/cayw-webui/tests/*.test.mjs
```

- 既有 WebUI Node 测试 122/122 通过；这不是完整真实 UI 操作验收。
- 40 个 Python 评分器自测通过；5 个需要真实 rustc 的测试跳过，不能计为通过。
- 覆盖任务准备、禁止覆盖、独立数字核对、原输入/manifest/prompt/评分器指纹、缺失/篡改/空证据、符号链接/路径穿越、未知用量、坏结构、非授权产物、只有完成宣称无文件、明确失败/取消终态、fixture 不进入效果比较、重复运行/重复 repetition、seed 与比较条件不一致。
- 自测里的“live_model”“reviewer”和证据均为人工构造的**评分器正反例**，不是真实 Provider 运行。即使测试覆盖 accepted 分支，也不产生 CAYW 产品成功样本。
- 未执行 Rust workspace fmt/check/test/clippy、Rust fixture 编译/行为检查、真实模型任务、交互 UI、插件效用或长任务恢复。

## PR CI 的评分器检查

现有 macOS workspace job 增加：

```sh
CAYW_EVAL_RUSTC="$(rustup which rustc)" python3 scripts/test_product_eval.py
```

它只编译执行本 PR 内的受控 Rust 正反例，不调用模型或 Provider。最初 5 个 native 测试验证：两份有缺陷初始代码不通过、正确 parser/重构通过、提前 exit(0) 不冒充检查通过、用户自带 exit 测试不能跳过独立检查。CI 状态以本 PR **精确 head SHA** 的实际结果为准；本文件描述计划与本地已做检查，不提前宣称 CI 成功。

隐藏 Rust 断言按名称逐个执行并检查完整结束标记，不只看退出码。标记检查不是防恶意伪造的证明，编译/执行都不是安全沙箱。实际模型产物必须先人工审查，然后在无凭据的一次性受限环境评分；例如编译期 `include_str!` 也可能读取宿主文件。

## 评分器缺陷修复复核（2026-11-09）

先从远端 PR56 重新获取 `fbb74059e360a9088e2268ee55f16bea6340cda9`，未修改代码时用完全合成的评分器记录逐项复现：

| 反例 | fbb7405 实际返回 | 修复后的要求 |
| --- | --- | --- |
| 完整人工声明 + unknown 用量 + cost=0/USD | accepted，没有 invalid_reasons | invalid，金额不能伪装为零 |
| 1 accepted + 1 invalid live_model | attempted=1，accepted=1，rate=1.0 | attempted=2，accepted=1，rate=0.5 |
| C02 和 C03 新增 target/extra | 均没有 scope 失败 | 两个 case 均 scope 失败，嵌套 target/debug 等同样失败 |
| report 输出 | 没有 rubric 分布、unknown 汇总、failure_category、人工修补汇总 | 按 execution_kind 分离这些字段，保留未知与原始失败 |

新增反例与正例覆盖：unknown 费用为 0/正数、真实记录零费用与 unknown/null 的区分、invalid/blocked/not_run 矛盾 live 分母、CLI 从原 run 重算、target 任意层级越界、Cargo.lock 不可改、rubric 零分/缺失/坏值、UI 与 fixture 隔离、人工救援前后及未知修补计数、失败类型与原始原因、币种/partial 分离、坏计数、人工修补与零干预/修复的矛盾记录、坏比较字段仍保留真实分母。计数汇总不把 null 变零，不用 fixture 补齐真实分母。

同一无 Rust/Cargo 环境运行：

```sh
python3 scripts/test_product_eval.py
python3 -m py_compile scripts/product_eval.py scripts/product_eval_cases.py scripts/test_product_eval.py
git diff --check
```

结果：**55 项 Python 测试通过，6 项 native 测试跳过**；另重跑既有 WebUI Node 测试 122/122 通过、Python compile/相对文档链接/git diff --check 通过。第 6 项 native 测试使用真实 Cargo 检查两份 fixture 的固定 Cargo.lock、`--locked --offline` 及宿主缓存目录配置，断言未产生 workspace/target、锁文件未变；这仅验证构建路径，不能替代已有 Rust 行为检查。它将在 macOS CI 同一评分器步骤执行。本记录不提前宣称本次 CI 已通过；以新推送精确 SHA 的 job/step 结果为准。

所有 live_model、Provider receipt、reviewer 和成功率样例均为**合成的评分器反例/正例**。本次没有调用真实/付费模型，没有进行产品效果测量；未把 Linux 上的 Python 检查当作 macOS 产品验收。fixture 更新到 `2026-11-09.v2`，旧 v1 记录不改写、不与新运行自动合并。

## 同一 PR 的 CI 阻塞：取消测试的就绪竞态

修复提交 `3f21ccedd7dafaa0edc52d3d18cb6b568d95ba56` 的 [CI #153](https://github.com/changw98ic/CAYW/actions/runs/42563947100) 已到终态：新增评分器 **61/61 通过，无跳过**，含 6 项 Rust/Cargo fixture 检查。Ubuntu workspace、macOS 产品 feature/故障恢复、arm64 打包/安装/升级三组通过；macOS workspace 在既有 `cayw-vision::backends::http_describe_cancel_cleans_temp_files` 失败，不能称全绿。

日志位于 [macOS workspace job](https://github.com/changw98ic/CAYW/actions/runs/42563947100/job/128986235743)：两个权限记录的断言实际只读到一个。FakeCurl 先追加第二个临时文件路径，再写第二条权限；原测试只等两个路径就取消，因此可打断后续记录。失败不是评分器计分错误，也不能用一次重跑成功将它抹掉。

后续修复仅调整测试 fixture：所有记录完成后发布显式就绪标记，正常取消测试等待该标记，原两条权限和临时文件清理断言全部保留。另加受控停点，在第二个路径已记录、第二条权限未记录时保持子进程，用确定性反例验证此时两条路径不等于记录齐全；取消后仍须清理两个临时文件。停点只用 shell builtin，不启动长寿命辅助子进程。

macOS CI 在完整 workspace 检查前执行针对性的 `cargo test -p cayw-vision --lib --locked backends::http_describe_cancel`，便于尽早发现本类回归；完整 workspace test 仍照常执行。当前本地无 Rust/Cargo，且可用列表中的 Mac 离线，未在本地执行或外推 macOS 结果；新提交的 CI 状态须按其精确 SHA 另查。

## 测试工具链与取消回归的有界执行（2026-11-09）

本轮从 PR56 当时最新 head `e28c3aa0ed67136f49c7c73c54086c10e10138ca` 追加，不改生产逻辑。

- 原 Cargo fixture 子进程只固定 `RUSTC`，却继承宿主 `RUSTDOC`/`PATH`。本地用 mock 子进程检查原环境时，选定 `/chosen/toolchain/bin/rustc` 仍传入冲突的 `/conflicting/rustdoc`。这是环境传递反例，不是本地真实 Cargo 运行。
- 现在由同一 helper 固定选定工具链的 `cargo`、`RUSTC` 和同目录 `RUSTDOC`。新增纯 Python 环境检查，证明覆盖继承值且不修改宿主环境；新增 native 回归，在 PATH 首位和 RUSTDOC 同时放置必失败的 rustdoc 替身，并实际运行一个通过的 doctest，防止只跑普通测试而漏掉 rustdoc。
- 原 Vision 受控停点为无限 shell 忙等，取消后无期限 join。现在停点使用 `exec sleep 5`，保持直接子进程身份且自然退出；两项取消测试改用有 10 秒上限的完成通道。就绪标记仍有独立 5 秒上限，原完整/部分权限记录、取消终态和两个临时文件清理断言不变。
- 新增 `http_describe_cancel_hold_expires_without_cancellation`，故意不取消，验证停点会自然结束、仍只留下部分记录、返回无有效响应的错误并清理临时文件。现有 CI 的 `backends::http_describe_cancel` 过滤器也会运行这项反例。这些秒级上限用于防止测试挂起，不是产品响应时间指标。

本地实际执行：`python3 scripts/test_product_eval.py` 为 **56 项通过、7 项 native 跳过**；Python compile、`git diff --check` 通过；既有 WebUI Node 测试 **122/122 通过**。另直接提取当前 Rust 文件中的完整 shell 模板运行，确认无取消停点约 5 秒退出，以及完整/部分记录两条取消路径都关闭 stdout/stderr 管道；这只验证 shell fixture，不能冒充 Rust 取消路径验证。

本地没有 Rust/Cargo，当前列出的 Mac 为离线，因此 Rust fmt/check/clippy/native 测试由新提交的 CI 精确 SHA 实际结果决定，本记录不提前宣称通过。没有运行真实/付费模型、交互 UI、插件效果或真实长任务恢复。
