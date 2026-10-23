export const meta = {
  name: 'complete-macos-mainline',
  description: '依次实现、验收和独立复核 macOS 本机无界面主线的 11 个 Issue；保留全部阻塞项，不自动提交或关闭任务。',
  phases: [
    { title: '启动核对', detail: '冻结 11 项任务条件，核对源码、检索、工具链和真实调用授权' },
    { title: 'Issue #2', detail: 'VerifyTool、离线证据与比较规则' },
    { title: 'Issue #3', detail: 'Luau 运行、签名和同机测量' },
    { title: 'Issue #4', detail: 'macOS 进程、PTY、取消与清理确认' },
    { title: 'Issue #5', detail: '完整 CodeMode 能力集与实际分派' },
    { title: 'Issue #6', detail: '持久化 Runtime 的授权真实模型多轮工具任务' },
    { title: 'Issue #7', detail: '崩溃恢复、未知副作用核对与旧执行者隔离' },
    { title: 'Issue #8', detail: '可信能力申请、授予和撤销' },
    { title: 'Issue #9', detail: '宿主 MCP 与 Skills 的真实接入和生命周期' },
    { title: 'Issue #10', detail: 'OpenAI、Anthropic、Gemini、Magpie 四项协议' },
    { title: 'Issue #11', detail: '多 Session 队列、容量、隔离与公平性' },
    { title: 'Issue #18', detail: '本机资源策略与权威内容存放、恢复' },
    { title: '最终组合验收', detail: '最终同一份源码的真实模型任务覆盖全部 11 项，并重跑故障和边界验证' },
    { title: '交付记录', detail: '保存逐项结果、未完成项和可复跑入口' },
  ],
}

