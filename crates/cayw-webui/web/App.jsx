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
    };
  }
  const s = stateRef.current;

  const promptInputRef = useRef(null);
  const forkDialogRef = useRef(null);
  const forkLabelInputRef = useRef(null);
  const forkConfirmInputRef = useRef(null);
  const projectWsInputRef = useRef(null);
  const projectLabelInputRef = useRef(null);

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
          response.status,
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

  const handleAddProject = async event => {
    event.preventDefault();
    const ws = (
      projectWsInputRef.current?.value ?? s.projectWorkspaceInput
    ).trim();
    const label = (
      projectLabelInputRef.current?.value ?? s.projectLabelInput
    ).trim();
    if (!ws || s.addProjectBusy) return;
    const login = s.loginGeneration;
    s.addProjectBusy = true;
    report(null);
    forceRender();
    try {
      await rpc('projects.create', label ? { workspace: ws, label } : { workspace: ws });
      if (login !== s.loginGeneration) return;
      s.projectWorkspaceInput = '';
      s.projectLabelInput = '';
      if (projectWsInputRef.current) projectWsInputRef.current.value = '';
      if (projectLabelInputRef.current) projectLabelInputRef.current.value = '';
      await refreshProjects();
    } catch (error) {
      if (login === s.loginGeneration) {
        s.projectsStatus = errorText(error);
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
    if (forkDialogRef.current?.showModal && !forkDialogRef.current.open) {
      try {
        forkDialogRef.current.showModal();
      } catch {
        forkDialogRef.current.open = true;
      }
    }
  };

  const closeForkDialog = () => {
    s.forkOpen = false;
    if (forkDialogRef.current?.close) {
      try {
        forkDialogRef.current.close();
      } catch {
        forkDialogRef.current.open = false;
      }
    }
    forceRender();
  };

  const handleForkSubmit = async event => {
    event?.preventDefault?.();
    const confirmed = Boolean(
      forkConfirmInputRef.current?.checked ?? s.forkConfirm,
    );
    const labelVal = (
      forkLabelInputRef.current?.value ?? s.forkLabel
    ).trim();
    s.forkConfirm = confirmed;
    if (labelVal) s.forkLabel = labelVal;
    if (!confirmed || s.forkBusy || !s.selected || !canFork(activeView())) return;
    const id = s.selected;
    const login = s.loginGeneration;
    s.forkBusy = true;
    s.forkError = '';
    forceRender();
    try {
      const snapshot = await writeRpc(id, 'session.fork', {
        session_id: id,
        label: labelVal,
        confirm: true,
        mode: 'chat',
      });
      if (
        typeof snapshot?.id !== 'string' ||
        !snapshot.id ||
        snapshot.id === id
      ) {
        throw new RpcError(
          'invalid_response',
          '新会话结果不完整，请由宿主核对；不要再次 Fork。',
          true,
        );
      }
      const forkView = applySnapshot(createView(snapshot.id), snapshot, true);
      const source = s.views.get(id);
      if (source && login === s.loginGeneration) {
        rememberView({
          ...source,
          writes: source.writes.filter(write => write.method !== 'session.fork'),
        });
      }
      if (login !== s.loginGeneration) return;
      rememberView(forkView);
      updateSessionSummary(snapshot);
      closeForkDialog();
      await selectSession(snapshot.id);
    } catch (error) {
      if (login === s.loginGeneration) {
        const source = s.views.get(id);
        if (source) {
          rememberView({
            ...source,
            writes: source.writes.map(write =>
              write.method === 'session.fork'
                ? { ...write, phase: 'uncertain' }
                : write,
            ),
          });
        }
        s.forkError = errorText(error);
        forceRender();
      }
    } finally {
      s.forkBusy = false;
      forceRender();
    }
  };

  useEffect(() => {
    const initPromise = trackAsync(connectLocal());
    env?.onInitialConnect?.(initPromise);
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
    document.addEventListener('visibilitychange', onVisibility);
    window.addEventListener('online', onOnline);
    window.addEventListener('pagehide', onPageHide);
    return () => {
      clearTimer(s.timer);
      for (const controller of s.controllers) controller.abort();
      document.removeEventListener('visibilitychange', onVisibility);
      window.removeEventListener('online', onOnline);
      window.removeEventListener('pagehide', onPageHide);
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

  return (
    <>
      <div className="backdrop" aria-hidden="true">
        <div className="backdrop-grid" />
        <div className="backdrop-glow glow-rust" />
        <div className="backdrop-glow glow-luau" />
        <div className="backdrop-noise" />
      </div>
      <a className="skip" href="#conversation">
        跳到会话内容
      </a>
      <header className="topbar">
        <div className="identity">
          <span className="mark" aria-hidden="true">
            C
          </span>
          <div>
            <strong>CAYW</strong>
            <span className="muted">本机会话</span>
          </div>
        </div>
        <div className="connection">
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

      <div id="workspace" className="workspace">
        <aside className="sidebar" aria-labelledby="sessions-heading">
          <nav className="page-nav" aria-label="界面视图">
            <button
              ref={bind({ click: () => switchPage('conversation') })}
              id="nav-conversation"
              className="page-tab"
              type="button"
              aria-current={String(s.activePage === 'conversation')}
              onClick={() => switchPage('conversation')}
            >
              会话
            </button>
            <button
              ref={bind({ click: () => switchPage('projects') })}
              id="nav-projects"
              className="page-tab"
              type="button"
              aria-current={String(s.activePage === 'projects')}
              onClick={() => switchPage('projects')}
            >
              项目空间
            </button>
            <button
              ref={bind({ click: () => switchPage('plugins') })}
              id="nav-plugins"
              className="page-tab"
              type="button"
              aria-current={String(s.activePage === 'plugins')}
              onClick={() => switchPage('plugins')}
            >
              插件管理
            </button>
          </nav>

          <SpotlightCard
            as="button"
            id="current-workspace-btn"
            className="workspace-switcher"
            type="button"
            handlers={{ click: () => switchPage('projects') }}
            data-tip={
              currentProjectSummary?.workspace
                ? `当前目录：${currentProjectSummary.workspace}（点击管理项目空间）`
                : `点击切换或添加本地项目空间（${currentProjectSummary?.address ?? '本机 IPC'}）`
            }
            onClick={() => switchPage('projects')}
          >
            <span className="small muted">当前项目空间</span>
            <strong id="current-workspace-label">
              {currentProjectSummary?.label ?? '默认项目空间'}
            </strong>
          </SpotlightCard>

          <details
            id="session-picker"
            className="session-picker"
            open={s.sessionPickerOpen}
            onToggle={e => {
              s.sessionPickerOpen = e.currentTarget.open;
            }}
          >
            <summary className="session-picker-summary">
              <span>会话</span>
              <span id="session-picker-label" className="muted">
                {clip(snapshot ? displayLabel(snapshot) : '选择或新建', 60)}
              </span>
            </summary>
            <div className="session-picker-content">
              <div className="section-heading">
                <div className="title-with-hint">
                  <h2 id="sessions-heading">会话</h2>
                  <HintPopover ariaLabel="会话同步说明">
                    对话保存在本机。断线后从已消费游标继续接收结果，消息不会自动重发。
                  </HintPopover>
                </div>
                <div className="session-actions">
                  <button
                    ref={bind({ click: () => trackAsync(refreshSessions()) })}
                    id="refresh-sessions"
                    className="quiet"
                    type="button"
                    disabled={s.refreshSessionsBusy}
                    onClick={() => trackAsync(refreshSessions())}
                  >
                    刷新
                  </button>
                  <button
                    ref={bind({ click: () => trackAsync(createNewSession()) })}
                    id="create-button"
                    className="create-btn"
                    type="button"
                    aria-label="新建会话"
                    disabled={s.createBusy}
                    onClick={() => trackAsync(createNewSession())}
                  >
                    {s.createBusy ? '创建中…' : '+ 新建会话'}
                  </button>
                </div>
              </div>
              <p id="list-status" className="small muted" role="status">
                {s.listStatus}
              </p>
              <nav id="session-list" className="session-list" aria-label="选择会话">
                {!s.sessions.length ? (
                  <p className="small muted">
                    暂无会话，点击上方“+ 新建会话”即可开始。
                  </p>
                ) : (
                  s.sessions.map(session => {
                    const shortId = String(session.id ?? '').slice(0, 8);
                    return (
                      <SpotlightCard
                        as="button"
                        key={session.id}
                        className="session-item"
                        type="button"
                        aria-current={String(session.id === s.selected)}
                        handlers={{
                          click: () => trackAsync(selectSession(session.id)),
                        }}
                        onClick={() => trackAsync(selectSession(session.id))}
                      >
                        <strong>{displayLabel(session)}</strong>
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
        </aside>

        <main id="conversation" className="conversation" tabIndex={-1}>
          <div id="conversation-view" hidden={s.activePage !== 'conversation'}>
            <div className="conversation-heading">
              <div>
                <ShinyText>工作空间</ShinyText>
                <GradientText id="session-title">
                  {snapshot
                    ? displayLabel(snapshot)
                    : s.selected
                      ? '正在读取会话…'
                      : '点击左侧“+ 新建会话”开始'}
                </GradientText>
              </div>
              <span id="session-state" className="state-label">
                {snapshot ? stateLabel(snapshot, view) : '等待选择'}
              </span>
            </div>

            <p id="global-error" className="error" role="alert" hidden={!s.globalError}>
              {s.globalError}
            </p>
            <p id="poll-status" className="small muted" role="status">
              {s.pollStatus}
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

            <section
              id="approvals"
              className="approval-section"
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
                    className="approval-card"
                  >
                    <div className="card-header">
                      <h3>请求你的授权</h3>
                      <TooltipBadge
                        label="精确参数范围 ?"
                        tip="仅限本次列明的能力和完全相同的参数；允许此任务也不会扩展到其他文件或内容。"
                      />
                    </div>
                    <p>{`操作：${operation}`}</p>
                    {scopeLines.map(([label, val, limit], sIdx) => (
                      <p key={sIdx} className="small">
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
                      <div className="approval-actions">
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

            <section aria-label="会话历史" className="history-section">
              <p id="history-note" className="small muted">
                {historyNote}
              </p>
              <div id="messages" className="messages">
                {!messages.length ? (
                  <div className="empty">
                    <div className="empty-heading">
                      <h2>{view?.snapshot ? '从一条消息开始' : '继续一段对话'}</h2>
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
                        className={`message ${role}${isOp ? ' operation-record' : ''}`}
                      >
                        <div className="role">{ROLE_LABELS[role]}</div>
                        {presentation.kind === 'text' ? (
                          <div className="body">{presentation.text}</div>
                        ) : presentation.kind === 'tool_call' ? (
                          <>
                            {presentation.text ? (
                              <div className="body">{presentation.text}</div>
                            ) : null}
                            {presentation.calls.map((call, callIndex) => {
                              const callKey = `${key}/${callIndex}`;
                              const title =
                                call.name === 'execute_code'
                                  ? call.arguments?.action === 'wait'
                                    ? '等待任务结果'
                                    : '运行任务'
                                  : `使用工具：${clip(call.name, 100)}`;
                              return (
                                <details
                                  key={callKey}
                                  className="record-details"
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
                            <strong
                              className={`result-title ${presentation.status ?? ''}`}
                            >
                              {presentation.title}
                            </strong>
                            {presentation.text ? (
                              <p className="result-description">
                                {presentation.text}
                              </p>
                            ) : null}
                            {(presentation.values ?? []).map((val, vIdx) => (
                              <React.Fragment key={vIdx}>
                                <div className="output-label">{val.label}</div>
                                <div className="body output-value">{val.text}</div>
                              </React.Fragment>
                            ))}
                            <details
                              className="record-details"
                              open={expandedSet.has(key)}
                              onToggle={e =>
                                toggleRecordExpand(key, e.currentTarget.open)
                              }
                            >
                              <summary>查看完整执行记录</summary>
                              <pre>{presentation.raw}</pre>
                            </details>
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
              className="live-section"
              aria-labelledby="live-heading"
              hidden={!view?.liveText}
            >
              <h2 id="live-heading" className="small">
                正在接收
              </h2>
              <pre id="live-text" aria-live="off">
                {view?.liveText ?? ''}
              </pre>
            </section>

            <details className="event-details">
              <summary data-tip="最近 120 条已消费事件；未识别事件按原始记录展示">
                事件记录{' '}
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

            <SpotlightCard
              as="form"
              id="prompt-form"
              border
              className="composer"
              handlers={{ submit: e => trackAsync(handleSendPrompt(e)) }}
              onSubmit={e => trackAsync(handleSendPrompt(e))}
            >
              <div className="composer-header">
                <label htmlFor="prompt">发送消息</label>
                <TooltipBadge
                  label="快捷键 ?"
                  tip="Enter 换行 · Ctrl / ⌘ + Enter 发送"
                />
              </div>
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
                rows={3}
                maxLength={64000}
                placeholder="输入你想完成的事情…"
                disabled={promptDisabled}
                value={s.promptText}
                onChange={e => {
                  s.promptText = e.target.value;
                  forceRender();
                }}
                onInput={e => {
                  s.promptText = e.currentTarget.value;
                  forceRender();
                }}
                onKeyDown={e => {
                  if (
                    e.key === 'Enter' &&
                    !e.nativeEvent?.isComposing &&
                    (e.ctrlKey || e.metaKey)
                  ) {
                    e.preventDefault();
                    if (!sendDisabled) trackAsync(handleSendPrompt(e));
                  }
                }}
              />
              <div className="composer-footer">
                <span id="prompt-status" className="small muted" role="status">
                  {promptStatus}
                </span>
                <div className="composer-actions">
                  <button
                    ref={bind({ click: () => trackAsync(handleCancel()) })}
                    id="cancel-button"
                    className="secondary"
                    type="button"
                    disabled={cancelDisabled}
                    onClick={() => trackAsync(handleCancel())}
                  >
                    {s.cancelPending ? '正在停止…' : '停止运行'}
                  </button>
                  <button id="send-button" type="submit" disabled={sendDisabled}>
                    {pending ? '等待完成…' : '发送'}
                  </button>
                </div>
              </div>
            </SpotlightCard>
          </div>

          <section
            id="projects-page"
            className="management-page"
            hidden={s.activePage !== 'projects'}
            aria-labelledby="projects-heading"
          >
            <div className="conversation-heading">
              <div>
                <ShinyText>项目空间管理</ShinyText>
                <div className="title-with-hint">
                  <GradientText id="projects-heading">本地项目空间</GradientText>
                  <HintPopover ariaLabel="项目空间管理说明">
                    添加、切换或移除本地项目工作目录；各项目空间的会话列表、事件游标与未确认请求锁相互独立。
                  </HintPopover>
                </div>
              </div>
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

            <SpotlightCard
              as="form"
              id="add-project-form"
              border
              className="project-add-bar"
              handlers={{ submit: e => trackAsync(handleAddProject(e)) }}
              onSubmit={e => trackAsync(handleAddProject(e))}
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
                {s.addProjectBusy ? '添加中…' : '+ 添加项目空间'}
              </button>
            </SpotlightCard>

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
                        {isCurrent ? '当前空间' : item.id}
                      </span>
                    </div>
                    <div className="stat-pills">
                      {item.workspace ? (
                        <span
                          className="stat-pill mono"
                          data-tip={`本地项目目录：${item.workspace}`}
                        >
                          {item.workspace}
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
                        data-tip={`项目运行状态：${item.statusLabel}`}
                      >
                        {item.statusLabel}
                      </span>
                      <span
                        className="stat-pill mono"
                        data-tip={`本机 IPC 端点：${item.address}`}
                      >
                        {item.address}
                      </span>
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
                <div className="empty">
                  <div className="empty-heading">
                    <h2>当前项目暂无插件记录</h2>
                    <TooltipBadge
                      className="hint-trigger"
                      label="?"
                      tip="当会话绑定 StrongFlow 或其他 plugin，或宿主公开插件能力后，可在此查看版本、状态与关联会话。"
                    />
                  </div>
                </div>
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
          </section>
        </main>
      </div>

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
    </>
  );
}
