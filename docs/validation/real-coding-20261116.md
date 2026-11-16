# C02 真实模型整题修复与复验

2026-11-16，在 macOS 27.0.1 arm64、Rust 1.95.0 上，经唯一本地 Client → Runtime → CodeMode → Host 跑完 C02。原 Task 正常完成，模型实际执行完整测试，独立检查通过，说明中的主要事实与原始输出一致。这是一个真实模型处理合成代码任务的样本，不代表其他题目、UI、插件组合或其他架构通过，也不替代人工质量评分。

## 修复内容

- 准备脚本不再替用户选择总 token、回合、任务时长和能力累计用量预算。原先的 60000 token 和 20 回合会中断合法任务。
- 固定测试配置使用同一工具链的 Cargo、rustc、rustdoc 绝对路径，PATH 包含系统链接器目录；发送任务前先跑完整 Cargo 预检。构建缓存固定在任务工作区外。
- 产物检查只单独识别 Host 保留的三个根路径：`.cayw-host-lock`、`.cayw-host-read-fence`、`.cayw-host-read-state`。同名前缀、嵌套路径、符号链接和 target 文件仍会失败。
- 新增 `scripts/validate-live-coding.py`，使用本地 Client 的 ModelAuto 审批流程，一次提交原题，核对原 Task、测试输出、产物和退出状态。没有自动重试。
- 本机免密服务不要求密钥；单次回复额度从服务的模型目录读取，不另设更小的值。

## 本次运行

- 源码基线：`9561c77a0056868df778dc735d5785220d6e1159`。Runtime 与独立 Client 的 Rust 源码未在本次修改；脚本与示例改动尚未提交时运行。实际脚本已按启动时摘要核对并保存副本。
- 模型：Magpie `http://127.0.0.1:3425/v1`，Chat Completions，`opencode-go/deepseek-v4.1-flash`，无认证。供应商后台路由和账单未独立核对。
- 目录报告：单次最多输出 384000 token，上下文 1000000 token。任务不设总 token、回合或总时长预算。
- 权限：预先允许读取题目源码、Cargo 清单和锁文件，以及无过滤器的固定离线 Cargo 测试；其他操作走 ModelAuto。题目仍只允许修改 `src/lib.rs` 和创建 `output/change.md`。
- 题目版本仍为 `2026-11-09.v2`。初始提示增加工具链、测试入口和依据实测写说明的要求，全文保存在 `review/actual-prompt.txt`；未向模型提供隐藏答案。
- Session：`36996264-dcc2-45be-9f8f-c4e14c1925b7`。
- Task：`f7678fc9-f4c9-4f1c-a0da-e43fa0e164c8`，仅派发一次，`completed`，`holds_slot=false`。
- 事件时间范围：约 148.165 秒。23 次模型请求全部收到报告用量：普通输入 24129、缓存读取 211584、输出 10813，总 token 246526；思考 2819 已包含在输出中。费用未知。
- 人工在任务中补发提示：0。人工直接修改模型产物：0。模型提交最终结果后的修复请求：0。

## 实际结果

1. 模型先保留旧实现并添加临时探针，用故意触发的测试失败打印旧行为。退出码 101 原样保留；不是通过结果。输出确认空白被拒绝、负数/NaN 变成 0、溢出和无穷饱和成 `u64::MAX`，没有沿用上次错误的“回绕”等描述。
2. 模型重写精确整数解析并添加 11 项测试。随后两次完整 `cargo test --locked --offline` 均退出 0，11 项通过，文档测试阶段正常完成。
3. 独立编译和两组检查均通过：10000 个分值往返、`u64::MAX` 合法上界、前导零及 16 种非法/越界输入。完整测试日志保留，不以模型自述代替。
4. `Cargo.toml`、`Cargo.lock` 保持不变，无新增依赖或工作区 target 文件。三个 Host 协调文件被单独报告，其他产物范围检查通过。
5. 独立阅读源码和 `change.md`，逐项对照旧行为探针、实际测试名/数量、退出码、整数运算和上界处理。问题、修复、边界与验证要求均已交付。说明有“除以 f64”的用词瑕疵；实际源码、公式和验证展示的是浮点换算，未影响验收结论。没有替模型修改产物。
6. Session 最终为 `idle`，没有待审批操作、未结调用或未知能力结果；原 Task 释放名额，Client 收到 SIGINT 后退出 0。

## 回归和复跑

实际执行并通过：

```sh
CAYW_EVAL_RUSTC="$(rustup which --toolchain 1.95.0 rustc)" \
  python3 scripts/test_product_eval.py
python3 -m py_compile scripts/product_eval.py scripts/validate-live-coding.py
cargo +1.95.0 fmt --all -- --check
git diff --check
python3 scripts/validate-live-coding.py \
  --runtime target/debug/cayw \
  --client examples/headless-client/target/debug/cayw-client \
  --out /Volumes/ORICO/cayw-validation-20261116-190124-coding-fixed
```

评分器 66 项测试全部通过，包含原生 Rust 检查；没有跳过项。新增回归覆盖空预算、三个精确根路径，以及相似名称、嵌套路径、target 文件和符号链接的拒绝。

复跑须使用新的输出目录，并再次确认真实调用授权。`review/summary.json` 的 `machine_passed` 只表示机器核对通过；说明仍需独立阅读。本次没有填写正式人工评分表，也不将机器字段冒充人工 `accepted`。

原始证据保存在 `/Volumes/ORICO/cayw-validation-20261116-190124-coding-fixed/`：

- `review/invocation.json`：源码基线、启动时差异摘要、脚本摘要、二进制摘要、去密钥配置。
- `review/source-snapshot/`、`source-fingerprints.json`：实际运行脚本、评分器、题目及测试副本与摘要。
- `review/evidence/005-prompt.stdout.log`：完整事件时间线；`model-test-calls.json` 包含三次测试的完整输出。
- `review/provider-receipts.json`、`metrics.json`：23 次真实请求终态与报告用量。
- `review/independent-checks.json` 与其中索引的 `grade-*` 目录：独立检查与原始编译/测试日志。
- `review/evidence/final-state.json`、`final-tasks.json`、`server.stderr.log`：最终状态与清理证据。
- `workspace/src/lib.rs`、`workspace/output/change.md`：模型原样产物，摘要分别为 `efb0401d69262d73b3f0be06ad0b24dbdb9c8328a5dcd3e57bc1e151ba41bcd6`、`50af16ce552bbd7af0254303768ed78c0c4aea43f5729af78c9cf917c74036e6`。

原失败记录 `/Volumes/ORICO/cayw-validation-20261116-163646-real-model/` 保持不变。新入口开发时另有三次配置预检失败，均在真实模型任务提交前结束：`185547`（文件打开参数）、`185654`（注册模型前未启动 Client）、`185751`（模型配置遗漏必填输出额度）；相应目录和 driver 日志保留，没有计成真实通过样本。

## 检查、限制、兜底与重试

新增执行用量限制、兜底、重试：无。没有收紧产品原本允许的输入。

新入口的工具链预检依据是上次实际缺少链接器和 rustdoc；Task、未结状态、测试完整输出及退出码检查依据是本题交付标准。它们让配置错误和未完成任务明确失败。

保留原验收的 120 秒单次 Host 外部调用时限；模型目录查询使用同一期限，没有叠加进程或中间等待时限。超过该时限的单次外部调用会失败；整题运行不受它限制。单次回复额度来自服务模型目录，超过供应商额度的回复无法一次输出。固定测试参数和产物范围来自题目要求，未新增产品限制。
