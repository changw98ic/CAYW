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
    label: 'Read Only',
    zhLabel: '只读与按需审批',
    desc: '只读操作直接执行；文件写入、修改、构建与外部工具需每次或按任务明确授权。',
  },
  {
    id: 'workspace-write',
    label: 'Workspace Write',
    zhLabel: '工作区读写',
    desc: '当前工作区内文件读写优先使用任务授权，跨目录或外部命令仍需审批。',
  },
  {
    id: 'danger-full-access',
    label: 'Full access',
    zhLabel: '完全访问',
    desc: '减少确认步骤，允许直接执行更多操作；仅建议在完全信任当前任务时启用。',
  },
];

const AGENT_PRESETS = [
  {
    id: 'code',
    name: 'PTC 模式',
    badge: '内置 · 默认',
    desc: '通过 Luau CodeMode 呈现受控能力，让模型用代码单元组合多步操作并统一经过权限与预算边界。',
  },
  {
    id: 'standard',
    name: '标准模式',
    badge: '内置',
    desc: '完整编码 Agent，支持文件读写、检索、Git 检视、预设构建测试与宿主连接工具。',
  },
  {
    id: 'strongflow',
    name: 'StrongFlow 强流程',
    badge: '插件',
    desc: '为复杂任务提供阶段门禁与检查点；插件卸载后已绑定会话保持冻结，支持确认后 Fork 为普通聊天。',
  },
];