// This file defines the implementation workflow. Creating or reviewing it does
// not execute the workflow. Run by its saved name after execution is requested.
// Start a fresh invocation after an interruption or an external source change:
// do not reuse cached acceptance calls with resumeFromRunId.
const PROJECT = '/Volumes/ORICO/CAYW'
const REPOSITORY = 'changw98ic/CAYW'
const ORDER = [2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 18]
const DEPENDENCIES = {
  2: [1], 3: [1, 2], 4: [1, 2], 5: [1, 3, 4],
  6: [1, 2, 4, 5], 7: [6], 8: [5, 6], 9: [5, 6],
  10: [6], 11: [6, 7], 18: [4, 7, 11],
}
// Frozen from the published Issue bodies. A changed body is a contract change,
// not permission to silently regenerate a smaller checklist during execution.
const CONTRACTS = {
  2: { body_sha256: '3515a6b57b964de224045c5ff9e634ec17e65b2d8225544dffa5aff5bfe990c8', checkbox_count: 12 },
  3: { body_sha256: 'dd388ef1f8268d0860c5f129498f90a3e3b3e0109b136bf35a418989bd6d23d0', checkbox_count: 9 },
  4: { body_sha256: 'bd42f2b614eb954312bfb0899c46c8fb78fc3658982008b5c3033e92c607c9da', checkbox_count: 9 },
  5: { body_sha256: 'fa4633e514b6918863fe97c0c18866741c33c3794e4d61da4b3566d6a92e21a8', checkbox_count: 11 },
  6: { body_sha256: '203c6f0000fa4f937fc184f262f7643924d4e11f9a7838b61bced62465e730b2', checkbox_count: 11 },
  7: { body_sha256: 'cc5c7866b21c13b343a7e92122fcb528d4c4b414176ee2fdb31c4d1914f9a305', checkbox_count: 9 },
  8: { body_sha256: 'ae4cab1ccb102be1d1d25c57ddb214cce05a36bae3feea221671d564b6759ee5', checkbox_count: 10 },
  9: { body_sha256: '2d8717ad535b68d697745ce8e4940f7eec3f951ec5d6e9bd3c0c7c0f4220ad15', checkbox_count: 10 },
  10: { body_sha256: '94cb49ca8b3aa3abf8aada3e230053ce9bd17a439b733e445fbf618fca5d1208', checkbox_count: 9 },
  11: { body_sha256: '21a9600410d92045260bccf519e9407fb79ef51a36c73168f4c93b40f2e0712e', checkbox_count: 10 },
  18: { body_sha256: '8ebe03b17fed218e31c1c714e498577c9aa043fbb625b9fbb2f2cd8f010f1833', checkbox_count: 11 },
}
const SCOPE_CRITERIA = {
  5: ['luau', 'shared-read', 'files', 'search', 'data', 'git', 'process', 'host-cooperation'],
  10: ['openai', 'anthropic', 'gemini', 'magpie'],
}
function criteriaFor(number) {
  const checkboxes = Array.from({ length: CONTRACTS[number].checkbox_count }, (_, index) => `${number}:checkbox:${index + 1}`)
  return checkboxes.concat((SCOPE_CRITERIA[number] || []).map(scope => `${number}:scope:${scope}`))
}
const TASKS = {
  2: {
    title: 'VerifyTool 与离线验收基线',
    packages: ['cayw-verify-tool', 'cayw-runtime', 'cayw-ipc'],
    requirements: [
      '交付可复用 SDK/认证 IPC 场景、故障注入、v2 证据格式、读取/比较和独立消费示例；只维护一个新规范路径，不恢复旧 harness 包或旧报告 reader。',
      '真实 Runtime/SQLite/Host 路径验证请求、审批、取消、查询、断连续读和最小恢复；区分事件连接与活动执行连接，不重发 prompt。',
      '离线样本至少三次，准确标冷暖/首执行/编排/建连关闭边界；保存全部失败和修复成本，未知用量不填零。',
      '验证器必须拒绝只有最终回答、缺实际效果、未读完精确 cell/cursor 输出、VM 仍活/让出、非正常退出、调用错配、事件内容缺失/篡改和只有清理成功的假通过。自定义场景缺任务语义检查保持未验证。',
      '条件已知不同拒绝比较；关键数据缺失待测；来源矛盾/附件损坏判无效；真实构建绑定不能被调用方声明的摘要代替。两侧实现允许有不同构建身份。',
      '原始 Child 在正常/失败/超时/Future 丢弃后停止并确认退出，输出有界而持续排空；清理不明停止采样。不等待 #6/#7 的完整真实或崩溃验收。',
      '历史 Issue #2 候选副本仅作为待核对输入；按当前 cayw-verify-tool、公共契约与最终源码迁移，逐缺陷先复现再修，不能直接复制旧副本覆盖主树。',
    ],
  },
  3: {
    title: 'Luau 原型、工具签名与测量',
    packages: ['cayw-codemode', 'cayw-cells', 'cayw-verify-tool'],
    requirements: [
      '有限冻结的离线脚本/签名清单和实际 Rust 能力；正确计算、多工具、错误参数/拒绝、语法错、长循环、输出/内存/调用预算和取消全部覆盖。',
      '同机同构建同权限至少三次，冷暖与首执行边界清楚；不丢失败/超时/取消样本，内存观测范围准确，不将统计值称硬限制。',
      '检查实际输出/文件效果；wait 不重置总预算，无法确认清理/副作用保持未知；原失败和修复逐条保留。',
      '交付由原始结果支持的技术选择说明；Monty、真实模型生成脚本和 #6 不作前置，不设固定毫秒门槛。',
    ],
  },
  4: {
    title: '本机进程、PTY、取消和预算',
    packages: ['cayw-device', 'cayw-host', 'cayw-runtime'],
    requirements: [
      '真实 pipe 与 PTY 分测父/子/孙拓扑，记录 Task/attempt/Worker/启动身份；正常结束、启动中/执行中取消、超时、预算、Worker 崩溃及宿主退出分别验收。',
      '确认原进程和声明受管后代退出、两路输出排空与句柄释放；仅退出码、Future Drop 或 PID 消失不算回收。',
      '可控身份替换覆盖 PID 复用，不误杀无关进程；自然退出/取消竞态、启动后登记失败、清理/持久化失败保持原归属和容量。',
      '公开取消与统一预算，不建杀进程旁路；迟到成功/旧 attempt 不反转终态。区分资源清理不明和业务副作用未知，不将进程组称安全沙箱。',
    ],
  },
  5: {
    title: '完整 CodeMode 与受控 Rust 能力集',
    packages: ['cayw-codemode', 'cayw-cells', 'cayw-host', 'cayw-workspace', 'cayw-data', 'cayw-device', 'cayw-bridge'],
    requirements: [
      '逐项契约矩阵覆盖八组全部能力，记录参数/返回/权限/资源/预算/取消/错误/公开入口/证据；不能只验文件读写或把未实现项标不适用。',
      'Luau exec/wait、cell/cursor、yield、max_output_tokens、store/load、ALL_TOOLS/toolDefinition/tool_search、text/image/audio/notify/exit；读完输出、正常结束与原调用配对。',
      '依赖快照、共享读合并/复用、写入/撤销/来源失效和中断对账；各原调用独立授权、预算、身份与结算，不合并写操作。',
      '文件/目录/树、语法校验补丁、单文件原子写与多文件候选批次/失败恢复；有界正则/多行、忽略规则 glob、单文件语法大纲和 diff 块。',
      'JSON/TOML/YAML/XML/CSV、diff、SHA256/BLAKE3、Base64/Hex 与工作区归档读取；Git status/diff/log/show/blame 和策略受控暂存/提交/分支/工作树。',
      '受控 test/check/build/lint/format、Device exec_command/write_stdin 和 terminal_handoffs；澄清输入、计划、图像、本机子代理、宿主 MCP/资源桥全部真实可运行。',
      '能力在同一 Host 校验/授权/开始/预算/结算路径执行；拒绝越界/链接替换/任意 Shell 旁路；媒体来源、父子 Task、取消和可信输入核对。',
      '跨单元值不授予新权限；精确输出分页、组合效果和持久记录独立核对。丢失 VM 不自动重跑，未知不重放，清理不明不释放名额。#7/#8/#9 深化但不反向阻塞本项。',
    ],
  },
  6: {
    title: '授权真实模型的无界面完整任务',
    packages: ['cayw-ai', 'cayw-agent', 'cayw-runtime', 'cayw-ipc', 'cayw-host'],
    requirements: [
      '首协议为 Magpie OpenAI Chat Completions 兼容 API，路由 group/auto-deepseek-v4-1-flash，绑定实际端点/响应模型/配置；本项真实调用须有覆盖本项的授权。',
      '独立客户端分别通过公开 SDK 和认证本机 IPC，至少两个用户回合，包含模型→Luau→真实 Host 工具→结果→模型，再使用上回合事实；核对实际效果、流、持久消息和调用配对。',
      '运行无 UI/plugin/remote/VerifyTool 依赖，最终产品依赖无 Codex；审批等待/可信允许拒绝、模型流中取消/工具中取消、连续游标续读、同一 Session 重开全部覆盖。',
      '错误认证/版本/参数拒绝且无受限读取或执行，迟到事件不反转终态；最小恢复/未知处理在本项交付，不推给 #7。',
      '真实证据与离线数据分开；无授权/费用或缺真实证据时保留未验收，不能用固定响应或早期内存 Agent 记录替代。',
    ],
  },
  7: {
    title: '崩溃恢复与未知副作用核对',
    packages: ['cayw-core', 'cayw-runtime', 'cayw-cells', 'cayw-requests', 'cayw-device'],
    requirements: [
      '真实进程故障覆盖意图落盘前后、效果前后、结果落盘前后、提交未通知；用带原调用 ID 的真实本机效果独立计数/核对。',
      '可信成功/失败/未执行/不足/矛盾各分支；已完成不重做，只有可信证明未执行才能显式新执行，未知不盲重放；核对追加记录不改历史。',
      '原 Worker 活着/独占未知拒绝接管，旧 lease/fence/attempt 的结果/审批/取消和重复投递不能改新任务。',
      '控制完成/取消提交顺序；Core/Cells/Requests 部分提交、存储/清理失败保持待核对和容量，不声称跨数据库天然原子。SDK/认证 IPC 实测查询/可信核对/拒绝/重开。',
      '可用固定 Provider 和真实本机副作用做故障测试，不要求每窗口都调用真实模型。',
    ],
  },
  8: {
    title: '任务能力申请、可信授权与撤销',
    packages: ['cayw-requests', 'cayw-host', 'cayw-runtime', 'cayw-device'],
    requirements: [
      '模型只提申请/建议，grant 来自用户受信入口或可信宿主策略；审批接口不得注册模型工具，伪造 actor/角色/批准、Skill 文本不授予权限。',
      '绑定 Session/Task/attempt、能力、参数/路径/profile、期限/预算；改参数/重放/跨任务拒绝。首轮明确需求可评估，次数阈值只影响评估不自动放行。',
      '允许/拒绝/超时/评估不可用/重复裁决、执行前撤销/运行中撤销、迟到许可/任务结束/到期/重开与跨会话隔离全部实测。',
      '真实受控项目测试/脚本经统一 Host/Device；拒绝通用执行旁路，拒绝/等待不阻塞无关已授权工具，取消和预算不绕过。',
    ],
  },
  9: {
    title: '宿主 MCP 与 Skills 接入',
    packages: ['cayw-bridge', 'cayw-runtime', 'cayw-host'],
    requirements: [
      '明确有限 MCP 传输与 Skill 格式，真实本地 MCP 服务和真实本地 Skill 各一个，按需发现/读取/执行，不任选一边代替全部。',
      '可观测连接/加载入口计数，宿主单一持有；Core 不新建客户端/loader，发现/读取/执行权限分测，Skill 不授权。',
      '记录来源/版本或摘要/origin/实例/连接代次及原调用；断连、重连、取消、撤销、替换/移除来源和迟到旧结果，不误用缓存、不盲重放未知。',
      '统一预算/取消/有界输出；宿主代码检索能力移除后普通 Chat 可用，显式缺失能力有诊断，不在 Core 新建重型索引。',
    ],
  },
  10: {
    title: '四项有限模型协议清单',
    packages: ['cayw-ai', 'cayw-providers', 'cayw-agent', 'cayw-runtime'],
    requirements: [
      '四行全部是关闭门槛：OpenAI 官方 Chat Completions、Anthropic 官方 Messages、Gemini 官方 generateContent/streamGenerateContent、Magpie 兼容 Chat Completions。不得删除官方行，也不得扩大为所有兼容服务。',
      '各行真实运行前固定 model ID/端点/API 版本/配置与授权；Magpie 固定 group/auto-deepseek-v4-1-flash 并保存响应模型。兼容网关不能代替三家官方服务证据。',
      '各行独立固定数据、本地 HTTP 与授权真实多用户回合工具往返；流式分片工具参数/多调用/结束原因/用量和原始协议映射核对。',
      '错误类型/非法消息/缺分片/截断/缺终止、请求/流中/工具等待取消、断连/错误/限流/超时；不默认重试/跳转/换模型。',
      '不支持明确拒绝，受控降级可见；用量缺失/矛盾不伪造零，失败/修复/未知保留预算。缺服务授权可完成离线代码，但本项未验收，不能阻塞无依赖的 #11/#18。',
    ],
  },
  11: {
    title: '多 Session 调度、隔离与公平性',
    packages: ['cayw-runtime', 'cayw-core', 'cayw-device'],
    requirements: [
      '明确 K、队列上限、准入/背压、预算、优先级及防长期等待规则，状态区分排队/审批/执行/清理/待核对。',
      '验收前固定有限派发机会和被越过次数上限；至少三个 Session，含持续补任务者、普通等待者和审批等待者，受控屏障/事件顺序验证低优先级也不会永久等待。',
      '超限拒绝/背压可见不丢已受理任务；停输入后有限可执行队列排空，不承诺无限过载队列收敛，不写固定毫秒公平断言。',
      '审批等待不占尽全部名额，结果/权限/预算/审批/游标隔离，取消一个不影响其他；清理不明不提前发槽。',
      '排队/执行/清理期间重启恢复顺序/公平/租约；旧 Worker/重复投递/部分存储失败不重复效果。未知保持阻塞，无关安全任务按容量继续。',
    ],
  },
  18: {
    title: '本机资源策略与权威内容',
    packages: ['cayw-device', 'cayw-content', 'cayw-runtime'],
    requirements: [
      'CPU/内存/GPU/I/O 矩阵逐行区分硬限制、软准入/预算、观察、不支持、未验证与错误；至少一个真实可执行的资源策略，不能全部填不支持或只有统计。',
      '正常准入/超限拒绝等待/执行预算耗尽取消，核对多 Session 总额度；硬限制须真实越限证据，不支持/未验证的保证在执行前拒绝，不能静默降成观察。',
      '原执行身份、退出/清理/持久化失败保留占用和容量；测量范围准确，不可测不填零。',
      '至少一种真实本机权威内容路径，在无 Server/同步进程时创建/读取/修改/同 Session 重开；权限/跨会话/路径/链接、缺失/损坏/版本冲突/写入失败/中断恢复均实测。',
      'Device 保持权威来源，缓存不代替；未提交/已提交/未知区分，不重放或宣称多文件原子/未经验证断电安全。无 #15 条件依赖或同步/密钥/远程范围。',
    ],
  },
}

