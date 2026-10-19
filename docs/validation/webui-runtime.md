# WebUI 本地运行与恢复验证

> 2026-10-19 起测试工具改名为 verifyTool，代码包 `cayw-harness` 改为 `cayw-verify-tool`，`cayw-modules` 改为 `cayw-plugins`。本记录保留当时的原始命令和路径，复跑时请换用新包名。

2026-10-19 在 macOS 27.0.1 / ARM64 上验证了网页和本地网关：工具结果先显示实际状态与文件内容，完整执行记录默认折叠；正常运行和等待审批不显示异常警告。手机宽度下会话选择可收起，输入区不遮住历史，新建会话会更新数量。折叠记录的展开状态在下一次轮询后保留。

本记录保留先前 86 项 JavaScript 和 19 项 WebUI Rust 测试，并补充一次绑定完整构建记录、保留可执行文件的 10 组浏览器复验。新复验的 10 组均通过，5 次服务启动（包括一次脚本失败）均已确认进程退出。模型使用固定离线输出，没有真实模型调用。浏览器 Runtime 仍来自先前固定的 Harness 源码，未包含后续共享读取修改；不能据此关闭 Issue #14 或宣称最新整个 workspace 通过。最新浏览器入口是下文 `browser-replay-review/acceptance.json`，旧记录只证明各自当时的输入。

## 源码与构建身份

证据根目录：`/Volumes/ORICO/cayw-webui-regression-20261019.FSpmEp`。

| 输入或产物 | SHA-256 |
| --- | --- |
| 28 个冻结输入的聚合值 | `5cdecd05927e9589f350448235b9188aedffe174c757b6d532d00da680852f6e` |
| `source-manifest.json` 文件 | `ef07fbaa5bcd8f5bf675af52c44b6f31cb2ad660c6d77094177320c8be739540` |
| `assets/app.js` | `197434206ae138b0c6c45517740a4d21c4c409c7eafb160bea2ab1e38ef81241` |
| `assets/index.html` | `223d6134e5de216a868f30618e17cf621e3939a1b157d5cfcdcf2464edec8a88` |
| `assets/styles.css` | `66fe396d017606bcd4546026b492b5ac379bb66c84aeccbc27ee1710098a97b0` |
| 旧自动检查冻结编译、供新复验保留的 `cayw-webui` | `9fd85ede8013ddab64f8364204bada4779cbdbbefb523806ab7497f6e5990d2a` |
| 旧浏览器使用的 `cayw` Runtime（构建来源记录不完整） | `147852e47691a16c4312ddaf96cfecc43585db5923c00e86de6d478ad7f5815d` |
| Core 历史记录 fixture 可执行文件 | `7825a91f089ef8257a613185f5561104348d6c90f23893e315ac23f9739efba2` |

`frozen/` 只包含 WebUI、IPC 和构建所需的根清单与锁文件。副本 workspace 收窄为这两个本地包，第三方包从根锁的 394 项裁剪至 183 项；`lock-pruning.json` 核对保留项的版本、来源和校验值均未改变。没有复制用户配置，生产根清单和锁文件未修改。

根清单 SHA-256 为 `76baed65b2c40b24ff5d92f2830ab88daceb2a95de1a4ffc86addcf9080d8874`，根锁为 `46e0440091aafa15a621795ccfd2560d2a2ab53baa3fb0c1044bad79be1365b3`，实际冻结锁为 `ab5339b4182afceffcbadef7623a95d9929403f7d9bcaf3bc1cc00266b86e8ea`。聚合算法是按相对路径排序，每行写文件 SHA-256、两个空格、相对路径、LF，再对拼接字节计算 SHA-256。

旧 Runtime 来源记录指向 `/Volumes/ORICO/cayw-harness-evidence-8abef5923844408b93c92d3d579c4b93/final-lint-corrected/snapshot`；其源码清单文件 SHA-256 为 `a74d9ead7ec61b08ebbc5009bc31c9fd204966029701ee97fbf2389ec069f31b`，既有范围见 [SDK / IPC 验证](sdk-ipc-harness.md)。独立复核确认：旧 `147852…` 可执行文件缺少把实际编译命令、feature、环境和产物摘要串联起来的完整记录。旧 `core-fixture-compile.json` 只证明其 fixture 编译，不能代替 Runtime 二进制的构建证据。该历史缺口继续保留，没有补写推测的旧构建事实。

