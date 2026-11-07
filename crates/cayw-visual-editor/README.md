# cayw-visual-editor

在已有的 HTML/CSS、Vite React JSX/TSX 或 Vue 3 页面上选择元素、拖拽布局、插入组件和修改属性。保存后写回对应源文件。原页面不需要注册组件。编辑器是独立可选应用，Core 不依赖它；直接编辑不调用模型。

支持范围、选型依据和边界见 [可视化编辑说明](../../docs/visual-editor.md)。当前 React/Vue 与 HTML 回归结果见 [框架验收记录](../../docs/validation/issue-30-framework-editor-macos.md)。较早的 HTML 首版结果保存在[历史验收记录](../../docs/validation/issue-30-visual-editor-macos.md)。

## 启动

从仓库根目录执行。需要 Rust 1.95+。`--project` 必须是已有项目目录，`--page` 是相对于该目录的 HTML 文件。项目原有资源按该根目录提供。

先在当前终端设置访问令牌。令牌至少含 32 个可见 ASCII 字符；可以用以下命令生成：

```sh
export CAYW_EDITOR_TOKEN="$(openssl rand -hex 32)"
```

启动服务，替换示例中的项目和审计目录：

```sh
cargo run -p cayw-visual-editor --locked -- \
  --project /absolute/path/to/project \
  --page index.html \
  --state-dir /absolute/path/outside-project/editor-state
```

程序只监听本机地址，并输出一行包含 `editor_url`、`preview_url` 和 `normal_preview_url` 的 JSON。用浏览器打开 `editor_url`，在连接页输入环境变量 `CAYW_EDITOR_TOKEN` 的值。程序不会打印令牌。令牌只保存在编辑器页面内存中，刷新编辑器需要重新输入。

`--state-dir` 保存持久调用记录，必须与项目目录分离。再次打开同一编辑会话时使用同一目录。一个审计目录同时只允许一个编辑器进程持有。用 Ctrl+C 正常停止。

React/Vue 项目还需要 Node 22.12+，以及项目已有的 Vite 和框架插件。先按项目自己的锁文件安装依赖。再安装本应用的可选解析和预览依赖：

```sh
npm --prefix crates/cayw-visual-editor/framework ci
```

以下命令打开已有 React 项目；Vue 3 项目将 `react` 改为 `vue`：

```sh
cargo run -p cayw-visual-editor --locked -- \
  --project /absolute/path/to/vite-project \
  --framework react \
  --page index.html \
  --state-dir /absolute/path/outside-project/editor-state
```

项目 Vite 配置和插件以当前用户身份执行。预览使用原配置的 `base`、别名和资源。普通预览读取磁盘，编辑预览读取内存草稿；两者使用独立地址。Vite 缓存写到状态目录。HTML 模式不需要 Node。

可用参数：

| 参数 | 默认值 | 用途 |
| --- | --- | --- |
| `--project PATH` | 必填 | 项目和资源根目录 |
| `--page NAME` | `index.html` | 根目录内的 HTML 页面 |
| `--state-dir PATH` | 必填 | 项目外的持久审计目录 |
| `--addr ADDR` | `127.0.0.1:0` | 编辑器地址，端口 0 由系统分配 |
| `--preview-addr ADDR` | `127.0.0.1:0` | 项目预览地址，必须与编辑器分离 |
| `--token-env NAME` | `CAYW_EDITOR_TOKEN` | 提供访问令牌的环境变量名 |
| `--framework NAME` | `html` | 显式选择 `html`、`react` 或 `vue`，不自动猜测或降级 |
| `--node PATH` | `node` | 框架模式的 Node 可执行文件 |
| `--companion PATH` | crate 内的 `framework/companion.mjs` | 框架解析和 Vite 进程入口；独立安装二进制时须提供该目录 |

## 编辑与保存

1. 连接后点击“可视化编辑”。悬停查看边界，点击选择元素，使用“父级”定位容器。
2. 右键修改文字、图片地址、链接、颜色、宽高、字体和布局。样式只修改当前元素的内联样式。
3. 普通布局拖拽调整源码顺序或容器；绝对／固定定位元素拖拽修改位置。右侧、底部和右下角手柄调整宽高。
4. 从“+”、右键入口或组件面板插入。面板支持搜索与拖入。基础元素和 Hero、Bento、FAQ、CTA 使用本地原生 HTML/CSS。
5. 使用撤销、重做和桌面／移动宽度检查结果。切换“交互预览”操作页面原有控件。
6. 点击保存。刷新原页面或重启服务后，已保存改动仍在 HTML、JSX/TSX 或 Vue 文件中，可以再次进入编辑。

未保存草稿和撤销历史只在本次进程内存中。每次操作会刷新预览，不保留页面脚本的临时内存状态。内联样式可能覆盖原媒体查询的同名属性，需要检查移动布局。

源码在外部发生变化后，保存返回冲突并锁定修改。点击明确的“重新加载项目”会丢弃草稿并读取新源码，之后才能继续保存。

保存结果未知时，应用停止后续写入，重启也拒绝自动恢复。需要核对 `calls.jsonl`、`server-events.jsonl`、未知标记和实际文件；刷新页面或换一个审计目录不是恢复流程。查看器显示最后已知的草稿，不证明未知写入的实际结果。

