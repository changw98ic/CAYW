// Conversation behavior. The page no longer has workbench panels.
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
  removeAttribute(name) { delete this[name]; }
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

async function setupApp(t, handler, { hash = '' } = {}) {
  const calls = [];
  const openedWindows = [];
  const replacedHashes = [];
  const server = createServer(async (req, res) => {
    if (req.url === '/api/settings/account') { res.writeHead(200, {'Content-Type':'application/json'}); res.end(JSON.stringify({can_manage:false,configured:false,message:'仅本机管理入口可配置账号'})); return; }
    if (req.url === '/api/auth/status') { res.writeHead(200, {'Content-Type':'application/json'}); res.end(JSON.stringify({mode:'local',authenticated:true})); return; }
    if (req.url === '/api/auth/logout') { res.writeHead(204); res.end(); return; }
    let body = '';
    for await (const chunk of req) body += chunk;
    const call = JSON.parse(body);
    calls.push(call);
    let result;
    if (call.method === 'runtime.health') {
      result = { status: 'ok', protocol_version: 4 };
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
      location: { hash },
      history: {
        replaceState(_state, _title, nextHash) {
          replacedHashes.push(nextHash);
          this.location = nextHash;
        },
      },
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
    URLSearchParams,
    fetch: (url, options) => fetch(`http://127.0.0.1:${server.address().port}${url}`, options),
    setTimeout: (fn, delay) => { const id = ++nextTimer; timers.set(id, { fn, delay }); return id; },
    clearTimeout: id => timers.delete(id),
  });

  vm.runInContext(source.replaceAll('export ', ''), context, { filename: 'app.js' });

  const WEB_TOKEN = 'test-web-token-0123456789abcdef';
  // The shipped page now connects automatically with its browser session.
  const connectedBy = Date.now() + 5000;
  while (elements['login-button'].disabled) {
    assert(Date.now() < connectedBy, 'automatic login did not settle');
    await new Promise(resolve => setTimeout(resolve, 5));
  }
  assert.equal(elements.workspace.hidden, false);

  const waitTick = (ms = 100) => new Promise(resolve => setTimeout(resolve, ms));
  const runTimers = async () => {
    for (const [id, entry] of [...timers]) {
      timers.delete(id);
      await entry.fn();
    }
  };

  return { ui: elements, calls, openedWindows, replacedHashes, WEB_TOKEN, waitTick, runTimers };
}

test('workbench_dom: launch hash opens and scopes one workspace before runtime calls', async t => {
  const workspaceId = `workspace-${'b'.repeat(64)}`;
  const workspacePath = '/tmp/cayw-hash-workspace';
  const app = await setupApp(t, async method => {
    if (method === 'settings.workspaces.list') return { workspaces: [{ id: workspaceId, path: workspacePath, state: 'registered' }] };
    if (method === 'settings.workspaces.open') return { id: workspaceId, path: workspacePath, state: 'open', reused: true };
    if (method === 'settings.workspace') return { workspace: workspacePath, known_workspaces: [workspacePath] };
    if (method === 'settings.tools') return { editor: { installed: false, install: '', start: '' }, trace: { installed: false, install: '', start: '' }, pi: { installed: false, install: '', start: '' } };
    if (method === 'session.list') return [];
    return {};
  }, { hash: `#workspace=${workspaceId}` });

  const catalog = app.calls.findIndex(call => call.method === 'settings.workspaces.list');
  const opened = app.calls.findIndex(call => call.method === 'settings.workspaces.open');
  const health = app.calls.findIndex(call => call.method === 'runtime.health');
  assert.ok(catalog >= 0 && opened > catalog && health > opened, 'workspace is registered before its first runtime call');
  assert.equal(app.calls[catalog].workspace_id, undefined);
  assert.equal(app.calls[opened].workspace_id, undefined);
  assert.equal(app.calls[health].workspace_id, workspaceId);
  assert.equal(app.calls.find(call => call.method === 'settings.workspace')?.workspace_id, workspaceId);
});

test('workbench_dom: interrupted tool history is presented cleanly and never locks the next prompt', async t => {
  const interrupted = {
    role: 'toolResult',
    tool_name: 'execute_code',
    content: [],
    is_error: true,
    outcome_unknown: true,
    structured_content: { status: 'outcome_unknown', outputs: [] },
  };
  const current = { id: 's1', label: '会话 1', cursor: 10, state: 'idle', messages: [interrupted], attempt: null, pending_approvals: [], unresolved_calls: [{ call: { id: 'call-1' }, state: 'outcome_unknown', result: { outcome_unknown: true } }] };
  let enqueueAttempted = false;

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
      return { id: 'next-task', session_id: 's1', state: 'queued' };
    }
    if (method === 'task.resume') return { ...current, cursor: 11 };
    return {};
  });

  await app.waitTick();
  assert.equal(app.ui['session-state'].textContent, '就绪', 'an interrupted tool leaves the session ready');
  assert.equal(app.ui['blocked-notice'].hidden, true, 'no reconciliation notice is shown');
  assert.ok(app.ui.messages.textContent.includes('已中断'), 'interrupted tool result is presented as interrupted');
  assert.ok(!app.ui.messages.textContent.includes('待核对') && !app.ui.messages.textContent.includes('宿主核对'), 'no host reconciliation demand');
  assert.equal(app.ui.prompt.disabled, false);

  app.ui.prompt.value = '继续下一个任务';
  await app.ui.prompt.dispatch('input');
  await app.ui['prompt-form'].dispatch('submit');
  await app.waitTick(200);
  assert.ok(enqueueAttempted, 'the next prompt is admitted immediately');
});