const COMMON = `
项目：${PROJECT}，GitHub：${REPOSITORY}。
当前用户范围：macOS 本机无界面运行时；主线恰好 #2–#11 和 #18，#1 已完成。
不开展 #12–#17 的 plugin/UI/远程/同步或 #19 的 Linux/Windows，也不因现有 workspace 包存在而删除它们。
README、AGENTS.md、docs/architecture.md 不在这次修改范围；可以读取既有架构约束，旧范围措辞不恢复取消功能。
每次源码/文件/符号/引用/调用/依赖发现都必须 CodeGraph；禁止 rg/grep/find/Glob/git grep/脚本遍历替代。
已知准确路径可 Read；CodeGraph 不可用或结果对当前源码失效就记录阻塞，不擅自 init/upgrade，不绕过。用户已授权并完成本仓库索引初始化，仍须实际查询当前工作树。
库/API/CLI 的具体语法、版本/配置问题优先 Context7。不要读取其他 Agent 配置、凭据文件或创建 Claude 配置。
全局说明引用的两份规则文件此前在准确路径均不存在；只记录读取结果，遵循当前已提供的规则。缺失引用本身不是产品实现前置，不猜测内容、不创建替代规则、不搜索其他配置。
只允许一个源码写者；不创建并行作者，不重置/清理/覆盖原有未跟踪文件，不使用 git worktree。
实现只维护一个当前规范接口，旧内部产品路径迁移后删除，不增加旧版本兼容 reader；不同系统边界除外。
工具均经 Host 的严格参数/权限/开始/预算/结算；未知副作用不重放，清理/持久化不明确不释放执行名额。
失败先复现真实行为，再最小修复再复跑；禁止只查源码字符串、改断言凑绿或用作者说明代替实际证据。
最终验收证据绑定完整产品源码及实际构建输入清单与内容摘要（含新增产品文件）、Cargo.lock/feature/构建、macOS 实际架构、配置/权限/输入/命令/退出码/原始日志。
启动只绑定 CodeGraph 返回的当前源码与已知输入清单，明确 source_id 的这一范围；尚未编译的构建身份和非源码输入覆盖记 observations，由作者构建和独立验收补齐，不能要求启动先证明一个未生成的最终二进制或任意磁盘文件全集。
完整指实际交付及所运行构建/场景的输入闭包，不是整个仓库或磁盘所有无关文件。源码发现只用 CodeGraph；明确路径可 Read，实际 Cargo 构建报告的输入、include 路径或 dep-info 可用于验证已经构建的输入，不作为通用源码搜索旁路。缺实际产品依赖/输入或源码变动无法绑定时，单项和最终验收仍必须拒绝。
不把 Git HEAD、调用方摘要、进程退出码、最终回答或测试数量单独当作交付证明。不写固定毫秒级 CI 断言。
真实模型只由单个串行 runner 通过一份持久授权预算账本发起；调用前保守预留，失败/取消/缺用量不归零、不重试或换模型。
已有 100M Magpie 授权原始用途是 Issue #2，不自动扩成主线最终真实验收、#6/#10 或 OpenAI/Anthropic/Gemini 官方服务授权；以用户原话和精确范围为准。
每项可先完成离线实现与精确故障验收，但整条主线的最终完成必须有真实 Provider 经公开 Runtime 实际执行的场景，逐一覆盖全部 11 项。共享场景可以覆盖多项，须逐项保存请求/工具/事件/外部效果映射；不能只给 #6/#10 实际调用，其他项用离线冒充真实覆盖。
本 workflow 不自动提交、推送、创建/合并 PR、改写/关闭 Issue、修改 CI/部署/系统/许可证/权限/外部自动化。
“实现完成”“本项验收通过”“已提交”“已合入”“Issue 已关闭”分别记录，不互相冒充。用户后续的明确授权按实际范围执行。
`

