/* CAYW visual editor — native browser code, no build step, no external libraries. */
(() => {
  'use strict';

  const NODE_ATTR = 'data-cayw-node';
  const MAX_TEXT = 20000;
  const MAX_ATTRIBUTE = 4000;
  const MAX_ID = 128;
  const RELOAD_CONFIRM_MS = 4500;

  const COMPONENTS = [
    { name: 'text', label: '文字', meta: '标题与段落', category: '基础' },
    { name: 'image', label: '图片', meta: '图片与占位图', category: '基础' },
    { name: 'button', label: '按钮', meta: '主要操作入口', category: '基础' },
    { name: 'input', label: '输入框', meta: '表单控件', category: '基础' },
    { name: 'card', label: '卡片', meta: '图文信息卡片', category: '基础' },
    { name: 'container', label: '容器', meta: '分组与布局', category: '基础' },
    { name: 'grid', label: '网格', meta: '多列内容排列', category: '基础' },
    { name: 'hero', label: 'Hero 首屏', meta: '标题与行动按钮', category: '区块', badge: 'CSS 动效' },
    { name: 'bento', label: 'Bento 网格', meta: '不对称卡片组合', category: '区块' },
    { name: 'faq', label: 'FAQ 问答', meta: '常见问题列表', category: '区块' },
    { name: 'cta', label: 'CTA 行动区', meta: '结尾行动号召', category: '区块', badge: 'CSS 动效' }
  ];
  const COMPONENT_NAMES = new Set(COMPONENTS.map((item) => item.name));
  const COMPONENT_BY_NAME = new Map(COMPONENTS.map((item) => [item.name, item]));
  const CATEGORY_ORDER = ['基础', '区块'];

  const CONTAINER_TAGS = new Set([
    'body', 'main', 'header', 'footer', 'nav', 'section', 'article', 'aside',
    'div', 'figure', 'figcaption', 'form', 'fieldset', 'ul', 'ol', 'li', 'dl', 'dt', 'dd',
    'table', 'thead', 'tbody', 'tfoot', 'tr', 'td', 'th', 'details', 'blockquote', 'pre', 'address'
  ]);

  const TABLE_STRUCTURE_TAGS = new Set(['table', 'thead', 'tbody', 'tfoot', 'tr']);

  const STYLE_PROPS = new Set([
    'color', 'background-color', 'background', 'border-color', 'border-width', 'border-style',
    'width', 'height', 'min-width', 'max-width', 'min-height', 'max-height',
    'font-family', 'font-size', 'font-weight', 'line-height', 'text-align',
    'display', 'gap', 'flex-direction', 'grid-template-columns', 'align-items', 'justify-content',
    'padding', 'margin', 'border-radius', 'opacity', 'left', 'top'
  ]);

  const ATTRIBUTE_NAMES = new Set(['src', 'href', 'alt', 'title']);
  const MENU_TABS = [
    { id: 'content', label: '内容' },
    { id: 'color', label: '颜色' },
    { id: 'size', label: '大小' },
    { id: 'font', label: '字体' },
    { id: 'layout', label: '布局' },
    { id: 'actions', label: '操作' }
  ];
  const FONT_FAMILIES = [
    { value: 'inherit', label: '继承页面字体' },
    { value: 'system-ui, -apple-system, "Helvetica Neue", Arial, sans-serif', label: '系统无衬线' },
    { value: '"PingFang SC", "Hiragino Sans GB", "Microsoft YaHei", sans-serif', label: '苹方 / 中文无衬线' },
    { value: 'Georgia, "Times New Roman", serif', label: '衬线（Georgia）' },
    { value: '"Courier New", ui-monospace, monospace', label: '等宽' },
    { value: 'ui-rounded, "SF Pro Rounded", system-ui, sans-serif', label: '圆润' }
  ];

  const state = {
    token: '',
    connected: false,
    previewOrigin: '',
    previewUrl: '',
    snapshot: null,
    nodes: [],
    dirty: false,
    canUndo: false,
    canRedo: false,
    editing: false,
    selection: null,
    selectedId: '',
    inspectorKey: '',
    forceInspector: false,
    selectionAck: null,
    selectionAckTimer: 0,
    device: 'desktop',
    busyCount: 0,
    frameReady: false,
    bridgePort: null,
    frameSeq: 0,
    bridgeTimer: 0,
    blocked: false,
    stale: false,
    unknown: false,
    backendAvailable: true,
    confirmReload: 0,
    placement: 'inside',
    placementManual: false,
    popoverPlacement: 'inside',
    popoverOpen: false,
    menuOpen: false,
    menuTab: 'content',
    menuSelectionKey: '',
    menuCoords: null,
    paletteDrag: null,
    paletteSeq: 0,
    palettePending: new Map(),
    suppressPaletteClick: false,
    toastTimers: new Set(),
    viewerFiles: []
  };

  const els = {};
  let inspectSeq = 0;

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', init, { once: true });
  } else {
    init();
  }

  /* ------------------------------------------------------------------ boot */

  function init() {
    cacheElements();
    renderComponentList(els.paletteList, '', 'palette');
    renderComponentList(els.popoverList, '', 'popover');
    bindConnect();
    bindTopbar();
    bindPanels();
    bindInspector(els.inspectorBody, 'sidebar');
    bindInspector(els.contextMenu, 'menu');
    bindSelectionLabel();
    bindPopover();
    bindGlobal();
    bindPaletteDrag();
    renderInspector(null);
    renderStatus();
  }

  function cacheElements() {
    const ids = [
      'app', 'connect-view', 'connect-form', 'token-input', 'token-toggle', 'connect-submit',
      'connect-error', 'workspace', 'toggle-palette', 'page-path', 'dirty-indicator', 'save-state',
      'enter-edit', 'preview-mode', 'undo', 'redo', 'reload', 'save', 'toggle-inspector',
      'conflict-banner', 'conflict-reload', 'conflict-dismiss', 'unknown-banner', 'unknown-inspect',
      'unknown-reason', 'source-viewer', 'source-target', 'source-file', 'source-close', 'source-code',
      'palette', 'palette-search', 'palette-list', 'canvas', 'canvas-hint', 'preview-frame',
      'inspector', 'inspector-scope', 'inspector-body', 'selection-label', 'selection-tag',
      'selection-add', 'selection-parent', 'selection-duplicate', 'selection-delete',
      'toasts', 'context-menu', 'palette-popover', 'popover-close', 'popover-search', 'popover-list',
      'palette-drag-ghost'
    ];
    for (const id of ids) {
      const key = id.replace(/-([a-z])/g, (_, letter) => letter.toUpperCase());
      els[key] = document.getElementById(id);
    }
    els.canvasScroll = document.querySelector('.canvas-scroll');
    els.frame = els.previewFrame;
    els.popover = els.palettePopover;
    els.dragGhost = els.paletteDragGhost;
    els.deviceButtons = Array.from(document.querySelectorAll('.device-btn[data-device]'));
    els.panelPlacementButtons = Array.from(document.querySelectorAll('[data-placement]'));
    els.popoverPlacementButtons = Array.from(document.querySelectorAll('[data-popover-placement]'));
  }

  /* --------------------------------------------------------------- connect */

  function bindConnect() {
    els.connectForm.addEventListener('submit', (event) => {
      event.preventDefault();
      connect(els.tokenInput.value);
    });
    els.tokenToggle.addEventListener('click', () => {
      const showing = els.tokenInput.type === 'text';
      els.tokenInput.type = showing ? 'password' : 'text';
      els.tokenToggle.textContent = showing ? '显示' : '隐藏';
      els.tokenToggle.setAttribute('aria-pressed', String(!showing));
      els.tokenToggle.setAttribute('aria-label', showing ? '显示令牌' : '隐藏令牌');
    });
  }

  async function connect(rawToken) {
    const token = (rawToken || '').trim();
    hideConnectError();
    if (!/^[\x21-\x7E]{32,}$/.test(token)) {
      showConnectError('令牌需要至少 32 个 ASCII 字符。请填写设置在 CAYW_EDITOR_TOKEN 中的令牌。');
      return;
    }
    setConnectBusy(true);
    try {
      const health = await fetch('/api/health', {
        method: 'GET',
        headers: { Accept: 'application/json' },
        cache: 'no-store',
        credentials: 'same-origin'
      });
      if (!health.ok) {
        throw new Error('编辑服务健康检查失败，无法继续连接。');
      }
      state.token = token;
      const documentData = await api('/api/document');
      applyDocument(documentData, { initial: true });
      validatePreview();
      state.connected = true;
      showWorkspace();
      setEditing(false);
      switchDevice('desktop', { silent: true });
      loadFrame();
      toast('已连接。先在“交互预览”查看原页面，再点击“可视化编辑”。', 'ok');
    } catch (error) {
      state.token = '';
      handleConnectError(error);
    } finally {
      setConnectBusy(false);
    }
  }

  function handleConnectError(error) {
    if (error && error.status === 401) {
      showConnectError('令牌无效。请核对设置在 CAYW_EDITOR_TOKEN 中的编辑器令牌。');
      return;
    }
    if (error && error.message) {
      showConnectError(error.message);
      return;
    }
    showConnectError('无法连接编辑服务。请确认本机编辑服务仍在运行。');
  }

  function showConnectError(message) {
    els.connectError.textContent = message;
    els.connectError.hidden = false;
  }

  function hideConnectError() {
    els.connectError.hidden = true;
    els.connectError.textContent = '';
  }

  function setConnectBusy(busy) {
    els.connectSubmit.disabled = busy;
    els.connectSubmit.textContent = busy ? '正在连接…' : '连接';
    els.app.setAttribute('aria-busy', busy ? 'true' : 'false');
  }

  function showWorkspace() {
    els.connectView.hidden = true;
    els.workspace.hidden = false;
    hideConnectError();
    els.tokenInput.value = '';
  }

  function disconnect(message) {
    state.connected = false;
    state.token = '';
    state.snapshot = null;
    state.nodes = [];
    state.selection = null;
    state.selectedId = '';
    state.inspectorKey = '';
    state.frameReady = false;
    closeBridgePort();
    state.dirty = false;
    state.canUndo = false;
    state.canRedo = false;
    state.blocked = false;
    state.stale = false;
    state.unknown = false;
    state.backendAvailable = true;
    state.toastTimers.forEach((timer) => clearTimeout(timer));
    state.toastTimers.clear();
    state.palettePending.forEach((pending) => {
      clearTimeout(pending.timer);
      pending.resolve({ reason: '连接已断开，拖放已取消。' });
    });
    state.palettePending.clear();
    els.toasts.textContent = '';
    closeMenu();
    closePopover();
    closeSourceViewer();
    els.selectionLabel.hidden = true;
    els.frame.src = 'about:blank';
    els.workspace.hidden = true;
    els.connectView.hidden = false;
    renderInspector(null);
    renderStatus();
    if (message) showConnectError(message);
  }

  function validatePreview() {
    const origin = new URL(state.previewOrigin);
    if (origin.protocol !== 'http:' && origin.protocol !== 'https:') {
      throw new Error('预览来源必须是本机 HTTP 地址。');
    }
    if (!isLoopbackHost(origin.hostname)) {
      throw new Error('预览来源必须是本机回环地址。');
    }
    const page = new URL(state.previewUrl || '/', origin.origin);
    if (page.origin !== origin.origin) {
      throw new Error('预览地址与预览来源不一致，编辑器已拒绝加载。');
    }
    if (!isLoopbackHost(page.hostname)) {
      throw new Error('预览地址必须是本机回环地址。');
    }
    state.previewOrigin = origin.origin;
    state.previewUrl = page.toString();
  }

  function isLoopbackHost(hostname) {
    return hostname === 'localhost' || hostname === '[::1]' || hostname === '::1' ||
      hostname === '127.0.0.1' || /^127\.\d{1,3}\.\d{1,3}\.\d{1,3}$/.test(hostname);
  }

  /* ------------------------------------------------------------------- api */

  class ApiError extends Error {
    constructor(code, message, status, outcomeUnknown) {
      super(message);
      this.name = 'ApiError';
      this.code = code;
      this.status = status;
      this.outcomeUnknown = Boolean(outcomeUnknown);
    }
  }

  async function api(path, options = {}) {
    const { method = 'GET', body } = options;
    const headers = { Accept: 'application/json' };
    if (state.token) headers.Authorization = `Bearer ${state.token}`;
    if (body !== undefined) headers['Content-Type'] = 'application/json';
    const controller = new AbortController();
    const timeout = setTimeout(() => controller.abort(), 15000);
    let response;
    let text;
    try {
      response = await fetch(path, {
        method,
        headers,
        body: body === undefined ? undefined : JSON.stringify(body),
        cache: 'no-store',
        credentials: 'same-origin',
        signal: controller.signal
      });
      text = await response.text();
    } catch (error) {
      throw new ApiError('network', '无法连接编辑服务，请检查本地服务是否仍在运行。', 0, false);
    } finally {
      clearTimeout(timeout);
    }
    let data = null;
    if (text) {
      try {
        data = JSON.parse(text);
      } catch (error) {
        data = null;
      }
    }
    if (!response.ok) {
      const payload = data && data.error && typeof data.error === 'object' ? data.error : {};
      const code = typeof payload.code === 'string' && payload.code
        ? payload.code
        : httpErrorCode(response.status);
      const message = typeof payload.message === 'string' && payload.message
        ? payload.message
        : `请求失败（HTTP ${response.status}）。`;
      throw new ApiError(code, message, response.status, payload.outcome_unknown === true);
    }
    if (!data || typeof data !== 'object') {
      throw new ApiError('bad_response', '编辑服务返回了无法识别的响应。', response.status, false);
    }
    return data;
  }

  function httpErrorCode(status) {
    if (status === 400) return 'bad_request';
    if (status === 401 || status === 403) return 'unauthorized';
    if (status === 409) return 'stale_source';
    if (status === 503) return 'service_unavailable';
    return 'http_error';
  }

  function applyDocument(data, options = {}) {
    if (!data || typeof data !== 'object' || !data.snapshot || typeof data.snapshot !== 'object') {
      throw new ApiError('bad_response', '文档数据缺少快照。', 200, false);
    }
    const snapshot = data.snapshot;
    if (typeof snapshot.html !== 'string') {
      throw new ApiError('bad_response', '文档快照缺少 HTML 内容。', 200, false);
    }
    state.snapshot = {
      path: typeof snapshot.path === 'string' ? snapshot.path : 'index.html',
      sha256: typeof snapshot.sha256 === 'string' ? snapshot.sha256 : '',
      adapter: typeof snapshot.adapter === 'string' ? snapshot.adapter : 'html',
      revision: typeof snapshot.revision === 'string' ? snapshot.revision : '',
      files: Array.isArray(snapshot.files) ? snapshot.files : [],
      html: snapshot.html,
      source: typeof snapshot.source === 'string' ? snapshot.source : '',
      nodes: Array.isArray(snapshot.nodes) ? snapshot.nodes : []
    };
    state.nodes = state.snapshot.nodes.filter((node) => node && typeof node.id === 'string');
    if (typeof data.preview_origin === 'string') state.previewOrigin = data.preview_origin;
    if (typeof data.preview_url === 'string') state.previewUrl = data.preview_url;
    state.backendAvailable = data.backend_available !== false;
    if (!state.backendAvailable) state.blocked = true;
    state.dirty = data.dirty === true;
    state.canUndo = data.can_undo === true;
    state.canRedo = data.can_redo === true;
    const selectedId = typeof snapshot.selected_id === 'string' ? snapshot.selected_id : '';
    state.selectedId = selectedId && state.nodes.some((node) => node.id === selectedId && node.editable !== false)
      ? selectedId
      : '';
    state.selection = null;
    if (options.initial) {
      state.inspectorKey = '';
    }
    renderStatus();
  }

  async function performOperation(rawOperation) {
    if (!state.connected) return;
    if (!state.editing) {
      toast('请先进入“可视化编辑”再修改页面。', 'warn');
      return;
    }
    if (state.blocked) {
      blockedToast();
      return;
    }
    if (state.busyCount > 0) {
      toast('正在处理上一条操作，请稍候。', 'warn');
      return;
    }
    let operation;
    try {
      operation = sanitizeOperation(rawOperation);
    } catch (error) {
      toast(error.message || '操作格式不受支持。', 'error');
      return;
    }
    closeIfStructural(operation.kind);
    setBusy(true);
    renderInspectorPlaceholder('已提交修改，预览刷新后会自动恢复选区。');
    try {
      const data = await api('/api/operation', { method: 'POST', body: operation });
      applyDocument(data);
      loadFrame();
      if (operation.kind === 'insert') toast('组件已插入。', 'ok');
      else if (operation.kind === 'duplicate') toast('已复制元素。', 'ok');
      else if (operation.kind === 'delete') toast('已删除元素，可撤销。', 'ok');
    } catch (error) {
      handleApiError(error);
      if (error.status === 0 || error.code === 'bad_response') await refreshDocument();
      renderInspector(state.selection);
    } finally {
      setBusy(false);
    }
  }

  function closeIfStructural(kind) {
    if (kind === 'delete' || kind === 'insert' || kind === 'duplicate' || kind === 'undo' || kind === 'redo' || kind === 'move') {
      closeMenu();
      closePopover();
    }
  }

  async function saveDocument() {
    if (!state.connected || state.busyCount > 0 || state.blocked) {
      if (state.blocked) blockedToast();
      return;
    }
    if (!state.dirty) {
      toast('没有需要保存的修改。');
      return;
    }
    if (!state.snapshot || !state.snapshot.sha256) {
      toast('缺少源码摘要，无法安全保存。', 'error');
      return;
    }
    const expected = state.snapshot.sha256;
    setBusy(true);
    try {
      const data = await api('/api/save', { method: 'POST', body: { expected_sha256: expected } });
      applyDocument(data);
      loadFrame();
      toast('已保存到项目源码。', 'ok');
    } catch (error) {
      handleApiError(error, { saving: true });
    } finally {
      setBusy(false);
    }
  }

  async function reloadProject(options = {}) {
    if (!state.connected || state.busyCount > 0) return;
    if (state.unknown) {
      toast('结果未知时，服务会继续拒绝写入。请查看最后已知草稿并核对持久审计记录，确认后由宿主恢复会话。', 'warn', '', 7000);
      return;
    }
    if (!options.force && state.dirty) {
      const now = Date.now();
      if (now > state.confirmReload) {
        state.confirmReload = now + RELOAD_CONFIRM_MS;
        toast('有未保存修改：再次点击“重新加载”会丢弃这些修改。', 'warn');
        return;
      }
      state.confirmReload = 0;
    }
    setBusy(true);
    try {
      const data = await api('/api/reload', { method: 'POST', body: {} });
      applyDocument(data);
      state.selectedId = '';
      state.selection = null;
      state.inspectorKey = '';
      clearBlocked();
      loadFrame();
      renderInspector(null);
      updateSelectionUi(null);
      toast('已重新读取项目源码。', 'ok');
    } catch (error) {
      handleApiError(error);
      if (error.status === 0 || error.code === 'bad_response') await refreshDocument();
    } finally {
      setBusy(false);
    }
  }

  async function refreshDocument() {
    try {
      applyDocument(await api('/api/document'));
      loadFrame();
    } catch (error) {
      handleApiError(error);
      if (state.connected && !state.blocked) {
        disconnect('无法读取操作后的草稿。请重新连接，读取服务中保留的草稿后继续编辑。');
      }
    }
  }

  async function history(kind) {
    if (!state.connected || state.busyCount > 0) return;
    if (state.blocked) {
      blockedToast();
      return;
    }
    if (kind === 'undo' && !state.canUndo) return;
    if (kind === 'redo' && !state.canRedo) return;
    await performOperation({ kind });
  }

  function handleApiError(error, options = {}) {
    const apiError = error instanceof ApiError
      ? error
      : new ApiError('internal', error && error.message ? error.message : '操作失败。', 0, false);
    if (apiError.status === 401) {
      disconnect('令牌已失效。请重新输入编辑器令牌。');
      return;
    }
    if (apiError.status === 409 || apiError.code === 'stale_source') {
      markStale();
      return;
    }
    if (apiError.outcomeUnknown || apiError.code === 'outcome_unknown') {
      markUnknown(apiError.message);
      return;
    }
    if (apiError.code === 'companion_unavailable') {
      state.backendAvailable = false;
      state.blocked = true;
      els.selectionLabel.hidden = true;
      renderStatus();
      renderInspector(state.selection);
      toast(apiError.message || '框架编译服务已停止。请重启编辑器；未保存草稿未写入项目。', 'error');
      return;
    }
    if (options.saving && (apiError.status === 0 || apiError.code === 'bad_response')) {
      markUnknown('网络中断，无法确认这次操作是否已经写入源码。');
      return;
    }
    toast(apiError.message || '操作失败。', 'error', apiError.code || '');
  }

  function markStale() {
    state.stale = true;
    state.blocked = true;
    els.conflictBanner.hidden = false;
    els.unknownBanner.hidden = true;
    els.selectionLabel.hidden = true;
    renderStatus();
    renderInspector(state.selection);
    toast('项目源码已在编辑器外更新，保存被阻止。', 'warn');
  }

  function markUnknown(message) {
    state.unknown = true;
    state.blocked = true;
    els.unknownBanner.hidden = false;
    els.conflictBanner.hidden = true;
    els.selectionLabel.hidden = true;
    if (message) {
      els.unknownReason.textContent = message;
      els.unknownReason.hidden = false;
    } else {
      els.unknownReason.textContent = '';
      els.unknownReason.hidden = true;
    }
    renderStatus();
    renderInspector(state.selection);
    toast('操作结果未知，已停止新的编辑。', 'error');
  }

  function blockedToast() {
    if (!state.backendAvailable) {
      toast('框架编译服务已停止。请重启编辑器；未保存草稿未写入项目。', 'warn', '', 7000);
      return;
    }
    if (state.unknown) {
      toast('结果未知，服务会继续拒绝写入。请查看最后已知草稿并核对持久审计记录，确认后由宿主恢复会话。', 'warn', '', 7000);
      return;
    }
    toast('源码冲突未解决，请先重新加载项目。', 'warn');
  }

  function clearBlocked() {
    state.stale = false;
    state.unknown = false;
    state.blocked = false;
    state.confirmReload = 0;
    els.conflictBanner.hidden = true;
    els.unknownBanner.hidden = true;
    renderStatus();
  }

  function setBusy(busy) {
    state.busyCount = Math.max(0, state.busyCount + (busy ? 1 : -1));
    els.app.setAttribute('aria-busy', state.busyCount > 0 ? 'true' : 'false');
    renderStatus();
  }

  /* --------------------------------------------------------------- topbar */

  function bindTopbar() {
    els.enterEdit.addEventListener('click', () => setEditing(true));
    els.previewMode.addEventListener('click', () => setEditing(false));
    els.undo.addEventListener('click', () => history('undo'));
    els.redo.addEventListener('click', () => history('redo'));
    els.save.addEventListener('click', saveDocument);
    els.reload.addEventListener('click', () => reloadProject());
    els.conflictReload.addEventListener('click', () => reloadProject({ force: true }));
    els.unknownInspect.addEventListener('click', openSourceViewer);
    els.sourceClose.addEventListener('click', closeSourceViewer);
    els.sourceFile.addEventListener('change', () => {
      const file = state.viewerFiles.find(file => file.path === els.sourceFile.value);
      if (!file) return;
      els.sourceTarget.textContent = file.path;
      els.sourceCode.textContent = file.source;
    });
    els.sourceViewer.addEventListener('pointerdown', (event) => {
      if (event.target === els.sourceViewer) closeSourceViewer();
    });
    els.conflictDismiss.addEventListener('click', () => {
      els.conflictBanner.hidden = true;
      toast('源码冲突仍未解决，保存会继续被阻止。', 'warn');
    });
    for (const button of els.deviceButtons) {
      button.addEventListener('click', () => switchDevice(button.dataset.device));
    }
    els.togglePalette.addEventListener('click', () => {
      const open = document.body.classList.toggle('panel-palette');
      document.body.classList.remove('panel-inspector');
      els.togglePalette.setAttribute('aria-expanded', String(open));
      els.toggleInspector.setAttribute('aria-expanded', 'false');
    });
    els.toggleInspector.addEventListener('click', () => {
      const open = document.body.classList.toggle('panel-inspector');
      document.body.classList.remove('panel-palette');
      els.toggleInspector.setAttribute('aria-expanded', String(open));
      els.togglePalette.setAttribute('aria-expanded', 'false');
    });
  }

  async function openSourceViewer() {
    if (!state.connected) return;
    els.sourceViewer.hidden = false;
    els.sourceTarget.textContent = '读取中…';
    els.sourceCode.textContent = '';
    try {
      const data = await api('/api/document');
      const snapshot = data && data.snapshot;
      if (!snapshot || typeof snapshot.source !== 'string' || !snapshot.source) {
        throw new ApiError('bad_response', '当前源码缺少原始草稿内容。', 200, false);
      }
      state.viewerFiles = Array.isArray(snapshot.files)
        ? snapshot.files.filter(file => file && typeof file.path === 'string' && typeof file.source === 'string')
        : [];
      const selected = nodeById(state.selectedId);
      const path = selected ? selected.file : snapshot.path;
      const file = state.viewerFiles.find(file => file.path === path);
      els.sourceFile.innerHTML = state.viewerFiles.map(file => `<option value="${escAttr(file.path)}">${esc(file.path)}</option>`).join('');
      els.sourceFile.hidden = state.viewerFiles.length <= 1;
      if (file) els.sourceFile.value = file.path;
      els.sourceTarget.textContent = file ? file.path : snapshot.path;
      els.sourceCode.textContent = file ? file.source : snapshot.source;
      if (!state.blocked) applyDocument(data);
    } catch (error) {
      els.sourceTarget.textContent = '';
      els.sourceCode.textContent = error && error.message ? error.message : '无法读取当前源码。';
      if (error instanceof ApiError && error.status === 401) {
        closeSourceViewer();
        handleApiError(error);
      }
    } finally {
      els.sourceCode.focus({ preventScroll: true });
    }
  }

  function closeSourceViewer() {
    els.sourceViewer.hidden = true;
  }

  function setEditing(value) {
    const next = Boolean(value);
    if (state.editing === next) {
      if (next && state.selectedId) {
        state.forceInspector = true;
        sendToBridge({ type: 'cayw:select', id: state.selectedId });
      }
      return;
    }
    state.editing = next;
    document.body.classList.toggle('editing', next);
    els.enterEdit.classList.toggle('active', next);
    els.previewMode.classList.toggle('active', !next);
    els.enterEdit.setAttribute('aria-pressed', String(next));
    els.previewMode.setAttribute('aria-pressed', String(!next));
    els.canvasHint.textContent = next
      ? '点击选择元素；拖动元素调整顺序；拖动右侧或底部手柄调整宽高；右键打开快捷修改。'
      : '交互预览：按钮、链接和表单控件可直接操作。点击“可视化编辑”开始修改。';
    els.selectionLabel.hidden = !next || !state.selection;
    if (state.frameReady) sendToBridge({ type: 'cayw:mode', editing: next });
    if (next && state.selectedId && state.frameReady) {
      state.forceInspector = true;
      sendToBridge({ type: 'cayw:select', id: state.selectedId });
      armSelectionAck(state.selectedId);
    }
    if (!next) closeMenu();
    renderInspector(state.selection);
    renderStatus();
    if (next) toast('已进入可视化编辑，原页面布局保持不变。', 'ok');
  }

  function switchDevice(device, options = {}) {
    const next = device === 'mobile' ? 'mobile' : 'desktop';
    state.device = next;
    els.canvas.dataset.device = next;
    for (const button of els.deviceButtons) {
      const active = button.dataset.device === next;
      button.classList.toggle('active', active);
      button.setAttribute('aria-pressed', String(active));
    }
    if (!options.silent && state.selectedId && state.frameReady) {
      sendToBridge({ type: 'cayw:select', id: state.selectedId });
    }
    if (!options.silent) {
      toast(next === 'mobile' ? '已切换到移动宽度（390px）。' : '已切换到桌面宽度。');
    }
  }

  function renderStatus() {
    const busy = state.busyCount > 0;
    const node = state.snapshot ? state.snapshot.nodes.length : 0;
    if (state.snapshot) els.pagePath.textContent = state.snapshot.path || 'index.html';
    els.pagePath.title = state.snapshot ? (state.snapshot.sha256 ? `源码摘要 ${state.snapshot.sha256.slice(0, 12)}…` : state.snapshot.path) : '';
    els.dirtyIndicator.hidden = !state.dirty;
    if (!state.backendAvailable) els.saveState.textContent = '编译服务已停止';
    else if (state.unknown) els.saveState.textContent = '结果未知';
    else if (state.stale) els.saveState.textContent = '源码冲突';
    else if (state.dirty) els.saveState.textContent = '有未保存修改';
    else els.saveState.textContent = node ? '已保存' : '未载入';
    els.undo.disabled = busy || state.blocked || !state.canUndo;
    els.redo.disabled = busy || state.blocked || !state.canRedo;
    els.save.disabled = busy || state.blocked || !state.dirty;
    els.reload.disabled = busy || state.unknown || !state.backendAvailable;
    els.reload.title = state.unknown
      ? '结果未知，重新加载不会解除写入阻止'
      : '重新读取项目源码，未保存修改会丢失';
    els.enterEdit.disabled = busy;
    els.previewMode.disabled = busy;
    for (const button of els.deviceButtons) button.disabled = busy;
  }

  /* --------------------------------------------------------------- palette */

  function renderComponentList(container, query, prefix) {
    const q = (query || '').trim().toLowerCase();
    const groups = new Map();
    for (const item of COMPONENTS) {
      const haystack = `${item.label} ${item.name} ${item.meta} ${item.category}`.toLowerCase();
      if (q && !haystack.includes(q)) continue;
      if (!groups.has(item.category)) groups.set(item.category, []);
      groups.get(item.category).push(item);
    }
    const ordered = [...groups.keys()].sort(
      (a, b) => CATEGORY_ORDER.indexOf(a) - CATEGORY_ORDER.indexOf(b)
    );
    if (!ordered.length) {
      container.innerHTML = '<p class="panel-foot"><span>没有匹配的组件。试试“卡片”或“Hero”。</span></p>';
      return;
    }
    container.innerHTML = ordered.map((category) => `
      <div class="palette-group">
        <div class="palette-group-title">${esc(category)}</div>
        <div class="palette-grid">
          ${groups.get(category).map((item) => `
            <button type="button" class="palette-item" data-component="${escAttr(item.name)}" data-testid="editor-${prefix}-item-${escAttr(item.name)}" aria-label="插入${escAttr(item.label)}">
              <span class="item-label">${esc(item.label)}</span>
              <span class="item-meta">${esc(item.meta)}</span>
              ${item.badge ? `<span class="item-badge">${esc(item.badge)}</span>` : ''}
            </button>
          `).join('')}
        </div>
      </div>
    `).join('');
  }

  function bindPanels() {
    els.paletteSearch.addEventListener('input', () => {
      renderComponentList(els.paletteList, els.paletteSearch.value, 'palette');
    });
    els.paletteList.addEventListener('click', (event) => {
      const item = event.target.closest('[data-component]');
      if (!item) return;
      if (state.suppressPaletteClick) {
        state.suppressPaletteClick = false;
        return;
      }
      insertComponent(item.dataset.component, state.placementManual ? state.placement : effectivePlacement());
    });
    els.popoverSearch.addEventListener('input', () => {
      renderComponentList(els.popoverList, els.popoverSearch.value, 'popover');
    });
    els.popoverList.addEventListener('click', (event) => {
      const item = event.target.closest('[data-component]');
      if (!item) return;
      insertComponent(item.dataset.component, state.popoverPlacement);
    });
    for (const button of els.panelPlacementButtons) {
      button.addEventListener('click', () => {
        state.placementManual = true;
        state.placement = button.dataset.placement;
        updatePlacementButtons();
      });
    }
    for (const button of els.popoverPlacementButtons) {
      button.addEventListener('click', () => {
        state.popoverPlacement = button.dataset.popoverPlacement;
        updatePopoverPlacementButtons();
      });
    }
    els.popoverClose.addEventListener('click', closePopover);
    els.canvasScroll.addEventListener('scroll', repositionLabel, { passive: true });
  }

  function insertComponent(name, placement) {
    if (!COMPONENT_NAMES.has(name)) {
      toast('未知组件，操作已拒绝。', 'error');
      return;
    }
    if (!state.selectedId) {
      toast('请先在预览中选中一个元素，作为插入位置。', 'warn');
      return;
    }
    if (state.selection && state.selection.partial) {
      toast('该选区包含未映射内容，不能插入结构；请选择相邻源码元素。', 'warn');
      return;
    }
    if (!state.editing) {
      toast('请先进入“可视化编辑”。', 'warn');
      return;
    }
    insertComponentAt(name, state.selectedId, placement);
  }

  function insertComponentAt(name, target, placement) {
    const item = COMPONENT_BY_NAME.get(name);
    if (!item || !target) {
      toast('没有可用的插入目标。', 'warn');
      return;
    }
    if (placement !== 'before' && placement !== 'after' && placement !== 'inside') {
      toast('插入位置无效，请重新选择。', 'warn');
      return;
    }
    const targetNode = nodeById(target);
    if (placement === 'inside') {
      if (!targetNode || !CONTAINER_TAGS.has(targetNode.tag)) {
        toast(`<${targetNode ? targetNode.tag : '未知'}> 不是可接收插入内容的容器。`, 'warn');
        return;
      }
      if (!tableChildAllowed(targetNode.tag, '')) {
        toast(`<${targetNode.tag}> 不能直接接收通用组件；请选择 td、th 或其他容器。`, 'warn');
        return;
      }
    } else {
      const parentNode = targetNode && targetNode.parent ? nodeById(targetNode.parent) : null;
      if (!parentNode || !CONTAINER_TAGS.has(parentNode.tag)) {
        toast(`父级 <${parentNode ? parentNode.tag : '未知'}> 不是可接收同级内容的容器。`, 'warn');
        return;
      }
      if (!tableChildAllowed(parentNode.tag, '')) {
        toast(`父级 <${parentNode.tag}> 不能直接接收通用组件；请选择 td、th 或其他容器。`, 'warn');
        return;
      }
    }
    closePopover();
    performOperation({ kind: 'insert', target, placement, component: name });
    toast(`正在插入${item.label}…`);
  }

  function effectivePlacement() {
    if (state.selection && CONTAINER_TAGS.has(state.selection.tag) &&
        !TABLE_STRUCTURE_TAGS.has(state.selection.tag) && !state.selection.dynamic) {
      return 'inside';
    }
    return 'after';
  }

  function updatePlacementButtons() {
    for (const button of els.panelPlacementButtons) {
      const active = button.dataset.placement === state.placement;
      button.classList.toggle('active', active);
      button.setAttribute('aria-pressed', String(active));
    }
  }

  function updatePopoverPlacementButtons() {
    for (const button of els.popoverPlacementButtons) {
      const active = button.dataset.popoverPlacement === state.popoverPlacement;
      button.classList.toggle('active', active);
      button.setAttribute('aria-pressed', String(active));
    }
  }

  function bindPaletteDrag() {
    els.paletteList.addEventListener('pointerdown', (event) => {
      if (event.button !== 0) return;
      const item = event.target.closest('[data-component]');
      if (!item) return;
      const start = {
        x: event.clientX,
        y: event.clientY,
        component: item.dataset.component,
        pointerId: event.pointerId,
        active: false
      };
      state.paletteDrag = start;

      const onMove = (moveEvent) => {
        if (!start.active) {
          if (Math.hypot(moveEvent.clientX - start.x, moveEvent.clientY - start.y) < 6) return;
          start.active = true;
          showDragGhost(moveEvent.clientX, moveEvent.clientY, start.component);
          try { item.setPointerCapture(start.pointerId); } catch (error) { /* ignore */ }
        }
        moveEvent.preventDefault();
        positionDragGhost(moveEvent.clientX, moveEvent.clientY);
        sendPalettePointer('move', start.component, moveEvent.clientX, moveEvent.clientY);
      };

      const onUp = (upEvent) => {
        window.removeEventListener('pointermove', onMove, true);
        window.removeEventListener('pointerup', onUp, true);
        window.removeEventListener('pointercancel', onUp, true);
        state.paletteDrag = null;
        if (!start.active) return;
        state.suppressPaletteClick = true;
        setTimeout(() => { state.suppressPaletteClick = false; }, 350);
        hideDragGhost();
        if (!state.editing) {
          sendPalettePointer('cancel', start.component, upEvent.clientX, upEvent.clientY);
          toast('请先进入“可视化编辑”，再把组件拖入页面。', 'warn');
          return;
        }
        if (!isOverCanvas(upEvent.clientX, upEvent.clientY)) {
          sendPalettePointer('cancel', start.component, upEvent.clientX, upEvent.clientY);
          return;
        }
        const point = framePoint(upEvent.clientX, upEvent.clientY);
        if (!point) {
          sendPalettePointer('cancel', start.component, upEvent.clientX, upEvent.clientY);
          toast('拖放落点不在项目页面内。', 'warn');
          return;
        }
        requestPaletteDrop(start.component, point).then((result) => {
          if (!result || !result.target) {
            toast(result && result.reason ? result.reason : '未能确定插入位置，请把组件拖到可编辑区域。', 'warn');
            return;
          }
          insertComponentAt(start.component, result.target, result.placement || 'inside');
        });
      };

      window.addEventListener('pointermove', onMove, true);
      window.addEventListener('pointerup', onUp, true);
      window.addEventListener('pointercancel', onUp, true);
    });
  }

  function showDragGhost(x, y, component) {
    const item = COMPONENT_BY_NAME.get(component);
    els.dragGhost.textContent = item ? `插入${item.label}` : '插入组件';
    els.dragGhost.hidden = false;
    positionDragGhost(x, y);
  }

  function positionDragGhost(x, y) {
    els.dragGhost.style.left = `${x}px`;
    els.dragGhost.style.top = `${y}px`;
    els.dragGhost.classList.toggle('over', isOverCanvas(x, y));
  }

  function hideDragGhost() {
    els.dragGhost.hidden = true;
    els.dragGhost.classList.remove('over');
  }

  function isOverCanvas(x, y) {
    const rect = els.canvas.getBoundingClientRect();
    return x >= rect.left && x <= rect.right && y >= rect.top && y <= rect.bottom;
  }

  function framePoint(clientX, clientY) {
    const rect = els.frame.getBoundingClientRect();
    if (!rect.width || !rect.height || !els.frame.clientWidth || !els.frame.clientHeight) return null;
    const x = (clientX - rect.left) * (els.frame.clientWidth / rect.width);
    const y = (clientY - rect.top) * (els.frame.clientHeight / rect.height);
    if (!Number.isFinite(x) || !Number.isFinite(y)) return null;
    if (x < 0 || y < 0 || x > els.frame.clientWidth || y > els.frame.clientHeight) return null;
    return { x, y };
  }

  function sendPalettePointer(phase, component, clientX, clientY) {
    if (!state.frameReady) return;
    if (phase === 'cancel') {
      sendToBridge({ type: 'cayw:palette', phase: 'cancel', component });
      return;
    }
    const point = framePoint(clientX, clientY);
    if (!point) {
      sendToBridge({ type: 'cayw:palette', phase: 'cancel', component });
      return;
    }
    sendToBridge({ type: 'cayw:palette', phase, component, x: point.x, y: point.y });
  }

  function requestPaletteDrop(component, point) {
    return new Promise((resolve) => {
      if (!state.frameReady) {
        resolve({ reason: '预览尚未就绪，请稍后再拖入。' });
        return;
      }
      state.paletteSeq += 1;
      const request = state.paletteSeq;
      const timer = setTimeout(() => {
        if (!state.palettePending.delete(request)) return;
        resolve({ reason: '未能确认落点，请把组件拖到可编辑元素上。' });
      }, 1600);
      state.palettePending.set(request, { resolve, timer });
      sendToBridge({
        type: 'cayw:palette',
        phase: 'drop',
        request,
        component,
        x: point.x,
        y: point.y
      });
    });
  }

  /* -------------------------------------------------------------- popover */

  function bindPopover() {
    document.addEventListener('pointerdown', (event) => {
      if (!els.popover.hidden && !els.popover.contains(event.target) && !els.selectionAdd.contains(event.target)) {
        closePopover();
      }
      if (!els.contextMenu.hidden && !els.contextMenu.contains(event.target)) {
        closeMenu();
      }
    }, true);
  }

  function openPopover(anchorRect, placement) {
    closeMenu();
    state.popoverPlacement = placement || effectivePlacement();
    updatePopoverPlacementButtons();
    els.popoverSearch.value = '';
    renderComponentList(els.popoverList, '', 'popover');
    els.popover.hidden = false;
    state.popoverOpen = true;
    positionFloating(els.popover, anchorRect);
    els.popoverSearch.focus({ preventScroll: true });
  }

  function closePopover() {
    if (els.popover.hidden) return;
    els.popover.hidden = true;
    state.popoverOpen = false;
  }

  function positionFloating(element, anchorRect) {
    element.hidden = false;
    const margin = 8;
    const anchor = anchorRect || els.canvas.getBoundingClientRect();
    const box = element.getBoundingClientRect();
    let left = anchor.left;
    let top = anchor.bottom + 6;
    if (top + box.height > window.innerHeight - margin) {
      top = Math.max(margin, anchor.top - box.height - 6);
    }
    left = clamp(left, margin, window.innerWidth - box.width - margin);
    top = clamp(top, margin, window.innerHeight - box.height - margin);
    element.style.left = `${left}px`;
    element.style.top = `${top}px`;
  }

  /* ------------------------------------------------------------- selection */

  function updateSelectionUi(rawSel) {
    const sel = normalizeReadonlySelection(rawSel || null);
    state.selection = sel;
    if (!sel) {
      state.selectedId = '';
      els.selectionLabel.hidden = true;
      els.inspectorScope.textContent = '未选择';
    } else {
      state.selectedId = sel.dynamic ? '' : sel.id;
      els.inspectorScope.textContent = sel.dynamic ? '只读区域' : sel.tag;
      const node = sel.id ? nodeById(sel.id) : null;
      els.selectionTag.textContent = sel.dynamic
        ? `${sel.tag || '区域'} · 只读`
        : `${sel.tag}${node ? ` · 第 ${node.line} 行` : ''}`;
      els.selectionLabel.hidden = !state.editing || !state.selection;
      positionLabel(sel);
      updateSelectionActions(sel);
    }
    if (!state.placementManual) {
      state.placement = sel ? effectivePlacement() : 'inside';
      updatePlacementButtons();
    }
    const force = state.forceInspector;
    const key = inspectorKeyOf(sel);
    const keyChanged = key !== state.inspectorKey;
    if (force || keyChanged) {
      state.forceInspector = false;
      state.inspectorKey = key;
      renderInspector(sel);
    }
    if (state.menuOpen && sel && state.menuCoords) {
      const menuChanged = menuKeyOf(sel) !== state.menuSelectionKey;
      if (force || menuChanged) {
        renderMenu(sel, state.menuCoords, force ? state.menuTab : 'content');
      }
    }
    if (sel && state.popoverOpen) {
      positionFloating(els.popover, els.selectionLabel.getBoundingClientRect());
    }
  }

  function normalizeReadonlySelection(sel) {
    if (!sel) return null;
    const node = sel.id ? nodeById(sel.id) : null;
    if (node && node.editable === false) {
      return {
        ...sel,
        dynamic: true,
        reason: sel.reason || node.reason || '该节点在源码映射中标记为只读。'
      };
    }
    return sel;
  }

  function updateSelectionActions(sel) {
    const node = sel && sel.id ? nodeById(sel.id) : null;
    const canEdit = Boolean(sel) && !sel.dynamic && !sel.partial && state.editing && !state.blocked;
    els.selectionAdd.disabled = !canEdit;
    els.selectionDuplicate.disabled = !canEdit;
    els.selectionDelete.disabled = !canEdit;
    if (sel && sel.dynamic) {
      els.selectionParent.disabled = !state.editing || state.blocked;
    } else {
      els.selectionParent.disabled = !state.editing || state.blocked || !node || !node.parent;
    }
  }

  function positionLabel(sel) {
    if (!sel || !state.editing) {
      els.selectionLabel.hidden = true;
      return;
    }
    const frameRect = els.frame.getBoundingClientRect();
    const rect = sel.rect;
    if (!frameRect.width || !frameRect.height) {
      els.selectionLabel.hidden = true;
      return;
    }
    if (rect.x + rect.width < -4 || rect.y + rect.height < -4 || rect.x > frameRect.width + 4 || rect.y > frameRect.height + 4) {
      els.selectionLabel.hidden = true;
      return;
    }
    els.selectionLabel.hidden = false;
    const label = els.selectionLabel;
    const width = label.offsetWidth;
    const height = label.offsetHeight;
    let left = frameRect.left + rect.x;
    let top = frameRect.top + rect.y - height - 4;
    if (top < frameRect.top + 2) top = frameRect.top + rect.y + 4;
    left = clamp(left, 8, window.innerWidth - width - 8);
    top = clamp(top, 8, window.innerHeight - height - 8);
    label.style.left = `${left}px`;
    label.style.top = `${top}px`;
  }

  function repositionLabel() {
    if (state.selection) positionLabel(state.selection);
  }

  function bindSelectionLabel() {
    els.selectionAdd.addEventListener('click', () => {
      if (!state.selectedId) return;
      if (state.selection && state.selection.partial) {
        toast('该选区包含未映射内容，不能插入结构；请选择相邻源码元素。', 'warn');
        return;
      }
      openPopover(els.selectionLabel.getBoundingClientRect(), effectivePlacement());
    });
    els.selectionParent.addEventListener('click', () => {
      if (state.selection && state.selection.dynamic) {
        sendToBridge({ type: 'cayw:parent' });
        return;
      }
      selectParent();
    });
    els.selectionDuplicate.addEventListener('click', () => {
      if (state.selectedId) performOperation({ kind: 'duplicate', id: state.selectedId });
    });
    els.selectionDelete.addEventListener('click', () => {
      if (state.selectedId) performOperation({ kind: 'delete', id: state.selectedId });
    });
  }

  function selectParent() {
    const node = nodeById(state.selectedId);
    if (!node || !node.parent) {
      toast('没有更上一级的可编辑容器。', 'warn');
      return;
    }
    selectNode(node.parent);
  }

  function selectNode(id) {
    if (!id || !state.editing || !state.frameReady) return;
    state.selectedId = id;
    state.forceInspector = true;
    sendToBridge({ type: 'cayw:select', id });
    armSelectionAck(id);
  }

  function armSelectionAck(id) {
    clearTimeout(state.selectionAckTimer);
    state.selectionAck = id;
    state.selectionAckTimer = setTimeout(() => {
      if (state.selectionAck !== id) return;
      state.selectionAck = null;
      state.selection = null;
      state.selectedId = '';
      state.inspectorKey = '';
      els.selectionLabel.hidden = true;
      renderInspector(null);
      toast('选区在重新解析后无法自动恢复，请重新选择元素。', 'warn');
    }, 1800);
  }

  /* ---------------------------------------------------------------- bridge */

  function closeBridgePort() {
    const port = state.bridgePort;
    state.bridgePort = null;
    if (!port) return;
    try {
      port.onmessage = null;
      port.onmessageerror = null;
      port.close();
    } catch (error) {
      // The port may already be closed with its old frame.
    }
  }

  function sendToBridge(message) {
    if (!state.frameReady || !state.bridgePort) return;
    try {
      state.bridgePort.postMessage(message);
    } catch (error) {
      // Ignore a closed port; the next frame load creates a new handshake.
    }
  }

  function onBridgeMessage(event) {
    if (!state.connected || !state.previewOrigin) return;
    if (event.origin !== state.previewOrigin) return;
    if (!els.frame.contentWindow || event.source !== els.frame.contentWindow) return;
    const data = event.data;
    if (!data || typeof data !== 'object' || data.type !== 'cayw:connect') return;
    const incomingPorts = event.ports || [];
    if (incomingPorts.length !== 1) {
      for (const rejected of incomingPorts) {
        try { rejected.close(); } catch (error) { /* already closed */ }
      }
      return;
    }
    const port = incomingPorts[0];
    if (data.url !== els.frame.src || state.bridgePort) {
      try { port.close(); } catch (error) { /* already closed */ }
      return;
    }
    state.bridgePort = port;
    port.onmessage = (messageEvent) => {
      if (state.bridgePort !== port) return;
      onBridgePortMessage(messageEvent);
    };
    port.onmessageerror = () => {
      if (state.bridgePort === port) closeBridgePort();
    };
    port.start();
  }

  function onBridgePortMessage(event) {
    const data = event.data;
    if (!data || typeof data !== 'object' || typeof data.type !== 'string') return;
    if (data.type === 'cayw:ready') {
      onBridgeReady();
      return;
    }
    if (!state.editing) return;
    if (data.type === 'cayw:palette-result') {
      const request = Number(data.request);
      const pending = state.palettePending.get(request);
      if (!pending) return;
      state.palettePending.delete(request);
      clearTimeout(pending.timer);
      pending.resolve({
        target: typeof data.target === 'string' ? data.target : '',
        placement: typeof data.placement === 'string' ? data.placement : '',
        reason: typeof data.reason === 'string' ? data.reason : ''
      });
      return;
    }
    if (data.type === 'cayw:select') {
      const sel = normalizeSelection(data);
      if (!sel) return;
      state.frameReady = true;
      clearTimeout(state.bridgeTimer);
      state.selectionAck = null;
      clearTimeout(state.selectionAckTimer);
      updateSelectionUi(sel);
      return;
    }
    if (data.type === 'cayw:context') {
      const sel = normalizeSelection(data);
      const x = Number(data.x);
      const y = Number(data.y);
      if (!sel || !Number.isFinite(x) || !Number.isFinite(y)) return;
      state.selectionAck = null;
      updateSelectionUi(sel);
      openContextMenu(sel, { x, y });
      return;
    }
    if (data.type === 'cayw:operation') {
      performOperation(data.operation);
    }
  }

  function onBridgeReady() {
    state.frameReady = true;
    clearTimeout(state.bridgeTimer);
    sendToBridge({ type: 'cayw:mode', editing: state.editing });
    if (state.editing && state.selectedId) {
      state.forceInspector = true;
      sendToBridge({ type: 'cayw:select', id: state.selectedId });
      armSelectionAck(state.selectedId);
    } else if (!state.selectedId) {
      state.selection = null;
      state.inspectorKey = '';
      updateSelectionUi(null);
    }
  }

  function normalizeSelection(data) {
    const rect = data.rect;
    if (!rect || typeof rect !== 'object') return null;
    const nums = [rect.x, rect.y, rect.width, rect.height];
    if (!nums.every((value) => typeof value === 'number' && Number.isFinite(value))) return null;
    const computed = data.computed && typeof data.computed === 'object' ? data.computed : {};
    const attributes = data.attributes && typeof data.attributes === 'object' ? data.attributes : {};
    const pick = (value) => (typeof value === 'string' ? value : '');
    return {
      id: pick(data.id),
      tag: pick(data.tag) || '元素',
      rect: {
        x: rect.x,
        y: rect.y,
        width: Math.max(0, rect.width),
        height: Math.max(0, rect.height)
      },
      computed: {
        color: pick(computed.color),
        backgroundColor: pick(computed.backgroundColor),
        borderColor: pick(computed.borderColor),
        width: pick(computed.width),
        height: pick(computed.height),
        fontFamily: pick(computed.fontFamily),
        fontSize: pick(computed.fontSize),
        fontWeight: pick(computed.fontWeight),
        lineHeight: pick(computed.lineHeight),
        textAlign: pick(computed.textAlign),
        display: pick(computed.display),
        gap: pick(computed.gap),
        flexDirection: pick(computed.flexDirection),
        gridTemplateColumns: pick(computed.gridTemplateColumns)
      },
      text: pick(data.text).slice(0, MAX_TEXT),
      attributes: { src: pick(attributes.src), href: pick(attributes.href) },
      dynamic: data.dynamic === true,
      partial: data.partial === true,
      reason: pick(data.reason)
    };
  }

  function previewSrc() {
    const base = new URL(state.previewUrl || '/', state.previewOrigin);
    if (base.origin !== state.previewOrigin) {
      throw new Error('预览地址与预览来源不一致，已拒绝加载。');
    }
    base.searchParams.set('cayw_edit', '1');
    state.frameSeq += 1;
    base.searchParams.set('cayw_r', String(state.frameSeq));
    return base.toString();
  }

  function loadFrame() {
    if (!state.connected) return;
    let src;
    try {
      src = previewSrc();
    } catch (error) {
      toast(error.message, 'error');
      return;
    }
    state.frameReady = false;
    closeBridgePort();
    state.forceInspector = true;
    els.selectionLabel.hidden = true;
    els.frame.src = src;
    clearTimeout(state.bridgeTimer);
    state.bridgeTimer = setTimeout(() => {
      if (!state.frameReady) {
        toast('预览桥接脚本尚未就绪。请确认预览服务正在提供页面。', 'warn', '', 7000);
      }
    }, 9000);
  }

  /* --------------------------------------------------------------- context */

  function openContextMenu(sel, coords) {
    state.menuCoords = coords;
    state.menuSelectionKey = menuKeyOf(sel);
    state.menuTab = 'content';
    state.menuOpen = true;
    renderMenu(sel, coords, 'content');
    closePopover();
  }

  function closeMenu() {
    if (els.contextMenu.hidden) {
      state.menuOpen = false;
      return;
    }
    els.contextMenu.hidden = true;
    state.menuOpen = false;
    state.menuCoords = null;
  }

  function renderMenu(sel, coords, tab) {
    state.menuOpen = true;
    state.menuCoords = coords || state.menuCoords;
    state.menuSelectionKey = menuKeyOf(sel);
    const enabled = state.editing && !state.blocked && !sel.dynamic;
    if (sel.dynamic) {
      els.contextMenu.innerHTML = `
        <div class="menu-head">
          <span class="menu-title">${esc(sel.tag)} · 只读区域</span>
          <button type="button" class="icon-btn" data-action="close-menu" aria-label="关闭">×</button>
        </div>
        <div class="readonly-panel">
          <strong>当前区域没有唯一的源码映射</strong>
          <p>${esc(sel.reason || '可能是脚本生成的内容，或源码标识不唯一。')}</p>
          <p>原页面会继续正常渲染。请选择相邻的可编辑元素，或从组件库插入新内容；其他区域不受影响。</p>
          <button type="button" class="btn subtle" data-action="bridge-parent" data-testid="editor-menu-bridge-parent">选择最近的源码容器</button>
        </div>
        <div class="menu-foot">动态区域保持只读，不阻断其他区域编辑。</div>
      `;
      els.contextMenu.hidden = false;
      positionMenu(coords);
      return;
    }
    const activeTab = MENU_TABS.some((item) => item.id === tab) ? tab : 'content';
    state.menuTab = activeTab;
    const tabs = MENU_TABS.map((item) => `
      <button type="button" class="menu-tab${item.id === activeTab ? ' active' : ''}"
        data-menu-tab="${item.id}" role="tab" aria-selected="${item.id === activeTab}"
        data-testid="editor-menu-tab-${item.id}">${item.label}</button>
    `).join('');
    const groups = inspectorGroups(sel, 'menu', enabled)
      .map((group, index) => {
        const id = group.datasetGroup;
        const active = id === activeTab;
        return group.html.replace(
          `<section class="insp-group" data-group="${id}">`,
          `<section class="insp-group${active ? ' active' : ''}" data-group="${id}"${index === 0 ? '' : ''}>`
        );
      }).join('');
    els.contextMenu.innerHTML = `
      <div class="menu-head">
        <span class="menu-title">${esc(sel.tag)} · ${esc(sel.id ? sel.id.slice(0, 24) : '无标识')}</span>
        <button type="button" class="icon-btn" data-action="close-menu" aria-label="关闭快捷菜单">×</button>
      </div>
      <div class="menu-tabs" role="tablist">${tabs}</div>
      <div class="menu-body">${groups}</div>
      <div class="menu-foot">样式只作用于当前元素的内联样式，不修改共享 CSS；保存后写回源码。绝对/固定定位元素拖动时写入 left/top（可为负值，相对 offsetParent 或视口），保留 position 与 transform；普通文档流元素仍按结构移动。</div>
    `;
    els.contextMenu.hidden = false;
    positionMenu(state.menuCoords);
  }

  function positionMenu(coords) {
    if (!coords) return;
    const frameRect = els.frame.getBoundingClientRect();
    const box = els.contextMenu.getBoundingClientRect();
    let left = frameRect.left + coords.x;
    let top = frameRect.top + coords.y;
    left = clamp(left, 8, window.innerWidth - box.width - 8);
    top = clamp(top, 8, window.innerHeight - box.height - 8);
    els.contextMenu.style.left = `${left}px`;
    els.contextMenu.style.top = `${top}px`;
  }

  function menuKeyOf(sel) {
    return sel ? `${sel.id}|${sel.tag}|${sel.dynamic ? 1 : 0}|${sel.partial ? 1 : 0}` : '';
  }

  function inspectorKeyOf(sel) {
    return sel ? `${sel.id}|${sel.tag}|${sel.dynamic ? 1 : 0}|${sel.partial ? 1 : 0}|${sel.reason}` : '';
  }

  /* -------------------------------------------------------------- inspector */

  function renderInspector(sel) {
    if (!sel) {
      state.inspectorKey = '';
      els.inspectorScope.textContent = '未选择';
      els.inspectorBody.innerHTML = emptyInspectorHtml();
      return;
    }
    state.inspectorKey = inspectorKeyOf(sel);
    els.inspectorScope.textContent = sel.dynamic ? '只读区域' : sel.tag;
    const enabled = state.editing && !state.blocked && !sel.dynamic;
    const parts = [summaryHtml(sel, 'sidebar')];
    if (sel.dynamic) {
      parts.push(`
        <div class="readonly-panel">
          <strong>动态或未映射区域，当前只读</strong>
          <p>${esc(sel.reason || '该区域没有唯一的源码映射。')}</p>
          <p>原页面继续正常渲染。请选择相邻的可编辑元素，或从组件库插入新内容。</p>
          <button type="button" class="btn subtle" data-action="bridge-parent" data-testid="editor-sidebar-bridge-parent">选择最近的源码容器</button>
        </div>
      `);
    } else {
      parts.push(...inspectorGroups(sel, 'sidebar', enabled).map((group) => group.html));
      parts.push('<p class="scope-hint">样式写入所选元素的源码，不修改共享 CSS。React 和 Vue 修改组件定义；重复实例和无法确定范围的动态区域只读。修改后请检查移动布局。文字与属性修改会写回源码。</p>');
    }
    if (!state.editing) {
      parts.unshift('<p class="summary-note info">当前是交互预览模式。点击“可视化编辑”后可修改属性。</p>');
    }
    if (state.blocked) {
      parts.unshift(`<p class="summary-note">${!state.backendAvailable ? '框架编译服务已停止。请重启编辑器；未保存草稿未写入项目。' : state.stale ? '源码冲突未解决，编辑已暂停。请重新加载项目。' : '上一次操作结果未知，写入已被永久阻止。请查看最后已知草稿并核对持久审计记录，确认后由宿主恢复会话。'}</p>`);
    }
    els.inspectorBody.innerHTML = parts.join('');
  }

  function renderInspectorPlaceholder(message) {
    els.inspectorScope.textContent = '刷新中';
    els.inspectorBody.innerHTML = `
      <div class="empty-state">
        <h3>正在刷新预览</h3>
        <p>${esc(message)}</p>
      </div>
    `;
  }

  function emptyInspectorHtml() {
    return `
      <div class="empty-state">
        <h3>未选择元素</h3>
        <p>在预览中悬停查看边界，点击选择元素；右键打开就近的快捷修改。</p>
        <ul>
          <li>拖动元素调整顺序，支持跨容器移动</li>
          <li>拖动右侧或底部手柄调整宽高</li>
          <li>通过选区“+”或右键插入组件</li>
          <li>Ctrl/Cmd+Z 撤销，加 Shift 重做</li>
        </ul>
        <div class="scope-box">
          支持 HTML/CSS 与 Vite React、Vue 3 项目的静态元素：文字、图片、按钮、表单控件、卡片、容器与网格。动态或重复区域保持只读。修改写回选中元素所在的源文件。
        </div>
      </div>
    `;
  }

  function summaryHtml(sel, prefix) {
    const node = sel.id ? nodeById(sel.id) : null;
    const crumbs = breadcrumbHtml(sel.id, prefix);
    let note = '';
    if (sel.partial) {
      note = '<p class="summary-note info">点击位置包含未映射内容，已选中最近的源码容器。文字与结构操作不可用；动态生成的内容保持原样。</p>';
    } else if (state.stale) {
      note = '<p class="summary-note">源码已在编辑器外变化，保存被阻止。请重新加载项目。</p>';
    } else if (state.unknown) {
      note = '<p class="summary-note">上一次操作结果未知。服务会继续拒绝写入；请查看当前源码并核对持久审计记录。</p>';
    }
    return `
      <div class="selection-summary">
        <div class="summary-top">
          <span class="summary-tag">${esc(sel.tag)}</span>
          ${node ? `<span class="badge">第 ${node.line} 行</span>` : ''}
          ${sel.dynamic ? '<span class="badge readonly">动态只读</span>' : ''}
          ${sel.partial ? '<span class="badge">含未映射内容</span>' : ''}
        </div>
        <div class="summary-meta">${node ? `${esc(node.file || state.snapshot.path)}:${node.line}` : '无源码位置'}${node && node.parent ? ` · 父级 ${esc(parentTag(node.parent))}` : ''}</div>
        ${crumbs}
        ${note}
      </div>
    `;
  }

  function breadcrumbHtml(id, prefix) {
    if (!id) return '';
    const chain = ancestorChain(id);
    if (!chain.length) return '';
    return `<div class="breadcrumb">${chain.map((node, index) => `
      <button type="button" class="crumb${node.id === id ? ' current' : ''}"
        data-action="select-node" data-node-id="${escAttr(node.id)}"
        data-testid="editor-${prefix}-crumb-${index}"
        title="选择 ${escAttr(node.tag)}">${esc(node.tag)}</button>
    `).join('<span aria-hidden="true">›</span>')}</div>`;
  }

  function inspectorGroups(sel, prefix, enabled) {
    const structureEnabled = enabled && !sel.partial;
    return [
      { datasetGroup: 'content', html: contentGroup(sel, prefix, structureEnabled) },
      { datasetGroup: 'color', html: colorGroup(sel, prefix, enabled) },
      { datasetGroup: 'size', html: sizeGroup(sel, prefix, enabled) },
      { datasetGroup: 'font', html: fontGroup(sel, prefix, enabled) },
      { datasetGroup: 'layout', html: layoutGroup(sel, prefix, enabled) },
      { datasetGroup: 'actions', html: actionsGroup(sel, prefix, structureEnabled) }
    ];
  }

  function dis(enabled) {
    return enabled ? '' : ' disabled';
  }

  function contentGroup(sel, p, enabled) {
    const d = dis(enabled);
    const text = sel.text || '';
    const showSrc = sel.tag === 'img' || Boolean(sel.attributes.src);
    const showHref = sel.tag === 'a' || Boolean(sel.attributes.href);
    return `
      <section class="insp-group" data-group="content">
        <h3>内容</h3>
        <div class="field-row">
          <span class="field-caption"><span>文字</span><span class="field-value-hint">${text.length} 字符</span></span>
          <textarea class="text-input" data-commit="text" data-testid="editor-${p}-text"${d} placeholder="元素文字" aria-label="元素文字">${esc(text)}</textarea>
          <div class="input-row">
            <button type="button" class="btn small subtle" data-action="commit" data-testid="editor-${p}-text-apply"${d}>应用文字</button>
            <span class="field-value-hint">有嵌套子元素时不能直接替换</span>
          </div>
        </div>
        ${showSrc ? `
          <div class="field-row">
            <span class="field-caption"><span>图片地址</span><span class="field-value-hint">${esc(truncate(sel.attributes.src || '未设置', 34))}</span></span>
            <div class="input-row">
              <input class="text-input" data-commit="attribute" data-name="src" data-testid="editor-${p}-src"
                value="${escAttr(sel.attributes.src || '')}"${d} placeholder="/images/a.png 或 https://…" aria-label="图片地址">
              <button type="button" class="btn small subtle" data-action="commit" data-testid="editor-${p}-src-apply"${d}>应用</button>
            </div>
          </div>
        ` : ''}
        ${showHref ? `
          <div class="field-row">
            <span class="field-caption"><span>链接地址</span><span class="field-value-hint">${esc(truncate(sel.attributes.href || '未设置', 34))}</span></span>
            <div class="input-row">
              <input class="text-input" data-commit="attribute" data-name="href" data-testid="editor-${p}-href"
                value="${escAttr(sel.attributes.href || '')}"${d} placeholder="#section 或 https://…" aria-label="链接地址">
              <button type="button" class="btn small subtle" data-action="commit" data-testid="editor-${p}-href-apply"${d}>应用</button>
            </div>
          </div>
        ` : ''}
        <p class="scope-hint">文字、图片地址和链接地址会写回源码；不会改动事件处理代码。每次修改在点击应用或回车后提交一次。</p>
      </section>
    `;
  }

  function colorGroup(sel, p, enabled) {
    const d = dis(enabled);
    const presets = colorPresets(sel);
    const rows = [
      { label: '文字颜色', prop: 'color', slug: 'color-text', current: sel.computed.color },
      { label: '背景颜色', prop: 'background-color', slug: 'color-background', current: sel.computed.backgroundColor },
      { label: '边框颜色', prop: 'border-color', slug: 'color-border', current: sel.computed.borderColor }
    ];
    return `
      <section class="insp-group" data-group="color">
        <h3>颜色</h3>
        ${rows.map((row) => {
          const hex = toOpaqueHexColor(row.current) || '#000000';
          const inputValue = colorInputValue(row.current) || hex;
          const current = isTransparent(row.current) ? '透明' : row.current;
          return `
            <div class="field-row">
              <span class="field-caption"><span>${row.label}</span><span class="field-value-hint">${esc(truncate(current, 26))}</span></span>
              <div class="color-row">
                <input type="color" class="color-swatch" value="${hex}" data-immediate="style" data-prop="${row.prop}"
                  data-testid="editor-${p}-${row.slug}"${d} aria-label="${row.label}">
                <input class="text-input hex-input" value="${escAttr(inputValue)}" data-commit="style" data-prop="${row.prop}"
                  data-format="color" data-testid="editor-${p}-${row.slug}-hex"${d} aria-label="${row.label}颜色值">
                <button type="button" class="btn small subtle" data-action="commit"
                  data-testid="editor-${p}-${row.slug}-apply"${d}>应用</button>
              </div>
              <div class="chips">
                ${presets.map((color) => `
                  <button type="button" class="chip" style="--chip-color:${color}" data-action="style"
                    data-prop="${row.prop}" data-value="${color}" title="${color}" aria-label="设为 ${color}"${d}></button>
                `).join('')}
              </div>
            </div>
          `;
        }).join('')}
        <div class="preset-row">
          <button type="button" class="btn small subtle" data-action="border-on" data-testid="editor-${p}-border-on"${d}>加 1px 实线边框</button>
        </div>
        <p class="scope-hint">色板、颜色值和预设色都会立即预览。只改当前元素的内联颜色，不修改共享样式表。</p>
      </section>
    `;
  }

  function sizeGroup(sel, p, enabled) {
    const d = dis(enabled);
    const rows = [
      { label: '宽度', prop: 'width', slug: 'width', current: sel.computed.width, presets: [['自动', 'auto'], ['填满', '100%'], ['240', '240px'], ['320', '320px']] },
      { label: '高度', prop: 'height', slug: 'height', current: sel.computed.height, presets: [['自动', 'auto'], ['120', '120px'], ['240', '240px'], ['360', '360px']] }
    ];
    return `
      <section class="insp-group" data-group="size">
        <h3>大小</h3>
        ${rows.map((row) => `
          <div class="field-row">
            <span class="field-caption"><span>${row.label}</span><span class="field-value-hint">当前 ${esc(truncate(row.current, 20))}</span></span>
            <div class="input-row">
              <input class="text-input" data-commit="style" data-prop="${row.prop}" data-format="length"
                data-testid="editor-${p}-${row.slug}"${d} placeholder="${escAttr(row.current)} (留空不改)" aria-label="${row.label}">
              <span class="unit">px</span>
              <button type="button" class="btn small subtle" data-action="commit" data-testid="editor-${p}-${row.slug}-apply"${d}>应用</button>
            </div>
            <div class="preset-row">
              ${row.presets.map(([label, value]) => `
                <button type="button" class="btn small" data-action="style" data-prop="${row.prop}"
                  data-value="${value}"${d}>${esc(label)}</button>
              `).join('')}
            </div>
          </div>
        `).join('')}
        <p class="scope-hint">尺寸以普通文档流的内联宽高写入，不把元素改成绝对定位；只影响当前元素。未修改的规则保持不变；修改后请检查移动布局。</p>
      </section>
    `;
  }

  function fontGroup(sel, p, enabled) {
    const d = dis(enabled);
    const currentFamily = sel.computed.fontFamily || '';
    const normalizedFamily = normalizeFontFamily(currentFamily);
    const inherited = !normalizedFamily || normalizedFamily === 'inherit';
    const matchedFamily = !inherited && FONT_FAMILIES.some(
      (option) => option.value !== 'inherit' && normalizeFontFamily(option.value) === normalizedFamily
    );
    const currentOption = !inherited && !matchedFamily
      ? `<option value="${escAttr(currentFamily)}" selected>当前：${esc(truncate(currentFamily, 30))}</option>`
      : '';
    const familyOptions = currentOption + FONT_FAMILIES.map((option) => {
      const selected = option.value === 'inherit'
        ? inherited
        : normalizeFontFamily(option.value) === normalizedFamily;
      return `<option value="${escAttr(option.value)}"${selected ? ' selected' : ''}>${esc(option.label)}</option>`;
    }).join('');
    const weights = ['inherit', '300', '400', '500', '600', '700', '800'];
    const currentWeight = (sel.computed.fontWeight || '').trim();
    const weightOptions = weights.map((value) => `
      <option value="${value}"${value === currentWeight ? ' selected' : ''}>${value === 'inherit' ? '继承' : value}</option>
    `).join('');
    const align = normalizeTextAlign(sel.computed.textAlign);
    return `
      <section class="insp-group" data-group="font">
        <h3>字体</h3>
        <div class="field-row">
          <span class="field-caption"><span>字体</span><span class="field-value-hint">${esc(truncate(currentFamily, 26))}</span></span>
          <select class="text-input" data-immediate="style" data-prop="font-family"
            data-testid="editor-${p}-font-family"${d} aria-label="字体">${familyOptions}</select>
        </div>
        <div class="field-row">
          <span class="field-caption"><span>字号</span><span class="field-value-hint">当前 ${esc(sel.computed.fontSize || '')}</span></span>
          <div class="input-row">
            <input class="text-input" data-commit="style" data-prop="font-size" data-format="length"
              data-testid="editor-${p}-font-size"${d} placeholder="例如 18" aria-label="字号">
            <span class="unit">px</span>
            <button type="button" class="btn small subtle" data-action="commit" data-testid="editor-${p}-font-size-apply"${d}>应用</button>
          </div>
        </div>
        <div class="field-row">
          <span class="field-caption"><span>字重</span><span class="field-value-hint">当前 ${esc(currentWeight || '400')}</span></span>
          <select class="text-input" data-immediate="style" data-prop="font-weight"
            data-testid="editor-${p}-font-weight"${d} aria-label="字重">${weightOptions}</select>
        </div>
        <div class="field-row">
          <span class="field-caption"><span>行高</span><span class="field-value-hint">当前 ${esc(sel.computed.lineHeight || '')}</span></span>
          <div class="input-row">
            <input class="text-input" data-commit="style" data-prop="line-height" data-format="line-height"
              data-testid="editor-${p}-line-height"${d} placeholder="例如 1.5 或 24px" aria-label="行高">
            <button type="button" class="btn small subtle" data-action="commit" data-testid="editor-${p}-line-height-apply"${d}>应用</button>
          </div>
        </div>
        <div class="field-row">
          <span class="field-caption"><span>对齐</span></span>
          <div class="align-row" role="group" aria-label="文字对齐">
            ${[['左', 'left'], ['中', 'center'], ['右', 'right'], ['两端', 'justify']].map(([label, value]) => `
              <button type="button" class="${align === value ? 'active' : ''}" data-action="style" data-prop="text-align"
                data-value="${value}" data-testid="editor-${p}-align-${value}" aria-pressed="${align === value}"${d}>${label}</button>
            `).join('')}
          </div>
        </div>
        <p class="scope-hint">字体修改会立即预览；字号、行高在点击应用或回车后提交一次。</p>
      </section>
    `;
  }

  function layoutGroup(sel, p, enabled) {
    const d = dis(enabled);
    const display = (sel.computed.display || 'block').trim();
    const displayOptions = ['block', 'flex', 'grid', 'inline', 'inline-block', 'inline-flex']
      .map((value) => `<option value="${value}"${value === display ? ' selected' : ''}>${value}</option>`)
      .join('');
    const direction = (sel.computed.flexDirection || 'row').trim();
    const align = (sel.computed.alignItems || '').trim();
    const justify = (sel.computed.justifyContent || '').trim();
    return `
      <section class="insp-group" data-group="layout">
        <h3>布局</h3>
        <div class="field-row">
          <span class="field-caption"><span>显示方式</span><span class="field-value-hint">${esc(display)}</span></span>
          <select class="text-input" data-immediate="style" data-prop="display"
            data-testid="editor-${p}-display"${d} aria-label="显示方式">${displayOptions}</select>
        </div>
        <div class="field-row">
          <span class="field-caption"><span>排列方向</span><span class="field-value-hint">${esc(direction)}</span></span>
          <div class="align-row" role="group" aria-label="排列方向">
            ${[['横排', 'row'], ['竖排', 'column'], ['反横', 'row-reverse'], ['反竖', 'column-reverse']].map(([label, value]) => `
              <button type="button" class="${direction === value ? 'active' : ''}" data-action="style" data-prop="flex-direction"
                data-value="${value}" data-testid="editor-${p}-direction-${value}" aria-pressed="${direction === value}"${d}>${label}</button>
            `).join('')}
          </div>
        </div>
        <div class="field-row">
          <span class="field-caption"><span>间距</span><span class="field-value-hint">当前 ${esc(sel.computed.gap || '未设置')}</span></span>
          <div class="input-row">
            <input class="text-input" data-commit="style" data-prop="gap" data-format="spacing"
              data-testid="editor-${p}-gap"${d} placeholder="例如 16" aria-label="间距">
            <span class="unit">px</span>
            <button type="button" class="btn small subtle" data-action="commit" data-testid="editor-${p}-gap-apply"${d}>应用</button>
          </div>
        </div>
        <div class="field-row">
          <span class="field-caption"><span>网格列数</span><span class="field-value-hint">${esc(truncate(sel.computed.gridTemplateColumns || '未设置', 22))}</span></span>
          <div class="input-row">
            <input class="text-input" type="number" min="1" data-commit="grid-columns"
              data-testid="editor-${p}-grid-columns"${d} placeholder="正整数" aria-label="网格列数">
            <button type="button" class="btn small subtle" data-action="commit" data-testid="editor-${p}-grid-columns-apply"${d}>应用</button>
          </div>
        </div>
        <div class="field-row">
          <span class="field-caption"><span>交叉轴对齐</span></span>
          <select class="text-input" data-immediate="style" data-prop="align-items"
            data-testid="editor-${p}-align-items"${d} aria-label="交叉轴对齐">
            <option value="inherit"${align === 'inherit' ? ' selected' : ''}>继承</option>
            ${['stretch', 'flex-start', 'center', 'flex-end', 'baseline'].map((value) => `<option value="${value}"${align === value ? ' selected' : ''}>${value}</option>`).join('')}
          </select>
        </div>
        <div class="field-row">
          <span class="field-caption"><span>主轴分布</span></span>
          <select class="text-input" data-immediate="style" data-prop="justify-content"
            data-testid="editor-${p}-justify-content"${d} aria-label="主轴分布">
            <option value="inherit"${justify === 'inherit' ? ' selected' : ''}>继承</option>
            ${['flex-start', 'center', 'flex-end', 'space-between', 'space-around', 'space-evenly'].map((value) => `<option value="${value}"${justify === value ? ' selected' : ''}>${value}</option>`).join('')}
          </select>
        </div>
        <div class="field-row">
          <span class="field-caption"><span>内边距</span></span>
          <div class="input-row">
            <input class="text-input" data-commit="style" data-prop="padding" data-format="spacing"
              data-testid="editor-${p}-padding"${d} placeholder="例如 16 或 16 24" aria-label="内边距">
            <button type="button" class="btn small subtle" data-action="commit" data-testid="editor-${p}-padding-apply"${d}>应用</button>
          </div>
        </div>
        <div class="field-row">
          <span class="field-caption"><span>外边距</span></span>
          <div class="input-row">
            <input class="text-input" data-commit="style" data-prop="margin" data-format="spacing"
              data-testid="editor-${p}-margin"${d} placeholder="例如 0 auto" aria-label="外边距">
            <button type="button" class="btn small subtle" data-action="commit" data-testid="editor-${p}-margin-apply"${d}>应用</button>
          </div>
        </div>
        <p class="scope-hint">普通文档流保持原有布局方式。绝对/固定定位元素可直接拖动，写入 left/top；坐标可为负值，absolute 相对 offsetParent，fixed 相对视口，position 与 transform 保持不变。拖拽排序与宽高手柄不会把所有元素改成固定坐标。</p>
      </section>
    `;
  }

  function actionsGroup(sel, p, enabled) {
    const d = dis(enabled);
    const node = sel.id ? nodeById(sel.id) : null;
    const parentDisabled = !node || !node.parent || !state.editing || state.blocked || sel.dynamic;
    return `
      <section class="insp-group" data-group="actions">
        <h3>组件操作</h3>
        <div class="action-grid">
          <button type="button" class="btn subtle" data-action="insert-before" data-testid="editor-${p}-insert-before"${d}>前插入</button>
          <button type="button" class="btn subtle" data-action="insert-after" data-testid="editor-${p}-insert-after"${d}>后插入</button>
          <button type="button" class="btn subtle wide" data-action="insert-inside" data-testid="editor-${p}-insert-inside"${d}>插入到容器内部</button>
          <button type="button" class="btn subtle" data-action="select-parent" data-testid="editor-${p}-select-parent"${parentDisabled ? ' disabled' : ''}>选择父级</button>
          <button type="button" class="btn subtle" data-action="move-up" data-testid="editor-${p}-move-up"${d}>上移</button>
          <button type="button" class="btn subtle" data-action="move-down" data-testid="editor-${p}-move-down"${d}>下移</button>
          <button type="button" class="btn subtle" data-action="duplicate" data-testid="editor-${p}-duplicate"${d}>复制</button>
          <button type="button" class="btn subtle danger-text" data-action="delete" data-testid="editor-${p}-delete"${d}>删除</button>
        </div>
        <p class="scope-hint">插入、复制、删除和移动都通过结构化操作写回，可在保存前撤销。</p>
      </section>
    `;
  }

  /* -------------------------------------------------------- inspector events */

  function bindInspector(container, prefix) {
    container.addEventListener('click', (event) => {
      const tab = event.target.closest('[data-menu-tab]');
      if (tab) {
        activateMenuTab(tab.dataset.menuTab, container);
        return;
      }
      const button = event.target.closest('button[data-action]');
      if (!button || button.disabled) return;
      onInspectorAction(button, container);
    });
    container.addEventListener('change', onInspectorChange);
    container.addEventListener('keydown', (event) => {
      if (event.key !== 'Enter') return;
      const target = event.target;
      if (target.tagName === 'TEXTAREA' && !(event.metaKey || event.ctrlKey)) return;
      if (target.matches('input[data-commit],textarea[data-commit]')) {
        event.preventDefault();
        commitInput(target);
      }
    });
    container.dataset.prefix = prefix;
  }

  function onInspectorAction(button, container) {
    const action = button.dataset.action;
    if (action === 'close-menu') {
      closeMenu();
      return;
    }
    if (action === 'commit') {
      const row = button.closest('.field-row') || button.closest('.insp-group');
      const input = row ? row.querySelector('[data-commit]') : null;
      if (input) commitInput(input);
      return;
    }
    if (action === 'style') {
      const prop = button.dataset.prop;
      const value = button.dataset.value;
      applyStyle({ [prop]: value });
      return;
    }
    if (action === 'border-on') {
      applyStyle({ 'border-width': '1px', 'border-style': 'solid' });
      return;
    }
    if (action === 'bridge-parent') {
      closeMenu();
      sendToBridge({ type: 'cayw:parent' });
      return;
    }
    if (action === 'select-node') {
      selectNode(button.dataset.nodeId);
      return;
    }
    if (action === 'select-parent') {
      selectParent();
      return;
    }
    if (action === 'duplicate' || action === 'delete') {
      if (state.selection && state.selection.partial) {
        toast('该选区包含未映射内容，不能执行结构操作。', 'warn');
        return;
      }
      if (state.selectedId) performOperation({ kind: action, id: state.selectedId });
      return;
    }
    if (action === 'move-up') {
      if (state.selection && state.selection.partial) {
        toast('该选区包含未映射内容，不能执行结构操作。', 'warn');
        return;
      }
      moveSibling(-1);
      return;
    }
    if (action === 'move-down') {
      if (state.selection && state.selection.partial) {
        toast('该选区包含未映射内容，不能执行结构操作。', 'warn');
        return;
      }
      moveSibling(1);
      return;
    }
    if (action === 'insert-before' || action === 'insert-inside' || action === 'insert-after') {
      if (state.selection && state.selection.partial) {
        toast('该选区包含未映射内容，不能插入结构；请选择相邻源码元素。', 'warn');
        return;
      }
      const placement = action === 'insert-before' ? 'before' : action === 'insert-inside' ? 'inside' : 'after';
      const rect = button.getBoundingClientRect();
      openPopover(rect, placement);
      return;
    }
  }

  function onInspectorChange(event) {
    const target = event.target;
    if (target.matches('[data-immediate="style"]')) {
      const prop = target.dataset.prop;
      let value = target.value;
      if (!value) return;
      if (prop === 'font-family' && value === 'inherit') {
        value = 'inherit';
      }
      if (!validateImmediate(prop, value)) return;
      applyStyle({ [prop]: value });
    }
  }

  function validateImmediate(prop, value) {
    if (prop === 'font-family' || prop === 'display' || prop === 'font-weight' ||
        prop === 'align-items' || prop === 'justify-content') {
      return typeof value === 'string' && value.length <= 160;
    }
    return true;
  }

  function activateMenuTab(name, container) {
    if (!MENU_TABS.some((tab) => tab.id === name)) return;
    state.menuTab = name;
    container.querySelectorAll('[data-menu-tab]').forEach((tab) => {
      const active = tab.dataset.menuTab === name;
      tab.classList.toggle('active', active);
      tab.setAttribute('aria-selected', String(active));
    });
    const body = container.querySelector('.menu-body');
    if (!body) return;
    body.querySelectorAll('.insp-group').forEach((group) => {
      group.classList.toggle('active', group.dataset.group === name);
    });
  }

  function commitInput(input) {
    if (!state.editing || state.blocked) return;
    const kind = input.dataset.commit;
    if (kind === 'text') {
      if (state.selection && state.selection.partial) {
        toast('该选区包含未映射内容，不能修改文字。', 'warn');
        return;
      }
      const text = input.value;
      if (text.length > MAX_TEXT) {
        toast(`文字超过 ${MAX_TEXT} 个字符，操作已拒绝。`, 'error');
        return;
      }
      if (!state.selectedId) return;
      applyOperation({ kind: 'text', id: state.selectedId, text });
      return;
    }
    if (kind === 'attribute') {
      const name = input.dataset.name;
      const value = input.value.trim();
      if (!ATTRIBUTE_NAMES.has(name)) {
        toast('不支持的属性。', 'error');
        return;
      }
      if (value.length > MAX_ATTRIBUTE) {
        toast('属性值过长，操作已拒绝。', 'error');
        return;
      }
      if (!state.selectedId) return;
      applyOperation({ kind: 'attribute', id: state.selectedId, name, value });
      return;
    }
    if (kind === 'grid-columns') {
      const count = Number(input.value);
      if (!Number.isInteger(count) || count < 1) {
        toast('网格列数需要是正整数。', 'warn');
        return;
      }
      applyStyle({ 'grid-template-columns': `repeat(${count}, minmax(0, 1fr))` });
      return;
    }
    if (kind === 'style') {
      const prop = input.dataset.prop;
      const format = input.dataset.format || 'raw';
      const value = normalizeStyleInput(prop, format, input.value);
      if (value === null) return;
      applyStyle({ [prop]: value });
    }
  }

  function normalizeStyleInput(prop, format, rawValue) {
    const value = String(rawValue || '').trim();
    if (!value) {
      toast('请先填写一个值。', 'warn');
      return null;
    }
    if (format === 'color') {
      const color = normalizeColorInput(value);
      if (!color) {
        toast('颜色值需要是 #rgb、#rrggbb、#rrggbbaa 或 rgb()/rgba() 格式。', 'warn');
        return null;
      }
      return color;
    }
    const cssValue = format === 'length' && /^\d+(\.\d+)?$/.test(value)
      ? `${value}px`
      : format === 'spacing'
        ? value.replace(/(^|\s)(-?\d+(?:\.\d+)?)(?=\s|$)/g, '$1$2px')
        : value;
    if (CSS.supports(prop, cssValue)) return cssValue;
    toast('该样式值无效，请填写有效的 CSS 值。', 'warn');
    return null;
  }

  function normalizeColorInput(value) {
    const color = parseCssColor(value);
    return color ? color.hex8 : null;
  }

  function applyStyle(properties) {
    if (!state.selectedId) {
      toast('请先选择元素。', 'warn');
      return;
    }
    if (!state.editing || state.blocked) return;
    applyOperation({ kind: 'style', id: state.selectedId, properties });
  }

  function applyOperation(operation) {
    performOperation(operation);
  }

  function moveSibling(direction) {
    const node = nodeById(state.selectedId);
    if (!node) return;
    const siblings = state.nodes
      .filter((item) => item.file === node.file && item.parent === node.parent)
      .sort((a, b) => a.start - b.start);
    const index = siblings.findIndex((item) => item.id === node.id);
    if (index < 0) return;
    const targetIndex = index + direction;
    if (targetIndex < 0 || targetIndex >= siblings.length) {
      toast('已经到达同级的边缘。', 'warn');
      return;
    }
    const target = siblings[targetIndex];
    performOperation({
      kind: 'move',
      id: node.id,
      target: target.id,
      placement: direction < 0 ? 'before' : 'after'
    });
  }

  /* ------------------------------------------------------- operation guard */

  function sanitizeOperation(raw) {
    if (!raw || typeof raw !== 'object' || Array.isArray(raw)) {
      throw new Error('操作必须是对象。');
    }
    const kind = raw.kind;
    if (typeof kind !== 'string') throw new Error('操作缺少类型。');
    const id = typeof raw.id === 'string' ? raw.id : '';
    const validId = (value) => typeof value === 'string' && value.length > 0 && value.length <= MAX_ID;
    const validPlacement = (value) => value === 'before' || value === 'after' || value === 'inside';
    switch (kind) {
      case 'style': {
        if (!validId(id)) throw new Error('样式操作缺少目标元素。');
        if (!raw.properties || typeof raw.properties !== 'object' || Array.isArray(raw.properties)) {
          throw new Error('样式操作缺少属性。');
        }
        const properties = {};
        for (const [prop, value] of Object.entries(raw.properties)) {
          if (!STYLE_PROPS.has(prop)) throw new Error(`不支持的样式属性：${prop}`);
          if (typeof value !== 'string' || value.length > 160) throw new Error('样式值无效。');
          properties[prop] = value;
        }
        if (!Object.keys(properties).length) throw new Error('样式操作没有有效属性。');
        return { kind, id, properties };
      }
      case 'text': {
        if (!validId(id)) throw new Error('文字操作缺少目标元素。');
        if (typeof raw.text !== 'string') throw new Error('文字内容无效。');
        if (raw.text.length > MAX_TEXT) throw new Error('文字内容过长。');
        return { kind, id, text: raw.text };
      }
      case 'attribute': {
        if (!validId(id)) throw new Error('属性操作缺少目标元素。');
        if (typeof raw.name !== 'string' || !ATTRIBUTE_NAMES.has(raw.name)) {
          throw new Error('不支持的属性名。');
        }
        if (typeof raw.value !== 'string' || raw.value.length > MAX_ATTRIBUTE) {
          throw new Error('属性值无效。');
        }
        return { kind, id, name: raw.name, value: raw.value };
      }
      case 'move': {
        if (!validId(id) || !validId(raw.target)) throw new Error('移动操作缺少目标。');
        if (!validPlacement(raw.placement)) throw new Error('移动位置无效。');
        return { kind, id, target: raw.target, placement: raw.placement };
      }
      case 'insert': {
        if (!validId(raw.target)) throw new Error('插入操作缺少目标元素。');
        if (!validPlacement(raw.placement)) throw new Error('插入位置无效。');
        if (typeof raw.component !== 'string' || !COMPONENT_NAMES.has(raw.component)) {
          throw new Error('未知组件。');
        }
        return { kind, target: raw.target, placement: raw.placement, component: raw.component };
      }
      case 'duplicate':
        if (!validId(id)) throw new Error('复制操作缺少目标元素。');
        return { kind, id };
      case 'delete':
        if (!validId(id)) throw new Error('删除操作缺少目标元素。');
        return { kind, id };
      case 'undo':
      case 'redo':
        return { kind };
      default:
        throw new Error(`不支持的操作：${kind}`);
    }
  }

  /* ------------------------------------------------------------- selection map */

  function nodeById(id) {
    if (!id) return null;
    return state.nodes.find((node) => node.id === id) || null;
  }

  function parentTag(parentId) {
    const parent = nodeById(parentId);
    return parent ? parent.tag : '未知';
  }

  function ancestorChain(id) {
    const map = new Map(state.nodes.map((node) => [node.id, node]));
    const chain = [];
    let current = map.get(id);
    let guard = 0;
    while (current && guard < 12) {
      chain.unshift(current);
      current = current.parent ? map.get(current.parent) : null;
      guard += 1;
    }
    return chain;
  }

  /* ------------------------------------------------------------------ global */

  function bindGlobal() {
    window.addEventListener('message', onBridgeMessage);
    window.addEventListener('resize', () => {
      repositionLabel();
      closeMenu();
      closePopover();
    });
    window.addEventListener('scroll', repositionLabel, true);
    document.addEventListener('keydown', onGlobalKeydown);
    els.frame.addEventListener('load', () => {
      if (!state.connected) return;
      // The bridge sends "cayw:ready" itself; this only covers pages that fail to load it.
      setTimeout(() => {
        if (!state.frameReady) {
          // Warning is emitted by loadFrame's timer.
        }
      }, 1200);
    });
  }

  function onGlobalKeydown(event) {
    if (event.isComposing) return;
    const mod = event.metaKey || event.ctrlKey;
    const typing = isTypingTarget(event.target);
    if (mod && !typing && event.key.toLowerCase() === 'z') {
      event.preventDefault();
      history(event.shiftKey ? 'redo' : 'undo');
      return;
    }
    if (mod && !typing && event.key.toLowerCase() === 'y') {
      event.preventDefault();
      history('redo');
      return;
    }
    if (mod && event.key.toLowerCase() === 's') {
      event.preventDefault();
      if (!els.workspace.hidden) saveDocument();
      return;
    }
    if (event.key === 'Escape') {
      if (!els.sourceViewer.hidden) {
        closeSourceViewer();
      } else if (!els.contextMenu.hidden) {
        closeMenu();
      } else if (!els.popover.hidden) {
        closePopover();
      } else {
        document.body.classList.remove('panel-palette', 'panel-inspector');
      }
    }
  }

  function isTypingTarget(target) {
    if (!target || target.nodeType !== 1) return false;
    const tag = target.tagName;
    return tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT' || target.isContentEditable;
  }

  /* ------------------------------------------------------------------ toasts */

  function toast(message, type = 'info', detail = '', duration = 0) {
    if (!message) return;
    const item = document.createElement('div');
    item.className = `toast ${type === 'info' ? '' : type}`.trim();
    const label = document.createElement('span');
    label.textContent = message;
    item.appendChild(label);
    if (detail) {
      const extra = document.createElement('span');
      extra.className = 'toast-detail';
      extra.textContent = detail;
      item.appendChild(extra);
    }
    item.addEventListener('click', () => removeToast(item));
    els.toasts.appendChild(item);
    while (els.toasts.children.length > 4) {
      els.toasts.removeChild(els.toasts.firstElementChild);
    }
    const timeout = duration || (type === 'error' ? 7000 : 4200);
    const timer = setTimeout(() => removeToast(item), timeout);
    state.toastTimers.add(timer);
    item._caywTimer = timer;
  }

  function removeToast(item) {
    if (!item || !item.isConnected) return;
    if (item._caywTimer) {
      clearTimeout(item._caywTimer);
      state.toastTimers.delete(item._caywTimer);
    }
    item.remove();
  }

  /* ------------------------------------------------------------------- utils */

  function esc(value) {
    return String(value == null ? '' : value)
      .replace(/&/g, '&amp;')
      .replace(/</g, '&lt;')
      .replace(/>/g, '&gt;')
      .replace(/"/g, '&quot;')
      .replace(/'/g, '&#39;');
  }

  function escAttr(value) {
    return esc(value);
  }

  function clamp(value, min, max) {
    return Math.min(Math.max(value, min), Math.max(min, max));
  }

  function truncate(value, length) {
    const text = String(value || '');
    return text.length > length ? `${text.slice(0, length - 1)}…` : text;
  }

  function isTransparent(value) {
    const text = String(value || '').trim();
    if (!text) return true;
    const color = parseCssColor(text);
    return Boolean(color && color.alpha === 0);
  }

  function toHexColor(value) {
    const color = parseCssColor(value);
    return color ? color.hex8 : null;
  }

  function toOpaqueHexColor(value) {
    const color = parseCssColor(value);
    return color ? color.hex : null;
  }

  function colorInputValue(value) {
    const color = parseCssColor(value);
    return color ? color.hex8 : '';
  }

  function parseCssColor(value) {
    const text = String(value || '').trim().toLowerCase();
    if (!text) return null;
    if (text === 'transparent') return buildColor(0, 0, 0, 0);
    if (text.startsWith('#')) {
      const digits = text.slice(1);
      if (!/^[0-9a-f]+$/.test(digits)) return null;
      if (digits.length === 3 || digits.length === 4) {
        const alpha = digits.length === 4 ? Number.parseInt(`${digits[3]}${digits[3]}`, 16) / 255 : 1;
        return buildColor(
          Number.parseInt(`${digits[0]}${digits[0]}`, 16),
          Number.parseInt(`${digits[1]}${digits[1]}`, 16),
          Number.parseInt(`${digits[2]}${digits[2]}`, 16),
          alpha
        );
      }
      if (digits.length === 6 || digits.length === 8) {
        const alpha = digits.length === 8 ? Number.parseInt(digits.slice(6, 8), 16) / 255 : 1;
        return buildColor(
          Number.parseInt(digits.slice(0, 2), 16),
          Number.parseInt(digits.slice(2, 4), 16),
          Number.parseInt(digits.slice(4, 6), 16),
          alpha
        );
      }
      return null;
    }
    const match = text.match(/^rgba?\((.*)\)$/);
    if (!match) return null;
    const tokens = match[1].trim().split(/[\s,/]+/).filter(Boolean);
    if (tokens.length < 3 || tokens.length > 4) return null;
    const channels = tokens.slice(0, 3).map((token) => {
      const number = token.endsWith('%')
        ? Number.parseFloat(token.slice(0, -1)) * 2.55
        : Number.parseFloat(token);
      return Number.isFinite(number) && number >= 0 && number <= 255 ? number : null;
    });
    if (channels.some((channel) => channel === null)) return null;
    let alpha = 1;
    if (tokens[3]) {
      alpha = tokens[3].endsWith('%')
        ? Number.parseFloat(tokens[3].slice(0, -1)) / 100
        : Number.parseFloat(tokens[3]);
      if (!Number.isFinite(alpha) || alpha < 0 || alpha > 1) return null;
    }
    return buildColor(channels[0], channels[1], channels[2], alpha);
  }

  function buildColor(red, green, blue, alpha) {
    const channels = [red, green, blue].map((channel) => {
      const value = Math.max(0, Math.min(255, Math.round(channel)));
      return value.toString(16).padStart(2, '0');
    });
    const hex = `#${channels.join('')}`;
    const clampedAlpha = Math.max(0, Math.min(1, alpha));
    const alphaByte = Math.round(clampedAlpha * 255).toString(16).padStart(2, '0');
    return {
      hex,
      hex8: clampedAlpha < 1 ? `${hex}${alphaByte}` : hex,
      alpha: clampedAlpha
    };
  }

  function normalizeTextAlign(value) {
    const text = String(value || '').trim().toLowerCase();
    if (text === 'start') return 'left';
    if (text === 'end') return 'right';
    return text;
  }

  function normalizeFontFamily(value) {
    return String(value || '')
      .toLowerCase()
      .replace(/["']/g, '')
      .replace(/\s*,\s*/g, ',')
      .replace(/\s+/g, ' ')
      .trim();
  }

  function tableChildAllowed(parentTag, childTag) {
    if (!TABLE_STRUCTURE_TAGS.has(parentTag)) return true;
    if (!childTag) return false;
    if (parentTag === 'table') {
      return ['caption', 'colgroup', 'thead', 'tbody', 'tfoot', 'tr'].includes(childTag);
    }
    if (parentTag === 'thead' || parentTag === 'tbody' || parentTag === 'tfoot') {
      return childTag === 'tr';
    }
    return childTag === 'td' || childTag === 'th';
  }

  function colorPresets(sel) {
    const list = [
      toOpaqueHexColor(sel.computed.color),
      toOpaqueHexColor(sel.computed.backgroundColor),
      '#111827',
      '#f8fafc',
      '#3b5bfd',
      '#0ea5e9',
      '#f59e0b',
      '#ef4444',
      '#10b981'
    ].filter((color) => color && /^#[0-9a-f]{6}$/.test(color));
    return [...new Set(list)].slice(0, 8);
  }
})();