test('workbench_dom: stopping first and second external tasks targets each active task', async t => {
  const lease1 = { session_id: 's1', attempt_id: 'attempt-ext-1', worker_id: 'w1', fence: 1 };
  const lease2 = { session_id: 's1', attempt_id: 'attempt-ext-2', worker_id: 'w1', fence: 2 };
  let phase = 'ext1';
  let cursor = 10;
  const cancelledTargets = [];
  const tasks = {
    'ext-task-1': { id: 'ext-task-1', session_id: 's1', state: 'running', worker_id: 'w1', attempt_id: 'attempt-ext-1', fence: 1 },
    'ext-task-2': { id: 'ext-task-2', session_id: 's1', state: 'running', worker_id: 'w1', attempt_id: 'attempt-ext-2', fence: 2 },
  };

  const currentSnapshot = () => {
    if (phase === 'ext1') {
      return { id: 's1', label: '会话 1', cursor, state: 'running', messages: [], attempt: lease1, pending_approvals: [], unresolved_calls: [] };
    }
    if (phase === 'idle1') {
      return { id: 's1', label: '会话 1', cursor, state: 'idle', messages: [], attempt: null, pending_approvals: [], unresolved_calls: [] };
    }
    if (phase === 'ext2') {
      return {
        id: 's1',
        label: '会话 1',
        cursor,
        state: 'running',
        messages: [],
        attempt: lease2,
        pending_approvals: [],
        unresolved_calls: [{ call: { id: 'old-c1', task_id: 'ext-task-1' }, state: 'outcome_unknown', result: { outcome_unknown: true } }],
      };
    }
    return { id: 's1', label: '会话 1', cursor, state: 'idle', messages: [], attempt: null, pending_approvals: [], unresolved_calls: [] };
  };

  const events = [];
  const app = await setupApp(t, async (method, params, url) => {
    if (url === '/api/panels') {
      if (method === 'panels.catalog') return { sidebar: [], bottom: [] };
      if (method === 'panels.list_open') return [];
    }
    if (method === 'session.list') return [currentSnapshot()];
    if (method === 'session.get') return currentSnapshot();
    if (method === 'session.events') return events;
    if (method === 'session.tasks') {
      if (phase === 'ext1') return [tasks['ext-task-1']];
      return [tasks['ext-task-1'], tasks['ext-task-2']];
    }
    if (method === 'task.get') return tasks[params.task_id];
    if (method === 'session.cancel') {
      cancelledTargets.push(params);
      if (params.task_id === 'ext-task-1') {
        tasks['ext-task-1'] = { ...tasks['ext-task-1'], state: 'cancelled' };
        cursor = 11;
        events.push({ protocol_version: 4, session_id: 's1', cursor: 11, timestamp: 1, attempt: lease1, event: { type: 'attempt_finished', lease: lease1 } });
        phase = 'idle1';
      } else if (params.task_id === 'ext-task-2') {
        tasks['ext-task-2'] = { ...tasks['ext-task-2'], state: 'cancelled' };
        cursor = 13;
        events.push({ protocol_version: 4, session_id: 's1', cursor: 13, timestamp: 2, attempt: lease2, event: { type: 'attempt_finished', lease: lease2 } });
        phase = 'idle2';
      }
      return { cancelled: true };
    }
    return {};
  });

  await app.waitTick(150);
  assert.equal(app.ui['cancel-button'].disabled, false, 'cancel enabled for first external task');
  await app.ui['cancel-button'].dispatch('click');
  await app.waitTick(150);
  assert.equal(cancelledTargets.length, 1);
  assert.equal(cancelledTargets[0].task_id, 'ext-task-1', 'first stop targets first external task');

  // Start second external task and run the scheduled poll tick
  cursor = 12;
  events.push({ protocol_version: 4, session_id: 's1', cursor: 12, timestamp: 2, attempt: lease2, event: { type: 'attempt_started', lease: lease2 } });
  phase = 'ext2';
  await app.runTimers();
  await app.waitTick(150);

  assert.equal(app.ui['cancel-button'].disabled, false, 'cancel enabled for second external task');
  await app.ui['cancel-button'].dispatch('click');
  await app.waitTick(150);
  assert.equal(cancelledTargets.length, 2);
  assert.equal(cancelledTargets[1].task_id, 'ext-task-2', 'second stop targets second external task instead of stale ext-task-1');
  assert.equal(cancelledTargets[1].attempt_id, 'attempt-ext-2');
});