const string = { type: 'string', minLength: 1 }
const digest = { type: 'string', pattern: '^[a-f0-9]{64}$' }
const nonemptyStrings = { type: 'array', minItems: 1, uniqueItems: true, items: string }
const realCoverage = {
  type: 'object', additionalProperties: false,
  properties: {
    id: string, issue: { type: 'integer' }, scenario_id: string, source_id: digest,
    provider: { enum: ['openai', 'anthropic', 'gemini', 'magpie'] }, api_shape: { enum: ['chat_completions', 'messages', 'generate_content'] },
    requested_model: string, response_model: string, user_turns: { type: 'integer', minimum: 1 }, criteria_ids: nonemptyStrings,
    authorization_ref: string, ledger_ref: string, request_ids: nonemptyStrings, task_ids: nonemptyStrings, tool_call_ids: nonemptyStrings,
    runtime_entrypoints: { type: 'array', minItems: 1, uniqueItems: true, items: { enum: ['sdk', 'ipc'] } },
    evidence_paths: nonemptyStrings, actual_effects_verified: { type: 'boolean' }, usage_accounted: { type: 'boolean' },
  },
  required: ['id', 'issue', 'scenario_id', 'source_id', 'provider', 'api_shape', 'requested_model', 'response_model', 'user_turns', 'criteria_ids', 'authorization_ref', 'ledger_ref', 'request_ids', 'task_ids', 'tool_call_ids', 'runtime_entrypoints', 'evidence_paths', 'actual_effects_verified', 'usage_accounted'],
}
const blocker = {
  type: 'object', additionalProperties: false,
  properties: { kind: { enum: ['code', 'tests', 'evidence', 'authorization', 'codegraph', 'environment', 'contract', 'scope', 'tool_failure'] }, detail: string },
  required: ['kind', 'detail'],
}
const contract = {
  type: 'object', additionalProperties: false,
  properties: { number: { type: 'integer' }, body_sha256: digest, checkbox_count: { type: 'integer', minimum: 1 }, packet_path: string, criteria_ids: nonemptyStrings, dependencies: { type: 'array', items: { type: 'integer' } } },
  required: ['number', 'body_sha256', 'checkbox_count', 'packet_path', 'criteria_ids', 'dependencies'],
}
const startupChecks = {
  type: 'object', additionalProperties: false,
  properties: {
    codegraph_available: { type: 'boolean' }, baseline_verified: { type: 'boolean' },
    toolchain_verified: { type: 'boolean' }, source_manifest_verified: { type: 'boolean' },
  },
  required: ['codegraph_available', 'baseline_verified', 'toolchain_verified', 'source_manifest_verified'],
}
const SETUP = {
  type: 'object', additionalProperties: false,
  properties: {
    ready: { type: 'boolean' }, run_dir: string, working_branch: string,
    source_id: digest, source_manifest_path: string, source_manifest_scope: { enum: ['indexed_source_and_known_inputs'] },
    startup_checks: startupChecks, observations: { type: 'array', items: blocker },
    issue_1_completed: { type: 'boolean' }, contracts: { type: 'array', items: contract },
    authorization_path: string, blockers: { type: 'array', items: blocker },
  },
  required: ['ready', 'run_dir', 'working_branch', 'source_id', 'source_manifest_path', 'source_manifest_scope', 'startup_checks', 'observations', 'issue_1_completed', 'contracts', 'authorization_path', 'blockers'],
}
const AUTHOR = {
  type: 'object', additionalProperties: false,
  properties: {
    status: { enum: ['ready_for_review', 'blocked', 'partial'] }, source_id: digest,
    report_path: string, commands_manifest_path: string, blockers: { type: 'array', items: blocker },
  },
  required: ['status', 'source_id', 'report_path', 'commands_manifest_path', 'blockers'],
}
const VERDICT = {
  type: 'object', additionalProperties: false,
  properties: {
    accepted: { type: 'boolean' }, source_id: digest, after_source_id: digest,
    body_sha256_after: digest, checkbox_count: { type: 'integer', minimum: 1 }, observed_criteria_ids: nonemptyStrings,
    real_coverage: { type: 'array', items: realCoverage }, real_evidence_verified: { type: 'boolean' },
    contract_unchanged: { type: 'boolean' }, required_checks_passed: { type: 'boolean' },
    evidence_verified: { type: 'boolean' }, report_path: string,
    criteria: { type: 'array', items: { type: 'object', additionalProperties: false,
      properties: { id: string, status: { enum: ['pass', 'fail', 'missing'] }, evidence_paths: { type: 'array', minItems: 1, items: string } },
      required: ['id', 'status', 'evidence_paths'] } },
    blockers: { type: 'array', items: blocker },
  },
  required: ['accepted', 'source_id', 'after_source_id', 'body_sha256_after', 'checkbox_count', 'observed_criteria_ids', 'real_coverage', 'real_evidence_verified', 'contract_unchanged', 'required_checks_passed', 'evidence_verified', 'report_path', 'criteria', 'blockers'],
}
const FINAL = {
  type: 'object', additionalProperties: false,
  properties: {
    ready: { type: 'boolean' }, source_id: digest, report_path: string,
    full_checks_passed: { type: 'boolean' }, issues: { type: 'array', items: { type: 'integer' }, uniqueItems: true },
    real_provider_coverage: { type: 'array', items: realCoverage }, real_coverage_verified: { type: 'boolean' },
    blockers: { type: 'array', items: blocker },
  },
  required: ['ready', 'source_id', 'report_path', 'full_checks_passed', 'issues', 'real_provider_coverage', 'real_coverage_verified', 'blockers'],
}
const RECORD = {
  type: 'object', additionalProperties: false,
  properties: { report_path: string, state_path: string, recorded_issues: { type: 'array', uniqueItems: true, items: { type: 'integer' } }, verified: { type: 'boolean' } },
  required: ['report_path', 'state_path', 'recorded_issues', 'verified'],
}

