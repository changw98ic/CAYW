// Integration tests for WebUI workbench DOM panels and service entrances.
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { createServer } from 'node:http';
import { once } from 'node:events';
import vm from 'node:vm';

const source = await readFile(new URL('../assets/app.js', import.meta.url), 'utf8');
const html = await readFile(new URL('../assets/index.html', import.meta.url), 'utf8');

class Element {
  constructor(tag = 'div') {
    this.tagName = tag;
    this.children = [];
    this.listeners = new Map();
    this.dataset = {};
    this.disabled = false;
    this.hidden = false;
    this.value = '';
    this.checked = false;
    this.ownText = '';
  }
  set textContent(text) { this.ownText = String(text); this.children = []; }
  get textContent() { return this.ownText + this.children.map(child => child.textContent).join(''); }
  append(...children) {
    this.children.push(...children.flatMap(child => child.tagName === '#fragment' ? child.children : [child]));
  }
  replaceChildren(...children) { this.ownText = ''; this.children = []; this.append(...children); }
  get classList() {
    return {
      add: (...names) => {
        this.className = [...new Set([...(this.className ?? '').split(/\s+/).filter(Boolean), ...names])].join(' ');
      }
    };
  }
  setAttribute(name, value) { this[name] = value; }
  addEventListener(name, fn) {
    const list = this.listeners.get(name) ?? [];
    list.push(fn);
    this.listeners.set(name, list);
  }
  async dispatch(name, event = { preventDefault() {} }) {
    for (const fn of this.listeners.get(name) ?? []) await fn(event);
  }
  showModal() { this.open = true; }
  close() { this.open = false; }
  focus() {}
  querySelectorAll(selector) {
    const result = [];
    const walk = node => {
      if (!node || !node.children) return;
      for (const child of node.children) {
        if (selector === 'button' && child.tagName === 'button') result.push(child);
        if (selector === 'input' && child.tagName === 'input') result.push(child);
        if (selector === 'form' && child.tagName === 'form') result.push(child);
        walk(child);
      }
    };
    walk(this);
    return result;
  }
}

async function setupApp(t, handler) {
  const calls = [];
  const openedWindows = [];
  const server = createServer(async (req, res) => {
    let body = '';
    for await (const chunk of req) body += chunk;
    const call = JSON.parse(body);
    calls.push(call);
    let result;
    if (call.method === 'runtime.health') {
      result = { status: 'ok', protocol_version: 3 };
    } else {
      result = await handler(call.method, call.params, req.url);
    }
    const error = result?.error;
    res.writeHead(error ? (result.status || 500) : 200, { 'Content-Type': 'application/json' });
    res.end(JSON.stringify(error ? { error } : { result }));
  });
  server.listen(0, '127.0.0.1');
  await once(server, 'listening');
  t.after(() => new Promise(resolve => { server.close(resolve); server.closeAllConnections(); }));

  const ids = [...html.matchAll(/id="([^"]+)"/g)].map(match => match[1]);
  const elements = Object.fromEntries(ids.map(id => [id, new Element()]));
  for (const match of html.matchAll(/<[^>]*id="([^"]+)"[^>]*>/g)) {
    elements[match[1]].disabled = /\bdisabled\b/.test(match[0]);
    elements[match[1]].hidden = /\bhidden\b/.test(match[0]);
  }

  const timers = new Map();
  let nextTimer = 0;
  const context = vm.createContext({
    document: {
      getElementById: id => elements[id],
      createElement: tag => new Element(tag),
      createDocumentFragment: () => new Element('#fragment'),
      addEventListener() {},
      hidden: false
    },
    window: {
      addEventListener() {},
      matchMedia: () => ({ matches: false }),
      open: (url, target, features) => {
        openedWindows.push({ url, target, features });
        return {};
      },
    },
    AbortController,
    TypeError,
    URL,
    fetch: (url, options) => fetch(`http://127.0.0.1:${server.address().port}${url}`, options),
    setTimeout: (fn, delay) => { const id = ++nextTimer; timers.set(id, { fn, delay }); return id; },
    clearTimeout: id => timers.delete(id),
  });

  vm.runInContext(source.replaceAll('export ', ''), context, { filename: 'app.js' });

  const WEB_TOKEN = 'test-web-token-0123456789abcdef';
  elements['web-token'].value = WEB_TOKEN;
  await elements['login-form'].dispatch('submit');
  assert.equal(elements.workspace.hidden, false);

  const waitTick = (ms = 100) => new Promise(resolve => setTimeout(resolve, ms));

  return { ui: elements, calls, openedWindows, WEB_TOKEN, waitTick };
}

test('workbench_dom: visual editor configurable entrance opens in external window without leaking tokens', async t => {
  let openedPanelState = null;
  const app = await setupApp(t, async (method, params, url) => {
    if (method === 'session.list') return [{ id: 's1', label: '会话 1' }];
    if (method === 'session.get') return { id: 's1', label: '会话 1', cursor: 0, state: 'idle', messages: [] };
    if (method === 'session.events') return [];
    if (url === '/api/panels') {
      if (method === 'panels.catalog') return { sidebar: [{ id: 'editor', title: '可视化编辑器' }], bottom: [] };
      if (method === 'panels.open') {
        openedPanelState = {
          instance_id: 'inst-ed-1',
          panel_id: 'editor',
          title: '可视化编辑器',
          active: true,
          zone: 'sidebar',
          path: 'index.html',
        };
        return openedPanelState;
      }
      if (method === 'panels.list_open') return openedPanelState ? [openedPanelState] : [];
      if (method === 'panels.interact') {
        openedPanelState.path = params.path;
        return openedPanelState;
      }
    }
    return {};
  });

  await app.ui['wb-editor'].dispatch('click');
  await app.waitTick();

  const sidebar = app.ui['panel-sidebar-open'];
  const text = sidebar.textContent;
  assert.ok(text.includes('可视化编辑器服务配置'), 'editor configuration card rendered');
  assert.ok(text.includes('在新窗口中输入 CAYW_EDITOR_TOKEN'), 'actual service login explained');

  const inputs = sidebar.querySelectorAll('input');
  assert.equal(inputs.length, 2, 'only address and host-record path are collected');
  inputs[0].value = 'http://127.0.0.1:4173/';

  const buttons = sidebar.querySelectorAll('button');
  const openBtn = buttons.find(b => b.textContent.includes('新窗口打开'));
  assert.ok(openBtn, 'open in new window button found');

  await openBtn.dispatch('click');
  assert.equal(app.openedWindows.length, 1);
  const win = app.openedWindows[0];
  assert.ok(win.url.startsWith('http://127.0.0.1:4173'), 'opens editor service URL');
  assert.equal(win.target, '_blank');
  assert.ok(win.features.includes('noopener') && win.features.includes('noreferrer'), 'security flags set');
  assert.ok(!win.url.includes(app.WEB_TOKEN), 'main web token must never be in external URL');
  assert.equal(new URL(win.url).search, '', 'no query token or unsupported page selector');
  inputs[0].value = 'http://127.0.0.1:4173/?token=secret';
  await openBtn.dispatch('click');
  assert.equal(app.openedWindows.length, 1, 'credential-bearing URL is refused');
});