test('workbench_dom: finished local prompt settles while sibling external task runs, Stop targets external task, and poll task errors surface explicitly', async t => {
  const localLease = { session_id: 's1', attempt_id: 'attempt-local-1', worker_id: 'w1', fence: 1 };
  const extLease = { session_id: 's1', attempt_id: 'attempt-ext-9', worker_id: 'w1', fence: 2 };
  const extLease2 = { session_id: 's1', attempt_id: 'attempt-ext-10', worker_id: 'w1', fence: 3 };
  let phase = 'idle';
  let cursor = 10;
  let failTasksPoll = false;
  const cancelledTargets = [];
  const events = [];
  const tasks = {
    'local-task-1': { id: 'local-task-1', session_id: 's1', state: 'completed', worker_id: 'w1', attempt_id: 'attempt-local-1', fence: 1 },
    'ext-task-9': { id: 'ext-task-9', session_id: 's1', state: 'running', worker_id: 'w1', attempt_id: 'attempt-ext-9', fence: 2 },
    'ext-task-10': { id: 'ext-task-10', session_id: 's1', state: 'running', worker_id: 'w1', attempt_id: 'attempt-ext-10', fence: 3 },
  };

  const currentSnapshot = () => {
    if (phase === 'ext_running') {
      return { id: 's1', label: '会话 1', cursor, state: 'running', messages: [], attempt: extLease, pending_approvals: [], unresolved_calls: [] };
    }
    if (phase === 'ext2_running') {
      return { id: 's1', label: '会话 1', cursor, state: 'running', messages: [], attempt: extLease2, pending_approvals: [], unresolved_calls: [] };
    }
    return { id: 's1', label: '会话 1', cursor, state: 'idle', messages: [], attempt: null, pending_approvals: [], unresolved_calls: [] };
  };

  const app = await setupApp(t, async (method, params, url) => {
    if (url === '/api/panels') {
      if (method === 'panels.catalog') return { sidebar: [], bottom: [] };
      if (method === 'panels.list_open') return [];
    }
    if (method === 'session.list') return [currentSnapshot()];
    if (method === 'session.get') return currentSnapshot();
    if (method === 'session.events') return events;
    if (method === 'task.enqueue') {
      return { id: 'local-task-1', session_id: 's1', state: 'queued' };
    }
    if (method === 'task.resume') {
      // Local task finishes at cursor 12, and sibling external task immediately takes over at cursor 13!
      events.push(
        { protocol_version: 4, session_id: 's1', cursor: 11, timestamp: 1, attempt: localLease, event: { type: 'attempt_started', lease: localLease } },
        { protocol_version: 4, session_id: 's1', cursor: 12, timestamp: 2, attempt: localLease, event: { type: 'attempt_finished', lease: localLease } },
        { protocol_version: 4, session_id: 's1', cursor: 13, timestamp: 3, attempt: extLease, event: { type: 'attempt_started', lease: extLease } },
      );
      cursor = 13;
      phase = 'ext_running';
      return { id: 's1', label: '会话 1', cursor: 12, state: 'idle', messages: [], attempt: null, pending_approvals: [], unresolved_calls: [] };
    }
    if (method === 'session.tasks') {
      if (failTasksPoll) {
        return { status: 502, error: { code: 'bad_gateway', message: 'session.tasks backend unavailable', outcome_unknown: false } };
      }
      if (phase === 'ext2_running') {
        return [tasks['local-task-1'], { ...tasks['ext-task-9'], state: 'completed' }, tasks['ext-task-10']];
      }
      return [tasks['local-task-1'], tasks['ext-task-9']];
    }
    if (method === 'task.get') return tasks[params.task_id];
    if (method === 'session.cancel') {
      cancelledTargets.push(params);
      tasks[params.task_id] = { ...tasks[params.task_id], state: 'cancelled' };
      cursor += 1;
      const doneLease = params.task_id === 'ext-task-10' ? extLease2 : extLease;
      events.push({ protocol_version: 4, session_id: 's1', cursor, timestamp: 4, attempt: doneLease, event: { type: 'attempt_finished', lease: doneLease } });
      phase = 'idle';
      return { cancelled: true };
    }
    return {};
  });

  await app.waitTick(120);
  app.ui.prompt.value = '本地任务完成后外部任务无缝接力';
  await app.ui.prompt.dispatch('input');
  await app.ui['prompt-form'].dispatch('submit');
  await app.waitTick(150);
  await app.runTimers();
  await app.waitTick(150);

  assert.equal(app.ui['send-button'].textContent, '发送', 'local promptPending cleared once its own task.resume completed');
  assert.equal(app.ui['cancel-button'].disabled, false, 'cancel stays enabled for concurrent external task');

  // Transition to a new external attempt while session.tasks is failing -> poll must surface partial unavailability instead of pretending 已同步
  failTasksPoll = true;
  cursor = 15;
  events.push(
    { protocol_version: 4, session_id: 's1', cursor: 14, timestamp: 4, attempt: extLease, event: { type: 'attempt_finished', lease: extLease } },
    { protocol_version: 4, session_id: 's1', cursor: 15, timestamp: 5, attempt: extLease2, event: { type: 'attempt_started', lease: extLease2 } },
  );
  phase = 'ext2_running';
  await app.runTimers();
  await app.waitTick(150);
  assert.match(app.ui['poll-status'].textContent, /部分状态暂不可用.*session\.tasks backend unavailable/);
  assert.match(app.ui['global-error'].textContent, /session\.tasks backend unavailable/);

  // Restore session.tasks and click Stop -> must target ext-task-10, never stale local-task-1 or ext-task-9!
  failTasksPoll = false;
  await app.ui['cancel-button'].dispatch('click');
  await app.waitTick(150);
  assert.equal(cancelledTargets.length, 1);
  assert.equal(cancelledTargets[0].task_id, 'ext-task-10', 'Stop targets running external task rather than completed local-task-1');
  assert.equal(cancelledTargets[0].attempt_id, 'attempt-ext-10');
});
