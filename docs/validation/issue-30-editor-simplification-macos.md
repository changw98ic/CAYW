# Issue #30 编辑器精简与重测

日期：2026-10-24。范围：删除当前可视化编辑器中已确认的重复检查、无效分支和误拦截正常操作的限制，并重新验证 HTML、React、Vue 编辑流程。工作树为 `issue/30-visual-editor`，代码未提交、推送或部署。

本轮检查通过。工作区测试通过 1,815 项，另有 7 项默认忽略；末次修正后，编辑器 115 项测试、工作区编译和严格代码检查再次通过。源码转换测试通过 35 项。React/Vue 浏览器通过 8 组场景、58 项具名检查，HTML 通过 6 组场景、41 项检查。Playwright MCP 另完成 11 项实际界面检查。

本记录只证明下面列出的源码、构建、平台和输入。较早的[框架验收](issue-30-framework-editor-macos.md)与 [HTML 首版验收](issue-30-visual-editor-macos.md)保留各自历史范围。

## 删除和修正的内容

| 修改 | 实际结果 |
| --- | --- |
| 框架节点只在解析入口校验一次 | 删除四处重复校验，以及把节点全部字段复制回中间类型再校验的代码 |
| 框架启动统一使用服务入口已经检查过的路径和框架名 | 删除内层重复的路径规范化、状态目录创建和框架名处理 |
| reload 只解析一次 JSON | 使用已经解析出的值判断是否为对象，继续接受空请求体与 JSON 对象 |
| reload 不再重复检查同一批已读取源码的大小 | 文件清单限制仍在扫描入口，单文件和总字节限制仍在读取入口；编辑后新增内容仍检查总量 |
| 源码编辑后只解析一次 | 使用同一次解析的节点恢复选区，继续拒绝修改后的无效语法 |
| 插入位置和 revision 统一检查 | 删除同一父容器的第二次检查、重复 revision 检查和不可到达的缺失文件回退 |
| `.mts` / `.cts` 使用 TypeScript 解析 | 合法类型声明不再阻止项目中其他静态元素的映射与编辑；真实语法错误仍拒绝 |
| 固定 shutdown 请求直接生成 | 删除不可能发生的 JSON 序列化失败和 32 MiB 超限分支，保留退出等待和自有进程组清理 |
| 样式统一在提交入口检查 | 删除 `applyStyle` 的第二层静默过滤，错误值直接说明原因 |
| 删除字号、宽高、行高与网格列数的任意上限 | 实际浏览器通过宽度 5001px、字号 250px、行高 8、16 列网格；网格列数仍须为正整数 |
| 样式输入使用浏览器的 CSS 有效性检查 | 实际通过 `clamp(180px, 25vw, 320px)`；裸数字长度补 `px`，行高倍数不补单位 |
| 普通编辑失败不再误标保存未知或编译进程停止 | 临时 503 只显示错误，明确的 `companion_unavailable` 才进入编译服务不可用状态 |
| 丢失编辑响应后读回草稿 | 不重发原操作；若读取也失败，返回连接页，重新连接后读取服务保留的草稿 |
| 删除预算常量的重复 NaN 断言 | 总额等式检查继续有效，本轮没有模型请求 |

文件权限、磁盘内容摘要、补丁位置和原文核对、动态源码目标、部分保存结果未知、自有进程清理等检查保留。这些检查分别防止越权写入、覆盖外部改动、修改错误位置、覆盖动态表达式及重复执行未知副作用。资源大小限制也保留。本轮不声称已穷尽整个仓库中的所有冗余。

## 源码、构建和环境

| 项目 | 实际值 |
| --- | --- |
| 工作树 | `/Volumes/ORICO/CAYW/.claude/worktrees/issue-30-visual-editor` |
| 分支 / 基线 HEAD | `issue/30-visual-editor` / `cc9092a32f26cea0030fc13f847a5de5612131ef` |
| 系统 / 架构 | macOS 27.0.1，Build 26A434，Darwin 27.0.0；ARM64 |
| Rust / Cargo | 1.95.0；Clippy 使用 `+1.95.0` |
| Node / npm | v24.4.1 / 11.4.2 |
| 浏览器 | Google Chrome 155.0.8059.85；Playwright 1.61.1 |
| 编辑器构建 | debug、默认 feature；`.local/issue30-target/debug/cayw-visual-editor` |
| 二进制 SHA-256 | `f74d14d34b0c80cbe7612de9571d917f73182dc1ca04d9dfa03e2a6585fbdb63` |
| React/Vue 运行时间 | UTC `2026-10-24T06:20:26.438Z` 至 `2026-10-24T06:20:55.958Z` |
| HTML 运行时间 | UTC `2026-10-24T06:13:02.492Z` 至 `2026-10-24T06:13:26.168Z` |
| MCP 运行时间 | UTC `2026-10-24T06:21:47.730Z` 至 `2026-10-24T06:21:50.923Z` |