const BUILTIN_CAPABILITY_GROUPS = [
  {
    title: '文件与目录（fs.*）',
    badge: '已挂载',
    items: [
      ['fs.read', '读取文件内容与 SHA-256'],
      ['fs.write', '原子创建或凭摘要覆盖文件'],
      ['fs.patch', '按精确上下文替换文件片段'],
      ['fs.list / fs.stat', '列出目录项与读取文件元数据'],
    ],
  },
  {
    title: '代码检索与语法（fs / code）',
    badge: '已挂载',
    items: [
      ['fs.grep / fs.glob', '工作区有界文本检索与路径匹配'],
      ['code.outline', '提取源码结构与符号大纲'],
      ['code.diff_hunks', '计算文件改动差异片段'],
    ],
  },
  {
    title: 'Git 版本控制（git.*）',
    badge: '已挂载',
    items: [
      ['git.status', '查看工作区当前变更状态'],
      ['git.diff', '查看未提交或指定范围的代码差异'],
      ['git.log / git.show', '查看提交历史与单次提交详情'],
    ],
  },
  {
    title: '预设构建与测试（build / test）',
    badge: '已挂载',
    items: [
      ['build.run', '运行宿主显式配置的预设构建任务'],
      ['test.run', '运行宿主显式配置的预设测试任务'],
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
      // DSH Settings Modal state
      settingsOpen: false,
      settingsTab: 'general',
      themeMode: 'dark',
      enterBehavior: 'enter',
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

  const openSettings = (tab = 'general') => {
    s.settingsTab = tab;
    s.settingsOpen = true;
    forceRender();
    if (tab === 'permissions') {
      void refreshPermissions();
    } else if (tab === 'plugins' && !s.cachedCapabilities.length) {
      void refreshPlugins();
    } else if (tab === 'models' && s.projects.length === 1 && s.projects[0]?.address === '本机 IPC') {
      void refreshProjects();
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
            <div className="wSkVaW_viewArea" hidden={isHero}>
              <div className="Md3f7G_root">
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
                                    ? 'Wait'
                                    : 'Execute Code'
                                  : clip(call.name, 60);
                              const summary =
                                call.name === 'execute_code'
                                  ? clip(
                                      call.arguments?.code ??
                                        call.arguments?.action ??
                                        '运行 Luau 代码单元',
                                      80,
                                    )
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
                            探索未至之境
                          </span>
                          <span className="pXSMma_previewBadge">预览版</span>
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
                    </div>
                  </>
                ) : null}

                <div className={clsx('uV2eYG_root', isHero && 'uV2eYG_hero')}>
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
                            s.promptText = e.target.value;
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
                        <Tooltip label="添加工作区" side="top" delayMs={400}>
                          <button
                            type="button"
                            className="uV2eYG_add"
                            aria-label="添加工作区"
                            onClick={() => openNewProjectModal()}
                          >
                            <IconPlusOutline16 size={14} />
                          </button>
                        </Tooltip>

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
                        </div>
                      </div>

                      <div className="uV2eYG_trailing">
                        <div className="_7KE1Ra_root">
                          <button
                            id="composer-model-btn"
                            type="button"
                            className="_7KE1Ra_trigger"
                            aria-label="当前模型：deepseek-v4.1-flash"
                            title="点击查看模型与网关配置"
                            onClick={() => openSettings('models')}
                          >
                            <span className="_7KE1Ra_triggerLabel">
                              deepseek-v4.1-flash
                            </span>
                            <IconChevronDownOutline14 className="_7KE1Ra_chevron" />
                          </button>
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
                  ['models', '模型', IconDataOutline16],
                  ['agent-presets', 'Agent 预设', IconAgentPresetOutline16],
                  ['permissions', '权限与审计', IconPersonalizationOutline16],
                  ['plugins', '插件', IconPersonalizationOutline16],
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
                          配置输入框按下 Enter 键时的默认发送方式。
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
                            ? 'Enter 直接发送'
                            : 'Cmd/Ctrl + Enter 发送'}
                        </span>
                        <IconChevronDownOutline14 className="T1PP_q_chevron" />
                      </button>
                    </div>
                  </div>
                ) : null}

                {s.settingsTab === 'models' ? (
                  <div className="zGbnIq_section">
                    <h3 className="zGbnIq_title">模型</h3>
                    <p className="zGbnIq_intro">
                      本机 Runtime 已接入真实模型提供方，API 密钥由本机进程安全持有。
                    </p>
                    <div className="zGbnIq_rows">
                      <div className="zGbnIq_rowCard">
                        <div className="zGbnIq_rowHead">
                          <div className="zGbnIq_rowIdentity">
                            <span className="zGbnIq_credentialDot zGbnIq_credentialDotConfigured" />
                            <span className="zGbnIq_rowName">
                              DeepSeek / Magpie OpenAI 兼容提供方
                            </span>
                            <span className="zGbnIq_rowTag">API 密钥已配置</span>
                          </div>
                        </div>
                        <div className="zGbnIq_modelCatalog">
                          <div className="zGbnIq_modelCatalogHeading">
                            <span className="zGbnIq_modelCatalogTitle">
                              模型目录
                            </span>
                            <span className="zGbnIq_modelCatalogMeta">
                              http://127.0.0.1:3425/v1
                            </span>
                          </div>
                          <div className="zGbnIq_modelList">
                            <div className="zGbnIq_modelEntry">
                              <div className="zGbnIq_modelRow">
                                <strong>auto/deepseek-v4.1-flash</strong>
                                <span className="zGbnIq_rowTag">当前使用</span>
                              </div>
                            </div>
                          </div>
                        </div>
                      </div>
                    </div>
                  </div>
                ) : null}

                {s.settingsTab === 'agent-presets' ? (
                  <div className="zGbnIq_section">
                    <h3 className="zGbnIq_title">Agent 预设</h3>
                    <p className="zGbnIq_intro">
                      预设决定会话的 Agent 所使用的工具编排方式与能力集合。
                    </p>
                    <div className="zGbnIq_rows">
                      {AGENT_PRESETS.map(ap => (
                        <div key={ap.id} className="zGbnIq_rowCard">
                          <div className="zGbnIq_rowHead">
                            <div className="zGbnIq_rowIdentity">
                              <span className="zGbnIq_rowName">{ap.name}</span>
                              <span className="zGbnIq_rowTag">{ap.badge}</span>
                            </div>
                            <Button
                              variant={
                                s.selectedAgentPreset === ap.id
                                  ? 'primary'
                                  : 'outline'
                              }
                              size="sm"
                              onClick={() => {
                                s.selectedAgentPreset = ap.id;
                                forceRender();
                              }}
                            >
                              {s.selectedAgentPreset === ap.id
                                ? '当前默认'
                                : '设为默认'}
                            </Button>
                          </div>
                          <p className="zGbnIq_intro" style={{ margin: 0 }}>
                            {ap.desc}
                          </p>
                        </div>
                      ))}
                    </div>
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
                    <h3 className="pbvGtq_heading">插件列表</h3>
                    <p className="pbvGtq_intro">
                      查看当前 Runtime 已挂载的受控能力组与会话插件状态。
                    </p>
                    <div className="qSYn7G_cards">
                      {BUILTIN_CAPABILITY_GROUPS.map(group => (
                        <div key={group.title} className="qSYn7G_card">
                          <div className="qSYn7G_cardContent">
                            <span className="qSYn7G_cardTitle">
                              {group.title}
                            </span>
                            <span className="qSYn7G_cardTrailing">
                              <span className="qSYn7G_status">
                                <StateDot
                                  state="done"
                                  className="qSYn7G_statusDot"
                                />
                                {group.badge}
                              </span>
                            </span>
                          </div>
                          <div className="qSYn7G_cardDetails">
                            {group.items.map(([code, desc]) => (
                              <div key={code} className="qSYn7G_details">
                                <span>{code}</span>
                                <span className="qSYn7G_entryValue">{desc}</span>
                              </div>
                            ))}
                          </div>
                        </div>
                      ))}
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
        title="确认启用 Full access？"
        description="启用 Full access 后，agent 将减少确认步骤，并且可以直接执行更多操作，包括敏感操作、文件修改或外部命令。仅建议在你信任当前任务时使用。"
        acknowledgeLabel="我已了解风险，并愿意继续"
        cancelLabel="取消"
        confirmLabel="启用 Full access"
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