早先手机网关 `f27d7f58e54a363dd83969614ab3cf7db7d1403296df0f25a3d0715704a3f9ca` 的原可执行文件也未保留。启动记录中的 `app.js` 摘要相同，不能证明整个网关相同。旧手机结果因此只作为历史行为记录；下面的新复验统一使用已保留、有独立构建证据的 `9fd85e…` 网关。

## 保留构建产物后的重新验证

新证据根目录为 `/Volumes/ORICO/cayw-webui-build-repair-20261019-kshr58xx`。`acceptance.json` 是构建准备入口；`browser-replay-review/acceptance.json` 是随后实际运行浏览器的入口，两者范围分开。构建入口自身摘要为 `b8b1e0180a3ff287ddf407a7e11c75044fb21cb9aa3c62b6c5aeac09a778f668`。

| 新复验输入或产物 | SHA-256 |
| --- | --- |
| 新复制源码清单 `source-manifest.json` 文件 | `5d9debd3d91c78a16ecfa5a0fae7f48daafb697a05fe9fc52dff8ad2e1eed797` |
| 保留的 Runtime `binaries/cayw` | `fb14df766fc309da14fd84ccc810c811c95592b54c259f5411d3d13c330de7d9` |
| 保留的网关 `binaries/cayw-webui` | `9fd85ede8013ddab64f8364204bada4779cbdbbefb523806ab7497f6e5990d2a` |
| 新 Core fixture `binaries/core-history-fixture` | `57d2aac64e24486ca6cfea46569df703ed64961c098c5f2583c5e244471cfba6` |
| 构建证据清单 `evidence-seal.json` 文件 | `d487931332544e4e59a563945c18e5447fe3d643758002fa82a8d91b6de00a52` |
| 浏览器入口 `browser-replay-review/acceptance.json` 文件 | `169b4100999f3efc29cde2b64ec20fdadab15bec18fc1e354e23b9dc07ffec64` |
| 浏览器证据清单 `browser-replay-review/evidence-seal.json` 文件 | `885d656125d15c47efb7f8b8a0922397698a83c59d34ccd7c30f339fc5ba1dbc` |

旧 Harness 清单的 561 个输入逐项核对两次后，复制其中 335 个代码、包及文档输入；226 个旧数据库、日志、二进制和辅助产物不复制，每项排除及摘要保存在新清单。保留 20 个 workspace 成员和根锁全部 394 项，没有裁剪或升级。新 Runtime 的实际命令为 `cargo +1.95.0 build --manifest-path <新 source>/Cargo.toml -p cayw-runtime --bin cayw --offline --locked --message-format=json --verbose`，于 `2026-10-18T23:59:07.460123Z` 到 `23:59:40.037159Z` 退出 0。`records/runtime-build.json`、完整 stdout/stderr、`commands.jsonl`、`records/rustc-invocations.jsonl` 和实际产物摘要共同绑定该构建。fixture 链接实际 Cargo compiler-artifact 指定的库，其编译命令、169 项传递库及 2 个 native archive 的摘要另行记录。

构建使用独立 Cargo home，只有已有 registry 缓存链接；未查看或复制用户 Cargo 配置、凭据。完整替换的构建环境及工具链、SDK 探测已记录。已有第三方缓存、工具链及 macOS SDK 在原位置使用，未单独冻结；正常工具链隐式配置读取没有审计。因此不声称换路径或换机器后得到逐字相同的二进制。网关沿用已完整记录的冻结构建，不追补其未记录的历史环境。

新浏览器复验使用 Playwright MCP、Chromium `155.0.8059.84`。所有组均为全新的 Runtime 数据库与工作区：

| 目录（相对于新证据根） | 实际结果 |
| --- | --- |
| `replay-02/mobile-replay` | 手机审批与文件结果、取消后键盘重发并拒绝、会话切换、断线恢复、桌面布局，5 组通过 |
| `faults-replay` | 未知 prompt 不借其他客户端解锁、原终态先于回执且取消为 false、核对事实不被旧快照撤销，3 组通过 |
| `early-replay` | 未看见 attempt 就取消；只在原回执和精确原终态均到达后解锁，1 组通过 |
| `cache-replay` | 后续 5 次离线调用产生 140 条事件，缓存 51–170；精确读回原游标 30，前向游标保持 170，1 组通过 |

