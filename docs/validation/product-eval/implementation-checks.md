# 本 PR 工具检查，不是产品效果结果

基线为 PR55 `43a44949c5a3eae05fc125763921adef19ad83f7`。本 PR 只加 eval 方案、任务生成/独立评分脚本、离线自测与贡献入口；没有调用模型、运行付费测试、改变生产逻辑或合并 PR。

## 本地执行

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

## PR CI 的新增检查

现有 macOS workspace job 增加：

```sh
CAYW_EVAL_RUSTC="$(rustup which rustc)" python3 scripts/test_product_eval.py
```

它只编译执行本 PR 内的受控 Rust 正反例，不调用模型或 Provider。5 个 native 测试验证：两份有缺陷初始代码不通过、正确 parser/重构通过、提前 exit(0) 不冒充检查通过、用户自带 exit 测试不能跳过独立检查。CI 状态以本 PR **精确 head SHA** 的实际结果为准；本文件描述计划与本地已做检查，不提前宣称 CI 成功。

隐藏 Rust 断言按名称逐个执行并检查完整结束标记，不只看退出码。标记检查不是防恶意伪造的证明，编译/执行都不是安全沙箱。实际模型产物必须先人工审查，然后在无凭据的一次性受限环境评分；例如编译期 `include_str!` 也可能读取宿主文件。
