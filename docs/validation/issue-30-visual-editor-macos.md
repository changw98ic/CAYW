# Issue #30 可视化编辑验收

日期：2026-10-24（本地日期；最终浏览器运行的 UTC 时间为 2026-10-23 16:26–16:27）。范围：[Issue #30](https://github.com/changw98ic/CAYW/issues/30) 的首版 HTML/CSS 路线。

首版 HTML/CSS 路线已完成 macOS ARM64 本地交付与验收。已有页面进入编辑、选择元素、拖拽、插入组件、右键修改、保存源码、停服重启和再次编辑均已实际运行。浏览器六组场景、41 项具名检查通过；最终完整工作区测试通过 1,783 项，格式、编译、严格代码检查和离线示例全部退出 0。具体输入、结果和限制如下。

本次代码位于独立工作树，尚未提交或推送。GitHub Issue 仍为 OPEN，远端正文和勾选状态未同步。没有调用真实模型或付费 Provider，没有修改部署、许可证或外部自动化。

## 1. 源码与环境

| 项目 | 实际值 |
| --- | --- |
| 工作树 | `/Volumes/ORICO/CAYW/.claude/worktrees/issue-30-visual-editor` |
| 分支 | `issue/30-visual-editor` |
| 基线 HEAD | `cc9092a32f26cea0030fc13f847a5de5612131ef`；未提交改动另由逐文件摘要绑定 |
| 系统 | macOS 27.0.1，Build 26A434，Darwin 27.0.0 |
| 架构 | arm64；Rust 目标 `aarch64-apple-darwin` |
| 工具链 | rustc 1.95.0 (`59807616e 2026-04-14`)，Cargo 1.95.0 |
| 浏览器 | 本机 Google Chrome 155.0.8059.85 |
| 验收驱动 | Playwright 1.61.1、Node v24.4.1；仅为开发机工具，不进入应用依赖 |
| 构建 | debug、默认 feature；`CARGO_TARGET_DIR="$PWD/.local/issue30-target"` |
| 最终二进制 SHA-256 | `df5c94e61d26d4cd96d0fc6161b6c27446412fce00a7e7a6cb01464c7d70df5b` |
| Cargo.lock SHA-256 | `38ed28a161c12115ed888015bee48f1542e656d814ec3bda3467a9243de720a8` |
| 浏览器运行 | UTC `2026-10-23T16:26:23.966Z` 至 `2026-10-23T16:27:10.154Z` |

原始证据根目录为工作树下 `.local/issue30-validation/`。最终浏览器证据位于 `lifecycle-final-01/`。该目录包含逐场景 JSON、截图、页面副本、实际源码差异、进程启动参数、停止结果和 Host/服务审计记录。`.local` 不进入产品源码；复跑会生成新目录。

`lifecycle-final-01/source-receipt.json` 记录 Cargo 清单、锁文件、编辑器产品源码、前端资源、测试和三个验收脚本，共 17 个明确路径的 SHA-256。运行前后这些路径及二进制均未变化，`source_changed_during_run=[]`。这不等于追踪了全部外部构建输入。验收结束后只整理说明文档和证据，不用后写的说明覆盖原始运行记录。

关键原始证据摘要：

| 文件（相对证据根目录） | SHA-256 |
| --- | --- |
| `lifecycle-final-01/source-receipt.json` | `6c542f806a54c749e189745bfda96b7f326f552d5e72b8ca0a612fb213d0dda2` |
| `lifecycle-final-01/summary.json` | `e91415cb56b48c89d52043ab32deed11a9b3872829b051dcefca00b2f18852eb` |
| `workspace-test-final.log` | `ed58fb75608db6a1c38b1d8679839ff7d9120980da873fe5867e76890c932313` |
| `workspace-clippy-final.log` | `4e634f86a43f3c50cce4f6be4ed1f58dd3f9cafe2668e3c00307df71042c0006` |

`final-delivery.json` 另记录全部 23 个交付改动文件和最终命令日志的摘要。收尾再次核对 17 个产品与验收路径仍与浏览器运行时相同。文档本地链接和全部交付文件的空白格式已检查。本任务启动的固定端口服务均已停止；额外 MCP 复核服务也以 exit 0 结束。

新增注册表依赖只有 `html5gum 0.8.3`（MIT）及其 `jetscii 0.5.3`（MIT OR Apache-2.0），均与本项目 Apache-2.0 兼容。其余新声明的直接依赖已在锁文件中存在，本次没有升级已有注册表版本。原生组件由本项目实现，未复制 shadcn/ui、Magic UI、React Bits 或旧项目代码。

## 2. 命令与结果

命令均在上述工作树执行。Cargo 实际入口为 `/Users/chengwen/.cargo/bin/cargo`。下表日志名相对证据根目录。

| 命令 | 日志 | 实际结果 |
| --- | --- | --- |
| `cargo build -p cayw-visual-editor --locked` | `editor-build-final.log` | exit 0，无编译 warning |
| `cargo fmt --all -- --check` | `workspace-fmt-final.log` | exit 0 |
| `cargo check --workspace --locked` | `workspace-check-final.log` | exit 0 |
| `cargo test --workspace --locked -- --test-threads=4` | `workspace-test-final.log` | exit 0；268 个顶层目标汇总 1,783 passed、0 failed、7 ignored |
| `cargo +1.95.0 clippy --workspace --all-targets --locked -- -D warnings` | `workspace-clippy-final.log` | exit 0 |
| `cargo run -p cayw-agent --example offline --locked` | `offline-final.log` | exit 0；`add` 返回 5，最后 `5 messages, running=false` |
| `node scripts/validate-visual-editor.mjs`，环境见下文 | `lifecycle-final-01.log` | exit 0；六组场景全部通过，运行期间源码及二进制未变化 |

测试汇总采用每个 Cargo `Running` / `Doc-tests` 目标的最后一条结果，排除父测试启动的子进程重复汇总。新编辑器共 83 项通过：库单元 27、CLI 单元 3、源码引擎 33、服务 20。7 个默认忽略入口属于原有工作区，本轮没有额外以 `--ignored` 全部执行；它们不作为新增通过项。工作区测试仍报告 Agent、Content、Runtime 的 `offline` 示例输出文件重名提示，测试正常退出；随后按包名实际运行 Agent 示例成功，未把该提示写成已经修复。

最终浏览器命令：

```sh
CAYW_PLAYWRIGHT_MODULE="$PWD/.local/issue30-browser-driver/node_modules/playwright/index.mjs" \
CAYW_BROWSER_EXECUTABLE='/Applications/Google Chrome.app/Contents/MacOS/Google Chrome' \
CAYW_EDITOR_BINARY='.local/issue30-target/debug/cayw-visual-editor' \
CAYW_EDITOR_VALIDATION_AGENCY='.local/issue30-pages/agency' \
CAYW_EDITOR_VALIDATION_OUT='.local/issue30-validation/lifecycle-final-01' \
node scripts/validate-visual-editor.mjs
```

复跑时输出目录必须换成尚不存在的路径。脚本拒绝覆盖旧证据。服务使用系统分配的本机端口，编辑器与项目预览分开。脚本使用公开的本地验收令牌，未使用用户凭据。启动令牌只通过环境变量提供，未写入页面、URL、浏览器存储或审计正文。

## 3. 真实页面与合成输入

### 仓库 site 页面

输入为基线 HEAD 的 `site/` 工作副本。它已有卡片、按钮、布局与 JavaScript 导航，没有编辑器标记或预注册组件。原文件摘要：

| 文件 | SHA-256 |
| --- | --- |
| `index.html` | `59821d18600022ae927c00ea4330d40de0f8a14d9cddaa8fbc32ec0dc3e8a9f9` |
| `styles.css` | `e33dde1bd4f8e7c3da08cdc18eadefd21c71d7f62991803dd222941967406af8` |
| `script.js` | `e8162374c212abfc6648cbe6d98623494b4fb432d7973628336cbca28813441a` |

同为 976px 预览宽度时，未注入的普通页面、注入映射后的交互预览、进入编辑后的页面，四个目标的坐标、宽高、字体和颜色全部相同。四个目标是首屏名称、按钮组、Bento 容器和原有卡片。截图与实际测量共同使用，不把四个元素的几何对照写成全页面像素一致证明。

原页面在该宽度已有 10px 横向溢出；编辑后仍为 10px。390px 移动宽度原有 1px 溢出，编辑后仍为 1px。原页面 1440px 的独立基线没有横向溢出。这些数值不表示编辑器修复了原站布局。

`site/` 没有原生 `<img>`。已有图片另用下一个真实页面验收。

### StartBootstrap Agency

来源：[StartBootstrap/startbootstrap-agency](https://github.com/StartBootstrap/startbootstrap-agency)，固定提交 `b2d5d5c6b1396bc5b0e8dd92c693b106cfb7cee1`，MIT。上游归档 SHA-256 为 `9f5d2eb63073aab43752f9a750f0412bb8924003948ec8e20703a30251e59b94`；原 `index.html` 为 `3b89a428da39a6f1bb2b280788a15c9156184d1292ee5303329ae85af46e480e`。页面先以原始状态启动，再复制到最终验收目录，不预先增加编辑器接入代码。

通过真实指针选择原有团队图片，将 `assets/img/team/1.jpg` 换为 `assets/img/team/2.jpg`，等待图片解码成功，选择父级，再修改原有标题并保存。原 portfolio 图片有页面自带悬停覆盖层，因此没有通过隐藏覆盖层来伪造直接点击图片。

保存后切到交互预览，原 Bootstrap 模态窗口正常打开和关闭。390px 下原移动导航展开，横向溢出为 0，30 张图片已加载。

原页面的外部脚本、Google Fonts 和 FontAwesome 请求保持存在。Bootstrap、FontAwesome 和表单脚本实际返回成功，部分字体请求出现 `ERR_CONNECTION_CLOSED` 或导航取消；完整资源状态保存在 `agency-image.json`。没有提交外部表单。本项不证明完全离线运行、外部字体全部可用或外部服务业务成功。

### 合成反例页面

`validate-visual-editor.mjs` 明确生成独立 fixture。它包含脚本生成内容、重复克隆的映射、模板文本、Shadow DOM、Canvas、iframe、绝对定位元素，以及带 padding/border 的 `content-box` 容器。它只证明具名边界，不充当未接入编辑器的真实页面证据。

## 4. Issue 验收对照

| 交付要求 | 实际操作与结果 |
| --- | --- |
| 已有页面直接进入编辑 | 普通原页面与编辑预览在相同宽度下对照；进入编辑没有改变四个测量目标的内容与布局；无需重建或注册原组件 |
| 选择文本、按钮、图片、卡片、容器和父级 | site 实际选择文本、按钮、article 卡片和容器；Agency 实际选择原有图片和父级；插入、移动和跨容器后核对服务端 `selected_id` 与实际元素、选框一致 |
| 拖拽与落点 | 真实 mouse 操作调整按钮源码顺序；有插入线；将输入框移动到新容器；从面板拖入按钮，实际落点优先于之前选区 |
| 快捷新增 | 选区“+”插入输入框，面板点击插入容器，面板拖入按钮；11 种组件目录可用并可搜索 FAQ；原生 HTML/CSS 无需增加前端框架 |
| 颜色、尺寸、字体和内容 | 右键修改文字色、背景色、边框色、宽高、字体、字号、字重、行高及文字；实时核对 computed style；手柄拖动 24px，宽度从 180px 变为 204px |
| 撤销与重做 | 分别核对文字、文字颜色和源码顺序；恢复后实际页面内容与布局对应 |
| 保存、刷新与重启 | 磁盘 HTML 变化；普通预览响应与磁盘内容相同；正常停止后，同项目、同审计目录重开，文字、样式与结构保留，并再次修改保存 |
| 原有交互与移动布局 | site 移动导航、插入输入框，Agency 原图片、模态窗口和移动导航可操作；移动宽度没有增加已测横向溢出 |
| 动态区域与邻区 | 动态生成、Shadow DOM、Canvas、iframe、重复克隆和模板区域明确只读，无尺寸手柄，保留渲染；相邻静态文字仍可修改保存 |
| 直接操作不调用模型 | 浏览器请求只使用编辑器接口，Host 能力仅为 `fs.read` / `fs.write`；产品操作不接入 Provider；本次真实模型调用为 0 |
| 外部变化不能被旧草稿覆盖 | 创建草稿后从外部改 HTML，保存返回 409 并锁定；显式 reload 后读取新源码，保留外部内容，再编辑保存 |

额外浏览器反例通过：

- 原页面伪造 Window 操作消息、用相同当前 URL 再建立 MessageChannel，均未产生编辑 API 请求或修改草稿。
- 既有 Georgia 字体和不透明 RGB 红色正确回显；直接应用半透明黑色没有丢失 alpha。
- `content-box` 外框宽度由 254px 变为 278px，高度保持 94px，没有重复加入 padding/border。
- 拖动离开 iframe 后松手，拖影与落点提示被清理，没有提交误操作。
- 绝对定位元素从 `left:40px; top:40px` 移至 `80px; 70px`，保留 `position:absolute` 与原 `translateX(2px)`。

41 项检查是脚本中的具名断言数，不是 41 个独立真实项目，也不是全浏览器或全框架覆盖。

## 5. 源码写回与持久记录

首轮保存后的 site HTML SHA-256 为 `193a7c2c818d21885457101946f4574002c40b74a7e24a84ee8afdfcab4e6aeb`。`site-source.diff` 显示文字、内联样式、按钮顺序和新增控件确实写回 HTML。原 CSS、JS 摘要不变。保存源码没有 `data-cayw-node`、只读原因或桥接脚本标记。

外部修改后的 HTML 摘要为 `7a93d972a602cb358df8ccd0f522b33aede09db9eadb53f1bb675835775eb7d9`。冲突重载并再次保存后为 `40d62319e363894ffaa5aba85c231302b60bbaca1924956df7e2766199c2f7fd`。外部新增节点仍在，旧草稿文字没有覆盖它。

Agency 保存后的 HTML 为 `b2a6d9e0389fc16aa678dc0500ecfd4a4925b65bb4999788184e5abc941292f0`。

最终 runner 结束时的审计：

| 项目 | Host 行数 / 未结算 | 服务行数 / 未结算 |
| --- | --- | --- |
| site（含重启、冲突） | 16 / 0 | 60 / 0 |
| Agency | 4 / 0 | 6 / 0 |
| 合成反例 | 6 / 0 | 12 / 0 |

三组均实际记录 `fs.write`，开始与结果配对，没有 unknown，没有持久化访问令牌。四个服务生命周期全部 SIGINT 正常结束，exit 0，没有强制杀死。

验收后通过 Playwright MCP 再次打开最终保存页面，核对保存文字、Georgia 字体、右键菜单和两个来源地址。截图为证据根目录的 `final-mcp-source-restored.png`，结果为 `final-mcp-review.json`。这次只读重开增加了一个 Host 读取配对。`final-mcp-audit-history.json` 明确记录增量；`site-run-end-calls.jsonl` 和 `site-run-end-server-events.jsonl` 保留原 runner 结束时的精确前缀，摘要与原回执相同，不重写原回执。

## 6. 独立复核与回归

调研、实现和复核使用用户授权的 Workflow。最终只读复核采用三个方向分别检查，再由另一执行者尝试反证；原始结果位于本会话 workflow `wf_753f6565-d39` 的 journal。已确认问题交回各自实现任务修复后，重新构建并完成本记录中的最终运行。

新增回归覆盖：旧草稿元素标识被拒绝，结构变化后不误改重新编号元素；并发修改与 document/preview 发布保持一致；显式结束记录仍为 unknown 时重启拒绝；Drop 取消服务；state 路径别名在拒绝前不创建目录；桥接先于原页面脚本且保留 head 属性；模板后置文本、rawtext 与含 script 的结构操作受限；审计标识不依赖墙上时钟。

早期失败没有改写为通过：Playwright 自带 Chromium 下载断线，最终使用已安装 Chrome；Agency 初次点击命中悬停层、菜单遮住父级按钮、模态关闭选择器不匹配，以及重复使用已经保存的输入造成无新改动，均在验收脚本中修正。最终使用新的页面副本和新的证据目录。最终 MCP 首次复核过早读取 iframe 列表，等待当前预览完成后复核通过，未改产品来绕过问题。

## 7. 支持边界

- 只验证 macOS ARM64、本机 Chrome 和上述输入。Intel macOS、其他浏览器未验证；Linux、Windows 不在首发范围。
- 首版编辑单个 HTML 文件。React/TSX、Vue、Svelte、服务端模板和构建工具尚无适配。
- 样式写当前元素的内联 style，不改共享 CSS；内联样式可能覆盖媒体查询中的同名属性。每次修改刷新预览，不保留页面脚本临时内存。
- 普通布局按源码结构移动；绝对／固定定位按适用位置属性处理。无法可靠定位的区域保持只读；含 script 的包裹结构不能复制、移动或删除。
- HTML 源码范围、页面大小、节点数、历史、请求体、Host 调用数和字节均有限制。Host 调用审计超过 64MiB 会拒绝继续打开并要求核对；服务操作审计按行重放，不因累计超过 8MiB 拒绝正常会话。
- 编辑器与项目预览地址分离并使用私有消息端口，但原页面脚本仍执行。这不是运行恶意页面的安全沙箱。
- 未确认保存结果会保留 unknown 并阻止继续写入。重启、刷新或更换审计目录不构成恢复；必须核对真实文件与持久记录。
- Host 摘要与文件锁检查不等于和任意外部编辑器之间的文件系统事务。跨程序并发写入仍需使用者协调。
- 未做性能比较，不从这次功能验收推断延迟或吞吐收益。
