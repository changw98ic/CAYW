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
  SpotlightCard,
  ShinyText,
  GradientText,
  HintPopover,
  TooltipBadge,
} from './components/Bits.jsx';

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

const ROLE_LABELS = {
  user: '你',
  assistant: '助手',
  system: '系统',
  toolResult: '工具结果',
  custom: '宿主记录',
};

const PERMISSION_PRESETS = [
  {
    id: 'standard',
    label: '按需审批',
    enLabel: 'Standard',
    desc: '只读操作直接执行；文件写入、修改、构建与外部工具需每次或按任务明确授权。',
  },
  {
    id: 'workspace-write',
    label: '工作区读写',
    enLabel: 'Workspace Write',
    desc: '当前工作区内文件读写优先使用任务授权，跨目录或外部命令仍需审批。',
  },
  {
    id: 'danger-full-access',
    label: '完全访问',
    enLabel: 'Full access',
    desc: '减少确认步骤，允许直接执行更多操作；仅建议在完全信任当前任务时启用。',
  },
];

const STARTER_PROMPTS = [
  {
    title: '📄 读取工作区说明',
    desc: '读取 README.md 并用一句话概括当前项目',
    prompt: '读取当前工作区的 README.md，并用一句话总结项目内容。',
  },
  {
    title: '✍️ 创建并验证文件',
    desc: '写入一份测试笔记并读回校验内容',
    prompt: '在当前工作区创建 hello-cayw.txt，写入 Hello from CAYW，再读取确认内容。',
  },
  {
    title: '🔍 查看工作区文件列表',
    desc: '列出当前工作区根目录下的全部文件',
    prompt: '列出当前工作区根目录下的所有文件，并简要说明各自用途。',
  },
];

const BUILTIN_CAPABILITY_GROUPS = [
  {
    title: '文件与目录（fs.*）',
    badge: '写操作需授权',
    items: [
      ['fs.read', '读取文件内容与 SHA-256'],
      ['fs.write', '原子创建或凭摘要覆盖文件'],
      ['fs.patch', '按精确上下文替换文件片段'],
      ['fs.list / fs.stat', '列出目录项与读取文件元数据'],
    ],
  },
  {
    title: '代码检索与语法（fs / code）',
    badge: '受控只读',
    items: [
      ['fs.grep / fs.glob', '工作区有界文本检索与路径匹配'],
      ['code.outline', '提取源码结构与符号大纲'],
      ['code.diff_hunks', '计算文件改动差异片段'],
    ],
  },
  {
    title: 'Git 版本控制（git.*）',
    badge: '只读检视',
    items: [
      ['git.status', '查看工作区当前变更状态'],
      ['git.diff', '查看未提交或指定范围的代码差异'],
      ['git.log / git.show', '查看提交历史与单次提交详情'],
    ],
  },
  {
    title: '预设构建与测试（build / test）',
    badge: '执行需授权',
    items: [
      ['build.run', '运行宿主显式配置的预设构建任务'],
      ['test.run', '运行宿主显式配置的预设测试任务'],
    ],
  },
];

const OPTIONAL_PLUGIN_CATALOG = [
  {
    name: 'StrongFlow',
    role: '强流程编排插件',
    desc: '为复杂任务提供阶段门禁与计划检查点；卸载后已绑定会话保持冻结，支持确认后 Fork 为普通聊天。',
  },
  {
    name: 'JEv',
    role: '执行验证插件',
    desc: '对关键步骤产物与断言进行独立核验，不直接读写 Core 数据库。',
  },
  {
    name: 'Fusion',
    role: '多路探索与融合插件',
    desc: '在受控预算内对比候选方案并汇总验证证据。',
  },
  {
    name: 'Observer',
    role: '运行轨迹观测插件',
    desc: '订阅会话事件流进行只读审计与诊断统计，不干预普通聊天。',
  },
];

const QUICK_WORKSPACES = [
  ['/Volumes/ORICO/CAYW', 'CAYW 主仓库'],
  ['/Volumes/ORICO/CAYW-tui', 'CAYW TUI 分支'],
  ['/Volumes/ORICO/CAYW-webui', 'CAYW WebUI 分支'],
  ['/tmp/cayw-demo-project', '新建演示工作区 (/tmp)'],
];

