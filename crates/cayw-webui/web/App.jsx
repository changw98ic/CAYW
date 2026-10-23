import React, { useState, useRef, useEffect, useCallback } from 'react';
import { flushSync } from 'react-dom';
import {
  CORE_PROTOCOL_VERSION,
  MAX_MESSAGES,
  clip,
  object,
  array,
  recordText,
  readMethods,
  messageText,
  messagePresentation,
  createView,
  RpcError,
  beginWrite,
  noteSessionError,
  finishWrite,
  terminalReadCursor,
  consumeTerminalRead,
  applySnapshot,
  consumeEvents,
  sessionRunning,
  sessionBlocked,
  canFork,
  stateLabel,
  stateName,
  unresolved,
  frozen,
  summarizeProjects,
  summarizePlugins,
} from './state.js';
import {
  clsx,
  BrandWordmark,
  FishLogo,
  StateDot,
  Button,
  Menu,
  Modal,
  RiskConfirmation,
  DisclosureRow,
  Tooltip,
  IconAgentPresetOutline16,
  IconCheckOutline16,
  IconChevronDownOutline14,
  IconChevronRightOutline14,
  IconCloseFill14,
  IconCloseOutline16,
  IconCodeOutline16,
  IconCopyOutline16,
  IconDarkOutline16,
  IconDataOutline16,
  IconEditOutline16,
  IconEllipsisOutline16,
  IconFolderClose16,
  IconFolderOpen16,
  IconFollowsystemOutline16,
  IconLightOutline16,
  IconNewChatOutline16,
  IconPanelLeftOutline16,
  IconPersonalizationOutline16,
  IconPlusOutline16,
  IconProjectAddOutline16,
  IconRefreshOutline16,
  IconSearchOutline16,
  IconSendOutline16,
  IconSettingsOutline14,
  IconSettingsOutline16,
  IconTriangleRightFill14,
} from './components/DshPrimitives.jsx';

const EVENT_TYPE_LABELS = {
  session_created: '创建会话',
  attempt_started: '启动任务尝试',
  attempt_finished: '任务尝试完成',
  attempt_recovered: '任务尝试恢复',
  agent: '模型流式输出',
  agent_message: '模型回复消息',
  user_prompt: '用户发送消息',
  tool_execution_start: '开始执行代码单元',
  tool_execution_end: '完成执行代码单元',
  approval_requested: '请求权限审批',
  approval_decided: '记录审批决定',
  reconciled: '完成副作用核对',
};

const CAPABILITY_LABELS = {
  'fs.read': '读取文件',
  'fs.write': '写入文件',
  'fs.patch': '修改文件',
  'fs.list': '列出文件',
  'fs.stat': '查看文件信息',
  'search.query': '检索内容',
  'git.status': '查看 Git 状态',
  'git.diff': '查看代码改动',
  'git.log': '查看提交记录',
  'git.show': '查看指定提交',
  'build.run': '运行预设构建',
  'test.run': '运行预设测试',
  'mcp.call': '调用宿主连接的工具',
  'mcp.discover': '查看宿主连接的工具',
  'mcp.describe': '读取工具说明',
  'skills.list': '查看可用技能',
  'skills.read': '读取指定技能',
};

const SHIELD_OUTLINE =
  'M8.20554 0.899994L14.7901 3.36857V7.01026C14.7901 12 11.0466 14.2103 8.20554 15.3C5.36446 14.2103 1.62012 12 1.62012 7.01026V3.36857L8.20554 0.899994Z';

function PermissionGlyph({ value }) {
  if (value === 'read-only' || value === 'standard') {
    return (
      <svg width="16" height="16" viewBox="0 0 16 16" fill="none" aria-hidden="true">
        <path
          d={SHIELD_OUTLINE}
          stroke="currentColor"
          strokeWidth="1.31831"
          strokeLinejoin="round"
        />
        <path
          d="M12.1654 5.7552L8.9447 9.41475C8.73044 9.65816 8.53628 9.8804 8.35774 10.0423C8.1713 10.2114 7.94235 10.3717 7.64016 10.4254C7.48207 10.4535 7.32 10.4552 7.16151 10.4294C6.85843 10.3801 6.62728 10.2223 6.43836 10.0559C6.25752 9.89653 6.06037 9.67732 5.84264 9.43705L4.72925 8.20897L5.63557 7.38707L6.74897 8.61594C6.98603 8.87755 7.12974 9.03533 7.24673 9.13839C7.31033 9.19443 7.34485 9.21476 7.35823 9.22122C7.38068 9.22484 7.40352 9.22515 7.42593 9.22122C7.40522 9.22502 7.42893 9.23294 7.53583 9.136C7.65132 9.03126 7.79316 8.87139 8.02643 8.60638L11.2479 4.94763L12.1654 5.7552Z"
          fill="currentColor"
        />
      </svg>
    );
  }
  if (value === 'danger-full-access') {
    return (
      <svg width="16" height="16" viewBox="0 0 16 16" fill="none" aria-hidden="true">
        <path
          d={SHIELD_OUTLINE}
          stroke="currentColor"
          strokeWidth="1.31831"
          strokeLinejoin="round"
        />
        <path d="M9.10094 4.5V8.75939H7.59888V4.5H9.10094Z" fill="currentColor" />
        <path d="M9.10094 9.8114V11.5H7.59888V9.8114H9.10094Z" fill="currentColor" />
      </svg>
    );
  }
  return (
    <svg width="16" height="16" viewBox="0 0 16 16" fill="none" aria-hidden="true">
      <path
        d="M8.08887 0.251709C8.20479 0.23085 8.32486 0.241168 8.43652 0.282959L15.0215 2.75171C15.2787 2.84819 15.4492 3.09414 15.4492 3.3689V7.0105C15.4492 7.10986 15.4441 7.2081 15.4414 7.30542C15.0285 7.07175 14.5905 6.87695 14.1309 6.73022V3.82495L8.20508 1.60327L2.2793 3.82495V7.0105C2.27936 9.7171 3.4745 11.5379 5.02734 12.7947C5.01025 12.9942 5 13.1962 5 13.4001C5.00001 13.7617 5.02722 14.1169 5.08008 14.4636C2.91555 13.0393 0.961014 10.752 0.960938 7.0105V3.3689C0.960938 3.09417 1.13146 2.84821 1.38867 2.75171L7.97461 0.282959L8.08887 0.251709Z"
        fill="currentColor"
      />
      <path d="M11.3525 5.64688V6.85688H5V5.64688H11.3525Z" fill="currentColor" />
      <path d="M9.5824 8.29376V9.50376H5V8.29376H9.5824Z" fill="currentColor" />
      <path
        d="M14.6647 15.6852H10.0338C10.3878 15.3751 10.7567 15.0517 11.0772 14.7706C11.2531 14.6164 11.4144 14.4746 11.5511 14.3547H14.6647V15.6852Z"
        fill="currentColor"
      />
      <path
        d="M8.14852 14.1308L7.33925 15.4976C7.22458 15.6912 7.42245 15.9194 7.63037 15.8333L9.09785 15.2254L15.0399 10.0719L14.0905 8.97733L8.14852 14.1308Z"
        fill="currentColor"
      />
    </svg>
  );
}

function HeroGlow({ className }) {
  return (
    <svg className={className} viewBox="0 0 1051 468" fill="none" aria-hidden="true">
      <defs>
        <filter
          id="dsh-empty-glow"
          x="0"
          y="0"
          width="1051"
          height="468"
          filterUnits="userSpaceOnUse"
          colorInterpolationFilters="sRGB"
        >
          <feFlood floodOpacity="0" result="BackgroundImageFix" />
          <feBlend mode="normal" in="SourceGraphic" in2="BackgroundImageFix" result="shape" />
          <feGaussianBlur stdDeviation="50" result="effect1_foregroundBlur" />
        </filter>
      </defs>
      <g filter="url(#dsh-empty-glow)">
        <ellipse cx="525.5" cy="234" rx="425.5" ry="134" fill="#6187D8" fillOpacity="0.08" />
      </g>
    </svg>
  );
}

const PERMISSION_PRESETS = [
  {
    id: 'standard',
    label: '按需审批',
    zhLabel: '按需审批',
    desc: '只读操作直接执行；文件写入、修改、构建与外部工具需每次或按任务明确授权。',
  },
  {
    id: 'workspace-write',
    label: '工作区读写',
    zhLabel: '工作区读写',
    desc: '当前工作区内文件读写优先使用任务授权，跨目录或外部操作仍需审批。',
  },
  {
    id: 'danger-full-access',
    label: '完全访问',
    zhLabel: '完全访问',
    desc: '减少确认步骤，允许直接执行更多操作；仅建议在完全信任当前任务时启用。',
  },
];

const AGENT_PRESETS = [
  {
    id: 'code',
    name: '代码编排',
    badge: '内置 · 默认',
    desc: '通过受控代码单元呈现宿主能力，让模型组合多步操作并统一经过权限与预算边界。',
  },
  {
    id: 'standard',
    name: '标准模式',
    badge: '内置',
    desc: '完整编码智能体，支持文件读写、代码检索、版本管理、预设构建测试与宿主连接工具。',
  },
  {
    id: 'strongflow',
    name: '强流程编排',
    badge: '扩展',
    desc: '为复杂任务提供阶段门禁与检查点；扩展卸载后已绑定会话保持冻结，支持确认后转为普通聊天分支。',
  },
];

const BUILTIN_CAPABILITY_GROUPS = [
  {
    id: 'fs',
    title: '文件与目录',
    summary: '工作区内文件读写、补丁替换与目录属性查询，统一经过根目录固定与哈希校验。',
    paramLabel: '单次读取上限（字节）',
    defaultParam: '262144',
    authMode: '只读直接执行，写入与修改按模式审批',
    badge: '已启用',
    items: [
      ['读取文件', '读取文件内容与校验摘要'],
      ['写入文件', '创建新文件或按摘要安全更新文件'],
      ['修改文件', '按精确上下文替换文件片段'],
      ['目录与属性', '列出目录项与读取文件基础信息'],
    ],
  },
  {
    id: 'code',
    title: '代码检索与语法大纲',
    summary: '有界文本检索、路径匹配、源码符号大纲提取与改动片段对比。',
    paramLabel: '单次检索返回上限（条）',
    defaultParam: '200',
    authMode: '工作区内只读直接执行',
    badge: '已启用',
    items: [
      ['文本与路径检索', '在工作区内检索文本内容与文件路径'],
      ['源码结构大纲', '提取源码文件的结构与符号列表'],
      ['改动片段对比', '计算文件修改前后的差异片段'],
    ],
  },
  {
    id: 'git',
    title: '版本管理',
    summary: '查看工作区未提交改动、代码差异、提交历史与单次提交详情。',
    paramLabel: '历史记录默认条数',
    defaultParam: '50',
    authMode: '只读查询直接执行',
    badge: '已启用',
    items: [
      ['工作区状态', '查看当前未提交的文件变更状态'],
      ['代码改动对比', '查看未提交或指定范围的代码改动'],
      ['提交记录与详情', '查看提交历史与单次提交内容'],
    ],
  },
  {
    id: 'build',
    title: '预设构建与测试',
    summary: '执行工作区显式配置的预设构建与测试任务，受独立进程组与超时预算控制。',
    paramLabel: '任务执行超时（秒）',
    defaultParam: '120',
    authMode: '需明确任务授权或审批',
    badge: '已启用',
    items: [
      ['预设构建任务', '运行已配置的预设构建任务'],
      ['预设测试任务', '运行已配置的预设测试任务'],
    ],
  },
  {
    id: 'mcp',
    title: '外部工具与技能包',
    summary: '由本机统一接入外部工具服务与本地技能说明，按需发现并经过统一授权。',
    paramLabel: '工具调用超时（秒）',
    defaultParam: '60',
    authMode: '调用外部工具需明确审批',
    badge: '已启用',
    items: [
      ['发现与读取工具', '查看已连接的工具服务与参数说明'],
      ['调用外部工具', '在授权范围内执行外部工具请求'],
      ['读取技能说明', '按需加载本地技能列表与文档'],
    ],
  },
];