test('workbench_dom: trace debug entrance and in-panel run_inspector integration', async t => {
  let openedPanelState = null;
  const app = await setupApp(t, async (method, params, url) => {
    if (method === 'session.list') return [{ id: 's1', label: '会话 1' }];
    if (method === 'session.get') return { id: 's1', label: '会话 1', cursor: 0, state: 'idle', messages: [] };
    if (method === 'session.events') return [];
    if (url === '/api/panels') {
      if (method === 'panels.catalog') return { sidebar: [{ id: 'trace', title: '追踪调试' }], bottom: [] };
      if (method === 'panels.open') {
        openedPanelState = {
          instance_id: 'inst-trace-1',
          panel_id: 'trace',
          title: '追踪调试',
          active: true,
          zone: 'sidebar',
        };
        return openedPanelState;
      }
      if (method === 'panels.list_open') return openedPanelState ? [openedPanelState] : [];
    }
    if (method === 'session.run_inspector') {
      return {
        protocol_version: 2,
        snapshot: {
          session_id: 's1',
          steps: [
            {
              step_id: 'step-1',
              session_id: 's1',
              turn: 1,
              ordinal: 0,
              tool: 'fs.read',
              params_digest: 'sha256:abc12345',
              outcome: 'failed',
              started_at_ms: 100,
              elapsed_ms: 25,
              error_summary: 'file not found',
            },
            {
              step_id: 'step-2',
              session_id: 's1',
              turn: 1,
              ordinal: 1,
              tool: 'fs.read',
              params_digest: 'sha256:def67890',
              outcome: 'success',
              started_at_ms: 130,
              elapsed_ms: 12,
            },
            {
              step_id: 'step-3',
              session_id: 's1',
              turn: 2,
              ordinal: 0,
              tool: 'model',
              params_digest: 'sha256:11122233',
              // outcome omitted to verify missing outcome defaults to unknown, never success
              started_at_ms: 150,
              elapsed_ms: 5,
            }
          ]
        },
        timeline: {
          session_id: 's1',
          paths: [
            {
              failed_step_id: 'step-1',
              recovered_step_id: 'step-2',
              kind: 'changed_params'
            }
          ],
          track: { total_duration_ms: 42, points: [] }
        },
        events_total: 10,
        after: 0,
        events_scanned: 10,
        has_more: false,
        next_after: 10,
      };
    }
    return {};
  });

  await app.ui['wb-trace'].dispatch('click');
  await app.waitTick();

  const sidebar = app.ui['panel-sidebar-open'];
  assert.ok(sidebar.textContent.includes('Trace Debug 外部调试服务'), 'trace debug card rendered');

  const buttons = sidebar.querySelectorAll('button');
  const openBtn = buttons.find(b => b.textContent.includes('新窗口打开 Trace Debug'));
  assert.ok(openBtn, 'open trace debug button present');
  sidebar.querySelectorAll('input')[0].value = 'http://127.0.0.1:3000/';
  await openBtn.dispatch('click');
  assert.equal(app.openedWindows.length, 1);
  assert.equal(new URL(app.openedWindows[0].url).search, '', 'no query token or unsupported session selector');
  assert.ok(app.openedWindows[0].url.startsWith('http://127.0.0.1:3000'), 'opens trace debug URL');
  assert.ok(!app.openedWindows[0].url.includes(app.WEB_TOKEN), 'main token isolated');

  const refreshBtn = buttons.find(b => b.textContent.includes('刷新执行步骤'));
  assert.ok(refreshBtn, 'refresh steps button present');
  await refreshBtn.dispatch('click');
  await app.waitTick();

  const text = sidebar.textContent;
  assert.ok(text.includes('fs.read'), 'tool step rendered');
  assert.ok(text.includes('file not found'), 'error summary rendered');
  assert.ok(text.includes('changed_params'), 'recovery path rendered');
  assert.ok(text.includes('step-1 ➔ step-2'), 'recovery transition rendered');
  const findStep = element => {
    if (element.children?.some(child => child.tagName === 'strong' && child.textContent === '[轮次 2 · 序 0] model')) return element;
    for (const child of element.children || []) {
      const row = findStep(child);
      if (row) return row;
    }
  };
  const missingOutcomeRow = findStep(sidebar);
  assert.ok(missingOutcomeRow, 'step with missing outcome rendered');
  const outcomeBadge = missingOutcomeRow.children.find(child => child.className?.split(' ').includes('badge'));
  assert.equal(outcomeBadge?.textContent, 'unknown', 'missing outcome must not render as success');
});

