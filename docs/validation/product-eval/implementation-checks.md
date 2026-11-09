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