原始证据均在工作树的 `.local/issue30-validation/`。这些目录不进入产品源码；复跑使用新目录。

- `simplified-01/`：第一次完整工作区命令、原始日志、返回码及源码摘要。保留 Clippy 首次失败和运行中浏览器脚本变化的事实。
- `simplified-final-01/`：末次修正后的检查、编辑器测试、构建及 `final-verification.json` 汇总。
- `framework-simplified-03/`：最终 React/Vue 完整流程、项目副本、源码摘要、构建、截图、启动停止和调用记录。
- `html-simplified-01/`：HTML 完整回归、实际保存差异、截图和调用记录。
- `mcp-simplified-01/`：Playwright MCP 返回结果、实际保存的 HTML、截图、预期故障日志和进程停止记录。复跑驱动为证据根目录下 `validate-simplified-mcp-01.js`。

汇总脚本重新核对原始日志摘要、当前源码、同一二进制、浏览器返回结果、六次框架构建和调用配对。最终 `passed=true`。框架与 HTML 的 `source_changed_during_run=[]`，收尾核对的源码摘要也无差异。

| 文件或证据 | SHA-256 |
| --- | --- |
| `framework-simplified-03/summary.json` | `fd3de87a077f81493e7b8fc957228b4a3d7b949f896bce0c949b426806b0c2fa` |
| `framework-simplified-03/source-receipt.json` | `33dea6865bb82737a961ba76a6b64f2f0c78166426654e886116e82f534e0ec2` |
| `html-simplified-01/summary.json` | `89eb94c666c002fce9037ff11d720f02621e513c1316bccb46f9420072fdf0d2` |
| `mcp-simplified-01/verification.json` | `bff8b6ce054eb8f29b8772ab2f753b453e6ce09b700703bd40fcf111b05e675e` |
| `crates/cayw-visual-editor/src/framework.rs` | `06d8965bc15024be1af709d506583f7b92dd1059a33822c2703c9a52487d579a` |
| `crates/cayw-visual-editor/assets/editor.js` | `0ac19297b00f2ba79cd3522f2d87589b427efeb16d7b247d5006963ca1ad46a3` |
| `crates/cayw-visual-editor/framework/source.mjs` | `f2640d00e6db130e36f4be4c54b0e16f4db16c99dbd72841e3a90ad1df9903e9` |

## 命令和结果

命令均在本工作树执行。Cargo 入口为 `/Users/chengwen/.cargo/bin/cargo`，构建目录设为工作树内 `.local/issue30-target`。

| 命令 | 最终采用的证据 | 结果 |
| --- | --- | --- |
| `cargo fmt --all -- --check` | `simplified-final-01/fmt.log` | exit 0 |
| `cargo check --workspace --locked` | `simplified-final-01/check.log` | exit 0 |
| `cargo test --workspace --locked -- --test-threads=4` | `simplified-01/tests.log` | exit 0；1,815 passed、0 failed、7 ignored |
| `cargo +1.95.0 clippy --workspace --all-targets --locked -- -D warnings` | `simplified-final-01/clippy.log` | exit 0 |
| `cargo run -p cayw-agent --example offline --locked` | `simplified-01/offline.log` | exit 0 |
| `cargo test -p cayw-visual-editor --locked` | `simplified-final-01/editor-tests.log` | exit 0；115 passed、0 failed |
| `node --test crates/cayw-visual-editor/framework/source.test.mjs` | `simplified-01/source.log` | exit 0；35 passed、0 failed |
| `cargo build -p cayw-visual-editor --locked` | `simplified-final-01/build.log` | exit 0 |
| `node --check crates/cayw-visual-editor/assets/editor.js` | `simplified-final-01/editor-js.log` | exit 0 |
| `node --check scripts/validate-visual-editor-magpie.mjs` | `simplified-01/magpie-js.log` | exit 0；没有模型请求 |
| `node scripts/validate-visual-editor-framework.mjs`，环境见下文 | `framework-simplified-03/summary.json` | exit 0；8 组、58 项检查 |
| `node scripts/validate-visual-editor.mjs`，环境见下文 | `html-simplified-01/summary.json` | exit 0；6 组、41 项检查 |

工作区计数取 269 个 Cargo 顶层 `Running` / `Doc-tests` 目标的最后一条结果，排除子进程重复输出。115 项编辑器测试已包含在工作区总数中，末次修正后另行重跑，不能再加到 1,815 上。

完整工作区测试结束后，Rust 的唯一变化是按 Clippy 要求把 `Host::new(&root, ...)` 改为 `Host::new(root, ...)`，删除多余借用。随后重新运行工作区编译、全目标 Clippy、编辑器 115 项测试和构建。前端随后补齐读取失败时的重新连接恢复；它由最终 HTML、React/Vue 和 MCP 浏览器记录验证。源码转换模块在其 35 项测试之后没有变化。