test('workbench_dom: artifacts panel uses canonical artifacts.* RPCs with preview and recovery', async t => {
  let openedPanelState = null;
  const artifactList = [
    {
      artifact_id: 'art-1',
      session_id: 's1',
      kind: 'html_card',
      title: '架构设计图',
      content_version: 1,
      body: '<div>SVG Content</div>',
    }
  ];

  const app = await setupApp(t, async (method, params, url) => {
    if (method === 'session.list') return [{ id: 's1', label: '会话 1' }];
    if (method === 'session.get') return { id: 's1', label: '会话 1', cursor: 0, state: 'idle', messages: [] };
    if (method === 'session.events') return [];
    if (url === '/api/panels') {
      if (method === 'panels.catalog') return { sidebar: [{ id: 'artifacts', title: '产物与动作' }], bottom: [] };
      if (method === 'panels.open') {
        openedPanelState = {
          instance_id: 'inst-art-1',
          panel_id: 'artifacts',
          title: '产物与动作',
          active: true,
          zone: 'sidebar',
        };
        return openedPanelState;
      }
      if (method === 'panels.list_open') return openedPanelState ? [openedPanelState] : [];
    }
    if (method === 'artifacts.list') return artifactList;
    if (method === 'artifacts.preview') return { html: '<div>SVG Content Preview</div>', content_type: 'text/html' };
    if (method === 'artifacts.action') {
      return {
        record: {
          action_id: params.action_id,
          status: 'applied',
          outcome_json: JSON.stringify({ ok: true })
        }
      };
    }
    if (method === 'artifacts.recovery_save') return { version: 1 };
    if (method === 'artifacts.recovery_load') return { loaded: true };
    return {};
  });

  await app.ui['wb-artifacts'].dispatch('click');
  await app.waitTick(150);

  const sidebar = app.ui['panel-sidebar-open'];
  let buttons = sidebar.querySelectorAll('button');
  const refreshBtn = buttons.find(b => b.textContent.includes('刷新产物'));
  if (refreshBtn) await refreshBtn.dispatch('click');
  await app.waitTick(150);

  assert.ok(sidebar.textContent.includes('架构设计图'), 'artifact title rendered');

  buttons = sidebar.querySelectorAll('button');
  const previewBtn = buttons.find(b => b.textContent.includes('获取渲染投影'));
  assert.ok(previewBtn, 'preview projection button present');
  await previewBtn.dispatch('click');
  await app.waitTick(150);
  assert.ok(sidebar.textContent.includes('text/html'), 'preview content type rendered');

  const actionBtn = buttons.find(b => b.textContent.includes('转发动作到 Runtime'));
  assert.ok(actionBtn, 'action dispatch button present');
  await actionBtn.dispatch('click');
  await app.waitTick(150);

  const saveBtn = buttons.find(b => b.textContent.includes('保存快照'));
  assert.ok(saveBtn, 'recovery save button present');
  await saveBtn.dispatch('click');
  await app.waitTick(150);

  const loadBtn = buttons.find(b => b.textContent.includes('恢复快照'));
  assert.ok(loadBtn, 'recovery load button present');
  await loadBtn.dispatch('click');
  await app.waitTick(150);

  const calledMethods = app.calls.map(c => c.method);
  assert.ok(calledMethods.includes('artifacts.list'), 'artifacts.list called');
  assert.ok(calledMethods.includes('artifacts.preview'), 'artifacts.preview called');
  assert.ok(calledMethods.includes('artifacts.action'), 'artifacts.action called');
  assert.ok(calledMethods.includes('artifacts.recovery_save'), 'artifacts.recovery_save called');
  assert.ok(calledMethods.includes('artifacts.recovery_load'), 'artifacts.recovery_load called');
  assert.ok(!calledMethods.some(m => m.startsWith('artifact.')), 'no legacy artifact.* calls');
  assert.ok(!calledMethods.includes('artifacts.action_dispatch'), 'no legacy action_dispatch alias');
});

test('workbench_dom: plannotator binds to authoritative session.plan facts and rejects fake client version inputs', async t => {
  let openedPanelState = null;
  const currentPlan = {
    task_id: 'task-plan-42',
    version: 3,
    content_sha256: 'a'.repeat(64),
    steps: [
      { id: 's1', text: '编写测试用例', status: 'completed' },
      { id: 's2', text: '重构核心逻辑', status: 'in_progress' }
    ]
  };
  const feedbackList = [
    {
      feedback_id: 'fb-1',
      target: {
        target_id: 'plan-task-plan-42',
        kind: 'plan',
        file: { path: 'plan:task-plan-42', version: 3, content_sha256: 'abc' }
      },
      decision: 'approve',
      summary: '前期设计审查通过'
    }
  ];

  let submittedFeedback = null;

  const app = await setupApp(t, async (method, params, url) => {
    if (method === 'session.list') return [{ id: 's1', label: '会话 1' }];
    if (method === 'session.get') return { id: 's1', label: '会话 1', cursor: 0, state: 'idle', messages: [] };
    if (method === 'session.events') return [];
    if (url === '/api/panels') {
      if (method === 'panels.catalog') return { sidebar: [{ id: 'plannotator', title: '版本批注' }], bottom: [] };
      if (method === 'panels.open') {
        openedPanelState = {
          instance_id: 'inst-plann-1',
          panel_id: 'plannotator',
          title: '版本批注',
          active: true,
          zone: 'sidebar',
        };
        return openedPanelState;
      }
      if (method === 'panels.list_open') return openedPanelState ? [openedPanelState] : [];
    }
    if (method === 'session.plan') return currentPlan;
    if (method === 'plannotator.list') return feedbackList;
    if (method === 'plannotator.submit') {
      submittedFeedback = params;
      return {
        feedback_id: 'fb-2',
        target: {
          target_id: 'plan-task-plan-42',
          file: { path: 'plan:task-plan-42', version: 3 }
        }
      };
    }
    return {};
  });

  await app.ui['wb-plannotator'].dispatch('click');
  await app.waitTick(150);

  const sidebar = app.ui['panel-sidebar-open'];
  const text = sidebar.textContent;
  assert.ok(text.includes('task-plan-42'), 'authoritative task_id bound from session.plan');
  assert.ok(text.includes('v3'), 'authoritative plan version bound from session.plan');
  assert.ok(text.includes('编写测试用例'), 'plan step summary rendered');

  const forms = sidebar.querySelectorAll('form');
  assert.equal(forms.length, 1);
  await forms[0].dispatch('submit');
  await app.waitTick(150);

  assert.ok(submittedFeedback, 'plannotator feedback was submitted');
  assert.equal(submittedFeedback.session_id, 's1');
  assert.equal(submittedFeedback.decision, 'approve');
  assert.equal(submittedFeedback.expected_task_id, currentPlan.task_id);
  assert.equal(submittedFeedback.expected_version, 3);
  assert.equal(submittedFeedback.expected_content_sha256, 'a'.repeat(64));
  assert.ok(!('file_version' in submittedFeedback), 'must NOT send client-invented file_version');
  assert.ok(!('target' in submittedFeedback), 'must NOT send client-invented target');
  assert.ok(Array.isArray(submittedFeedback.comments), 'comments included');
});