## 接口边界

`start(EditorConfig)` 启动编辑器与预览，返回 `EditorHandle`。HTML 共两个本机服务；框架模式共三个本机服务，另有一个受编辑器管理的 Node 进程。调用方应在结束时等待 `shutdown()`。库消费 `cayw-host` 的公开 Rust 接口，源码读写经过明确的文件权限、预算、调用开始和结果记录。只允许当前页面或框架文件清单的 `fs.read` / `fs.write`，写入必须核对 `expected_sha256`。框架进程返回最小源码补丁，Rust 校验路径、字节边界和旧文本后才提交草稿。

编辑器 API 使用 Bearer 令牌。修改请求同时检查编辑器 Origin 和 Host。项目预览不能取得令牌；预览资源只允许受限类型、大小和相对路径，不允许越界或符号链接。项目脚本仍会运行，这不是运行恶意页面的安全沙箱。

HTML 按单个页面编辑。React/Vue 按文件清单编辑，最多 256 个文件、16 MiB 源码，单文件最多 8 MiB。一次修改只影响一个源文件，不支持跨文件移动。多文件保存逐个检查摘要；已写入部分文件后再失败，会保留未知状态并停止继续写入，不自动回滚。

React 支持原生 JSX/TSX 标签；Vue 3 支持单文件组件模板，原 script 和 scoped style 保留。动态文字、样式或属性不会被静态值覆盖。循环、条件分支、属性展开和重复运行实例只读。含事件或动态子树的结构不能任意移动、克隆或删除。无法可靠映射的脚本生成、Shadow DOM、Canvas 和子页面也只读。Svelte、Next.js/Nuxt 服务端渲染和服务端模板尚未适配。

编译进程停止后，后续修改和保存返回不可用。界面保留最后草稿供查看，重启时只读取已保存源码。该状态不等于磁盘副作用未知，不能拿重新启动来清除真正的保存未知记录。

## 本地验证

```sh
cargo test -p cayw-visual-editor --locked
cargo clippy -p cayw-visual-editor --all-targets --locked -- -D warnings
```

浏览器验收脚本会启动实际二进制，复制页面到新目录，真实操作页面，保存、停服、重启并再次保存。Playwright 属于开发机工具，不是应用运行依赖。可选设置 `CAYW_PLAYWRIGHT_MODULE` 指向开发机已有 Playwright 的 `index.mjs`，用 `CAYW_BROWSER_EXECUTABLE` 指向本机 Chrome。安装或配置 Playwright 时按其当前官方文档操作。

```sh
cargo build -p cayw-visual-editor --locked
CAYW_EDITOR_BINARY=target/debug/cayw-visual-editor \
CAYW_EDITOR_VALIDATION_SITE=site \
CAYW_EDITOR_VALIDATION_OUT=.local/issue30-browser-evidence \
node scripts/validate-visual-editor.mjs
```

输出目录必须不存在，以免覆盖先前证据。设置 `CAYW_EDITOR_VALIDATION_AGENCY` 为未接入编辑器的 StartBootstrap Agency 静态目录，会额外验证已有图片、原模态窗口和移动导航。页面来源和固定版本见验收记录。缺少该输入时结果会明确标为未运行，不能作为完整 Issue 验收。

React/Vue 源码转换有独立回归：

```sh
node --test crates/cayw-visual-editor/framework/source.test.mjs
```

仓库提供从零编写的 React/Vue 测试项目。按各自锁文件安装依赖后，把两个项目路径传给同一个完整生命周期脚本。脚本会复制输入，不在输入目录上执行编辑保存：

```sh
npm --prefix crates/cayw-visual-editor/tests/fixtures/react-vite ci
npm --prefix crates/cayw-visual-editor/tests/fixtures/vue-vite ci
CAYW_EDITOR_BINARY=target/debug/cayw-visual-editor \
CAYW_EDITOR_VALIDATION_REACT=crates/cayw-visual-editor/tests/fixtures/react-vite \
CAYW_EDITOR_VALIDATION_VUE=crates/cayw-visual-editor/tests/fixtures/vue-vite \
CAYW_EDITOR_VALIDATION_OUT=.local/issue30-framework-evidence \
node scripts/validate-visual-editor-framework.mjs
```

同样可以设置 `CAYW_PLAYWRIGHT_MODULE` 和 `CAYW_BROWSER_EXECUTABLE`。输出包括保存前后的文件摘要、桌面和移动截图、三次构建、服务停止与重启、外部修改冲突和 Host 调用记录。输出目录必须不存在。该流程不调用模型。

真实模型生成另用 `scripts/validate-visual-editor-magpie.mjs`。脚本只从当前环境读取 `MAGPIE_API_KEY`。它要求本地框架验收通过且源码、编辑器二进制与验收一致；然后通过 Runtime、审批和持久预算生成两个项目，再运行上面同一个完整编辑脚本。它不会自动重试或切换模型。缺少凭据或指定 `--public-only` 时，只做公开探测和本项目脚手架构建，不会记为生成验收通过。本次 100M token 授权与当前未运行状态见[框架验收记录](../../docs/validation/issue-30-framework-editor-macos.md)；后续独立付费运行仍需对应授权。

首发只验证 macOS。其他 macOS 架构和未实际运行的场景不由本机测试外推。