新手机两会话为 `fe0ca6b1-8d9f-4e19-82c5-71c33e45ba16` 和 `d0dff5ba-2724-48f2-861b-a666764c2d4b`，分别保留连续 40 和 58 条 Core v3 事件。实际文件仍为上面所述 21 字节、无末尾换行，摘要与读取内容一致。断线前后 prompt 请求都是 3 次；320、390、1280 CSS 像素均无横向溢出。各组 `supervisor-launch.json` 将实际三个二进制、测试代理和 supervisor 摘要绑定到该次启动；结果、完整会话事件、截图及清理记录再由浏览器证据清单绑定。Main 逐项重算了 428 份构建证据与 99 份新浏览器证据，均相符。

新复验还保留两项脚本失败：首次 `mobile-replay` 对收起会话区域使用 `innerText`，读不到已更新的计数；改用 `textContent` 后在全新 `replay-02` 运行，未改生产页面。一次磁盘核对辅助程序错误地预期末尾换行，实际批准内容没有换行；失败假设和原始字节另存，文件没有为迎合辅助程序而修改。主动中断事件读取和丢失回执分别留下 `net::ERR_FAILED`、`net::ERR_CONTENT_LENGTH_MISMATCH`，控制台记录保留，不写成零错误。

完成后浏览器离开测试页面，5 个 supervisor（包括首次失败）及所有记录的子进程均已退出；没有强杀，独立 PID 核对也确认不再存在。最新复验没有运行 121 次吞吐测试，不能改写其旧失败，也不包含当前共享读取、Device 或 Modules 的新组合。

## 自动检查

Rust 1.95.0，`aarch64-apple-darwin`，debug 构建；Cargo 使用 `--offline --locked`，独立构建与临时目录。`commands.jsonl` 保存六个命令的实际参数、工作目录、环境覆盖、UTC 起始时间、退出码和全部 12 份输出日志的摘要。

| 检查 | 实际结果 |
| --- | --- |
| `state.test.mjs`、`ui.test.mjs`、`dom.test.mjs`、`core-protocol.mjs` | 86 通过、0 失败 |
| WebUI 全 target、全 feature 测试 | 19 通过、0 失败，包含 2 项配置、14 项网关、3 项 HTTP 测试 |
| WebUI / IPC 格式检查 | 退出码 0 |
| WebUI / IPC 全 target、全 feature 编译检查 | 退出码 0 |
| 冻结 WebUI 构建 | 退出码 0 |
| WebUI / IPC 全 target、全 feature 严格 Clippy | 退出码 0，警告视为错误 |

IPC 测试在本次只参与编译检查，没有执行；上表不包含根 workspace 的其余 18 个包。测试开始时，旧 JavaScript fixture 仍返回 Core v1，且部分断言依赖旧的运行提示，得到 44 通过、42 失败。已把这些 Core 消费数据迁到 v3，补齐小型 DOM fixture 的 `matchMedia` 和 `classList.add`，保留原调用身份、未知结果和快照覆盖检查。修后在生产路径和冻结副本各执行一次，均为 86 通过；`existing-js-before.log` 与 `existing-js-current.log` 均保留。

Core 协议为 v3，IPC 帧协议仍为 v1。页面只接受 Core v3 的 health 和原结束记录；旧协议、不同 session/attempt/worker/fence、事件与外层身份不一致都不能解除取消锁定。未知取消回执不能借原 prompt 的成功记录清除。工具展示只解释已知的 `execute_code` 结构；未知副作用优先于成功状态，不显示成功输出；普通助手 JSON 仍作为普通文字展示。

## 旧浏览器行为记录

以下表格保留新复验之前的行为观察；它们仍受上文旧 Runtime 构建记录与手机网关产物缺失的限制，不替代新复验。手机与桌面记录位于 `/Volumes/ORICO/CAYW/.local/webui-browser-runtime3-bjukll83`，故障场景位于本证据根目录各个 `browser-*-frozen/` 下。测试代理只改变 HTTP 交付顺序、丢失响应或暂扣记录；实际授权、取消、执行、终态和事件来自本地 Runtime。Core fixture 通过公开 SDK 创建和核对一条没有实际调用 executor 的未知记录，未直接伪造网页事件。