test('workbench_dom: stale plan submission preserves notes and never reloads or rebinds automatically', async t => {
  let opened = false;
  let planReads = 0;
  const submitted = [];
  const app = await setupApp(t, async (method, params) => {
    if (method === 'session.list') return [{ id: 's1', label: 'Review' }];
    if (method === 'session.get') return { id: 's1', cursor: 0, state: 'idle', messages: [] };
    if (method === 'session.events') return [];
    if (method === 'panels.catalog') return { sidebar: [{ id: 'plannotator' }], bottom: [] };
    if (method === 'panels.open') { opened = true; return {}; }
    if (method === 'panels.list_open') return opened ? [{ panel_id: 'plannotator', instance_id: 'review', active: true, zone: 'sidebar' }] : [];
    if (method === 'session.plan') {
      planReads++;
      return { task_id: 't1', version: planReads, content_sha256: (planReads === 1 ? 'a' : 'b').repeat(64), steps: [{ id: 'one', text: `Plan ${planReads}`, status: 'pending' }] };
    }
    if (method === 'plannotator.list') return [];
    if (method === 'plannotator.submit') {
      submitted.push(params);
      return { status: 502, error: { code: 'invalid_params', message: 'plan feedback version conflict', outcome_unknown: false } };
    }
    return {};
  });
  await app.ui['wb-plannotator'].dispatch('click');
  await app.waitTick();
  const sidebar = app.ui['panel-sidebar-open'];
  const form = sidebar.querySelectorAll('form')[0];
  const note = form.children.find(node => node.tagName === 'textarea');
  note.value = 'Only for plan one';
  await form.dispatch('submit');
  assert.equal(planReads, 1, 'no automatic reload after conflict');
  assert.equal(submitted.length, 1);
  assert.equal(submitted[0].expected_version, 1);
  assert.equal(note.value, 'Only for plan one');
  assert.match(sidebar.textContent, /意见已保留/);
  await form.dispatch('submit');
  assert.equal(submitted.length, 1, 'retry blocked until explicit review');
  const reload = sidebar.querySelectorAll('button').find(button => button.textContent.includes('重新读取计划'));
  await reload.dispatch('click');
  assert.equal(planReads, 2);
  assert.equal(note.value, '', 'old notes are cleared before reviewing the new version');
});

test('workbench_dom: usage panel displays session.context_overview tokens, cache hit rate and evolution', async t => {
  let openedPanelState = null;
  let overviewData = {
    session_id: 's1',
    budget_session_id: 's1',
    request_count: 5,
    turn_count: 2,
    tokens: {
      total_tokens: 3500,
      input: 2000,
      output: 1000,
      cache_read: 500,
      cache_write: 200,
      reasoning: 300,
    },
    ledger_settled_tokens: 3000,
    ledger_reserved_tokens: 500,
    ledger_remaining_tokens: 46500,
    budget_unknown: false,
    cost: { status: 'unknown' },
    cache: { cache_read: 500, cache_write: 200, hit_rate: 0.714 },
    composition: [
      { kind: 'input', tokens: 2000 },
      { kind: 'output', tokens: 1000 },
      { kind: 'cache_read', tokens: 500 }
    ],
    evolution: [
      {
        request_id: 'req-turn-1',
        session_id: 's1',
        task_id: 'task-1',
        turn: 1,
        provider: 'test-provider',
        model: 'test-model',
        terminal: 'completed',
        usage_status: 'reported',
        usage: {
          input: 800,
          output: 400,
          cache_read: 0,
          cache_write: 0,
          reasoning: 0,
          total_tokens: 1200,
        },
      },
      {
        request_id: 'req-turn-2',
        session_id: 's1',
        task_id: 'task-1',
        turn: 2,
        provider: 'test-provider',
        model: 'test-model',
        terminal: 'completed',
        usage_status: 'reported',
        usage: {
          input: 1200,
          output: 600,
          cache_read: 500,
          cache_write: 200,
          reasoning: 300,
          total_tokens: 2300,
        },
      },
    ]
  };

  const app = await setupApp(t, async (method, params, url) => {
    if (method === 'session.list') return [{ id: 's1', label: '会话 1' }];
    if (method === 'session.get') return { id: 's1', label: '会话 1', cursor: 0, state: 'idle', messages: [] };
    if (method === 'session.events') return [];
    if (url === '/api/panels') {
      if (method === 'panels.catalog') return { sidebar: [{ id: 'usage', title: '用量 / 状态' }], bottom: [] };
      if (method === 'panels.open') {
        openedPanelState = {
          instance_id: 'inst-usage-1',
          panel_id: 'usage',
          title: '用量 / 状态',
          active: true,
          zone: 'sidebar',
        };
        return openedPanelState;
      }
      if (method === 'panels.list_open') return openedPanelState ? [openedPanelState] : [];
    }
    if (method === 'session.context_overview') return overviewData;
    return {};
  });

  await app.ui['wb-usage'].dispatch('click');
  await app.waitTick();

  const sidebar = app.ui['panel-sidebar-open'];
  const buttons = sidebar.querySelectorAll('button');
  const usageBtn = buttons.find(b => b.textContent.includes('刷新用量'));
  if (usageBtn) await usageBtn.dispatch('click');
  await app.waitTick();

  const text = sidebar.textContent;
  assert.ok(text.includes('Tokens 总量: 3500 (输入: 2000, 输出: 1000, 推理: 300)'), 'total tokens card rendered without distortion');
  assert.ok(text.includes('71.4%'), 'cache hit rate rendered');
  assert.ok(text.includes('缓存读取: 500, 缓存写入: 200'), 'cache read and write rendered');
  assert.ok(text.includes('账本结算: 3000, 预留: 500, 剩余: 46500'), 'ledger tokens rendered');
  assert.ok(text.includes('费用结算状态: unknown'), 'unpriced usage stays unknown');
  assert.ok(text.includes('请求次数: 5, 对话轮次: 2'), 'request count and turn count rendered');
  assert.ok(text.includes('Token 构成分析'), 'composition breakdown rendered');
  assert.ok(text.includes('上下文演变过程 (2 轮)'), 'evolution breakdown rendered');
  assert.ok(text.includes('[轮次 1] 输入: 800, 输出: 400, 增量: 1200'), 'turn 1 accurate non-zero usage rendered');
  assert.ok(text.includes('[轮次 2] 输入: 1200, 输出: 600, 增量: 2300'), 'turn 2 accurate differing non-zero usage rendered');

  // Verify missing or empty stats render safe defaults
  overviewData = {};
  if (usageBtn) await usageBtn.dispatch('click');
  await app.waitTick();
  const emptyText = sidebar.textContent;
  assert.ok(emptyText.includes('Tokens 总量: 0 (输入: 0, 输出: 0, 推理: 0)'), 'empty stats render zero tokens');
  assert.ok(emptyText.includes('命中率: 0.0%'), 'empty cache renders 0.0% hit rate');
  assert.ok(emptyText.includes('费用结算状态: unknown'), 'empty cost renders unknown');
  assert.ok(emptyText.includes('账本结算: 0, 预留: 0, 剩余: 0'), 'empty ledger renders zero amounts');
});

