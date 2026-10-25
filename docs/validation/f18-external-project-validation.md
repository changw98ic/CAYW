# F18 / Issue #30：外部工程验收补强（SLICE L）

日期：2026-10-25（America/New_York）。范围：为 [F18](https://github.com/changw98ic/CAYW/issues/33) / [#30](https://github.com/changw98ic/CAYW/issues/30) 补齐「真实 React/Vue（或同类）外部工程 + 可视化编辑」验收入口；在 Linux 上用仓内 fixtures 填补 Magpie/`darwin` 阻塞缺口；并记录 macOS Magpie 凭据路径（`/workspace/.env` 的 `GATEWAY_*`），**证据中不打印密钥**。

本记录只证明本分支新增的 harness/docs/scripts。既有 macOS 浏览器验收与 Magpie 生成记录仍各自独立，不能被本轮 Linux fixture 结果改写为已通过真实生成。

## 目标与边界

| 项 | 说明 |
| --- | --- |
| SLICE | L（仅本切片；不实现其他 F 项） |
| 功能 | F18 外部已有真实工程验收的跟进 |
| 可在 Linux 完成 | 工程结构核对、fixture/外部 Vite React·Vue 的 `npm ci`/`build`、`framework/source.test.mjs`、`cargo test -p cayw-visual-editor` |
| 仍受 `darwin` 限制 | Playwright 浏览器全链（`validate-visual-editor-framework.mjs` 等）、Magpie 真实生成（`validate-visual-editor-magpie.mjs`） |
| 不宣称 | 本轮 Linux 结果 ≠ Magpie 生成通过；fixture 路径 ≠ 任意第三方下载工程已验收 |

Issue #33 对 F18 的缺口原文要求补「React/Vue 外部已有真实工程与复杂动态页面验证」。本切片交付的是可复跑入口与 Linux 可填补部分；复杂动态页面与 macOS Magpie 生成编辑仍待 darwin 宿主执行。

## 新增入口

| 路径 | 作用 |
| --- | --- |
| `scripts/load-gateway-magpie-env.sh` | 从 `CAYW_GATEWAY_ENV`（默认 `/workspace/.env`）读取 `GATEWAY_URL`/`GATEWAY_KEY`，在未设置时映射为 `MAGPIE_BASE_URL`/`MAGPIE_API_KEY`；stderr 只报告 present/absent，不打印值 |
| `scripts/validate-visual-editor-external.mjs` | 外部/夹具工程验收 harness：复制工程、结构检查、可选 npm 构建、source 测试、visual-editor 包测；可选无认证 Magpie 探活 |
| `scripts/run-f18-external-validation.sh` | 先 `source` 凭据加载器，再调用上项；默认输出目录 `.local/f18-external-validation/run-<UTC>` |

### Linux / 非 darwin 推荐命令

```sh
# 默认使用仓内 fixtures（填补 Magpie/darwin 缺口的可复跑路径）
./scripts/run-f18-external-validation.sh .local/f18-external-validation/linux-r01

# 或显式指向外部工程（须为 Vite React / Vue 布局）
node scripts/validate-visual-editor-external.mjs \
  --out .local/f18-external-project/ext-r01 \
  --react-project /absolute/path/to/react-app \
  --vue-project /absolute/path/to/vue-app
```

`--out` 必须是新目录。证据写入该目录下的 `summary.json` 与各步骤日志。`summary.chinese_ready_facts` 提供中文结项要点字段。

### macOS Magpie 真实生成路径（文档约定）

Magpie 生成脚本仍限定 `darwin`。在 macOS 验收宿主上：

```sh
# 1) 映射凭据（不打印密钥）
source scripts/load-gateway-magpie-env.sh
# 确认 stderr 中 MAGPIE_API_KEY=present；不要 echo "$MAGPIE_API_KEY"

# 2) 公开预检（可选，不消耗生成配额）
node scripts/validate-visual-editor-magpie.mjs \
  --out .local/issue30-validation/magpie-preflight-new \
  --public-only

# 3) 在已有绿色本地 framework 验收与编辑器二进制后，去掉 --public-only
#    需要明确的当次真实调用授权与预算；默认不在 CI 运行
node scripts/validate-visual-editor-magpie.mjs \
  --out .local/issue30-validation/magpie-live-new \
  --runtime /path/to/cayw \
  --editor /path/to/cayw-visual-editor \
  --local-acceptance .local/issue30-validation/framework-green
```

凭据映射规则：

- `GATEWAY_KEY` → `MAGPIE_API_KEY`（仅当后者为空）
- `GATEWAY_URL` → `MAGPIE_BASE_URL`（仅当后者为空；若 URL 路径为空或 `/`，自动追加 `/v1`）
- 已存在的 `MAGPIE_*` 不被覆盖
- harness 与本记录只保存「present/absent」或「set/unset」，不写密钥正文、不写 Authorization 头

## 与既有验收的关系

| 记录 | 关系 |
| --- | --- |
| [框架验收](issue-30-framework-editor-macos.md) | macOS 合成 React/Vue 浏览器全链；本切片不替代 |
| [精简重测](issue-30-editor-simplification-macos.md) | 最新 macOS 浏览器结果；本切片不替代 |
| [Linux 变基](issue-30-linux-rebase-20261025.md) | 包测与 Host `O_PATH` 修复；本切片在其之上增加外部/夹具 harness |
| Magpie 脚本 | 仍 `darwin`；本切片文档化 GATEWAY 映射，不放宽平台门禁 |

仓内 fixtures（`crates/cayw-visual-editor/tests/fixtures/{react-vite,vue-vite}/`）带 `FIXTURE.md`，harness 标记 `source_origin=in_repo_synthetic_fixture`。调用方提供的工程无该标记时标记为 `external_or_caller_supplied`；外部工程不强制 Issue-30 fixture 的 DOM id，但仍须具备 Vite + 对应框架依赖 + `index.html`。

## 本轮执行留空位

合并前在本机实际跑一轮时，将命令、退出码与 `summary.json` 的 SHA-256 填入下表。未跑则保持「未在本记录执行」。

| 命令 | 平台 | 结果 |
| --- | --- | --- |
| `./scripts/run-f18-external-validation.sh .local/f18-external-validation/linux-r01 --probe-magpie` | Linux x86_64，Node v20.19.2，Rust 1.95.0 | exit 0；`summary.passed=true`；`magpie_generation=darwin_blocked`；React/Vue `source_origin=in_repo_synthetic_fixture`；`summary.json` SHA-256 `63bcd6ccdc5f49d06e4f43bc48add9d0ff9a67942af9a0f4c46557a0ace643c7` |

## 中文结项要点（可直接转述）

1. **切片 L 只交付 F18 外部工程验收跟进**：harness + 凭据加载 + 本说明；未改产品编辑语义。
2. **Linux 用 fixtures 填 Magpie/`darwin` 缺口**：结构检查、npm 构建、framework source 测试、visual-editor 包测可复跑。
3. **Magpie 真实生成仍限 macOS**：用 `load-gateway-magpie-env.sh` 读 `/workspace/.env` 的 `GATEWAY_*`，证据不打印密钥。
4. **fixture ≠ 任意外部下载工程**：外部路径可传入，但须自行满足 Vite React/Vue 布局；复杂动态页与生成后再编辑仍待 darwin 实跑。