| 场景 | 结果与核对内容 |
| --- | --- |
| 允许写入，再独立允许读取 | 两项实际授权各自匹配原参数、task、target、attempt；页面显示返回的文件内容，原记录可展开，下一轮轮询保留展开状态 |
| 审批前停止，再次发送并拒绝 | 停止和拒绝各留下 `not_executed`，没有借其他调用解锁；键盘提交下一任务可用 |
| 切换会话 | 两个会话的历史互相独立 |
| 中断事件读取再恢复 | 同一会话从 `after:40, limit:100` 续读，prompt 请求数量保持 3，没有自动重发 |
| 手机与桌面布局 | 320、390、1280 CSS 像素宽度下，页面滚动宽度等于视口宽度，无横向溢出 |
| prompt 在到达 Runtime 前丢失响应 | 别的客户端完成任务仍不能解除原未知请求的锁定，提供宿主核对及新建独立会话的说明 |
| 原结束记录先于 HTTP 回执，随后取消返回 `false` | 无操作的取消只结束自身；原 prompt 仍等自己的回执，收到后才恢复发送 |
| 核对事实先于旧未知快照 | 已消费的真实 Core 核对记录不被延迟的旧快照撤销 |
| 看见 attempt 之前就停止 | 实际取消返回 `true`；原回执和原结束记录分别延迟，只有证据完整后才恢复发送 |
| 原结束记录被挤出 120 条事件显示缓存 | 5 次后续实际离线调用产生 140 条新事件；页面准确读回原游标 30 的记录，再恢复发送 |

手机两会话的 98 条事件分别连续覆盖 1–40 和 1–58。实际磁盘文件为 21 字节的 `headless runtime demo`，没有末尾换行，SHA-256 为 `40c60e79df2178318ac0f3da1c52349f9ae4774a01c1dfa35cef51eccc32d7c1`；读取结果中的内容、字节数和摘要均与磁盘相同。两会话最终为空闲，无待审批或未核对调用。`mobile-durable-audit.json` 保存核对结果。

缓存场景的可复跑脚本是 `crates/cayw-webui/tests/browser-early-cancel-eviction.mjs`。它每次只推进一次真实调用，等待页面消费后检查实际缓存边界，最多推进 16 次；未触发边界就明确失败，不静默缩小覆盖。本次在第 5 次触发：缓存只剩游标 51–170，原结束游标为 30。原回执释放前仍禁止发送；释放后页面发出 `{session_id: 原会话, after:29, limit:1}`，返回的完整记录与原事件完全一致。当前事件游标仍为 170，发送恢复，停止按钮禁用，只有新草稿而无新提交。

该脚本在冻结自动检查之后修改，修改前脚本仍保留在 `frozen/`。新脚本及本轮补充验证脚本记录于 `final-review/inputs/`、`supplemental-inputs.json`，两份浏览器脚本均完成 Node 语法检查；`live-input-comparison.json` 确认其余 25 个包内输入与冻结副本相同，全部生产字节未变。

## 原始失败与限制

以下记录没有删除或计作通过：

- 最初页面要求 Core v1，真实 Runtime health 为 v3，无法登录；原结束记录的协议检查也存在相同错误。生产消费者已统一到 Core v3，没有恢复旧应用协议兼容路径。
- 早期一次人工分步审批超过默认 CodeMode 时限，留下真实 Timeout 和 `not_executed`。后来以连续浏览器步骤及时完成审批，没有放宽生产时限或自动批准。
- 一次断线脚本错误地比较了不同会话的事件游标；修正为相同 session、after、limit 及 prompt 数量核对后，重新执行五组场景。
- 第一次缓存压力脚本复用了已有会话的服务，自动选中旧会话，代理却注入新会话快照。页面正确拒绝身份不一致，保存 `session did not load`；重跑使用全新服务。
- 原固定 121 次调用压力场景在第 4 批控制请求超过 120 秒。失败报告返回前 30 次的结果，该批剩余调用随后完成，总计 40 次。旧固定 Runtime 中，同一会话的单次耗时随历史增长，约从前十次的 0.1–0.9 秒增至第 40 次约 14 秒；这是本机该场景的观察，尚未定位原因，不作一般性能结论。121 次吞吐验收未通过。

超时场景继续核对了已经完成的 40 次调用：产生 1120 条新事件，页面只保留 1031–1150；释放原回执后准确读回游标 30 的原记录，当前游标保持 1150，恢复发送。这是缓存恢复的额外通过证据，不能改写 121 次压力失败。`browser-eviction-frozen/eviction-result.json`、`timeout-trace-audit.json`、`history-continuation.json` 和新全流程 `browser-cache-frozen/eviction-result.json` 分开保存。

手机记录中 console 有 favicon 404 和一次主动中断事件读取的连接错误；没有观察到其他应用错误，不能写成 console 零错误。后续测试代理为 favicon 返回 204。Windows、Linux、真实手机软键盘、移动 Safari、当前网页真实 Provider 流式输出、最新共享读取 Runtime 组合均未验证。离线 Provider 只给出固定完成输出，不能用此证明真实增量流显示或模型质量。