test('workbench_dom: tasks panel displays plan, queue, and subagent fleet with stop and steer controls', async t => {
  let openedPanelState = null;
  let stoppedChildTask = null;
  let steeredChildTask = null;

  const app = await setupApp(t, async (method, params, url) => {
    if (method === 'session.list') return [{ id: 's1', label: '会话 1' }];
    if (method === 'session.get') return { id: 's1', label: '会话 1', cursor: 0, state: 'idle', messages: [] };
    if (method === 'session.events') return [];
    if (url === '/api/panels') {
      if (method === 'panels.catalog') return { sidebar: [{ id: 'tasks', title: '任务 / 时间线' }], bottom: [] };
      if (method === 'panels.open') {
        openedPanelState = {
          instance_id: 'inst-tasks-1',
          panel_id: 'tasks',
          title: '任务 / 时间线',
          active: true,
          zone: 'sidebar',
        };
        return openedPanelState;
      }
      if (method === 'panels.list_open') return openedPanelState ? [openedPanelState] : [];
    }
    if (method === 'session.tasks') {
      return [{ id: 'task-parent-1', state: 'running', input: { text: '父任务执行中' } }];
    }
    if (method === 'session.plan') {
      return { task_id: 'task-parent-1', version: 2, steps: [{ step_id: 'p1', title: '子任务编排', status: 'running' }] };
    }
    if (method === 'runtime.fleet') {
      return {
        members: [
          {
            child_task_id: 'sub-task-1',
            child_session_id: 'child-sess-1',
            parent_session_id: 's1',
            parent_task_id: 'task-parent-1',
            label: '代码分析子 Agent',
            depth: 1,
            mode: 'foreground',
            detached: false,
            state: 'running',
            error: null,
          },
          {
            child_task_id: 'sub-task-2',
            child_session_id: 'child-sess-2',
            parent_session_id: 's1',
            parent_task_id: 'task-parent-1',
            // label omitted to verify fallback to child_task_id
            depth: 2,
            mode: 'background',
            detached: true,
            state: 'completed',
            error: 'execution timeout',
          }
        ]
      };
    }
    if (method === 'runtime.fleet.stop') {
      stoppedChildTask = params.child_task_id;
      return { stopped: true };
    }
    if (method === 'runtime.fleet.steer') {
      steeredChildTask = params;
      return { steered: true };
    }
    return {};
  });

  await app.ui['wb-tasks'].dispatch('click');
  await app.waitTick();

  const sidebar = app.ui['panel-sidebar-open'];
  const buttons = sidebar.querySelectorAll('button');
  const refreshBtn = buttons.find(b => b.textContent.includes('刷新任务'));
  if (refreshBtn) await refreshBtn.dispatch('click');
  await app.waitTick();

  const text = sidebar.textContent;
  assert.ok(text.includes('子 Agent 舰队管理 (FleetView · 2 个实例)'), 'fleet header rendered with count');
  assert.ok(text.includes('代码分析子 Agent'), 'fleet member label rendered');
  assert.ok(text.includes('sub-task-2'), 'missing label falls back to child_task_id');
  assert.ok(text.includes('深度: 2 · 模式: background (分离)'), 'detached background member info rendered');
  assert.ok(text.includes('错误: execution timeout'), 'error message rendered');

  const updatedButtons = sidebar.querySelectorAll('button');
  const stopBtn = updatedButtons.find(b => b.textContent.includes('停止子 Agent'));
  assert.ok(stopBtn, 'stop child agent button present');
  await stopBtn.dispatch('click');
  await app.waitTick(150);
  assert.equal(stoppedChildTask, 'sub-task-1', 'runtime.fleet.stop called with child task id');

  const inputsAfterStop = sidebar.querySelectorAll('input');
  const steerInput = inputsAfterStop.find(i => i.placeholder && i.placeholder.includes('steer'));
  assert.ok(steerInput, 'steer input present');
  steerInput.value = '优先搜索 tests 目录';

  const buttonsAfterStop = sidebar.querySelectorAll('button');
  const steerBtn = buttonsAfterStop.find(b => b.textContent.includes('发送引导'));
  assert.ok(steerBtn, 'steer button present');
  await steerBtn.dispatch('click');
  await app.waitTick(150);
  assert.deepEqual(steeredChildTask, { child_task_id: 'sub-task-1', text: '优先搜索 tests 目录' });
});

test('workbench_dom: session creation with optional execution device affinity passes device_id to session.create', async t => {
  let createParams = null;
  const app = await setupApp(t, async (method, params) => {
    if (method === 'session.list') return [];
    if (method === 'session.create') {
      createParams = params;
      return { id: 'sess-affinity-1', label: params.label, cursor: 0, state: 'idle', messages: [] };
    }
    if (method === 'session.get') return { id: 'sess-affinity-1', cursor: 0, state: 'idle', messages: [] };
    if (method === 'session.events') return [];
    return {};
  });

  app.ui['session-label'].value = '设备亲和测试会话';
  app.ui['session-device'].value = 'device-macbook-pro';
  await app.ui['create-form'].dispatch('submit');

  assert.ok(createParams, 'session.create was called');
  assert.equal(createParams.label, '设备亲和测试会话');
  assert.equal(createParams.device_id, 'device-macbook-pro', 'device_id passed for OwnershipStore affinity');
});