function sameNumbers(a, b) {
  return Array.isArray(a) && a.length === b.length && new Set(a).size === a.length && b.every(n => a.includes(n))
}
function canonical(value) {
  if (Array.isArray(value)) return `[${value.map(canonical).join(',')}]`
  if (value && typeof value === 'object') return `{${Object.keys(value).sort().map(key => `${JSON.stringify(key)}:${canonical(value[key])}`).join(',')}}`
  return JSON.stringify(value)
}
function validSetup(setup) {
  return setup && setup.issue_1_completed === true && setup.blockers.length === 0 &&
    setup.source_manifest_scope === 'indexed_source_and_known_inputs' &&
    typeof setup.source_manifest_path === 'string' && setup.source_manifest_path.length > 0 &&
    setup.startup_checks && ['codegraph_available', 'baseline_verified', 'toolchain_verified', 'source_manifest_verified'].every(key => setup.startup_checks[key] === true) &&
    sameNumbers(setup.contracts.map(c => c.number), ORDER) && setup.contracts.every(c =>
      sameNumbers(c.dependencies, DEPENDENCIES[c.number]) && c.body_sha256 === CONTRACTS[c.number].body_sha256 &&
      c.checkbox_count === CONTRACTS[c.number].checkbox_count && sameNumbers(c.criteria_ids, criteriaFor(c.number)))
}
function validRealRows(rows, numbers, sourceId) {
  if (!Array.isArray(rows) || !rows.length || new Set(rows.map(row => row.id)).size !== rows.length) return false
  if (!rows.every(row => numbers.includes(row.issue) && row.source_id === sourceId && row.actual_effects_verified === true &&
      row.usage_accounted === true && ['openai', 'anthropic', 'gemini', 'magpie'].includes(row.provider) &&
      [row.id, row.scenario_id, row.requested_model, row.response_model, row.authorization_ref, row.ledger_ref].every(value => typeof value === 'string' && value.length > 0) &&
      [row.request_ids, row.task_ids, row.tool_call_ids, row.evidence_paths, row.runtime_entrypoints, row.criteria_ids].every(list =>
        Array.isArray(list) && list.length > 0 && new Set(list).size === list.length && list.every(value => typeof value === 'string' && value.length > 0)) &&
      row.criteria_ids.every(id => criteriaFor(row.issue).includes(id)) &&
      row.runtime_entrypoints.every(entry => ['sdk', 'ipc'].includes(entry)) &&
      row.api_shape === ({ openai: 'chat_completions', anthropic: 'messages', gemini: 'generate_content', magpie: 'chat_completions' })[row.provider] &&
      Number.isInteger(row.user_turns) && row.user_turns >= ([6, 10].includes(row.issue) ? 2 : 1) &&
      (row.provider !== 'magpie' || row.requested_model === 'group/auto-deepseek-v4-1-flash'))) return false
  return numbers.every(number => rows.some(row => row.issue === number)) &&
    (!numbers.includes(10) || ['openai', 'anthropic', 'gemini', 'magpie'].every(provider => rows.some(row => row.issue === 10 && row.provider === provider))) &&
    (!numbers.includes(6) || ['sdk', 'ipc'].every(entry => rows.some(row => row.issue === 6 && row.runtime_entrypoints.includes(entry)))) &&
    (!numbers.includes(5) || SCOPE_CRITERIA[5].every(scope => rows.some(row => row.issue === 5 && row.criteria_ids.includes(`5:scope:${scope}`))))
}
function accepted(verdict, expected, sourceId, requiredRealRows = null) {
  if (!verdict || verdict.accepted !== true || !verdict.contract_unchanged || !verdict.required_checks_passed ||
      !verdict.evidence_verified || verdict.blockers.length || verdict.source_id !== sourceId || verdict.after_source_id !== sourceId ||
      verdict.body_sha256_after !== expected.body_sha256 || verdict.checkbox_count !== expected.checkbox_count ||
      !sameNumbers(verdict.observed_criteria_ids, expected.criteria_ids)) return false
  const criteriaAccepted = verdict.criteria.length === expected.criteria_ids.length && new Set(verdict.criteria.map(c => c.id)).size === expected.criteria_ids.length &&
    verdict.criteria.every(c => expected.criteria_ids.includes(c.id) && c.status === 'pass' && c.evidence_paths.length > 0)
  if (!criteriaAccepted || requiredRealRows === null) return criteriaAccepted
  return verdict.real_evidence_verified === true && validRealRows(verdict.real_coverage, [expected.number], sourceId) &&
    sameNumbers(verdict.real_coverage.map(row => row.id), requiredRealRows.map(row => row.id)) &&
    verdict.real_coverage.every(row => canonical(row) === canonical(requiredRealRows.find(required => required.id === row.id)))
}
async function ask(prompt, opts) {
  try {
    return await agent(prompt, opts)
  } catch (error) {
    log(`代理 ${opts.label} 未完成：${String(error)}；保留阻塞，不视为通过。`)
    return null
  }
}

