# F18 / Issue #30：外部工程验收补强（SLICE L）

日期：2026-10-26（America/New_York）。范围：为 [F18](https://github.com/changw98ic/CAYW/issues/33) / [#30](https://github.com/changw98ic/CAYW/issues/30) 补齐「真实 React/Vue（或同类）外部工程 + 可视化编辑」验收入口。

本记录只证明本分支 harness/docs/scripts。**不宣称 Magpie 生成通过**；既有 macOS 浏览器验收与 Magpie 生成记录仍各自独立。

## 目标与边界

| 项 | 说明 |
| --- | --- |
| SLICE | L（仅本切片；不实现其他 F 项） |
| 功能 | F18 外部已有真实工程验收的跟进 |
| 本 harness 可证明 | 对传入工程路径：Vite/框架结构检查；可选 `npm ci`/`build`；**cayw-visual-editor HTTP：open → edit → Host `fs.write` save → 停服 reopen**；可选 framework source 测试与 `cargo test -p cayw-visual-editor` |
| 仍不由本脚本确立 | Magpie 真实生成（`validate-visual-editor-magpie.mjs`，darwin）；Playwright 浏览器全链（`validate-visual-editor-framework.mjs`，darwin） |
| 外部工程与 fixture id | 带 `FIXTURE.md` 的仓内夹具仍要求 Issue-30 选择器 id；**调用方外部工程缺少这些 id 不得误失败** |

## 新增入口

| 路径 | 作用 |
| --- | --- |
| `scripts/load-gateway-magpie-env.sh` | `/workspace/.env` 的 `GATEWAY_*` → `MAGPIE_*`（不打印密钥） |
| `scripts/validate-visual-editor-external.mjs` | 外部/夹具 harness：结构、可选构建、**编辑器 open/edit/Host-save/reopen**、可选 source/cargo；可选无认证 Magpie 探活（探活 ≠ 生成验收） |
| `scripts/run-f18-external-validation.sh` | 先加载凭据再调用上项 |

### Linux / 非 darwin 推荐命令

```sh
# 默认：仓内 fixtures 副本上跑结构 + 构建 + 编辑器链
./scripts/run-f18-external-validation.sh .local/f18-external-validation/linux-r02

# 显式外部工程（Vite React / Vue；无需 Issue-30 fixture DOM id）
node scripts/validate-visual-editor-external.mjs \
  --out .local/f18-external-project/ext-r01 \
  --react-project /absolute/path/to/react-app \
  --vue-project /absolute/path/to/vue-app
```

编辑器链默认使用 `target/debug/cayw-visual-editor`（可用 `--editor-binary` 覆盖）。`--skip-editor-chain` 可跳过该步。

### macOS Magpie 真实生成路径（文档约定；本脚本不跑）

```sh
source scripts/load-gateway-magpie-env.sh
node scripts/validate-visual-editor-magpie.mjs --out .local/issue30-validation/magpie-preflight-new --public-only
# 去掉 --public-only 需当次授权与预算；仍限 darwin
```

## 与既有验收的关系

| 记录 | 关系 |
| --- | --- |
| [框架验收](issue-30-framework-editor-macos.md) | macOS 合成 React/Vue 浏览器全链；本切片不替代 |
| [精简重测](issue-30-editor-simplification-macos.md) | 最新 macOS 浏览器结果；本切片不替代 |
| Magpie 脚本 | 仍 `darwin`；本切片不放宽平台门禁、不宣称生成通过 |

仓内 fixtures（`crates/cayw-visual-editor/tests/fixtures/{react-vite,vue-vite}/`）带 `FIXTURE.md` → `source_origin=in_repo_synthetic_fixture`。调用方工程无该标记 → `external_or_caller_supplied`。

## 本轮执行留空位

| 命令 | 平台 | 结果 |
| --- | --- | --- |
| `node scripts/validate-visual-editor-external.mjs --out …/linux-chain-r02 --skip-cargo-test --skip-source-test` | Linux x86_64 | exit 0；fixtures 上 react/vue editor_chain=true（hero-title text → Host fs.write → reopen）；`magpie…established_by_this_script=false`；summary SHA-256 `9f151f5b77e249c94e826704e3ce5ac61938fafe07641fb9ea5a9e6655680eaa` |
| `… --react-project /tmp/f18-external-min/react --vue-project …/vue`（无 Issue-30 fixture id） | Linux x86_64 | exit 0；`source_origin=external_or_caller_supplied`；缺 14 个 fixture id **未失败**；style edit → Host save → reopen；summary SHA-256 `d30d75981245fddfdf8de8b242fd94ad2d27d3defaa8751134a2ebe2cfbc6c9f` |

## 中文结项要点

1. **切片 L**：F18 外部工程验收 harness；未改产品编辑语义。
2. **编辑器链**：对传入路径用现有 `cayw-visual-editor` API 做 open/edit/Host-save/reopen；不是 Magpie E2E。
3. **外部工程缺 fixture id 不再误失败**；夹具仍检查 Issue-30 选择器。
4. **Magpie 真实生成仍限 macOS**；本脚本 `magpie_generation.established_by_this_script=false`。