test('workbench_dom: tasks panel handles RPC errors with explicit failure display and preserves partial results without zero-masking', async t => {
  let tasksResponse = { ok: true, data: [{ id: 'task-1', state: 'running', input: { text: '进行中的已知任务' } }] };
  let planResponse = { ok: true, data: { task_id: 'task-1', version: 1, steps: [{ step_id: 's1', text: '执行步骤 1', status: 'completed' }] } };
  let fleetResponse = { ok: true, data: { members: [{ child_task_id: 'child-1', label: '工作 Agent', depth: 1, mode: 'foreground', state: 'running' }] } };

  const app = await setupApp(t, async (method, params, url) => {
    if (method === 'session.list') return [{ id: 's1', label: '会话 1' }];
    if (method === 'session.get') return { id: 's1', label: '会话 1', cursor: 0, state: 'idle', messages: [] };
    if (method === 'session.events') return [];
    if (url === '/api/panels') {
      if (method === 'panels.catalog') return { sidebar: [{ id: 'tasks', title: '任务 / 时间线' }], bottom: [] };
      if (method === 'panels.open') return { instance_id: 'tasks:1', panel_id: 'tasks', title: '任务 / 时间线', active: true, zone: 'sidebar' };
      if (method === 'panels.list_open') return [{ instance_id: 'tasks:1', panel_id: 'tasks', title: '任务 / 时间线', active: true, zone: 'sidebar' }];
    }
    if (method === 'session.tasks') {
      if (!tasksResponse.ok) return { status: tasksResponse.status || 502, error: { code: tasksResponse.status === 403 ? 'forbidden' : 'bad_gateway', message: tasksResponse.message || 'task queue failed' } };
      return tasksResponse.data;
    }
    if (method === 'session.plan') {
      if (!planResponse.ok) return { status: planResponse.status || 502, error: { code: 'plan_error', message: planResponse.message || 'plan store failed' } };
      return planResponse.data;
    }
    if (method === 'runtime.fleet') {
      if (!fleetResponse.ok) return { status: fleetResponse.status || 500, error: { code: 'fleet_error', message: fleetResponse.message || 'fleet manager failed' } };
      return fleetResponse.data;
    }
    return {};
  });

  // Open tasks panel
  await app.ui['wb-tasks'].dispatch('click');
  await app.waitTick();
  const sidebar = app.ui['panel-sidebar-open'];
  const refreshBtn = sidebar.querySelectorAll('button').find(b => b.textContent.includes('刷新任务'));

  // 1. Tasks failure (502): must display error, NOT zero or empty queue, while plan & fleet are preserved
  tasksResponse = { ok: false, status: 502, message: 'upstream task queue unavailable' };
  if (refreshBtn) await refreshBtn.dispatch('click');
  await app.waitTick();

  let text = sidebar.textContent;
  assert.ok(text.includes('会话任务队列 (读取失败)'), 'tasks failure header displayed');
  assert.ok(text.includes('upstream task queue unavailable'), 'tasks error message rendered');
  assert.ok(!text.includes('会话任务队列 (0)'), 'must not claim zero tasks on failure');
  assert.ok(!text.includes('当前会话暂无排队任务'), 'must not claim empty queue on failure');
  assert.ok(text.includes('当前计划 (v1) · 任务: task-1'), 'successful plan remains visible');
  assert.ok(text.includes('工作 Agent'), 'successful fleet remains visible');

  // 2. Fleet failure (500): tasks succeed, fleet fails -> fleet shows error, not 0 instances
  tasksResponse = { ok: true, data: [{ id: 'task-1', state: 'running', input: { text: '进行中的已知任务' } }] };
  fleetResponse = { ok: false, status: 500, message: 'fleet service internal error' };
  if (refreshBtn) await refreshBtn.dispatch('click');
  await app.waitTick();

  text = sidebar.textContent;
  assert.ok(text.includes('子 Agent 舰队管理 (FleetView · 读取失败)'), 'fleet failure header displayed');
  assert.ok(text.includes('fleet service internal error'), 'fleet error message rendered');
  assert.ok(!text.includes('0 个实例'), 'must not claim zero instances on fleet failure');
  assert.ok(!text.includes('当前会话暂无派生的子 Agent'), 'must not claim empty fleet on failure');
  assert.ok(text.includes('进行中的已知任务'), 'successful tasks remain visible');

  // 3. Plan failure (502): plan shows error, tasks and fleet succeed
  planResponse = { ok: false, status: 502, message: 'plan store unreachable' };
  fleetResponse = { ok: true, data: { members: [{ child_task_id: 'child-1', label: '工作 Agent', depth: 1, mode: 'foreground', state: 'running' }] } };
  if (refreshBtn) await refreshBtn.dispatch('click');
  await app.waitTick();

  text = sidebar.textContent;
  assert.ok(text.includes('当前计划 (读取失败)'), 'plan failure header displayed');
  assert.ok(text.includes('plan store unreachable'), 'plan error message rendered');
  assert.ok(text.includes('会话任务队列 (1)'), 'successful tasks remain visible');
  assert.ok(text.includes('工作 Agent'), 'successful fleet remains visible');

  // 4. Auth failure (403): triggers disconnect and auth error
  tasksResponse = { ok: false, status: 403, message: '网页访问令牌无效或已失效，请重新连接。' };
  if (refreshBtn) await refreshBtn.dispatch('click');
  await app.waitTick();

  assert.equal(app.ui.workspace.hidden, true, 'auth error hides workspace');
  assert.equal(app.ui['login-panel'].hidden, false, 'auth error shows login panel');
  assert.ok(app.ui['login-error'].textContent.includes('令牌无效或已失效'), 'auth error message rendered');
});