function renderInlineTokens(text) {
  const parts = String(text ?? '').split(/(`[^`\n]+`|\*\*[^*\n]+\*\*)/g);
  return parts.map((part, idx) => {
    if (part.startsWith('`') && part.endsWith('`') && part.length >= 2) {
      return (
        <code key={idx} className="inline-code">
          {part.slice(1, -1)}
        </code>
      );
    }
    if (part.startsWith('**') && part.endsWith('**') && part.length >= 4) {
      return <strong key={idx}>{part.slice(2, -2)}</strong>;
    }
    return part;
  });
}

function FormattedBody({ text }) {
  const raw = String(text ?? '');
  const blocks = raw.split(/(```[\s\S]*?```)/g);
  return (
    <div className="body dsh-markdown">
      {blocks.map((block, bIdx) => {
        if (block.startsWith('```') && block.endsWith('```') && block.length >= 6) {
          const firstLineMatch = block.slice(3, -3).match(/^([a-zA-Z0-9_-]+)\n/);
          const lang = firstLineMatch ? firstLineMatch[1] : 'code';
          const inner = block
            .slice(3, -3)
            .replace(/^[a-zA-Z0-9_-]+\n/, '');
          return (
            <div key={bIdx} className="dsh-codeblock">
              <div className="dsh-codeblock-banner">
                <span className="dsh-codeblock-lang">{lang}</span>
              </div>
              <pre className="code-block">
                <code>{inner}</code>
              </pre>
            </div>
          );
        }
        return <React.Fragment key={bIdx}>{renderInlineTokens(block)}</React.Fragment>;
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
          label: '默认项目空间',
          workspace: '',
          address: '本机 IPC',
          default: true,
        },
      ],
      projectStates: new Map(),
      cachedCapabilities: [],
      sessionTitleOverrides: new Map(),
      sessionPickerOpen: true,
      sessionSearchQuery: '',
      // DSH Settings Modal state
      settingsOpen: false,
      settingsTab: 'general',
      themeMode: 'dark',
      enterBehavior: 'enter',
      // DSH Permission Preset & Grants state
      permissionPreset: 'standard',
      permissionPopoverOpen: false,
      fullAccessConfirmOpen: false,
      fullAccessAcknowledged: false,
      pendingPresetTarget: null,
      sessionGrants: [],
      sessionAudit: [],
      revokeBusyKey: null,
      // DSH New Project / Directory Picker Modal state
      newProjectModalOpen: false,
      browsePath: '/Volumes/ORICO',
      browseParent: '/Volumes',
      browseEntries: [],
      browseSelectedPath: '',
      browseShowHidden: false,
      browseLoading: false,
      browseError: '',
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
      if (mode === 'system' && typeof window !== 'undefined' && typeof window.matchMedia === 'function') {
        resolved = window.matchMedia('(prefers-color-scheme: light)').matches ? 'light' : 'dark';
      }
      document.documentElement.dataset.theme = resolved;
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
      if (
        typeof window !== 'undefined' &&
        typeof window.matchMedia === 'function' &&
        window.matchMedia('(max-width: 640px)').matches
      ) {
        s.sessionPickerOpen = false;
      }
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

  // Compute derived projections for rendering
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

  return (
    <div className="dsh-app-shell">
      <a className="skip" href="#conversation">
        跳到会话内容
      </a>
      <header className="topbar dsh-topbar">
        <div className="identity">
          <span className="mark" aria-hidden="true">
            C
          </span>
          <div>
            <strong>CAYW</strong>
            <span className="dsh-build-badge">v3</span>
            <span className="muted">DeepSeek Harness 风格工作台</span>
          </div>
        </div>
        <div className="connection">
          <button
            type="button"
            className="dsh-top-pill"
            onClick={() => openNewProjectModal()}
          >
            📁 + 新开项目
          </button>
          <button
            type="button"
            className="dsh-top-pill"
            onClick={() => switchPage('permissions')}
          >
            {`🔒 权限：${currentPresetObj.label}${approvals.length ? ` (${approvals.length} 待审)` : ''}`}
          </button>
          <button
            type="button"
            className="dsh-top-pill"
            onClick={() => openSettings('general')}
          >
            ⚙ 设置
          </button>
          <span className="dsh-conn-dot" data-connected={String(s.connectionStatus === '已连接本机')} />
          <span id="connection-status" role="status">
            {s.connectionStatus}
          </span>
          <button
            ref={bind({ click: () => trackAsync(connectLocal()) })}
            id="reconnect"
            className="quiet"
            type="button"
            disabled={s.reconnectBusy}
            onClick={() => trackAsync(connectLocal())}
          >
            重连
          </button>
        </div>
      </header>

      <div id="workspace" className="workspace dsh-frame">
        <aside className="sidebar dsh-sidebar" aria-labelledby="sessions-heading">
          <div className="dsh-sidebar-primary-actions">
            <button
              ref={bind({ click: () => trackAsync(createNewSession()) })}
              id="create-button"
              className="dsh-new-session-btn create-btn"
              type="button"
              aria-label="新建会话"
              disabled={s.createBusy}
              onClick={() => trackAsync(createNewSession())}
            >
              <span>{s.createBusy ? '创建中…' : '+ 新建会话'}</span>
            </button>
            <button
              id="open-new-project-btn"
              className="dsh-new-project-btn secondary"
              type="button"
              title="选择本地目录并新开项目工作区"
              onClick={() => openNewProjectModal()}
            >
              📁 新开项目
            </button>
          </div>

          <nav className="page-nav dsh-nav-segmented" aria-label="界面视图">
            <button
              ref={bind({ click: () => switchPage('conversation') })}
              id="nav-conversation"
              className="page-tab"
              type="button"
              aria-current={String(s.activePage === 'conversation')}
              onClick={() => switchPage('conversation')}
            >
              对话
            </button>
            <button
              ref={bind({ click: () => switchPage('projects') })}
              id="nav-projects"
              className="page-tab"
              type="button"
              aria-current={String(s.activePage === 'projects')}
              onClick={() => switchPage('projects')}
            >
              项目
            </button>
            <button
              ref={bind({ click: () => switchPage('permissions') })}
              id="nav-permissions"
              className="page-tab"
              type="button"
              aria-current={String(s.activePage === 'permissions')}
              onClick={() => switchPage('permissions')}
            >
              {`权限${approvals.length ? `(${approvals.length})` : ''}`}
            </button>
            <button
              ref={bind({ click: () => switchPage('plugins') })}
              id="nav-plugins"
              className="page-tab"
              type="button"
              aria-current={String(s.activePage === 'plugins')}
              onClick={() => switchPage('plugins')}
            >
              插件
            </button>
          </nav>

          <div className="dsh-workspace-box">
            <SpotlightCard
              as="button"
              id="current-workspace-btn"
              className="workspace-switcher dsh-workspace-pill"
              type="button"
              handlers={{ click: () => switchPage('projects') }}
              data-tip={
                currentProjectSummary?.workspace
                  ? `当前目录：${currentProjectSummary.workspace}（点击切换或管理项目空间）`
                  : `点击切换或新开本地项目空间（${currentProjectSummary?.address ?? '本机 IPC'}）`
              }
              onClick={() => switchPage('projects')}
            >
              <div className="dsh-workspace-pill-top">
                <span className="small muted">📁 当前工作区</span>
                <span className="dsh-workspace-switch-hint">切换 ▾</span>
              </div>
              <strong id="current-workspace-label">
                {currentProjectSummary?.label ?? '默认项目空间'}
              </strong>
              {currentProjectSummary?.workspace ? (
                <span className="dsh-workspace-path mono">
                  {currentProjectSummary.workspace}
                </span>
              ) : null}
            </SpotlightCard>
          </div>

          <details
            id="session-picker"
            className="session-picker dsh-session-browser"
            open={s.sessionPickerOpen}
            onToggle={e => {
              s.sessionPickerOpen = e.currentTarget.open;
            }}
          >
            <summary className="session-picker-summary">
              <span>工作区会话</span>
              <span id="session-picker-label" className="muted">
                {clip(snapshot ? displayLabel(snapshot) : '选择或新建', 60)}
              </span>
            </summary>
            <div className="session-picker-content">
              <div className="section-heading dsh-section-header">
                <div className="title-with-hint">
                  <h2 id="sessions-heading">会话列表</h2>
                  <HintPopover ariaLabel="会话同步说明">
                    对话保存在本机。断线后从已消费游标继续接收结果，消息不会自动重发。
                  </HintPopover>
                </div>
                <div className="session-actions">
                  <button
                    ref={bind({ click: () => trackAsync(refreshSessions()) })}
                    id="refresh-sessions"
                    className="quiet dsh-icon-btn"
                    type="button"
                    title="刷新会话列表"
                    disabled={s.refreshSessionsBusy}
                    onClick={() => trackAsync(refreshSessions())}
                  >
                    ↻ 刷新
                  </button>
                </div>
              </div>

              <div className="dsh-search-bar">
                <input
                  type="search"
                  className="dsh-search-input"
                  placeholder="搜索会话…"
                  aria-label="搜索会话"
                  value={s.sessionSearchQuery}
                  onChange={e => {
                    s.sessionSearchQuery = e.target.value;
                    forceRender();
                  }}
                />
              </div>

              <p id="list-status" className="small muted" role="status">
                {s.listStatus}
              </p>
              <nav id="session-list" className="session-list dsh-tree-list" aria-label="选择会话">
                {!filteredSessions.length ? (
                  <p className="small muted dsh-empty-note">
                    {s.sessionSearchQuery.trim()
                      ? '无匹配会话'
                      : '暂无会话，点击上方“+ 新建会话”即可开始。'}
                  </p>
                ) : (
                  filteredSessions.map(session => {
                    const shortId = String(session.id ?? '').slice(0, 8);
                    const sState = stateName(session);
                    return (
                      <SpotlightCard
                        as="button"
                        key={session.id}
                        className="session-item dsh-session-row"
                        type="button"
                        aria-current={String(session.id === s.selected)}
                        handlers={{
                          click: () => trackAsync(selectSession(session.id)),
                        }}
                        onClick={() => trackAsync(selectSession(session.id))}
                      >
                        <div className="dsh-session-row-main">
                          <span className={`dsh-status-dot dot-${sState}`} />
                          <strong>{displayLabel(session)}</strong>
                        </div>
                        <span>
                          {`${stateLabel(session, s.views.get(session.id))} · ${shortId}`}
                        </span>
                      </SpotlightCard>
                    );
                  })
                )}
              </nav>
            </div>
          </details>

          <div className="dsh-sidebar-footer">
            <button
              id="open-settings-button"
              type="button"
              className="dsh-settings-trigger"
              onClick={() => openSettings('general')}
            >
              <span className="dsh-settings-gear">⚙</span>
              <span className="dsh-settings-label">设置与偏好</span>
              <span className="dsh-settings-badge">{currentPresetObj.label}</span>
            </button>
          </div>
        </aside>

        <main id="conversation" className="conversation dsh-center-col" tabIndex={-1}>
          <div id="conversation-view" className="dsh-chat-surface" hidden={s.activePage !== 'conversation'}>
            <div className="conversation-heading dsh-chat-header">
              <div className="dsh-crumbs">
                <button
                  type="button"
                  className="dsh-crumb-btn"
                  onClick={() => switchPage('projects')}
                >
                  {`📁 ${currentProjectSummary?.label ?? '默认项目空间'}`}
                </button>
                <span className="dsh-crumb-sep">/</span>
                <GradientText id="session-title" className="dsh-session-title">
                  {snapshot
                    ? displayLabel(snapshot)
                    : s.selected
                      ? '正在读取会话…'
                      : '点击左侧“+ 新建会话”开始'}
                </GradientText>
              </div>
              <div className="conversation-meta">
                <span id="history-note" className="meta-pill muted" hidden={!historyNote}>
                  {historyNote}
                </span>
                <span
                  id="poll-status"
                  className="meta-pill muted"
                  role="status"
                  hidden={!s.pollStatus || s.pollStatus === '已同步'}
                >
                  {s.pollStatus}
                </span>
                <span
                  id="session-state"
                  className={`state-label state-${stateName(snapshot)}`}
                >
                  {snapshot ? stateLabel(snapshot, view) : '等待选择'}
                </span>
              </div>
            </div>

            <p id="global-error" className="error" role="alert" hidden={!s.globalError}>
              {s.globalError}
            </p>

            <SpotlightCard
              as="section"
              id="blocked-notice"
              className="notice"
              hidden={!attention}
              aria-labelledby="blocked-title"
            >
              <div className="notice-bar">
                <div className="title-with-hint">
                  <h2 id="blocked-title">会话暂时不能继续</h2>
                  <HintPopover
                    chip
                    label="查看原因与说明"
                    bubbleId="blocked-text"
                  >
                    {blockedText}
                  </HintPopover>
                </div>
                <button
                  ref={bind({ click: openForkDialog })}
                  id="open-fork"
                  type="button"
                  hidden={!showForkBtn}
                  disabled={!forkEnabled}
                  onClick={openForkDialog}
                >
                  确认后 Fork 为普通聊天
                </button>
              </div>
            </SpotlightCard>

            <section aria-label="会话历史" className="history-section dsh-chat-scroll">
              <div id="messages" className="messages dsh-chat-column">
                {!messages.length ? (
                  <div className="empty dsh-hero">
                    <div className="dsh-hero-headline">
                      <span className="dsh-hero-mark">C</span>
                      <h2>{view?.snapshot ? '探索未至之境 · 从一条消息开始' : '探索未至之境 · 继续一段对话'}</h2>
                      <span className="dsh-preview-badge">CAYW v3</span>
                      <TooltipBadge
                        className="hint-trigger"
                        label="?"
                        tip={
                          view?.snapshot
                            ? '输入任务后，Runtime 会在此保留对话和工具记录。'
                            : '从左侧选择已有会话，或点击“+ 新建会话”。'
                        }
                      />
                    </div>
                    <div className="dsh-hero-workspace-bar">
                      <button
                        type="button"
                        className="dsh-hero-ws-pill"
                        onClick={() => switchPage('projects')}
                      >
                        {`📁 工作区：${currentProjectSummary?.label ?? '默认项目空间'}${currentProjectSummary?.workspace ? ` (${currentProjectSummary.workspace})` : ''} ▾`}
                      </button>
                      <button
                        type="button"
                        className="dsh-hero-ws-pill"
                        onClick={() => openNewProjectModal()}
                      >
                        + 新开项目目录
                      </button>
                      <button
                        type="button"
                        className="dsh-hero-ws-pill"
                        onClick={() => switchPage('permissions')}
                      >
                        {`🔒 权限模式：${currentPresetObj.label}`}
                      </button>
                    </div>
                    <div className="starter-grid dsh-starter-pills">
                      {STARTER_PROMPTS.map((item, idx) => (
                        <SpotlightCard
                          as="button"
                          key={idx}
                          type="button"
                          className="starter-card dsh-starter-card"
                          onClick={() =>
                            trackAsync(
                              (async () => {
                                if (!s.selected) {
                                  await createNewSession();
                                }
                                s.promptText = item.prompt;
                                if (promptInputRef.current) {
                                  promptInputRef.current.value = item.prompt;
                                  promptInputRef.current.focus();
                                }
                                forceRender();
                              })(),
                            )
                          }
                        >
                          <strong>{item.title}</strong>
                          <span>{item.desc}</span>
                        </SpotlightCard>
                      ))}
                    </div>
                  </div>
                ) : (
                  messages.map((message, index) => {
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
                    const isOp = presentation.kind !== 'text';
                    return (
                      <SpotlightCard
                        as="article"
                        key={key}
                        className={`message ${role}${isOp ? ' operation-record dsh-tool-card' : ''}`}
                      >
                        <div className="role">{ROLE_LABELS[role]}</div>
                        {presentation.kind === 'text' ? (
                          <FormattedBody text={presentation.text} />
                        ) : presentation.kind === 'tool_call' ? (
                          <>
                            {presentation.text ? (
                              <FormattedBody text={presentation.text} />
                            ) : null}
                            {presentation.calls.map((call, callIndex) => {
                              const callKey = `${key}/${callIndex}`;
                              const title =
                                call.name === 'execute_code'
                                  ? call.arguments?.action === 'wait'
                                    ? '等待任务结果'
                                    : '运行 Luau 代码单元（execute_code）'
                                  : `使用工具：${clip(call.name, 100)}`;
                              return (
                                <details
                                  key={callKey}
                                  className="record-details dsh-disclosure-row"
                                  open={expandedSet.has(callKey)}
                                  onToggle={e =>
                                    toggleRecordExpand(callKey, e.currentTarget.open)
                                  }
                                >
                                  <summary>{title}</summary>
                                  <pre>{recordText(call)}</pre>
                                </details>
                              );
                            })}
                          </>
                        ) : (
                          <>
                            <div className="result-header">
                              <strong
                                className={`result-title ${presentation.status ?? ''}`}
                              >
                                {presentation.title}
                              </strong>
                              <details
                                className="record-details inline-details"
                                open={expandedSet.has(key)}
                                onToggle={e =>
                                  toggleRecordExpand(key, e.currentTarget.open)
                                }
                              >
                                <summary>查看完整执行记录</summary>
                                <pre>{presentation.raw}</pre>
                              </details>
                            </div>
                            {presentation.text ? (
                              <p className="result-description">
                                {presentation.text}
                              </p>
                            ) : null}
                            {(presentation.values ?? []).map((val, vIdx) => (
                              <div key={vIdx} className="file-output-box">
                                <div className="output-label">{`📄 ${val.label}`}</div>
                                <div className="body output-value">{val.text}</div>
                              </div>
                            ))}
                          </>
                        )}
                        {message.error_message ? (
                          <p className="effect-warning">
                            {clip(message.error_message, 2000)}
                          </p>
                        ) : null}
                        {message.outcome_unknown ? (
                          <p className="effect-warning">
                            此工具调用的真实结果待宿主核对。
                          </p>
                        ) : null}
                      </SpotlightCard>
                    );
                  })
                )}
              </div>
            </section>

            <section
              id="live-section"
              className="live-section dsh-live-stream"
              aria-labelledby="live-heading"
              hidden={!view?.liveText}
            >
              <h2 id="live-heading" className="small">
                正在生成回复…
              </h2>
              <pre id="live-text" aria-live="off">
                {view?.liveText ?? ''}
              </pre>
            </section>

            <section
              id="approvals"
              className="approval-section dsh-approval-dock"
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
                  if (scope.path !== undefined) scopeLines.push(['文件', scope.path, 400]);
                  if (scope.profile !== undefined) scopeLines.push(['预设任务', scope.profile, 400]);
                  if (scope.query !== undefined) scopeLines.push(['检索内容', scope.query, 400]);
                  if (scope.filter !== undefined) scopeLines.push(['测试范围', scope.filter, 400]);
                  if (scope.server !== undefined) scopeLines.push(['工具连接', scope.server, 400]);
                  if (scope.tool !== undefined) scopeLines.push(['工具', scope.tool, 400]);
                  if (scope.name !== undefined) scopeLines.push(['名称', scope.name, 400]);
                  if (scope.commit !== undefined) scopeLines.push(['提交', scope.commit, 400]);
                  if (scope.overwrite === true) scopeLines.push(['文件操作', '覆盖现有文件', 400]);
                  if (typeof scope.content === 'string') scopeLines.push(['写入内容', scope.content, 600]);
                  if (typeof scope.old === 'string') scopeLines.push(['替换原文', scope.old, 400]);
                  if (typeof scope.new === 'string') scopeLines.push(['替换为', scope.new, 400]);
                }
                const isDeciding =
                  typeof id === 'string' &&
                  s.approvalPending.has(`${snapshot?.id}/${id}`);

                return (
                  <SpotlightCard
                    as="article"
                    key={id ?? idx}
                    border
                    className="approval-card dsh-approval-card"
                  >
                    <div className="dsh-approval-strip">
                      <span className="dsh-approval-dot" />
                      <strong>等待审批 · 请求你的授权</strong>
                      <TooltipBadge
                        label="精确参数范围 ?"
                        tip="仅限本次列明的能力和完全相同的参数；允许此任务也不会扩展到其他文件或内容。"
                      />
                    </div>
                    <p className="dsh-approval-headline">{`操作：${operation}`}</p>
                    {scopeLines.map(([label, val, limit], sIdx) => (
                      <p key={sIdx} className="small dsh-approval-command">
                        {`${label}：${clip(val, limit)}`}
                      </p>
                    ))}
                    <details>
                      <summary>查看完整授权参数</summary>
                      <pre>{recordText(approval)}</pre>
                    </details>
                    {typeof id !== 'string' ? (
                      <p className="error">审批记录缺少 ID，请在宿主中处理。</p>
                    ) : (
                      <div className="approval-actions dsh-approval-actions">
                        {[
                          ['allow_once', '允许一次', ''],
                          ['allow_task', '允许此任务', 'secondary'],
                          ['deny', '拒绝', 'secondary deny'],
                        ].map(([decision, btnLabel, cls]) => (
                          <button
                            key={decision}
                            ref={bind({
                              click: () =>
                                trackAsync(decide(snapshot.id, id, decision)),
                            })}
                            type="button"
                            className={cls}
                            disabled={isDeciding || approvalButtonsDisabled}
                            onClick={() =>
                              trackAsync(decide(snapshot.id, id, decision))
                            }
                          >
                            {isDeciding ? '正在记录…' : btnLabel}
                          </button>
                        ))}
                      </div>
                    )}
                  </SpotlightCard>
                );
              })}
            </section>

            <div className="dsh-composer-dock">
              {s.permissionPopoverOpen ? (
                <div className="dsh-permission-popover" role="dialog" aria-label="切换权限模式">
                  <div className="dsh-popover-header">
                    <strong>会话权限模式 (`/permission`)</strong>
                    <button
                      type="button"
                      className="quiet"
                      onClick={() => {
                        s.permissionPopoverOpen = false;
                        forceRender();
                      }}
                    >
                      ✕
                    </button>
                  </div>
                  <div className="dsh-preset-list">
                    {PERMISSION_PRESETS.map(preset => (
                      <button
                        key={preset.id}
                        type="button"
                        className={`dsh-preset-option${s.permissionPreset === preset.id ? ' is-selected' : ''}`}
                        onClick={() => selectPermissionPreset(preset.id)}
                      >
                        <div className="dsh-preset-option-top">
                          <strong>{`${preset.label} (${preset.enLabel})`}</strong>
                          {s.permissionPreset === preset.id ? <span>✓ 当前</span> : null}
                        </div>
                        <span className="small muted">{preset.desc}</span>
                      </button>
                    ))}
                  </div>
                  <div className="dsh-popover-footer">
                    <button
                      type="button"
                      className="secondary"
                      onClick={() => {
                        s.permissionPopoverOpen = false;
                        switchPage('permissions');
                      }}
                    >
                      查看已授权能力与撤销记录 →
                    </button>
                  </div>
                </div>
              ) : null}

              <SpotlightCard
                as="form"
                id="prompt-form"
                border
                className="composer dsh-input-card"
                handlers={{ submit: e => trackAsync(handleSendPrompt(e)) }}
                onSubmit={e => trackAsync(handleSendPrompt(e))}
              >
                <label htmlFor="prompt" className="visually-hidden">
                  发送消息
                </label>
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
                  className="dsh-composer-textarea"
                  rows={3}
                  maxLength={32000}
                  disabled={promptDisabled}
                  placeholder="给智能体发消息（Enter 发送，Shift + Enter 换行）"
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
                      (s.enterBehavior === 'enter' || e.metaKey || e.ctrlKey)
                    ) {
                      e.preventDefault();
                      trackAsync(handleSendPrompt(e));
                    }
                  }}
                />
                <div className="composer-actions dsh-composer-row">
                  <div className="dsh-composer-tools">
                    <button
                      id="composer-workspace-btn"
                      type="button"
                      className="dsh-chip-btn"
                      title="切换工作区项目"
                      onClick={() => switchPage('projects')}
                    >
                      {`📁 ${currentProjectSummary?.label ?? '默认项目空间'}`}
                    </button>
                    <button
                      id="composer-new-project-btn"
                      type="button"
                      className="dsh-chip-btn"
                      title="浏览本地目录并新开项目"
                      onClick={() => openNewProjectModal()}
                    >
                      + 新开项目
                    </button>
                    <button
                      id="composer-permission-btn"
                      type="button"
                      className={`dsh-chip-btn dsh-perm-chip${s.permissionPreset === 'danger-full-access' ? ' is-danger' : ''}`}
                      title="切换权限预设或管理会话授权"
                      onClick={() => {
                        s.permissionPopoverOpen = !s.permissionPopoverOpen;
                        forceRender();
                      }}
                    >
                      {`🔒 ${currentPresetObj.label} ▾`}
                    </button>
                    <button
                      id="composer-model-btn"
                      type="button"
                      className="dsh-chip-btn"
                      title="查看模型与网关设置"
                      onClick={() => openSettings('models')}
                    >
                      ⚡ deepseek-v4.1-flash
                    </button>
                  </div>
                  <div className="action-buttons dsh-composer-trailing">
                    <span id="prompt-status" className="small muted" role="status">
                      {promptStatus}
                    </span>
                    <button
                      ref={bind({ click: () => trackAsync(handleCancel()) })}
                      id="cancel-button"
                      className="secondary dsh-stop-btn"
                      type="button"
                      disabled={cancelDisabled}
                      onClick={() => trackAsync(handleCancel())}
                    >
                      停止
                    </button>
                    <button
                      id="send-button"
                      className="dsh-send-btn"
                      type="submit"
                      disabled={sendDisabled}
                    >
                      {pending ? '等待完成…' : '↑ 发送'}
                    </button>
                  </div>
                </div>
              </SpotlightCard>
            </div>

            <details className="event-details dsh-event-drawer">
              <summary data-tip="最近 120 条已消费事件；未识别事件按原始记录展示">
                <span>诊断与事件记录</span>
                <span id="event-count" className="muted">
                  {`${events.length} · 游标 ${view?.cursor ?? 0}`}
                </span>
              </summary>
              <div id="event-log">
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

          <section
            id="projects-page"
            className="management-page"
            hidden={s.activePage !== 'projects'}
            aria-labelledby="projects-heading"
          >
            <div className="conversation-heading">
              <div>
                <ShinyText>工作区与项目空间</ShinyText>
                <div className="title-with-hint">
                  <GradientText id="projects-heading">本地项目空间</GradientText>
                  <HintPopover ariaLabel="项目空间管理说明">
                    选择目录即可新开独立项目 Runtime；各项目的会话列表、权限授权与事件游标相互隔离。
                  </HintPopover>
                </div>
              </div>
              <div className="approval-actions">
                <button
                  type="button"
                  onClick={() => openNewProjectModal()}
                >
                  📁 浏览目录新开项目
                </button>
                <button
                  ref={bind({ click: () => trackAsync(refreshProjects()) })}
                  id="refresh-projects"
                  className="secondary"
                  type="button"
                  disabled={s.refreshProjectsBusy}
                  onClick={() => trackAsync(refreshProjects())}
                >
                  刷新空间
                </button>
              </div>
            </div>

            <SpotlightCard
              as="form"
              id="add-project-form"
              border
              className="project-add-bar"
              handlers={{ submit: e => trackAsync(handleAddProject(e, true)) }}
              onSubmit={e => trackAsync(handleAddProject(e, true))}
            >
              <input
                ref={projectWsInputRef}
                id="project-workspace-input"
                type="text"
                maxLength={1024}
                required
                placeholder="输入本地项目目录路径，例如 /Volumes/ORICO/CAYW"
                value={s.projectWorkspaceInput}
                onChange={e => {
                  s.projectWorkspaceInput = e.target.value;
                  forceRender();
                }}
              />
              <input
                ref={projectLabelInputRef}
                id="project-label-input"
                type="text"
                maxLength={160}
                placeholder="空间名称（可选，默认取目录名）"
                value={s.projectLabelInput}
                onChange={e => {
                  s.projectLabelInput = e.target.value;
                  forceRender();
                }}
              />
              <button
                id="add-project-button"
                type="submit"
                disabled={s.addProjectBusy}
              >
                {s.addProjectBusy ? '正在启动项目…' : '+ 新开项目空间'}
              </button>
            </SpotlightCard>

            <div className="quick-paths">
              <span className="small muted">常用目录快填：</span>
              {QUICK_WORKSPACES.map(([p, lbl]) => (
                <button
                  key={p}
                  type="button"
                  className="quiet path-chip"
                  onClick={() => {
                    s.projectWorkspaceInput = p;
                    s.projectLabelInput = lbl;
                    if (projectWsInputRef.current) projectWsInputRef.current.value = p;
                    if (projectLabelInputRef.current) projectLabelInputRef.current.value = lbl;
                    forceRender();
                  }}
                >
                  {`📁 ${lbl}`}
                </button>
              ))}
            </div>

            <p id="projects-status" className="small muted" role="status">
              {projectsStatusText}
            </p>

            <div id="project-list" className="management-grid">
              {projectSummaries.map(item => {
                const isCurrent = item.id === currentProjectId();
                return (
                  <SpotlightCard
                    as="article"
                    key={item.id}
                    tilt
                    border
                    className={`management-card${isCurrent ? ' is-active' : ''}`}
                  >
                    <div className="card-header">
                      <strong>{item.label}</strong>
                      <span className="state-label">
                        {isCurrent ? '当前使用中' : '可切换'}
                      </span>
                    </div>
                    <div className="stat-pills">
                      {item.workspace ? (
                        <span
                          className="stat-pill mono"
                          data-tip={`本地项目目录：${item.workspace}（IPC 端点：${item.address}）`}
                        >
                          {`📁 ${item.workspace}`}
                        </span>
                      ) : null}
                      <span
                        className="stat-pill"
                        data-tip={`该项目当前共有 ${item.sessionCount} 个会话`}
                      >
                        {`${item.sessionCount} 个会话`}
                      </span>
                      <span
                        className="stat-pill"
                        data-tip={`项目运行状态：${item.statusLabel}（IPC：${item.address}）`}
                      >
                        {item.statusLabel}
                      </span>
                      <span className="stat-pill mono">{item.address}</span>
                      {item.frozenCount ? (
                        <span className="stat-pill warn">
                          {`${item.frozenCount} 已冻结`}
                        </span>
                      ) : null}
                      {item.unresolvedCount ? (
                        <span className="stat-pill danger">
                          {`${item.unresolvedCount} 待核对`}
                        </span>
                      ) : null}
                    </div>
                    <div className="approval-actions">
                      <button
                        ref={bind({
                          click: () => trackAsync(selectProject(item.id)),
                        })}
                        type="button"
                        className={isCurrent ? 'secondary' : ''}
                        onClick={() => trackAsync(selectProject(item.id))}
                      >
                        {isCurrent ? '查看空间会话' : '切换到此空间'}
                      </button>
                      <button
                        ref={bind({
                          click: () =>
                            trackAsync(
                              (async () => {
                                await selectProject(item.id);
                                await createNewSession();
                              })(),
                            ),
                        })}
                        type="button"
                        className="secondary"
                        onClick={() =>
                          trackAsync(
                            (async () => {
                              await selectProject(item.id);
                              await createNewSession();
                            })(),
                          )
                        }
                      >
                        + 在此空间新建会话
                      </button>
                      {!item.default ? (
                        <button
                          ref={bind({
                            click: () => trackAsync(removeProject(item.id)),
                          })}
                          type="button"
                          className="secondary deny"
                          onClick={() => trackAsync(removeProject(item.id))}
                        >
                          移除
                        </button>
                      ) : null}
                    </div>
                  </SpotlightCard>
                );
              })}
            </div>
          </section>

          <section
            id="permissions-page"
            className="management-page"
            hidden={s.activePage !== 'permissions'}
            aria-labelledby="permissions-heading"
          >
            <div className="conversation-heading">
              <div>
                <ShinyText>权限与安全控制</ShinyText>
                <div className="title-with-hint">
                  <GradientText id="permissions-heading">
                    会话权限预设与已授权管理
                  </GradientText>
                  <HintPopover ariaLabel="权限说明">
                    所有 CodeMode 工具调用均经过宿主参数校验与权限门禁。你可以在此切换默认权限模式、查看当前会话已授予的精确参数授权，或随时撤销任意授权。
                  </HintPopover>
                </div>
              </div>
              <button
                id="refresh-permissions"
                className="secondary"
                type="button"
                disabled={s.refreshPermissionsBusy}
                onClick={() => trackAsync(refreshPermissions())}
              >
                刷新权限记录
              </button>
            </div>

            <div className="dsh-settings-card">
              <div className="dsh-permission-row">
                <div className="dsh-permission-row-text">
                  <strong>默认权限模式（Permission Preset）</strong>
                  <span className="small muted">
                    选择当前与新建会话的权限门禁策略。启用 Full access 前需显式确认风险。
                  </span>
                </div>
                <div className="dsh-preset-pills">
                  {PERMISSION_PRESETS.map(preset => (
                    <button
                      key={preset.id}
                      type="button"
                      className={`dsh-preset-pill${s.permissionPreset === preset.id ? ' is-active' : ''}`}
                      onClick={() => selectPermissionPreset(preset.id)}
                    >
                      {`${preset.label} (${preset.enLabel})`}
                    </button>
                  ))}
                </div>
              </div>
              <p className="small muted">{currentPresetObj.desc}</p>
            </div>

            <p id="permissions-status" className="small muted" role="status">
              {s.permissionsStatus ||
                `当前会话：${snapshot ? displayLabel(snapshot) : '未选择'} · ${s.sessionGrants.length} 项生效授权`}
            </p>

            <div className="management-grid">
              <SpotlightCard as="article" border className="management-card">
                <div className="card-header">
                  <strong>当前会话生效授权（`session.grants`）</strong>
                  <span className="state-label">{`${s.sessionGrants.length} 项`}</span>
                </div>
                {!s.sessionGrants.length ? (
                  <p className="small muted">
                    当前会话暂无持久任务授权。当你在审批卡片点击“允许此任务”后，授权条目会显示在此处并可随时撤销。
                  </p>
                ) : (
                  <div className="dsh-grants-list">
                    {s.sessionGrants.map((grant, idx) => {
                      const cap = grant.capability ?? grant.input?.capability ?? '未知能力';
                      const scopeStr =
                        typeof grant.scope === 'string'
                          ? grant.scope
                          : JSON.stringify(grant.scope ?? {});
                      const key = `${cap}:${scopeStr}`;
                      return (
                        <div key={`${key}/${idx}`} className="bound-session-row">
                          <div>
                            <strong>{CAPABILITY_LABELS[cap] ?? cap}</strong>
                            <span className="small mono muted">{` · ${clip(scopeStr, 140)}`}</span>
                          </div>
                          <button
                            type="button"
                            className="secondary deny"
                            disabled={s.revokeBusyKey === key}
                            onClick={() => trackAsync(revokeGrant(grant))}
                          >
                            {s.revokeBusyKey === key ? '撤销中…' : '撤销授权'}
                          </button>
                        </div>
                      );
                    })}
                  </div>
                )}
              </SpotlightCard>

              <SpotlightCard as="article" border className="management-card">
                <div className="card-header">
                  <strong>权限与工具审计记录（`session.audit`）</strong>
                  <span className="state-label">{`${s.sessionAudit.length} 条`}</span>
                </div>
                {!s.sessionAudit.length ? (
                  <p className="small muted">
                    当前会话暂无工具审批审计记录。发生工具调用或授权决策后将自动同步。
                  </p>
                ) : (
                  <div className="dsh-audit-list">
                    {s.sessionAudit.slice(-15).map((item, idx) => (
                      <details key={idx} className="record-details">
                        <summary>
                          {`#${idx + 1} · ${clip(
                            item.capability ?? item.kind ?? item.decision ?? '审计条目',
                            80,
                          )}`}
                        </summary>
                        <pre>{recordText(item)}</pre>
                      </details>
                    ))}
                  </div>
                )}
              </SpotlightCard>
            </div>
          </section>

          <section
            id="plugins-page"
            className="management-page"
            hidden={s.activePage !== 'plugins'}
            aria-labelledby="plugins-heading"
          >
            <div className="conversation-heading">
              <div>
                <ShinyText>插件管理</ShinyText>
                <div className="title-with-hint">
                  <GradientText id="plugins-heading">
                    插件与会话绑定
                  </GradientText>
                  <HintPopover ariaLabel="插件管理说明">
                    展示当前项目已绑定的 plugin、版本代际、公开能力与冻结状态。plugin 启停由本机宿主控制；若卸载导致会话冻结，可在副作用核对完毕后显式 Fork 为普通聊天。
                  </HintPopover>
                </div>
              </div>
              <button
                ref={bind({ click: () => trackAsync(refreshPlugins()) })}
                id="refresh-plugins"
                className="secondary"
                type="button"
                disabled={s.refreshPluginsBusy}
                onClick={() => trackAsync(refreshPlugins())}
              >
                刷新插件状态
              </button>
            </div>

            <p id="plugins-status" className="small muted" role="status">
              {pluginsStatusText}
            </p>

            <div id="plugin-list" className="management-grid">
              {!pluginItems.length ? (
                OPTIONAL_PLUGIN_CATALOG.map(mod => (
                  <SpotlightCard
                    as="article"
                    key={mod.name}
                    tilt
                    border
                    className="management-card"
                  >
                    <div className="card-header">
                      <strong>{mod.name}</strong>
                      <span className="state-label">按需插拔</span>
                    </div>
                    <div className="stat-pills">
                      <span className="stat-pill">{mod.role}</span>
                      <span className="stat-pill">0 个绑定会话</span>
                    </div>
                    <p className="small muted card-desc">{mod.desc}</p>
                  </SpotlightCard>
                ))
              ) : (
                pluginItems.map(plugin => {
                  const shortReg =
                    plugin.registryId.length > 14
                      ? `${plugin.registryId.slice(0, 8)}…`
                      : plugin.registryId;
                  return (
                    <SpotlightCard
                      as="article"
                      key={plugin.name}
                      tilt
                      border
                      className={`management-card${plugin.frozen ? ' is-frozen' : ''}`}
                    >
                      <div className="card-header">
                        <strong>{plugin.name}</strong>
                        <span className="state-label">{plugin.statusLabel}</span>
                      </div>
                      <div className="stat-pills">
                        <span
                          className="stat-pill"
                          data-tip={`插件版本：${plugin.version}`}
                        >
                          {`版本 ${plugin.version}`}
                        </span>
                        <span
                          className="stat-pill mono"
                          data-tip={`实例标识：${plugin.registryId}`}
                        >
                          {shortReg}
                        </span>
                        {plugin.generation !== null ? (
                          <span className="stat-pill">
                            {`代际 #${plugin.generation}`}
                          </span>
                        ) : null}
                        <span className="stat-pill">
                          {`${plugin.boundSessions.length} 个绑定会话`}
                        </span>
                        {plugin.reasons.length ? (
                          <HintPopover chip warn label="冻结原因 ?">
                            {plugin.reasons.join('；')}
                          </HintPopover>
                        ) : null}
                      </div>
                      {plugin.capabilities.length ? (
                        <div className="stat-pills">
                          {plugin.capabilities.map(cap => (
                            <span
                              key={cap}
                              className="stat-pill mono"
                              data-tip={`插件公开能力：${cap}`}
                            >
                              {cap}
                            </span>
                          ))}
                        </div>
                      ) : null}
                      {plugin.boundSessions.map(bound => (
                        <div key={bound.id} className="bound-session-row">
                          <span className="small">
                            {`${bound.label}（${bound.stateLabel}）`}
                          </span>
                          <div className="approval-actions">
                            <button
                              type="button"
                              className="secondary"
                              onClick={() =>
                                trackAsync(
                                  (async () => {
                                    switchPage('conversation');
                                    await selectSession(bound.id);
                                  })(),
                                )
                              }
                            >
                              打开会话
                            </button>
                            {bound.frozen ? (
                              <button
                                type="button"
                                disabled={!bound.canFork}
                                onClick={() =>
                                  trackAsync(
                                    (async () => {
                                      switchPage('conversation');
                                      await selectSession(bound.id);
                                      openForkDialog();
                                    })(),
                                  )
                                }
                              >
                                确认后 Fork 为普通聊天
                              </button>
                            ) : null}
                          </div>
                        </div>
                      ))}
                    </SpotlightCard>
                  );
                })
              )}
            </div>

            <div className="capability-catalog">
              <div className="section-heading">
                <div className="title-with-hint">
                  <h2>工作区内置能力（CodeMode）</h2>
                  <HintPopover ariaLabel="CodeMode 内置能力说明">
                    模型通过 Luau 单元调用以下受控能力；每次具体调用都经过宿主参数校验、权限审批与结果配对。
                  </HintPopover>
                </div>
                <span className="small muted">
                  {s.cachedCapabilities.length
                    ? `已加载 ${s.cachedCapabilities.length} 项宿主能力`
                    : '共 4 组预置工作区能力'}
                </span>
              </div>
              <div className="management-grid">
                {BUILTIN_CAPABILITY_GROUPS.map(group => (
                  <SpotlightCard
                    as="article"
                    key={group.title}
                    border
                    className="management-card capability-card"
                  >
                    <div className="card-header">
                      <strong>{group.title}</strong>
                      <span className="state-label">{group.badge}</span>
                    </div>
                    <div className="capability-items">
                      {group.items.map(([code, desc]) => (
                        <div key={code} className="capability-row">
                          <code className="inline-code">{code}</code>
                          <span className="small muted">{desc}</span>
                        </div>
                      ))}
                    </div>
                  </SpotlightCard>
                ))}
              </div>
            </div>
          </section>
        </main>
      </div>

      {/* DSH Settings Modal (@deepseek-ai/dsh-client-ui-settings-general style) */}
      {s.settingsOpen ? (
        <div className="dsh-modal-overlay" role="presentation">
          <div
            className="dsh-modal-mask"
            onClick={() => {
              s.settingsOpen = false;
              forceRender();
            }}
          />
          <div
            id="settings-dialog"
            className="dsh-settings-panel"
            role="dialog"
            aria-modal="true"
            aria-labelledby="settings-modal-title"
          >
            <nav className="dsh-settings-nav" aria-label="设置分区">
              <div id="settings-modal-title" className="dsh-settings-nav-title">
                设置
              </div>
              <div className="dsh-settings-nav-list">
                {[
                  ['general', '⚙ 通用设置'],
                  ['permissions', '🔒 权限与安全'],
                  ['models', '⚡ 模型与连接'],
                  ['plugins', '🧩 插件与能力'],
                ].map(([tabId, label]) => (
                  <button
                    key={tabId}
                    type="button"
                    className={`dsh-settings-nav-cell${s.settingsTab === tabId ? ' is-active' : ''}`}
                    onClick={() => openSettings(tabId)}
                  >
                    {label}
                  </button>
                ))}
              </div>
            </nav>

            <div className="dsh-settings-content">
              <div className="dsh-settings-header">
                <strong>
                  {{
                    general: '通用与外观设置',
                    permissions: '权限模式与会话授权',
                    models: '模型提供方与本机 Runtime 连接',
                    plugins: '插件绑定与 CodeMode 能力目录',
                  }[s.settingsTab]}
                </strong>
                <button
                  id="close-settings-button"
                  type="button"
                  className="dsh-modal-close"
                  aria-label="关闭设置"
                  onClick={() => {
                    s.settingsOpen = false;
                    forceRender();
                  }}
                >
                  ✕
                </button>
              </div>

              <div className="dsh-settings-options">
                {s.settingsTab === 'general' ? (
                  <div className="dsh-settings-section">
                    <div className="dsh-appearance-group">
                      <div className="dsh-setting-title">外观主题（Appearance）</div>
                      <div className="dsh-theme-cube-row">
                        {[
                          ['light', '☀️ 浅色模式', 'Light'],
                          ['dark', '🌙 深色模式', 'Dark'],
                          ['system', '💻 跟随系统', 'System'],
                        ].map(([mode, title, sub]) => (
                          <button
                            key={mode}
                            type="button"
                            className={`dsh-theme-cube${s.themeMode === mode ? ' is-selected' : ''}`}
                            onClick={() => applyTheme(mode)}
                          >
                            <strong>{title}</strong>
                            <span className="small muted">{sub}</span>
                          </button>
                        ))}
                      </div>
                    </div>

                    <div className="dsh-setting-row">
                      <div className="dsh-setting-row-text">
                        <div className="dsh-setting-title">默认权限模式（Permission）</div>
                        <div className="dsh-setting-desc">
                          选择新会话与当前工作区的权限策略；Full access 需风险确认。
                        </div>
                      </div>
                      <select
                        aria-label="默认权限模式"
                        className="dsh-setting-select"
                        value={s.permissionPreset}
                        onChange={e => selectPermissionPreset(e.target.value)}
                      >
                        {PERMISSION_PRESETS.map(p => (
                          <option key={p.id} value={p.id}>
                            {`${p.label} (${p.enLabel})`}
                          </option>
                        ))}
                      </select>
                    </div>

                    <div className="dsh-setting-row">
                      <div className="dsh-setting-row-text">
                        <div className="dsh-setting-title">发送快捷键（Enter 键行为）</div>
                        <div className="dsh-setting-desc">
                          设置输入框按下 Enter 时的默认发送方式。
                        </div>
                      </div>
                      <select
                        aria-label="发送快捷键"
                        className="dsh-setting-select"
                        value={s.enterBehavior}
                        onChange={e => {
                          s.enterBehavior = e.target.value;
                          forceRender();
                        }}
                      >
                        <option value="enter">Enter 直接发送（Shift+Enter 换行）</option>
                        <option value="cmd-enter">Cmd/Ctrl + Enter 发送（Enter 换行）</option>
                      </select>
                    </div>
                  </div>
                ) : null}

                {s.settingsTab === 'permissions' ? (
                  <div className="dsh-settings-section">
                    <div className="dsh-setting-row">
                      <div className="dsh-setting-row-text">
                        <div className="dsh-setting-title">当前权限模式</div>
                        <div className="dsh-setting-desc">{currentPresetObj.desc}</div>
                      </div>
                      <div className="dsh-preset-pills">
                        {PERMISSION_PRESETS.map(p => (
                          <button
                            key={p.id}
                            type="button"
                            className={`dsh-preset-pill${s.permissionPreset === p.id ? ' is-active' : ''}`}
                            onClick={() => selectPermissionPreset(p.id)}
                          >
                            {p.label}
                          </button>
                        ))}
                      </div>
                    </div>

                    <div className="dsh-setting-block">
                      <div className="card-header">
                        <strong>当前会话已授权项（`session.grants`）</strong>
                        <button
                          type="button"
                          className="quiet"
                          onClick={() => trackAsync(refreshPermissions())}
                        >
                          ↻ 刷新
                        </button>
                      </div>
                      {!s.sessionGrants.length ? (
                        <p className="small muted">暂无按任务记住的持久化授权记录。</p>
                      ) : (
                        s.sessionGrants.map((grant, idx) => {
                          const cap = grant.capability ?? grant.input?.capability ?? '未知能力';
                          const scopeStr =
                            typeof grant.scope === 'string'
                              ? grant.scope
                              : JSON.stringify(grant.scope ?? {});
                          const key = `${cap}:${scopeStr}`;
                          return (
                            <div key={`${key}/${idx}`} className="bound-session-row">
                              <div>
                                <strong>{CAPABILITY_LABELS[cap] ?? cap}</strong>
                                <span className="small mono muted">{` · ${clip(scopeStr, 120)}`}</span>
                              </div>
                              <button
                                type="button"
                                className="secondary deny"
                                disabled={s.revokeBusyKey === key}
                                onClick={() => trackAsync(revokeGrant(grant))}
                              >
                                撤销授权
                              </button>
                            </div>
                          );
                        })
                      )}
                    </div>
                  </div>
                ) : null}

                {s.settingsTab === 'models' ? (
                  <div className="dsh-settings-section">
                    <div className="dsh-provider-card">
                      <div className="dsh-provider-head">
                        <span className="dsh-cred-dot configured" />
                        <strong>DeepSeek / Magpie OpenAI 兼容网关</strong>
                        <span className="dsh-row-tag">已连接</span>
                      </div>
                      <p className="small muted">
                        默认模型：<code>auto/deepseek-v4.1-flash</code> · 端点：<code>http://127.0.0.1:3425/v1</code>
                      </p>
                      <p className="small muted">
                        API 凭据由本机 Runtime 进程持有，从不向浏览器暴露或回显。
                      </p>
                    </div>

                    <div className="dsh-provider-card">
                      <div className="dsh-provider-head">
                        <span
                          className={`dsh-cred-dot ${s.connectionStatus === '已连接本机' ? 'configured' : 'missing'}`}
                        />
                        <strong>本机 Loopback 网关与项目 Runtime</strong>
                        <span className="dsh-row-tag">{`Core 协议 v${CORE_PROTOCOL_VERSION}`}</span>
                      </div>
                      <div className="dsh-grants-list">
                        {projectSummaries.map(proj => (
                          <div key={proj.id} className="bound-session-row">
                            <div>
                              <strong>{proj.label}</strong>
                              <span className="small mono muted">
                                {` · ${proj.address}${proj.workspace ? ` · ${proj.workspace}` : ''}`}
                              </span>
                            </div>
                            <span className="state-label">{proj.statusLabel}</span>
                          </div>
                        ))}
                      </div>
                    </div>
                  </div>
                ) : null}

                {s.settingsTab === 'plugins' ? (
                  <div className="dsh-settings-section">
                    {BUILTIN_CAPABILITY_GROUPS.map(group => (
                      <div key={group.title} className="dsh-provider-card">
                        <div className="card-header">
                          <strong>{group.title}</strong>
                          <span className="dsh-row-tag">{group.badge}</span>
                        </div>
                        <div className="capability-items">
                          {group.items.map(([code, desc]) => (
                            <div key={code} className="capability-row">
                              <code className="inline-code">{code}</code>
                              <span className="small muted">{desc}</span>
                            </div>
                          ))}
                        </div>
                      </div>
                    ))}
                  </div>
                ) : null}
              </div>
            </div>
          </div>
        </div>
      ) : null}

      {/* DSH New Project / Directory Picker Modal (@deepseek-ai/dsh-client-ui-directory-picker-browse style) */}
      {s.newProjectModalOpen ? (
        <div className="dsh-modal-overlay" role="presentation">
          <div
            className="dsh-modal-mask"
            onClick={() => {
              s.newProjectModalOpen = false;
              forceRender();
            }}
          />
          <div
            id="new-project-dialog"
            className="dsh-dir-dialog"
            role="dialog"
            aria-modal="true"
            aria-labelledby="dir-dialog-title"
          >
            <div className="dsh-dir-header">
              <div className="card-header">
                <h2 id="dir-dialog-title" className="dsh-dir-title">
                  选择工作区目录 · 新开项目
                </h2>
                <button
                  type="button"
                  className="dsh-modal-close"
                  onClick={() => {
                    s.newProjectModalOpen = false;
                    forceRender();
                  }}
                >
                  ✕
                </button>
              </div>
              <div className="dsh-crumb-bar">
                {s.browseParent ? (
                  <button
                    type="button"
                    className="quiet dsh-up-btn"
                    onClick={() => void browseDirectory(s.browseParent)}
                  >
                    ⬆ 上级目录
                  </button>
                ) : null}
                <input
                  ref={modalProjectWsInputRef}
                  id="modal-project-path"
                  type="text"
                  className="dsh-path-input mono"
                  placeholder="输入或选择本地绝对目录路径，如 /Volumes/ORICO/CAYW"
                  value={s.projectWorkspaceInput}
                  onChange={e => {
                    s.projectWorkspaceInput = e.target.value;
                    forceRender();
                  }}
                  onKeyDown={e => {
                    if (e.key === 'Enter') {
                      e.preventDefault();
                      void browseDirectory(s.projectWorkspaceInput);
                    }
                  }}
                />
                <button
                  type="button"
                  className="secondary"
                  onClick={() => void browseDirectory(s.projectWorkspaceInput)}
                >
                  跳转目录
                </button>
              </div>
              <div className="quick-paths">
                {QUICK_WORKSPACES.map(([p, lbl]) => (
                  <button
                    key={p}
                    type="button"
                    className="quiet path-chip"
                    onClick={() => {
                      s.projectWorkspaceInput = p;
                      s.projectLabelInput = lbl;
                      if (modalProjectWsInputRef.current) modalProjectWsInputRef.current.value = p;
                      if (modalProjectLabelInputRef.current) modalProjectLabelInputRef.current.value = lbl;
                      void browseDirectory(p);
                    }}
                  >
                    {`📁 ${lbl}`}
                  </button>
                ))}
              </div>
            </div>

            <div className="dsh-dir-content">
              {s.browseLoading ? (
                <p className="small muted">正在读取目录…</p>
              ) : (
                <div className="dsh-dir-column">
                  {!visibleBrowseEntries.length ? (
                    <p className="small muted">当前目录下没有子文件夹，可直接选择当前目录或新建文件夹。</p>
                  ) : (
                    visibleBrowseEntries.map(entry => (
                      <button
                        key={entry.path}
                        type="button"
                        className={`dsh-dir-row${s.projectWorkspaceInput === entry.path ? ' is-selected' : ''}`}
                        onClick={() => {
                          s.projectWorkspaceInput = entry.path;
                          if (modalProjectWsInputRef.current) {
                            modalProjectWsInputRef.current.value = entry.path;
                          }
                          forceRender();
                        }}
                        onDoubleClick={() => void browseDirectory(entry.path)}
                      >
                        <span className="dsh-dir-icon">📁</span>
                        <span className="dsh-dir-name">{entry.name}</span>
                        <span
                          className="dsh-dir-enter"
                          onClick={e => {
                            e.stopPropagation();
                            void browseDirectory(entry.path);
                          }}
                        >
                          进入 ›
                        </span>
                      </button>
                    ))
                  )}
                </div>
              )}

              {s.newFolderOpen ? (
                <div className="dsh-new-folder-bar">
                  <input
                    type="text"
                    placeholder="输入新建文件夹名称"
                    value={s.newFolderName}
                    onChange={e => {
                      s.newFolderName = e.target.value;
                      forceRender();
                    }}
                  />
                  <button
                    type="button"
                    onClick={() => {
                      if (s.newFolderName.trim()) {
                        void browseDirectory(s.browsePath, s.newFolderName.trim());
                        s.newFolderName = '';
                        s.newFolderOpen = false;
                      }
                    }}
                  >
                    创建并进入
                  </button>
                  <button
                    type="button"
                    className="secondary"
                    onClick={() => {
                      s.newFolderOpen = false;
                      forceRender();
                    }}
                  >
                    取消
                  </button>
                </div>
              ) : null}

              <div className="dsh-dir-label-bar">
                <input
                  ref={modalProjectLabelInputRef}
                  id="modal-project-label"
                  type="text"
                  maxLength={160}
                  placeholder="项目显示名称（可选，默认使用文件夹名）"
                  value={s.projectLabelInput}
                  onChange={e => {
                    s.projectLabelInput = e.target.value;
                    forceRender();
                  }}
                />
              </div>

              {s.browseError ? (
                <p className="error" role="alert">
                  {s.browseError}
                </p>
              ) : null}
            </div>

            <div className="dsh-dir-footer">
              <button
                type="button"
                className="secondary"
                onClick={() => {
                  s.newFolderOpen = !s.newFolderOpen;
                  forceRender();
                }}
              >
                + 新建文件夹
              </button>
              <label className="small muted dsh-hidden-toggle">
                <input
                  type="checkbox"
                  checked={s.browseShowHidden}
                  onChange={e => {
                    s.browseShowHidden = e.target.checked;
                    forceRender();
                  }}
                />
                显示隐藏文件夹
              </label>
              <div className="dsh-footer-gap" />
              <button
                type="button"
                className="secondary"
                onClick={() => {
                  s.newProjectModalOpen = false;
                  forceRender();
                }}
              >
                取消
              </button>
              <button
                id="confirm-new-project-btn"
                type="button"
                disabled={s.addProjectBusy || !s.projectWorkspaceInput.trim()}
                onClick={e => trackAsync(handleAddProject(e, true))}
              >
                {s.addProjectBusy ? '正在启动 Runtime…' : '新开项目并进入'}
              </button>
            </div>
          </div>
        </div>
      ) : null}

      {/* DSH Full Access Risk Confirmation Modal (@deepseek-ai/dsh-client-ui-permission-presets) */}
      {s.fullAccessConfirmOpen ? (
        <div className="dsh-modal-overlay" role="presentation">
          <div
            className="dsh-modal-mask"
            onClick={() => {
              s.fullAccessConfirmOpen = false;
              forceRender();
            }}
          />
          <div className="dsh-confirm-dialog" role="dialog" aria-modal="true">
            <h3>确认启用 Full access？</h3>
            <p className="small muted">
              启用 Full access 后，agent 将减少确认步骤，并且可以直接执行更多操作，包括敏感操作、文件修改或外部命令。仅建议在你信任当前任务时使用。
            </p>
            <label className="confirm-label">
              <input
                type="checkbox"
                checked={s.fullAccessAcknowledged}
                onChange={e => {
                  s.fullAccessAcknowledged = e.target.checked;
                  forceRender();
                }}
              />
              我已了解风险，并愿意继续
            </label>
            <div className="dialog-actions">
              <button
                type="button"
                className="secondary"
                onClick={() => {
                  s.fullAccessConfirmOpen = false;
                  forceRender();
                }}
              >
                取消
              </button>
              <button
                type="button"
                disabled={!s.fullAccessAcknowledged}
                onClick={confirmFullAccess}
              >
                启用 Full access
              </button>
            </div>
          </div>
        </div>
      ) : null}

      <dialog
        ref={forkDialogRef}
        id="fork-dialog"
        open={s.forkOpen}
        aria-labelledby="fork-heading"
      >
        <div className="heading-with-hint">
          <h2 id="fork-heading">创建普通聊天分支</h2>
          <HintPopover ariaLabel="分支规则说明">
            原强流程会话及记录继续保留冻结状态；新建分支作为普通聊天运行。尚未核对的工具副作用不能通过 Fork 清除。
          </HintPopover>
        </div>
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
          <label className="confirm-label">
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
          <p id="fork-error" className="error" role="alert">
            {s.forkError}
          </p>
          <div className="dialog-actions">
            <button
              ref={bind({ click: closeForkDialog })}
              id="close-fork"
              className="secondary"
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
