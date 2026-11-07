# Issue #30 React/Vue 源码编辑验收

日期：2026-10-24。范围：[Issue #30](https://github.com/changw98ic/CAYW/issues/30) 的 Vite React JSX/TSX、Vue 3 适配，以及当时的 HTML 路线回归。

本文保留适配阶段的源码与证据。随后删除重复检查和误拦截限制后的结果，见[精简重测记录](issue-30-editor-simplification-macos.md)；本文的旧摘要不证明后续源码。

React/Vue 已完成源码映射、直接编辑和保存。两个从零编写的测试项目均实际通过多文件修改、保存、构建、停服重启、再次编辑和外部修改冲突流程。当前 HTML 六组场景也通过回归。**真实模型生成仍未验收：本次 Magpie 100,000,000 token 上限已获授权，但当前环境没有 `MAGPIE_API_KEY`，真实生成请求为 0 次。**

代码在独立工作树，未提交、推送或部署。2026-10-24 只读核对时，远端 Issue 仍 OPEN，正文及勾选状态尚未同步。本文不把合成项目验收写成整个扩展任务完成。

## 源码、环境和原始证据

| 项目 | 实际值 |
| --- | --- |
| 工作树 | `/Volumes/ORICO/CAYW/.claude/worktrees/issue-30-visual-editor` |
| 分支 | `issue/30-visual-editor` |
| 基线 HEAD | `cc9092a32f26cea0030fc13f847a5de5612131ef`；当前未提交改动另以逐文件摘要绑定 |
| 系统 | macOS 27.0.1，Build 26A434，Darwin 27.0.0 |
| 架构 | ARM64；Rust 目标 `aarch64-apple-darwin` |
| Rust | rustc 1.95.0，Cargo 1.95.0；Clippy 使用 `+1.95.0` |
| Node / npm | Node v24.4.1 / npm 11.4.2 |
| 浏览器 | 本机 Google Chrome 155.0.8059.85，Playwright 1.61.1 |
| 构建 | debug、默认 feature；`CARGO_TARGET_DIR="$PWD/.local/issue30-target"` |
| 编辑器二进制 SHA-256 | `bc9250d463fc1432b7415f2769a4dfd84640e4748086dc438ff5aa5d7addb34b` |
| 最终框架浏览器运行 | UTC `2026-10-24T03:03:22.560Z` 至 `2026-10-24T03:03:42.030Z` |
| 当前 HTML 回归运行 | UTC `2026-10-24T02:41:44.890Z` 至 `2026-10-24T02:42:14.245Z` |

原始证据根目录为工作树下 `.local/issue30-validation/`。目录不进入产品源码，复跑必须使用新目录。

- `framework-02/`：最终 React/Vue 浏览器记录，8 组场景、48 项具名检查全部通过。包含实际项目副本、源码摘要、桌面／移动截图、三次构建、启动／停止结果及调用记录。
- `html-framework-regression-01/`：当前二进制的 HTML 回归，6 组场景、41 项具名检查全部通过。
- `framework-final-verification-02.json`：主线程再次核对两份运行记录、当前源码摘要、二进制、停止结果和日志计数的派生记录。
- `magpie-preflight-r11/`：最终模型验收脚本的公开预检。记录缺少凭据、未开始生成、`acceptance_passed=false`。
- `relay-selftest-r01/`、`runtime-record-selftest-r01/`、`helper-selftest/`：本机模拟 HTTP 记录、进程结果记录和离线账本导出检查；均没有真实模型请求。

`framework-02/source-receipt.json` 绑定 22 个明确路径，包括 Cargo 清单、锁文件、Rust 产品代码、前端资源、Node 解析及预览代码和框架验收脚本。运行前后源码与二进制均未变化，`source_changed_during_run=[]`。收尾再次核对这些路径，没有差异。HTML 的 17 个记录路径也仍与对应运行一致。框架 Node 源码由外部文件加载，因此最后两项 JavaScript 修复没有改变 Rust 二进制；最终框架运行绑定的是修复后的 Node 文件。

| 证据或源码（相对证据根目录或仓库） | SHA-256 |
| --- | --- |
| `framework-02/summary.json` | `d387a80ba2503ae7f8e266855eac2af16fbd9a1b530f7d1998ec9abfa2e0b01c` |
| `framework-02/source-receipt.json` | `56f550a95eee40a57fd4cffacf46538193813a5894cf5a0dfe8155ffc0c9bd84` |
| `html-framework-regression-01/summary.json` | `c2619da660a36fa1d307cd5e3033e1defa3fae183a34f49058517cd9a83d5222` |
| `crates/cayw-visual-editor/framework/source.mjs` | `768c5947d22f093389b270da19d4a2f108d12e620cca47fd0ee977614bc79911` |
| `crates/cayw-visual-editor/framework/source.test.mjs` | `04da4c29dd3d91c87ca121d7b71753a77db989cc6ba55324de373c7da73d06e4` |
| `crates/cayw-visual-editor/framework/companion.mjs` | `7f19f831a7602c39b4c7f77eff8f4292f989848324c085fc1eff660240c3958b` |
| `workspace-framework-tests-01.log` | `268ac0646e9d371d713d8ad73ebe96bfa2c9be4f1a7ab06f6bf20f2b0eadaf8a` |
| `workspace-framework-clippy-01.log` | `6268a485c5506425316bfd3b3349f0beb35fd9a8ef1658e45a8621b2b647c504` |
| `framework-source-tests-final-02.log` | `ca08000cbd3aa39b4259e4b0d4044035458db6e7caa5afc30e3ae5830e4f9a12` |

较早的 `framework-01/` 是最后两项边缘修复前的原始运行，保留为历史。早先派生文件 `framework-final-verification.json` 已标注被替代及测试去重更正。它不能替代 `framework-final-verification-02.json`。较早的 [HTML 首版记录](issue-30-visual-editor-macos.md)同样只证明当时源码。

## 输入和依赖

React/Vue 输入是本项目从零编写的合成项目，不是下载的第三方项目，也不是模型生成结果：

- `crates/cayw-visual-editor/tests/fixtures/react-vite/`：React 19.1.1、Vite 7.3.5、`@vitejs/plugin-react` 5.1.0。`HeroSection.tsx` 与其他 JSX 组件共同进入实际保存、构建和重启路径。
- `crates/cayw-visual-editor/tests/fixtures/vue-vite/`：Vue 3.5.22、Vite 7.3.5、`@vitejs/plugin-vue` 6.0.2。组件包含原脚本、事件、数据绑定和 scoped 样式。

本次实际输入位于 `.local/issue30-framework-inputs/{react-vite,vue-vite}/`，安装依赖后由脚本复制到新证据目录。每份运行保存输入文件摘要，未保存阶段逐文件比较磁盘内容。提交用 fixture 不包含 `node_modules` 或构建产物。

框架解析与预览直接依赖固定版本：`@babel/parser` 7.28.4、`@vue/compiler-dom` / `@vue/compiler-sfc` 3.5.22、`magic-string` 0.30.19、Vite 7.3.5。上述框架、插件及工具包采用 MIT 许可证，与本项目 Apache-2.0 兼容。没有复制第三方组件实现。最终用户应用不依赖 Playwright。

HTML 回归继续使用仓库 `site/` 和此前已准备的 StartBootstrap Agency 固定工作副本。Agency 提交为 `b2d5d5c6b1396bc5b0e8dd92c693b106cfb7cee1`，MIT。本轮没有下载并执行另一个外部框架项目。HTML 输入来源、原件摘要和原场景边界见[历史 HTML 记录](issue-30-visual-editor-macos.md)。

## 实际命令与结果

命令均在本工作树执行。Cargo 入口为 `/Users/chengwen/.cargo/bin/cargo`。日志名相对证据根目录。

| 命令 | 日志 | 结果 |
| --- | --- | --- |
| `cargo build -p cayw-visual-editor --locked` | `framework-build-03.log` | exit 0 |
| `cargo fmt --all -- --check` | `workspace-framework-fmt-final.log` | exit 0 |
| `cargo check --workspace --locked` | `workspace-framework-check-01.log` | exit 0 |
| `cargo test --workspace --locked -- --test-threads=4` | `workspace-framework-tests-01.log` | exit 0；1,815 passed、0 failed、7 ignored |
| `cargo +1.95.0 clippy --workspace --all-targets --locked -- -D warnings` | `workspace-framework-clippy-01.log` | exit 0 |
| `cargo run -p cayw-agent --example offline --locked` | `workspace-framework-offline-01.log` | exit 0；`add` 返回 5，`5 messages, running=false` |
| `cargo test -p cayw-visual-editor --locked` | `framework-integration-tests-03.log` | exit 0；115 passed、0 failed |
| `node --test crates/cayw-visual-editor/framework/source.test.mjs` | `framework-source-tests-final-02.log` | exit 0；34 passed、0 failed |
| `node scripts/validate-visual-editor-framework.mjs`，环境见下文 | `framework-02.log` | exit 0；8 组、48 项检查通过 |
| `node scripts/validate-visual-editor.mjs`，输入为 site 与既有 Agency 副本 | `html-framework-regression-01.log` | exit 0；6 组、41 项检查通过 |

工作区计数采用 269 个 Cargo `Running` / `Doc-tests` 顶层目标的最后一条结果，排除父测试启动的子进程重复输出。直接累加全部结果行会得到 1,818，其中有 3 项重复；准确值为 1,815。编辑器的 115 项包括库 47、CLI 3、框架集成 12、HTML 源码引擎 33、服务 20。7 个忽略入口未额外全部运行，不计作通过。

Rust 工作区检查完成后，最后修改的产品文件只有框架 `source.mjs` 与对应测试。修改后重新执行了全部 34 项转换测试和两个框架的完整浏览器流程；没有用旧框架摘要证明新源码。

最终框架命令：

```sh
CAYW_PLAYWRIGHT_MODULE="$PWD/.local/issue30-browser-driver/node_modules/playwright/index.mjs" \
CAYW_BROWSER_EXECUTABLE='/Applications/Google Chrome.app/Contents/MacOS/Google Chrome' \
CAYW_EDITOR_VALIDATION_OUT=.local/issue30-validation/framework-02 \
node scripts/validate-visual-editor-framework.mjs
```

该脚本默认使用 `.local/issue30-target/debug/cayw-visual-editor` 及上述本地输入目录。更换输入可设置 `CAYW_EDITOR_VALIDATION_REACT` / `CAYW_EDITOR_VALIDATION_VUE`。复跑时必须更换输出目录。脚本使用公开的本地测试令牌，不使用模型凭据。

## 框架完整流程

两个框架分别运行 `edit`、`restart`、`conflict-draft`、`conflict-save`。每个框架在原始输入、首次保存后、冲突恢复后各构建一次，六次构建全部 exit 0。

| 实际场景 | 证据及含义 |
| --- | --- |
| 普通预览、映射预览、进入编辑 | 四个目标的坐标、宽高、字体、颜色一致；普通预览没有编辑器标记和桥接脚本。只证明这四个目标，不扩大为整页像素一致 |
| 文字、样式、图片 | 修改中文与 emoji、颜色、按钮宽度和图片地址；图片解码成功；文字撤销／重做后选区回到正确源文件位置 |
| 多文件编辑 | React 修改 `FeatureGrid.jsx`、`HeroSection.tsx`、`SecondaryNote.jsx`；Vue 修改同名三个 `.vue` 文件 |
| 静态结构 | 插入按钮、复制、删除、撤销；实际鼠标拖拽将第二张卡片移到第一张前面，检查请求为 `placement=before`，再验证撤销、重做和选区 |
| 动态边界 | 循环与同一源码的重复实例只读；动态样式拒绝静态覆盖，草稿摘要保持不变 |
| Hero CSS | 插入后实际布局为 grid，样式表含 `cayw-fade-up`；Vue template 内没有 style 标签，原 script 与 scoped style 块逐字保留；保存重启后 Hero 仍在 |
| 保存前 | 项目文件摘要逐个与原件相同，内存草稿没有提前写盘 |
| 保存后 | 写入真实组件源文件，未将响应插桩标记保存到源码；重新构建成功 |
| 停服、重启、再次编辑 | 同状态目录重启，文字和新增结构仍在；再次修改并保存成功 |
| 外部修改 | 保存旧草稿返回 409 并锁定；明确 reload 后外部文字保留，随后编辑保存成功 |
| 原交互 | 计数器仍增长；输入 `Framework interaction` 并真正提交表单，显示 `Hello, Framework interaction.` |
| 移动宽度 | 项目预览宽 390px，无新增横向溢出；最终两个框架实测 overflow 均为 0 |
| 调用记录 | 每框架 `calls.jsonl` 178 行、`server-events.jsonl` 50 行；开始和终态配对，没有未完成调用；实际能力只有 `fs.read` / `fs.write` |
| 正常清理 | 两次编辑器进程均 exit 0；脚本和收尾核对确认返回的三个服务地址均停止响应 |

桌面证据：`framework-02/react-browser-before-save.png`。移动证据：`framework-02/vue-browser-saved-mobile.png`。主线程已实际查看。截图中的动态样式拒绝提示来自明确的反例操作，不是编译失败。浏览器记录的 `errors=[]`。截图拍摄时仍可见短暂提示，因此判断保存和重启采用对应 JSON、磁盘摘要与构建结果，不能只依赖提示文字。

本轮浏览器使用 `reducedMotion=reduce`。Hero 检查证明 CSS 已编译并保留，不声称本轮测过动画播放时序。

## 错误修复与反例

以下错误均有对应修复及测试：

- React 样式原可能写成字符串，使页面无法渲染。现在写成框架要求的对象；测试实际求出对象值，浏览器也验证修改结果。React `!important` 静态修改明确拒绝，避免返回成功却无效果。Vue 仍可接受该 CSS 声明。
- 纯字面量 React style 对象原被误判为动态，导致新按钮不能复制。现在区分字面量与变量、spread、事件等真正动态内容，11 种本地组件均覆盖结构操作回归。
- Vue 插入含 style 的区块原会触发编译错误。现在把静态 CSS 写入同一单文件组件的可用样式块，或新建普通样式块，保留原 script 和 scoped style。
- Vue 项目中的 JSX 目标原会用错序列化器。现在按实际目标文件的解析结果选择 JSX 或 Vue 写法。此边缘场景有转换回归，未将其扩大为混合框架项目完整浏览器认证。
- 只有除可选 `lang` 外没有其他属性的 Vue style 块可追加，且 `lang` 限缺省、css、scss、less、postcss。`media`、`nonce`、scoped、module、src、未知属性及其他预处理语言的块保持原样，新增 CSS 使用新的普通 style 块。
- 框架 reload 重新扫描文件，新增、改名和删除后可更新清单；扫描、读取、解析或同步失败时保留原草稿和历史。
- 框架每次明确的读取、保存、reload 使用独立 Host 操作预算，并复制原根目录句柄。不会按外部路径换根，不会用新预算解除持久未知状态。
- Node 父进程快速退出时，编辑器先清理自有进程组再回收父进程；可用性检查能观察已退出进程。解析／编译停止与磁盘保存结果未知分别处理。

Rust 回归还覆盖 UTF-8 补丁边界、旧文本核对、过期元素标识、跨文件拒绝、同步失败不提交草稿、只读节点、冲突锁定，以及部分保存失败后持久保留未知状态。没有宣称多文件保存是事务或可以自动回滚。

## Magpie 授权与未完成项

用户当次授权为“允许使用 magpie 100m 预算 适配完 react vue 之后跑”。本次总上限为 100,000,000 token，包含已结算、失败和未知预留。脚本采用 React 45M、Vue 45M、预留 10M 的内部分账，最多使用该总额，不自动重试，不自动切换模型。指定路由为 `opencode-go/deepseek-v4.1-flash`，地址为 `http://127.0.0.1:3425/v1`。

当前环境未设置 `MAGPIE_API_KEY`。只检查变量是否存在，没有读取私人凭据文件、Keychain 或 shell 历史。**没有发送真实生成请求，没有产生本次真实模型用量；不能把公开预检的成功退出当作生成通过。**

主线程最终执行了：

```sh
CAYW_PLAYWRIGHT_MODULE="$PWD/.local/issue30-browser-driver/node_modules/playwright/index.mjs" \
CAYW_BROWSER_EXECUTABLE='/Applications/Google Chrome.app/Contents/MacOS/Google Chrome' \
node scripts/validate-visual-editor-magpie.mjs \
  --out .local/issue30-validation/magpie-preflight-r11 \
  --runtime .local/issue30-target/debug/cayw \
  --editor .local/issue30-target/debug/cayw-visual-editor \
  --scaffold-root .local/issue30-framework-inputs \
  --local-acceptance .local/issue30-validation/framework-02 \
  --ledger-helper target/debug/examples/framework_magpie \
  --public-only
```

结果：exit 0；本地框架门禁、源码摘要、编辑器二进制、浏览器工具和账本工具检查通过；两个宿主编写的脚手架构建成功。公开 `GET /hello` 返回 404，`GET /v1/models` 返回 200 且包含目标路由。两次 GET 不带认证头，只证明接口可达。最终状态为 `generation_started=false`、`credential_present=false`、`generation_status=skipped_missing_credential`、`acceptance_passed=false`，`generated_projects=[]`。

脚本通过公开 Runtime/IPC、逐项文件审批和持久模型预算执行生成。生成后先构建和操作普通页面，再调用唯一的 `validate-visual-editor-framework.mjs`，实际验证编辑、保存、重启和冲突恢复。只有完整结果通过才记为真实验收通过。普通可视化编辑阶段继续要求模型请求数为 0。

开发用账本示例 `crates/cayw-runtime/examples/framework_magpie.rs` 不打开原数据库。它先复制 Core 数据库和 SQLite 侧文件，再仅用离线 Provider 打开副本。`helper-selftest/ledger-4.json` 记录真实离线运行成功、`original_sqlite_opened=false`、`source_db_unchanged=true`、无请求和预留。该空会话自检只证明导出路径可运行，不证明真实模型用量结算。

请求录制器另外用本机模拟 HTTP 上游验证了两个独立目录和上游断开，共 44 项检查通过；记录不保存认证头。进程收尾 JSON 实际写入并通过 7 项检查。两份自检都记录 `external_network=false`、`model_requests=0`。它们不是 Magpie 生成证据。

最终模型验收脚本 SHA-256 为 `161c3f278a2bd7fcdc5541d9481d5ba81ac024f4e1a1f80052776754685f389d`；账本示例源码为 `561beba1ce3976af4ce4c9751117be05293f84b46054116b7a5b22a240d95264`。供本次脚本调用的 Runtime 二进制为 `bc1f01b5a9252b524a368d4d9cefe714fec2d2743f6fe23b90e69f7af5c73bf7`。

后续在当前进程具备凭据后，可沿用上述参数，去掉 `--public-only` 并改为新的真实证据目录。运行前脚本会重新核对本地源码与二进制。不重用旧 Issue 的预算目录；缺用量或仍有预留时保留记录并停止。

## 支持边界

本记录只覆盖上述 macOS ARM64、Chrome、版本和输入。Intel Mac、其他浏览器、其他 Vite 版本、Tailwind 专用修改、Svelte、Next.js/Nuxt 服务端渲染及任意构建系统未由本次记录验证。

同一源码对应多个运行实例时只读。含事件、组件或动态表达式的子树不能任意克隆、删除或移动；可独立处理的静态属性仍按节点规则修改。不支持跨文件移动。修改会刷新项目预览，不保证保留计数器等临时状态。

框架项目配置以本机用户权限执行，Node 进程和进程组不是恶意代码沙箱。多文件保存逐个检查摘要；部分写入后失败会停止并保留未知状态。源码和调用记录仍须核对，不能以刷新或更换状态目录清除未知结果。

当前剩余的任务验收条件是：在具备凭据后，实际运行已授权的 Magpie 生成，再验证生成项目的完整编辑、保存和重启。远端 Issue 未更新，不能据本地合成检查宣布扩展任务整项完成。