test('workbench_dom: session switching synchronizes panel ownership and isolates per-session panel instances', async t => {
  const sessionPanels = {
    s1: [
      { instance_id: 'tasks:1', panel_id: 'tasks', title: '任务 (S1)', active: true, zone: 'sidebar' }
    ],
    s2: []
  };
  const activatedCalls = [];
  const openedCalls = [];
  let s1TasksDelayMs = 0;

  const app = await setupApp(t, async (method, params, url) => {
    if (method === 'session.list') return [
      { id: 's1', label: '会话 1' },
      { id: 's2', label: '会话 2' }
    ];
    if (method === 'session.get') {
      return { id: params.session_id, label: params.session_id === 's1' ? '会话 1' : '会话 2', cursor: 0, state: 'idle', messages: [] };
    }
    if (method === 'session.events') return [];
    if (url === '/api/panels') {
      if (method === 'panels.catalog') return { sidebar: [{ id: 'tasks', title: '任务 / 时间线' }], bottom: [] };
      if (method === 'panels.list_open') {
        return sessionPanels[params.session_id] || [];
      }
      if (method === 'panels.open') {
        openedCalls.push(params);
        const inst = { instance_id: 'tasks:1', panel_id: params.panel_id, title: `任务 (${params.session_id})`, active: true, zone: params.zone };
        sessionPanels[params.session_id] = [inst];
        return inst;
      }
      if (method === 'panels.activate') {
        activatedCalls.push(params);
        for (const p of sessionPanels[params.session_id] || []) {
          p.active = p.instance_id === params.instance_id;
        }
        return { activated: true };
      }
      if (method === 'panels.close') {
        sessionPanels[params.session_id] = (sessionPanels[params.session_id] || []).filter(p => p.instance_id !== params.instance_id);
        return { closed: true };
      }
    }
    if (method === 'session.tasks') {
      if (params.session_id === 's1' && s1TasksDelayMs > 0) {
        await new Promise(r => setTimeout(r, s1TasksDelayMs));
      }
      return [{ id: `task-for-${params.session_id}`, state: 'running', input: { text: `任务数据归属于 ${params.session_id}` } }];
    }
    if (method === 'session.plan') return null;
    if (method === 'runtime.fleet') return { members: [] };
    return {};
  });

  // Initial session is s1, open panel for s1 is rendered
  await app.waitTick();
  const sidebar = app.ui['panel-sidebar-open'];
  assert.ok(sidebar.textContent.includes('任务 (S1)'), 'initial session s1 open panel rendered');

  // Find session-item button for s2 in session-list and click it
  const sessionButtons = app.ui['session-list'].querySelectorAll('button');
  const s2Btn = sessionButtons.find(b => b.textContent.includes('s2') || b.textContent.includes('会话 2'));
  assert.ok(s2Btn, 'session s2 button present');

  await s2Btn.dispatch('click');
  await app.waitTick();

  // In s2, s1 panel instance is cleared and s2 panels list is loaded (which is currently empty)
  assert.ok(!sidebar.textContent.includes('任务 (S1)'), 's1 panel instance cleared from s2 view');

  // Click wb-tasks in s2: since s2 has no opened tasks panel, it MUST call panels.open with session_id: 's2', NOT panels.activate with s1 instance_id
  await app.ui['wb-tasks'].dispatch('click');
  await app.waitTick();

  assert.equal(openedCalls.length, 1);
  assert.equal(openedCalls[0].session_id, 's2', 'panels.open targeted to current session s2');
  assert.equal(openedCalls[0].panel_id, 'tasks');
  assert.ok(sidebar.textContent.includes('任务 (s2)'), 's2 opened panel now rendered');

  // Both s1 and s2 have instance_id: 'tasks:1', but activating in s2 targets s2
  const closeBtn = sidebar.querySelectorAll('button').find(b => b.textContent.includes('关闭'));
  assert.ok(closeBtn, 'close button in s2 panel');
  await closeBtn.dispatch('click');
  await app.waitTick();

  // Re-open in s2
  await app.ui['wb-tasks'].dispatch('click');
  await app.waitTick();
  assert.equal(openedCalls[openedCalls.length - 1].session_id, 's2');

  // Test race condition: switch back to s1, trigger slow fetch, immediately switch to s2
  const s1Btn = app.ui['session-list'].querySelectorAll('button').find(b => b.textContent.includes('s1') || b.textContent.includes('会话 1'));
  await s1Btn.dispatch('click');
  await app.waitTick();

  s1TasksDelayMs = 80;
  const refreshTasksBtn = sidebar.querySelectorAll('button').find(b => b.textContent.includes('刷新任务'));
  if (refreshTasksBtn) refreshTasksBtn.dispatch('click'); // fire in s1 with 80ms delay

  // Immediately switch to s2 (before s1's tasks respond)
  const s2BtnAgain = app.ui['session-list'].querySelectorAll('button').find(b => b.textContent.includes('s2') || b.textContent.includes('会话 2'));
  await s2BtnAgain.dispatch('click');
  await app.waitTick(120); // wait for s1 delayed response to complete

  // s2 view must not contain s1 data
  assert.ok(!sidebar.textContent.includes('任务数据归属于 s1'), 'delayed s1 task data did not overwrite s2 view');
});