`simplified-01/checks.json` 的总状态仍为 false：首次 Clippy 报 `needless_borrows_for_generic_args`，且浏览器脚本在该轮运行中有过修改。原始记录没有改成成功。最终汇总逐项引用实际通过的命令，并使用后续复跑证明修正。最后一次仅调整了浏览器脚本的容器选择和检查定位信息；最终框架记录绑定该脚本，没有再用旧脚本摘要替代。

提交整理时，仅删除 `crates/cayw-visual-editor/assets/editor.css` 文件末尾的一个空行，没有修改 CSS 规则。上述源码与构建摘要仍对应测试时的原件；该空白整理后未重新构建或重跑浏览器，不将原摘要写成提交后文件的摘要。

框架复跑命令：

```sh
CAYW_PLAYWRIGHT_MODULE="$PWD/.local/issue30-browser-driver/node_modules/playwright/index.mjs" \
CAYW_BROWSER_EXECUTABLE='/Applications/Google Chrome.app/Contents/MacOS/Google Chrome' \
CAYW_EDITOR_VALIDATION_OUT=.local/issue30-validation/framework-simplified-new \
node scripts/validate-visual-editor-framework.mjs
```

HTML 复跑命令：

```sh
CAYW_PLAYWRIGHT_MODULE="$PWD/.local/issue30-browser-driver/node_modules/playwright/index.mjs" \
CAYW_BROWSER_EXECUTABLE='/Applications/Google Chrome.app/Contents/MacOS/Google Chrome' \
CAYW_EDITOR_BINARY=.local/issue30-target/debug/cayw-visual-editor \
CAYW_EDITOR_VALIDATION_AGENCY=.local/issue30-pages/agency \
CAYW_EDITOR_VALIDATION_OUT=.local/issue30-validation/html-simplified-new \
node scripts/validate-visual-editor.mjs
```

框架输入仍是从零编写的 Vite React JSX/TSX 与 Vue 3 测试项目，实际路径为 `.local/issue30-framework-inputs/{react-vite,vue-vite}/`。HTML 使用仓库 `site/` 和已有 Agency 固定版本副本。本轮没有下载新的第三方项目。依赖版本与许可证见[此前记录](issue-30-framework-editor-macos.md#输入和依赖)。

## 浏览器实际验证

两个框架均完成多文件修改、保存前磁盘不变、保存后构建、停服重启、再次编辑、外部修改冲突及明确重新加载。原计数器、表单提交、图片解码、静态结构拖拽和移动宽度继续通过。原始、首次保存后、冲突恢复后共六次构建全部 exit 0。

新增故障场景使用浏览器请求拦截实际执行：

1. 在请求发送前中断网络，重新读取草稿后可以继续编辑。
2. 返回不含未知结果标记的临时 503，界面不误锁为编译服务停止。
3. 先把操作转发到服务端并取得已应用结果，再丢弃响应。界面读回草稿，操作仅执行一次，且可撤销和保存。
4. 丢弃操作响应后，再中断草稿读取。界面返回连接页；重新连接后草稿仍在，操作没有重放，也没有被误标为磁盘写入未知。
5. 实际输入超出旧上限的四组数值及 `clamp()`，核对生成样式，然后撤销，确认后续编辑流程继续通过。

每个框架的 Host 调用日志为 178 行，编辑服务操作日志为 78 行，开始与终态配对，未完成调用为 0。四次框架进程停止均 exit 0，预览服务停止响应。MCP 启动的独立编辑器也经自有子进程句柄正常停止，两个地址均不再响应。

Playwright MCP 的 11 项检查另在最小 HTML 项目运行，并实际将 `Verified after recovery 中文 😀` 保存到磁盘。磁盘摘要与返回摘要一致，源码没有编辑器标记。主线程已查看 `mcp-simplified-01/saved.png` 和 Vue 移动截图。MCP 控制台的五条网络/503 错误来自主动注入的故障；未捕获的页面脚本异常为 0。

保留的失败记录：`framework-simplified-01/` 在点击网格容器时选到了内部文字；`framework-simplified-02/` 又暴露测试脚本重复点击当前元素路径造成表单更新的时序问题。脚本改为仅在需要切到父容器时点击元素路径，最终 `framework-simplified-03/` 完整通过。MCP 首次驱动中的文件导入与预览等待错误也保留在返回结果记录中；没有修改产品来迁就这些驱动错误。

## 验证范围

本轮只验证上述 macOS ARM64、Chrome 和输入。没有把合成项目写成真实模型生成结果，也没有验证 Intel Mac 或其他浏览器。

本轮真实模型请求为 0。此前授权的 Magpie 100M token 总预算继续有效；缺少凭据的真实生成验收仍按此前记录保留未完成，不作为本次精简重测的额外阻塞条件。普通编辑阶段没有模型请求。

GitHub Issue 状态未修改。本轮交付是已确认问题的精简与重测，不宣布整个 Issue #30 结项。
