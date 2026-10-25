(function () {
  'use strict';

  if (typeof document === 'undefined') return;

  var SCHEMA = 'cayw.trace.v1';
  var SVG_NS = 'http://www.w3.org/2000/svg';
  var STATUS_LABELS = {
    running: '运行中',
    waiting: '等待响应',
    completed: '已完成',
    failed: '失败',
    blocked: '已阻断',
    outcome_unknown: '结果未知'
  };
  var MODE_LABELS = {
    observed: '原始记录（observed）',
    replay: '本地重放（replay）',
    recompute: '局部重算（recompute）'
  };
  var MODE_DETAILS = {
    observed: '原始记录（observed）：数据来自一次实际 Runtime 执行。',
    replay: '本地重放（replay）：从入口整图本地重放，只用捕获的 Host 响应。',
    recompute: '局部重算（recompute）：从修改节点起局部重算受影响下游；上游可复用，只用捕获的 Host 响应，不跑整项目构建。'
  };

  var activeControllers = new Set();
  var connected = false;
  var token = '';
  var doc = null;
  var pollGeneration = 0;
  var pollTimer = null;
  var pollBusy = false;
  var pollTick = null;
  var failures = 0;
  var loginPending = false;

  var selectedProject = null;
  var selectedEvaluation = null;
  var selectedNode = null;
  var moduleFilter = null;
  var compareVersion = null;
  var undoTarget = null;
  var drafts = new Map();
  var editorKey = null;
  var loadedEditorValues = null;
  var editorNotice = null;
  var pendingSave = false;
  var pendingUndo = false;
  var undoNotice = null;

  var nodeElements = new Map();
  var nodeCenters = new Map();
  var moduleElements = new Map();
  var evidenceIndexDoc = null;
  var evidenceIndex = new Map();
  var openEvidence = new Set();

  var ui = {};

  function boot() {
    [
      'login-panel', 'login-form', 'login-error', 'login-button', 'web-token',
      'workspace', 'global-error', 'connection-status', 'project-select', 'evaluation-select',
      'evaluation-mode', 'evaluation-status', 'document-sequence', 'project-summary',
      'session-captures', 'error-facts-list', 'timeline-meta', 'timeline-list',
      'module-graph', 'module-graph-note',
      'node-graph', 'graph-filter-label', 'graph-show-all', 'node-detail-subtitle',
      'node-empty', 'node-body', 'node-heading', 'node-error', 'node-gaps', 'node-facts',
      'node-source', 'node-params', 'node-pointer', 'save-node', 'discard-draft', 'save-status',
      'revision-count', 'revision-list', 'undo-version', 'undo-node', 'undo-status',
      'compare-select', 'comparison-body', 'gaps-list'
    ].forEach(function (id) { ui[id] = document.getElementById(id); });

    ui['login-form'].addEventListener('submit', login);
    ui['project-select'].addEventListener('change', onProjectChange);
    ui['evaluation-select'].addEventListener('change', onEvaluationChange);
    ui['compare-select'].addEventListener('change', onCompareChange);
    ui['undo-version'].addEventListener('change', onUndoTargetChange);
    ui['save-node'].addEventListener('click', saveNode);
    ui['discard-draft'].addEventListener('click', discardDraft);
    ui['undo-node'].addEventListener('click', undoToVersion);
    ui['graph-show-all'].addEventListener('click', showAllModules);
    ['node-source', 'node-params', 'node-pointer'].forEach(function (id) {
      ui[id].addEventListener('input', onEditorInput);
    });
    document.addEventListener('visibilitychange', function () {
      if (document.visibilityState === 'visible' && connected) {
        clearTimeout(pollTimer);
        if (!pollBusy && pollTick) pollTick();
      }
    });
    ui['web-token'].focus();
  }

  // ---------- small helpers ----------

  function el(tag, className, text) {
    var node = document.createElement(tag);
    if (className) node.className = className;
    if (text !== undefined && text !== null) node.textContent = String(text);
    return node;
  }

  function svgEl(tag, attrs) {
    var node = document.createElementNS(SVG_NS, tag);
    if (attrs) {
      Object.keys(attrs).forEach(function (key) { node.setAttribute(key, attrs[key]); });
    }
    return node;
  }

  function badge(text, className) {
    var node = el('span', 'badge ' + (className || 'badge-none'));
    node.textContent = text;
    return node;
  }

  function statusClass(status) {
    return Object.prototype.hasOwnProperty.call(STATUS_LABELS, status) ? 'status-' + status : 'status-none';
  }

  function statusLabel(status) {
    return STATUS_LABELS[status] || (status ? String(status) : '未执行');
  }

  function modeLabel(mode) {
    return MODE_LABELS[mode] || (mode ? String(mode) : '未标注');
  }

  function statusBadge(status) {
    return badge(statusLabel(status), statusClass(status));
  }

  function modeBadge(mode) {
    var kind = mode === 'observed' ? 'mode-observed' : mode === 'replay' ? 'mode-replay' : mode === 'recompute' ? 'mode-recompute' : 'badge-none';
    var node = badge(modeLabel(mode), kind);
    node.title = MODE_DETAILS[mode] || '';
    return node;
  }

  function clip(text, limit) {
    var value = text === undefined || text === null ? '' : String(text);
    return value.length > limit ? value.slice(0, limit) + '…' : value;
  }

  function jsonText(value) {
    if (value === undefined) return '—';
    try {
      var text = JSON.stringify(value, null, 2);
      return text === undefined ? String(value) : text;
    } catch (error) {
      return String(value);
    }
  }

  function jsonFactText(value) {
    if (value === undefined || value === null) return '—';
    return jsonText(value);
  }

  function summaryJson(value, limit) {
    if (value === undefined) return '—';
    try {
      var text = JSON.stringify(value);
      if (text === undefined) return clip(String(value), limit || 480);
      return clip(text, limit || 480);
    } catch (error) {
      return clip(String(value), limit || 480);
    }
  }

  function formatClock(ms) {
    if (typeof ms !== 'number' || !isFinite(ms) || ms <= 0) return '—';
    try {
      return new Date(ms).toLocaleTimeString('zh-CN', { hour12: false });
    } catch (error) {
      return '—';
    }
  }

  function formatTime(ms) {
    if (typeof ms !== 'number' || !isFinite(ms) || ms <= 0) return '—';
    try {
      return new Date(ms).toLocaleString('zh-CN', { hour12: false });
    } catch (error) {
      return '—';
    }
  }

  function formatMs(value) {
    if (typeof value !== 'number' || !isFinite(value)) return '—';
    if (value < 1000) return (Math.round(value * 10) / 10) + ' ms';
    return (Math.round(value / 10) / 100) + ' s';
  }

  function preText(text, empty) {
    var node = el('pre', 'value');
    node.textContent = text;
    if (empty) node.classList.add('is-empty');
    return node;
  }

  function factRow(dl, label, content) {
    var dt = el('dt', null, label);
    var dd = el('dd');
    if (content instanceof Node) dd.append(content);
    else dd.textContent = content === undefined || content === null ? '—' : String(content);
    dl.append(dt, dd);
  }

  function factJson(dl, label, value) {
    var text = jsonFactText(value);
    factRow(dl, label, preText(text, text === '—'));
  }

  function clearAndAppend(container, nodes) {
    container.replaceChildren();
    for (var i = 0; i < nodes.length; i += 1) container.append(nodes[i]);
  }

  // ---------- data helpers ----------

  function checkDocument(value) {
    return Boolean(value) && typeof value === 'object' && !Array.isArray(value) &&
      value.schema === SCHEMA && value.projects && typeof value.projects === 'object' &&
      !Array.isArray(value.projects);
  }

  function currentProject() {
    if (!doc || !selectedProject) return null;
    return doc.projects[selectedProject] || null;
  }

  function sortedEvaluations(project) {
    return (Array.isArray(project && project.evaluations) ? project.evaluations.slice() : [])
      .sort(function (a, b) { return a.version - b.version; });
  }

  function sortedRevisions(project) {
    return (Array.isArray(project && project.revisions) ? project.revisions.slice() : [])
      .sort(function (a, b) { return a.version - b.version; });
  }

  function projectVersion(project) {
    var revisions = sortedRevisions(project);
    return revisions.length ? revisions[revisions.length - 1].version : 0;
  }

  function latestEvaluationVersion(project) {
    var evaluations = sortedEvaluations(project);
    return evaluations.length ? evaluations[evaluations.length - 1].version : null;
  }

  function currentEvaluation() {
    var project = currentProject();
    if (!project || selectedEvaluation === null || selectedEvaluation === undefined) return null;
    var evaluations = sortedEvaluations(project);
    for (var i = 0; i < evaluations.length; i += 1) {
      if (evaluations[i].version === selectedEvaluation) return evaluations[i];
    }
    return null;
  }

  function revisionFor(project, version) {
    var revisions = sortedRevisions(project);
    if (!revisions.length) return null;
    if (version !== null && version !== undefined) {
      for (var i = 0; i < revisions.length; i += 1) {
        if (revisions[i].version === version) return revisions[i];
      }
    }
    return revisions[revisions.length - 1];
  }

  function selectedRevision() {
    var project = currentProject();
    if (!project) return null;
    var evaluation = currentEvaluation();
    return revisionFor(project, evaluation ? evaluation.version : null);
  }

  function planForSelected() {
    var revision = selectedRevision();
    return revision && revision.plan ? revision.plan : null;
  }

  function stepMap(evaluation) {
    var map = new Map();
    var steps = evaluation && Array.isArray(evaluation.steps) ? evaluation.steps : [];
    for (var i = 0; i < steps.length; i += 1) {
      if (steps[i] && typeof steps[i].node_id === 'string') map.set(steps[i].node_id, steps[i]);
    }
    return map;
  }

  // Display-only status: a running node in the current observed execution is
  // "waiting" while a call recorded for that node has no outcome yet. Replay
  // and any non-running evaluation never derive waiting from newer calls.
  function displayStepStatus(project, evaluation, step) {
    if (!step) return null;
    if (step.status === 'running' &&
        evaluation && evaluation.mode === 'observed' && evaluation.status === 'running' &&
        project && Array.isArray(project.calls)) {
      for (var i = 0; i < project.calls.length; i += 1) {
        var call = project.calls[i];
        if (call && call.node_id === step.node_id &&
            (call.outcome === null || call.outcome === undefined)) {
          return 'waiting';
        }
      }
    }
    return step.status;
  }

  function findEvidence(project, cursor) {
    if (!doc || !Array.isArray(doc.events) || cursor === null || cursor === undefined) return null;
    if (evidenceIndexDoc !== doc) {
      evidenceIndexDoc = doc;
      evidenceIndex = new Map();
      doc.events.forEach(function (record) {
        if (record && record.session_id !== undefined && record.cursor !== undefined) {
          evidenceIndex.set(record.session_id + '\u001f' + record.cursor, record);
        }
      });
    }
    var session = project ? project.session_id : null;
    if (session === null || session === undefined) return null;
    return evidenceIndex.get(session + '\u001f' + cursor) || null;
  }

  function evidenceDetails(project, cursor) {
    var details = el('details', 'evidence-details');
    if (cursor === null || cursor === undefined) {
      details.append(el('summary', null, '无证据游标'));
      return details;
    }
    var record = findEvidence(project, cursor);
    details.append(el('summary', null, '游标 ' + cursor + (record ? ' · 查看 Document.events 原始 JSON' : ' · Document.events 中没有对应记录')));
    if (record) {
      details.append(preText(jsonText(record), false));
    } else {
      details.append(el('p', 'small muted',
        'Document.events 中没有 session_id=' + (project ? project.session_id : '—') +
        '、cursor=' + cursor + ' 的原始记录；页面不会补造证据。'));
    }
    var evidenceKey = (project ? project.session_id : '') + '\u001f' + cursor;
    details.open = openEvidence.has(evidenceKey);
    details.addEventListener('toggle', function () {
      if (!details.isConnected) return;
      if (details.open) openEvidence.add(evidenceKey);
      else openEvidence.delete(evidenceKey);
    });
    return details;
  }

  function actualTransitions(evaluation) {
    var steps = evaluation && Array.isArray(evaluation.steps) ? evaluation.steps : [];
    var pairs = [];
    for (var i = 0; i + 1 < steps.length; i += 1) {
      var from = steps[i];
      var to = steps[i + 1];
      if (!from || !to || typeof from.node_id !== 'string' || typeof to.node_id !== 'string') continue;
      pairs.push({
        from: from.node_id,
        to: to.node_id,
        branch: from.branch === null || from.branch === undefined ? null : String(from.branch)
      });
    }
    return pairs;
  }

  function walkedEdge(pairs, edge) {
    for (var i = 0; i < pairs.length; i += 1) {
      var pair = pairs[i];
      if (pair.from !== edge.from || pair.to !== edge.to) continue;
      if (edge.branch === null || edge.branch === undefined) return true;
      if (pair.branch === edge.branch) return true;
    }
    return false;
  }

  function relatedGaps(project, evaluation, nodeId) {
    var all = [];
    if (evaluation && Array.isArray(evaluation.gaps)) {
      evaluation.gaps.forEach(function (gap) { all.push({ gap: gap, source: '本轮' }); });
    }
    if (project && Array.isArray(project.gaps)) {
      project.gaps.forEach(function (gap) { all.push({ gap: gap, source: '项目' }); });
    }
    var seen = new Set();
    return all.filter(function (entry) {
      var gap = entry.gap;
      if (gap.node_id && gap.node_id !== nodeId) return false;
      var key = entry.source + '\u001f' + (gap.node_id || '') + '\u001f' + (gap.message || '');
      if (seen.has(key)) return false;
      seen.add(key);
      return true;
    });
  }

  function normalizeSelection() {
    if (!doc) return;
    var ids = Object.keys(doc.projects);
    if (!ids.length) {
      selectedProject = null;
      selectedEvaluation = null;
      selectedNode = null;
      moduleFilter = null;
      compareVersion = null;
      undoTarget = null;
      return;
    }
    if (!selectedProject || !doc.projects[selectedProject]) selectedProject = ids[0];
    var project = doc.projects[selectedProject];
    var evaluations = sortedEvaluations(project);
    if (!evaluations.some(function (item) { return item.version === selectedEvaluation; })) {
      selectedEvaluation = evaluations.length ? evaluations[evaluations.length - 1].version : null;
    }
    var plan = planForSelected();
    if (selectedNode && (!plan || !plan.nodes.some(function (node) { return node.id === selectedNode; }))) {
      selectedNode = null;
      editorKey = null;
    }
    if (moduleFilter && (!plan || !plan.nodes.some(function (node) { return node.module === moduleFilter; }))) {
      moduleFilter = null;
    }
    if (compareVersion !== null && !evaluations.some(function (item) { return item.version === compareVersion; })) {
      compareVersion = null;
    }
    if (compareVersion === null && evaluations.length > 1) {
      var index = evaluations.findIndex(function (item) { return item.version === selectedEvaluation; });
      if (index > 0) compareVersion = evaluations[index - 1].version;
    }
    var revisions = sortedRevisions(project);
    if (!revisions.some(function (item) { return item.version === undoTarget; })) {
      undoTarget = revisions.length > 1 ? revisions[revisions.length - 2].version : (revisions.length ? revisions[0].version : null);
    }
  }

  // ---------- transport ----------

  async function api(path, options, timeoutMs) {
    var controller = new AbortController();
    activeControllers.add(controller);
    var timer = setTimeout(function () { controller.abort(); }, timeoutMs || 15000);
    var opts = options || {};
    var headers = { Accept: 'application/json', Authorization: 'Bearer ' + token };
    if (opts.body !== undefined) headers['Content-Type'] = 'application/json';
    try {
      var response = await fetch(path, {
        method: opts.method || 'GET',
        headers: headers,
        body: opts.body,
        signal: controller.signal,
        cache: 'no-store',
        credentials: 'omit'
      });
      var text = await response.text();
      var payload = null;
      if (text) {
        try { payload = JSON.parse(text); } catch (error) { payload = text; }
      }
      return { status: response.status, payload: payload };
    } finally {
      clearTimeout(timer);
      activeControllers.delete(controller);
    }
  }

  function extractError(payload) {
    if (!payload) return '';
    if (typeof payload === 'string') return clip(payload, 1200);
    if (typeof payload.error === 'string') return clip(payload.error, 1200);
    if (payload.error && typeof payload.error === 'object' && typeof payload.error.message === 'string') {
      return clip(payload.error.message, 1200);
    }
    if (typeof payload.message === 'string') return clip(payload.message, 1200);
    try { return clip(JSON.stringify(payload), 1200); } catch (error) { return ''; }
  }

  function setConnection(kind, text) {
    var node = ui['connection-status'];
    node.textContent = text;
    node.classList.remove('is-connected', 'is-error');
    if (kind === 'ok') node.classList.add('is-connected');
    if (kind === 'error') node.classList.add('is-error');
  }

  function reportGlobal(message) {
    var node = ui['global-error'];
    if (!message) {
      node.textContent = '';
      node.hidden = true;
      return;
    }
    node.textContent = message;
    node.hidden = false;
  }

  // ---------- login and polling ----------

  async function login(event) {
    if (event) event.preventDefault();
    if (loginPending) return;
    var candidate = ui['web-token'].value.trim();
    if (!candidate) {
      showLoginError('请输入网页访问令牌。');
      ui['web-token'].focus();
      return;
    }
    loginPending = true;
    ui['login-button'].disabled = true;
    ui['login-button'].textContent = '连接中…';
    ui['login-error'].hidden = true;
    token = candidate;
    try {
      var result = await api('/api/state', {}, 12000);
      if (result.status === 401) {
        token = '';
        showLoginError('网页访问令牌无效或已失效。');
        return;
      }
      if (result.status !== 200) {
        token = '';
        showLoginError('连接失败（HTTP ' + result.status + '）。请确认本机调试服务正在运行。');
        return;
      }
      if (!checkDocument(result.payload)) {
        token = '';
        showLoginError('服务端返回了不兼容的数据格式，页面不会继续。');
        return;
      }
      connected = true;
      doc = null;
      editorKey = null;
      reportGlobal(null);
      ui['web-token'].value = '';
      ui['login-panel'].hidden = true;
      ui['workspace'].hidden = false;
      applyDocument(result.payload, { force: true });
      setConnection('ok', '已连接 · 文档序列 ' + (doc ? doc.sequence : 0));
      startPolling();
    } catch (error) {
      token = '';
      showLoginError(error && error.name === 'AbortError'
        ? '连接超时，未建立会话。页面没有自动重试。'
        : '无法连接本机调试服务。请确认地址和令牌后重试。');
    } finally {
      loginPending = false;
      ui['login-button'].disabled = false;
      ui['login-button'].textContent = '连接';
    }
  }

  function showLoginError(message) {
    ui['login-error'].textContent = message;
    ui['login-error'].hidden = false;
  }

  function disconnect(message) {
    connected = false;
    pollGeneration += 1;
    clearTimeout(pollTimer);
    activeControllers.forEach(function (controller) { controller.abort(); });
    activeControllers.clear();
    token = '';
    doc = null;
    editorKey = null;
    pendingSave = false;
    pendingUndo = false;
    ui['workspace'].hidden = true;
    ui['login-panel'].hidden = false;
    ui['web-token'].value = '';
    ui['login-error'].textContent = message || '';
    ui['login-error'].hidden = !message;
    setConnection('error', '未连接');
    ui['web-token'].focus();
  }

  function startPolling() {
    var generation = ++pollGeneration;
    failures = 0;
    pollBusy = false;
    pollTick = async function () {
      if (!connected || generation !== pollGeneration) return;
      if (pollBusy) return;
      pollBusy = true;
      var ok = false;
      try {
        var after = doc && typeof doc.sequence === 'number' ? doc.sequence : 0;
        var result = await api('/api/state?after=' + after, {}, 30000);
        if (!connected || generation !== pollGeneration) return;
        if (result.status === 401) {
          disconnect('网页访问令牌已失效，请重新连接。');
          return;
        }
        if (result.status !== 200) throw new Error('HTTP ' + result.status);
        if (!checkDocument(result.payload)) throw new Error('服务端返回的数据格式不受支持。');
        applyDocument(result.payload);
        failures = 0;
        ok = true;
        setConnection('ok', '已连接 · 文档序列 ' + (doc ? doc.sequence : 0));
      } catch (error) {
        if (!connected || generation !== pollGeneration) return;
        // A long-poll that hits our own timeout is not a disconnect. Retry
        // quietly; a real network failure still shows the retry banner.
        if (!(error && error.name === 'AbortError')) {
          failures += 1;
          setConnection('error', '连接中断，正在重试（第 ' + failures + ' 次）');
        }
      } finally {
        pollBusy = false;
      }
      if (!connected || generation !== pollGeneration) return;
      var delay = ok ? 0 : (failures ? Math.min(8000, 1000 * Math.pow(2, Math.min(failures - 1, 3))) : 250);
      pollTimer = setTimeout(pollTick, delay);
    };
    pollTick();
  }

  function refreshNow() {
    if (!connected) return;
    clearTimeout(pollTimer);
    if (!pollBusy && pollTick) pollTick();
  }

  function applyDocument(next, options) {
    if (!checkDocument(next)) {
      reportGlobal('服务端返回了不兼容的数据格式，页面不会更新。');
      return false;
    }
    if (doc && typeof next.sequence === 'number' && typeof doc.sequence === 'number' && next.sequence < doc.sequence) {
      return false;
    }
    if (!options || !options.force) {
      if (doc && next.sequence === doc.sequence) return true;
    }
    // Promote a newly captured graph over the bootstrap call record. Existing
    // graph/history selections remain stable while later tasks arrive.
    var previousProject = doc && selectedProject ? doc.projects[selectedProject] : null;
    if (previousProject && !previousProject.revisions.length) {
      var arriving = Object.keys(next.projects).find(function (id) {
        return next.projects[id].revisions.length && (!doc.projects[id] || !doc.projects[id].revisions.length);
      });
      if (arriving) {
        selectedProject = arriving;
        selectedEvaluation = null;
        selectedNode = null;
        moduleFilter = null;
        editorKey = null;
      }
    }
    doc = next;
    reportGlobal(null);
    normalizeSelection();
    renderAll();
    return true;
  }

  // ---------- selection and selectors ----------

  function fillSelect(select, items, value) {
    var signature = items.map(function (item) { return String(item.value); }).join('\u001f');
    if (select.dataset.signature !== signature) {
      var fragment = document.createDocumentFragment();
      for (var i = 0; i < items.length; i += 1) {
        var option = document.createElement('option');
        option.value = String(items[i].value);
        option.textContent = items[i].label;
        fragment.append(option);
      }
      select.replaceChildren(fragment);
      select.dataset.signature = signature;
    } else {
      for (var j = 0; j < items.length; j += 1) {
        if (select.options[j] && select.options[j].textContent !== items[j].label) {
          select.options[j].textContent = items[j].label;
        }
      }
    }
    var target = value === null || value === undefined ? '' : String(value);
    if (target !== '' && select.value !== target) select.value = target;
  }

  function projectLabel(project) {
    var prefix = project.task_id ? project.task_id : project.id;
    return clip(prefix + ' · ' + project.id, 90);
  }

  function renderSelectors() {
    var project = currentProject();
    var evaluations = sortedEvaluations(project);
    var revisions = sortedRevisions(project);

    if (!doc || !Object.keys(doc.projects).length) {
      fillSelect(ui['project-select'], [{ value: '', label: '暂无调用项目' }], '');
      fillSelect(ui['evaluation-select'], [{ value: '', label: '暂无结果轮次' }], '');
      fillSelect(ui['compare-select'], [{ value: '', label: '暂无对比基准' }], '');
      fillSelect(ui['undo-version'], [{ value: '', label: '暂无版本' }], '');
      ui['project-select'].disabled = true;
      ui['evaluation-select'].disabled = true;
      ui['compare-select'].disabled = true;
      ui['undo-version'].disabled = true;
      return;
    }

    var projectItems = Object.keys(doc.projects).map(function (id) {
      return { value: id, label: projectLabel(doc.projects[id]) };
    });
    fillSelect(ui['project-select'], projectItems, selectedProject);
    ui['project-select'].disabled = false;

    var evaluationItems = evaluations.map(function (item) {
      return {
        value: item.version,
        label: 'v' + item.version + ' · ' + modeLabel(item.mode) + ' · ' + statusLabel(item.status) + ' · ' + formatClock(item.saved_at_ms)
      };
    });
    fillSelect(ui['evaluation-select'], evaluationItems.length ? evaluationItems : [{ value: '', label: '暂无结果轮次' }], selectedEvaluation);
    ui['evaluation-select'].disabled = !evaluationItems.length;

    var compareItems = evaluations.map(function (item) {
      return {
        value: item.version,
        label: 'v' + item.version + ' · ' + modeLabel(item.mode) + ' · ' + statusLabel(item.status)
      };
    });
    fillSelect(ui['compare-select'], compareItems.length ? compareItems : [{ value: '', label: '暂无对比基准' }], compareVersion);
    ui['compare-select'].disabled = compareItems.length < 2;

    var undoItems = revisions.slice().reverse().map(function (item) {
      var label = 'v' + item.version + ' · ' + (item.reason || '修改');
      if (item.changed_node) label += ' · ' + item.changed_node;
      return { value: item.version, label: clip(label, 110) };
    });
    fillSelect(ui['undo-version'], undoItems.length ? undoItems : [{ value: '', label: '暂无版本' }], undoTarget);
    ui['undo-version'].disabled = !undoItems.length;
    updateActionButtons();
  }

  function updateActionButtons() {
    var project = currentProject();
    var revision = selectedRevision();
    var node = revision && revision.plan && selectedNode
      ? revision.plan.nodes.find(function (item) { return item.id === selectedNode; })
      : null;
    var currentVersion = project ? projectVersion(project) : 0;
    var key = draftKey();
    var draft = key ? drafts.get(key) : null;
    ui['save-node'].disabled = !node || pendingSave || pendingUndo;
    ui['save-node'].textContent = pendingSave ? '正在保存并求值…' : '保存并重算';
    ui['discard-draft'].hidden = !draft;
    ui['discard-draft'].disabled = pendingSave || pendingUndo;
    ui['undo-node'].disabled = !project || pendingUndo || pendingSave ||
      undoTarget === null || undoTarget === undefined || undoTarget === currentVersion;
    ui['undo-node'].textContent = pendingUndo ? '正在撤销…' : '撤销到选中版本';
  }

  function onProjectChange() {
    var value = ui['project-select'].value;
    if (!value) return;
    selectedProject = value;
    selectedEvaluation = null;
    selectedNode = null;
    moduleFilter = null;
    compareVersion = null;
    undoTarget = null;
    editorKey = null;
    editorNotice = null;
    undoNotice = null;
    normalizeSelection();
    renderAll();
  }

  function onEvaluationChange() {
    var value = Number(ui['evaluation-select'].value);
    if (!isFinite(value)) return;
    selectedEvaluation = value;
    compareVersion = null;
    editorNotice = null;
    normalizeSelection();
    renderAll();
  }

  function onCompareChange() {
    var value = Number(ui['compare-select'].value);
    if (!isFinite(value)) return;
    compareVersion = value;
    renderComparison();
  }

  function onUndoTargetChange() {
    var value = Number(ui['undo-version'].value);
    if (!isFinite(value)) return;
    undoTarget = value;
    updateActionButtons();
  }

  function showAllModules() {
    moduleFilter = null;
    renderGraphs();
    renderNodeDetail();
  }

  // ---------- rendering ----------

  function renderAll() {
    renderSelectors();
    renderControls();
    renderSummary();
    renderErrorFacts();
    renderTimeline();
    renderGraphs();
    renderNodeDetail();
    renderHistory();
    renderComparison();
    renderGaps();
  }

  function renderControls() {
    var evaluation = currentEvaluation();
    var modeNode = ui['evaluation-mode'];
    modeNode.className = 'badge ' + (evaluation && evaluation.mode === 'observed' ? 'mode-observed' : evaluation && evaluation.mode === 'replay' ? 'mode-replay' : evaluation && evaluation.mode === 'recompute' ? 'mode-recompute' : 'badge-none');
    modeNode.textContent = evaluation ? modeLabel(evaluation.mode) : '—';
    modeNode.title = evaluation && MODE_DETAILS[evaluation.mode] ? MODE_DETAILS[evaluation.mode] : '';

    var statusNode = ui['evaluation-status'];
    statusNode.className = 'badge ' + (evaluation ? statusClass(evaluation.status) : 'badge-none');
    statusNode.textContent = evaluation ? statusLabel(evaluation.status) : '—';
    statusNode.title = '';

    ui['document-sequence'].textContent = doc ? '文档序列 ' + doc.sequence : '';
  }

  function renderSummary() {
    var dl = ui['project-summary'];
    dl.replaceChildren();
    var project = currentProject();
    if (!doc || !Object.keys(doc.projects).length) {
      factRow(dl, '状态', '暂无调用记录。等待 Runtime 会话产生 trace 数据。');
    } else if (!project) {
      factRow(dl, '状态', '没有可显示的项目。');
    } else {
      factRow(dl, '项目', project.id);
      factRow(dl, '会话', project.session_id || '—');
      factRow(dl, '尝试', project.attempt_id || '—');
      factRow(dl, '任务', project.task_id || '—');
      factRow(dl, '源码摘要', project.source_hash || '—');
      factRow(dl, '运行状态', statusBadge(project.runtime_status));
      factRow(dl, '活动节点', project.active_node || '—');
      factRow(dl, '版本与结果', sortedRevisions(project).length + ' 个版本 · ' + sortedEvaluations(project).length + ' 轮结果');
      factRow(dl, '已记录调用', (Array.isArray(project.calls) ? project.calls.length : 0) + ' 条');
      factRow(dl, '文档序列', doc.sequence);
    }

    var captures = ui['session-captures'];
    var sessions = doc && doc.sessions ? Object.keys(doc.sessions).sort() : [];
    if (!sessions.length) {
      captures.replaceChildren(el('p', 'small muted', '暂无会话采集记录。'));
      return;
    }
    var chips = sessions.map(function (id) {
      var capture = doc.sessions[id] || {};
      var isMine = project && project.session_id === id;
      var chip = el('span', 'session-chip' + (capture.error ? ' is-error' : ''));
      chip.textContent = (isMine ? '本项目 · ' : '') + '会话 ' + clip(id, 30) + ' · 游标 ' + (capture.after === undefined ? '—' : capture.after);
      if (capture.error) chip.textContent += ' · 错误：' + clip(capture.error, 400);
      return chip;
    });
    clearAndAppend(captures, chips);
  }

  function stepErrors(evaluation) {
    var items = [];
    var steps = evaluation && Array.isArray(evaluation.steps) ? evaluation.steps : [];
    for (var i = 0; i < steps.length; i += 1) {
      var step = steps[i];
      if (!step) continue;
      if (step.error || step.status === 'failed' || step.status === 'blocked' || step.status === 'outcome_unknown') {
        items.push({
          node: step.node_id,
          status: step.status,
          message: step.error || statusLabel(step.status),
          evidence: step.evidence_cursor,
          elapsed: step.elapsed_ms,
          input: step.input,
          state: step.state_before,
          branch: step.branch,
          step: step
        });
      }
    }
    return items;
  }

  function renderErrorFacts() {
    var container = ui['error-facts-list'];
    var project = currentProject();
    var evaluation = currentEvaluation();
    var cards = [];
    var plan = planForSelected();
    var nodeLabels = new Map();
    if (plan) plan.nodes.forEach(function (node) { nodeLabels.set(node.id, node.label || node.id); });

    var stepItems = stepErrors(evaluation);
    stepItems.forEach(function (item) {
      var card = el('article', 'fact-item is-failed');
      card.append(el('h3', null, '节点 ' + item.node + (nodeLabels.has(item.node) ? ' · ' + nodeLabels.get(item.node) : '')));
      card.append(el('p', 'fact-meta', '状态：' + statusLabel(item.status) + ' · 耗时：' + formatMs(item.elapsed) + ' · 证据游标：' + (item.evidence === null || item.evidence === undefined ? '无' : item.evidence)));
      card.append(el('p', 'fact-error', clip(String(item.message).split('\n')[0], 300)));
      card.append(el('p', 'fact-meta', '关键输入：' + summaryJson(item.input, 520)));
      card.append(el('p', 'fact-meta', '前置状态：' + summaryJson(item.state, 420)));
      if (item.branch !== null && item.branch !== undefined) {
        card.append(el('p', 'fact-meta', '分支：' + item.branch));
      }
      card.append(el('p', 'fact-suggestion', '建议检查：本节点的 source、input_pointer 和 params。这里只列出需要核对的位置，不推断根因。'));
      var raw = el('details', 'raw-details');
      raw.append(el('summary', null, '查看原始错误与完整步骤 JSON'));
      raw.append(preText(item.message, false));
      raw.append(preText(jsonText(item.step), false));
      card.append(raw);
      if (item.evidence !== null && item.evidence !== undefined) {
        card.append(evidenceDetails(project, item.evidence));
      }
      if (plan && plan.nodes.some(function (node) { return node.id === item.node; })) {
        var button = el('button', 'secondary', '查看节点');
        button.type = 'button';
        button.addEventListener('click', function () { selectNode(item.node, true); });
        card.append(button);
      }
      cards.push(card);
    });

    var gaps = (evaluation && Array.isArray(evaluation.gaps) ? evaluation.gaps : []).slice();
    if (project && Array.isArray(project.gaps)) gaps = gaps.concat(project.gaps);
    gaps.forEach(function (gap) {
      var card = el('article', 'fact-item is-gap');
      card.append(el('h3', null, '缺口' + (gap.node_id ? ' · ' + gap.node_id : '')));
      card.append(el('p', 'fact-meta', '无法自动补全的事实，需要人工核对。'));
      card.append(preText(gap.message || '—', false));
      cards.push(card);
    });

    if (evaluation && !stepItems.length && !gaps.length &&
        (evaluation.status === 'failed' || evaluation.status === 'blocked' || evaluation.status === 'outcome_unknown')) {
      var statusCard = el('article', 'fact-item ' + (evaluation.status === 'blocked' ? 'is-blocked' : 'is-failed'));
      statusCard.append(el('h3', null, '本轮状态 · ' + statusLabel(evaluation.status)));
      statusCard.append(el('p', 'fact-meta', modeLabel(evaluation.mode) + ' · 耗时 ' + formatMs(evaluation.elapsed_ms)));
      statusCard.append(preText('本轮被标记为“' + statusLabel(evaluation.status) + '”，但没有记录步骤错误或缺口。', false));
      cards.push(statusCard);
    }

    var sessions = doc && doc.sessions ? Object.keys(doc.sessions).sort() : [];
    sessions.forEach(function (id) {
      var capture = doc.sessions[id];
      if (!capture || !capture.error) return;
      var card = el('article', 'fact-item is-session');
      card.append(el('h3', null, '会话采集错误 · ' + clip(id, 40)));
      card.append(preText(capture.error, false));
      cards.push(card);
    });

    if (!cards.length) {
      container.replaceChildren(el('p', 'muted', evaluation ? '本轮没有记录错误事实。' : '没有可选的结果轮次。'));
      return;
    }
    clearAndAppend(container, cards);
  }

  // ---------- execution timeline ----------

  function timelineSource(step, evaluation) {
    if (step && step.reused) {
      return { label: '复用', kind: 'badge-none', detail: '该步骤复用上一轮已捕获的结果，没有重新计算。' };
    }
    if (evaluation && evaluation.mode === 'observed') {
      return { label: '已捕获', kind: 'mode-observed', detail: '该步骤来自原始 Runtime 执行记录。' };
    }
    if (evaluation && evaluation.mode === 'recompute') {
      return { label: '局部重算', kind: 'mode-recompute', detail: '该步骤由局部重算得到，只使用捕获到的 Host 响应。' };
    }
    if (evaluation && evaluation.mode === 'replay') {
      return { label: '本地重放', kind: 'mode-replay', detail: '该步骤由入口整图本地重放计算，只使用捕获到的 Host 响应。' };
    }
    return { label: '方式未知', kind: 'badge-none', detail: '这一轮没有标注步骤来源。' };
  }

  function renderTimeline() {
    var container = ui['timeline-list'];
    var project = currentProject();
    var plan = planForSelected();
    var evaluation = currentEvaluation();
    var focusedNode = null;
    var active = document.activeElement;
    if (active && active.closest) {
      var focused = active.closest('.timeline-button');
      if (focused) focusedNode = focused.dataset.nodeId;
    }
    container.replaceChildren();
    if (!evaluation) {
      ui['timeline-meta'].textContent = '无结果轮次';
      container.append(el('li', 'timeline-empty', '没有可选的结果轮次。'));
      return;
    }
    var steps = Array.isArray(evaluation.steps) ? evaluation.steps : [];
    ui['timeline-meta'].textContent = '第 v' + evaluation.version + ' 轮 · ' +
      modeLabel(evaluation.mode) + ' · ' + steps.length + ' 步';
    if (!steps.length) {
      container.append(el('li', 'timeline-empty', '本轮没有步骤记录（可能已阻断）；请查看下方缺口。'));
      return;
    }
    var planIds = new Set();
    var nodeLabels = new Map();
    if (plan && Array.isArray(plan.nodes)) {
      plan.nodes.forEach(function (node) {
        planIds.add(node.id);
        nodeLabels.set(node.id, node.label || node.id);
      });
    }

    steps.forEach(function (step, index) {
      var item = el('li', 'timeline-item');
      var inPlan = planIds.has(step.node_id);
      var selected = selectedNode === step.node_id;
      var status = displayStepStatus(project, evaluation, step);
      var source = timelineSource(step, evaluation);
      var cursorText = step.evidence_cursor === null || step.evidence_cursor === undefined
        ? '无原始证据游标'
        : '原始证据游标 ' + step.evidence_cursor;
      var button = el('button', 'timeline-button' + (selected ? ' is-selected' : ''));
      button.type = 'button';
      button.dataset.nodeId = step.node_id;
      button.disabled = !inPlan;
      button.setAttribute('aria-pressed', selected ? 'true' : 'false');
      var spoken = ['第 ' + (index + 1) + ' 步', step.node_id, statusLabel(status),
        formatMs(step.elapsed_ms), source.label, cursorText];
      if (step.branch !== null && step.branch !== undefined) spoken.push('分支 ' + step.branch);
      if (step.error) spoken.push(String(step.error).split('\n')[0]);
      if (!inPlan) spoken.push('不在当前计划中');
      button.setAttribute('aria-label', spoken.join('，'));
      button.title = source.detail + (step.error ? '\n错误：' + step.error : '');

      button.append(el('span', 'timeline-index', String(index + 1).padStart(2, '0')));
      var main = el('span', 'timeline-main');
      var nodeLine = el('span', 'timeline-node');
      nodeLine.append(el('code', null, step.node_id));
      var label = nodeLabels.get(step.node_id);
      if (label && label !== step.node_id) nodeLine.append(el('span', null, label));
      var meta = el('span', 'timeline-meta');
      meta.append(statusBadge(status));
      meta.append(el('span', null, '耗时 ' + formatMs(step.elapsed_ms)));
      var sourceBadge = badge(source.label, source.kind);
      sourceBadge.title = source.detail;
      meta.append(sourceBadge);
      meta.append(el('span', 'timeline-cursor', cursorText));
      if (step.branch !== null && step.branch !== undefined) {
        meta.append(el('span', null, '分支 ' + clip(String(step.branch), 40)));
      }
      main.append(nodeLine, meta);
      button.append(main, el('span', 'timeline-arrow', '查看'));
      button.addEventListener('click', function () {
        if (!inPlan) return;
        selectNode(step.node_id, true);
      });
      button.addEventListener('keydown', onTimelineKeydown);
      item.append(button);
      container.append(item);
    });

    if (focusedNode) {
      var buttons = container.querySelectorAll('.timeline-button');
      for (var i = 0; i < buttons.length; i += 1) {
        if (buttons[i].dataset.nodeId === focusedNode && !buttons[i].disabled) {
          try { buttons[i].focus({ preventScroll: true }); } catch (error) { buttons[i].focus(); }
          break;
        }
      }
    }
  }

  function onTimelineKeydown(event) {
    var key = event.key;
    if (key !== 'ArrowDown' && key !== 'ArrowUp' && key !== 'ArrowRight' && key !== 'ArrowLeft' &&
        key !== 'Home' && key !== 'End') return;
    var buttons = Array.prototype.slice.call(
      ui['timeline-list'].querySelectorAll('.timeline-button:not(:disabled)')
    );
    if (!buttons.length) return;
    var index = buttons.indexOf(event.currentTarget);
    if (index < 0) return;
    var next = index;
    if (key === 'Home') next = 0;
    else if (key === 'End') next = buttons.length - 1;
    else if (key === 'ArrowDown' || key === 'ArrowRight') next = Math.min(buttons.length - 1, index + 1);
    else next = Math.max(0, index - 1);
    if (next === index) return;
    event.preventDefault();
    buttons[next].focus();
  }

  // ---------- graphs ----------

  function makeGraph(width, height, label) {
    var svg = svgEl('svg', {
      viewBox: '0 0 ' + width + ' ' + height,
      width: width,
      height: height,
      role: 'group',
      'aria-label': label
    });
    return svg;
  }

  function addArrow(svg, id, className) {
    var defs = svgEl('defs');
    var marker = svgEl('marker', {
      id: id,
      markerWidth: 8,
      markerHeight: 8,
      refX: 7,
      refY: 4,
      orient: 'auto'
    });
    marker.append(svgEl('path', { d: 'M0,0 L8,4 L0,8 z', class: className || 'g-arrow' }));
    defs.append(marker);
    svg.append(defs);
  }

  function layoutNodes(nodes, edges) {
    var NODE_W = 178;
    var NODE_H = 64;
    var GAP_X = 54;
    var GAP_Y = 30;
    var PAD = 20;
    var index = new Map();
    nodes.forEach(function (node, i) { index.set(node.id, i); });
    var layers = new Map();
    var indegree = new Map();
    var successors = new Map();
    nodes.forEach(function (node) {
      layers.set(node.id, 0);
      indegree.set(node.id, 0);
      successors.set(node.id, []);
    });
    (edges || []).forEach(function (edge) {
      if (!index.has(edge.from) || !index.has(edge.to) || edge.from === edge.to) return;
      successors.get(edge.from).push(edge.to);
      indegree.set(edge.to, indegree.get(edge.to) + 1);
    });
    var queue = nodes.filter(function (node) { return indegree.get(node.id) === 0; })
      .map(function (node) { return node.id; });
    queue.sort(function (a, b) { return index.get(a) - index.get(b); });
    var visited = new Set();
    while (queue.length) {
      var id = queue.shift();
      if (visited.has(id)) continue;
      visited.add(id);
      var nexts = successors.get(id) || [];
      for (var i = 0; i < nexts.length; i += 1) {
        var to = nexts[i];
        layers.set(to, Math.max(layers.get(to) || 0, (layers.get(id) || 0) + 1));
        indegree.set(to, indegree.get(to) - 1);
        if (indegree.get(to) === 0) queue.push(to);
      }
    }
    var maxLayer = 0;
    layers.forEach(function (value) { if (value > maxLayer) maxLayer = value; });
    var byLayer = new Map();
    nodes.forEach(function (node) {
      var layer = visited.has(node.id) ? layers.get(node.id) : 0;
      if (!byLayer.has(layer)) byLayer.set(layer, []);
      byLayer.get(layer).push(node);
    });
    var maxCount = 1;
    byLayer.forEach(function (list) {
      list.sort(function (a, b) { return index.get(a.id) - index.get(b.id); });
      if (list.length > maxCount) maxCount = list.length;
    });
    var cell = NODE_W + GAP_X;
    var maxRowWidth = maxCount * NODE_W + Math.max(0, maxCount - 1) * GAP_X;
    var positions = new Map();
    for (var layer = 0; layer <= maxLayer; layer += 1) {
      var list = byLayer.get(layer) || [];
      var rowWidth = list.length * NODE_W + Math.max(0, list.length - 1) * GAP_X;
      var startX = PAD + Math.max(0, (maxRowWidth - rowWidth) / 2);
      for (var k = 0; k < list.length; k += 1) {
        positions.set(list[k].id, {
          x: startX + k * cell,
          y: PAD + layer * (NODE_H + GAP_Y),
          layer: layer
        });
      }
    }
    return {
      positions: positions,
      width: Math.max(320, PAD * 2 + maxRowWidth),
      height: Math.max(120, PAD * 2 + (maxLayer + 1) * NODE_H + maxLayer * GAP_Y),
      nodeWidth: NODE_W,
      nodeHeight: NODE_H,
      pad: PAD
    };
  }

  function edgePath(from, to, layout, edgeKind) {
    var width = layout.nodeWidth;
    var height = layout.nodeHeight;
    var x1 = from.x + width / 2;
    var y1 = from.y + height;
    var x2 = to.x + width / 2;
    var y2 = to.y;
    if (edgeKind === 'same-layer') {
      y1 = from.y + height / 2;
      y2 = to.y + height / 2;
      x1 = from.x + width;
      x2 = to.x;
      var mid = (x1 + x2) / 2;
      return 'M ' + x1 + ' ' + y1 + ' C ' + mid + ' ' + y1 + ', ' + mid + ' ' + y2 + ', ' + x2 + ' ' + y2;
    }
    var midY = (y1 + y2) / 2;
    return 'M ' + x1 + ' ' + y1 + ' C ' + x1 + ' ' + midY + ', ' + x2 + ' ' + midY + ', ' + x2 + ' ' + y2;
  }

  function renderNodeGraph(plan, evaluation, project) {
    var container = ui['node-graph'];
    var focusedId = null;
    var active = document.activeElement;
    if (active && active.closest) {
      var focused = active.closest('.g-node');
      if (focused) focusedId = focused.dataset.nodeId;
    }
    nodeElements = new Map();
    nodeCenters = new Map();
    container.replaceChildren();
    if (!plan || !Array.isArray(plan.nodes) || !plan.nodes.length) {
      container.append(el('p', 'graph-empty', '没有可显示的节点。'));
      return;
    }
    var steps = stepMap(evaluation);
    var transitions = actualTransitions(evaluation);
    var nodes = plan.nodes;
    var edges = Array.isArray(plan.edges) ? plan.edges : [];
    var layout = layoutNodes(nodes, edges);
    var svg = makeGraph(layout.width, layout.height, '节点图，共 ' + nodes.length + ' 个节点');
    addArrow(svg, 'trace-arrow-nodes');
    addArrow(svg, 'trace-arrow-nodes-active', 'g-arrow-active');
    var edgeLayer = svgEl('g');
    var nodeLayer = svgEl('g');
    svg.append(edgeLayer, nodeLayer);

    var visible = new Set();
    nodes.forEach(function (node) {
      if (!moduleFilter || node.module === moduleFilter) visible.add(node.id);
    });

    edges.forEach(function (edge) {
      var from = layout.positions.get(edge.from);
      var to = layout.positions.get(edge.to);
      if (!from || !to) return;
      var hidden = !visible.has(edge.from) || !visible.has(edge.to);
      var sameLayer = from.layer === to.layer;
      var walked = walkedEdge(transitions, edge);
      var focusedEdge = !hidden && (selectedNode === edge.from || selectedNode === edge.to);
      var edgeClass = walked ? ' is-active' : focusedEdge ? ' is-focused' : ' is-planned';
      var path = svgEl('path', {
        d: edgePath(from, to, layout, sameLayer ? 'same-layer' : ''),
        class: 'g-edge' + edgeClass + (hidden ? ' is-hidden' : ''),
        'marker-end': walked ? 'url(#trace-arrow-nodes-active)' : 'url(#trace-arrow-nodes)'
      });
      if (hidden) path.setAttribute('display', 'none');
      edgeLayer.append(path);
      if (edge.branch !== null && edge.branch !== undefined && !hidden) {
        var label = svgEl('text', {
          x: (from.x + to.x) / 2 + layout.nodeWidth / 2,
          y: (from.y + to.y) / 2 + layout.nodeHeight / 2,
          class: 'g-edge-label' + (walked ? ' is-active-label' : focusedEdge ? ' is-focused-label' : ''),
          'text-anchor': 'middle'
        });
        label.textContent = clip(String(edge.branch), 20);
        edgeLayer.append(label);
      }
    });

    nodes.forEach(function (node) {
      var position = layout.positions.get(node.id);
      if (!position) return;
      var step = steps.get(node.id);
      var status = step ? displayStepStatus(project, evaluation, step) : null;
      var classes = ['g-node', statusClass(status)];
      if (selectedNode === node.id) classes.push('is-selected');
      if (step && step.reused) classes.push('is-reused');
      if (!visible.has(node.id)) classes.push('is-hidden');
      var group = svgEl('g', {
        class: classes.join(' '),
        tabindex: '0',
        role: 'button',
        'aria-pressed': selectedNode === node.id ? 'true' : 'false',
        'aria-label': '节点 ' + node.id + '，' + (node.label || '') + '，状态 ' + statusLabel(status) + '，模块 ' + (node.module || '未命名')
      });
      group.dataset.nodeId = node.id;
      group.append(svgEl('rect', {
        class: 'halo',
        x: position.x - 4,
        y: position.y - 4,
        width: layout.nodeWidth + 8,
        height: layout.nodeHeight + 8,
        rx: 11
      }));
      group.append(svgEl('rect', {
        class: 'box',
        x: position.x,
        y: position.y,
        width: layout.nodeWidth,
        height: layout.nodeHeight,
        rx: 8
      }));
      var title = svgEl('text', { class: 'g-title', x: position.x + 10, y: position.y + 20 });
      title.textContent = clip(node.id, 20);
      var subtitle = svgEl('text', { class: 'g-subtitle', x: position.x + 10, y: position.y + 38 });
      subtitle.textContent = clip(node.label || node.id, 22);
      var meta = svgEl('text', { class: 'g-meta', x: position.x + 10, y: position.y + 54 });
      meta.textContent = clip(statusLabel(status) + ' · ' + (node.module || '未命名模块'), 30);
      var fullTitle = svgEl('title');
      fullTitle.textContent = node.id + ' · ' + (node.label || '') + '\n模块：' + (node.module || '未命名') +
        '\n实现：' + (node.implementation || '—') +
        '\n状态：' + statusLabel(status) +
        (step && step.error ? '\n错误：' + step.error : '');
      group.append(title, subtitle, meta, fullTitle);
      group.addEventListener('click', function () { selectNode(node.id, false); });
      group.addEventListener('keydown', onNodeKeydown);
      nodeLayer.append(group);
      nodeElements.set(node.id, group);
      nodeCenters.set(node.id, {
        x: position.x + layout.nodeWidth / 2,
        y: position.y + layout.nodeHeight / 2,
        hidden: !visible.has(node.id)
      });
    });

    container.append(svg);
    if (focusedId && nodeElements.has(focusedId) && !nodeElements.get(focusedId).classList.contains('is-hidden')) {
      try { nodeElements.get(focusedId).focus({ preventScroll: true }); } catch (error) { /* keep going */ }
    }
  }

  function onNodeKeydown(event) {
    var id = event.currentTarget.dataset.nodeId;
    if (event.key === 'Enter' || event.key === ' ' || event.key === 'Spacebar') {
      event.preventDefault();
      selectNode(id, true);
      return;
    }
    var point = nodeCenters.get(id);
    if (!point) return;
    var dx = 0;
    var dy = 0;
    if (event.key === 'ArrowRight') dx = 1;
    else if (event.key === 'ArrowLeft') dx = -1;
    else if (event.key === 'ArrowDown') dy = 1;
    else if (event.key === 'ArrowUp') dy = -1;
    else return;
    event.preventDefault();
    var best = null;
    var bestScore = Infinity;
    nodeCenters.forEach(function (candidate, candidateId) {
      if (candidateId === id || candidate.hidden) return;
      var cdx = candidate.x - point.x;
      var cdy = candidate.y - point.y;
      var primary;
      var secondary;
      if (dx !== 0) {
        if (cdx * dx <= 4) return;
        primary = Math.abs(cdx);
        secondary = Math.abs(cdy);
      } else {
        if (cdy * dy <= 4) return;
        primary = Math.abs(cdy);
        secondary = Math.abs(cdx);
      }
      var score = primary + secondary * 0.45;
      if (score < bestScore) {
        bestScore = score;
        best = candidateId;
      }
    });
    if (best && nodeElements.get(best)) nodeElements.get(best).focus({ preventScroll: false });
  }

  function layoutModules(nodes) {
    var MODULE_W = 220;
    var MODULE_H = 78;
    var GAP_X = 160;
    var GAP_Y = 54;
    var PAD = 20;
    var order = [];
    var groups = new Map();
    nodes.forEach(function (node) {
      var name = node.module || '未命名模块';
      if (!groups.has(name)) {
        groups.set(name, []);
        order.push(name);
      }
      groups.get(name).push(node);
    });
    var columns = Math.min(3, order.length || 1);
    var rows = Math.ceil((order.length || 1) / columns);
    var positions = new Map();
    order.forEach(function (name, index) {
      var column = index % columns;
      var row = Math.floor(index / columns);
      positions.set(name, {
        x: PAD + column * (MODULE_W + GAP_X),
        y: PAD + row * (MODULE_H + GAP_Y)
      });
    });
    return {
      order: order,
      groups: groups,
      positions: positions,
      width: Math.max(320, PAD * 2 + columns * MODULE_W + (columns - 1) * GAP_X),
      height: Math.max(140, PAD * 2 + rows * MODULE_H + (rows - 1) * GAP_Y),
      moduleWidth: MODULE_W,
      moduleHeight: MODULE_H
    };
  }

  function moduleStatus(moduleNodes, steps, activeNode, project, evaluation) {
    var statuses = moduleNodes.map(function (node) {
      var step = steps.get(node.id);
      return step ? displayStepStatus(project, evaluation, step) : null;
    });
    var priorities = ['failed', 'outcome_unknown', 'blocked', 'waiting', 'running', 'completed'];
    for (var i = 0; i < priorities.length; i += 1) {
      if (statuses.indexOf(priorities[i]) >= 0) return priorities[i];
    }
    if (activeNode && moduleNodes.some(function (node) { return node.id === activeNode; })) return 'running';
    return null;
  }

  function renderModuleGraph(plan, evaluation, project) {
    var container = ui['module-graph'];
    var focusedModule = null;
    var active = document.activeElement;
    if (active && active.closest) {
      var focused = active.closest('.g-module');
      if (focused) focusedModule = focused.dataset.module;
    }
    moduleElements = new Map();
    container.replaceChildren();
    if (!plan || !Array.isArray(plan.nodes) || !plan.nodes.length) {
      container.append(el('p', 'graph-empty', '没有可显示的模块。'));
      return;
    }
    var steps = stepMap(evaluation);
    var layout = layoutModules(plan.nodes);
    var svg = makeGraph(layout.width, layout.height, '模块架构图，共 ' + layout.order.length + ' 个模块');
    addArrow(svg, 'trace-arrow-modules');
    addArrow(svg, 'trace-arrow-modules-active', 'g-arrow-active');
    var edgeLayer = svgEl('g');
    var moduleLayer = svgEl('g');
    svg.append(edgeLayer, moduleLayer);

    var transitions = actualTransitions(evaluation);
    var modulesById = new Map();
    plan.nodes.forEach(function (node) { modulesById.set(node.id, node.module || '未命名模块'); });
    var aggregate = new Map();
    function aggregateEdge(from, to) {
      if (from === to || !layout.positions.has(from) || !layout.positions.has(to)) return null;
      var key = from + '\u001f' + to;
      if (!aggregate.has(key)) aggregate.set(key, { from: from, to: to, planned: 0, actual: 0 });
      return aggregate.get(key);
    }
    (Array.isArray(plan.edges) ? plan.edges : []).forEach(function (edge) {
      var fromModule = modulesById.get(edge.from);
      var toModule = modulesById.get(edge.to);
      if (fromModule === undefined || toModule === undefined) return;
      var entry = aggregateEdge(fromModule, toModule);
      if (entry) entry.planned += 1;
    });
    transitions.forEach(function (pair) {
      var fromModule = modulesById.get(pair.from);
      var toModule = modulesById.get(pair.to);
      var entry = aggregateEdge(fromModule, toModule);
      if (entry) entry.actual += 1;
    });
    aggregate.forEach(function (edge) {
      var from = layout.positions.get(edge.from);
      var to = layout.positions.get(edge.to);
      var sameRow = from.y === to.y;
      var x1;
      var y1;
      var x2;
      var y2;
      var midX;
      var midY;
      var pathData;
      if (sameRow) {
        // Same-row edges leave the right edge and enter the left edge so the
        // label can sit in the widened horizontal gap instead of on a box.
        var rightward = to.x > from.x;
        x1 = rightward ? from.x + layout.moduleWidth : from.x;
        x2 = rightward ? to.x : to.x + layout.moduleWidth;
        y1 = y2 = from.y + layout.moduleHeight / 2;
        midX = (x1 + x2) / 2;
        pathData = 'M ' + x1 + ' ' + y1 + ' C ' + midX + ' ' + y1 + ', ' + midX + ' ' + y2 + ', ' + x2 + ' ' + y2;
      } else {
        x1 = from.x + layout.moduleWidth / 2;
        y1 = from.y + layout.moduleHeight;
        x2 = to.x + layout.moduleWidth / 2;
        y2 = to.y;
        midY = (y1 + y2) / 2;
        midX = (x1 + x2) / 2;
        pathData = 'M ' + x1 + ' ' + y1 + ' C ' + x1 + ' ' + midY + ', ' + x2 + ' ' + midY + ', ' + x2 + ' ' + y2;
      }
      var active = edge.actual > 0;
      var path = svgEl('path', {
        d: pathData,
        class: 'g-edge' + (active ? ' is-active' : ' is-planned'),
        'marker-end': active ? 'url(#trace-arrow-modules-active)' : 'url(#trace-arrow-modules)'
      });
      edgeLayer.append(path);
      var label = svgEl('text', {
        x: midX,
        y: sameRow ? y1 - 10 : midY,
        class: 'g-edge-label' + (active ? ' is-active-label' : ''),
        'text-anchor': 'middle'
      });
      label.textContent = active
        ? '本轮 ' + edge.actual + ' 次' + (edge.planned > 1 ? ' · 计划 ' + edge.planned + ' 条' : '')
        : '计划' + (edge.planned > 1 ? ' ' + edge.planned + ' 条' : '');
      edgeLayer.append(label);
    });

    layout.order.forEach(function (name) {
      var position = layout.positions.get(name);
      var moduleNodes = layout.groups.get(name);
      var status = moduleStatus(moduleNodes, steps, project ? project.active_node : null, project, evaluation);
      var executed = moduleNodes.filter(function (node) { return steps.has(node.id); }).length;
      var failed = moduleNodes.filter(function (node) {
        var step = steps.get(node.id);
        return step && (step.status === 'failed' || step.status === 'outcome_unknown');
      }).length;
      var classes = ['g-module', statusClass(status)];
      if (moduleFilter === name) classes.push('is-selected');
      var group = svgEl('g', {
        class: classes.join(' '),
        tabindex: '0',
        role: 'button',
        'aria-label': '模块 ' + name + '，' + moduleNodes.length + ' 个节点，状态 ' + statusLabel(status)
      });
      group.dataset.module = name;
      group.append(svgEl('rect', {
        class: 'halo',
        x: position.x - 4,
        y: position.y - 4,
        width: layout.moduleWidth + 8,
        height: layout.moduleHeight + 8,
        rx: 11
      }));
      group.append(svgEl('rect', {
        class: 'box',
        x: position.x,
        y: position.y,
        width: layout.moduleWidth,
        height: layout.moduleHeight,
        rx: 8
      }));
      var title = svgEl('text', { class: 'g-title', x: position.x + 12, y: position.y + 26 });
      title.textContent = clip(name, 26);
      var meta = svgEl('text', { class: 'g-meta', x: position.x + 12, y: position.y + 46 });
      meta.textContent = moduleNodes.length + ' 个节点 · ' + executed + ' 步已执行' + (failed ? ' · ' + failed + ' 个错误' : '');
      var meta2 = svgEl('text', { class: 'g-meta', x: position.x + 12, y: position.y + 62 });
      meta2.textContent = '状态 ' + statusLabel(status);
      var fullTitle = svgEl('title');
      fullTitle.textContent = '模块 ' + name + '\n节点：' + moduleNodes.map(function (node) { return node.id; }).join(', ');
      group.append(title, meta, meta2, fullTitle);
      group.addEventListener('click', function () { drillModule(name); });
      group.addEventListener('keydown', function (event) {
        if (event.key === 'Enter' || event.key === ' ' || event.key === 'Spacebar') {
          event.preventDefault();
          drillModule(name);
        }
      });
      moduleLayer.append(group);
      moduleElements.set(name, group);
    });

    container.append(svg);
    if (focusedModule && moduleElements.has(focusedModule)) {
      try { moduleElements.get(focusedModule).focus({ preventScroll: true }); } catch (error) { /* keep going */ }
    }
  }

  function renderGraphs() {
    var plan = planForSelected();
    var evaluation = currentEvaluation();
    var project = currentProject();
    ui['graph-filter-label'].textContent = moduleFilter ? '已下钻：' + clip(moduleFilter, 60) : '显示全部模块';
    ui['graph-show-all'].hidden = !moduleFilter;
    ui['module-graph-note'].textContent = moduleFilter ? '当前高亮模块：' + clip(moduleFilter, 60) : '点击模块下钻到节点图。';
    renderModuleGraph(plan, evaluation, project);
    renderNodeGraph(plan, evaluation, project);
  }

  function selectNode(id, reveal) {
    var plan = planForSelected();
    if (!plan) return;
    var node = plan.nodes.find(function (item) { return item.id === id; });
    if (!node) return;
    if (selectedNode !== id) editorNotice = null;
    selectedNode = id;
    if (moduleFilter && node.module !== moduleFilter) moduleFilter = null;
    renderGraphs();
    renderTimeline();
    renderNodeDetail();
    var element = nodeElements.get(id);
    if (element) {
      try { element.focus({ preventScroll: true }); } catch (error) { element.focus(); }
    }
    if (reveal && element) {
      var container = ui['node-graph'];
      var rect = element.getBoundingClientRect();
      var box = container.getBoundingClientRect();
      container.scrollLeft += rect.left - box.left - 16;
      container.scrollTop += rect.top - box.top - 16;
    }
  }

  function preferredNodeForModule(plan, evaluation, name, project) {
    var steps = stepMap(evaluation);
    var priorities = ['failed', 'outcome_unknown', 'blocked', 'waiting', 'running'];
    for (var i = 0; i < priorities.length; i += 1) {
      var failed = plan.nodes.find(function (node) {
        var step = steps.get(node.id);
        return node.module === name && step && displayStepStatus(project, evaluation, step) === priorities[i];
      });
      if (failed) return failed;
    }
    return plan.nodes.find(function (node) { return node.module === name; }) || null;
  }

  function drillModule(name) {
    var plan = planForSelected();
    if (!plan) return;
    moduleFilter = name;
    var target = preferredNodeForModule(plan, currentEvaluation(), name, currentProject());
    if (target && selectedNode !== target.id) editorNotice = null;
    if (target) selectedNode = target.id;
    renderGraphs();
    renderTimeline();
    renderNodeDetail();
    var element = selectedNode ? nodeElements.get(selectedNode) : null;
    var container = ui['node-graph'];
    if (element) {
      try { element.focus({ preventScroll: true }); } catch (error) { element.focus(); }
      var rect = element.getBoundingClientRect();
      var box = container.getBoundingClientRect();
      container.scrollLeft += rect.left - box.left - 16;
      container.scrollTop += rect.top - box.top - 16;
    }
  }

  // ---------- node detail and editor ----------

  function draftKey() {
    if (!selectedProject || !selectedNode) return null;
    return selectedProject + '\u001f' + selectedNode;
  }

  function editorFocused() {
    var active = document.activeElement;
    return active === ui['node-source'] || active === ui['node-params'] || active === ui['node-pointer'];
  }

  function setEditorValues(values) {
    if (ui['node-source'].value !== values.source) ui['node-source'].value = values.source;
    if (ui['node-params'].value !== values.params) ui['node-params'].value = values.params;
    if (ui['node-pointer'].value !== values.pointer) ui['node-pointer'].value = values.pointer;
    loadedEditorValues = { source: values.source, params: values.params, pointer: values.pointer };
  }

  function editorValuesChanged() {
    if (!loadedEditorValues) return false;
    return ui['node-source'].value !== loadedEditorValues.source ||
      ui['node-params'].value !== loadedEditorValues.params ||
      ui['node-pointer'].value !== loadedEditorValues.pointer;
  }

  function renderNodeDetail() {
    var project = currentProject();
    var plan = planForSelected();
    var evaluation = currentEvaluation();
    var node = plan && selectedNode ? plan.nodes.find(function (item) { return item.id === selectedNode; }) : null;
    var label = evaluation ? '第 v' + evaluation.version + ' 轮 · ' + modeLabel(evaluation.mode) : '尚无结果轮次';
    ui['node-detail-subtitle'].textContent = node ? label : '';

    if (!node) {
      ui['node-empty'].hidden = false;
      ui['node-empty'].textContent = plan ? '在图中选择一个节点。' : '没有可编辑的节点计划。';
      ui['node-body'].hidden = true;
      editorKey = null;
      updateActionButtons();
      return;
    }

    ui['node-empty'].hidden = true;
    ui['node-body'].hidden = false;

    var heading = ui['node-heading'];
    heading.replaceChildren();
    heading.append(el('code', null, node.id), el('strong', null, node.label || node.id));
    heading.append(badge(node.module || '未命名模块', 'badge-none'));
    if (node.implementation) heading.append(el('span', 'small muted', '实现：' + node.implementation));
    if (evaluation) {
      var mode = modeBadge(evaluation.mode);
      heading.append(mode);
    }

    var step = evaluation ? stepMap(evaluation).get(node.id) : null;
    var stepStatus = step ? displayStepStatus(project, evaluation, step) : null;
    var facts = ui['node-facts'];
    facts.replaceChildren();
    factRow(facts, '状态', step
      ? statusBadge(stepStatus)
      : badge(evaluation ? '本轮未执行' : '无结果轮次', 'status-none'));
    factJson(facts, '输入', step ? step.input : undefined);
    factJson(facts, '前置状态', step ? step.state_before : undefined);
    factJson(facts, '输出', step ? step.output : undefined);
    factJson(facts, '后置状态', step ? step.state_after : undefined);
    factRow(facts, '分支', step && step.branch !== null && step.branch !== undefined ? step.branch : '—');
    factRow(facts, '耗时', step ? formatMs(step.elapsed_ms) : '—');
    if (step) factRow(facts, '证据游标', evidenceDetails(project, step.evidence_cursor));
    else factRow(facts, '证据游标', '无');
    factRow(facts, '复用', step ? (step.reused ? '是（复用上轮结果）' : '否') : '—');
    factRow(facts, '实现', node.implementation || '—');
    factRow(facts, '模块', node.module || '未命名');
    factRow(facts, '输入指针', node.input_pointer ? node.input_pointer : '（完整输入）');

    var errorNode = ui['node-error'];
    errorNode.replaceChildren();
    if (step && step.error) {
      errorNode.append(preText(step.error, false));
      errorNode.append(el('p', 'fact-suggestion', '建议检查：本节点的 source、input_pointer 和 params。这里只列出需要核对的位置，不推断根因。'));
      errorNode.className = 'alert';
      errorNode.hidden = false;
    } else if (step && (step.status === 'failed' || step.status === 'blocked' || step.status === 'outcome_unknown')) {
      errorNode.append(el('p', null, '本轮节点状态为“' + statusLabel(step.status) + '”，但没有记录具体错误文本。'));
      errorNode.append(el('p', 'fact-suggestion', '建议检查：本节点的 source、input_pointer 和 params。这里只列出需要核对的位置，不推断根因。'));
      errorNode.className = 'alert is-warning';
      errorNode.hidden = false;
    } else {
      errorNode.className = 'alert';
      errorNode.hidden = true;
    }

    var gapContainer = ui['node-gaps'];
    gapContainer.replaceChildren();
    var gapsForNode = relatedGaps(project, evaluation, node.id);
    if (gapsForNode.length) {
      gapContainer.hidden = false;
      gapContainer.append(el('h3', null, '上下文缺口'));
      gapsForNode.forEach(function (entry) {
        var card = el('article', 'fact-item is-gap');
        card.append(el('h3', null, entry.source + '缺口 · ' + (entry.gap.node_id || '全局')));
        card.append(el('p', null, entry.gap.message || '—'));
        gapContainer.append(card);
      });
    } else {
      gapContainer.hidden = true;
    }

    var key = draftKey();
    var draft = key ? drafts.get(key) : null;
    if (editorKey !== key) {
      editorKey = key;
      if (draft) setEditorValues(draft);
      else setEditorValues({
        source: node.source || '',
        params: jsonText(node.params === undefined ? null : node.params),
        pointer: node.input_pointer || ''
      });
    } else if (!draft && !editorFocused() && !editorValuesChanged()) {
      var fresh = {
        source: node.source || '',
        params: jsonText(node.params === undefined ? null : node.params),
        pointer: node.input_pointer || ''
      };
      setEditorValues(fresh);
    }

    updateActionButtons();
    updateSaveStatus(draft);
  }

  function onEditorInput() {
    var key = draftKey();
    if (!key) return;
    var existing = drafts.get(key);
    drafts.set(key, {
      source: ui['node-source'].value,
      params: ui['node-params'].value,
      pointer: ui['node-pointer'].value,
      dirty: true,
      baseVersion: existing && existing.baseVersion !== undefined ? existing.baseVersion : (selectedRevision() ? selectedRevision().version : projectVersion(currentProject()))
    });
    editorNotice = null;
    updateSaveStatus(drafts.get(key));
  }

  function updateSaveStatus(draft) {
    var node = ui['save-status'];
    var text = '';
    var kind = 'small';
    if (pendingSave) {
      text = '正在保存并求值…';
    } else if (editorNotice) {
      text = editorNotice.text;
      if (editorNotice.kind === 'error') kind += ' status-error';
      else if (editorNotice.kind === 'ok') kind += ' status-ok';
      else kind += ' status-warn';
    } else if (draft && draft.dirty) {
      var latest = currentProject() ? projectVersion(currentProject()) : 0;
      text = '有未保存的修改。';
      if (draft.baseVersion !== undefined && draft.baseVersion !== latest) {
        text += '草稿基于 v' + draft.baseVersion + '，最新版本是 v' + latest + '；请先查看最新结果并核对。旧草稿不会覆盖新版本。要基于最新版本编辑，请选择最新结果轮次并放弃草稿。';
      } else {
        text += '保存后会自动求值并显示新结果。';
      }
      kind += ' status-warn';
    } else if (!draft) {
      var project = currentProject();
      var revision = selectedRevision();
      var latestVersion = project ? projectVersion(project) : 0;
      if (revision && latestVersion > revision.version && selectedNode) {
        text = '当前查看的是 v' + revision.version + '，最新版本是 v' + latestVersion +
          '。保存会触发版本冲突保护；请先选择最新结果轮次再编辑。';
        kind += ' status-warn';
      }
    }
    node.className = kind;
    node.textContent = text;
  }

  // ---------- save and undo ----------

  function discardDraft() {
    if (pendingSave || pendingUndo) return;
    var key = draftKey();
    if (!key || !drafts.has(key)) return;
    drafts.delete(key);
    editorKey = null;
    editorNotice = { kind: 'warn', text: '已放弃草稿，编辑区恢复为当前所选版本的内容。若要基于最新版本编辑，请先在“结果轮次”选择最新一轮。' };
    renderNodeDetail();
  }

  async function saveNode() {
    if (pendingSave) return;
    var project = currentProject();
    var plan = planForSelected();
    if (!project || !selectedNode || !plan) return;
    if (!plan.nodes.some(function (node) { return node.id === selectedNode; })) {
      editorNotice = { kind: 'error', text: '当前轮次的计划里没有这个节点，不能保存。' };
      renderNodeDetail();
      return;
    }
    var paramsText = ui['node-params'].value.trim();
    var params;
    try {
      params = JSON.parse(paramsText === '' ? 'null' : paramsText);
    } catch (error) {
      editorNotice = { kind: 'error', text: '参数不是合法 JSON，未发送保存请求。' };
      updateSaveStatus(drafts.get(draftKey()));
      ui['node-params'].focus();
      return;
    }

    var key = draftKey();
    var draft = drafts.get(key);
    var expectedVersion = draft && draft.baseVersion !== undefined ? draft.baseVersion : selectedRevision().version;
    var sourceText = ui['node-source'].value;
    var pointerText = ui['node-pointer'].value.trim();
    var body = {
      expected_version: expectedVersion,
      node_id: selectedNode,
      source: sourceText,
      params: params,
      input_pointer: pointerText
    };
    // Keep the user's exact JSON number lexemes; JSON.parse would round values
    // beyond IEEE-754 precision. The text was already validated above.
    var bodyText = '{"expected_version":' + JSON.stringify(expectedVersion) +
      ',"node_id":' + JSON.stringify(selectedNode) +
      ',"source":' + JSON.stringify(sourceText) +
      ',"params":' + (paramsText === '' ? 'null' : paramsText) +
      ',"input_pointer":' + JSON.stringify(pointerText) + '}';
    var projectId = project.id;
    // Snapshot the fields at request time. If the user keeps typing while the
    // save is in flight, those newer edits must survive as a new draft.
    var savedValues = {
      source: ui['node-source'].value,
      params: ui['node-params'].value,
      pointer: ui['node-pointer'].value
    };
    pendingSave = true;
    editorNotice = null;
    updateActionButtons();
    updateSaveStatus(drafts.get(key));
    try {
      var result = await api('/api/projects/' + encodeURIComponent(projectId) + '/edit', {
        method: 'POST',
        body: bodyText
      }, 60000);
      if (result.status === 401) {
        disconnect('网页访问令牌已失效，请重新连接。草稿仍保留在本页。');
        return;
      }
      if (result.status === 409) {
        var conflict = '版本冲突：' + (extractError(result.payload) || '服务端已有更新') +
          ' 草稿已保留，但不会用旧内容覆盖新版本。请核对最新结果；如需基于最新版本重新编辑，请先在“结果轮次”选择最新一轮，再放弃草稿。';
        if (checkDocument(result.payload)) applyDocument(result.payload, { force: true });
        else await refreshState();
        if (draftKey() === key) {
          editorNotice = { kind: 'error', text: conflict };
          renderNodeDetail();
        }
        return;
      }
      if (result.status === 422 || result.status === 400) {
        if (draftKey() === key) {
          editorNotice = { kind: 'error', text: '保存被拒绝：' + (extractError(result.payload) || '源码、参数或输入指针无效。') };
          renderNodeDetail();
        }
        return;
      }
      if (result.status === 404) {
        if (draftKey() === key) {
          editorNotice = { kind: 'error', text: '项目不存在，可能已被清理。草稿仍保留在本页。' };
          renderNodeDetail();
        }
        return;
      }
      if (result.status !== 200 || !checkDocument(result.payload)) {
        if (draftKey() === key) {
          editorNotice = { kind: 'error', text: '保存失败（HTTP ' + result.status + '）。草稿仍保留在本页。' };
          renderNodeDetail();
        }
        return;
      }

      var updated = result.payload.projects && result.payload.projects[projectId] ? result.payload.projects[projectId] : null;
      var newRevisionVersion = updated ? projectVersion(updated) : body.expected_version;
      var latest = updated ? latestEvaluationVersion(updated) : null;
      var ready = latest !== null && newRevisionVersion > body.expected_version && latest >= newRevisionVersion;
      // Compare the saved node's own draft with the values that were sent.
      // The editor may now show a different node; its values must not be
      // written back here or used to decide that this node has no newer edits.
      var currentDraft = drafts.get(key);
      var hasNewerEdits = Boolean(currentDraft && (
        currentDraft.source !== savedValues.source ||
        currentDraft.params !== savedValues.params ||
        currentDraft.pointer !== savedValues.pointer
      ));
      if (hasNewerEdits) {
        // The user typed while the request was in flight. Keep those edits,
        // now based on the revision the server just created.
        drafts.set(key, {
          source: currentDraft.source,
          params: currentDraft.params,
          pointer: currentDraft.pointer,
          dirty: true,
          baseVersion: newRevisionVersion
        });
      } else {
        drafts.delete(key);
        if (draftKey() === key) editorKey = null;
      }
      if (ready && selectedProject === projectId) {
        selectedEvaluation = latest;
      }
      applyDocument(result.payload, { force: true });
      if (draftKey() === key) {
        editorNotice = {
          kind: hasNewerEdits ? 'warn' : 'ok',
          text: (ready ? '已保存，当前显示第 v' + latest + ' 轮结果。' : '已保存，等待新的结果轮次。') +
            (hasNewerEdits ? '保存期间的新修改已保留为草稿（基于 v' + newRevisionVersion + '），可继续保存或放弃。' : '')
        };
        renderNodeDetail();
      }
    } catch (error) {
      if (draftKey() === key) {
        editorNotice = {
          kind: 'error',
          text: error && error.name === 'AbortError'
            ? '保存请求超时，结果未确认。草稿已保留；页面会继续刷新状态。'
            : '连接中断，保存结果未确认。草稿已保留；页面会继续刷新状态。'
        };
        renderNodeDetail();
      }
      refreshNow();
    } finally {
      pendingSave = false;
      updateActionButtons();
      updateSaveStatus(drafts.get(draftKey()));
    }
  }

  async function refreshState() {
    try {
      var result = await api('/api/state', {}, 15000);
      if (result.status === 200 && checkDocument(result.payload)) {
        applyDocument(result.payload, { force: true });
        return true;
      }
      if (result.status === 401) disconnect('网页访问令牌已失效，请重新连接。');
    } catch (error) {
      reportGlobal('刷新状态失败，页面会继续轮询。');
    }
    return false;
  }

  async function undoToVersion() {
    if (pendingUndo) return;
    var project = currentProject();
    if (!project || undoTarget === null || undoTarget === undefined) return;
    var expected = projectVersion(project);
    if (undoTarget === expected) return;
    var projectId = project.id;
    var target = undoTarget;
    pendingUndo = true;
    undoNotice = null;
    updateActionButtons();
    ui['undo-status'].textContent = '正在撤销…';
    ui['undo-status'].className = 'small';
    try {
      var result = await api('/api/projects/' + encodeURIComponent(projectId) + '/undo', {
        method: 'POST',
        body: JSON.stringify({ expected_version: expected, target_version: target })
      }, 60000);
      if (result.status === 401) {
        disconnect('网页访问令牌已失效，请重新连接。');
        return;
      }
      if (result.status === 409) {
        var message = '版本冲突：' + (extractError(result.payload) || '服务端已有更新') + ' 撤销没有执行，已刷新状态。';
        if (checkDocument(result.payload)) applyDocument(result.payload, { force: true });
        else await refreshState();
        undoNotice = { kind: 'error', text: message };
        return;
      }
      if (result.status === 422 || result.status === 400) {
        undoNotice = { kind: 'error', text: '撤销被拒绝：' + (extractError(result.payload) || '目标版本无效。') };
        return;
      }
      if (result.status !== 200 || !checkDocument(result.payload)) {
        undoNotice = { kind: 'error', text: '撤销失败（HTTP ' + result.status + '）。' };
        return;
      }

      Array.from(drafts.keys()).forEach(function (key) {
        if (key.indexOf(projectId + '\u001f') === 0) drafts.delete(key);
      });
      editorKey = null;
      editorNotice = null;
      var updated = result.payload.projects && result.payload.projects[projectId] ? result.payload.projects[projectId] : null;
      var latest = updated ? latestEvaluationVersion(updated) : null;
      var newVersion = updated ? projectVersion(updated) : null;
      var ready = latest !== null && newVersion !== null && newVersion > expected && latest >= newVersion;
      if (ready) selectedEvaluation = latest;
      applyDocument(result.payload, { force: true });
      undoNotice = {
        kind: 'ok',
        text: '已撤销到 v' + target + (newVersion !== null ? '，并新建 v' + newVersion : '') +
          (ready ? '，当前显示第 v' + latest + ' 轮结果' : '，等待新的结果轮次') + '。草稿已清空。'
      };
      undoTarget = null;
      normalizeSelection();
      renderAll();
    } catch (error) {
      undoNotice = {
        kind: 'error',
        text: error && error.name === 'AbortError'
          ? '撤销请求超时，结果未确认。页面会继续刷新状态。'
          : '连接中断，撤销结果未确认。页面会继续刷新状态。'
      };
      refreshNow();
    } finally {
      pendingUndo = false;
      updateActionButtons();
      renderHistory();
    }
  }

  // ---------- history, comparison, gaps ----------

  function renderHistory() {
    var project = currentProject();
    var list = ui['revision-list'];
    list.replaceChildren();
    var revisions = sortedRevisions(project);
    var evaluations = sortedEvaluations(project);
    ui['revision-count'].textContent = revisions.length
      ? revisions.length + ' 个版本 · ' + evaluations.length + ' 轮结果'
      : '';
    if (!revisions.length) {
      list.append(el('li', 'revision-item', '暂无版本历史。'));
    } else {
      var currentVersion = projectVersion(project);
      revisions.slice().reverse().forEach(function (revision) {
        var item = el('li', 'revision-item' + (revision.version === currentVersion ? ' is-current' : ''));
        item.append(el('strong', null, 'v' + revision.version + (revision.version === currentVersion ? ' · 当前' : '')));
        item.append(el('p', null, '原因：' + (revision.reason || '—')));
        if (revision.changed_node) item.append(el('p', null, '修改节点：' + revision.changed_node));
        item.append(el('p', null, '时间：' + formatTime(revision.created_at_ms) + ' · 父版本：' + (revision.parent === null || revision.parent === undefined ? '无' : 'v' + revision.parent) + ' · ' + (revision.plan && Array.isArray(revision.plan.nodes) ? revision.plan.nodes.length : 0) + ' 个节点'));
        list.append(item);
      });
    }

    var status = ui['undo-status'];
    status.className = 'small';
    if (pendingUndo) {
      status.textContent = '正在撤销…';
    } else if (undoNotice) {
      status.textContent = undoNotice.text;
      if (undoNotice.kind === 'error') status.className = 'small status-error';
      else if (undoNotice.kind === 'ok') status.className = 'small status-ok';
      else status.className = 'small status-warn';
    } else if (revisions.length && undoTarget === projectVersion(project)) {
      status.textContent = '请选择一个更早的版本。';
    } else {
      status.textContent = '撤销会以最新版本为基准，创建一个新版本，不删除历史。';
    }
  }

  function pathText(evaluation) {
    if (!evaluation) return '—';
    var steps = Array.isArray(evaluation.steps) ? evaluation.steps : [];
    if (!steps.length) return '（无步骤）';
    return steps.map(function (step) {
      var text = step.node_id;
      if (step.branch !== null && step.branch !== undefined) text += ' [' + step.branch + ']';
      if (step.status === 'failed') text += '（失败）';
      else if (step.status === 'blocked') text += '（阻断）';
      else if (step.status === 'outcome_unknown') text += '（结果未知）';
      return text;
    }).join(' → ');
  }

  function errorText(evaluation) {
    if (!evaluation) return '—';
    var lines = stepErrors(evaluation).map(function (item) {
      return item.node + '：' + item.message;
    });
    var gaps = Array.isArray(evaluation.gaps) ? evaluation.gaps : [];
    gaps.forEach(function (gap) {
      lines.push('缺口' + (gap.node_id ? '（' + gap.node_id + '）' : '') + '：' + gap.message);
    });
    return lines.length ? lines.join('\n') : '（无记录）';
  }

  function outputText(evaluation) {
    if (!evaluation) return '—';
    if (evaluation.output === null || evaluation.output === undefined) return '（无输出）';
    return jsonText(evaluation.output);
  }

  function compareRow(label, beforeText, afterText, beforeEval, afterEval) {
    var row = el('div', 'compare-row');
    var heading = el('h3');
    heading.append(document.createTextNode(label));
    var same = beforeText === afterText;
    heading.append(badge(same ? '相同' : '变化', same ? 'same' : 'changed'));
    if (beforeEval && afterEval && beforeEval.status !== afterEval.status) {
      heading.append(badge(statusLabel(beforeEval.status) + ' → ' + statusLabel(afterEval.status), 'badge-none'));
    }
    row.append(heading);
    var grid = el('div', 'compare-grid');
    [
      { title: '基准', evaluation: beforeEval, text: beforeText },
      { title: '当前', evaluation: afterEval, text: afterText }
    ].forEach(function (side) {
      var column = el('div', 'compare-side');
      var head = el('h4');
      head.append(document.createTextNode(side.title));
      if (side.evaluation) {
        head.append(badge('v' + side.evaluation.version, 'badge-none'));
        head.append(modeBadge(side.evaluation.mode));
      }
      column.append(head, preText(side.text, side.text === '—'));
      grid.append(column);
    });
    row.append(grid);
    return row;
  }

  function renderComparison() {
    var container = ui['comparison-body'];
    container.replaceChildren();
    var project = currentProject();
    var evaluation = currentEvaluation();
    if (!project || !evaluation) {
      container.append(el('p', 'muted', '选择两轮结果后可查看路径、输出和错误的变化。'));
      return;
    }
    var evaluations = sortedEvaluations(project);
    var base = null;
    if (compareVersion !== null) {
      base = evaluations.find(function (item) { return item.version === compareVersion; }) || null;
    }
    if (!base) {
      container.append(el('p', 'muted', evaluations.length > 1
        ? '请选择对比基准。默认使用上一轮结果。'
        : '只有一轮结果，没有可对比的基准。'));
      return;
    }
    container.append(compareRow('路径', pathText(base), pathText(evaluation), base, evaluation));
    container.append(compareRow('输出', outputText(base), outputText(evaluation), base, evaluation));
    container.append(compareRow('错误', errorText(base), errorText(evaluation), base, evaluation));
  }

  function renderGaps() {
    var container = ui['gaps-list'];
    var cards = [];
    var project = currentProject();
    var evaluation = currentEvaluation();
    if (evaluation) {
      var modeCard = el('article', 'fact-item is-session');
      modeCard.append(el('h3', null, evaluation.mode === 'recompute' ? '局部重算' : evaluation.mode === 'replay' ? '本地重放' : evaluation.mode === 'observed' ? '原始记录' : '运行方式未知'));
      modeCard.append(el('p', null, MODE_DETAILS[evaluation.mode] || '这一轮没有标注运行方式。'));
      modeCard.append(el('p', 'fact-meta', '本地重放与局部重算都只使用捕获到的 Host 响应；缺失的分支或响应会显示为缺口，不会静默成功。'));
      cards.push(modeCard);
    }
    var gaps = (evaluation && Array.isArray(evaluation.gaps) ? evaluation.gaps : []).slice();
    if (project && Array.isArray(project.gaps)) gaps = gaps.concat(project.gaps);
    gaps.forEach(function (gap, index) {
      var card = el('article', 'fact-item is-gap');
      card.append(el('h3', null, '缺口 ' + (index + 1) + (gap.node_id ? ' · ' + gap.node_id : '')));
      card.append(preText(gap.message || '—', false));
      cards.push(card);
    });
    if (!gaps.length) {
      cards.push(el('p', 'muted', evaluation ? '本轮没有记录缺口。' : '没有可选的结果轮次。'));
    }
    clearAndAppend(container, cards);
  }

  boot();
})();