test('workbench_dom: session switching drops delayed usage and artifacts responses across sessions', async t => {
  let s1DelayMs = 0;
  const openedPanelsMap = {
    s1: [],
    s2: [],
  };

  const app = await setupApp(t, async (method, params, url) => {
    if (method === 'session.list') return [
      { id: 's1', label: '会话 1' },
      { id: 's2', label: '会话 2' }
    ];
    if (method === 'session.get') {
      return { id: params.session_id, label: params.session_id === 's1' ? '会话 1' : '会话 2', cursor: 0, state: 'idle', messages: [] };
    }
    if (method === 'session.events') return [];
    if (url === '/api/panels') {
      if (method === 'panels.catalog') return { sidebar: [{ id: 'usage', title: '用量' }, { id: 'artifacts', title: '产物' }], bottom: [] };
      if (method === 'panels.list_open') return openedPanelsMap[params.session_id] || [];
      if (method === 'panels.open') {
        const inst = { instance_id: `${params.panel_id}:1`, panel_id: params.panel_id, title: params.panel_id, active: true, zone: params.zone, session_id: params.session_id };
        openedPanelsMap[params.session_id] = [inst];
        return inst;
      }
      if (method === 'panels.activate') {
        for (const p of openedPanelsMap[params.session_id] || []) {
          p.active = p.instance_id === params.instance_id;
        }
        return { activated: true };
      }
    }
    if (method === 'session.context_overview') {
      if (params.session_id === 's1' && s1DelayMs > 0) {
        await new Promise(r => setTimeout(r, s1DelayMs));
      }
      return {
        session_id: params.session_id,
        tokens: { total_tokens: params.session_id === 's1' ? 99999 : 11111 },
        cache: { hit_rate: 0.5 },
      };
    }
    if (method === 'artifacts.list') {
      if (params.session_id === 's1' && s1DelayMs > 0) {
        await new Promise(r => setTimeout(r, s1DelayMs));
      }
      return [
        {
          artifact_id: `art-${params.session_id}`,
          title: `产物仅归属于 ${params.session_id}`,
          kind: 'markdown',
          content_version: 1,
          body: `body for ${params.session_id}`,
        }
      ];
    }
    return {};
  });

  await app.waitTick();
  const sidebar = app.ui['panel-sidebar-open'];

  // Open usage in s1
  await app.ui['wb-usage'].dispatch('click');
  await app.waitTick();
  assert.ok(sidebar.textContent.includes('Tokens 总量: 99999'), 's1 usage loaded');

  // Trigger delayed fetch in s1, then immediately switch to s2
  s1DelayMs = 80;
  const usageButtons = sidebar.querySelectorAll('button');
  const refreshUsageBtn = usageButtons.find(b => b.textContent.includes('刷新用量'));
  if (refreshUsageBtn) refreshUsageBtn.dispatch('click');

  // Switch to s2 immediately
  const s2Btn = app.ui['session-list'].querySelectorAll('button').find(b => b.textContent.includes('s2') || b.textContent.includes('会话 2'));
  await s2Btn.dispatch('click');
  await app.waitTick(120); // wait for s1 delayed response to complete

  // In s2, open usage panel
  await app.ui['wb-usage'].dispatch('click');
  await app.waitTick();

  // s2 view must display s2's total tokens (11111) and NEVER s1's (99999)
  assert.ok(sidebar.textContent.includes('Tokens 总量: 11111'), 's2 usage rendered');
  assert.ok(!sidebar.textContent.includes('Tokens 总量: 99999'), 'delayed s1 usage did not overwrite s2 view');

  // Now test artifacts panel: switch back to s1
  s1DelayMs = 0;
  const s1Btn = app.ui['session-list'].querySelectorAll('button').find(b => b.textContent.includes('s1') || b.textContent.includes('会话 1'));
  await s1Btn.dispatch('click');
  await app.waitTick();

  // Open artifacts in s1
  await app.ui['wb-artifacts'].dispatch('click');
  await app.waitTick();
  assert.ok(sidebar.textContent.includes('产物仅归属于 s1'), 's1 artifact rendered');

  // Trigger delayed fetch in s1
  s1DelayMs = 80;
  const artButtons = sidebar.querySelectorAll('button');
  const refreshArtBtn = artButtons.find(b => b.textContent.includes('刷新列表'));
  if (refreshArtBtn) refreshArtBtn.dispatch('click');

  // Immediately switch to s2
  await s2Btn.dispatch('click');
  await app.waitTick(120);

  // Open artifacts in s2
  await app.ui['wb-artifacts'].dispatch('click');
  await app.waitTick();

  // s2 view must display s2's artifact and not s1's
  assert.ok(sidebar.textContent.includes('产物仅归属于 s2'), 's2 artifact rendered');
  assert.ok(!sidebar.textContent.includes('产物仅归属于 s1'), 'delayed s1 artifact did not overwrite s2 view');
});

test('workbench_dom: needs_reconciliation_outcome_unknown is classified as admission fault and does not disguise as uncertain write', async t => {
  let enqueueAttempted = false;
  let enqueueParams = null;
  const current = { id: 's1', label: '会话 1', cursor: 10, state: 'idle', messages: [], attempt: null, pending_approvals: [], unresolved_calls: [] };

  const app = await setupApp(t, async (method, params, url) => {
    if (url === '/api/panels') {
      if (method === 'panels.catalog') return { sidebar: [], bottom: [] };
      if (method === 'panels.list_open') return [];
    }
    if (method === 'session.list') return [current];
    if (method === 'session.get') return current;
    if (method === 'session.events') return [];
    if (method === 'task.enqueue') {
      enqueueAttempted = true;
      enqueueParams = params;
      // Real Runtime RPC response for RuntimeError::NeedsReconciliation
      return {
        status: 502,
        error: {
          code: 'needs_reconciliation_outcome_unknown',
          message: 'session requires reconciliation before executing prompts',
          outcome_unknown: true,
        }
      };
    }
    return {};
  });

  await app.waitTick();
  // App is connected and s1 is loaded
  // Submit a prompt to s1
  app.ui.prompt.value = '新任务';
  await app.ui.prompt.dispatch('input');
  await app.ui['prompt-form'].dispatch('submit');
  await app.waitTick(200);

  assert.ok(enqueueAttempted, 'task.enqueue admission check was attempted');
  assert.equal(enqueueParams?.session_id, 's1', 'task.enqueue targeted to session s1');
  assert.equal(enqueueParams?.text, '新任务', 'task.enqueue passed prompt text');
  // Global error should show clear reconciliation message, not connection lost
  assert.ok(app.ui['global-error'].textContent.includes('工具副作用尚未核对'), 'global error displays reconciliation failure');
  assert.ok(!app.ui['global-error'].textContent.includes('连接中断或请求结果待确认'), 'must not disguise admission rejection as connection lost');

  // Blocked notice on conversation should be displayed
  assert.equal(app.ui['blocked-notice'].hidden, false, 'blocked notice displayed');
  assert.ok(app.ui['blocked-text'].textContent.includes('尚未核对'), 'blocked text displays reconciliation obligation');
  // Prompt form must NOT say "连接中断，请求回执待确认"
  assert.ok(!app.ui['prompt-status'].textContent.includes('请求回执待确认'), 'write must be rejected, not trapped in uncertain write lock');
  assert.ok(!app.ui['prompt-status'].textContent.includes('发送和 Fork 已暂停'), 'write notice not triggered by rejected prompt');
});
