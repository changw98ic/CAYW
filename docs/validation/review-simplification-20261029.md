# 重复逻辑、OCR 配置与过时说明修正：2026-10-29

本记录覆盖本轮提出的 7 项问题。实际平台为 macOS arm64（Darwin 27.0.0）。不将这些检查写成全部 F01–F20 或整个 Issue 的验收。

## 实际修改

| 问题 | 修改和结果 |
| --- | --- |
| WebUI 存在只供测试的格式化函数 | 删除 `formatContextOverview`、`formatRunInspectorSteps`、`formatFleetMembers` 及其独立格式化测试。页面只有原有 DOM 渲染路径。指标、成员字段和缺失 `outcome` 的断言移到已有 DOM 测试；缺失状态显示 `unknown`。 |
| OCR 配置错误被空 fixture 隐藏 | 环境配置解析不再创建空 `FixtureOcrEngine`。未配置返回明确不可用错误。错误配置保留配置值，Tesseract 初始化失败保留原始 `VisionError`。测试显式注入 fixture。配置测试使用内部函数，没有为测试新增公开配置接口或类型别名。 |
| TUI 一页会话解析两次 | 第一遍收集已解析的 `SessionView` 并检查重复 ID，整页通过后直接应用这些值。格式错误或重复 ID 仍在修改现有状态之前失败。 |
| Trace 同次执行重复校验整张图 | `evaluate` 和捕获入口完成结构校验后调用内部脚本构造函数。公开 `script_for_plan` 仍在自身入口校验。捕获响应匹配、未知结果核对和节点源码校验继续承担各自职责。 |
| Vision 重复检查固定配置与像素长度 | 构造器继续检查 limits，`ImageBuffer::from_rgba8` 继续检查像素长度。删除执行层的重复部分，保留外部输入解析和图片字节限制。 |
| Panel 二次检查不能保证并发卸载安全 | 在一次注册表读取中解析面板和目标区域。打开操作使用这次读取的快照。注释说明实际语义，不宣称返回之后仍持有注册表锁。 |
| 注释落后于实现 | `web.extract` 注释改为按 URL 下载并提取正文。Vision 注释说明未配置时返回不可用后端，测试后端需明确选择或注入。没有为迁就注释增加功能。 |

## 可复跑检查

证据根目录：`.local/integration-validation/finish-search-rust-20261029/`。

- `review-checks-01/verification.json`：`cargo +1.95.0 test -p cayw-tui -p cayw-vision -p cayw-webui -p cayw-trace-debug -p cayw-panel -p cayw-web-access --locked` 退出 0，265 项通过，源码清单前后一致。原始输出为同目录 `review-tests.stdout` 和 `review-tests.stderr`。
- `webui-checks-03/verification.json`：最终页面源码上的 86 项 Node 测试全部通过，相关源码前后一致。原始输出为同目录 `node.stdout` 和 `node.stderr`。复跑命令为 `python3 -I .local/integration-validation/finish-search-rust-20261029/check-webui.py --name <新目录名>`。
- WebUI 使用现有 Node 测试和模拟 DOM，另有本机 HTTP 测试服务。它执行实际 `app.js` 页面事件处理和渲染代码，不再调用被删除的格式化函数。该结果不代表真实浏览器或视觉验证。
- `workspace-checks-02/verification.json`：原七项修改的 check/test/Clippy 和离线示例通过，2,477 项通过、0 失败、11 项默认 ignored；运行期间新 Runtime 测试的 import 换行发生变化，因此原收据如实保留 `source_unchanged=false`。`workspace-final-binding-01/verification.json` 对两份留存源码证明唯一变化是空白，并绑定最终 fmt 和定向回归。
- 后续原 Session 恢复又修改 Core 流式记录路径。最终 `workspace-checks-03/verification.json` 全部四个命令退出 0，2,479 项通过、0 失败、11 项默认 ignored，源码清单前后一致。`workspace-final-output-summary-01/verification.json` 核对原始 stdout/stderr 摘要和数量。默认 ignored 包括 8 个父测试使用的子进程入口和 3 个需显式条件的独立入口，不将它们写成本轮直接执行通过。
- 最终 Rust 格式检查和 `git diff --check` 通过。检查范围只绑定各收据列出的源码和本机平台。

本轮这些检查不调用真实模型，不操作原 Rust Session 数据库。原 Rust Session 恢复的真实执行单独记在 [搜索与 Rust 恢复记录](f01-f20-search-rust-20261029.md)。