## 清理与证据索引

本轮四个故障/缓存服务都由独立 supervisor 启动。完成后先让浏览器离开页面，再关闭服务；每组 `cleanup.json` 均记录 Node 正常退出、无需强杀、进程组与两个直接子进程已退出。收口脚本再次查询对应 PID，确认没有遗留进程。早先手机服务也有独立清理记录。

旧记录入口为 `final-review/acceptance.json`；新复验以新证据根的 `browser-replay-review/acceptance.json` 为准。`final-review/evidence-seal.json` 对 231 个明确选定的源码、构建记录、原始输出、截图、浏览器记录和清理文件记录字节数与 SHA-256，文件自身 SHA-256 为 `c267eef5fff8f1e0eae15c04a8ed61746d2a56a654cd3c48fe99f6ddf65d3f38`。数据库、构建缓存和临时目录不纳入该清单。证据保持本地，没有发布测试访问令牌或页面。

## 复跑

先核对 `source-manifest.json` 和补充输入摘要。以下命令使用当前机器的冻结证据路径；换机器时应复制完整输入并使用自己的独立输出目录，不覆盖现有记录。

```sh
export CARGO_TARGET_DIR=/Volumes/ORICO/cayw-webui-regression-20261019.FSpmEp/target-replay
export TMPDIR=/Volumes/ORICO/cayw-webui-regression-20261019.FSpmEp/tmp
export CARGO_NET_OFFLINE=true

node --test /Volumes/ORICO/cayw-webui-regression-20261019.FSpmEp/frozen/crates/cayw-webui/tests/state.test.mjs /Volumes/ORICO/cayw-webui-regression-20261019.FSpmEp/frozen/crates/cayw-webui/tests/ui.test.mjs /Volumes/ORICO/cayw-webui-regression-20261019.FSpmEp/frozen/crates/cayw-webui/tests/dom.test.mjs /Volumes/ORICO/cayw-webui-regression-20261019.FSpmEp/frozen/crates/cayw-webui/tests/core-protocol.mjs
cargo +1.95.0 fmt --manifest-path /Volumes/ORICO/cayw-webui-regression-20261019.FSpmEp/frozen/Cargo.toml --all -- --check
cargo +1.95.0 check --manifest-path /Volumes/ORICO/cayw-webui-regression-20261019.FSpmEp/frozen/Cargo.toml --workspace --all-targets --all-features --offline --locked
cargo +1.95.0 test --manifest-path /Volumes/ORICO/cayw-webui-regression-20261019.FSpmEp/frozen/Cargo.toml -p cayw-webui --all-targets --all-features --offline --locked
cargo +1.95.0 clippy --manifest-path /Volumes/ORICO/cayw-webui-regression-20261019.FSpmEp/frozen/Cargo.toml --workspace --all-targets --all-features --offline --locked -- -D warnings
```

浏览器使用公开接口的实际服务，不以 DOM fixture 替代。新复验的脚本已保留在新证据根 `browser-replay-review/`：`mobile-runner.mjs`、`fault-runner.mjs`、`early-runner.mjs`、`cache-runner.mjs`、`postflight-runner.mjs` 及 `runtime-browser.mjs`。将 `runtime-browser.mjs` 的第三个参数设为全新且不存在的证据目录；故障脚本使用普通离线模式，提前取消与缓存脚本加 `--approval-gated`。测试代理地址从启动时输出的 manifest 读取；在 Playwright MCP 中打开地址，再运行同一新证据根 `browser-replay-review/` 保存的相应 `.mjs`，缓存场景使用其中的 `cache-runner.mjs`。`final-review/inputs/` 只保留旧轮次脚本，不作为此次复跑入口。测试结束必须等待并核对服务实际清理，不以关闭网页或发出信号替代。

```sh
node /Volumes/ORICO/cayw-webui-build-repair-20261019-kshr58xx/browser-replay-review/runtime-browser.mjs /Volumes/ORICO/cayw-webui-build-repair-20261019-kshr58xx/binaries/cayw /Volumes/ORICO/cayw-webui-build-repair-20261019-kshr58xx/binaries/cayw-webui /Volumes/ORICO/cayw-webui-cache-replay-NEW /Volumes/ORICO/cayw-webui-build-repair-20261019-kshr58xx/binaries/core-history-fixture --approval-gated
```