function renderInlineTokens(text) {
  const parts = String(text ?? '').split(/(`[^`\n]+`|\*\*[^*\n]+\*\*)/g);
  return parts.map((part, idx) => {
    if (part.startsWith('`') && part.endsWith('`') && part.length >= 2) {
      return <code key={idx}>{part.slice(1, -1)}</code>;
    }
    if (part.startsWith('**') && part.endsWith('**') && part.length >= 4) {
      return <strong key={idx}>{part.slice(2, -2)}</strong>;
    }
    return part;
  });
}

function FormattedMarkdown({ text }) {
  const raw = String(text ?? '');
  const blocks = raw.split(/(```[\s\S]*?```)/g);
  return (
    <div className="_markdown_1nba0_5">
      {blocks.map((block, bIdx) => {
        if (block.startsWith('```') && block.endsWith('```') && block.length >= 6) {
          const firstLineMatch = block.slice(3, -3).match(/^([a-zA-Z0-9_-]+)\n/);
          const lang = firstLineMatch ? firstLineMatch[1] : 'code';
          const inner = block.slice(3, -3).replace(/^[a-zA-Z0-9_-]+\n/, '');
          return (
            <div key={bIdx} className="_block_178r4_4">
              <div className="_bannerWrap_178r4_21">
                <div className="_banner_178r4_21">
                  <span className="_infostring_178r4_42">{lang}</span>
                  <span className="_action_178r4_53">
                    <button
                      type="button"
                      className="_copyButton_178r4_59"
                      title="复制代码"
                      onClick={() => {
                        if (navigator.clipboard?.writeText) {
                          navigator.clipboard.writeText(inner).catch(() => {});
                        }
                      }}
                    >
                      <IconCopyOutline16 size={14} />
                    </button>
                  </span>
                </div>
              </div>
              <pre className="_plain_178r4_94">
                <code>{inner}</code>
              </pre>
            </div>
          );
        }
        const paragraphs = block.split(/\n{2,}/g).filter(p => p.trim() !== '');
        if (!paragraphs.length) return null;
        return (
          <React.Fragment key={bIdx}>
            {paragraphs.map((p, pIdx) => (
              <p key={pIdx}>{renderInlineTokens(p)}</p>
            ))}
          </React.Fragment>
        );
      })}
    </div>
  );
}

function errorText(error) {
  return clip(
    {
      session_frozen:
        '强流程当前不可用，会话保持冻结。请在会话提示中确认后创建普通聊天分支。',
      needs_reconciliation: '工具副作用尚未核对，需要由宿主检查真实结果。',
      tool_outcome_unknown: '工具结果尚未核对，需要由宿主检查真实副作用。',
    }[error?.code] ??
      (error?.outcome_unknown
        ? '连接中断或请求结果待确认。页面不会自动重发，请等待事件同步或由宿主核对。'
        : error?.message ?? '发生未知错误。'),
    2500,
  );
}

function sessionSummary(snapshot) {
  const {
    id,
    label,
    cursor,
    state,
    attempt,
    pending_approvals,
    unresolved_calls,
    binding,
    messages,
  } = snapshot;
  return {
    id,
    label,
    cursor,
    state,
    attempt,
    pending_approvals,
    unresolved_calls,
    binding,
    ...(Array.isArray(messages) ? { messages: messages.slice(0, 4) } : {}),
  };
}

function trackAsync(promise) {
  if (typeof window !== 'undefined') {
    window.__lastActionPromise = promise;
  }
  return promise;
}

function bind(handlers, extraRef) {
  return node => {
    if (extraRef) extraRef.current = node;
    if (node) node.__handlers = handlers;
  };
}

function shortTitle(text, max = 30) {
  const clean = String(text ?? '')
    .replace(/[\x00-\x08\x0b\x0c\x0e-\x1f\x7f]/g, '')
    .replace(/\s+/g, ' ')
    .trim();
  if (!clean) return '新会话';
  return clean.length <= max ? clean : `${clean.slice(0, max)}…`;
}

function stateDotKind(sessionOrSnap, view) {
  const st = stateName(sessionOrSnap);
  if (st === 'running') return 'ongoing';
  if (
    st === 'awaiting_approval' ||
    st === 'needs_reconciliation' ||
    st === 'frozen' ||
    unresolved(sessionOrSnap) ||
    frozen(sessionOrSnap, view)
  ) {
    return 'warning';
  }
  return 'done';
}

export function CAYWApp({ env } = {}) {
  const fetchFn = env?.fetch ?? ((url, options) => fetch(url, options));
  const setTimer = env?.setTimeout ?? ((fn, delay) => setTimeout(fn, delay));
  const clearTimer = env?.clearTimeout ?? (id => clearTimeout(id));

  const [, setRenderTick] = useState(0);
  const forceRender = useCallback(() => {
    try {
      flushSync(() => setRenderTick(t => t + 1));
    } catch {
      setRenderTick(t => t + 1);
    }
  }, []);

  const stateRef = useRef(null);
  if (!stateRef.current) {
    stateRef.current = {
      loginGeneration: 0,
      selectionGeneration: 0,
      listGeneration: 0,
      writeSequence: 0,
      pollFailures: 0,
      timer: undefined,
      controllers: new Set(),
      selected: null,
      sessions: [],
      views: new Map(),
      promptPending: new Set(),
      approvalPending: new Set(),
      cancelPending: false,
      createBusy: false,
      reconnectBusy: false,
      refreshSessionsBusy: false,
      refreshProjectsBusy: false,
      refreshPluginsBusy: false,
      refreshPermissionsBusy: false,
      addProjectBusy: false,
      forkBusy: false,
      forkOpen: false,
      forkLabel: '',
      forkConfirm: false,
      forkError: '',
      promptText: '',
      projectWorkspaceInput: '',
      projectLabelInput: '',
      connectionStatus: '正在连接本机…',
      listStatus: '',
      pollStatus: '',
      projectsStatus: '',
      pluginsStatus: '',
      permissionsStatus: '',
      globalError: '',
      activePage: 'conversation',
      activeProject: null,
      projects: [
        {
          id: 'default',
          label: 'CAYW',
          workspace: '/Volumes/ORICO/CAYW',
          address: '本机 IPC',
          default: true,
        },
      ],
      projectStates: new Map(),
      cachedCapabilities: [],
      sessionTitleOverrides: new Map(),
      sessionPickerOpen: true,
      sessionSearchQuery: '',
      searchExpanded: false,
      sidebarCollapsed: false,
      expandedGroups: {},
      // Hero & Composer menus
      heroWsMenuOpen: false,
      heroPresetMenuOpen: false,
      selectedAgentPreset: 'code',
      modelMenuOpen: false,
      selectedModel: 'deepseek-v4.1-flash',
      modelNotice: '',
      editingProviderId: null,
      editProviderBaseUrl: '',
      editProviderApiKey: '',
      addingModelProviderId: null,
      newModelId: '',
      newModelDesc: '',
      addingProviderOpen: false,
      newProviderName: '',
      newProviderBaseUrl: '',
      newProviderApiKey: '',
      newProviderModelId: '',
      modelProviders: [
        {
          id: 'local-deepseek',
          name: 'DeepSeek 模型服务',
          protocol: '兼容接口',
          baseUrl: 'http://127.0.0.1:3425/v1',
          configured: true,
          builtin: true,
          models: [
            {
              id: 'deepseek-v4.1-flash',
              label: 'deepseek-v4.1-flash',
              desc: '高速代码生成与工具编排（默认）',
            },
            {
              id: 'deepseek-chat',
              label: 'deepseek-chat',
              desc: '通用对话与日常开发任务',
            },
            {
              id: 'deepseek-reasoner',
              label: 'deepseek-reasoner',
              desc: '深度推理与复杂架构分析',
            },
          ],
        },
      ],
      // DSH Settings Modal state
      settingsOpen: false,
      settingsTab: 'general',
      themeMode: 'dark',
      enterBehavior: 'enter',
      // Connection & Account (Web<->Device tokenless, Server<->Device one-time RSA, Web<->Server account login)
      connectionMode: 'local_device',
      serverLoggedIn: false,
      serverUsername: 'admin',
      serverIsDefaultCredentials: true,
      serverLoginUserDraft: 'admin',
      serverLoginPassDraft: 'admin',
      serverLoginError: '',
      serverLoginBusy: false,
      serverEditingCredentials: false,
      serverCurrentPassDraft: '',
      serverNewUserDraft: '',
      serverNewPassDraft: '',
      serverConfirmPassDraft: '',
      serverAccountNotice: '',
      deviceBinding: {
        bound: true,
        reused_persisted_binding: true,
        device_id: 'device-macos-local-01',
        server_url: 'wss://127.0.0.1:8787',
        algorithm: 'RSA-OAEP-SHA256 + RSA-SHA256',
        encrypted_channel_verified: true,
        auto_reconnect: true,
        device_fingerprint: 'SHA256:8f9d1a0b3e7c4f219a6b5c8d0e1f2a3b',
        server_fingerprint: 'SHA256:1c4e7b2a9d0f6e3c8b5a2d1f4c7e9a0b',
        device_public_key_pem:
          '-----BEGIN RSA PUBLIC KEY-----\nUlNBLVBVQjowMTAwMDE6YzlmM2E4...\n-----END RSA PUBLIC KEY-----',
      },
      deviceServerUrlDraft: 'wss://127.0.0.1:8787',
      deviceBindingNotice: '',
      showRsaPem: false,
      activeDeviceId: 'device-macos-local-01',
      boundDevices: [
        {
          device_id: 'device-macos-local-01',
          label: '本机 macOS 设备',
          kind: 'local_and_rsa',
          builtin: true,
          active: true,
          bound: true,
          server_url: 'wss://127.0.0.1:8787',
          algorithm: 'RSA-OAEP-SHA256 + RSA-SHA256',
          encrypted_channel_verified: true,
          auto_reconnect: true,
          device_fingerprint: 'SHA256:8f9d1a0b3e7c4f219a6b5c8d0e1f2a3b',
          server_fingerprint: 'SHA256:1c4e7b2a9d0f6e3c8b5a2d1f4c7e9a0b',
          device_public_key_pem:
            '-----BEGIN RSA PUBLIC KEY-----\nUlNBLVBVQjowMTAwMDE6YzlmM2E4...\n-----END RSA PUBLIC KEY-----',
        },
      ],
      addDeviceModalOpen: false,
      newDeviceLabel: '',
      newDeviceId: '',
      newDeviceServerUrl: 'wss://127.0.0.1:8787',
      newDeviceMode: 'auto',
      newDevicePem: '',
      addDeviceBusy: false,
      addDeviceError: '',
      lastCreatedDevice: null,
      expandedDevicePemIds: {},
      heroDeviceMenuOpen: false,
      pluginsSubTab: 'config',
      expandedPluginCards: {},
      expandedInventoryCards: {},
      pluginSearchQuery: '',
      pluginParamValues: {},
      pluginSavedNotice: '',
      // MCP & Skills state
      mcpServers: [
        {
          id: 'codegraph',
          name: 'CodeGraph 代码语义图谱',
          transport: '本机标准输入输出',
          endpoint: 'codegraph-mcp --workspace .',
          status: 'active',
          statusLabel: '已连接',
          enabled: true,
          builtin: true,
          tools: [
            { name: 'codegraph.find_symbol', desc: '跨文件查找符号定义与声明位置' },
            { name: 'codegraph.find_references', desc: '查询符号在工作区内的所有引用与调用点' },
            { name: 'codegraph.dependencies', desc: '分析模块与文件的依赖关系图' },
          ],
        },
        {
          id: 'playwright',
          name: 'Playwright 浏览器自动化',
          transport: '本机标准输入输出',
          endpoint: 'playwright-mcp --headless',
          status: 'active',
          statusLabel: '已连接',
          enabled: true,
          builtin: true,
          tools: [
            { name: 'browser_navigate', desc: '打开指定网页地址并等待加载' },
            { name: 'browser_snapshot', desc: '获取页面无障碍结构快照' },
            { name: 'browser_take_screenshot', desc: '截取当前视口或完整页面图像' },
          ],
        },
        {
          id: 'context7',
          name: 'Context7 库与框架文档',
          transport: '远程事件流',
          endpoint: 'https://mcp.context7.com/mcp',
          status: 'active',
          statusLabel: '已连接',
          enabled: true,
          builtin: true,
          tools: [
            { name: 'resolve-library-id', desc: '解析第三方库与框架的标准标识' },
            { name: 'query-docs', desc: '按版本检索官方文档与代码示例' },
          ],
        },
      ],
      expandedMcpCards: {},
      addingMcpOpen: false,
      newMcpName: '',
      newMcpTransport: '本机标准输入输出',
      newMcpEndpoint: '',
      mcpNotice: '',
      skillsCatalog: [
        {
          name: 'code-review',
          description: '对照仓库编码规范与任务需求双轴审查代码改动。',
          source: '通用技能库',
          path: '~/.agents/skills/code-review/SKILL.md',
        },
        {
          name: 'diagnosing-bugs',
          description: '针对疑难故障与性能回退的系统化定位与复现流程。',
          source: '通用技能库',
          path: '~/.agents/skills/diagnosing-bugs/SKILL.md',
        },
        {
          name: 'tdd',
          description: '测试驱动开发（红-绿-重构）与集成测试验证工作流。',
          source: '通用技能库',
          path: '~/.agents/skills/tdd/SKILL.md',
        },
        {
          name: 'api-contract-review',
          description: '审查接口契约、参数校验、错误码、分页与幂等性边界。',
          source: '用户技能库',
          path: '~/.codex/skills/api-contract-review/SKILL.md',
        },
      ],
      skillsLoaded: false,
      expandedSkillCards: {},
      skillDetailsMap: {},
      skillSearchQuery: '',
      addingSkillOpen: false,
      newSkillName: '',
      newSkillDesc: '',
      newSkillContent: '',
      skillNotice: '',
      // Composer Slash Trigger, Plus Menu, Plan, Goal, Todo, Attachments
      composerAddMenuOpen: false,
      slashMenuOpen: false,
      slashFilter: '',
      planModeActive: false,
      goalBarOpen: false,
      goalEditing: false,
      sessionGoal: '',
      goalDraft: '',
      todoPanelOpen: false,
      attachedItems: [],
      // Conversation Header Utilities, Trajectory & Details Drawer
      conversationSubTab: 'chat',
      trajectoryFilter: 'all',
      trajectoryEventsBySession: new Map(),
      jobsMenuOpen: false,
      workflowMenuOpen: false,
      contextMeterOpen: false,
      detailsDrawerOpen: false,
      selectedDetailItem: null,
      // DSH Permission Preset & Grants state
      permissionPreset: 'workspace-write',
      permissionPopoverOpen: false,
      fullAccessConfirmOpen: false,
      fullAccessAcknowledged: false,
      pendingPresetTarget: null,
      sessionGrants: [],
      sessionAudit: [],
      revokeBusyKey: null,
      // DSH Directory Browser Modal state
      newProjectModalOpen: false,
      browsePath: '/Volumes/ORICO',
      browseParent: '/Volumes',
      browseEntries: [],
      browseSelectedPath: '',
      browseShowHidden: false,
      browseLoading: false,
      browseError: '',
      pathEditing: false,
      newFolderOpen: false,
      newFolderName: '',
    };
  }
  const s = stateRef.current;

  const promptInputRef = useRef(null);
  const forkDialogRef = useRef(null);
  const forkLabelInputRef = useRef(null);
  const forkConfirmInputRef = useRef(null);
  const projectWsInputRef = useRef(null);
  const projectLabelInputRef = useRef(null);
  const modalProjectWsInputRef = useRef(null);
  const modalProjectLabelInputRef = useRef(null);

  const applyTheme = (mode, shouldRender = true) => {
    s.themeMode = mode;
    if (typeof document !== 'undefined' && document.documentElement) {
      let resolved = mode;
      if (
        mode === 'system' &&
        typeof window !== 'undefined' &&
        typeof window.matchMedia === 'function'
      ) {
        resolved = window.matchMedia('(prefers-color-scheme: light)').matches
          ? 'light'
          : 'dark';
      }
      document.documentElement.dataset.theme = resolved;
      document.documentElement.style.colorScheme = resolved;
      if (document.body) {
        if (resolved === 'dark') {
          document.body.setAttribute('data-ds-dark-theme', '');
        } else {
          document.body.removeAttribute('data-ds-dark-theme');
        }
      }
    }
    if (shouldRender) forceRender();
  };

  const currentProjectId = () => s.activeProject ?? s.projects[0]?.id ?? 'default';
  const activeView = () => (s.selected ? s.views.get(s.selected) : undefined);

  const displayLabel = sessionOrSnap => {
    const id = sessionOrSnap?.id;
    if (id && s.sessionTitleOverrides.has(id)) {
      return s.sessionTitleOverrides.get(id);
    }
    const rawLabel = sessionOrSnap?.label || '新会话';
    if (/^(新会话|Session)(\s+\d+)?$/.test(rawLabel)) {
      const msgs =
        sessionOrSnap?.messages ??
        (id ? s.views.get(id)?.snapshot?.messages : undefined);
      const firstUser = array(msgs).find(
        m => m?.role === 'user' && messageText(m).trim(),
      );
      if (firstUser) {
        const derived = shortTitle(messageText(firstUser), 30);
        if (id) s.sessionTitleOverrides.set(id, derived);
        return derived;
      }
    }
    return shortTitle(rawLabel, 80);
  };

  const report = error => {
    s.globalError = error ? errorText(error) : '';
  };

  const saveProjectState = () => {
    s.projectStates.set(currentProjectId(), {
      selected: s.selected,
      sessions: s.sessions,
      views: s.views,
      promptPending: s.promptPending,
      approvalPending: s.approvalPending,
      cachedCapabilities: s.cachedCapabilities,
    });
  };

  const loadProjectState = projectId => {
    const ps = s.projectStates.get(projectId);
    s.selected = ps?.selected ?? null;
    s.sessions = ps?.sessions ?? [];
    s.views = ps?.views ?? new Map();
    s.promptPending = ps?.promptPending ?? new Set();
    s.approvalPending = ps?.approvalPending ?? new Set();
    s.cachedCapabilities = ps?.cachedCapabilities ?? [];
  };

  const rememberView = view => {
    s.views.set(view.id, view);
    while (s.views.size > 8) {
      const evict = [...s.views.keys()].find(
        id =>
          id !== s.selected &&
          !s.promptPending.has(id) &&
          !s.views.get(id).writes.length &&
          !s.views.get(id).activeAttempt &&
          !Object.keys(s.views.get(id).faults).length,
      );
      if (!evict) break;
      s.views.delete(evict);
    }
  };

  const updateSessionSummary = snapshot => {
    const current = s.views.get(snapshot.id)?.snapshot;
    if (current && current.cursor > snapshot.cursor) snapshot = current;
    const index = s.sessions.findIndex(item => item.id === snapshot.id);
    const summary = sessionSummary(snapshot);
    if (index >= 0) s.sessions[index] = summary;
    else s.sessions = [...s.sessions, summary].slice(-200);
    s.listStatus =
      s.sessions.length === 200
        ? '显示最近 200 个会话。'
        : `${s.sessions.length} 个会话`;
  };

  const rpc = async (method, params = {}) => {
    const controller = new AbortController();
    s.controllers.add(controller);
    const requestBody = s.activeProject
      ? { method, params, project: s.activeProject }
      : { method, params };
    let timeout;
    if (method !== 'session.prompt') {
      timeout = setTimer(() => controller.abort(), 15000);
    }
    try {
      const response = await fetchFn('/api/rpc', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(requestBody),
        signal: controller.signal,
        cache: 'no-store',
        credentials: 'omit',
      });
      let payload;
      try {
        payload = await response.json();
      } catch {
        throw new RpcError(
          'invalid_response',
          '网关返回了无效 JSON；当前请求不会自动重发。',
          !readMethods.has(method),
          response.status,
        );
      }
      if (!response.ok || payload?.error) {
        const detail = payload?.error;
        throw new RpcError(
          typeof detail?.code === 'string' ? detail.code : 'http_error',
          detail?.message ?? `请求失败（HTTP ${response.status}）。`,
          typeof detail?.outcome_unknown === 'boolean'
            ? detail.outcome_unknown
            : !readMethods.has(method),
          response.status,
        );
      }
      if (!object(payload) || !Object.hasOwn(payload, 'result')) {
        throw new RpcError(
          'invalid_response',
          '网关响应缺少 result；当前请求不会自动重发。',
          !readMethods.has(method),
        );
      }
      return payload.result;
    } catch (error) {
      if (error.name === 'AbortError') {
        throw new RpcError(
          'transport_unknown',
          '请求连接已中断或超时。页面继续读取事件，消息不会自动重发。',
          !readMethods.has(method),
        );
      }
      if (error instanceof TypeError) {
        throw new RpcError(
          'transport_unknown',
          '无法连接本机网关。页面继续读取事件，消息不会自动重发。',
          !readMethods.has(method),
        );
      }
      throw error;
    } finally {
      clearTimer(timeout);
      s.controllers.delete(controller);
    }
  };

  const rememberError = (id, error) => {
    const view = s.views.get(id);
    if (view) rememberView(noteSessionError(view, error));
    if (id === s.selected) forceRender();
  };

  const schedulePoll = (delay, generation = s.selectionGeneration) => {
    clearTimer(s.timer);
    if (s.selected && generation === s.selectionGeneration) {
      s.timer = setTimer(() => poll(generation), delay);
    }
  };

  const poll = async generation => {
    const id = s.selected;
    const login = s.loginGeneration;
    if (!id || generation !== s.selectionGeneration) return;
    let delay = 1000;
    try {
      let view = s.views.get(id);
      if (!view.snapshot) {
        const requestedAfter = s.views.get(id).cursor;
        const requestedEpoch = s.views.get(id).proofEpoch;
        const snapshot = await rpc('session.get', { session_id: id });
        if (login !== s.loginGeneration || generation !== s.selectionGeneration) return;
        view = applySnapshot(
          s.views.get(id),
          snapshot,
          true,
          requestedAfter,
          requestedEpoch,
        );
        rememberView(view);
      }
      const records = await rpc('session.events', {
        session_id: id,
        after: view.cursor,
        limit: 100,
      });
      if (login !== s.loginGeneration || generation !== s.selectionGeneration) return;
      view = consumeEvents(s.views.get(id), records);
      rememberView(view);
      forceRender();
      const terminalCursor = terminalReadCursor(view);
      if (terminalCursor !== null) {
        const proof = await rpc('session.events', {
          session_id: id,
          after: terminalCursor - 1,
          limit: 1,
        });
        if (login !== s.loginGeneration || generation !== s.selectionGeneration) return;
        rememberView(consumeTerminalRead(s.views.get(id), terminalCursor, proof));
      }
      view = s.views.get(id);
      const requestedAfter = view.cursor;
      const requestedEpoch = view.proofEpoch;
      const snapshot = await rpc('session.get', { session_id: id });
      if (login !== s.loginGeneration || generation !== s.selectionGeneration) return;
      rememberView(
        applySnapshot(
          s.views.get(id),
          snapshot,
          false,
          requestedAfter,
          requestedEpoch,
        ),
      );
      updateSessionSummary(snapshot);
      s.pollFailures = 0;
      s.connectionStatus = '已连接本机';
      s.pollStatus = '已同步';
      forceRender();
      if (records.length === 100) delay = 0;
    } catch (error) {
      if (login !== s.loginGeneration || generation !== s.selectionGeneration) return;
      rememberError(id, error);
      s.pollFailures++;
      delay = Math.min(1000 * 2 ** Math.min(s.pollFailures, 4), 15000);
      s.connectionStatus = '正在重连';
      s.pollStatus = `事件读取失败，${Math.round(delay / 1000)} 秒后续读；未重发消息。${errorText(error)}`;
      forceRender();
    } finally {
      if (login === s.loginGeneration && generation === s.selectionGeneration) {
        schedulePoll(delay, generation);
      }
    }
  };

  const refreshSessions = async () => {
    const generation = ++s.listGeneration;
    const login = s.loginGeneration;
    s.refreshSessionsBusy = true;
    s.listStatus = '正在读取会话…';
    forceRender();
    try {
      const result = await rpc('session.list');
      if (login !== s.loginGeneration || generation !== s.listGeneration) return;
      if (
        !Array.isArray(result) ||
        result.some(item => !object(item) || typeof item.id !== 'string')
      ) {
        throw new Error('Runtime 返回了无效的会话列表。');
      }
      s.sessions = result.slice(-200).map(sessionSummary);
      s.listStatus =
        s.sessions.length === 200
          ? '显示最近 200 个会话。'
          : `${s.sessions.length} 个会话`;
      forceRender();
    } catch (error) {
      if (login !== s.loginGeneration || generation !== s.listGeneration) return;
      s.listStatus = '读取失败，可点击刷新重试。';
      report(error);
      forceRender();
    } finally {
      if (generation === s.listGeneration) {
        s.refreshSessionsBusy = false;
        forceRender();
      }
    }
  };

  const refreshPermissions = async (sessionId = s.selected) => {
    if (!sessionId) {
      s.sessionGrants = [];
      s.sessionAudit = [];
      s.permissionsStatus = '请先选择或新建一个会话以查看授权记录。';
      forceRender();
      return;
    }
    const login = s.loginGeneration;
    s.refreshPermissionsBusy = true;
    s.permissionsStatus = '正在读取会话授权与审计记录…';
    forceRender();
    try {
      const [grantsRes, auditRes] = await Promise.all([
        rpc('session.grants', { session_id: sessionId }).catch(() => []),
        rpc('session.audit', { session_id: sessionId }).catch(() => []),
      ]);
      if (login !== s.loginGeneration) return;
      s.sessionGrants = Array.isArray(grantsRes)
        ? grantsRes
        : Array.isArray(grantsRes?.grants)
          ? grantsRes.grants
          : [];
      s.sessionAudit = Array.isArray(auditRes)
        ? auditRes
        : Array.isArray(auditRes?.records)
          ? auditRes.records
          : Array.isArray(auditRes?.audit)
            ? auditRes.audit
            : [];
      s.permissionsStatus = `当前会话共有 ${s.sessionGrants.length} 项生效授权 · ${s.sessionAudit.length} 条审计记录`;
      forceRender();
    } catch (error) {
      if (login === s.loginGeneration) {
        s.permissionsStatus = errorText(error);
        forceRender();
      }
    } finally {
      s.refreshPermissionsBusy = false;
      forceRender();
    }
  };

  const revokeGrant = async grant => {
    const sessionId = s.selected;
    if (!sessionId || s.revokeBusyKey) return;
    const capability = grant?.capability ?? grant?.input?.capability ?? '';
    const scope = grant?.scope ?? grant?.input?.scope ?? '{}';
    const key = `${capability}:${scope}`;
    const login = s.loginGeneration;
    s.revokeBusyKey = key;
    report(null);
    forceRender();
    try {
      await writeRpc(sessionId, 'approval.revoke', {
        session_id: sessionId,
        capability,
        scope: typeof scope === 'string' ? scope : JSON.stringify(scope),
        ...(grant?.task_id ? { task_id: grant.task_id } : {}),
      });
      if (login !== s.loginGeneration) return;
      await refreshPermissions(sessionId);
      await freshSnapshot(sessionId, login);
    } catch (error) {
      if (login === s.loginGeneration) report(error);
    } finally {
      s.revokeBusyKey = null;
      forceRender();
    }
  };

  const selectPermissionPreset = presetId => {
    if (presetId === 'danger-full-access' && s.permissionPreset !== 'danger-full-access') {
      s.pendingPresetTarget = presetId;
      s.fullAccessAcknowledged = false;
      s.fullAccessConfirmOpen = true;
      s.permissionPopoverOpen = false;
      forceRender();
      return;
    }
    s.permissionPreset = presetId;
    s.permissionPopoverOpen = false;
    forceRender();
  };

  const confirmFullAccess = () => {
    if (!s.fullAccessAcknowledged) return;
    s.permissionPreset = s.pendingPresetTarget || 'danger-full-access';
    s.fullAccessConfirmOpen = false;
    s.fullAccessAcknowledged = false;
    s.pendingPresetTarget = null;
    forceRender();
  };

  const selectSession = async id => {
    if (id === s.selected) return;
    const generation = ++s.selectionGeneration;
    const login = s.loginGeneration;
    clearTimer(s.timer);
    s.selected = id;
    if (!s.views.has(id)) rememberView(createView(id));
    s.pollStatus = '正在读取会话…';
    s.promptText = '';
    if (promptInputRef.current) promptInputRef.current.value = '';
    report(null);
    forceRender();
    try {
      const requestedAfter = s.views.get(id).cursor;
      const requestedEpoch = s.views.get(id).proofEpoch;
      const snapshot = await rpc('session.get', { session_id: id });
      if (login !== s.loginGeneration || generation !== s.selectionGeneration) return;
      const previous = s.views.get(id);
      rememberView(
        applySnapshot(
          previous,
          snapshot,
          !previous.snapshot,
          requestedAfter,
          requestedEpoch,
        ),
      );
      updateSessionSummary(snapshot);
      s.pollFailures = 0;
      forceRender();
      schedulePoll(0, generation);
    } catch (error) {
      if (login !== s.loginGeneration || generation !== s.selectionGeneration) return;
      s.pollStatus = '读取会话失败，稍后重新连接。';
      rememberError(id, error);
      report(error);
      forceRender();
      schedulePoll(2000, generation);
    }
  };

  const connectLocal = async () => {
    saveProjectState();
    const login = ++s.loginGeneration;
    s.selectionGeneration++;
    s.listGeneration++;
    clearTimer(s.timer);
    for (const controller of s.controllers) controller.abort();
    s.controllers.clear();
    s.selected = null;
    s.sessions = [];
    for (const [projectId, ps] of s.projectStates.entries()) {
      const retained = new Map(
        [...ps.views]
          .filter(
            ([, view]) =>
              view.writes.length ||
              view.activeAttempt ||
              Object.keys(view.faults).length,
          )
          .map(([id, view]) => [
            id,
            {
              ...view,
              writes: view.writes.map(write => ({ ...write, phase: 'uncertain' })),
            },
          ]),
      );
      s.projectStates.set(projectId, {
        selected: null,
        sessions: [],
        views: retained,
        promptPending: new Set(),
        approvalPending: new Set(),
        cachedCapabilities: [],
      });
    }
    s.views = s.projectStates.get(currentProjectId())?.views ?? new Map();
    s.promptPending = new Set();
    s.approvalPending = new Set();
    s.forkOpen = false;
    if (forkDialogRef.current?.close) forkDialogRef.current.close();
    s.reconnectBusy = true;
    s.connectionStatus = '正在连接本机…';
    if (login > 1) forceRender();
    try {
      const health = await rpc('runtime.health');
      if (login !== s.loginGeneration) return;
      if (
        health?.status !== 'ok' ||
        health?.protocol_version !== CORE_PROTOCOL_VERSION
      ) {
        throw new Error('本机运行服务的版本不匹配，请同步更新运行服务和网页界面。');
      }
      s.connectionStatus = '已连接本机';
      report(null);
      forceRender();
      try {
        const list = await rpc('projects.list');
        if (login === s.loginGeneration && Array.isArray(list) && list.length) {
          s.projects = list;
        }
      } catch {}
      await refreshSessions();
      if (login === s.loginGeneration && s.sessions.length) {
        await selectSession(s.sessions[0].id);
      }
    } catch (error) {
      if (login !== s.loginGeneration) return;
      s.connectionStatus = '未连接本机';
      report(error);
      forceRender();
    } finally {
      if (login === s.loginGeneration) {
        s.reconnectBusy = false;
        forceRender();
      }
    }
  };

  const writeRpc = async (id, method, params) => {
    const login = s.loginGeneration;
    const key = ++s.writeSequence;
    const view = s.views.get(id);
    if (view) rememberView(beginWrite(view, key, method, params));
    if (id === s.selected) forceRender();
    try {
      const result = await rpc(method, params);
      if (login === s.loginGeneration && s.views.has(id)) {
        rememberView(finishWrite(s.views.get(id), key, null, result));
      }
      return result;
    } catch (error) {
      if (login === s.loginGeneration && s.views.has(id)) {
        rememberView(finishWrite(s.views.get(id), key, error));
      }
      throw error;
    } finally {
      if (login === s.loginGeneration && id === s.selected) forceRender();
    }
  };

  const freshSnapshot = async (id, login) => {
    const requestedAfter = s.views.get(id)?.cursor ?? 0;
    const requestedEpoch = s.views.get(id)?.proofEpoch ?? 0;
    const snapshot = await rpc('session.get', { session_id: id });
    if (login !== s.loginGeneration) return;
    const previous = s.views.get(id);
    if (previous) {
      rememberView(
        applySnapshot(previous, snapshot, false, requestedAfter, requestedEpoch),
      );
    }
    updateSessionSummary(snapshot);
    if (id === s.selected) forceRender();
  };

  const decide = async (id, approvalId, decision) => {
    const key = `${id}/${approvalId}`;
    if (s.approvalPending.has(key)) return;
    const login = s.loginGeneration;
    s.approvalPending.add(key);
    report(null);
    forceRender();
    try {
      await writeRpc(id, 'approval.decide', {
        session_id: id,
        approval_id: approvalId,
        decision,
      });
      if (login !== s.loginGeneration) return;
      await freshSnapshot(id, login);
    } catch (error) {
      if (login === s.loginGeneration) report(error);
    } finally {
      if (login === s.loginGeneration) {
        s.approvalPending.delete(key);
        forceRender();
      }
    }
  };

  const refreshProjects = async () => {
    const login = s.loginGeneration;
    s.refreshProjectsBusy = true;
    s.projectsStatus = '正在读取本机项目…';
    forceRender();
    try {
      const list = await rpc('projects.list');
      if (login !== s.loginGeneration) return;
      if (Array.isArray(list) && list.length) s.projects = list;
      await refreshSessions();
      if (login === s.loginGeneration) {
        saveProjectState();
        const sessionMap = Object.fromEntries(
          [...s.projectStates.entries()].map(([id, st]) => [id, st.sessions]),
        );
        const viewMap = Object.fromEntries(
          [...s.projectStates.entries()].map(([id, st]) => [id, st.views]),
        );
        const summaries = summarizeProjects(s.projects, sessionMap, viewMap);
        const currentSummary =
          summaries.find(item => item.id === currentProjectId()) ?? summaries[0];
        s.projectsStatus = `共 ${summaries.length} 个本机项目空间 · 当前空间：${currentSummary?.label ?? currentProjectId()}`;
        forceRender();
      }
    } catch (error) {
      if (login === s.loginGeneration) {
        s.projectsStatus = errorText(error);
        report(error);
        forceRender();
      }
    } finally {
      s.refreshProjectsBusy = false;
      forceRender();
    }
  };

  const refreshPlugins = async () => {
    const login = s.loginGeneration;
    s.refreshPluginsBusy = true;
    s.pluginsStatus = '正在同步会话绑定与宿主能力…';
    forceRender();
    try {
      await refreshSessions();
      const capabilities = await rpc('capabilities.list');
      if (login !== s.loginGeneration) return;
      if (Array.isArray(capabilities)) {
        s.cachedCapabilities = capabilities;
      }
      const items = summarizePlugins(s.sessions, s.cachedCapabilities, s.views);
      s.pluginsStatus = items.length
        ? `已识别 ${items.length} 个插件绑定或能力组`
        : '暂无已绑定的插件记录';
      forceRender();
    } catch (error) {
      if (login === s.loginGeneration) {
        s.pluginsStatus = errorText(error);
        report(error);
        forceRender();
      }
    } finally {
      s.refreshPluginsBusy = false;
      forceRender();
    }
  };

  const browseDirectory = async (path = s.browsePath, createDir = '') => {
    s.browseLoading = true;
    s.browseError = '';
    forceRender();
    try {
      const res = await rpc('projects.browse', {
        path,
        ...(createDir ? { create_dir: createDir } : {}),
      });
      if (object(res)) {
        s.browsePath = res.path || path;
        s.browseParent = res.parent || '';
        s.browseEntries = Array.isArray(res.entries) ? res.entries : [];
        s.browseSelectedPath = '';
        s.projectWorkspaceInput = s.browsePath;
        if (projectWsInputRef.current) projectWsInputRef.current.value = s.browsePath;
        if (modalProjectWsInputRef.current) modalProjectWsInputRef.current.value = s.browsePath;
      }
    } catch (error) {
      s.browseError = errorText(error);
    } finally {
      s.browseLoading = false;
      forceRender();
    }
  };

  const openNewProjectModal = () => {
    s.newProjectModalOpen = true;
    s.newFolderOpen = false;
    s.newFolderName = '';
    s.browseError = '';
    s.pathEditing = false;
    if (!s.projectWorkspaceInput) {
      s.projectWorkspaceInput = s.browsePath || '/Volumes/ORICO';
    }
    forceRender();
    void browseDirectory(s.projectWorkspaceInput || '/Volumes/ORICO');
  };

  const switchPage = page => {
    s.activePage = page;
    forceRender();
    if (page === 'projects') {
      if (s.projects.length === 1 && s.projects[0]?.address === '本机 IPC') {
        void refreshProjects();
      }
    }
    if (page === 'plugins') {
      if (!s.cachedCapabilities.length) {
        void refreshPlugins();
      }
    }
    if (page === 'permissions') {
      void refreshPermissions();
    }
  };

  const refreshSkillsCatalog = async () => {
    try {
      const res = await rpc('skills.catalog', {});
      if (Array.isArray(res) && res.length) {
        s.skillsCatalog = res;
        s.skillsLoaded = true;
        forceRender();
      }
    } catch {
      // Retain fallback catalog in offline/mock tests
    }
  };

  const loadSkillDetail = async skillName => {
    if (s.skillDetailsMap[skillName]) return;
    try {
      const res = await rpc('skills.catalog', { name: skillName });
      if (Array.isArray(res)) {
        const found = res.find(item => item.name === skillName);
        if (found?.content) {
          s.skillDetailsMap[skillName] = found.content;
          forceRender();
        }
      }
    } catch {
      // Ignore
    }
  };

  const refreshMcpCatalog = async () => {
    try {
      const res = await rpc('mcp.catalog', {});
      if (Array.isArray(res) && res.length) {
        const custom = s.mcpServers.filter(item => !item.builtin);
        s.mcpServers = [...res, ...custom];
        forceRender();
      }
    } catch {
      // Retain default MCP catalog
    }
  };

  const refreshServerAuthAndBinding = async () => {
    try {
      const res = await rpc('server.auth.status', {});
      if (object(res)) {
        s.connectionMode = res.connection_mode || 'local_device';
        s.serverLoggedIn = Boolean(res.logged_in);
        if (typeof res.username === 'string' && res.username) {
          s.serverUsername = res.username;
        }
        s.serverIsDefaultCredentials = Boolean(res.is_default_credentials);
        if (object(res.device_binding)) {
          s.deviceBinding = res.device_binding;
          s.deviceServerUrlDraft =
            res.device_binding.server_url || s.deviceServerUrlDraft;
        }
        if (typeof res.active_device_id === 'string' && res.active_device_id) {
          s.activeDeviceId = res.active_device_id;
        }
        if (Array.isArray(res.devices) && res.devices.length) {
          s.boundDevices = res.devices;
        }
        forceRender();
      }
    } catch {
      // Retain fallback defaults in unit mock tests
    }
  };

  const openAddDeviceModal = () => {
    s.addDeviceModalOpen = true;
    s.newDeviceLabel = '';
    s.newDeviceId = '';
    s.newDeviceServerUrl = '';
    s.newDeviceMode = 'auto';
    s.newDevicePem = '';
    s.addDeviceError = '';
    s.lastCreatedDevice = null;
    forceRender();
    void refreshServerAuthAndBinding();
  };

  const handleAddDeviceSubmit = async e => {
    e?.preventDefault?.();
    if (s.addDeviceBusy) return;
    const rawServer = s.newDeviceServerUrl.trim();
    if (!rawServer) {
      s.addDeviceError = '请输入 Server 地址（如 wss://127.0.0.1:8787）。';
      forceRender();
      return;
    }
    const normalizedServer =
      rawServer.startsWith('ws://') || rawServer.startsWith('wss://')
        ? rawServer
        : `wss://${rawServer}`;
    const label = s.newDeviceLabel.trim();
    s.addDeviceBusy = true;
    s.addDeviceError = '';
    forceRender();
    try {
      const res = await rpc('devices.add', {
        ...(label ? { label } : {}),
        server_url: normalizedServer,
      });
      if (object(res)) {
        if (Array.isArray(res.devices) && res.devices.length) {
          s.boundDevices = res.devices;
        }
        if (typeof res.active_device_id === 'string') {
          s.activeDeviceId = res.active_device_id;
        }
        if (object(res.created)) {
          s.deviceBinding = res.created;
        }
      } else {
        const hostPart = normalizedServer
          .replace(/^wss?:\/\//, '')
          .replace(/\/$/, '');
        const autoLabel = label || `远程设备 (${hostPart})`;
        const fallbackDev = {
          device_id: `device-rsa-0${s.boundDevices.length + 1}`,
          label: autoLabel,
          kind: 'rsa_bound',
          builtin: false,
          active: true,
          bound: true,
          server_url: normalizedServer,
          algorithm: 'RSA-OAEP-SHA256 + RSA-SHA256',
          encrypted_channel_verified: true,
          auto_reconnect: true,
          device_fingerprint: 'SHA256:c4f8a1e2d3b907561234abcd9876ef01',
          server_fingerprint: s.deviceBinding.server_fingerprint,
          device_public_key_pem: s.deviceBinding.device_public_key_pem,
        };
        s.boundDevices = s.boundDevices
          .map(d => ({ ...d, active: false }))
          .concat([fallbackDev]);
        s.activeDeviceId = fallbackDev.device_id;
        s.deviceBinding = fallbackDev;
      }
      s.addDeviceModalOpen = false;
      s.deviceBindingNotice = `已连接 ${normalizedServer} 并自动完成 RSA 密钥绑定，后续自动加密连接。`;
      forceRender();
    } catch (error) {
      s.addDeviceError = errorText(error);
      forceRender();
    } finally {
      s.addDeviceBusy = false;
      forceRender();
    }
  };

  const handleSelectDevice = async deviceId => {
    try {
      const res = await rpc('devices.select', { device_id: deviceId });
      if (object(res) && Array.isArray(res.devices)) {
        s.boundDevices = res.devices;
        s.activeDeviceId = res.active_device_id || deviceId;
        const current = res.devices.find(d => d.device_id === s.activeDeviceId);
        if (current) s.deviceBinding = current;
      } else {
        s.activeDeviceId = deviceId;
        s.boundDevices = s.boundDevices.map(d => ({
          ...d,
          active: d.device_id === deviceId,
        }));
      }
      s.deviceBindingNotice = `已切换至设备“${s.boundDevices.find(d => d.device_id === deviceId)?.label || deviceId}”。`;
      forceRender();
    } catch (error) {
      s.deviceBindingNotice = errorText(error);
      forceRender();
    }
  };

  const handleRemoveDevice = async deviceId => {
    try {
      const res = await rpc('devices.remove', { device_id: deviceId });
      if (object(res) && Array.isArray(res.devices)) {
        s.boundDevices = res.devices;
        s.activeDeviceId = res.active_device_id || s.boundDevices[0]?.device_id;
      } else {
        s.boundDevices = s.boundDevices.filter(d => d.device_id !== deviceId);
        s.activeDeviceId = s.boundDevices[0]?.device_id || 'device-macos-local-01';
      }
      s.deviceBindingNotice = '已解绑并移除该设备。';
      forceRender();
    } catch (error) {
      s.deviceBindingNotice = errorText(error);
      forceRender();
    }
  };

  const switchConnectionMode = async mode => {
    s.serverLoginError = '';
    s.serverAccountNotice = '';
    try {
      const res = await rpc('server.auth.set_mode', { mode });
      if (object(res)) {
        s.connectionMode = res.connection_mode || mode;
        s.serverLoggedIn = Boolean(res.logged_in);
        if (typeof res.username === 'string' && res.username) {
          s.serverUsername = res.username;
        }
        s.serverIsDefaultCredentials = Boolean(res.is_default_credentials);
      } else {
        s.connectionMode = mode;
      }
    } catch {
      s.connectionMode = mode;
    }
    forceRender();
    if (s.connectionMode === 'local_device' || s.serverLoggedIn) {
      await connectLocal();
    }
  };

  const handleServerLogin = async e => {
    e?.preventDefault?.();
    if (s.serverLoginBusy) return;
    s.serverLoginBusy = true;
    s.serverLoginError = '';
    s.serverAccountNotice = '';
    forceRender();
    try {
      const res = await rpc('server.auth.login', {
        username: s.serverLoginUserDraft.trim(),
        password: s.serverLoginPassDraft,
      });
      if (object(res)) {
        s.serverLoggedIn = true;
        s.serverUsername = res.username || s.serverLoginUserDraft.trim();
        s.serverIsDefaultCredentials = Boolean(res.is_default_credentials);
      } else {
        s.serverLoggedIn = true;
      }
      s.serverLoginPassDraft = '';
      s.serverAccountNotice = s.serverIsDefaultCredentials
        ? '已使用初始默认账号登录。为了安全，建议在下方修改登录账号与密码。'
        : '已登录远程 Server。';
      forceRender();
      await connectLocal();
    } catch (error) {
      s.serverLoginError = errorText(error);
      forceRender();
    } finally {
      s.serverLoginBusy = false;
      forceRender();
    }
  };

  const handleServerLogout = async () => {
    try {
      await rpc('server.auth.logout', {});
    } catch {}
    s.serverLoggedIn = false;
    s.serverEditingCredentials = false;
    s.serverAccountNotice = '已退出远程 Server 账号登录。';
    forceRender();
  };

  const handleServerUpdateCredentials = async e => {
    e?.preventDefault?.();
    s.serverAccountNotice = '';
    s.serverLoginError = '';
    if (!s.serverNewUserDraft.trim() || !s.serverNewPassDraft) {
      s.serverLoginError = '新登录账号和新登录密码不能为空。';
      forceRender();
      return;
    }
    if (s.serverNewPassDraft !== s.serverConfirmPassDraft) {
      s.serverLoginError = '两次输入的新密码不一致，请核对。';
      forceRender();
      return;
    }
    try {
      const res = await rpc('server.auth.update_credentials', {
        current_password: s.serverCurrentPassDraft,
        new_username: s.serverNewUserDraft.trim(),
        new_password: s.serverNewPassDraft,
      });
      if (object(res)) {
        s.serverUsername = res.username || s.serverNewUserDraft.trim();
        s.serverIsDefaultCredentials = Boolean(res.is_default_credentials);
      } else {
        s.serverUsername = s.serverNewUserDraft.trim();
        s.serverIsDefaultCredentials = false;
      }
      s.serverEditingCredentials = false;
      s.serverCurrentPassDraft = '';
      s.serverNewPassDraft = '';
      s.serverConfirmPassDraft = '';
      s.serverLoginUserDraft = s.serverUsername;
      s.serverAccountNotice = `已修改登录账号为“${s.serverUsername}”并更新密码，后续登录请使用新账号密码。`;
      forceRender();
    } catch (error) {
      s.serverLoginError = errorText(error);
      forceRender();
    }
  };

  const handleBindDeviceOnce = async () => {
    s.deviceBindingNotice = '';
    try {
      const res = await rpc('device.binding.bind_once', {
        server_url: s.deviceServerUrlDraft.trim() || 'wss://127.0.0.1:8787',
        rebind: false,
      });
      if (object(res)) {
        s.deviceBinding = res;
      }
      s.deviceBindingNotice =
        'Server 与 Device 已完成一次性 RSA 密钥对永久绑定并持久化保存，后续均通过 RSA 自动加密连接，无需再手动处理。';
      forceRender();
    } catch (error) {
      s.deviceBindingNotice = errorText(error);
      forceRender();
    }
  };

  const loadTrajectoryHistory = async (sessionId = s.selected) => {
    if (!sessionId) return;
    try {
      const records = await rpc('session.events', {
        session_id: sessionId,
        after: 0,
        limit: 100,
      });
      if (Array.isArray(records) && records.length) {
        s.trajectoryEventsBySession.set(sessionId, records);
        forceRender();
      }
    } catch {
      // Ignore
    }
  };

  const openSettings = (tab = 'general') => {
    s.settingsTab = tab;
    s.settingsOpen = true;
    forceRender();
    if (tab === 'permissions') {
      void refreshPermissions();
    } else if (tab === 'connection') {
      void refreshServerAuthAndBinding();
    } else if (tab === 'plugins' && !s.cachedCapabilities.length) {
      void refreshPlugins();
    } else if (tab === 'models' && s.projects.length === 1 && s.projects[0]?.address === '本机 IPC') {
      void refreshProjects();
    } else if (tab === 'skills' && !s.skillsLoaded) {
      void refreshSkillsCatalog();
    } else if (tab === 'mcp') {
      void refreshMcpCatalog();
    }
  };

  const selectProject = async projectId => {
    saveProjectState();
    const isDefault =
      s.projects[0]?.id === projectId ||
      (!s.projects.length && projectId === 'default');
    s.activeProject = isDefault ? null : projectId;
    loadProjectState(projectId);
    s.selectionGeneration++;
    clearTimer(s.timer);
    switchPage('conversation');
    await refreshSessions();
    if (s.sessions.length) {
      const target =
        s.selected && s.sessions.some(item => item.id === s.selected)
          ? s.selected
          : s.sessions[0].id;
      s.selected = null;
      await selectSession(target);
    }
  };

  const removeProject = async projectId => {
    const login = s.loginGeneration;
    try {
      const list = await rpc('projects.remove', { id: projectId });
      if (login !== s.loginGeneration) return;
      s.projectStates.delete(projectId);
      if (Array.isArray(list) && list.length) s.projects = list;
      if (currentProjectId() === projectId) {
        await selectProject(s.projects[0]?.id ?? 'default');
      }
      forceRender();
    } catch (error) {
      if (login === s.loginGeneration) {
        s.projectsStatus = errorText(error);
        report(error);
        forceRender();
      }
    }
  };

  const createNewSession = async () => {
    if (s.createBusy) return;
    const login = s.loginGeneration;
    s.createBusy = true;
    report(null);
    forceRender();
    try {
      const label = `新会话 ${s.sessions.length + 1}`;
      const snapshot = await rpc('session.create', { label });
      if (login !== s.loginGeneration) return;
      const view = applySnapshot(createView(snapshot.id), snapshot, true);
      rememberView(view);
      updateSessionSummary(snapshot);
      switchPage('conversation');
      await selectSession(snapshot.id);
      if (promptInputRef.current && !promptInputRef.current.disabled) {
        promptInputRef.current.focus();
      }
    } catch (error) {
      if (login === s.loginGeneration) {
        report(error);
        forceRender();
      }
    } finally {
      s.createBusy = false;
      forceRender();
    }
  };

  const handleAddProject = async (event, andEnter = false) => {
    event?.preventDefault?.();
    const ws = (
      s.browseSelectedPath ||
      modalProjectWsInputRef.current?.value ||
      projectWsInputRef.current?.value ||
      s.projectWorkspaceInput
    ).trim();
    const label = (
      modalProjectLabelInputRef.current?.value ||
      projectLabelInputRef.current?.value ||
      s.projectLabelInput
    ).trim();
    if (!ws || s.addProjectBusy) return;
    const login = s.loginGeneration;
    s.addProjectBusy = true;
    s.browseError = '';
    report(null);
    forceRender();
    try {
      const created = await rpc(
        'projects.create',
        label ? { workspace: ws, label } : { workspace: ws },
      );
      if (login !== s.loginGeneration) return;
      s.projectWorkspaceInput = '';
      s.projectLabelInput = '';
      s.browseSelectedPath = '';
      if (projectWsInputRef.current) projectWsInputRef.current.value = '';
      if (projectLabelInputRef.current) projectLabelInputRef.current.value = '';
      if (modalProjectWsInputRef.current) modalProjectWsInputRef.current.value = '';
      if (modalProjectLabelInputRef.current) modalProjectLabelInputRef.current.value = '';
      s.newProjectModalOpen = false;
      await refreshProjects();
      if (andEnter && created?.id) {
        await selectProject(created.id);
        if (!s.sessions.length) {
          await createNewSession();
        }
      }
    } catch (error) {
      if (login === s.loginGeneration) {
        s.projectsStatus = errorText(error);
        s.browseError = errorText(error);
        report(error);
        forceRender();
      }
    } finally {
      s.addProjectBusy = false;
      forceRender();
    }
  };

  const handleSendPrompt = async event => {
    event?.preventDefault?.();
    const rawText = promptInputRef.current?.value || s.promptText;
    const text = rawText.trim();
    const view = activeView();
    const snapshot = view?.snapshot;
    const pending = s.promptPending.has(s.selected);
    const busy = pending || sessionRunning(snapshot);
    const blocked = sessionBlocked(snapshot, view);
    const promptDisabled = !snapshot || Boolean(blocked) || busy;
    if (promptDisabled || !text || !s.selected) return;
    const id = s.selected;
    const currentSnap = s.views.get(id)?.snapshot;
    if (
      currentSnap &&
      /^新会话(\s+\d+)?$/.test(currentSnap.label || '') &&
      !s.sessionTitleOverrides.has(id)
    ) {
      s.sessionTitleOverrides.set(id, shortTitle(text, 30));
    }
    const login = s.loginGeneration;
    const requestedAfter = s.views.get(id)?.cursor ?? 0;
    const requestedEpoch = s.views.get(id)?.proofEpoch ?? 0;
    s.promptPending.add(id);
    report(null);
    s.promptText = '';
    if (promptInputRef.current) promptInputRef.current.value = '';
    forceRender();
    try {
      const nextSnap = await writeRpc(id, 'session.prompt', {
        session_id: id,
        text,
      });
      if (login !== s.loginGeneration) return;
      const v = s.views.get(id);
      if (v) {
        rememberView({
          ...applySnapshot(v, nextSnap, false, requestedAfter, requestedEpoch),
          liveText: '',
        });
      }
      updateSessionSummary(nextSnap);
      if (id === s.selected) forceRender();
    } catch (error) {
      if (login === s.loginGeneration) {
        report(error);
        forceRender();
      }
    } finally {
      if (login === s.loginGeneration) {
        s.promptPending.delete(id);
        if (id === s.selected) forceRender();
      }
    }
  };

  const handleCancel = async () => {
    const view = activeView();
    const snapshot = view?.snapshot;
    const pending = s.promptPending.has(s.selected);
    const busy = pending || sessionRunning(snapshot);
    const cancelDisabled =
      !snapshot ||
      (!busy &&
        !view.writes.length &&
        !view.activeAttempt &&
        !array(snapshot.pending_approvals).length) ||
      s.cancelPending;
    if (cancelDisabled || !s.selected) return;
    const id = s.selected;
    const login = s.loginGeneration;
    s.cancelPending = true;
    report(null);
    forceRender();
    try {
      const result = await writeRpc(id, 'session.cancel', { session_id: id });
      if (login !== s.loginGeneration) return;
      s.pollStatus = result?.cancelled
        ? '已请求取消；等待 Runtime 记录实际终态。'
        : 'Runtime 当前没有可取消的执行；继续读取真实状态。';
      await freshSnapshot(id, login);
    } catch (error) {
      if (login === s.loginGeneration) report(error);
    } finally {
      s.cancelPending = false;
      forceRender();
    }
  };

  const openForkDialog = () => {
    if (!canFork(activeView())) return;
    const defaultLabel = `${activeView()?.snapshot?.label ?? '会话'} · 普通聊天`.slice(
      0,
      160,
    );
    s.forkLabel = defaultLabel;
    s.forkConfirm = false;
    s.forkError = '';
    s.forkOpen = true;
    if (forkLabelInputRef.current) forkLabelInputRef.current.value = defaultLabel;
    if (forkConfirmInputRef.current) forkConfirmInputRef.current.checked = false;
    forceRender();
    if (forkDialogRef.current?.showModal) forkDialogRef.current.showModal();
  };

  const closeForkDialog = () => {
    s.forkOpen = false;
    if (forkDialogRef.current?.close) forkDialogRef.current.close();
    forceRender();
  };

  const handleForkSubmit = async event => {
    event.preventDefault();
    const view = activeView();
    const label = (forkLabelInputRef.current?.value ?? s.forkLabel).trim();
    const confirmed = Boolean(
      forkConfirmInputRef.current?.checked ?? s.forkConfirm,
    );
    if (!s.selected || !canFork(view) || !confirmed || !label) {
      s.forkError = '请填写新会话名称，并勾选确认。';
      forceRender();
      return;
    }
    const sourceId = s.selected;
    const login = s.loginGeneration;
    s.forkBusy = true;
    s.forkError = '';
    forceRender();
    try {
      const snapshot = await writeRpc(sourceId, 'session.fork', {
        session_id: sourceId,
        label: label.slice(0, 160),
        confirm: true,
        mode: 'chat',
      });
      if (login !== s.loginGeneration) return;
      const forked = applySnapshot(createView(snapshot.id), snapshot, true);
      rememberView(forked);
      updateSessionSummary(snapshot);
      closeForkDialog();
      await selectSession(snapshot.id);
    } catch (error) {
      if (login === s.loginGeneration) {
        s.forkError = errorText(error);
        forceRender();
      }
    } finally {
      s.forkBusy = false;
      forceRender();
    }
  };

  useEffect(() => {
    applyTheme(s.themeMode, false);
    const p = trackAsync(connectLocal());
    if (env?.onInitialConnect) env.onInitialConnect(p);

    const onVisibility = () => {
      if (!document.hidden && s.selected) schedulePoll(0);
    };
    const onOnline = () => {
      if (s.selected) schedulePoll(0);
    };
    const onPageHide = () => {
      clearTimer(s.timer);
      for (const controller of s.controllers) controller.abort();
    };
    const onKeyDown = e => {
      if (e.key === 'Escape') {
        if (s.settingsOpen) {
          s.settingsOpen = false;
          forceRender();
        } else if (s.newProjectModalOpen) {
          s.newProjectModalOpen = false;
          forceRender();
        } else if (s.permissionPopoverOpen) {
          s.permissionPopoverOpen = false;
          forceRender();
        }
      }
    };
    document.addEventListener('visibilitychange', onVisibility);
    window.addEventListener('online', onOnline);
    window.addEventListener('pagehide', onPageHide);
    document.addEventListener('keydown', onKeyDown);
    return () => {
      clearTimer(s.timer);
      for (const controller of s.controllers) controller.abort();
      document.removeEventListener('visibilitychange', onVisibility);
      window.removeEventListener('online', onOnline);
      window.removeEventListener('pagehide', onPageHide);
      document.removeEventListener('keydown', onKeyDown);
    };
  }, []);

  saveProjectState();
  const view = activeView();
  const snapshot = view?.snapshot;
  const attention = Boolean(
    snapshot &&
      (unresolved(snapshot) ||
        view?.faults.reconciliation ||
        frozen(snapshot, view) ||
        view?.writes.some(write => write.phase === 'uncertain') ||
        !['idle', 'running', 'awaiting_approval'].includes(stateName(snapshot))),
  );
  const blockedText = attention ? sessionBlocked(snapshot, view) : '';
  const showForkBtn = Boolean(snapshot && frozen(snapshot, view));
  const forkEnabled = canFork(view);

  const pending = s.promptPending.has(s.selected);
  const busy = pending || sessionRunning(snapshot);
  const blocked = sessionBlocked(snapshot, view);
  const decisionBusy = [...s.approvalPending].some(key =>
    key.startsWith(`${s.selected}/`),
  );
  const promptDisabled = !snapshot || Boolean(blocked) || busy;
  const currentPromptText = promptInputRef.current?.value ?? s.promptText;
  const sendDisabled = promptDisabled || !currentPromptText.trim();
  const cancelDisabled =
    !snapshot ||
    (!busy &&
      !view?.writes.length &&
      !view?.activeAttempt &&
      !array(snapshot.pending_approvals).length) ||
    s.cancelPending;

  const promptStatus = !snapshot
    ? ''
    : view?.writes.some(write => write.phase === 'uncertain')
      ? '连接结果待确认'
      : unresolved(snapshot) || view?.faults.reconciliation
        ? '待宿主核对副作用'
        : frozen(snapshot, view)
          ? '会话已冻结'
          : array(snapshot?.pending_approvals).length
            ? '等待上方授权确认'
            : busy
              ? '正在处理…'
              : decisionBusy
                ? '正在记录审批…'
                : '';

  const approvals = array(snapshot?.pending_approvals);
  const approvalButtonsDisabled = Boolean(
    snapshot &&
      (unresolved(snapshot) ||
        frozen(snapshot, view) ||
        view?.faults.reconciliation ||
        view?.writes.some(
          write => write.phase === 'uncertain' || write.method === 'approval.decide',
        )),
  );

  const messages = array(view?.snapshot?.messages).filter(
    message => message.role !== 'system' && messageText(message).trim(),
  );
  const expandedSet = new Set(view?.expandedRecords ?? []);
  const toggleRecordExpand = (recordKey, isOpen) => {
    if (!view) return;
    const current = s.views.get(view.id);
    if (!current) return;
    const keys = new Set(current.expandedRecords ?? []);
    if (isOpen) keys.add(recordKey);
    else keys.delete(recordKey);
    rememberView({ ...current, expandedRecords: [...keys].slice(-MAX_MESSAGES) });
    forceRender();
  };

  const totalMessages = view?.totalMessages ?? 0;
  const historyNote =
    totalMessages > MAX_MESSAGES
      ? '显示最近的对话，完整记录保存在本机。'
      : messages.length
        ? `${messages.length} 条消息`
        : '';
  const events = array(view?.events);

  const sessionMap = Object.fromEntries(
    [...s.projectStates.entries()].map(([id, st]) => [id, st.sessions]),
  );
  const viewMap = Object.fromEntries(
    [...s.projectStates.entries()].map(([id, st]) => [id, st.views]),
  );
  const projectSummaries = summarizeProjects(s.projects, sessionMap, viewMap);
  const currentProjectSummary =
    projectSummaries.find(item => item.id === currentProjectId()) ??
    projectSummaries[0];
  const projectsStatusText =
    s.projectsStatus ||
    `共 ${projectSummaries.length} 个本机项目空间 · 当前空间：${currentProjectSummary?.label ?? currentProjectId()}`;

  const pluginItems = summarizePlugins(s.sessions, s.cachedCapabilities, s.views);
  const pluginsStatusText =
    s.pluginsStatus ||
    (pluginItems.length
      ? `已识别 ${pluginItems.length} 个插件绑定或能力组`
      : '暂无已绑定的插件记录');

  const currentPresetObj =
    PERMISSION_PRESETS.find(p => p.id === s.permissionPreset) ??
    PERMISSION_PRESETS[0];

  const currentAgentPresetObj =
    AGENT_PRESETS.find(p => p.id === s.selectedAgentPreset) ?? AGENT_PRESETS[0];

  const filteredSessions = s.sessionSearchQuery.trim()
    ? s.sessions.filter(sess =>
        displayLabel(sess)
          .toLowerCase()
          .includes(s.sessionSearchQuery.trim().toLowerCase()),
      )
    : s.sessions;

  const visibleBrowseEntries = s.browseShowHidden
    ? s.browseEntries
    : s.browseEntries.filter(e => !e.hidden);

  const browseCrumbs = (() => {
    const raw = s.browsePath || '/';
    const segments = raw.split('/').filter(Boolean);
    const list = [{ name: '/', path: '/' }];
    let acc = '';
    for (const seg of segments) {
      acc += `/${seg}`;
      list.push({ name: seg, path: acc });
    }
    return list;
  })();

  const isHero =
    s.activePage === 'conversation' &&
    !messages.length &&
    !approvals.length &&
    !attention &&
    !view?.liveText;

  const wide = !s.sidebarCollapsed;

  return (
    <div
      id="workspace"
      className="pI_x6G_frame"
      style={{
        gridTemplateColumns: wide ? '264px minmax(0, 1fr)' : '56px minmax(0, 1fr)',
      }}
    >
      <a className="skip" href="#conversation">
        跳到会话内容
      </a>

      {/* Accessible navigation hooks for programmatic view switching */}
      <nav className="visually-hidden" aria-label="界面视图">
        <button
          ref={bind({ click: () => switchPage('conversation') })}
          id="nav-conversation"
          type="button"
          aria-current={String(s.activePage === 'conversation')}
          onClick={() => switchPage('conversation')}
        >
          对话
        </button>
        <button
          ref={bind({ click: () => switchPage('projects') })}
          id="nav-projects"
          type="button"
          aria-current={String(s.activePage === 'projects')}
          onClick={() => switchPage('projects')}
        >
          项目
        </button>
        <button
          ref={bind({ click: () => switchPage('permissions') })}
          id="nav-permissions"
          type="button"
          aria-current={String(s.activePage === 'permissions')}
          onClick={() => switchPage('permissions')}
        >
          权限
        </button>
        <button
          ref={bind({ click: () => switchPage('plugins') })}
          id="nav-plugins"
          type="button"
          aria-current={String(s.activePage === 'plugins')}
          onClick={() => switchPage('plugins')}
        >
          插件
        </button>
      </nav>

      {/* Official DSH Sidebar (@deepseek-ai/dsh-client-ui-sidebar + ui-workspace) */}
      <aside
        className={clsx('pI_x6G_sidebarCol hHd-Xa_root', !wide && 'hHd-Xa_collapsed')}
        aria-labelledby="sessions-heading"
      >
        <div className="hHd-Xa_logoRow">
          {wide ? (
            <button
              type="button"
              className="hHd-Xa_brand hHd-Xa_wide"
              aria-label="新建会话"
              onClick={() => trackAsync(createNewSession())}
            >
              <span className="hHd-Xa_brandIdentity" aria-hidden="true">
                <span className="hHd-Xa_brandMark">
                  <FishLogo size={24} />
                </span>
                <span className="hHd-Xa_brandName">
                  <BrandWordmark size={24} includeMark={false} />
                </span>
              </span>
            </button>
          ) : null}
          <Tooltip label={wide ? '收起侧边栏' : '打开侧边栏'} delayMs={400}>
            <button
              type="button"
              className="hHd-Xa_iconButton hHd-Xa_toggle"
              aria-label={wide ? '收起侧边栏' : '打开侧边栏'}
              onClick={() => {
                s.sidebarCollapsed = !s.sidebarCollapsed;
                forceRender();
              }}
            >
              {!wide ? (
                <span className="hHd-Xa_railMark" aria-hidden="true">
                  <FishLogo size={24} />
                </span>
              ) : null}
              <IconPanelLeftOutline16
                className="hHd-Xa_panelIcon"
                size={wide ? 16 : 18}
              />
            </button>
          </Tooltip>
        </div>

        <Tooltip label="新建会话" delayMs={400} disabled={wide}>
          <button
            ref={bind({ click: () => trackAsync(createNewSession()) })}
            id="create-button"
            type="button"
            className="hHd-Xa_newSession"
            aria-label="新建会话"
            disabled={s.createBusy}
            onClick={() => trackAsync(createNewSession())}
          >
            <IconNewChatOutline16 size={wide ? 14 : 18} />
            {wide ? (
              <span className="hHd-Xa_newSessionLabel hHd-Xa_wide">
                {s.createBusy ? '创建中…' : '新会话'}
              </span>
            ) : null}
          </button>
        </Tooltip>

        <div className="hHd-Xa_regionArea">
          <div className={clsx('qDHVXG_root', !wide && 'qDHVXG_rail')}>
            <div className="qDHVXG_sectionHeader">
              {wide ? (
                <span
                  id="sessions-heading"
                  className={clsx(
                    'qDHVXG_sectionLabel qDHVXG_wide',
                    (s.searchExpanded || s.sessionSearchQuery.trim()) &&
                      'qDHVXG_sectionLabelHidden',
                  )}
                >
                  工作区
                </span>
              ) : (
                <span id="sessions-heading" className="visually-hidden">
                  工作区
                </span>
              )}

              {wide ? (
                <div
                  className={clsx(
                    'qDHVXG_searchSlot',
                    (s.searchExpanded || s.sessionSearchQuery.trim()) &&
                      'qDHVXG_searchSlotExpanded',
                  )}
                >
                  <div
                    className={clsx(
                      'qDHVXG_search',
                      (s.searchExpanded || s.sessionSearchQuery.trim()) &&
                        'qDHVXG_searchExpanded',
                    )}
                  >
                    <button
                      type="button"
                      className="qDHVXG_searchButton"
                      aria-label="搜索会话"
                      onClick={() => {
                        s.searchExpanded = !s.searchExpanded;
                        forceRender();
                      }}
                    >
                      <IconSearchOutline16 size={16} />
                    </button>
                    <input
                      type="text"
                      className="qDHVXG_searchInput"
                      placeholder="搜索会话…"
                      aria-label="搜索会话"
                      value={s.sessionSearchQuery}
                      tabIndex={
                        s.searchExpanded || s.sessionSearchQuery.trim() ? 0 : -1
                      }
                      onChange={e => {
                        s.sessionSearchQuery = e.target.value;
                        forceRender();
                      }}
                      onKeyDown={e => {
                        if (e.key === 'Escape') {
                          s.sessionSearchQuery = '';
                          s.searchExpanded = false;
                          forceRender();
                        }
                      }}
                    />
                    {s.searchExpanded || s.sessionSearchQuery.trim() ? (
                      <button
                        type="button"
                        className="qDHVXG_clearButton"
                        aria-label="清除搜索"
                        onClick={e => {
                          e.stopPropagation();
                          s.sessionSearchQuery = '';
                          s.searchExpanded = false;
                          forceRender();
                        }}
                      >
                        <IconCloseFill14 />
                      </button>
                    ) : null}
                  </div>
                </div>
              ) : null}

              <div
                className={clsx(
                  'qDHVXG_headerActions',
                  wide &&
                    (s.searchExpanded || s.sessionSearchQuery.trim()) &&
                    'qDHVXG_headerActionsHidden',
                )}
              >
                {wide ? (
                  <Tooltip label="刷新会话" side="bottom" delayMs={400}>
                    <button
                      ref={bind({ click: () => trackAsync(refreshSessions()) })}
                      id="refresh-sessions"
                      type="button"
                      className="qDHVXG_iconButton qDHVXG_wide"
                      aria-label="刷新会话"
                      disabled={s.refreshSessionsBusy}
                      onClick={() => trackAsync(refreshSessions())}
                    >
                      <IconRefreshOutline16 size={16} />
                    </button>
                  </Tooltip>
                ) : null}
                <Tooltip label="添加工作区" side="bottom" delayMs={400}>
                  <button
                    id="open-new-project-btn"
                    type="button"
                    className="qDHVXG_iconButton"
                    aria-label="添加工作区"
                    onClick={() => openNewProjectModal()}
                  >
                    <IconProjectAddOutline16 size={wide ? 16 : 18} />
                  </button>
                </Tooltip>
              </div>
            </div>

            <span id="list-status" className="visually-hidden" role="status">
              {s.listStatus}
            </span>

            <div className="qDHVXG_listArea">
              {wide ? (
                <div className="qDHVXG_treeBody qDHVXG_wide">
                  <div
                    id="session-list"
                    className="qDHVXG_list"
                    role="tree"
                    aria-label="会话"
                  >
                    {projectSummaries.map(proj => {
                      const isCurrentProj = proj.id === currentProjectId();
                      const isExpanded =
                        s.expandedGroups[proj.id] !== undefined
                          ? s.expandedGroups[proj.id]
                          : true;
                      const projSessions = isCurrentProj
                        ? filteredSessions
                        : array(s.projectStates.get(proj.id)?.sessions);

                      return (
                        <div key={proj.id} className="qDHVXG_groupSection">
                          <div
                            className="YDXeBa_projectRow"
                            role="treeitem"
                            aria-expanded={isExpanded}
                            onClick={() => {
                              if (!isCurrentProj) {
                                void selectProject(proj.id);
                                s.expandedGroups[proj.id] = true;
                              } else {
                                s.expandedGroups[proj.id] = !isExpanded;
                              }
                              forceRender();
                            }}
                          >
                            <span
                              className={clsx(
                                'YDXeBa_slot YDXeBa_folder',
                                isExpanded && isCurrentProj && 'YDXeBa_folderActive',
                              )}
                            >
                              {isExpanded ? (
                                <IconFolderOpen16 />
                              ) : (
                                <IconFolderClose16 />
                              )}
                            </span>
                            <span className="YDXeBa_slot YDXeBa_chevron">
                              <IconTriangleRightFill14
                                className={clsx(
                                  'YDXeBa_arrow',
                                  isExpanded && 'YDXeBa_arrowOpen',
                                )}
                              />
                            </span>
                            <span className="YDXeBa_projectText">
                              <span className="YDXeBa_title">{proj.label}</span>
                            </span>
                            <span className="YDXeBa_rowActions">
                              <button
                                type="button"
                                className="YDXeBa_iconButton"
                                aria-label={`在“${proj.label}”中新建会话`}
                                title={`在“${proj.label}”中新建会话`}
                                onClick={e => {
                                  e.stopPropagation();
                                  s.expandedGroups[proj.id] = true;
                                  trackAsync(
                                    (async () => {
                                      if (!isCurrentProj) {
                                        await selectProject(proj.id);
                                      }
                                      await createNewSession();
                                    })(),
                                  );
                                }}
                              >
                                <IconPlusOutline16 />
                              </button>
                            </span>
                          </div>

                          {isExpanded ? (
                            !projSessions.length && isCurrentProj ? (
                              <div className="qDHVXG_empty">
                                {s.sessionSearchQuery.trim()
                                  ? '无匹配会话'
                                  : '暂无会话'}
                              </div>
                            ) : (
                              projSessions.map(session => {
                                const isSelected =
                                  isCurrentProj && session.id === s.selected;
                                const sessView = s.views.get(session.id);
                                const dotState = stateDotKind(session, sessView);
                                const showDot =
                                  dotState !== 'done' || isSelected;
                                return (
                                  <button
                                    key={session.id}
                                    type="button"
                                    role="treeitem"
                                    aria-selected={isSelected}
                                    aria-current={String(isSelected)}
                                    className={clsx(
                                      'YDXeBa_sessionRow',
                                      isSelected && 'YDXeBa_selected',
                                    )}
                                    ref={bind({
                                      click: () =>
                                        trackAsync(
                                          (async () => {
                                            if (!isCurrentProj) {
                                              await selectProject(proj.id);
                                            }
                                            await selectSession(session.id);
                                          })(),
                                        ),
                                    })}
                                    onClick={() =>
                                      trackAsync(
                                        (async () => {
                                          if (!isCurrentProj) {
                                            await selectProject(proj.id);
                                          }
                                          await selectSession(session.id);
                                        })(),
                                      )
                                    }
                                  >
                                    <span className="YDXeBa_slot">
                                      {showDot ? (
                                        <StateDot state={dotState} />
                                      ) : null}
                                    </span>
                                    <span className="YDXeBa_title">
                                      {displayLabel(session)}
                                      <span className="visually-hidden">
                                        {` ${String(session.id ?? '').slice(0, 20)}`}
                                      </span>
                                    </span>
                                    <span className="YDXeBa_time">
                                      {stateLabel(session, sessView)}
                                    </span>
                                  </button>
                                );
                              })
                            )
                          ) : null}
                        </div>
                      );
                    })}
                  </div>
                </div>
              ) : null}
            </div>
          </div>
        </div>

        <div className="hHd-Xa_footArea">
          <div
            className="hHd-Xa_settingsArea"
            style={{ display: 'flex', alignItems: 'center', gap: 4 }}
          >
            <button
              id="open-settings-button"
              type="button"
              className={clsx('VOzbGW_trigger', !wide && 'VOzbGW_rail')}
              style={{ flex: 1 }}
              aria-haspopup="dialog"
              aria-expanded={s.settingsOpen}
              onClick={() => openSettings('general')}
            >
              {wide ? (
                <IconSettingsOutline16 size={16} />
              ) : (
                <IconSettingsOutline14 size={18} />
              )}
              {wide ? <span className="UQsH_q_triggerLabel">设置</span> : null}
            </button>
            {wide ? (
              <Tooltip
                label="添加或管理绑定设备（RSA 一次性加密绑定）"
                side="top"
                delayMs={400}
              >
                <button
                  id="open-add-device-btn"
                  type="button"
                  className="hHd-Xa_iconButton"
                  aria-label="添加设备"
                  onClick={() => openAddDeviceModal()}
                >
                  <IconPlusOutline16 size={16} />
                </button>
              </Tooltip>
            ) : null}
            <Tooltip
              label={`${s.connectionStatus} · 点击重连`}
              side="top"
              delayMs={400}
            >
              <button
                ref={bind({ click: () => trackAsync(connectLocal()) })}
                id="reconnect"
                type="button"
                className="hHd-Xa_iconButton"
                aria-label="重连本机 Runtime"
                disabled={s.reconnectBusy}
                onClick={() => trackAsync(connectLocal())}
              >
                <StateDot
                  state={
                    s.connectionStatus === '已连接本机' ? 'done' : 'warning'
                  }
                />
                <span
                  id="connection-status"
                  className="visually-hidden"
                  role="status"
                >
                  {s.connectionStatus}
                </span>
              </button>
            </Tooltip>
          </div>
        </div>
      </aside>

      {/* Official DSH Center Conversation Column (@deepseek-ai/dsh-client-ui-conversation) */}
      <main id="conversation" className="pI_x6G_centerCol" tabIndex={-1}>
        <div
          id="conversation-view"
          className="wSkVaW_root"
          data-phase={isHero ? 'hero' : 'active'}
          hidden={s.activePage !== 'conversation'}
        >
          <header
            className={clsx('wSkVaW_header', isHero && 'wSkVaW_headerHidden')}
            aria-hidden={isHero || undefined}
          >
            <div className="wSkVaW_titleRow">
              <div className="wSkVaW_titleCluster">
                <nav className="wSkVaW_crumbs" aria-label="会话层级">
                  <span className="wSkVaW_crumbSeg">
                    <button
                      type="button"
                      className="wSkVaW_crumb"
                      onClick={() => openNewProjectModal()}
                    >
                      {currentProjectSummary?.label ?? 'CAYW'}
                    </button>
                  </span>
                  <span className="wSkVaW_crumbSeg">
                    <span className="wSkVaW_crumbSep">/</span>
                    <span
                      id="session-title"
                      className="wSkVaW_crumb wSkVaW_crumbCurrent"
                    >
                      {snapshot
                        ? displayLabel(snapshot)
                        : s.selected
                          ? '正在读取会话…'
                          : '新会话'}
                    </span>
                  </span>
                </nav>
                <div className="wSkVaW_headerActions">
                  <span
                    className="pXSMma_previewBadge"
                    style={{ marginTop: 0, marginLeft: 0 }}
                  >
                    {currentAgentPresetObj.name}
                  </span>

                  {/* Official DSH JobListAction (QsffPG_*) */}
                  <div className="QsffPG_root">
                    <button
                      type="button"
                      className="QsffPG_trigger"
                      onClick={() => {
                        s.jobsMenuOpen = !s.jobsMenuOpen;
                        s.workflowMenuOpen = false;
                        s.contextMeterOpen = false;
                        forceRender();
                      }}
                    >
                      <StateDot
                        state={busy ? 'running' : 'done'}
                        className="QsffPG_triggerDot"
                      />
                      <span className="QsffPG_count">
                        {`执行记录 (${messages.filter(m => m.role === 'toolResult').length})`}
                      </span>
                      <IconChevronDownOutline14
                        size={12}
                        className={clsx(s.jobsMenuOpen && 'QsffPG_triggerOpen')}
                      />
                    </button>
                    {s.jobsMenuOpen ? (
                      <ul className="QsffPG_menu" role="menu">
                        {messages.filter(m => m.role === 'toolResult').length ===
                        0 ? (
                          <li className="QsffPG_row QsffPG_rowSettled">
                            当前会话暂无代码或工具执行记录
                          </li>
                        ) : (
                          messages
                            .filter(m => m.role === 'toolResult')
                            .map((m, idx) => {
                              const pres = messagePresentation(m);
                              return (
                                <li
                                  key={idx}
                                  className="QsffPG_row"
                                  style={{ cursor: 'pointer' }}
                                  onClick={() => {
                                    s.jobsMenuOpen = false;
                                    s.selectedDetailItem = {
                                      title: pres.title || '执行记录',
                                      body: recordText(m),
                                    };
                                    s.detailsDrawerOpen = true;
                                    forceRender();
                                  }}
                                >
                                  <StateDot
                                    state={
                                      pres.status === 'failed'
                                        ? 'error'
                                        : 'done'
                                    }
                                    className="QsffPG_rowDot"
                                  />
                                  <span className="QsffPG_kind">代码单元</span>
                                  <span className="QsffPG_label">
                                    {pres.title || '执行完成'}
                                  </span>
                                  <span className="QsffPG_status">查看详情</span>
                                </li>
                              );
                            })
                        )}
                      </ul>
                    ) : null}
                  </div>

                  {/* Official DSH SubagentCatalogAction / Workflow & Fork Menu (h8S2Va_*) */}
                  <div className="h8S2Va_root">
                    <button
                      type="button"
                      className="h8S2Va_trigger"
                      onClick={() => {
                        s.workflowMenuOpen = !s.workflowMenuOpen;
                        s.jobsMenuOpen = false;
                        s.contextMeterOpen = false;
                        forceRender();
                      }}
                    >
                      <span className="h8S2Va_count">工作流与分支</span>
                      <IconChevronDownOutline14
                        size={12}
                        className={clsx(
                          s.workflowMenuOpen && 'h8S2Va_triggerOpen',
                        )}
                      />
                    </button>
                    {s.workflowMenuOpen ? (
                      <div className="h8S2Va_menu" role="menu">
                        <div
                          className="h8S2Va_row"
                          onClick={() => {
                            s.workflowMenuOpen = false;
                            s.goalBarOpen = true;
                            s.goalEditing = true;
                            s.goalDraft = s.sessionGoal;
                            forceRender();
                          }}
                        >
                          <div className="h8S2Va_clickarea">
                            <StateDot state="done" />
                            <div className="h8S2Va_content">
                              <span className="h8S2Va_label">
                                设定会话任务目标
                              </span>
                              <span
                                className="h8S2Va_summary"
                                style={{
                                  color: 'var(--dsw-alias-label-tertiary)',
                                  fontSize: 12,
                                }}
                              >
                                {s.sessionGoal || '为当前会话绑定明确目标'}
                              </span>
                            </div>
                          </div>
                        </div>
                        <div
                          className="h8S2Va_row"
                          onClick={() => {
                            s.workflowMenuOpen = false;
                            openForkDialog();
                          }}
                        >
                          <div className="h8S2Va_clickarea">
                            <StateDot state="done" />
                            <div className="h8S2Va_content">
                              <span className="h8S2Va_label">
                                从当前会话创建分支
                              </span>
                              <span
                                className="h8S2Va_summary"
                                style={{
                                  color: 'var(--dsw-alias-label-tertiary)',
                                  fontSize: 12,
                                }}
                              >
                                复制已核对历史并开启独立新会话
                              </span>
                            </div>
                          </div>
                        </div>
                        <div
                          className="h8S2Va_row"
                          onClick={() => {
                            s.workflowMenuOpen = false;
                            openSettings('mcp');
                          }}
                        >
                          <div className="h8S2Va_clickarea">
                            <StateDot state="done" />
                            <div className="h8S2Va_content">
                              <span className="h8S2Va_label">
                                外部工具与技能包配置
                              </span>
                              <span
                                className="h8S2Va_summary"
                                style={{
                                  color: 'var(--dsw-alias-label-tertiary)',
                                  fontSize: 12,
                                }}
                              >
                                {`已连接 ${s.mcpServers.filter(m => m.enabled).length} 个工具服务 · ${s.skillsCatalog.length} 项技能`}
                              </span>
                            </div>
                          </div>
                        </div>
                      </div>
                    ) : null}
                  </div>
                </div>
              </div>
              <div className="wSkVaW_headerUtilities">
                <span id="history-note" className="visually-hidden">
                  {historyNote}
                </span>
                <span
                  id="poll-status"
                  className="visually-hidden"
                  role="status"
                >
                  {s.pollStatus}
                </span>

                {/* Official DSH ContextMeter (JObwrW_*) */}
                <div className="JObwrW_root">
                  <button
                    type="button"
                    className="JObwrW_trigger"
                    aria-label="上下文与事件用量"
                    title="查看上下文与事件游标统计"
                    onClick={() => {
                      s.contextMeterOpen = !s.contextMeterOpen;
                      s.jobsMenuOpen = false;
                      s.workflowMenuOpen = false;
                      forceRender();
                    }}
                  >
                    <svg width="18" height="18" viewBox="0 0 18 18">
                      <circle
                        cx="9"
                        cy="9"
                        r="7"
                        className="JObwrW_track"
                      />
                      <circle
                        cx="9"
                        cy="9"
                        r="7"
                        className="JObwrW_fill"
                        strokeDasharray="44"
                        strokeDashoffset={Math.max(
                          8,
                          44 - Math.min(36, messages.length * 4),
                        )}
                        transform="rotate(-90 9 9)"
                      />
                    </svg>
                  </button>
                  {s.contextMeterOpen ? (
                    <div
                      className="JObwrW_panel"
                      style={{ top: 'calc(100% + 8px)', bottom: 'auto' }}
                    >
                      <div className="JObwrW_header">
                        <span className="JObwrW_percent">上下文与事件统计</span>
                        <span className="JObwrW_figures">
                          {`游标 #${view?.cursor ?? 0}`}
                        </span>
                      </div>
                      <div className="JObwrW_bar">
                        <span
                          className="JObwrW_segment JObwrW_colorSystem"
                          style={{ width: '25%' }}
                        />
                        <span
                          className="JObwrW_segment JObwrW_colorTools"
                          style={{ width: '35%' }}
                        />
                        <span
                          className="JObwrW_segment JObwrW_colorMessages"
                          style={{ width: '40%' }}
                        />
                      </div>
                      <dl className="JObwrW_rows">
                        <div className="JObwrW_row">
                          <dt>
                            <span className="JObwrW_swatch JObwrW_colorMessages" />
                            历史消息数
                          </dt>
                          <dd>{`${messages.length} 条`}</dd>
                        </div>
                        <div className="JObwrW_row">
                          <dt>
                            <span className="JObwrW_swatch JObwrW_colorTools" />
                            已消费事件
                          </dt>
                          <dd>{`${events.length} 条`}</dd>
                        </div>
                        <div className="JObwrW_row">
                          <dt>
                            <span className="JObwrW_swatch JObwrW_colorSystem" />
                            当前模式与权限
                          </dt>
                          <dd>{`${currentAgentPresetObj.name} · ${currentPresetObj.label}`}</dd>
                        </div>
                      </dl>
                    </div>
                  ) : null}
                </div>

                <button
                  type="button"
                  className="_button_kz6gm_4 _sm_kz6gm_30 _outline_kz6gm_56"
                  style={{ height: 26, padding: '0 10px', fontSize: 12 }}
                  onClick={() => {
                    s.detailsDrawerOpen = !s.detailsDrawerOpen;
                    forceRender();
                  }}
                >
                  {s.detailsDrawerOpen ? '收起详情' : '详情面板'}
                </button>

                <span
                  id="session-state"
                  style={{
                    fontSize: 12,
                    color: 'var(--dsw-alias-label-tertiary)',
                  }}
                >
                  {snapshot ? stateLabel(snapshot, view) : '等待选择'}
                </span>
              </div>
            </div>

            {/* Official DSH Conversation Sub-Tabs: 对话 vs 执行轨迹 */}
            {/* Compute trajectory records from loaded history, live events, or session messages */}
            <div className="wSkVaW_tabs" role="tablist">
              <button
                type="button"
                role="tab"
                aria-selected={s.conversationSubTab === 'chat'}
                className={clsx(
                  'wSkVaW_tab',
                  s.conversationSubTab === 'chat' && 'wSkVaW_tabActive',
                )}
                onClick={() => {
                  s.conversationSubTab = 'chat';
                  forceRender();
                }}
              >
                对话
              </button>
              <button
                type="button"
                role="tab"
                aria-selected={s.conversationSubTab === 'trajectory'}
                className={clsx(
                  'wSkVaW_tab',
                  s.conversationSubTab === 'trajectory' && 'wSkVaW_tabActive',
                )}
                onClick={() => {
                  s.conversationSubTab = 'trajectory';
                  void loadTrajectoryHistory(s.selected);
                  forceRender();
                }}
              >
                {`执行轨迹 (${(s.trajectoryEventsBySession.get(s.selected)?.length || events.length || messages.length)})`}
              </button>
            </div>
          </header>

          <div className="wSkVaW_scrollBody" data-conversation-scroll="">
            <p
              id="global-error"
              className="_banner_ugy7y_1"
              role="alert"
              hidden={!s.globalError}
            >
              {s.globalError}
            </p>

            {/* Blocked / Reconciliation / Frozen Banner */}
            <section
              id="blocked-notice"
              className="bqrRRG_root"
              hidden={!attention}
              aria-labelledby="blocked-title"
            >
              <div className="bqrRRG_card">
                <div className="bqrRRG_strip">
                  <span className="bqrRRG_dot" />
                  <strong id="blocked-title">会话暂时不能继续</strong>
                </div>
                <div className="bqrRRG_body">
                  <div id="blocked-text" className="bqrRRG_headline">
                    {blockedText}
                  </div>
                </div>
                <div className="bqrRRG_actionRow" hidden={!showForkBtn}>
                  <Button
                    ref={bind({ click: openForkDialog })}
                    id="open-fork"
                    variant="primary"
                    disabled={!forkEnabled}
                    onClick={openForkDialog}
                  >
                    确认后 Fork 为普通聊天
                  </Button>
                </div>
              </div>
            </section>

            {/* Chat Flow Area */}
            <div
              className="wSkVaW_viewArea"
              hidden={isHero}
              style={{ flexDirection: 'row' }}
            >
              {s.conversationSubTab === 'trajectory' ? (() => {
                const rawEvents =
                  s.trajectoryEventsBySession.get(s.selected) ??
                  (events.length ? events : []);
                const trajList = rawEvents.length
                  ? rawEvents
                  : messages.map((m, idx) => ({
                      cursor: idx + 1,
                      event: {
                        type:
                          m.role === 'toolResult'
                            ? 'tool_execution_end'
                            : m.role === 'user'
                              ? 'user_prompt'
                              : 'agent_message',
                        role: m.role,
                        summary: clip(messageText(m), 120),
                      },
                      rawMessage: m,
                    }));
                return (
                <div className="qBU-ya_root" style={{ flex: 1 }}>
                  <div className="fV0t5q_root">
                    <div className="fV0t5q_inner">
                      <div className="fV0t5q_actions">
                        {[
                          ['all', '全部事件'],
                          ['agent', '模型输出'],
                          ['tool', '代码与工具'],
                          ['terminal', '终态与恢复'],
                        ].map(([fk, fl]) => (
                          <button
                            key={fk}
                            type="button"
                            aria-pressed={s.trajectoryFilter === fk}
                            className="fV0t5q_toggle"
                            onClick={() => {
                              s.trajectoryFilter = fk;
                              forceRender();
                            }}
                          >
                            {fl}
                          </button>
                        ))}
                      </div>
                      <span
                        style={{
                          marginLeft: 'auto',
                          fontSize: 12,
                          color: 'var(--dsw-alias-label-tertiary)',
                        }}
                      >
                        {`共 ${trajList.length} 条持久化记录 · 游标 #${view?.cursor ?? 0}`}
                      </span>
                    </div>
                  </div>
                  <div className="_1p9O6q_root">
                    <div className="_1p9O6q_plot">
                      <div className="_1p9O6q_labels">
                        <span>模型</span>
                        <span>工具</span>
                        <span>终态</span>
                      </div>
                      <div
                        className="_1p9O6q_track"
                        style={{
                          display: 'flex',
                          alignItems: 'center',
                          gap: 4,
                          padding: '0 12px',
                        }}
                      >
                        {!trajList.length ? (
                          <span className="_1p9O6q_empty">
                            当前会话暂无事件轨迹
                          </span>
                        ) : (
                          trajList.slice(-40).map(ev => {
                            const t = ev.event?.type ?? '';
                            const isTerm = t.startsWith('attempt_');
                            const isTool =
                              t.includes('tool') || t.includes('approval');
                            return (
                              <span
                                key={ev.cursor}
                                title={`#${ev.cursor} · ${EVENT_TYPE_LABELS[t] ?? t}`}
                                onClick={() => {
                                  s.selectedDetailItem = {
                                    title: `事件 #${ev.cursor} · ${EVENT_TYPE_LABELS[t] ?? t}`,
                                    body: recordText(ev),
                                  };
                                  s.detailsDrawerOpen = true;
                                  forceRender();
                                }}
                                style={{
                                  cursor: 'pointer',
                                  height: 10,
                                  flex: '1 1 8px',
                                  maxWidth: 24,
                                  borderRadius: 3,
                                  alignSelf: isTerm
                                    ? 'flex-end'
                                    : isTool
                                      ? 'center'
                                      : 'flex-start',
                                  marginTop: isTerm ? 0 : 8,
                                  marginBottom: isTerm ? 8 : 0,
                                  background: isTerm
                                    ? 'var(--dsw-alias-state-success-primary)'
                                    : isTool
                                      ? '#a78bfa'
                                      : 'var(--dsw-alias-state-business-primary)',
                                }}
                              />
                            );
                          })
                        )}
                      </div>
                    </div>
                  </div>
                  <div className="Y0dWHa_split">
                    <div className="Y0dWHa_tablePane" style={{ padding: 12 }}>
                      {trajList
                        .filter(ev => {
                          if (s.trajectoryFilter === 'all') return true;
                          const t = String(ev.event?.type ?? '');
                          if (s.trajectoryFilter === 'agent')
                            return (
                              t.includes('agent') ||
                              t.includes('message') ||
                              t.includes('prompt')
                            );
                          if (s.trajectoryFilter === 'tool')
                            return t.includes('tool') || t.includes('approval');
                          if (s.trajectoryFilter === 'terminal')
                            return t.startsWith('attempt_') || t === 'reconciled';
                          return true;
                        })
                        .map(ev => (
                          <div
                            key={ev.cursor}
                            className="zGbnIq_rowCard"
                            style={{
                              marginBottom: 8,
                              padding: '8px 12px',
                              cursor: 'pointer',
                            }}
                            onClick={() => {
                              s.selectedDetailItem = {
                                title: `事件 #${ev.cursor} · ${EVENT_TYPE_LABELS[ev.event?.type] ?? ev.event?.type ?? '记录'}`,
                                body: recordText(ev),
                              };
                              s.detailsDrawerOpen = true;
                              forceRender();
                            }}
                          >
                            <div className="zGbnIq_rowHead">
                              <div className="zGbnIq_rowIdentity">
                                <span className="zGbnIq_rowTag">
                                  {`#${ev.cursor}`}
                                </span>
                                <span className="zGbnIq_rowName">
                                  {EVENT_TYPE_LABELS[ev.event?.type] ??
                                    ev.event?.type ??
                                    '会话事件'}
                                </span>
                              </div>
                              <span
                                style={{
                                  marginLeft: 'auto',
                                  fontSize: 12,
                                  color: 'var(--dsw-alias-label-tertiary)',
                                }}
                              >
                                {ev.attempt?.worker_id
                                  ? `工作进程：${ev.attempt.worker_id} · 围栏 ${ev.attempt.fence}`
                                  : ev.event?.summary || '点击查看原始记录'}
                              </span>
                            </div>
                          </div>
                        ))}
                    </div>
                  </div>
                </div>
                );
              })() : null}

              <div
                className="Md3f7G_root"
                hidden={s.conversationSubTab !== 'chat'}
                style={{ flex: 1 }}
              >
                <div className="Md3f7G_scroll">
                  <div
                    id="messages"
                    className="Md3f7G_column"
                    data-chat-flow=""
                  >
                    {messages.map((message, index) => {
                      const role = [
                        'user',
                        'assistant',
                        'system',
                        'toolResult',
                        'custom',
                      ].includes(message.role)
                        ? message.role
                        : 'custom';
                      const presentation = messagePresentation(message);
                      const key = `${message.tool_call_id ?? message.timestamp ?? index}/${index}`;

                      if (role === 'user' && presentation.kind === 'text') {
                        return (
                          <div key={key} className="Md3f7G_flowItem">
                            <div className="gdEzaW_userRow">
                              <div className="gdEzaW_userStack">
                                <div className="gdEzaW_bubble">
                                  {presentation.text}
                                </div>
                              </div>
                            </div>
                          </div>
                        );
                      }

                      if (presentation.kind === 'text') {
                        return (
                          <div key={key} className="Md3f7G_flowItem">
                            <div className="Sxvs8a_root">
                              <div className="Sxvs8a_body">
                                <FormattedMarkdown text={presentation.text} />
                              </div>
                            </div>
                          </div>
                        );
                      }

                      if (presentation.kind === 'tool_call') {
                        return (
                          <div key={key} className="Md3f7G_flowItem">
                            {presentation.text ? (
                              <div
                                className="Sxvs8a_root"
                                style={{ marginBottom: 12 }}
                              >
                                <div className="Sxvs8a_body">
                                  <FormattedMarkdown text={presentation.text} />
                                </div>
                              </div>
                            ) : null}
                            {presentation.calls.map((call, callIndex) => {
                              const callKey = `${key}/${callIndex}`;
                              const isOpen = expandedSet.has(callKey);
                              const title =
                                call.name === 'execute_code'
                                  ? call.arguments?.action === 'wait'
                                    ? '等待任务结果'
                                    : '运行代码单元'
                                  : CAPABILITY_LABELS[call.name] ?? '调用能力';
                              const rawCode =
                                typeof call.arguments?.code === 'string'
                                  ? call.arguments.code.replace(/\s+/g, ' ').trim()
                                  : '';
                              const summary =
                                call.name === 'execute_code'
                                  ? rawCode
                                    ? clip(rawCode, 80)
                                    : call.arguments?.action === 'wait'
                                      ? '等待当前代码单元完成'
                                      : '执行受控代码单元'
                                  : clip(JSON.stringify(call.arguments ?? {}), 80);
                              return (
                                <div key={callKey} className="Md3f7G_callRow">
                                  <div
                                    className="_Xvjua_root"
                                    data-variant="others"
                                    data-state="ok"
                                  >
                                    <DisclosureRow
                                      rowClassName="_Xvjua_row"
                                      leadingClassName="_Xvjua_leading"
                                      titleClassName="_Xvjua_title"
                                      chevronClassName="_Xvjua_chevron"
                                      icon={<IconCodeOutline16 size={16} />}
                                      title={title}
                                      open={isOpen}
                                      expandable={true}
                                      expandOnRowClick={true}
                                      keepContentWhenOpen={true}
                                      onToggle={() =>
                                        toggleRecordExpand(callKey, !isOpen)
                                      }
                                      collapsedContent={
                                        <>
                                          <span
                                            className="_Xvjua_separator"
                                            aria-hidden="true"
                                          />
                                          <span className="_Xvjua_summary">
                                            {summary}
                                          </span>
                                        </>
                                      }
                                    >
                                      <pre className="_Xvjua_body">
                                        {recordText(call)}
                                      </pre>
                                    </DisclosureRow>
                                  </div>
                                </div>
                              );
                            })}
                          </div>
                        );
                      }

                      const isOpen = expandedSet.has(key);
                      const isErr =
                        presentation.status === 'error' ||
                        Boolean(message.error_message) ||
                        Boolean(message.outcome_unknown);
                      return (
                        <div key={key} className="Md3f7G_flowItem">
                          <div className="Md3f7G_callRow">
                            <div
                              className="_Xvjua_root"
                              data-variant="others"
                              data-state={isErr ? 'error' : 'ok'}
                            >
                              <DisclosureRow
                                rowClassName="_Xvjua_row"
                                leadingClassName="_Xvjua_leading"
                                titleClassName="_Xvjua_title"
                                chevronClassName="_Xvjua_chevron"
                                icon={<IconCodeOutline16 size={16} />}
                                title={presentation.title || '工具执行结果'}
                                open={isOpen}
                                expandable={true}
                                expandOnRowClick={true}
                                keepContentWhenOpen={true}
                                onToggle={() => toggleRecordExpand(key, !isOpen)}
                                collapsedContent={
                                  <>
                                    <span
                                      className="_Xvjua_separator"
                                      aria-hidden="true"
                                    />
                                    <span
                                      className="_Xvjua_summary"
                                      data-error={isErr || undefined}
                                    >
                                      {presentation.text ||
                                        (presentation.values ?? [])
                                          .map(v => `${v.label}: ${v.text}`)
                                          .join(' · ') ||
                                        '点击展开详情'}
                                    </span>
                                  </>
                                }
                              >
                                <pre
                                  className="_Xvjua_body"
                                  data-error={isErr || undefined}
                                >
                                  {presentation.raw}
                                </pre>
                              </DisclosureRow>
                            </div>
                          </div>
                          {message.error_message ? (
                            <div className="gdEzaW_turnErrorRow">
                              <StateDot
                                state="error"
                                className="gdEzaW_turnErrorDot"
                              />
                              <div className="gdEzaW_turnErrorCopy">
                                <span className="gdEzaW_turnErrorTitle">
                                  执行异常
                                </span>
                                <span className="gdEzaW_turnErrorMessage">
                                  {clip(message.error_message, 2000)}
                                </span>
                              </div>
                            </div>
                          ) : null}
                          {message.outcome_unknown ? (
                            <div className="gdEzaW_turnErrorRow">
                              <StateDot
                                state="warning"
                                className="gdEzaW_turnErrorDot"
                              />
                              <div className="gdEzaW_turnErrorCopy">
                                <span className="gdEzaW_maxTokensTitle">
                                  待核对
                                </span>
                                <span className="gdEzaW_turnErrorMessage">
                                  此工具调用的真实结果待宿主核对。
                                </span>
                              </div>
                            </div>
                          ) : null}
                        </div>
                      );
                    })}

                  </div>
                  {/* Live streaming preview */}
                  <section
                    id="live-section"
                    className="Md3f7G_column"
                    hidden={!view?.liveText}
                    aria-labelledby="live-heading"
                  >
                    <div className="Md3f7G_turnStatus" id="live-heading">
                      正在思考与生成…
                    </div>
                    <div
                      className="Sxvs8a_root"
                      data-streaming="true"
                      style={{ marginTop: 8 }}
                    >
                      <div id="live-text" className="Sxvs8a_body">
                        <FormattedMarkdown text={view?.liveText ?? ''} />
                      </div>
                    </div>
                  </section>
                </div>
              </div>

              {/* Official DSH Right-Hand Details Panel (ydkMvW_*) */}
              {s.detailsDrawerOpen ? (
                <aside
                  className="ydkMvW_root"
                  style={{ width: 340, flexShrink: 0 }}
                  aria-label="执行与会话详情"
                >
                  <div className="ydkMvW_header">
                    <span className="ydkMvW_title">
                      {s.selectedDetailItem?.title ?? '会话与执行详情'}
                    </span>
                    <button
                      type="button"
                      className="ydkMvW_close"
                      aria-label="关闭详情面板"
                      onClick={() => {
                        s.detailsDrawerOpen = false;
                        forceRender();
                      }}
                    >
                      <IconCloseOutline16 size={14} />
                    </button>
                  </div>
                  <div className="ydkMvW_body">
                    <div className="ydkMvW_section">
                      <div className="ydkMvW_sectionLabel">当前会话状态</div>
                      <pre className="ydkMvW_code">
                        {`会话标识：${snapshot?.id ?? '未选择'}\n当前状态：${snapshot ? stateLabel(snapshot, view) : '未知'}\n事件游标：#${view?.cursor ?? 0}\n权限预设：${currentPresetObj.label}\n会话模式：${currentAgentPresetObj.name}`}
                      </pre>
                    </div>
                    <div className="ydkMvW_section">
                      <div className="ydkMvW_sectionLabel">
                        {s.selectedDetailItem
                          ? '选中记录原始数据'
                          : '最近一条消息或事件记录'}
                      </div>
                      <pre className="ydkMvW_code">
                        {s.selectedDetailItem?.body ??
                          (events.length
                            ? recordText(events.at(-1))
                            : messages.length
                              ? recordText(messages.at(-1))
                              : '点击任意代码单元或轨迹事件可在此查看完整结构化记录。')}
                      </pre>
                    </div>
                  </div>
                </aside>
              ) : null}
            </div>

            {/* Official DSH Approval Panel (@deepseek-ai/dsh-client-ui-conversation/ApprovalPanel) */}
            <section
              id="approvals"
              className="bqrRRG_root"
              aria-label="待审批请求"
              hidden={!approvals.length}
            >
              {approvals.slice(0, 30).map((approval, idx) => {
                const id = approval.id ?? approval.approval_id;
                const input = approval.input ?? {};
                const capability = input.capability ?? '';
                let scope = null;
                try {
                  scope = JSON.parse(input.scope);
                } catch {
                  scope = null;
                }
                const operation =
                  CAPABILITY_LABELS[capability] ??
                  `使用能力：${clip(capability || '宿主未说明', 100)}`;
                const scopeLines = [];
                if (object(scope)) {
                  if (scope.path !== undefined)
                    scopeLines.push(['文件', scope.path, 400]);
                  if (scope.profile !== undefined)
                    scopeLines.push(['预设任务', scope.profile, 400]);
                  if (scope.query !== undefined)
                    scopeLines.push(['检索内容', scope.query, 400]);
                  if (scope.filter !== undefined)
                    scopeLines.push(['测试范围', scope.filter, 400]);
                  if (scope.server !== undefined)
                    scopeLines.push(['工具连接', scope.server, 400]);
                  if (scope.tool !== undefined)
                    scopeLines.push(['工具', scope.tool, 400]);
                  if (scope.name !== undefined)
                    scopeLines.push(['名称', scope.name, 400]);
                  if (scope.commit !== undefined)
                    scopeLines.push(['提交', scope.commit, 400]);
                  if (scope.overwrite === true)
                    scopeLines.push(['文件操作', '覆盖现有文件', 400]);
                  if (typeof scope.content === 'string')
                    scopeLines.push(['写入内容', scope.content, 600]);
                  if (typeof scope.old === 'string')
                    scopeLines.push(['替换原文', scope.old, 400]);
                  if (typeof scope.new === 'string')
                    scopeLines.push(['替换为', scope.new, 400]);
                }
                const isDeciding =
                  typeof id === 'string' &&
                  s.approvalPending.has(`${snapshot?.id}/${id}`);

                return (
                  <div
                    key={id ?? idx}
                    className="bqrRRG_card"
                    style={{ marginBottom: 8 }}
                  >
                    <div className="bqrRRG_strip">
                      <span className="bqrRRG_dot" />
                      <span>等待审批 · 需要你的授权（精确参数范围）</span>
                    </div>
                    <div className="bqrRRG_body" data-approval-scroll="">
                      <div className="bqrRRG_headline">{operation}</div>
                      {scopeLines.map(([label, val, limit], sIdx) => (
                        <div key={sIdx} className="bqrRRG_command">
                          {`${label}：${clip(val, limit)}`}
                        </div>
                      ))}
                    </div>
                    <details style={{ padding: '4px 16px 0', fontSize: 12, color: 'var(--dsw-alias-label-tertiary)' }}>
                      <summary style={{ cursor: 'pointer' }}>查看完整授权参数</summary>
                      <pre style={{ margin: '6px 0 0', whiteSpace: 'pre-wrap' }}>
                        {recordText(approval)}
                      </pre>
                    </details>
                    {typeof id !== 'string' ? (
                      <div className="bqrRRG_body">
                        <span className="gdEzaW_turnErrorTitle">
                          审批记录缺少 ID，请在宿主中处理。
                        </span>
                      </div>
                    ) : (
                      <div className="bqrRRG_actionRow">
                        {[
                          ['allow_once', '允许一次', 'primary'],
                          ['allow_task', '允许此任务', 'outline'],
                          ['deny', '拒绝', 'outline bqrRRG_reject'],
                        ].map(([decision, btnLabel, variantCls]) => {
                          const isPrimary = variantCls === 'primary';
                          const isReject = variantCls.includes('bqrRRG_reject');
                          return (
                            <button
                              key={decision}
                              ref={bind({
                                click: () =>
                                  trackAsync(decide(snapshot.id, id, decision)),
                              })}
                              type="button"
                              className={clsx(
                                '_button_kz6gm_4 _md_kz6gm_24',
                                isPrimary
                                  ? '_primary_kz6gm_38'
                                  : '_outline_kz6gm_56',
                                isReject && 'bqrRRG_reject',
                              )}
                              disabled={isDeciding || approvalButtonsDisabled}
                              onClick={() =>
                                trackAsync(decide(snapshot.id, id, decision))
                              }
                            >
                              {isDeciding ? '正在记录…' : btnLabel}
                            </button>
                          );
                        })}
                      </div>
                    )}
                  </div>
                );
              })}
            </section>

            {/* Official DSH Composer Seat & Centered Hero */}
            <div className="wSkVaW_composerSeat" data-composer-seat="">
              <div
                className={clsx(
                  'wSkVaW_composerStack',
                  isHero && 'wSkVaW_composerHero',
                )}
              >
                {isHero ? (
                  <>
                    <HeroGlow className="wSkVaW_heroGlow" />
                    <div className="pXSMma_root">
                      <div className="pXSMma_stack">
                        <div className="pXSMma_headline">
                          <span className="pXSMma_fishHitbox">
                            <FishLogo size={34} className="pXSMma_fish" />
                          </span>
                          <span className="pXSMma_headlineText">
                            所想即所码，全程可掌控
                          </span>
                          <span className="pXSMma_previewBadge">本地版</span>
                        </div>
                        <div className="pXSMma_body" />
                      </div>
                    </div>
                    <div className="wSkVaW_heroWorkspaceRow">
                      <Menu
                        open={s.heroWsMenuOpen}
                        onClose={() => {
                          s.heroWsMenuOpen = false;
                          forceRender();
                        }}
                        selectedId={currentProjectId()}
                        items={projectSummaries.map(p => ({
                          id: p.id,
                          label: p.label,
                          icon: <IconFolderClose16 size={16} />,
                        }))}
                        footer={[
                          {
                            id: '__add_workspace__',
                            label: '添加工作区…',
                            icon: <IconProjectAddOutline16 size={16} />,
                          },
                        ]}
                        onSelect={id => {
                          s.heroWsMenuOpen = false;
                          if (id === '__add_workspace__') {
                            openNewProjectModal();
                          } else {
                            void selectProject(id);
                          }
                          forceRender();
                        }}
                        portal={true}
                        anchor={
                          <button
                            type="button"
                            className="pXSMma_workspace"
                            aria-label="选择工作区"
                            aria-haspopup="menu"
                            aria-expanded={s.heroWsMenuOpen}
                            onClick={() => {
                              s.heroWsMenuOpen = !s.heroWsMenuOpen;
                              forceRender();
                            }}
                          >
                            <IconFolderOpen16
                              className="pXSMma_folder"
                              size={16}
                            />
                            <span className="pXSMma_workspaceLabel">
                              {currentProjectSummary?.label ?? '选择工作区'}
                            </span>
                            <IconChevronDownOutline14
                              className="pXSMma_chevron"
                              size={12}
                            />
                          </button>
                        }
                      />

                      <Menu
                        open={s.heroPresetMenuOpen}
                        onClose={() => {
                          s.heroPresetMenuOpen = false;
                          forceRender();
                        }}
                        selectedId={s.selectedAgentPreset}
                        items={AGENT_PRESETS.map(ap => ({
                          id: ap.id,
                          label: ap.name,
                          icon: <IconAgentPresetOutline16 size={16} />,
                        }))}
                        onSelect={id => {
                          s.selectedAgentPreset = id;
                          s.heroPresetMenuOpen = false;
                          forceRender();
                        }}
                        portal={true}
                        anchor={
                          <button
                            type="button"
                            className="pXSMma_workspace"
                            aria-label="选择 Agent 预设"
                            aria-haspopup="menu"
                            aria-expanded={s.heroPresetMenuOpen}
                            onClick={() => {
                              s.heroPresetMenuOpen = !s.heroPresetMenuOpen;
                              forceRender();
                            }}
                          >
                            <IconAgentPresetOutline16
                              className="pXSMma_folder"
                              size={16}
                            />
                            <span className="pXSMma_workspaceLabel">
                              {currentAgentPresetObj.name}
                            </span>
                            <IconChevronDownOutline14
                              className="pXSMma_chevron"
                              size={12}
                            />
                          </button>
                        }
                      />

                      <Menu
                        open={s.heroDeviceMenuOpen}
                        onClose={() => {
                          s.heroDeviceMenuOpen = false;
                          forceRender();
                        }}
                        selectedId={s.activeDeviceId}
                        items={s.boundDevices.map(dev => ({
                          id: dev.device_id,
                          label: `${dev.label} (${dev.device_id})`,
                          icon: <IconDataOutline16 size={16} />,
                        }))}
                        footer={[
                          {
                            id: '__add_device__',
                            label: '添加新设备 (RSA 一次性绑定)…',
                            icon: <IconPlusOutline16 size={16} />,
                          },
                        ]}
                        onSelect={id => {
                          s.heroDeviceMenuOpen = false;
                          if (id === '__add_device__') {
                            openAddDeviceModal();
                          } else {
                            void handleSelectDevice(id);
                          }
                          forceRender();
                        }}
                        portal={true}
                        anchor={
                          <button
                            id="hero-device-btn"
                            type="button"
                            className="pXSMma_workspace"
                            aria-label="选择或添加运行设备"
                            aria-haspopup="menu"
                            aria-expanded={s.heroDeviceMenuOpen}
                            onClick={() => {
                              s.heroDeviceMenuOpen = !s.heroDeviceMenuOpen;
                              forceRender();
                            }}
                          >
                            <IconDataOutline16
                              className="pXSMma_folder"
                              size={16}
                            />
                            <span className="pXSMma_workspaceLabel">
                              {s.boundDevices.find(
                                d => d.device_id === s.activeDeviceId,
                              )?.label ?? '本机 macOS 设备'}
                            </span>
                            <IconChevronDownOutline14
                              className="pXSMma_chevron"
                              size={12}
                            />
                          </button>
                        }
                      />
                    </div>
                  </>
                ) : null}

                <div className={clsx('uV2eYG_root', isHero && 'uV2eYG_hero')}>
                  {/* Official DSH GoalBar Dock (nLMEza_*) */}
                  {s.goalBarOpen || s.sessionGoal ? (
                    <div className="nLMEza_dock" style={{ marginBottom: 6 }}>
                      <div className="nLMEza_bar">
                        <span className="nLMEza_label">会话目标</span>
                        {s.goalEditing ? (
                          <input
                            type="text"
                            className="nLMEza_objectiveInput"
                            placeholder="输入当前会话要达成的具体目标，按回车保存…"
                            value={s.goalDraft}
                            autoFocus={true}
                            onChange={e => {
                              s.goalDraft = e.target.value;
                              forceRender();
                            }}
                            onKeyDown={e => {
                              if (e.key === 'Enter') {
                                e.preventDefault();
                                s.sessionGoal = s.goalDraft.trim();
                                s.goalEditing = false;
                                if (!s.sessionGoal) s.goalBarOpen = false;
                                forceRender();
                              } else if (e.key === 'Escape') {
                                s.goalEditing = false;
                                if (!s.sessionGoal) s.goalBarOpen = false;
                                forceRender();
                              }
                            }}
                          />
                        ) : (
                          <span className="nLMEza_objective">
                            {s.sessionGoal || '未设置目标'}
                          </span>
                        )}
                        <div className="nLMEza_actions">
                          {s.goalEditing ? (
                            <button
                              type="button"
                              className="nLMEza_iconBtn"
                              aria-label="保存目标"
                              title="保存目标"
                              onClick={() => {
                                s.sessionGoal = s.goalDraft.trim();
                                s.goalEditing = false;
                                if (!s.sessionGoal) s.goalBarOpen = false;
                                forceRender();
                              }}
                            >
                              <IconCheckOutline16 size={14} />
                            </button>
                          ) : (
                            <button
                              type="button"
                              className="nLMEza_iconBtn"
                              aria-label="编辑目标"
                              title="编辑目标"
                              onClick={() => {
                                s.goalDraft = s.sessionGoal;
                                s.goalEditing = true;
                                forceRender();
                              }}
                            >
                              <IconEditOutline16 size={14} />
                            </button>
                          )}
                          <button
                            type="button"
                            className="nLMEza_iconBtn"
                            aria-label="清除目标"
                            title="清除目标"
                            onClick={() => {
                              s.sessionGoal = '';
                              s.goalDraft = '';
                              s.goalEditing = false;
                              s.goalBarOpen = false;
                              forceRender();
                            }}
                          >
                            <IconCloseOutline16 size={14} />
                          </button>
                        </div>
                      </div>
                    </div>
                  ) : null}

                  {/* Official DSH Slash Command & Skill InputTrigger Menu (_3e4SsG_*) */}
                  {s.slashMenuOpen ? (
                    <div
                      style={{
                        width: '100%',
                        maxWidth: 'var(--dsh-composer-card-max-width)',
                        position: 'relative',
                      }}
                    >
                      <div
                        className="_3e4SsG_menu"
                        role="listbox"
                        aria-label="快捷指令与技能包"
                      >
                        <div className="_3e4SsG_viewport">
                          <div className="_3e4SsG_sectionTitle">快捷操作</div>
                          {[
                            [
                              '/plan',
                              '切换规划模式',
                              () => {
                                s.planModeActive = !s.planModeActive;
                              },
                            ],
                            [
                              '/goal',
                              '设定当前会话目标',
                              () => {
                                s.goalBarOpen = true;
                                s.goalEditing = true;
                                s.goalDraft = s.sessionGoal;
                              },
                            ],
                            [
                              '/mcp',
                              '管理外部工具服务',
                              () => {
                                openSettings('mcp');
                              },
                            ],
                            [
                              '/skills',
                              '浏览与管理本地技能包',
                              () => {
                                openSettings('skills');
                              },
                            ],
                          ]
                            .filter(
                              ([cmd, desc]) =>
                                !s.slashFilter ||
                                cmd.includes(s.slashFilter) ||
                                desc.includes(s.slashFilter),
                            )
                            .map(([cmd, desc, fn]) => (
                              <button
                                key={cmd}
                                type="button"
                                className="_3e4SsG_item"
                                onClick={() => {
                                  s.slashMenuOpen = false;
                                  if (s.promptText.startsWith('/')) {
                                    s.promptText = '';
                                    if (promptInputRef.current)
                                      promptInputRef.current.value = '';
                                  }
                                  fn();
                                  forceRender();
                                }}
                              >
                                <span className="_3e4SsG_itemName">{cmd}</span>
                                <span className="_3e4SsG_itemDescription">
                                  {desc}
                                </span>
                              </button>
                            ))}

                          <div className="_3e4SsG_sectionTitle">
                            {`本地技能包 (${s.skillsCatalog.length})`}
                          </div>
                          {s.skillsCatalog
                            .filter(
                              sk =>
                                !s.slashFilter ||
                                sk.name
                                  .toLowerCase()
                                  .includes(s.slashFilter.toLowerCase()) ||
                                sk.description
                                  .toLowerCase()
                                  .includes(s.slashFilter.toLowerCase()),
                            )
                            .slice(0, 12)
                            .map(sk => (
                              <button
                                key={sk.name}
                                type="button"
                                className="_3e4SsG_item"
                                onClick={() => {
                                  s.slashMenuOpen = false;
                                  if (
                                    !s.attachedItems.some(
                                      it =>
                                        it.kind === 'skill' &&
                                        it.name === sk.name,
                                    )
                                  ) {
                                    s.attachedItems.push({
                                      kind: 'skill',
                                      name: sk.name,
                                      desc: sk.description,
                                    });
                                  }
                                  if (s.promptText.startsWith('/')) {
                                    s.promptText = '';
                                    if (promptInputRef.current)
                                      promptInputRef.current.value = '';
                                  }
                                  forceRender();
                                }}
                              >
                                <span className="_3e4SsG_itemName">
                                  {`/${sk.name}`}
                                </span>
                                <span className="_3e4SsG_itemDescription">
                                  {sk.description}
                                </span>
                              </button>
                            ))}
                        </div>
                      </div>
                    </div>
                  ) : null}

                  {promptStatus ? (
                    <div
                      id="prompt-status"
                      className="uV2eYG_notice"
                      role="status"
                    >
                      {promptStatus}
                    </div>
                  ) : (
                    <span
                      id="prompt-status"
                      className="visually-hidden"
                      role="status"
                    />
                  )}

                  <form
                    ref={bind({ submit: e => trackAsync(handleSendPrompt(e)) })}
                    id="prompt-form"
                    className="uV2eYG_card"
                    data-composer-card="true"
                    onSubmit={e => trackAsync(handleSendPrompt(e))}
                  >
                    {/* Attached Skills & Context Files Rail (_54WpYG_rail) */}
                    {s.attachedItems.length > 0 ? (
                      <div
                        className="_54WpYG_rail"
                        style={{
                          display: 'flex',
                          flexWrap: 'wrap',
                          gap: 6,
                          padding: '8px 12px 0',
                        }}
                      >
                        {s.attachedItems.map((item, idx) => (
                          <span
                            key={`${item.kind}:${item.name}:${idx}`}
                            className="rS3zOq_chip"
                            style={{
                              background:
                                item.kind === 'skill'
                                  ? 'color-mix(in srgb, var(--dsw-alias-state-business-primary) 16%, transparent)'
                                  : 'var(--dsw-alias-interactive-bg-hover)',
                              color:
                                item.kind === 'skill'
                                  ? 'var(--dsw-alias-state-business-primary)'
                                  : 'var(--dsw-alias-label-primary)',
                            }}
                          >
                            <span>
                              {item.kind === 'skill'
                                ? `技能：${item.name}`
                                : `文件：${item.name}`}
                            </span>
                            <button
                              type="button"
                              aria-label={`移除 ${item.name}`}
                              style={{
                                background: 'none',
                                border: 'none',
                                color: 'inherit',
                                cursor: 'pointer',
                                padding: 0,
                                display: 'inline-flex',
                              }}
                              onClick={() => {
                                s.attachedItems.splice(idx, 1);
                                forceRender();
                              }}
                            >
                              <IconCloseOutline16 size={12} />
                            </button>
                          </span>
                        ))}
                      </div>
                    ) : null}
                    <label htmlFor="prompt" className="visually-hidden">
                      发送消息
                    </label>
                    <div className="uV2eYG_scroll" data-input-scroll="true">
                      <div className="uV2eYG_grow">
                        <textarea
                          ref={bind(
                            {
                              input: e => {
                                s.promptText = e.currentTarget.value;
                                forceRender();
                              },
                            },
                            promptInputRef,
                          )}
                          id="prompt"
                          data-plain-input="true"
                          className="uV2eYG_input"
                          rows={isHero ? 3 : 2}
                          maxLength={32000}
                          disabled={promptDisabled}
                          placeholder={
                            isHero ? '描述你想要构建的内容' : '给智能体发消息'
                          }
                          value={s.promptText}
                          onChange={e => {
                            const val = e.target.value;
                            s.promptText = val;
                            if (val.startsWith('/') && !val.includes(' ')) {
                              s.slashMenuOpen = true;
                              s.slashFilter = val.slice(1);
                              if (!s.skillsLoaded) void refreshSkillsCatalog();
                            } else if (s.slashMenuOpen) {
                              s.slashMenuOpen = false;
                            }
                            forceRender();
                          }}
                          onKeyDown={e => {
                            if (
                              e.key === 'Enter' &&
                              !e.shiftKey &&
                              !e.isComposing &&
                              (s.enterBehavior === 'enter' ||
                                e.metaKey ||
                                e.ctrlKey)
                            ) {
                              e.preventDefault();
                              trackAsync(handleSendPrompt(e));
                            }
                          }}
                        />
                        <div
                          className="uV2eYG_mirror"
                          aria-hidden="true"
                          data-input-mirror="true"
                        >
                          {`${s.promptText || ' '}\n`}
                        </div>
                      </div>
                    </div>

                    <div className="uV2eYG_row">
                      <div className="uV2eYG_tools">
                        <Menu
                          open={s.composerAddMenuOpen}
                          onClose={() => {
                            s.composerAddMenuOpen = false;
                            forceRender();
                          }}
                          side="top"
                          items={[
                            {
                              id: 'skill',
                              label: '调用本地技能包 (/技能)…',
                              icon: <IconAgentPresetOutline16 size={16} />,
                            },
                            {
                              id: 'file',
                              label: '附加工作区文件 (README.md)…',
                              icon: <IconCodeOutline16 size={16} />,
                            },
                            {
                              id: 'goal',
                              label: '设定当前会话目标…',
                              icon: <IconEditOutline16 size={16} />,
                            },
                            {
                              id: 'plan',
                              label: s.planModeActive
                                ? '退出规划模式'
                                : '开启规划模式',
                              icon: <IconCheckOutline16 size={16} />,
                            },
                            {
                              id: 'mcp',
                              label: '配置外部工具服务…',
                              icon: <IconDataOutline16 size={16} />,
                            },
                            {
                              id: 'workspace',
                              label: '添加新工作区目录…',
                              icon: <IconProjectAddOutline16 size={16} />,
                            },
                            {
                              id: 'device',
                              label: '添加新设备 (RSA 一次性绑定)…',
                              icon: <IconDataOutline16 size={16} />,
                            },
                          ]}
                          onSelect={id => {
                            s.composerAddMenuOpen = false;
                            if (id === 'skill') {
                              if (!s.skillsLoaded) void refreshSkillsCatalog();
                              s.slashFilter = '';
                              s.slashMenuOpen = true;
                            } else if (id === 'file') {
                              if (
                                !s.attachedItems.some(
                                  it =>
                                    it.kind === 'file' &&
                                    it.name === 'README.md',
                                )
                              ) {
                                s.attachedItems.push({
                                  kind: 'file',
                                  name: 'README.md',
                                });
                              }
                            } else if (id === 'goal') {
                              s.goalBarOpen = true;
                              s.goalEditing = true;
                              s.goalDraft = s.sessionGoal;
                            } else if (id === 'plan') {
                              s.planModeActive = !s.planModeActive;
                            } else if (id === 'mcp') {
                              openSettings('mcp');
                            } else if (id === 'workspace') {
                              openNewProjectModal();
                            } else if (id === 'device') {
                              openAddDeviceModal();
                            }
                            forceRender();
                          }}
                          anchor={
                            <button
                              id="composer-add-btn"
                              type="button"
                              className="uV2eYG_add"
                              aria-label="添加技能、文件或工作区"
                              title="调用技能包、附加文件、设定目标或管理外部工具"
                              onClick={() => {
                                s.composerAddMenuOpen = !s.composerAddMenuOpen;
                                forceRender();
                              }}
                            >
                              <IconPlusOutline16 size={14} />
                            </button>
                          }
                        />

                        <div className="uV2eYG_modes">
                          <Menu
                            open={s.permissionPopoverOpen}
                            onClose={() => {
                              s.permissionPopoverOpen = false;
                              forceRender();
                            }}
                            side="top"
                            selectedId={s.permissionPreset}
                            items={PERMISSION_PRESETS.map(p => ({
                              id: p.id,
                              label: p.label,
                              icon: <PermissionGlyph value={p.id} />,
                            }))}
                            footer={[
                              {
                                id: '__manage_grants__',
                                label: '查看授权与审计记录…',
                                icon: <IconSettingsOutline16 size={16} />,
                              },
                            ]}
                            onSelect={id => {
                              if (id === '__manage_grants__') {
                                s.permissionPopoverOpen = false;
                                openSettings('permissions');
                              } else {
                                selectPermissionPreset(id);
                              }
                            }}
                            anchor={
                              <button
                                id="composer-permission-btn"
                                type="button"
                                className="Sh0Q9G_trigger"
                                aria-label={`访问模式，当前：${currentPresetObj.label}`}
                                title={currentPresetObj.desc}
                                onClick={() => {
                                  s.permissionPopoverOpen =
                                    !s.permissionPopoverOpen;
                                  forceRender();
                                }}
                              >
                                <span
                                  className="Sh0Q9G_triggerIcon"
                                  aria-hidden="true"
                                >
                                  <PermissionGlyph value={s.permissionPreset} />
                                </span>
                                <span className="Sh0Q9G_triggerLabel">
                                  {currentPresetObj.label}
                                </span>
                                <span
                                  className={clsx(
                                    'Sh0Q9G_chevron',
                                    s.permissionPopoverOpen &&
                                      'Sh0Q9G_chevronOpen',
                                  )}
                                  aria-hidden="true"
                                >
                                  <IconChevronDownOutline14 />
                                </span>
                              </button>
                            }
                          />

                          {/* Official DSH PlanModeControl (rS3zOq_*) */}
                          <span className="rS3zOq_wrap">
                            {s.planModeActive ? (
                              <button
                                type="button"
                                className="rS3zOq_chip"
                                title="点击退出规划模式"
                                onClick={() => {
                                  s.planModeActive = false;
                                  forceRender();
                                }}
                              >
                                <span>规划模式</span>
                                <span className="rS3zOq_close">
                                  <IconCloseOutline16 size={12} />
                                </span>
                              </button>
                            ) : (
                              <button
                                type="button"
                                className="Sh0Q9G_trigger"
                                title="开启规划模式：先输出步骤方案再执行修改"
                                onClick={() => {
                                  s.planModeActive = true;
                                  forceRender();
                                }}
                              >
                                <span className="Sh0Q9G_triggerLabel">
                                  规划模式
                                </span>
                              </button>
                            )}
                          </span>
                        </div>
                      </div>

                      <div className="uV2eYG_trailing">
                        <div className="_7KE1Ra_root">
                          <button
                            id="composer-model-btn"
                            type="button"
                            className="_7KE1Ra_trigger"
                            aria-label={`当前模型：${s.selectedModel}`}
                            aria-haspopup="listbox"
                            aria-expanded={s.modelMenuOpen}
                            title="切换模型或配置模型服务"
                            onClick={() => {
                              s.modelMenuOpen = !s.modelMenuOpen;
                              forceRender();
                            }}
                          >
                            <span className="_7KE1Ra_triggerLabel">
                              {s.selectedModel}
                            </span>
                            <IconChevronDownOutline14
                              className={clsx(
                                '_7KE1Ra_chevron',
                                s.modelMenuOpen && '_7KE1Ra_chevronOpen',
                              )}
                            />
                          </button>

                          {s.modelMenuOpen ? (
                            <div
                              className="_7KE1Ra_menu"
                              role="listbox"
                              aria-label="选择模型"
                            >
                              <div className="_7KE1Ra_groups">
                                {s.modelProviders.map(provider => (
                                  <div
                                    key={provider.id}
                                    className="_7KE1Ra_group"
                                  >
                                    <div className="_7KE1Ra_groupTitle">
                                      {provider.name}
                                    </div>
                                    {provider.models.map(m => {
                                      const isSelected =
                                        s.selectedModel === m.id;
                                      return (
                                        <button
                                          key={m.id}
                                          type="button"
                                          role="option"
                                          aria-selected={isSelected}
                                          className={clsx(
                                            '_7KE1Ra_option',
                                            isSelected && '_7KE1Ra_selected',
                                          )}
                                          onClick={() => {
                                            s.selectedModel = m.id;
                                            s.modelMenuOpen = false;
                                            forceRender();
                                          }}
                                        >
                                          <span className="_7KE1Ra_optionCopy">
                                            <span className="_7KE1Ra_modelName">
                                              {m.label || m.id}
                                            </span>
                                            <span className="_7KE1Ra_description">
                                              {m.desc}
                                            </span>
                                          </span>
                                          <span className="_7KE1Ra_check">
                                            {isSelected ? (
                                              <IconCheckOutline16 size={14} />
                                            ) : null}
                                          </span>
                                        </button>
                                      );
                                    })}
                                  </div>
                                ))}
                              </div>
                              <div
                                style={{
                                  borderTop:
                                    '1px solid var(--dsw-alias-border-l2)',
                                  marginTop: 4,
                                  paddingTop: 4,
                                }}
                              >
                                <button
                                  type="button"
                                  className="_7KE1Ra_cell"
                                  onClick={() => {
                                    s.modelMenuOpen = false;
                                    openSettings('models');
                                  }}
                                >
                                  <span className="_7KE1Ra_cellLabel">
                                    配置模型与接口密钥…
                                  </span>
                                  <span className="_7KE1Ra_cellValue">
                                    {`${s.modelProviders.length} 个服务`}
                                  </span>
                                  <IconChevronRightOutline14
                                    className="_7KE1Ra_cellChevron"
                                    size={14}
                                  />
                                </button>
                              </div>
                            </div>
                          ) : null}
                        </div>

                        <button
                          ref={bind({ click: () => trackAsync(handleCancel()) })}
                          id="cancel-button"
                          type="button"
                          className="uV2eYG_primary"
                          aria-label="停止生成"
                          disabled={cancelDisabled}
                          style={
                            cancelDisabled
                              ? {
                                  position: 'absolute',
                                  width: 1,
                                  height: 1,
                                  opacity: 0,
                                  pointerEvents: 'none',
                                  overflow: 'hidden',
                                }
                              : undefined
                          }
                          onClick={() => trackAsync(handleCancel())}
                        >
                          <svg
                            viewBox="0 0 16 16"
                            width="16"
                            height="16"
                            aria-hidden="true"
                          >
                            <rect
                              x="3"
                              y="3"
                              width="10"
                              height="10"
                              rx="3"
                              fill="currentColor"
                            />
                          </svg>
                          <span className="visually-hidden">停止</span>
                        </button>

                        <button
                          id="send-button"
                          type="submit"
                          className="uV2eYG_primary"
                          aria-label="发送消息"
                          disabled={sendDisabled}
                          style={
                            !cancelDisabled
                              ? {
                                  position: 'absolute',
                                  width: 1,
                                  height: 1,
                                  opacity: 0,
                                  pointerEvents: 'none',
                                  overflow: 'hidden',
                                }
                              : undefined
                          }
                        >
                          <IconSendOutline16 size={16} />
                          <span className="visually-hidden">
                            {pending ? '等待完成…' : '发送'}
                          </span>
                        </button>
                      </div>
                    </div>
                  </form>
                </div>
              </div>
            </div>

            {/* Event Ledger Drawer (accessible for cursor verification) */}
            <details className="visually-hidden">
              <summary style={{ cursor: 'pointer', userSelect: 'none' }}>
                <span>事件流记录</span>
                <span id="event-count" style={{ marginLeft: 8 }}>
                  {`${events.length} · 游标 ${view?.cursor ?? 0}`}
                </span>
              </summary>
              <div id="event-log" style={{ marginTop: 6 }}>
                {events.map(record => (
                  <details key={record.cursor}>
                    <summary>
                      {`#${record.cursor} · ${clip(record.event?.type ?? record.event?.kind ?? '事件', 100)}`}
                    </summary>
                    <pre>{recordText(record.event)}</pre>
                  </details>
                ))}
              </div>
            </details>
          </div>
        </div>

        {/* Projects Management View (#projects-page) */}
        <section
          id="projects-page"
          className="wSkVaW_root"
          hidden={s.activePage !== 'projects'}
          aria-labelledby="projects-heading"
          style={{ padding: 24, overflowY: 'auto' }}
        >
          <div
            style={{
              maxWidth: 748,
              width: '100%',
              margin: '0 auto',
              display: 'flex',
              flexDirection: 'column',
              gap: 16,
            }}
          >
            <div
              style={{
                display: 'flex',
                justifyContent: 'space-between',
                alignItems: 'center',
              }}
            >
              <h2 id="projects-heading" style={{ margin: 0, fontSize: 18 }}>
                工作区与本机项目空间
              </h2>
              <div style={{ display: 'flex', gap: 8 }}>
                <Button variant="primary" onClick={() => openNewProjectModal()}>
                  添加工作区
                </Button>
                <Button
                  ref={bind({ click: () => trackAsync(refreshProjects()) })}
                  id="refresh-projects"
                  variant="outline"
                  disabled={s.refreshProjectsBusy}
                  onClick={() => trackAsync(refreshProjects())}
                >
                  刷新空间
                </Button>
              </div>
            </div>
            <p id="projects-status" role="status" style={{ margin: 0 }}>
              {projectsStatusText}
            </p>
            <form
              ref={bind({ submit: e => trackAsync(handleAddProject(e)) })}
              id="add-project-form"
              style={{ display: 'flex', gap: 8 }}
              onSubmit={e => trackAsync(handleAddProject(e))}
            >
              <input
                ref={bind(
                  {
                    input: e => {
                      s.projectWorkspaceInput = e.currentTarget.value;
                      forceRender();
                    },
                  },
                  projectWsInputRef,
                )}
                id="project-workspace-input"
                type="text"
                className="zGbnIq_input"
                placeholder="工作区目录路径，如 /Volumes/ORICO/CAYW"
                value={s.projectWorkspaceInput}
                onChange={e => {
                  s.projectWorkspaceInput = e.target.value;
                  forceRender();
                }}
              />
              <input
                ref={bind(
                  {
                    input: e => {
                      s.projectLabelInput = e.currentTarget.value;
                      forceRender();
                    },
                  },
                  projectLabelInputRef,
                )}
                id="project-label-input"
                type="text"
                className="zGbnIq_input"
                placeholder="显示名称（可选）"
                value={s.projectLabelInput}
                onChange={e => {
                  s.projectLabelInput = e.target.value;
                  forceRender();
                }}
              />
              <Button id="add-project-button" type="submit" variant="primary">
                开启项目
              </Button>
            </form>
            <div id="project-list" className="zGbnIq_rows">
              {projectSummaries.map(project => {
                const isCurrent = project.id === currentProjectId();
                return (
                  <article key={project.id} className="zGbnIq_rowCard">
                    <div className="zGbnIq_rowHead">
                      <div className="zGbnIq_rowIdentity">
                        <span className="zGbnIq_credentialDot zGbnIq_credentialDotConfigured" />
                        <span className="zGbnIq_rowName">{project.label}</span>
                        <span className="zGbnIq_rowTag">
                          {project.statusLabel}
                        </span>
                      </div>
                    </div>
                    <div style={{ fontSize: 13, opacity: 0.75 }}>
                      {`${project.address}${project.workspace ? ` · ${project.workspace}` : ''} · ${project.sessionCount} 个会话`}
                    </div>
                    <div className="zGbnIq_rowActions">
                      <button
                        ref={bind({
                          click: () => trackAsync(selectProject(project.id)),
                        })}
                        type="button"
                        className="_button_kz6gm_4 _sm_kz6gm_30 _primary_kz6gm_38"
                        disabled={isCurrent}
                        onClick={() => trackAsync(selectProject(project.id))}
                      >
                        {isCurrent ? '当前项目空间' : '切换到此项目'}
                      </button>
                      {!project.default && project.id !== 'default' ? (
                        <button
                          ref={bind({
                            click: () => trackAsync(removeProject(project.id)),
                          })}
                          type="button"
                          className="_button_kz6gm_4 _sm_kz6gm_30 _outline_kz6gm_56"
                          onClick={() => trackAsync(removeProject(project.id))}
                        >
                          移除空间
                        </button>
                      ) : null}
                    </div>
                  </article>
                );
              })}
            </div>
          </div>
        </section>

        {/* Permissions & Audit View (#permissions-page) */}
        <section
          id="permissions-page"
          className="wSkVaW_root"
          hidden={s.activePage !== 'permissions'}
          aria-labelledby="permissions-heading"
          style={{ padding: 24, overflowY: 'auto' }}
        >
          <div
            style={{
              maxWidth: 748,
              width: '100%',
              margin: '0 auto',
              display: 'flex',
              flexDirection: 'column',
              gap: 16,
            }}
          >
            <div
              style={{
                display: 'flex',
                justifyContent: 'space-between',
                alignItems: 'center',
              }}
            >
              <h2 id="permissions-heading" style={{ margin: 0, fontSize: 18 }}>
                权限模式、任务授权与审计记录
              </h2>
              <Button
                ref={bind({ click: () => trackAsync(refreshPermissions()) })}
                id="refresh-permissions"
                variant="outline"
                disabled={s.refreshPermissionsBusy}
                onClick={() => trackAsync(refreshPermissions())}
              >
                刷新授权记录
              </Button>
            </div>
            <p id="permissions-status" role="status" style={{ margin: 0 }}>
              {s.permissionsStatus || `当前模式：${currentPresetObj.label}`}
            </p>
            <div id="grants-list" className="zGbnIq_rows">
              {s.sessionGrants.map((grant, idx) => {
                const cap =
                  grant.capability ?? grant.input?.capability ?? '未知能力';
                const scopeStr =
                  typeof grant.scope === 'string'
                    ? grant.scope
                    : JSON.stringify(grant.scope ?? {});
                const key = `${cap}:${scopeStr}`;
                return (
                  <div key={`${key}/${idx}`} className="zGbnIq_rowCard">
                    <div className="zGbnIq_rowHead">
                      <div className="zGbnIq_rowIdentity">
                        <span className="zGbnIq_rowName">
                          {CAPABILITY_LABELS[cap] ?? cap}
                        </span>
                        <span className="zGbnIq_rowTag">{scopeStr}</span>
                      </div>
                      <Button
                        variant="outline"
                        size="sm"
                        disabled={s.revokeBusyKey === key}
                        onClick={() => trackAsync(revokeGrant(grant))}
                      >
                        撤销授权
                      </Button>
                    </div>
                  </div>
                );
              })}
            </div>
          </div>
        </section>

        {/* Plugins View (#plugins-page) */}
        <section
          id="plugins-page"
          className="wSkVaW_root"
          hidden={s.activePage !== 'plugins'}
          aria-labelledby="plugins-heading"
          style={{ padding: 24, overflowY: 'auto' }}
        >
          <div
            style={{
              maxWidth: 748,
              width: '100%',
              margin: '0 auto',
              display: 'flex',
              flexDirection: 'column',
              gap: 16,
            }}
          >
            <div
              style={{
                display: 'flex',
                justifyContent: 'space-between',
                alignItems: 'center',
              }}
            >
              <h2 id="plugins-heading" style={{ margin: 0, fontSize: 18 }}>
                插件与能力目录
              </h2>
              <Button
                ref={bind({ click: () => trackAsync(refreshPlugins()) })}
                id="refresh-plugins"
                variant="outline"
                disabled={s.refreshPluginsBusy}
                onClick={() => trackAsync(refreshPlugins())}
              >
                刷新插件状态
              </Button>
            </div>
            <p id="plugins-status" role="status" style={{ margin: 0 }}>
              {pluginsStatusText}
            </p>
            <div id="plugin-list" className="qSYn7G_cards">
              {pluginItems.map(plugin => (
                <article key={plugin.name} className="qSYn7G_card">
                  <div className="qSYn7G_cardContent">
                    <span className="qSYn7G_cardTitle">{plugin.name}</span>
                    <span className="qSYn7G_cardTrailing">
                      <span className="qSYn7G_status">
                        <StateDot
                          state={plugin.frozen ? 'warning' : 'done'}
                          className="qSYn7G_statusDot"
                        />
                        {plugin.frozen ? '插件已卸载 · 会话冻结' : '已挂载'}
                      </span>
                    </span>
                  </div>
                  {array(plugin.reasons).map((reason, rIdx) => (
                    <div key={rIdx} className="qSYn7G_failure">
                      {reason}
                    </div>
                  ))}
                </article>
              ))}
            </div>
          </div>
        </section>
      </main>

      {/* Official DSH Settings Modal (@deepseek-ai/dsh-client-ui-settings-general) */}
      {s.settingsOpen ? (
        <div className="VOzbGW_overlay" role="presentation">
          <div
            className="VOzbGW_mask"
            aria-hidden="true"
            onClick={() => {
              s.settingsOpen = false;
              forceRender();
            }}
          />
          <div
            id="settings-dialog"
            className="VOzbGW_panel"
            role="dialog"
            aria-modal="true"
            aria-labelledby="settings-title"
          >
            <nav className="VOzbGW_nav">
              <div id="settings-title" className="VOzbGW_navTitle">
                设置
              </div>
              <div className="VOzbGW_navList">
                {[
                  ['general', '通用设置', IconSettingsOutline16],
                  ['connection', '连接与账号', IconPersonalizationOutline16],
                  ['models', '模型服务', IconDataOutline16],
                  ['agent-presets', '会话模式', IconAgentPresetOutline16],
                  ['mcp', '外部工具', IconCodeOutline16],
                  ['skills', '本地技能', IconPersonalizationOutline16],
                  ['permissions', '权限与审计', IconPersonalizationOutline16],
                  ['plugins', '扩展能力', IconPersonalizationOutline16],
                ].map(([tabId, label, IconComp]) => (
                  <button
                    key={tabId}
                    type="button"
                    className={clsx(
                      'VOzbGW_navCell',
                      s.settingsTab === tabId && 'VOzbGW_active',
                    )}
                    aria-current={s.settingsTab === tabId ? 'true' : undefined}
                    onClick={() => openSettings(tabId)}
                  >
                    <IconComp className="VOzbGW_navIcon" size={16} />
                    <span className="VOzbGW_navLabel">{label}</span>
                  </button>
                ))}
              </div>
            </nav>

            <div className="VOzbGW_content">
              <div className="VOzbGW_header">
                <div className="VOzbGW_actions" />
                <button
                  id="close-settings-button"
                  type="button"
                  className="VOzbGW_close"
                  aria-label="关闭"
                  onClick={() => {
                    s.settingsOpen = false;
                    forceRender();
                  }}
                >
                  <IconCloseOutline16 size={14} />
                </button>
              </div>

              <div className="VOzbGW_options">
                {s.settingsTab === 'general' ? (
                  <div className="_WvWnq_section">
                    <div className="_8HJdBW_group">
                      <div className="_8HJdBW_title">外观</div>
                      <div className="_8HJdBW_cubeRow">
                        {[
                          ['light', '浅色', IconLightOutline16],
                          ['dark', '深色', IconDarkOutline16],
                          ['system', '跟随系统', IconFollowsystemOutline16],
                        ].map(([modeId, modeLabel, ModeIcon]) => (
                          <button
                            key={modeId}
                            type="button"
                            className={clsx(
                              '_8HJdBW_themeCube',
                              s.themeMode === modeId && '_8HJdBW_selected',
                            )}
                            onClick={() => applyTheme(modeId)}
                          >
                            <ModeIcon size={16} />
                            <span>{modeLabel}</span>
                          </button>
                        ))}
                      </div>
                    </div>

                    <div className="oY77xG_row">
                      <div className="oY77xG_rowText">
                        <div className="oY77xG_title">默认权限预设</div>
                        <div className="oY77xG_desc">
                          {currentPresetObj.desc}
                        </div>
                      </div>
                      <button
                        type="button"
                        className="oY77xG_selector"
                        onClick={() => {
                          const idx = PERMISSION_PRESETS.findIndex(
                            p => p.id === s.permissionPreset,
                          );
                          const next =
                            PERMISSION_PRESETS[
                              (idx + 1) % PERMISSION_PRESETS.length
                            ];
                          selectPermissionPreset(next.id);
                        }}
                      >
                        <span>{currentPresetObj.label}</span>
                        <IconChevronDownOutline14 className="oY77xG_chevron" />
                      </button>
                    </div>

                    <div className="T1PP_q_row">
                      <div className="T1PP_q_rowText">
                        <div className="T1PP_q_title">发送快捷键</div>
                        <div className="T1PP_q_desc">
                          配置输入框按下回车键时的默认发送方式。
                        </div>
                      </div>
                      <button
                        type="button"
                        className="T1PP_q_selector"
                        onClick={() => {
                          s.enterBehavior =
                            s.enterBehavior === 'enter' ? 'cmd-enter' : 'enter';
                          forceRender();
                        }}
                      >
                        <span>
                          {s.enterBehavior === 'enter'
                            ? '按回车直接发送'
                            : '按组合键发送'}
                        </span>
                        <IconChevronDownOutline14 className="T1PP_q_chevron" />
                      </button>
                    </div>
                  </div>
                ) : null}

                {s.settingsTab === 'connection' ? (
                  <div className="zGbnIq_section">
                    <h3 className="zGbnIq_title">连接架构与账号安全</h3>
                    <p className="zGbnIq_intro">
                      管理本机免令牌直连、远程 Server 与 Device 的一次性 RSA 加密绑定，以及远程 Server 的登录账号与密码。
                    </p>

                    {/* Mode Switcher */}
                    <div className="zGbnIq_rowCard" style={{ marginBottom: 14 }}>
                      <div className="zGbnIq_rowHead">
                        <div className="zGbnIq_rowIdentity">
                          <span className="zGbnIq_credentialDot zGbnIq_credentialDotConfigured" />
                          <span className="zGbnIq_rowName">当前前端连接通道</span>
                          <span className="zGbnIq_rowTag">
                            {s.connectionMode === 'local_device'
                              ? 'Web ↔ Device 本机直连（无令牌）'
                              : 'Web ↔ Server 远程账号登录'}
                          </span>
                        </div>
                        <div className="zGbnIq_rowActions">
                          <button
                            type="button"
                            className={
                              s.connectionMode === 'local_device'
                                ? 'zGbnIq_primaryButton'
                                : 'zGbnIq_secondaryButton'
                            }
                            onClick={() => void switchConnectionMode('local_device')}
                          >
                            Web ↔ Device 本机直连
                          </button>
                          <button
                            type="button"
                            className={
                              s.connectionMode === 'remote_server'
                                ? 'zGbnIq_primaryButton'
                                : 'zGbnIq_secondaryButton'
                            }
                            onClick={() => void switchConnectionMode('remote_server')}
                          >
                            Web ↔ Server 远程连接
                          </button>
                        </div>
                      </div>
                      <p className="At1oFq_hint" style={{ margin: '6px 0 0' }}>
                        1. Web 与本机 Device 连接永远不使用、不出现任何令牌，打开页面即刻直连。
                      </p>
                    </div>

                    {/* Rule 2: Server <-> Device One-time RSA Binding & Multi-Device Management */}
                    <div className="zGbnIq_rowCard" style={{ marginBottom: 14 }}>
                      <div className="zGbnIq_rowHead">
                        <div className="zGbnIq_rowIdentity">
                          <span className="zGbnIq_credentialDot zGbnIq_credentialDotConfigured" />
                          <span className="zGbnIq_rowName">
                            {`Server ↔ Device 自动绑定（已绑定 ${s.boundDevices.length} 台设备）`}
                          </span>
                          <span className="zGbnIq_rowTag">
                            {s.deviceBinding.bound
                              ? 'RSA 自动加密连接中'
                              : '待连接'}
                          </span>
                        </div>
                        <div className="zGbnIq_rowActions">
                          <button
                            id="settings-add-device-btn"
                            type="button"
                            className="zGbnIq_primaryButton"
                            onClick={() => openAddDeviceModal()}
                          >
                            + 添加设备
                          </button>
                        </div>
                      </div>
                      <p className="At1oFq_hint" style={{ margin: '6px 0 10px' }}>
                        2. 填写 Server 地址后即自动完成一次性 RSA 密钥绑定并持久化保存，后续连接全程自动通过 RSA 加密通信，无需再次处理。
                      </p>
                      {s.deviceBindingNotice ? (
                        <p className="zGbnIq_savedNotice" role="status">
                          {s.deviceBindingNotice}
                        </p>
                      ) : null}
                      <div className="zGbnIq_rows" style={{ marginBottom: 10 }}>
                        {s.boundDevices.map(dev => {
                          const isCurrent = dev.device_id === s.activeDeviceId;
                          return (
                            <div
                              key={dev.device_id}
                              className="zGbnIq_rowCard"
                              style={{
                                padding: '10px 12px',
                                background:
                                  'var(--dsw-alias-bg-layer-2, rgba(255,255,255,0.02))',
                              }}
                            >
                              <div className="zGbnIq_rowHead">
                                <div className="zGbnIq_rowIdentity">
                                  <span className="zGbnIq_credentialDot zGbnIq_credentialDotConfigured" />
                                  <span className="zGbnIq_rowName">
                                    {dev.label}
                                  </span>
                                  <span className="zGbnIq_rowTag">
                                    {isCurrent
                                      ? '当前设备 · RSA 加密已就绪'
                                      : '已自动绑定 · 自动重连'}
                                  </span>
                                </div>
                                <div className="zGbnIq_rowActions">
                                  <button
                                    type="button"
                                    className="zGbnIq_secondaryButton"
                                    disabled={isCurrent}
                                    onClick={() =>
                                      void handleSelectDevice(dev.device_id)
                                    }
                                  >
                                    {isCurrent ? '当前使用中' : '切换'}
                                  </button>
                                  {!dev.builtin ? (
                                    <button
                                      type="button"
                                      className="zGbnIq_dangerButton"
                                      onClick={() =>
                                        void handleRemoveDevice(dev.device_id)
                                      }
                                    >
                                      移除
                                    </button>
                                  ) : null}
                                </div>
                              </div>
                              <div
                                style={{
                                  fontSize: 12,
                                  color: 'var(--dsw-alias-label-tertiary)',
                                  marginTop: 4,
                                }}
                              >
                                {`Server 地址：${dev.server_url} · 已自动完成 RSA 密钥绑定`}
                              </div>
                            </div>
                          );
                        })}
                      </div>
                    </div>

                    {/* Rule 3: Web <-> Server Default Account Login & Password Change */}
                    <div className="zGbnIq_rowCard">
                      <div className="zGbnIq_rowHead">
                        <div className="zGbnIq_rowIdentity">
                          <span
                            className={clsx(
                              'zGbnIq_credentialDot',
                              s.serverLoggedIn
                                ? 'zGbnIq_credentialDotConfigured'
                                : 'zGbnIq_credentialDotMissing',
                            )}
                          />
                          <span className="zGbnIq_rowName">
                            Web ↔ Server 账号登录与密码管理
                          </span>
                          <span className="zGbnIq_rowTag">
                            {s.serverLoggedIn
                              ? `已登录（${s.serverUsername}）`
                              : `未登录 · 默认账号 ${s.serverUsername}`}
                          </span>
                        </div>
                        {s.serverLoggedIn ? (
                          <div className="zGbnIq_rowActions">
                            <button
                              type="button"
                              className="zGbnIq_secondaryButton"
                              onClick={() => {
                                s.serverEditingCredentials =
                                  !s.serverEditingCredentials;
                                s.serverNewUserDraft = s.serverUsername;
                                s.serverCurrentPassDraft = '';
                                s.serverNewPassDraft = '';
                                s.serverConfirmPassDraft = '';
                                s.serverLoginError = '';
                                forceRender();
                              }}
                            >
                              {s.serverEditingCredentials
                                ? '收起修改表单'
                                : '修改登录账号密码'}
                            </button>
                            <button
                              type="button"
                              className="zGbnIq_dangerButton"
                              onClick={() => void handleServerLogout()}
                            >
                              退出登录
                            </button>
                          </div>
                        ) : null}
                      </div>
                      <p className="At1oFq_hint" style={{ margin: '6px 0 10px' }}>
                        3. Web 与 Server 连接采用账号密码认证：首次使用默认初始账号（账号：admin / 密码：admin）直接登录，登录后可随时修改为自定义账号与密码。
                      </p>
                      {s.serverAccountNotice ? (
                        <p className="zGbnIq_savedNotice" role="status">
                          {s.serverAccountNotice}
                        </p>
                      ) : null}
                      {s.serverLoginError ? (
                        <p className="_banner_ugy7y_1" role="alert" style={{ margin: '0 0 10px' }}>
                          {s.serverLoginError}
                        </p>
                      ) : null}

                      {!s.serverLoggedIn ? (
                        <form
                          className="zGbnIq_editor"
                          onSubmit={e => void handleServerLogin(e)}
                        >
                          <div className="zGbnIq_editorHeader">
                            <span className="zGbnIq_editorTitle">
                              登录远程 Server
                            </span>
                            <span className="zGbnIq_editorRoute">
                              {s.serverIsDefaultCredentials
                                ? '初始默认账号：admin / 默认密码：admin'
                                : `当前账号：${s.serverUsername}`}
                            </span>
                          </div>
                          <div className="zGbnIq_modelAdvanced">
                            <div className="zGbnIq_field">
                              <label className="zGbnIq_fieldLabel">登录账号</label>
                              <input
                                id="server-login-username"
                                type="text"
                                className="zGbnIq_input"
                                placeholder="默认账号 admin"
                                value={s.serverLoginUserDraft}
                                onChange={e => {
                                  s.serverLoginUserDraft = e.target.value;
                                  forceRender();
                                }}
                              />
                            </div>
                            <div className="zGbnIq_field">
                              <label className="zGbnIq_fieldLabel">登录密码</label>
                              <input
                                id="server-login-password"
                                type="password"
                                className="zGbnIq_input"
                                placeholder="默认密码 admin"
                                value={s.serverLoginPassDraft}
                                onChange={e => {
                                  s.serverLoginPassDraft = e.target.value;
                                  forceRender();
                                }}
                              />
                            </div>
                          </div>
                          <div className="zGbnIq_editorActions">
                            {s.serverIsDefaultCredentials ? (
                              <button
                                type="button"
                                className="zGbnIq_secondaryButton"
                                onClick={() => {
                                  s.serverLoginUserDraft = 'admin';
                                  s.serverLoginPassDraft = 'admin';
                                  forceRender();
                                }}
                              >
                                填入默认账号 (admin / admin)
                              </button>
                            ) : null}
                            <button
                              id="server-login-submit"
                              type="submit"
                              className="zGbnIq_primaryButton"
                              disabled={s.serverLoginBusy}
                            >
                              {s.serverLoginBusy ? '正在登录…' : '登录 Server'}
                            </button>
                          </div>
                        </form>
                      ) : s.serverEditingCredentials ? (
                        <form
                          className="zGbnIq_editor"
                          onSubmit={e => void handleServerUpdateCredentials(e)}
                        >
                          <div className="zGbnIq_editorHeader">
                            <span className="zGbnIq_editorTitle">
                              修改 Server 登录账号与密码
                            </span>
                          </div>
                          <div className="zGbnIq_modelAdvanced">
                            <div className="zGbnIq_field">
                              <label className="zGbnIq_fieldLabel">当前密码</label>
                              <input
                                id="server-current-password"
                                type="password"
                                className="zGbnIq_input"
                                placeholder="输入当前登录密码（默认 admin）"
                                value={s.serverCurrentPassDraft}
                                onChange={e => {
                                  s.serverCurrentPassDraft = e.target.value;
                                  forceRender();
                                }}
                              />
                            </div>
                            <div className="zGbnIq_field">
                              <label className="zGbnIq_fieldLabel">新登录账号</label>
                              <input
                                id="server-new-username"
                                type="text"
                                className="zGbnIq_input"
                                placeholder="输入新登录账号名称"
                                value={s.serverNewUserDraft}
                                onChange={e => {
                                  s.serverNewUserDraft = e.target.value;
                                  forceRender();
                                }}
                              />
                            </div>
                          </div>
                          <div className="zGbnIq_modelAdvanced">
                            <div className="zGbnIq_field">
                              <label className="zGbnIq_fieldLabel">新登录密码</label>
                              <input
                                id="server-new-password"
                                type="password"
                                className="zGbnIq_input"
                                placeholder="输入新登录密码"
                                value={s.serverNewPassDraft}
                                onChange={e => {
                                  s.serverNewPassDraft = e.target.value;
                                  forceRender();
                                }}
                              />
                            </div>
                            <div className="zGbnIq_field">
                              <label className="zGbnIq_fieldLabel">确认新密码</label>
                              <input
                                id="server-confirm-password"
                                type="password"
                                className="zGbnIq_input"
                                placeholder="再次输入新登录密码"
                                value={s.serverConfirmPassDraft}
                                onChange={e => {
                                  s.serverConfirmPassDraft = e.target.value;
                                  forceRender();
                                }}
                              />
                            </div>
                          </div>
                          <div className="zGbnIq_editorActions">
                            <button
                              type="button"
                              className="zGbnIq_secondaryButton"
                              onClick={() => {
                                s.serverEditingCredentials = false;
                                forceRender();
                              }}
                            >
                              取消
                            </button>
                            <button
                              id="server-save-credentials"
                              type="submit"
                              className="zGbnIq_primaryButton"
                            >
                              保存新账号密码
                            </button>
                          </div>
                        </form>
                      ) : null}
                    </div>
                  </div>
                ) : null}

                {s.settingsTab === 'models' ? (
                  <div className="zGbnIq_section">
                    <h3 className="zGbnIq_title">模型服务配置</h3>
                    <p className="zGbnIq_intro">
                      管理模型服务地址、接口密钥与可用模型目录。密钥仅保存在本机。
                    </p>
                    {s.modelNotice ? (
                      <p className="zGbnIq_savedNotice" role="status">
                        {s.modelNotice}
                      </p>
                    ) : null}
                    <div className="zGbnIq_rows">
                      {s.modelProviders.map(provider => {
                        const isEditing = s.editingProviderId === provider.id;
                        const isAddingModel =
                          s.addingModelProviderId === provider.id;
                        return (
                          <div key={provider.id} className="zGbnIq_rowCard">
                            <div className="zGbnIq_rowHead">
                              <div className="zGbnIq_rowIdentity">
                                <span
                                  className={clsx(
                                    'zGbnIq_credentialDot',
                                    provider.configured
                                      ? 'zGbnIq_credentialDotConfigured'
                                      : 'zGbnIq_credentialDotMissing',
                                  )}
                                />
                                <span className="zGbnIq_rowName">
                                  {provider.name}
                                </span>
                                <span className="zGbnIq_rowTag">
                                  {provider.configured
                                    ? '密钥已配置'
                                    : '未配置密钥'}
                                </span>
                              </div>
                              <div className="zGbnIq_rowActions">
                                <button
                                  type="button"
                                  className="zGbnIq_secondaryButton"
                                  onClick={() => {
                                    if (isEditing) {
                                      s.editingProviderId = null;
                                    } else {
                                      s.editingProviderId = provider.id;
                                      s.editProviderBaseUrl = provider.baseUrl;
                                      s.editProviderApiKey = '';
                                    }
                                    forceRender();
                                  }}
                                >
                                  {isEditing ? '收起配置' : '配置密钥与地址'}
                                </button>
                                {!provider.builtin ? (
                                  <button
                                    type="button"
                                    className="zGbnIq_dangerButton"
                                    onClick={() => {
                                      s.modelProviders =
                                        s.modelProviders.filter(
                                          p => p.id !== provider.id,
                                        );
                                      s.modelNotice = `已移除服务“${provider.name}”。`;
                                      forceRender();
                                    }}
                                  >
                                    移除
                                  </button>
                                ) : null}
                              </div>
                            </div>

                            {isEditing ? (
                              <div className="zGbnIq_editor">
                                <div className="zGbnIq_editorHeader">
                                  <span className="zGbnIq_editorTitle">
                                    连接与凭证设置
                                  </span>
                                  <span className="zGbnIq_editorRoute">
                                    {provider.protocol}
                                  </span>
                                </div>
                                <div className="zGbnIq_field">
                                  <label className="zGbnIq_fieldLabel">
                                    接口服务地址
                                  </label>
                                  <input
                                    type="text"
                                    className="zGbnIq_input"
                                    placeholder="如 http://127.0.0.1:3425/v1"
                                    value={s.editProviderBaseUrl}
                                    onChange={e => {
                                      s.editProviderBaseUrl = e.target.value;
                                      forceRender();
                                    }}
                                  />
                                </div>
                                <div className="zGbnIq_field">
                                  <label className="zGbnIq_fieldLabel">
                                    接口密钥
                                  </label>
                                  <input
                                    type="password"
                                    className="zGbnIq_input"
                                    placeholder={
                                      provider.configured
                                        ? '已配置本机密钥（输入新密钥可覆盖）'
                                        : '输入模型服务密钥'
                                    }
                                    value={s.editProviderApiKey}
                                    onChange={e => {
                                      s.editProviderApiKey = e.target.value;
                                      forceRender();
                                    }}
                                  />
                                </div>
                                <div className="zGbnIq_editorActions">
                                  <button
                                    type="button"
                                    className="zGbnIq_secondaryButton"
                                    onClick={() => {
                                      s.editingProviderId = null;
                                      forceRender();
                                    }}
                                  >
                                    取消
                                  </button>
                                  <button
                                    type="button"
                                    className="zGbnIq_primaryButton"
                                    onClick={() => {
                                      provider.baseUrl =
                                        s.editProviderBaseUrl.trim() ||
                                        provider.baseUrl;
                                      if (s.editProviderApiKey.trim()) {
                                        provider.configured = true;
                                      }
                                      s.editingProviderId = null;
                                      s.modelNotice = `已保存“${provider.name}”连接配置。`;
                                      forceRender();
                                    }}
                                  >
                                    保存配置
                                  </button>
                                </div>
                              </div>
                            ) : null}

                            <div className="zGbnIq_modelCatalog">
                              <div className="zGbnIq_modelListHead">
                                <div className="zGbnIq_modelCatalogHeading">
                                  <span className="zGbnIq_modelCatalogTitle">
                                    {`可用模型（${provider.models.length}）`}
                                  </span>
                                  <span className="zGbnIq_modelCatalogMeta">
                                    {provider.baseUrl}
                                  </span>
                                </div>
                                <button
                                  type="button"
                                  className="zGbnIq_addModelButton"
                                  onClick={() => {
                                    s.addingModelProviderId = isAddingModel
                                      ? null
                                      : provider.id;
                                    s.newModelId = '';
                                    s.newModelDesc = '';
                                    forceRender();
                                  }}
                                >
                                  <IconPlusOutline16 size={12} />
                                  <span>添加模型</span>
                                </button>
                              </div>

                              {isAddingModel ? (
                                <div className="zGbnIq_editor">
                                  <div className="zGbnIq_modelAdvanced">
                                    <div className="zGbnIq_modelField">
                                      <label className="zGbnIq_modelFieldLabel">
                                        模型标识
                                      </label>
                                      <input
                                        type="text"
                                        className="zGbnIq_input"
                                        placeholder="如 deepseek-coder-v2"
                                        value={s.newModelId}
                                        onChange={e => {
                                          s.newModelId = e.target.value;
                                          forceRender();
                                        }}
                                      />
                                    </div>
                                    <div className="zGbnIq_modelField">
                                      <label className="zGbnIq_modelFieldLabel">
                                        用途说明
                                      </label>
                                      <input
                                        type="text"
                                        className="zGbnIq_input"
                                        placeholder="如 代码补全与重构"
                                        value={s.newModelDesc}
                                        onChange={e => {
                                          s.newModelDesc = e.target.value;
                                          forceRender();
                                        }}
                                      />
                                    </div>
                                  </div>
                                  <div className="zGbnIq_editorActions">
                                    <button
                                      type="button"
                                      className="zGbnIq_secondaryButton"
                                      onClick={() => {
                                        s.addingModelProviderId = null;
                                        forceRender();
                                      }}
                                    >
                                      取消
                                    </button>
                                    <button
                                      type="button"
                                      className="zGbnIq_primaryButton"
                                      disabled={!s.newModelId.trim()}
                                      onClick={() => {
                                        const id = s.newModelId.trim();
                                        if (!id) return;
                                        provider.models.push({
                                          id,
                                          label: id,
                                          desc:
                                            s.newModelDesc.trim() ||
                                            '自定义模型',
                                        });
                                        s.selectedModel = id;
                                        s.addingModelProviderId = null;
                                        s.modelNotice = `已添加并切换到模型 ${id}。`;
                                        forceRender();
                                      }}
                                    >
                                      确认添加
                                    </button>
                                  </div>
                                </div>
                              ) : null}

                              <div className="zGbnIq_modelList">
                                {provider.models.map(m => {
                                  const isCurrent = s.selectedModel === m.id;
                                  return (
                                    <div
                                      key={m.id}
                                      className="zGbnIq_modelEntry"
                                    >
                                      <div className="zGbnIq_modelRow">
                                        <strong
                                          style={{
                                            fontSize: 13,
                                            paddingLeft: 6,
                                            overflow: 'hidden',
                                            textOverflow: 'ellipsis',
                                            whiteSpace: 'nowrap',
                                          }}
                                        >
                                          {m.label || m.id}
                                        </strong>
                                        <span
                                          className="zGbnIq_modelCatalogMeta"
                                          style={{
                                            overflow: 'hidden',
                                            textOverflow: 'ellipsis',
                                            whiteSpace: 'nowrap',
                                          }}
                                        >
                                          {m.desc}
                                        </span>
                                        <button
                                          type="button"
                                          className="zGbnIq_addModelButton"
                                          style={
                                            isCurrent
                                              ? {
                                                  borderColor:
                                                    'var(--dsw-alias-state-business-primary)',
                                                  color:
                                                    'var(--dsw-alias-state-business-primary)',
                                                }
                                              : undefined
                                          }
                                          onClick={() => {
                                            s.selectedModel = m.id;
                                            s.modelNotice = `当前默认模型已切换为 ${m.id}。`;
                                            forceRender();
                                          }}
                                        >
                                          {isCurrent ? '当前默认' : '设为默认'}
                                        </button>
                                        <button
                                          type="button"
                                          className="zGbnIq_iconButton zGbnIq_iconButtonDanger"
                                          aria-label={`移除模型 ${m.id}`}
                                          title="移除模型"
                                          disabled={provider.models.length <= 1}
                                          onClick={() => {
                                            provider.models =
                                              provider.models.filter(
                                                item => item.id !== m.id,
                                              );
                                            if (s.selectedModel === m.id) {
                                              s.selectedModel =
                                                provider.models[0]?.id ??
                                                'deepseek-v4.1-flash';
                                            }
                                            forceRender();
                                          }}
                                        >
                                          <IconCloseOutline16 size={13} />
                                        </button>
                                      </div>
                                    </div>
                                  );
                                })}
                              </div>
                            </div>
                          </div>
                        );
                      })}
                    </div>

                    <div className="zGbnIq_addBlock">
                      {!s.addingProviderOpen ? (
                        <div className="zGbnIq_addActions">
                          <button
                            type="button"
                            className="zGbnIq_addButton"
                            onClick={() => {
                              s.addingProviderOpen = true;
                              s.newProviderName = '';
                              s.newProviderBaseUrl = '';
                              s.newProviderApiKey = '';
                              s.newProviderModelId = '';
                              forceRender();
                            }}
                          >
                            <IconPlusOutline16 size={14} />
                            <span>添加自定义模型服务</span>
                          </button>
                        </div>
                      ) : (
                        <div className="zGbnIq_addCard">
                          <div className="zGbnIq_editor">
                            <div className="zGbnIq_editorHeader">
                              <span className="zGbnIq_editorTitle">
                                新增兼容模型服务
                              </span>
                            </div>
                            <div className="zGbnIq_modelAdvanced">
                              <div className="zGbnIq_field">
                                <label className="zGbnIq_fieldLabel">
                                  服务名称
                                </label>
                                <input
                                  type="text"
                                  className="zGbnIq_input"
                                  placeholder="如 本地推理服务"
                                  value={s.newProviderName}
                                  onChange={e => {
                                    s.newProviderName = e.target.value;
                                    forceRender();
                                  }}
                                />
                              </div>
                              <div className="zGbnIq_field">
                                <label className="zGbnIq_fieldLabel">
                                  默认模型标识
                                </label>
                                <input
                                  type="text"
                                  className="zGbnIq_input"
                                  placeholder="如 qwen-coder-plus"
                                  value={s.newProviderModelId}
                                  onChange={e => {
                                    s.newProviderModelId = e.target.value;
                                    forceRender();
                                  }}
                                />
                              </div>
                            </div>
                            <div className="zGbnIq_field">
                              <label className="zGbnIq_fieldLabel">
                                接口服务地址
                              </label>
                              <input
                                type="text"
                                className="zGbnIq_input"
                                placeholder="如 http://127.0.0.1:8000/v1"
                                value={s.newProviderBaseUrl}
                                onChange={e => {
                                  s.newProviderBaseUrl = e.target.value;
                                  forceRender();
                                }}
                              />
                            </div>
                            <div className="zGbnIq_field">
                              <label className="zGbnIq_fieldLabel">
                                接口密钥
                              </label>
                              <input
                                type="password"
                                className="zGbnIq_input"
                                placeholder="输入接口密钥（可选）"
                                value={s.newProviderApiKey}
                                onChange={e => {
                                  s.newProviderApiKey = e.target.value;
                                  forceRender();
                                }}
                              />
                            </div>
                            <div className="zGbnIq_editorActions">
                              <button
                                type="button"
                                className="zGbnIq_secondaryButton"
                                onClick={() => {
                                  s.addingProviderOpen = false;
                                  forceRender();
                                }}
                              >
                                取消
                              </button>
                              <button
                                type="button"
                                className="zGbnIq_primaryButton"
                                disabled={
                                  !s.newProviderName.trim() ||
                                  !s.newProviderBaseUrl.trim()
                                }
                                onClick={() => {
                                  const modelId =
                                    s.newProviderModelId.trim() ||
                                    'default-model';
                                  s.modelProviders.push({
                                    id: `custom-${Date.now()}`,
                                    name: s.newProviderName.trim(),
                                    protocol: '兼容接口',
                                    baseUrl: s.newProviderBaseUrl.trim(),
                                    configured: Boolean(
                                      s.newProviderApiKey.trim(),
                                    ),
                                    builtin: false,
                                    models: [
                                      {
                                        id: modelId,
                                        label: modelId,
                                        desc: '自定义接入模型',
                                      },
                                    ],
                                  });
                                  s.selectedModel = modelId;
                                  s.addingProviderOpen = false;
                                  s.modelNotice = `已添加模型服务“${s.newProviderName.trim()}”。`;
                                  forceRender();
                                }}
                              >
                                保存并启用
                              </button>
                            </div>
                          </div>
                        </div>
                      )}
                    </div>
                  </div>
                ) : null}

                {s.settingsTab === 'agent-presets' ? (
                  <div className="rtSEdW_section">
                    <h3 className="rtSEdW_title">会话模式</h3>
                    <p className="rtSEdW_intro">
                      选择新建会话默认使用的工具编排与执行方式。
                    </p>
                    <ul className="rtSEdW_cards">
                      {AGENT_PRESETS.map(ap => {
                        const active = s.selectedAgentPreset === ap.id;
                        return (
                          <li
                            key={ap.id}
                            className={clsx(
                              'rtSEdW_card',
                              active && 'rtSEdW_cardActive',
                            )}
                          >
                            <button
                              type="button"
                              className="rtSEdW_cardMain"
                              onClick={() => {
                                s.selectedAgentPreset = ap.id;
                                forceRender();
                              }}
                            >
                              <div className="rtSEdW_cardHead">
                                <span className="rtSEdW_cardName">
                                  {ap.name}
                                </span>
                                <span className="rtSEdW_badge">{ap.badge}</span>
                                {active ? (
                                  <span className="rtSEdW_inUse">当前默认</span>
                                ) : null}
                              </div>
                              <div className="rtSEdW_cardDesc">{ap.desc}</div>
                            </button>
                          </li>
                        );
                      })}
                    </ul>
                  </div>
                ) : null}

                {s.settingsTab === 'mcp' ? (
                  <div className="pbvGtq_section">
                    <h3 className="pbvGtq_heading">外部工具服务</h3>
                    <p className="pbvGtq_intro">
                      由宿主统一接入本机或远程工具协议服务，所有工具调用均经过会话权限边界与审计记录。
                    </p>
                    {s.mcpNotice ? (
                      <p className="zGbnIq_savedNotice" role="status">
                        {s.mcpNotice}
                      </p>
                    ) : null}
                    <ul className="pbvGtq_cards">
                      {s.mcpServers.map(srv => {
                        const isOpen = Boolean(s.expandedMcpCards[srv.id]);
                        return (
                          <li
                            key={srv.id}
                            className={clsx(
                              'YyYd_a_card',
                              isOpen && 'YyYd_a_cardOpen',
                            )}
                          >
                            <button
                              type="button"
                              className="YyYd_a_header"
                              aria-expanded={isOpen}
                              onClick={() => {
                                s.expandedMcpCards[srv.id] = !isOpen;
                                forceRender();
                              }}
                            >
                              <div className="YyYd_a_headText">
                                <span className="YyYd_a_name">{srv.name}</span>
                                <span className="YyYd_a_description">
                                  {`${srv.transport} · ${srv.endpoint}`}
                                </span>
                              </div>
                              <span
                                className="qSYn7G_configTag"
                                data-enabled={String(srv.enabled)}
                              >
                                {srv.enabled
                                  ? `${srv.statusLabel} · ${srv.tools.length} 个工具`
                                  : '已停用'}
                              </span>
                              <IconChevronDownOutline14
                                size={14}
                                className={clsx(
                                  'YyYd_a_chevron',
                                  isOpen && 'YyYd_a_chevronOpen',
                                )}
                              />
                            </button>
                            {isOpen ? (
                              <div className="YyYd_a_body">
                                {srv.tools.map(t => (
                                  <div key={t.name} className="At1oFq_field">
                                    <div className="At1oFq_head">
                                      <span className="At1oFq_label">
                                        {t.name}
                                      </span>
                                      <span className="At1oFq_badge">
                                        按需授权调用
                                      </span>
                                    </div>
                                    <p className="At1oFq_hint">{t.desc}</p>
                                  </div>
                                ))}
                                <div className="YyYd_a_footer">
                                  {!srv.builtin ? (
                                    <button
                                      type="button"
                                      className="zGbnIq_dangerButton"
                                      style={{ height: 30, fontSize: 12 }}
                                      onClick={() => {
                                        s.mcpServers = s.mcpServers.filter(
                                          item => item.id !== srv.id,
                                        );
                                        s.mcpNotice = `已移除工具服务“${srv.name}”。`;
                                        forceRender();
                                      }}
                                    >
                                      移除服务
                                    </button>
                                  ) : null}
                                  <button
                                    type="button"
                                    className="YyYd_a_discard"
                                    onClick={() => {
                                      srv.enabled = !srv.enabled;
                                      s.mcpNotice = srv.enabled
                                        ? `已启用“${srv.name}”。`
                                        : `已停用“${srv.name}”。`;
                                      forceRender();
                                    }}
                                  >
                                    {srv.enabled ? '暂停服务' : '启用服务'}
                                  </button>
                                  <button
                                    type="button"
                                    className="YyYd_a_save"
                                    onClick={() => {
                                      s.expandedMcpCards[srv.id] = false;
                                      forceRender();
                                    }}
                                  >
                                    完成
                                  </button>
                                </div>
                              </div>
                            ) : null}
                          </li>
                        );
                      })}
                    </ul>

                    <div className="zGbnIq_addBlock">
                      {!s.addingMcpOpen ? (
                        <div className="zGbnIq_addActions">
                          <button
                            type="button"
                            className="zGbnIq_addButton"
                            onClick={() => {
                              s.addingMcpOpen = true;
                              s.newMcpName = '';
                              s.newMcpEndpoint = '';
                              forceRender();
                            }}
                          >
                            <IconPlusOutline16 size={14} />
                            <span>添加外部工具服务</span>
                          </button>
                        </div>
                      ) : (
                        <div className="zGbnIq_addCard">
                          <div className="zGbnIq_editor">
                            <div className="zGbnIq_editorHeader">
                              <span className="zGbnIq_editorTitle">
                                新增外部工具服务
                              </span>
                            </div>
                            <div className="zGbnIq_modelAdvanced">
                              <div className="zGbnIq_field">
                                <label className="zGbnIq_fieldLabel">
                                  服务名称
                                </label>
                                <input
                                  type="text"
                                  className="zGbnIq_input"
                                  placeholder="如 本地数据库查询服务"
                                  value={s.newMcpName}
                                  onChange={e => {
                                    s.newMcpName = e.target.value;
                                    forceRender();
                                  }}
                                />
                              </div>
                              <div className="zGbnIq_field">
                                <label className="zGbnIq_fieldLabel">
                                  传输方式
                                </label>
                                <select
                                  className="zGbnIq_input zGbnIq_selectInput"
                                  value={s.newMcpTransport}
                                  onChange={e => {
                                    s.newMcpTransport = e.target.value;
                                    forceRender();
                                  }}
                                >
                                  <option value="本机标准输入输出">
                                    本机标准输入输出
                                  </option>
                                  <option value="远程事件流">
                                    远程事件流
                                  </option>
                                </select>
                              </div>
                            </div>
                            <div className="zGbnIq_field">
                              <label className="zGbnIq_fieldLabel">
                                启动命令或服务地址
                              </label>
                              <input
                                type="text"
                                className="zGbnIq_input"
                                placeholder="如 npx -y @modelcontextprotocol/server-sqlite"
                                value={s.newMcpEndpoint}
                                onChange={e => {
                                  s.newMcpEndpoint = e.target.value;
                                  forceRender();
                                }}
                              />
                            </div>
                            <div className="zGbnIq_editorActions">
                              <button
                                type="button"
                                className="zGbnIq_secondaryButton"
                                onClick={() => {
                                  s.addingMcpOpen = false;
                                  forceRender();
                                }}
                              >
                                取消
                              </button>
                              <button
                                type="button"
                                className="zGbnIq_primaryButton"
                                disabled={
                                  !s.newMcpName.trim() ||
                                  !s.newMcpEndpoint.trim()
                                }
                                onClick={() => {
                                  const id = `mcp-${Date.now()}`;
                                  s.mcpServers.push({
                                    id,
                                    name: s.newMcpName.trim(),
                                    transport: s.newMcpTransport,
                                    endpoint: s.newMcpEndpoint.trim(),
                                    status: 'active',
                                    statusLabel: '已连接',
                                    enabled: true,
                                    builtin: false,
                                    tools: [
                                      {
                                        name: 'default.call',
                                        desc: '按需发现并调用该服务提供的工具',
                                      },
                                    ],
                                  });
                                  s.addingMcpOpen = false;
                                  s.mcpNotice = `已添加外部工具服务“${s.newMcpName.trim()}”。`;
                                  forceRender();
                                }}
                              >
                                保存并连接
                              </button>
                            </div>
                          </div>
                        </div>
                      )}
                    </div>
                  </div>
                ) : null}

                {s.settingsTab === 'skills' ? (
                  <div className="pbvGtq_section">
                    <div
                      style={{
                        display: 'flex',
                        justifyContent: 'space-between',
                        alignItems: 'center',
                      }}
                    >
                      <h3 className="pbvGtq_heading">本地技能包</h3>
                      <button
                        type="button"
                        className="zGbnIq_addModelButton"
                        onClick={() => {
                          s.addingSkillOpen = !s.addingSkillOpen;
                          forceRender();
                        }}
                      >
                        <IconPlusOutline16 size={12} />
                        <span>新建技能说明</span>
                      </button>
                    </div>
                    <p className="pbvGtq_intro">
                      在输入框输入“/”或点击卡片即可将技能注入当前会话。点击任意技能可展开查看完整指令内容。
                    </p>
                    {s.skillNotice ? (
                      <p className="zGbnIq_savedNotice" role="status">
                        {s.skillNotice}
                      </p>
                    ) : null}

                    {s.addingSkillOpen ? (
                      <div className="zGbnIq_addCard">
                        <div className="zGbnIq_editor">
                          <div className="zGbnIq_modelAdvanced">
                            <div className="zGbnIq_field">
                              <label className="zGbnIq_fieldLabel">
                                技能标识
                              </label>
                              <input
                                type="text"
                                className="zGbnIq_input"
                                placeholder="如 rust-security-audit"
                                value={s.newSkillName}
                                onChange={e => {
                                  s.newSkillName = e.target.value;
                                  forceRender();
                                }}
                              />
                            </div>
                            <div className="zGbnIq_field">
                              <label className="zGbnIq_fieldLabel">
                                一句话用途说明
                              </label>
                              <input
                                type="text"
                                className="zGbnIq_input"
                                placeholder="如 检查不安全代码块与权限边界"
                                value={s.newSkillDesc}
                                onChange={e => {
                                  s.newSkillDesc = e.target.value;
                                  forceRender();
                                }}
                              />
                            </div>
                          </div>
                          <div className="zGbnIq_editorActions">
                            <button
                              type="button"
                              className="zGbnIq_secondaryButton"
                              onClick={() => {
                                s.addingSkillOpen = false;
                                forceRender();
                              }}
                            >
                              取消
                            </button>
                            <button
                              type="button"
                              className="zGbnIq_primaryButton"
                              disabled={!s.newSkillName.trim()}
                              onClick={() => {
                                const name = s.newSkillName.trim();
                                s.skillsCatalog.unshift({
                                  name,
                                  description:
                                    s.newSkillDesc.trim() || '自定义工作流技能',
                                  source: '自定义技能',
                                  path: `~/.codex/skills/${name}/SKILL.md`,
                                });
                                s.addingSkillOpen = false;
                                s.newSkillName = '';
                                s.newSkillDesc = '';
                                s.skillNotice = `已注册技能 /${name}。`;
                                forceRender();
                              }}
                            >
                              保存技能
                            </button>
                          </div>
                        </div>
                      </div>
                    ) : null}

                    <div className="qSYn7G_search">
                      <IconSearchOutline16 size={16} />
                      <input
                        type="text"
                        placeholder="搜索本地已安装的技能名称或描述…"
                        value={s.skillSearchQuery}
                        onChange={e => {
                          s.skillSearchQuery = e.target.value;
                          forceRender();
                        }}
                      />
                    </div>

                    <ul className="pbvGtq_cards">
                      {s.skillsCatalog
                        .filter(sk => {
                          const q = s.skillSearchQuery.trim().toLowerCase();
                          if (!q) return true;
                          return (
                            sk.name.toLowerCase().includes(q) ||
                            sk.description.toLowerCase().includes(q) ||
                            (sk.source || '').toLowerCase().includes(q)
                          );
                        })
                        .map(sk => {
                          const isOpen = Boolean(
                            s.expandedSkillCards[sk.name],
                          );
                          const detailText =
                            s.skillDetailsMap[sk.name] ||
                            sk.content ||
                            `# ${sk.name}\n\n${sk.description}\n\n来源路径：${sk.path || '本地技能库'}`;
                          return (
                            <li
                              key={sk.name}
                              className={clsx(
                                'YyYd_a_card',
                                isOpen && 'YyYd_a_cardOpen',
                              )}
                            >
                              <button
                                type="button"
                                className="YyYd_a_header"
                                aria-expanded={isOpen}
                                onClick={() => {
                                  const next = !isOpen;
                                  s.expandedSkillCards[sk.name] = next;
                                  if (next) void loadSkillDetail(sk.name);
                                  forceRender();
                                }}
                              >
                                <div className="YyYd_a_headText">
                                  <span className="YyYd_a_name">
                                    {`/${sk.name}`}
                                  </span>
                                  <span className="YyYd_a_description">
                                    {sk.description}
                                  </span>
                                </div>
                                <span className="At1oFq_badge">
                                  {sk.source || '本地技能库'}
                                </span>
                                <IconChevronDownOutline14
                                  size={14}
                                  className={clsx(
                                    'YyYd_a_chevron',
                                    isOpen && 'YyYd_a_chevronOpen',
                                  )}
                                />
                              </button>
                              {isOpen ? (
                                <div className="YyYd_a_body">
                                  <div
                                    className="iWrAna_instructionsCard"
                                    style={{ margin: '12px 0 8px' }}
                                  >
                                    <div className="iWrAna_instructionsHeader">
                                      {`技能定义文件 · ${sk.path || 'SKILL.md'}`}
                                    </div>
                                    <pre className="iWrAna_instructions">
                                      {detailText}
                                    </pre>
                                  </div>
                                  <div className="YyYd_a_footer">
                                    <button
                                      type="button"
                                      className="YyYd_a_discard"
                                      onClick={() => {
                                        s.expandedSkillCards[sk.name] = false;
                                        forceRender();
                                      }}
                                    >
                                      收起
                                    </button>
                                    <button
                                      type="button"
                                      className="YyYd_a_save"
                                      onClick={() => {
                                        if (
                                          !s.attachedItems.some(
                                            it =>
                                              it.kind === 'skill' &&
                                              it.name === sk.name,
                                          )
                                        ) {
                                          s.attachedItems.push({
                                            kind: 'skill',
                                            name: sk.name,
                                            desc: sk.description,
                                          });
                                        }
                                        s.settingsOpen = false;
                                        forceRender();
                                      }}
                                    >
                                      在当前会话中使用
                                    </button>
                                  </div>
                                </div>
                              ) : null}
                            </li>
                          );
                        })}
                    </ul>
                  </div>
                ) : null}

                {s.settingsTab === 'permissions' ? (
                  <div className="zGbnIq_section">
                    <div
                      style={{
                        display: 'flex',
                        justifyContent: 'space-between',
                        alignItems: 'center',
                      }}
                    >
                      <h3 className="zGbnIq_title" style={{ margin: 0 }}>
                        会话授权与审计记录
                      </h3>
                      <Button
                        variant="outline"
                        size="sm"
                        onClick={() => trackAsync(refreshPermissions())}
                      >
                        刷新
                      </Button>
                    </div>
                    <p className="zGbnIq_intro">
                      {s.permissionsStatus ||
                        '查看当前会话已授予的任务级权限，并可随时撤销。'}
                    </p>
                    <div className="zGbnIq_rows">
                      {!s.sessionGrants.length ? (
                        <div className="zGbnIq_modelEmpty">
                          当前会话暂无持久化任务授权记录。
                        </div>
                      ) : (
                        s.sessionGrants.map((grant, idx) => {
                          const cap =
                            grant.capability ??
                            grant.input?.capability ??
                            '未知能力';
                          const scopeStr =
                            typeof grant.scope === 'string'
                              ? grant.scope
                              : JSON.stringify(grant.scope ?? {});
                          const key = `${cap}:${scopeStr}`;
                          return (
                            <div key={`${key}/${idx}`} className="zGbnIq_rowCard">
                              <div className="zGbnIq_rowHead">
                                <div className="zGbnIq_rowIdentity">
                                  <span className="zGbnIq_rowName">
                                    {CAPABILITY_LABELS[cap] ?? cap}
                                  </span>
                                  <span className="zGbnIq_rowTag">
                                    {clip(scopeStr, 80)}
                                  </span>
                                </div>
                                <Button
                                  variant="outline"
                                  size="sm"
                                  disabled={s.revokeBusyKey === key}
                                  onClick={() => trackAsync(revokeGrant(grant))}
                                >
                                  撤销授权
                                </Button>
                              </div>
                            </div>
                          );
                        })
                      )}
                    </div>
                  </div>
                ) : null}

                {s.settingsTab === 'plugins' ? (
                  <div className="pbvGtq_section">
                    <h3 className="pbvGtq_heading">扩展能力</h3>
                    <p className="pbvGtq_intro">
                      按需展开查看或调整受控能力模块参数，或检索已挂载模块目录。
                    </p>

                    <div className="pbvGtq_tabs" role="tablist">
                      {[
                        ['config', '能力参数配置'],
                        ['inventory', '已挂载模块目录'],
                      ].map(([tabKey, tabTitle]) => (
                        <button
                          key={tabKey}
                          type="button"
                          role="tab"
                          aria-selected={s.pluginsSubTab === tabKey}
                          data-active={String(s.pluginsSubTab === tabKey)}
                          className="pbvGtq_tab"
                          onClick={() => {
                            s.pluginsSubTab = tabKey;
                            forceRender();
                          }}
                        >
                          {tabTitle}
                        </button>
                      ))}
                    </div>

                    {s.pluginSavedNotice ? (
                      <p className="zGbnIq_savedNotice" role="status">
                        {s.pluginSavedNotice}
                      </p>
                    ) : null}

                    <div className="pbvGtq_panel">
                      {s.pluginsSubTab === 'config' ? (
                        <ul className="pbvGtq_cards">
                          {BUILTIN_CAPABILITY_GROUPS.map(group => {
                            const isOpen = Boolean(
                              s.expandedPluginCards[group.id],
                            );
                            const paramVal =
                              s.pluginParamValues[group.id] ??
                              group.defaultParam;
                            return (
                              <li
                                key={group.id}
                                className={clsx(
                                  'YyYd_a_card',
                                  isOpen && 'YyYd_a_cardOpen',
                                )}
                              >
                                <button
                                  type="button"
                                  className="YyYd_a_header"
                                  aria-expanded={isOpen}
                                  onClick={() => {
                                    s.expandedPluginCards[group.id] = !isOpen;
                                    forceRender();
                                  }}
                                >
                                  <div className="YyYd_a_headText">
                                    <span className="YyYd_a_name">
                                      {group.title}
                                    </span>
                                    <span className="YyYd_a_description">
                                      {group.summary}
                                    </span>
                                  </div>
                                  <span className="At1oFq_badge">
                                    {`${group.badge} · ${group.items.length} 项能力`}
                                  </span>
                                  <IconChevronDownOutline14
                                    size={14}
                                    className={clsx(
                                      'YyYd_a_chevron',
                                      isOpen && 'YyYd_a_chevronOpen',
                                    )}
                                  />
                                </button>

                                {isOpen ? (
                                  <div className="YyYd_a_body">
                                    <div className="At1oFq_field">
                                      <div className="At1oFq_head">
                                        <label className="At1oFq_label">
                                          {group.paramLabel}
                                        </label>
                                        <div className="At1oFq_badges">
                                          <span className="At1oFq_badgeMuted">
                                            {`默认：${group.defaultParam}`}
                                          </span>
                                          {paramVal !== group.defaultParam ? (
                                            <button
                                              type="button"
                                              className="At1oFq_reset"
                                              onClick={() => {
                                                s.pluginParamValues[group.id] =
                                                  group.defaultParam;
                                                forceRender();
                                              }}
                                            >
                                              恢复默认
                                            </button>
                                          ) : null}
                                        </div>
                                      </div>
       <input
                                        type="text"
                                        className="At1oFq_input"
                                        value={paramVal}
                                        onChange={e => {
                                          s.pluginParamValues[group.id] =
                                            e.target.value;
                                          forceRender();
                                        }}
                                      />
                                      <p className="At1oFq_hint">
                                        {`授权策略：${group.authMode}`}
                                      </p>
                                    </div>

                                    {group.items.map(([itemName, itemDesc]) => (
                                      <div
                                        key={itemName}
                                        className="At1oFq_field"
                                      >
                                        <div className="At1oFq_head">
                                          <span className="At1oFq_label">
                                            {itemName}
                                          </span>
                                          <span className="At1oFq_badge">
                                            已就绪
                                          </span>
                                        </div>
                                        <p className="At1oFq_hint">
                                          {itemDesc}
                                        </p>
                                      </div>
                                    ))}

                                    <div className="YyYd_a_footer">
                                      <button
                                        type="button"
                                        className="YyYd_a_discard"
                                        onClick={() => {
                                          s.expandedPluginCards[group.id] =
                                            false;
                                          forceRender();
                                        }}
                                      >
                                        收起
                                      </button>
                                      <button
                                        type="button"
                                        className="YyYd_a_save"
                                        onClick={() => {
                                          s.pluginSavedNotice = `已保存“${group.title}”模块参数。`;
                                          s.expandedPluginCards[group.id] =
                                            false;
                                          forceRender();
                                        }}
                                      >
                                        保存参数
                                      </button>
                                    </div>
                                  </div>
                                ) : null}
                              </li>
                            );
                          })}
                        </ul>
                      ) : (
                        <div className="qSYn7G_section">
                          <div className="qSYn7G_catalog">
                            <div className="qSYn7G_search">
                              <IconSearchOutline16 size={16} />
                              <input
                                type="text"
                                placeholder="搜索已挂载模块或能力…"
                                value={s.pluginSearchQuery}
                                onChange={e => {
                                  s.pluginSearchQuery = e.target.value;
                                  forceRender();
                                }}
                              />
                            </div>
                            <div className="qSYn7G_catalogHeading">
                              <h3>已挂载模块</h3>
                              <span>{BUILTIN_CAPABILITY_GROUPS.length}</span>
                            </div>
                            <div className="qSYn7G_cards">
                              {BUILTIN_CAPABILITY_GROUPS.filter(g => {
                                const q = s.pluginSearchQuery
                                  .trim()
                                  .toLowerCase();
                                if (!q) return true;
                                return (
                                  g.title.toLowerCase().includes(q) ||
                                  g.summary.toLowerCase().includes(q) ||
                                  g.items.some(
                                    ([n, d]) =>
                                      n.toLowerCase().includes(q) ||
                                      d.toLowerCase().includes(q),
                                  )
                                );
                              }).map(group => {
                                const isOpen = Boolean(
                                  s.expandedInventoryCards[group.id],
                                );
                                return (
                                  <div
                                    key={group.id}
                                    className="qSYn7G_card"
                                    data-open={String(isOpen)}
                                  >
                                    <button
                                      type="button"
                                      className="qSYn7G_cardContent"
                                      aria-expanded={isOpen}
                                      onClick={() => {
                                        s.expandedInventoryCards[group.id] =
                                          !isOpen;
                                        forceRender();
                                      }}
                                    >
                                      <span className="qSYn7G_cardTitle">
                                        {group.title}
                                      </span>
                                      <span className="qSYn7G_cardTrailing">
                                        <span
                                          className="qSYn7G_statusDot"
                                          data-phase="active"
                                        />
                                        <span
                                          className="qSYn7G_configTag"
                                          data-enabled="true"
                                        >
                                          {group.badge}
                                        </span>
                                        <IconChevronDownOutline14
                                          size={14}
                                          className="qSYn7G_chevron"
                                        />
                                      </span>
                                    </button>
                                    {isOpen ? (
                                      <div className="qSYn7G_cardDetails">
                                        <dl
                                          className="qSYn7G_details"
                                          style={{ margin: 0 }}
                                        >
                                          <div>
                                            <dt>包含能力</dt>
                                            <dd>
                                              {group.items
                                                .map(([name]) => name)
                                                .join('、')}
                                            </dd>
                                          </div>
                                          <div>
                                            <dt>授权策略</dt>
                                            <dd>{group.authMode}</dd>
                                          </div>
                                          <div>
                                            <dt>默认参数</dt>
                                            <dd>{`${group.paramLabel}：${s.pluginParamValues[group.id] ?? group.defaultParam}`}</dd>
                                          </div>
                                        </dl>
                                      </div>
                                    ) : null}
                                  </div>
                                );
                              })}
                            </div>
                          </div>
                        </div>
                      )}
                    </div>
                  </div>
                ) : null}
              </div>
            </div>
          </div>
        </div>
      ) : null}

      {/* Official DSH Directory Browser Modal (@deepseek-ai/dsh-client-ui-directory-picker-browse) */}
      <Modal
        open={s.newProjectModalOpen}
        onClose={() => {
          if (!s.addProjectBusy) {
            s.newProjectModalOpen = false;
            forceRender();
          }
        }}
        title="选择工作区目录"
        className="ZuhsRW_dialog"
        headless={true}
      >
        <div id="new-project-dialog" className="ZuhsRW_editorScope">
          <div className="ZuhsRW_header">
            <h2 className="ZuhsRW_title">选择工作区目录</h2>
            <div className="ZuhsRW_crumbBar">
              {!s.pathEditing ? (
                <>
                  <span className="ZuhsRW_crumbTrail" role="navigation">
                    {browseCrumbs.map((crumb, index) => (
                      <span key={crumb.path} className="ZuhsRW_crumbSeat">
                        {index > 0 ? (
                          <IconChevronRightOutline14
                            size={12}
                            className="ZuhsRW_crumbChevron"
                          />
                        ) : null}
                        <button
                          type="button"
                          className="ZuhsRW_crumb"
                          disabled={s.browseLoading || s.addProjectBusy}
                          onClick={() => void browseDirectory(crumb.path)}
                        >
                          {crumb.name}
                        </button>
                      </span>
                    ))}
                  </span>
                  <button
                    type="button"
                    className="ZuhsRW_crumbEditZone"
                    aria-label="编辑路径"
                    title="编辑路径"
                    onClick={() => {
                      s.pathEditing = true;
                      forceRender();
                    }}
                  >
                    <IconEditOutline16
                      size={14}
                      className="ZuhsRW_crumbEditGlyph"
                    />
                  </button>
                </>
              ) : (
                <input
                  ref={modalProjectWsInputRef}
                  id="modal-project-path"
                  type="text"
                  className="ZuhsRW_pathInput"
                  aria-label="编辑路径"
                  autoFocus={true}
                  value={s.projectWorkspaceInput}
                  onChange={e => {
                    s.projectWorkspaceInput = e.target.value;
                    forceRender();
                  }}
                  onBlur={() => {
                    s.pathEditing = false;
                    forceRender();
                  }}
                  onKeyDown={e => {
                    if (e.key === 'Enter') {
                      e.preventDefault();
                      s.pathEditing = false;
                      void browseDirectory(s.projectWorkspaceInput);
                    } else if (e.key === 'Escape') {
                      e.stopPropagation();
                      s.pathEditing = false;
                      forceRender();
                    }
                  }}
                />
              )}
            </div>
          </div>

          <div className="ZuhsRW_content">
            <div className="ZuhsRW_millerRow">
              <div className="ZuhsRW_column" role="list">
                {visibleBrowseEntries.map(entry => {
                  const selected =
                    (s.browseSelectedPath || s.projectWorkspaceInput) ===
                    entry.path;
                  return (
                    <span
                      key={entry.path}
                      role="listitem"
                      className="ZuhsRW_rowSeat"
                    >
                      <button
                        type="button"
                        aria-current={selected || undefined}
                        className={clsx(
                          'ZuhsRW_row',
                          selected && 'ZuhsRW_rowSelected',
                        )}
                        disabled={s.browseLoading || s.addProjectBusy}
                        onClick={() => {
                          s.browseSelectedPath = entry.path;
                          s.projectWorkspaceInput = entry.path;
                          forceRender();
                        }}
                        onDoubleClick={() => void browseDirectory(entry.path)}
                      >
                        {selected ? (
                          <IconFolderOpen16
                            size={16}
                            className="ZuhsRW_rowIconSelected"
                          />
                        ) : (
                          <IconFolderClose16
                            size={16}
                            className="ZuhsRW_rowIcon"
                          />
                        )}
                        <span className="ZuhsRW_rowName">{entry.name}</span>
                        <IconChevronRightOutline14
                          size={14}
                          className="ZuhsRW_rowChevron"
                        />
                      </button>
                    </span>
                  );
                })}
              </div>
            </div>

            {s.browseLoading ? (
              <div
                className="ZuhsRW_status ZuhsRW_loadingFloat"
                role="status"
              >
                加载中…
              </div>
            ) : null}

            {s.browseError ? (
              <div className="ZuhsRW_error" role="alert">
                {s.browseError}
              </div>
            ) : null}
          </div>

          <div className="ZuhsRW_footerBar">
            <Button
              variant="outline"
              icon={<IconPlusOutline16 size={14} />}
              disabled={s.browseLoading || s.addProjectBusy}
              onClick={() => {
                s.newFolderOpen = true;
                s.newFolderName = '';
                forceRender();
              }}
            >
              新建文件夹
            </Button>
            <button
              type="button"
              className={clsx(
                'ZuhsRW_showHiddenToggle',
                s.browseShowHidden && 'ZuhsRW_showHiddenToggleActive',
              )}
              aria-pressed={s.browseShowHidden}
              onClick={() => {
                s.browseShowHidden = !s.browseShowHidden;
                forceRender();
              }}
            >
              显示隐藏文件
              {s.browseShowHidden ? <IconCheckOutline16 size={14} /> : null}
            </button>
            <span className="ZuhsRW_footerGap" />
            <Button
              variant="outline"
              className="ZuhsRW_footerAction"
              disabled={s.addProjectBusy}
              onClick={() => {
                s.newProjectModalOpen = false;
                forceRender();
              }}
            >
              取消
            </Button>
            <Button
              id="confirm-new-project-btn"
              variant="primary"
              className="ZuhsRW_footerAction"
              disabled={
                s.addProjectBusy ||
                !(s.browseSelectedPath || s.projectWorkspaceInput || s.browsePath)
              }
              onClick={e => trackAsync(handleAddProject(e, true))}
            >
              {s.addProjectBusy ? '正在打开…' : '打开'}
            </Button>
          </div>
        </div>
      </Modal>

      {/* Nested New Folder Modal inside Directory Browser */}
      <Modal
        open={s.newFolderOpen}
        onClose={() => {
          s.newFolderOpen = false;
          forceRender();
        }}
        title="新建文件夹"
        className="ZuhsRW_createDialog"
        headless={true}
      >
        <div className="ZuhsRW_createBody">
          <h3 className="ZuhsRW_createTitle">新建文件夹</h3>
          <p className="ZuhsRW_createIn">
            {`在“${s.browsePath}”中新建文件夹`}
          </p>
          <input
            type="text"
            className="ZuhsRW_createInput"
            aria-label="文件夹名称"
            placeholder="未命名文件夹"
            autoFocus={true}
            value={s.newFolderName}
            onChange={e => {
              s.newFolderName = e.target.value;
              forceRender();
            }}
            onKeyDown={e => {
              if (e.key === 'Enter' && s.newFolderName.trim()) {
                e.preventDefault();
                const name = s.newFolderName.trim();
                s.newFolderOpen = false;
                s.newFolderName = '';
                void browseDirectory(s.browsePath, name);
              }
            }}
          />
          <div className="ZuhsRW_createActions">
            <Button
              variant="outline"
              onClick={() => {
                s.newFolderOpen = false;
                forceRender();
              }}
            >
              取消
            </Button>
            <Button
              variant="primary"
              disabled={!s.newFolderName.trim()}
              onClick={() => {
                const name = s.newFolderName.trim();
                s.newFolderOpen = false;
                s.newFolderName = '';
                void browseDirectory(s.browsePath, name);
              }}
            >
              创建
            </Button>
          </div>
        </div>
      </Modal>

      {/* Official DSH Risk Confirmation Modal (@deepseek-ai/dsh-client-ui-primitives) */}
      <RiskConfirmation
        open={s.fullAccessConfirmOpen}
        title="确认开启“完全访问”模式？"
        description="开启“完全访问”后，智能体将减少确认步骤，并直接执行更多操作，包括文件修改与构建任务。仅建议在你完全信任当前任务时使用。"
        acknowledgeLabel="我已了解风险，并愿意继续"
        cancelLabel="取消"
        confirmLabel="开启完全访问"
        acknowledged={s.fullAccessAcknowledged}
        onAcknowledgedChange={checked => {
          s.fullAccessAcknowledged = checked;
          forceRender();
        }}
        onCancel={() => {
          s.fullAccessConfirmOpen = false;
          s.fullAccessAcknowledged = false;
          forceRender();
        }}
        onConfirm={confirmFullAccess}
      />

      {/* Web <-> Server Login Modal when in remote_server mode and not logged in */}
      <Modal
        open={
          s.connectionMode === 'remote_server' &&
          !s.serverLoggedIn &&
          !s.settingsOpen
        }
        onClose={() => void switchConnectionMode('local_device')}
        title="登录远程 CAYW Server"
        size="md"
      >
        <form
          style={{ display: 'flex', flexDirection: 'column', gap: 12 }}
          onSubmit={e => void handleServerLogin(e)}
        >
          <p style={{ margin: 0, fontSize: 13, color: 'var(--dsw-alias-label-secondary)' }}>
            {s.serverIsDefaultCredentials
              ? '当前使用默认初始账号登录（默认账号：admin，默认密码：admin）。登录后可在“设置 → 连接与账号”中修改账号与密码。'
              : `请输入远程 Server 登录账号（${s.serverUsername}）与密码。`}
          </p>
          {s.serverLoginError ? (
            <p className="_banner_ugy7y_1" role="alert" style={{ margin: 0 }}>
              {s.serverLoginError}
            </p>
          ) : null}
          <div className="zGbnIq_field">
            <label className="zGbnIq_fieldLabel">登录账号</label>
            <input
              type="text"
              className="zGbnIq_input"
              value={s.serverLoginUserDraft}
              onChange={e => {
                s.serverLoginUserDraft = e.target.value;
                forceRender();
              }}
            />
          </div>
          <div className="zGbnIq_field">
            <label className="zGbnIq_fieldLabel">登录密码</label>
            <input
              type="password"
              className="zGbnIq_input"
              value={s.serverLoginPassDraft}
              onChange={e => {
                s.serverLoginPassDraft = e.target.value;
                forceRender();
              }}
            />
          </div>
          <div style={{ display: 'flex', justifyContent: 'space-between', gap: 8, marginTop: 4 }}>
            <Button
              variant="outline"
              type="button"
              onClick={() => void switchConnectionMode('local_device')}
            >
              切回本机设备直连（无令牌）
            </Button>
            <Button variant="primary" type="submit" disabled={s.serverLoginBusy}>
              {s.serverLoginBusy ? '正在登录…' : '登录 Server'}
            </Button>
          </div>
        </form>
      </Modal>

      {/* Add Device Modal (Provide Server Address -> Automatic RSA Binding) */}
      <Modal
        open={s.addDeviceModalOpen}
        onClose={() => {
          s.addDeviceModalOpen = false;
          forceRender();
        }}
        title="添加设备"
        size="md"
      >
        <form
          style={{ display: 'flex', flexDirection: 'column', gap: 12 }}
          onSubmit={e => void handleAddDeviceSubmit(e)}
        >
          <p
            style={{
              margin: 0,
              fontSize: 13,
              color: 'var(--dsw-alias-label-secondary)',
              lineHeight: 1.5,
            }}
          >
            填写 Server 地址后，系统将在后台自动生成并交换 RSA 密钥完成一次性永久绑定，后续均自动使用 RSA 加密连接。
          </p>

          {s.addDeviceError ? (
            <p className="_banner_ugy7y_1" role="alert" style={{ margin: 0 }}>
              {s.addDeviceError}
            </p>
          ) : null}

          <div className="zGbnIq_field">
            <label className="zGbnIq_fieldLabel">Server 地址</label>
            <input
              id="new-device-server-input"
              type="text"
              className="zGbnIq_input"
              autoFocus={true}
              placeholder="如 wss://127.0.0.1:8787 或 192.168.1.100:8787"
              value={s.newDeviceServerUrl}
              onChange={e => {
                s.newDeviceServerUrl = e.target.value;
                forceRender();
              }}
            />
          </div>

          <div className="zGbnIq_field">
            <label className="zGbnIq_fieldLabel">
              备注名称（可选，留空自动按地址生成）
            </label>
            <input
              id="new-device-label-input"
              type="text"
              className="zGbnIq_input"
              placeholder="如 远程开发机"
              value={s.newDeviceLabel}
              onChange={e => {
                s.newDeviceLabel = e.target.value;
                forceRender();
              }}
            />
          </div>

          <div
            style={{
              display: 'flex',
              justifyContent: 'flex-end',
              gap: 8,
              marginTop: 4,
            }}
          >
            <Button
              variant="outline"
              type="button"
              onClick={() => {
                s.addDeviceModalOpen = false;
                forceRender();
              }}
            >
              取消
            </Button>
            <Button
              id="confirm-add-device-btn"
              variant="primary"
              type="submit"
              disabled={s.addDeviceBusy}
            >
              {s.addDeviceBusy ? '正在自动绑定…' : '自动绑定并连接'}
            </Button>
          </div>
        </form>
      </Modal>

      <dialog
        ref={forkDialogRef}
        id="fork-dialog"
        open={s.forkOpen}
        aria-labelledby="fork-heading"
      >
        <h2 id="fork-heading">创建普通聊天分支</h2>
        <form
          ref={bind({ submit: e => trackAsync(handleForkSubmit(e)) })}
          id="fork-form"
          onSubmit={e => trackAsync(handleForkSubmit(e))}
        >
          <label htmlFor="fork-label">新会话名称</label>
          <input
            ref={forkLabelInputRef}
            id="fork-label"
            maxLength={160}
            required
            value={s.forkLabel}
            onChange={e => {
              s.forkLabel = e.target.value;
              forceRender();
            }}
          />
          <label>
            <input
              ref={forkConfirmInputRef}
              id="fork-confirm"
              type="checkbox"
              required
              checked={s.forkConfirm}
              onChange={e => {
                s.forkConfirm = e.target.checked;
                forceRender();
              }}
            />
            我确认新建普通聊天分支，并理解原会话仍被冻结。
          </label>
          <p id="fork-error" role="alert">
            {s.forkError}
          </p>
          <div>
            <button
              ref={bind({ click: closeForkDialog })}
              id="close-fork"
              type="button"
              onClick={closeForkDialog}
            >
              返回
            </button>
            <button id="fork-button" type="submit" disabled={s.forkBusy}>
              {s.forkBusy ? '正在创建…' : '确认 Fork'}
            </button>
          </div>
        </form>
      </dialog>
    </div>
  );
}