const configuredRepairs = args && args.maxRepairRounds
if (configuredRepairs !== undefined && (!Number.isInteger(configuredRepairs) || configuredRepairs < 0 || configuredRepairs > 6)) {
  throw new Error('maxRepairRounds must be an integer from 0 to 6; it never changes acceptance or authorization')
}
const maxRepairs = configuredRepairs === undefined ? 3 : configuredRepairs
log(`主线固定 11 项：${ORDER.map(n => `#${n}`).join('、')}。单项最多 ${maxRepairs} 轮修复；耗尽后明确记录阻塞，继续无依赖的任务。`)
phase('启动核对')
const setup = await ask(`${COMMON}
你负责启动核对，不实现产品功能、不调用真实模型。
1. Read docs/development-map.md；gh 回读恰好 11 项的完整正文、标题和状态，以及 #1/#12–#17/#19 状态。路线图和远端一致才继续；#1 是已完成起点，不重做。任何任务/依赖变化返回 contract 阻塞，不偷偷更新范围。
2. 回读正文以 UTF-8 原始正文计算 SHA256，逐行解析所有 checkbox，必须匹配冻结清单 ${JSON.stringify(CONTRACTS)}。criteria_ids 按正文顺序固定为 N:checkbox:1 至 N:checkbox:count；#5 再加八组能力 ID、#10 再加四服务 ID：${JSON.stringify(SCOPE_CRITERIA)}，格式 N:scope:名称。保存每个 ID 对应原文、完整正文、实际计数/摘要、依赖和证据待办 packet，不能靠代理摘要自行删减。正文或计数不同即 contract 阻塞，不自动更新本清单。全部 11 项一个不能少。
3. git fetch origin main，核对当前基线包含最新 main。若路线图 PR #23 尚未合入，可使用含该已审阅路线图的当前提交作为显式基线，记录两者；不自动合并 PR。已有冲突/未知源码改动先报告，不 reset/clean/stash。保留原有未跟踪材料。
4. 在原项目使用独立的普通工作分支（非 worktree）；当前若已是该任务的 workflow 分支，可沿用，否则创建分支。只有后续当前作者可改源码；不在 main 或文档 PR 分支直接实施。建立 /Volumes/ORICO 下新的运行证据目录，证据不混入产品源码。CodeGraph 必须能准确查询这一工作树；不可用/索引失效即阻塞，不 init/upgrade 或改用文本搜索。
5. 核对 macOS 实际版本/架构、Cargo/Rust 1.95、rustfmt、+1.95.0 clippy、短 TMPDIR、可用磁盘与构建目录。不安装系统工具，不处理其他平台。
6. 保存授权范围表：#2 的既有 Magpie 100M 按原范围记录，不自动覆盖 #6/#10；三家官方缺授权只标待授权，不把这点当全局离线准备失败。账本必须单一串行、可恢复、预留/未知不归零，不读取外部 Agent 凭据配置。
7. 保存输入/来源/依赖/环境，以及通过 CodeGraph 列出的当前源码和明确已知的 Cargo 清单/锁文件摘要。返回 source_manifest_path、source_manifest_scope=indexed_source_and_known_inputs，source_id 是这份实际清单摘要，不冒充最终完整构建身份。startup_checks 分别记录 codegraph_available、baseline_verified、toolchain_verified、source_manifest_verified 的实际核对结果。最终构建输入闭包、未编译二进制和缺失引用规则文件写 observations，不写 blockers；缺失规则不能靠猜测补齐，也不阻止遵循现有规则实施。
8. 当前已有普通 workflow 分支且跟踪源码干净时可沿用该分支，不为每次启动新建无改动分支。可读取上次 ${'/Volumes/ORICO/cayw-startup-20261020T012416Z-8f7112/startup-record.json'} 及 source-manifest.json 作为明确输入，但本次重新核对正文摘要、源码内容和索引，不能复用旧通过结论。只有实际 CodeGraph/基线/工具链/已知源码身份/任务契约失败才全局阻塞。11 项任务/123 条 ID 和最终所有真实模型、故障、构建证据要求不减少。
若全部启动检查通过且没有实际阻塞，ready=true、blockers=[]，其余事实保留 observations。返回实际文件路径；摘要/路径不允许编造。`, { label: 'mainline:setup', phase: '启动核对', schema: SETUP })

const results = ORDER.map(number => ({ number, status: 'pending', attempts: [], blockers: [] }))
const completed = new Set([1])
const prepared = validSetup(setup)
if (setup) {
  setup.reported_ready = setup.ready
  setup.ready = Boolean(prepared)
}
if (!prepared) {
  const blockers = setup && setup.blockers.length ? setup.blockers : [{ kind: 'tool_failure', detail: '启动返回缺失或未满足完整任务/依赖/源码核对要求' }]
  for (const result of results) {
    result.status = 'setup_blocked'
    result.blockers = blockers
  }
  log('启动核对未通过，11 项全部保留为未完成；不启动产品作者或真实模型。')
}

for (const number of ORDER) {
  if (!prepared) break
  phase(`Issue #${number}`)
  const result = results.find(item => item.number === number)
  const unmet = DEPENDENCIES[number].filter(dependency => !completed.has(dependency))
  if (unmet.length) {
    result.status = 'dependency_blocked'
    result.blockers = [{ kind: 'contract', detail: `尚未完成验收的前置：${unmet.map(n => `#${n}`).join('、')}` }]
    log(`#${number} 保留待完成：${result.blockers[0].detail}`)
    continue
  }
  const current = setup.contracts.find(item => item.number === number)
  let feedback = []
  for (let round = 0; round <= maxRepairs; round++) {
    const author = await ask(`${COMMON}
你是当前唯一源码作者，完整交付 Issue #${number}：${TASKS[number].title}。不要实现其他 Issue。
运行目录/分支/授权记录：${JSON.stringify(setup)}
权威任务包：${JSON.stringify(current)}
实现重点（不能代替任务包全部验收）：${JSON.stringify(TASKS[number])}
此前当前项审查问题：${JSON.stringify(feedback)}
每次先回读当前 Issue 正文和源码身份，契约变化停止并报告。通过 CodeGraph 定位现有实现，复用已有代码，缺陷先有行为失败证据再修复。
实现、逐条正向与拒绝/故障对照、取消/恢复和独立消费样例必须落到真实公开路径。完成当前包所有 criteria，保存逐条 evidence map、原始命令与退出码、源码/构建清单；报告里区分业务任务通过与预期故障场景通过。
运行针对性测试与必要完整检查，至少符合当前 AGENTS 的 fmt/check/test/clippy/offline 要求；不因 workspace 有已取消包而扩展功能。独立 examples/issue1-client 不由根 all-targets 自动覆盖，需要显式回归。
单项离线关卡用于推进实现依赖，不代表全部真实任务验收完成。#6/#10 自身必需真实部分，以及最终覆盖全部 11 项的真实组合任务，只在范围明确授权时通过唯一串行预算 runner 执行；无授权时完成可做的离线部分并给出 partial/blocked 与精确缺项，不把现有 #2 授权当新的许可，不等待无限重试。#3/#7/#11 的离线故障场景可先验，最终仍须实际参与真实模型任务并逐项有证据。
未授权系统/外部操作不得做，也不得删减验收。未结未知效果或原进程清理不明必须记录并停止受影响执行。
只在本项完整交付及自测足以独立审查时返回 ready_for_review，source_id 是当前完整源码清单摘要，report_path/commands_manifest_path 必须真实存在。`,
      { label: `issue:${number}:implement:${round}`, phase: `Issue #${number}`, schema: AUTHOR })
    result.attempts.push({ round, author, review: null })
    if (!author || author.status !== 'ready_for_review' || author.blockers.length) {
      result.status = author && author.status === 'partial' ? 'partial' : 'blocked'
      result.blockers = author && author.blockers.length ? author.blockers : [{ kind: 'tool_failure', detail: '作者未提交可验收交付；缺失/部分结果不能视为完成' }]
      break
    }
    const review = await ask(`${COMMON}
你是独立验收者，不是作者；源码只读，不替作者修复，不提交/推送或改 Issue。测试产生的构建文件与本轮证据可写入规定目录，禁止与作者并发修改源码。
独立核对 Issue #${number} 的全部条件。任务包：${JSON.stringify(current)}；作者材料：${JSON.stringify(author)}；运行上下文：${JSON.stringify(setup)}。
自行回读 GitHub 任务正文核对 body SHA256，重算当前完整源码身份，阅读实际代码/报告并实际复跑关键行为反例、公开 SDK/认证 IPC 适用路径及 required checks。源码发现只用 CodeGraph。
独立解析回读正文的所有 checkbox，报告 body_sha256_after、checkbox_count、observed_criteria_ids，须与冻结正文和全部 ID ${JSON.stringify(current.criteria_ids)} 一致；多出/遗漏/变化报告 contract 阻塞，不能照抄 setup 省略原文。逐条返回 criteria，每条 pass 须有实际证据路径；缺功能/未跑/未知写 fail 或 missing，禁止因作者摘要/测试数/最终回答而通过。单项离线复核可以 real_coverage=[]、real_evidence_verified=false；不得声称已满足最终全 11 项真实模型覆盖。
核对原始日志、退出状态、输入/权限/环境/构建与实际效果；负例证明验证器会抓住假成功，清理不明/未知成本不能被抹去。所需真实模型无授权则不调用，保留 authorization 阻塞。
source_id 和 after_source_id 都是你实际重算的审查前后源码摘要，必须与作者版本一致；期间变更就失败，不审查旧二进制后放行新源码。给出可复现的具体问题；全部条件/必要检查/证据通过才 accepted=true。`,
      { label: `issue:${number}:verify:${round}`, phase: `Issue #${number}`, schema: VERDICT })
    result.attempts[result.attempts.length - 1].review = review
    if (accepted(review, current, author.source_id)) {
      result.status = 'issue_accepted'
      result.blockers = []
      result.final_source_status = 'pending_final_revalidation'
      result.source_id = author.source_id
      result.report_path = review.report_path
      completed.add(number)
      log(`#${number} 当前源码的单项实现关卡通过；最终源码与真实模型组合验收仍待完成，不代表已提交、合入或关闭。`)
      break
    }
    feedback = review && review.blockers.length ? review.blockers : [{ kind: 'evidence', detail: '独立验收未满足逐条证据、原始日志、契约或同一源码要求' }]
    result.blockers = feedback
    const fixable = feedback.every(item => ['code', 'tests', 'evidence'].includes(item.kind))
    if (!review || !fixable || round === maxRepairs) {
      result.status = 'blocked'
      log(`#${number} 未完成验收；保留 ${feedback.length} 个阻塞，不削减条件。`)
      break
    }
    log(`#${number} 进入第 ${round + 1} 轮修复；后续依赖任务暂不开始。`)
  }
}

phase('最终组合验收')
let integration = null
let finalReviews = []
let finalAccepted = false
const allIssuesAccepted = results.every(result => result.status === 'issue_accepted')
if (prepared && allIssuesAccepted) {
  integration = await ask(`${COMMON}
你负责最终组合运行，冻结当前最终完整源码身份，不能再改产品源码。上下文：${JSON.stringify(setup)}；逐项记录：${JSON.stringify(results)}。
在这一份最终源码上运行 cargo fmt --all -- --check、cargo check --workspace --locked、cargo test --workspace --locked、cargo +1.95.0 clippy --workspace --all-targets --locked -- -D warnings、cargo run -p cayw-agent --example offline --locked，以及独立 issue1-client 和 11 项具名验收入口。
同时验证实际无 UI/plugin/remote/VerifyTool 产品依赖的构建与公开 SDK/IPC 任务。测试消费者可依赖 VerifyTool，产品运行不能反向依赖。
按全部 Issue 条件更新证据到最终构建，不能把早期单项版本证据直接当最终版本通过。最终必须真实 Provider 经公开 Runtime 的组合任务逐项覆盖全部 11 个 Issue；共享场景可以覆盖多项，但必须建立每项真实覆盖行，不能只给 #6/#10 请求或把离线数据填成真实。
真实场景至少包含：VerifyTool 真实任务/成本记录（#2）；实际生成并执行 Luau/签名（#3）；受管真实工具进程/取消清理（#4）；八组 CodeMode 能力的适用组合与实际效果（#5）；SDK 与 IPC 多用户回合（#6）；真实任务中断后的同 Session 核对恢复（#7）；模型提出精确申请、可信宿主裁决后实际执行/撤销（#8）；模型按需读取真实 Skill 并调用真实本地 MCP（#9）；四项官方/兼容服务各自真实工具往返（#10）；真实模型驱动多 Session 任务和公平/隔离事件（#11）；真实任务触发本机资源策略与权威内容重开恢复（#18）。模型自报或让一个无关请求旁挂离线结果不算覆盖。
每项至少一条 real_provider_coverage；#10 至少四条对应 openai/anthropic/gemini/magpie。行必须记录唯一 id、issue、scenario_id、最终 source_id、provider、api_shape（chat_completions/messages/generate_content）、requested_model/response_model、实际 user_turns、所覆盖冻结 criteria_ids、授权依据 authorization_ref、唯一串行预算账本 ledger_ref、真实 Provider request_ids、Runtime task_ids、实际 tool_call_ids、实际 SDK/IPC 入口 runtime_entrypoints、原始请求/流/工具/持久事件/外部效果 evidence_paths，actual_effects_verified 与 usage_accounted。#6 的行合起来覆盖 SDK 和 IPC，#5 的行合起来覆盖八组 scope criteria；#6/#10 各真实任务至少两个用户回合。用量未知如实保留预留不归零；usage_accounted 仅表示账已核对，不表示已知零费用。
所有真实请求仅在有效授权和剩余额度内串行运行；已有 #2 授权不能自动扩到整条主线或官方服务。缺额度/授权时保存离线结果和精确阻塞，ready=false、real_coverage_verified=false，不能用离线通过或空映射补齐。
每个场景保存源码/构建、输入、权限、原始命令、退出码、事件/持久记录、实际外部结果。记录每条成功/预期拒绝/失败/未验证；真实覆盖不替代精确故障/越权/预算/清理负例，二者都必须通过。
返回 issues 恰好 ${JSON.stringify(ORDER)}，实际 report_path、最终 source_id；没有全 11 项的真实执行证据就不能 ready/full_checks_passed/real_coverage_verified=true。`,
    { label: 'mainline:final-run', phase: '最终组合验收', schema: FINAL })
  if (integration && integration.ready && integration.full_checks_passed && integration.real_coverage_verified &&
      integration.blockers.length === 0 && sameNumbers(integration.issues, ORDER) && validRealRows(integration.real_provider_coverage, ORDER, integration.source_id)) {
    for (const number of ORDER) {
      const current = setup.contracts.find(item => item.number === number)
      const verdict = await ask(`${COMMON}
你是最终源码的独立复核者。不要修改源码或重复发起付费请求；实际运行必要离线/故障验证，核对已授权真实原始记录及其最终构建归属。
仅核对 Issue #${number}，任务包 ${JSON.stringify(current)}；最终运行报告 ${JSON.stringify(integration)}；上下文 ${JSON.stringify(setup)}。
回读 Issue 与源码，完整核对所有 criteria；每项引用真实证据。对最终版本重新验证关键反例、调用配对、持久状态和实际效果，不能照抄作者结论或旧单项通过。
source_id/after_source_id 必须实际重算且等于 ${integration.source_id}。独立重算正文摘要、checkbox_count 与 observed_criteria_ids；正文变化、源码变化、模型证据不覆盖最终构建或任何缺项都拒绝。
本项必需真实覆盖行 ${JSON.stringify(integration.real_provider_coverage.filter(row => row.issue === number))}。逐条核对授权和账本、真实请求/流/工具/事件/实际效果及最终构建，不把独立审查者或模型的文字声明当服务调用证据；把核对后的原行返回 real_coverage，全部可信才 real_evidence_verified=true。不发新的模型请求，不修改行内容来伪造匹配；缺失/损坏/不属于本项则拒绝并报告。
返回恰好 ${JSON.stringify(current.criteria_ids)}，所有正负条件与本项真实覆盖都通过才 accepted=true。`,
        { label: `issue:${number}:final-verify`, phase: '最终组合验收', schema: VERDICT })
      const verified = accepted(verdict, current, integration.source_id, integration.real_provider_coverage.filter(row => row.issue === number))
      finalReviews.push({ number, verdict, accepted: verified })
      results.find(item => item.number === number).final_source_status = verified ? 'accepted_with_real_provider' : 'revalidation_failed'
    }
    finalAccepted = finalReviews.length === ORDER.length && finalReviews.every(item => item.accepted)
  }
} else {
  log('至少一项尚未验收；不运行伪完整组合验收，交付记录仍逐一列出全部 11 项。')
}
// Single-Issue evidence keeps its original source binding. Do not present it as
// passing the final tree when later Issues changed shared implementation files.
for (const result of results) {
  if (result.status === 'issue_accepted' && result.final_source_status !== 'accepted_with_real_provider') {
    result.final_source_status = result.final_source_status === 'revalidation_failed' ? 'revalidation_failed' : 'not_verified_on_final_source'
  }
}

phase('交付记录')
const outcome = {
  status: finalAccepted ? 'accepted_locally' : 'incomplete',
  issue_order: ORDER,
  scope: 'macOS local headless; SDK + authenticated local IPC',
  setup, results, integration, final_reviews: finalReviews,
  all_issues_accepted: allIssuesAccepted, final_accepted: finalAccepted,
  git_delivery: 'not_committed_or_pushed', remote_issue_updates: 'not_performed',
}
const record = await ask(`${COMMON}
你是交付记录员，不修改产品源码，不补写通过，不提交/推送/创建 PR/关闭 Issue。
实际结果：${JSON.stringify(outcome)}
将机器可读 state.json、逐 Issue 结果与可复跑报告保存到本轮运行证据目录（启动失败时在 /Volumes/ORICO 下创建独立证据目录）。表格必须恰好有 ${JSON.stringify(ORDER)}，列已实现、单项验收、最终组合复核、依赖/授权/环境阻塞、源码身份、命令/日志与实际交付状态。
所有 blocker、未读日志、失败命令、未知资源/用量和修复上限保留；不把 setup/partial/dependency_blocked 改为完成。若全部最终通过，说明只是本地验收完成，等待用户另行决定 Git/远端交付。
写入后回读 state/report，核对 11 项全在、结果与上方实际值一致；verified 只表示记录核对，不代表产品通过。不要编辑 README/AGENTS/architecture 或路线图进度。`,
  { label: 'mainline:record', phase: '交付记录', schema: RECORD })
const recordVerified = record && record.verified && sameNumbers(record.recorded_issues, ORDER)
log(finalAccepted && recordVerified
  ? '11 项在最终源码上通过本地验收，证据已保存；尚未提交、推送或关闭 Issue。'
  : '工作流结束时仍有未完成项或记录失败，全部 11 项状态已保留；不能宣称主线已完成。')
return { ...outcome, record, record_verified: Boolean(recordVerified), status: finalAccepted && recordVerified ? 'accepted_locally' : 'incomplete' }
