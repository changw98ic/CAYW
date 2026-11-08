// Runs the shipped app against real HTTP/fetch, with a small DOM and clock.
// No npm dependencies. The fixture uses the Core event/snapshot envelopes.
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { createServer } from 'node:http';
import { once } from 'node:events';
import vm from 'node:vm';

const source = await readFile(new URL('../assets/app.js', import.meta.url), 'utf8');
const html = await readFile(new URL('../assets/index.html', import.meta.url), 'utf8');
const lease = (attempt = 'attempt-1') => ({ session_id: 's1', attempt_id: attempt, worker_id: 'worker-1', fence: 1 });
const snapshot = (extra = {}) => ({ id: 's1', label: '会话', messages: [], cursor: 10, state: 'idle', attempt: null, pending_approvals: [], unresolved_calls: [], ...extra });
const record = (cursor, event, attempt = lease()) => ({ protocol_version: 3, session_id: 's1', cursor, timestamp: 1, attempt, event });
const failure = (code, outcome_unknown = false) => ({ status: 502, error: { code, message: 'internal diagnostic is not the main user message', outcome_unknown } });
const continuationProposal = () => ({
  expected: [{ identity: { request_id: 'request-1' }, reservation_tokens: 128000 }],
  unknown_request_count: 1,
  additional_tokens: 128000,
  token_limit: 256000,
});

test('shipped HTML pattern attributes compile with browser Unicode Sets rules', () => {
  const patterns = [...html.matchAll(/\spattern="([^"]*)"/g)].map(match => match[1]);
  assert.ok(patterns.length > 0);
  for (const pattern of patterns) {
    assert.doesNotThrow(() => new RegExp(pattern, 'v'), `invalid pattern attribute: ${pattern}`);
  }
});

test('model credential edits preserve by default and require an explicit clear choice', () => {
  assert.match(html, /name="clear_api_key" type="checkbox"/);
  assert.match(source, /model\.authentication === 'plain_file'/);
  assert.match(source, /clear_api_key:clearApiKey/);
  assert.match(source, /留空表示保持不变/);
});

test('unknown model usage requires an explicit finite continuation confirmation', () => {
  assert.match(html, /id="model-budget-dialog"/);
  assert.match(html, /不会清除原预留，也不会把未知费用记为零/);
  assert.match(source, /'model\.budget\.status'/);
  assert.match(source, /'model\.budget\.continue'/);
  assert.match(source, /confirm: true/);
});

class Element {
  constructor(tag = 'div') { this.tagName = tag; this.children = []; this.listeners = new Map(); this.dataset = {}; this.disabled = false; this.hidden = false; this.value = ''; this.checked = false; this.ownText = ''; }
  set textContent(text) { this.ownText = String(text); this.children = []; }
  get textContent() { return this.ownText + this.children.map(child => child.textContent).join(''); }
  append(...children) { this.children.push(...children.flatMap(child => child.tagName === '#fragment' ? child.children : [child])); }
  replaceChildren(...children) { this.ownText = ''; this.children = []; this.append(...children); }
  get classList() { return { add: (...names) => { this.className = [...new Set([...(this.className ?? '').split(/\s+/).filter(Boolean), ...names])].join(' '); } }; }
  setAttribute(name, value) { this[name] = value; }
  addEventListener(name, fn) { const list = this.listeners.get(name) ?? []; list.push(fn); this.listeners.set(name, list); }
  async dispatch(name) { for (const fn of this.listeners.get(name) ?? []) await fn({ preventDefault() {} }); }
  showModal() { this.open = true; }
  close() { this.open = false; }
  focus() {}
}

async function page(t, handler) {
  const calls = [];
  const server = createServer(async (req, res) => {
    if (req.url === '/api/settings/account') { res.writeHead(200, {'Content-Type':'application/json'}); res.end(JSON.stringify({can_manage:false,configured:false,message:'仅本机管理入口可配置账号'})); return; }
    if (req.url === '/api/auth/status') { res.writeHead(200, {'Content-Type':'application/json'}); res.end(JSON.stringify({mode:'local',authenticated:true})); return; }
    if (req.url === '/api/auth/logout') { res.writeHead(204); res.end(); return; }
    let body = '';
    for await (const chunk of req) body += chunk;
    const call = JSON.parse(body);
    calls.push(call);
    let result;
    if (call.method === 'model.budget.status') {
      result = await handler(call.method, call.params) ?? { budget_unknown: false, continuation: null };
    } else if (call.method === 'task.enqueue') {
      result = await handler('task.enqueue', call.params) ?? { id: 'original-task', session_id: call.params.session_id, state: 'queued', worker_id: null, attempt_id: null, fence: 0, dispatches: 0, holds_slot: false, cancel_requested: false };
    } else if (call.method === 'task.resume') result = await handler('session.prompt', call.params);
    else if (call.method === 'task.get') result = await handler('task.get', call.params) ?? { id: 'original-task', session_id: 's1', state: 'running', worker_id: 'worker-1', attempt_id: 'attempt-1', fence: 7, dispatches: 1, holds_slot: true, cancel_requested: false };
    else result = await handler(call.method, call.params);
    if (result?.disconnect) { req.socket.destroy(); return; }
    if (call.method === 'runtime.health') result = { status: 'ok', protocol_version: 3 };
    const error = result?.error;
    res.writeHead(error ? result.status : 200, { 'Content-Type': 'application/json' });
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
    document: { getElementById: id => elements[id], createElement: tag => new Element(tag), createDocumentFragment: () => new Element('#fragment'), addEventListener() {}, hidden: false },
    window: { addEventListener() {}, matchMedia: () => ({ matches: false }) }, AbortController, TypeError, URLSearchParams,
    fetch: (url, options) => fetch(`http://127.0.0.1:${server.address().port}${url}`, options),
    setTimeout: (fn, delay) => { const id = ++nextTimer; timers.set(id, { fn, delay }); return id; },
    clearTimeout: id => timers.delete(id),
  });
  vm.runInContext(source.replaceAll('export ', ''), context, { filename: 'app.js' });
  // The shipped page now connects automatically with its browser session.
  const connectedBy = Date.now() + 5000;
  while (elements['login-button'].disabled) {
    assert(Date.now() < connectedBy, 'automatic login did not settle');
    await new Promise(resolve => setTimeout(resolve, 5));
  }
  assert.equal(elements['login-error'].textContent, '', 'fixture must connect to the real fetch endpoint');
  assert.equal(elements.workspace.hidden, false);
  const poll = async () => {
    const entry = [...timers].find(([, timer]) => timer.delay < 15000);
    assert.ok(entry, 'a session poll must be scheduled');
    timers.delete(entry[0]);
    await entry[1].fn();
  };
  const send = async () => {
    elements.prompt.value = '执行任务';
    await elements.prompt.dispatch('input');
    await elements['prompt-form'].dispatch('submit');
  };
  return { ui: elements, calls, poll, send };
}

async function startConfirmedBudgetSend(app) {
  const sending = app.send();
  const openedBy = Date.now() + 1000;
  while (!app.ui['model-budget-dialog'].open) {
    assert(Date.now() < openedBy, 'budget confirmation dialog did not open');
    await new Promise(resolve => setTimeout(resolve, 1));
  }
  app.ui['model-budget-confirm'].checked = true;
  await app.ui['model-budget-form'].dispatch('submit');
  return { sending };
}

// Regression: finally deleting promptPending after an unknown response allowed
// a second prompt against the same old idle snapshot.
test('unknown prompt stays locked through old idle, unrelated terminal and uncovered snapshot', async t => {
  let current = snapshot();
  let events = [];
  const app = await page(t, async (method, params) => {
    if (method === 'session.list') return [current];
    if (method === 'session.get') return current;
    if (method === 'session.events') return events.filter(event => event.cursor > params.after);
    if (method === 'session.prompt') return failure('unknown_timeout', true);
  });
  await app.send();
  assert.equal(app.ui.prompt.disabled, true);
  assert.match(app.ui['prompt-status'].textContent, /连接|结果.*确认|待确认/);
  await app.poll();
  assert.equal(app.ui.prompt.disabled, true, 'same-cursor idle is not settlement');
  current = snapshot({ cursor: 12, state: 'running', attempt: lease() });
  events = [record(11, { type: 'attempt_started', lease: lease() }), record(12, { type: 'agent', event: { type: 'agent_end', messages: [] } })];
  await app.poll();
  assert.equal(app.ui.prompt.disabled, true, 'agent_end does not finish an attempt');
  current = snapshot({ cursor: 14 });
  events.push(record(13, { type: 'attempt_finished', lease: lease('another-attempt') }, lease('another-attempt')));
  await app.poll();
  assert.equal(app.ui.prompt.disabled, true, 'unrelated terminal must not release the lock');
  events.push(record(15, { type: 'attempt_finished', lease: lease() }));
  await app.poll();
  assert.equal(app.ui.prompt.disabled, true, 'snapshot before the consumed terminal cannot release the lock');
  current = snapshot({ cursor: 15 });
  await app.poll();
  assert.equal(app.ui.prompt.disabled, true, 'no attempt fact correlates an unknown prompt to its request');
  assert.equal(app.calls.filter(call => call.method === 'task.resume').length, 1);
});

test('known model_not_configured admission failure keeps the session usable', async t => {
  const current = snapshot();
  let admissions = 0;
  const app = await page(t, async method => {
    if (method === 'session.list') return [current];
    if (method === 'session.get') return current;
    if (method === 'session.events') return [];
    if (method === 'task.enqueue') {
      admissions++;
      return failure('model_not_configured', false);
    }
  });
  await app.send();
  assert.equal(app.ui.prompt.disabled, false);
  assert.doesNotMatch(app.ui['blocked-text'].textContent, /待确认|核对/);
  assert.equal(app.calls.filter(call => call.method === 'task.resume').length, 0);
  await app.send();
  assert.equal(admissions, 2, 'a known failure must not strand the session');
});

for (const code of ['busy', 'queue_full']) {
  test(`${code} before execution preserves the original prompt draft`, async t => {
    const current = snapshot();
    const app = await page(t, async method => {
      if (method === 'session.list') return [current];
      if (method === 'session.get') return current;
      if (method === 'session.events') return [];
      if (method === 'task.enqueue') return failure(code, false);
    });
    await app.send();
    assert.equal(app.ui.prompt.value, '执行任务');
    assert.equal(app.ui.prompt.disabled, false);
    assert.equal(app.calls.filter(call => call.method === 'task.resume').length, 0);
  });
}

test('unknown model usage is shown before admission and continues only after confirmation', async t => {
  const current = snapshot();
  const proposal = {
    expected: [{ identity: { request_id: 'request-1' }, reservation_tokens: 128000 }],
    unknown_request_count: 1,
    additional_tokens: 128000,
    token_limit: 256000,
  };
  const app = await page(t, async method => {
    if (method === 'session.list') return [current];
    if (method === 'session.get') return current;
    if (method === 'session.events') return [];
    if (method === 'model.budget.status') return { budget_unknown: true, reserved_tokens: 128000, continuation: proposal };
    if (method === 'model.budget.continue') return { continuation: { actor: 'rpc-user' } };
    if (method === 'session.prompt') return snapshot({ cursor: 13 });
  });

  const sending = app.send();
  const openedBy = Date.now() + 1000;
  while (!app.ui['model-budget-dialog'].open) {
    assert(Date.now() < openedBy, 'budget confirmation dialog did not open');
    await new Promise(resolve => setTimeout(resolve, 1));
  }
  assert.equal(app.calls.filter(call => call.method === 'task.enqueue').length, 0, 'review happens before creating a task');
  assert.equal(app.ui.prompt.value, '执行任务', 'draft remains until confirmation');
  assert.match(app.ui['model-budget-summary'].textContent, /128000.*256000/);

  await app.ui['model-budget-form'].dispatch('submit');
  assert.match(app.ui['model-budget-error'].textContent, /确认/);
  assert.equal(app.calls.filter(call => call.method === 'task.enqueue').length, 0);

  app.ui['model-budget-confirm'].checked = true;
  await app.ui['model-budget-form'].dispatch('submit');
  await sending;
  const methods = app.calls.map(call => call.method);
  assert.ok(methods.indexOf('model.budget.status') < methods.indexOf('task.enqueue'));
  assert.ok(methods.indexOf('task.enqueue') < methods.indexOf('model.budget.continue'));
  assert.ok(methods.indexOf('model.budget.continue') < methods.indexOf('task.resume'));
  const continuation = app.calls.find(call => call.method === 'model.budget.continue');
  assert.deepEqual(continuation.params, {
    session_id: 's1',
    task_id: 'original-task',
    expected: proposal.expected,
    additional_tokens: 128000,
    token_limit: 256000,
    confirm: true,
  });
  assert.equal(app.ui.prompt.value, '', 'a successful unchanged draft is cleared once');
});

test('closing model budget confirmation keeps the draft and creates no task', async t => {
  const current = snapshot();
  const app = await page(t, async method => {
    if (method === 'session.list') return [current];
    if (method === 'session.get') return current;
    if (method === 'session.events') return [];
    if (method === 'model.budget.status') return {
      budget_unknown: true,
      reserved_tokens: 128000,
      continuation: { expected: [{}], unknown_request_count: 1, additional_tokens: 128000, token_limit: 256000 },
    };
  });
  const sending = app.send();
  const openedBy = Date.now() + 1000;
  while (!app.ui['model-budget-dialog'].open) {
    assert(Date.now() < openedBy, 'budget confirmation dialog did not open');
    await new Promise(resolve => setTimeout(resolve, 1));
  }
  await app.ui['close-model-budget'].dispatch('click');
  await sending;
  assert.equal(app.ui.prompt.value, '执行任务');
  assert.equal(app.ui.prompt.disabled, false);
  assert.equal(app.calls.filter(call => call.method === 'task.enqueue').length, 0);
});

test('failed budget grant unlocks only after exact queued task cancellation', async t => {
  const current = snapshot();
  const proposal = continuationProposal();
  const queued = { id: 'original-task', session_id: 's1', state: 'queued', worker_id: null, attempt_id: null, fence: 0, dispatches: 0, holds_slot: false, cancel_requested: false };
  const app = await page(t, async (method, params) => {
    if (method === 'session.list') return [current];
    if (method === 'session.get') return current;
    if (method === 'session.events') return [];
    if (method === 'model.budget.status') return { budget_unknown: true, reserved_tokens: 128000, continuation: proposal };
    if (method === 'model.budget.continue') return failure('invalid_params', false);
    if (method === 'task.get') return queued;
    if (method === 'session.cancel') {
      assert.deepEqual(params, { session_id: 's1', task_id: 'original-task', worker_id: null, attempt_id: null, fence: 0 });
      return { cancelled: true };
    }
  });
  const { sending } = await startConfirmedBudgetSend(app);
  await sending;
  assert.equal(app.calls.filter(call => call.method === 'task.resume').length, 0);
  assert.equal(app.calls.filter(call => call.method === 'session.cancel').length, 1);
  assert.equal(app.ui.prompt.value, '执行任务');
  assert.equal(app.ui.prompt.disabled, false, 'proved non-execution must release the local uncertain lock');
  assert.match(app.ui['global-error'].textContent, /模型请求未开始/);
});

test('failed budget grant does not cancel or unlock a running task', async t => {
  const current = snapshot();
  const proposal = continuationProposal();
  const running = { id: 'original-task', session_id: 's1', state: 'running', worker_id: 'worker-1', attempt_id: 'attempt-1', fence: 7, dispatches: 1, holds_slot: true, cancel_requested: false };
  const app = await page(t, async method => {
    if (method === 'session.list') return [current];
    if (method === 'session.get') return current;
    if (method === 'session.events') return [];
    if (method === 'model.budget.status') return { budget_unknown: true, reserved_tokens: 128000, continuation: proposal };
    if (method === 'model.budget.continue') return failure('invalid_params', false);
    if (method === 'task.get') return running;
  });
  const { sending } = await startConfirmedBudgetSend(app);
  await sending;
  assert.equal(app.calls.filter(call => call.method === 'session.cancel').length, 0);
  assert.equal(app.calls.filter(call => call.method === 'task.resume').length, 0);
  assert.equal(app.ui.prompt.value, '执行任务');
  assert.equal(app.ui.prompt.disabled, true, 'a possibly claimed task must retain the uncertain lock');
  assert.match(app.ui['blocked-text'].textContent, /核对|确认/);
});

test('stale queued-task cancellation after budget failure remains unknown', async t => {
  const current = snapshot();
  const proposal = continuationProposal();
  const queued = { id: 'original-task', session_id: 's1', state: 'queued', worker_id: null, attempt_id: null, fence: 0, dispatches: 0, holds_slot: false, cancel_requested: false };
  const app = await page(t, async method => {
    if (method === 'session.list') return [current];
    if (method === 'session.get') return current;
    if (method === 'session.events') return [];
    if (method === 'model.budget.status') return { budget_unknown: true, reserved_tokens: 128000, continuation: proposal };
    if (method === 'model.budget.continue') return failure('invalid_params', false);
    if (method === 'task.get') return queued;
    if (method === 'session.cancel') return failure('stale_task', false);
  });
  const { sending } = await startConfirmedBudgetSend(app);
  await sending;
  assert.equal(app.calls.filter(call => call.method === 'session.cancel').length, 1);
  assert.equal(app.calls.filter(call => call.method === 'task.resume').length, 0);
  assert.equal(app.ui.prompt.value, '执行任务');
  assert.equal(app.ui.prompt.disabled, true, 'a read-then-claim race must remain unknown');
  assert.match(app.ui['blocked-text'].textContent, /核对|确认/);
});

test('successful prompt response waits for consumed durable terminal before next send', async t => {
  let current = snapshot();
  let events = [];
  const app = await page(t, async (method, params) => {
    if (method === 'session.list') return [current];
    if (method === 'session.get') return current;
    if (method === 'session.events') return events.filter(event => event.cursor > params.after);
    if (method === 'session.prompt') return snapshot({ cursor: 13 });
  });
  await app.send();
  assert.equal(app.ui.prompt.disabled, true);
  current = snapshot({ cursor: 13 });
  events = [record(11, { type: 'attempt_started', lease: lease() }), record(12, { type: 'agent', event: { type: 'agent_end', messages: [] } }), record(13, { type: 'attempt_finished', lease: lease() })];
  await app.poll();
  assert.equal(app.ui.prompt.disabled, false);
});

test('an older successful prompt never clears a newer editor revision', async t => {
  const current = snapshot();
  let release;
  let entered;
  const started = new Promise(resolve => { entered = resolve; });
  const app = await page(t, async method => {
    if (method === 'session.list') return [current];
    if (method === 'session.get') return current;
    if (method === 'session.events') return [];
    if (method === 'session.prompt') {
      entered();
      return new Promise(resolve => { release = resolve; });
    }
  });
  const sending = app.send();
  await started;
  app.ui.prompt.value = '等待期间的新草稿';
  await app.ui.prompt.dispatch('input');
  release(snapshot({ cursor: 13 }));
  await sending;
  assert.equal(app.ui.prompt.value, '等待期间的新草稿');
});

test('session_frozen error survives bare Core idle snapshots and offers explicit confirmed fork', async t => {
  const current = snapshot(); // Actual CoreSnapshot has no frozen/binding field.
  const app = await page(t, async method => {
    if (method === 'session.list') return [current];
    if (method === 'session.get') return current;
    if (method === 'session.events') return [];
    if (method === 'session.prompt') return failure('session_frozen');
    if (method === 'session.fork') return snapshot({ id: 's2', label: '普通聊天', cursor: 2 });
  });
  await app.send();
  await app.poll();
  assert.equal(app.ui.prompt.disabled, true);
  assert.match(app.ui['blocked-text'].textContent, /冻结/);
  assert.equal(app.ui['open-fork'].hidden, false);
  assert.equal(app.ui['open-fork'].disabled, false);
  await app.ui['open-fork'].dispatch('click');
  await app.ui['fork-form'].dispatch('submit');
  assert.equal(app.calls.filter(call => call.method === 'session.fork').length, 0);
  app.ui['fork-confirm'].checked = true;
  await app.ui['fork-form'].dispatch('submit');
  const fork = app.calls.find(call => call.method === 'session.fork');
  assert.deepEqual(fork.params, { session_id: 's1', label: '会话 · 普通聊天', confirm: true, mode: 'chat' });
});

test('needs_reconciliation error cannot be cleared by an idle refresh or bypassed with fork', async t => {
  const current = snapshot({ binding: { plugin: 'strongflow', frozen: true } });
  let initialRead = true;
  const app = await page(t, async method => {
    if (method === 'session.list') return [snapshot()];
    if (method === 'session.get') { if (initialRead) { initialRead = false; return snapshot(); } return current; }
    if (method === 'session.events') return [];
    if (method === 'session.prompt') return failure('needs_reconciliation');
  });
  await app.send();
  await app.poll();
  assert.match(app.ui['blocked-text'].textContent, /核对.*副作用|副作用.*核对/);
  assert.equal(app.ui.prompt.disabled, true);
  assert.equal(app.ui['open-fork'].hidden || app.ui['open-fork'].disabled, true);
});

test('binding metadata and plain approval scope are consumed without showing system instructions', async t => {
  const approval = { id: 'approval-1', session_id: 's1', attempt_id: 'attempt-1', requested_at: 1, decision: null, input: { task_id: 'task-1', call_id: 'call-1', capability: 'fs.write', scope: '{"path":"notes.txt","content":"hello"}', reason: 'opaque internal implementation reason' } };
  const current = snapshot({ cursor: 12, state: 'awaiting_approval', attempt: lease(), pending_approvals: [approval], messages: [{ role: 'system', content: 'private system instruction' }, { role: 'system', content: [] }, { role: 'user', content: '更新笔记' }] });
  const app = await page(t, async method => {
    if (method === 'session.list') return [current];
    if (method === 'session.get') return current;
    if (method === 'session.events') return [];
  });
  assert.doesNotMatch(app.ui.messages.textContent, /private system instruction|系统/);
  assert.equal(app.ui.messages.children.length, 1);
  const card = app.ui.approvals.children[0];
  const primary = card.children.filter(child => child.tagName !== 'details').map(child => child.textContent).join(' ');
  assert.match(primary, /写入|修改/);
  assert.match(primary, /notes\.txt/);
  assert.match(primary, /范围/);
  assert.doesNotMatch(primary, /opaque internal|task-1|call-1|attempt-1/);
  assert.ok(card.children.some(child => child.tagName === 'details'), 'raw approval belongs in closed details');
});

test('frozen Runtime binding locks both prompt and fork until the active attempt settles', async t => {
  let current = snapshot({ state:'running', attempt:lease(), binding:{plugin:'strongflow',generation:4,frozen:true} });
  const app = await page(t, async method => {
    if (method === 'session.list') return [current];
    if (method === 'session.get') return current;
    if (method === 'session.events') return [];
  });
  assert.equal(app.ui.prompt.disabled,true);
  assert.equal(app.ui['open-fork'].hidden,false);
  assert.equal(app.ui['open-fork'].disabled,true);
  current = snapshot({cursor:11,binding:{plugin:'strongflow',generation:4,frozen:true}});
  await app.poll();
  assert.equal(app.ui['open-fork'].disabled,true, 'active attempt needs its durable terminal, not just a newer idle');
});

test('a dropped HTTP write retains an uncertain lock even after disconnect and reconnect', async t => {
  const current = snapshot();
  const app = await page(t, async method => {
    if (method === 'session.list') return [current];
    if (method === 'session.get') return current;
    if (method === 'session.events') return [];
    if (method === 'session.prompt') return { disconnect:true };
  });
  await app.send();
  assert.equal(app.ui.prompt.disabled,true);
  assert.match(app.ui['prompt-status'].textContent,/确认/);
  await app.ui.logout.dispatch('click');
  await app.ui['login-form'].dispatch('submit');
  assert.equal(app.ui.prompt.disabled,true,'reauthentication is not settlement');
  await app.poll();
  assert.equal(app.ui.prompt.disabled,true);
  assert.equal(app.calls.filter(call=>call.method==='task.resume').length,1);
});

test('cancel response is not execution settlement and cannot unlock a pending prompt', async t => {
  let current = snapshot();
  let events = [];
  let resolvePrompt;
  let started;
  const promptStarted = new Promise(resolve=> { started=resolve; });
  const app = await page(t, async (method, params) => {
    if (method === 'session.list') return [current];
    if (method === 'session.get') return current;
    if (method === 'session.events') return events.filter(event=>event.cursor>params.after);
    if (method === 'session.prompt') { started(); return await new Promise(resolve=>{resolvePrompt=resolve;}); }
    if (method === 'session.cancel') {
      assert.deepEqual(params, {session_id:'s1',task_id:'original-task',worker_id:'worker-1',attempt_id:'attempt-1',fence:7});
      return {cancelled:true};
    }
  });
  const pending = app.send();
  await promptStarted;
  current = snapshot({cursor:11,state:'running',attempt:lease()});
  events = [record(11,{type:'attempt_started',lease:lease()})];
  await app.poll();
  await app.ui['cancel-button'].dispatch('click');
  assert.equal(app.ui.prompt.disabled,true);
  current = snapshot({cursor:12});
  events.push(record(12,{type:'attempt_finished',lease:lease()}));
  resolvePrompt(failure('cancelled'));
  await pending;
  await app.poll();
  assert.equal(app.ui.prompt.disabled,false);
});

test('an incomplete fork receipt cannot enable a second fork', async t => {
  const current=snapshot({binding:{plugin:'strongflow',generation:1,frozen:true}});
  const app=await page(t,async method=>{
    if(method==='session.list') return [current];
    if(method==='session.get') return current;
    if(method==='session.events') return [];
    if(method==='session.fork') return {id:'s2'};
  });
  await app.ui['open-fork'].dispatch('click');
  app.ui['fork-confirm'].checked=true;
  await app.ui['fork-form'].dispatch('submit');
  await app.ui['fork-form'].dispatch('submit');
  assert.equal(app.calls.filter(call=>call.method==='session.fork').length,1);
  assert.equal(app.ui['open-fork'].disabled,true);
});

test('two approvals use their own consumed decision facts while a prompt is still pending', async t => {
  const approval = id => ({id,session_id:'s1',attempt_id:'attempt-1',requested_at:1,decision:null,input:{task_id:'task-1',call_id:'call-1',capability:'fs.write',scope:'{"path":"notes.txt","content":"hello"}',reason:'internal'}});
  let current = snapshot();
  let events = [];
  let started;
  let resolvePrompt;
  const promptStarted = new Promise(resolve=>{started=resolve;});
  const app = await page(t,async(method,params)=>{
    if(method==='session.list') return [current];
    if(method==='session.get') return current;
    if(method==='session.events') return events.filter(event=>event.cursor>params.after);
    if(method==='session.prompt') {started();return await new Promise(resolve=>{resolvePrompt=resolve;});}
    if(method==='approval.decide') {
      const cursor=params.approval_id==='p1'?13:15;
      const decided={...approval(params.approval_id),decision:{decision:'approve',actor:'rpc-user',decided_at:2}};
      events.push(record(cursor,{type:'approval_decided',approval:decided}));
      current=snapshot({cursor:cursor+1,state:params.approval_id==='p1'?'awaiting_approval':'running',attempt:lease(),pending_approvals:params.approval_id==='p1'?[approval('p2')]:[]});
      return decided;
    }
  });
  const pending=app.send();
  await promptStarted;
  events=[record(11,{type:'attempt_started',lease:lease()}),record(12,{type:'approval_requested',approval:approval('p1')})];
  current=snapshot({cursor:12,state:'awaiting_approval',attempt:lease(),pending_approvals:[approval('p1')]});
  await app.poll();
  const first=app.ui.approvals.children[0].children.at(-1).children[0];
  assert.equal(first.disabled,false);
  await first.dispatch('click');
  await app.poll();
  const second=app.ui.approvals.children[0].children.at(-1).children[0];
  assert.equal(second.disabled,false);
  await second.dispatch('click');
  current=snapshot({cursor:17});
  events.push(record(17,{type:'attempt_finished',lease:lease()}));
  resolvePrompt(current);
  await pending;
  await app.poll();
  assert.equal(app.ui.prompt.disabled,false);
  assert.deepEqual(app.calls.filter(call=>call.method==='approval.decide').map(call=>call.params.approval_id),['p1','p2']);
});

test('a lost prompt response cannot borrow the first execution from another client', async t => {
  let current = snapshot();
  let events = [];
  const app = await page(t, async (method, params) => {
    if (method === 'session.list') return [current];
    if (method === 'session.get') return current;
    if (method === 'session.events') return events.filter(event => event.cursor > params.after);
    if (method === 'session.prompt') return { disconnect: true };
  });
  await app.send();
  const other = lease('other-client');
  events = [record(11, {type:'attempt_started', lease:other}, other), record(12, {type:'attempt_finished', lease:other}, other)];
  current = snapshot({cursor:12, messages:[{role:'user', content:'另一位客户端的任务'}, {role:'assistant', content:'已完成另一项任务'}]});
  await app.poll();
  await app.poll();
  assert.equal(app.ui.prompt.disabled, true, 'a later execution has no authority over this lost request');
  assert.match(app.ui['blocked-text'].textContent, /回执|请求编号/);
  assert.match(app.ui['blocked-text'].textContent, /宿主.*核对/);
  assert.match(app.ui['blocked-text'].textContent, /事件记录/);
  assert.match(app.ui['blocked-text'].textContent, /新建会话/);
  assert.equal(app.ui['create-button'].disabled, false, 'independent work still has a usable route');
  assert.equal(app.calls.filter(call => call.method === 'task.resume').length, 1);
});

test('a no-op cancel after the terminal cannot strand a delayed successful prompt receipt', async t => {
  let current = snapshot();
  let events = [];
  let release;
  let started;
  const entered = new Promise(resolve => { started = resolve; });
  const app = await page(t, async (method, params) => {
    if (method === 'session.list') return [current];
    if (method === 'session.get') return current;
    if (method === 'session.events') return events.filter(event => event.cursor > params.after);
    if (method === 'session.prompt') { started(); return new Promise(resolve => { release = resolve; }); }
    if (method === 'session.cancel') return {cancelled:false};
  });
  const pending = app.send();
  await entered;
  events = [record(11, {type:'attempt_started', lease:lease()}), record(12, {type:'attempt_finished', lease:lease()})];
  current = snapshot({cursor:12});
  await app.poll();
  assert.equal(app.ui['cancel-button'].disabled, false);
  await app.ui['cancel-button'].dispatch('click');
  assert.equal(app.ui.prompt.disabled, true, 'the original HTTP operation has not returned');
  release(current);
  await pending;
  await app.poll();
  assert.equal(app.ui.prompt.disabled, false, 'cancelled:false must not demand a new execution terminal');
  assert.equal(app.ui['cancel-button'].disabled, true);
  assert.equal(app.calls.filter(call => call.method === 'session.cancel').length, 1);
  assert.equal(app.calls.filter(call => call.method === 'task.resume').length, 1);
});

for (const order of ['receipt-first', 'events-first', 'cancel-receipt-last']) {
  test(`stop before the first attempt event settles with ${order}`, async t => {
    let current = snapshot();
    let events = [];
    let releasePrompt;
    let releaseCancel;
    let started;
    let cancelStarted;
    const entered = new Promise(resolve => { started = resolve; });
    const cancelEntered = new Promise(resolve => { cancelStarted = resolve; });
    const app = await page(t, async (method, params) => {
      if (method === 'session.list') return [current];
      if (method === 'session.get') return current;
      if (method === 'session.events') return events.filter(event => event.cursor > params.after);
      if (method === 'session.prompt') { started(); return new Promise(resolve => { releasePrompt = resolve; }); }
      if (method === 'session.cancel') { cancelStarted(); return new Promise(resolve => { releaseCancel = resolve; }); }
    });
    const pending = app.send();
    await entered;
    assert.equal(app.ui['cancel-button'].disabled, false);
    const cancelling = app.ui['cancel-button'].dispatch('click');
    await cancelEntered;
    if (order !== 'cancel-receipt-last') { releaseCancel({cancelled:true}); await cancelling; }
    const complete = snapshot({cursor:12});
    if (order === 'receipt-first') { releasePrompt(complete); await pending; }
    assert.equal(app.ui.prompt.disabled, true, 'no terminal has reached the browser');
    events = [record(11, {type:'attempt_started', lease:lease()}), record(12, {type:'attempt_finished', lease:lease()})];
    current = complete;
    await app.poll();
    if (order !== 'receipt-first') { releasePrompt(complete); await pending; }
    if (order === 'cancel-receipt-last') {
      assert.equal(app.ui.prompt.disabled, true, 'pending cancel still needs its own receipt');
      releaseCancel({cancelled:true});
      await cancelling;
    }
    await app.poll();
    assert.equal(app.ui.prompt.disabled, false, 'known early cancel must settle on the original request completion');
    assert.equal(app.ui['cancel-button'].disabled, true);
    current = {...complete, binding:{plugin:'strongflow', frozen:true}};
    await app.poll();
    assert.equal(app.ui['open-fork'].disabled, false, 'the settled cancel must not strand a later frozen session');
    assert.equal(app.calls.filter(call => call.method === 'session.cancel').length, 1);
    assert.equal(app.calls.filter(call => call.method === 'task.resume').length, 1);
  });
}

// A receipt can arrive after arbitrarily many other clients have completed.
// Exercise the shipped app through public HTTP; historical reads honor limit.
async function delayedEarlyCancel(t, otherCount = 121, replay = records => records) {
  let current = snapshot();
  let events = [];
  let releasePrompt;
  let entered;
  const started = new Promise(resolve => { entered = resolve; });
  const app = await page(t, async (method, params) => {
    if (method === 'session.list') return [current];
    if (method === 'session.get') return current;
    if (method === 'session.events') {
      const records = events.filter(event => event.cursor > params.after).slice(0, params.limit);
      return params.limit === 1 ? replay(records) : records;
    }
    if (method === 'session.prompt') { entered(); return new Promise(resolve => { releasePrompt = resolve; }); }
    if (method === 'session.cancel') {
      assert.deepEqual(params, {session_id:'s1',task_id:'original-task',worker_id:'worker-1',attempt_id:'attempt-1',fence:7});
      return {cancelled:true};
    }
  });
  const sending = app.send();
  await started;
  await app.ui['cancel-button'].dispatch('click');
  events = [record(11, {type:'attempt_started', lease:lease()}), record(12, {type:'attempt_finished', lease:lease()})];
  current = snapshot({cursor:12});
  await app.poll();
  for (let i = 0; i < otherCount; i++) {
    const other = {...lease(`other-client-${i}`), fence:i + 2};
    events.push(record(13 + i * 2, {type:'attempt_started', lease:other}, other), record(14 + i * 2, {type:'attempt_finished', lease:other}, other));
  }
  current = snapshot({cursor:12 + otherCount * 2});
  for (let i = 0; i < Math.ceil(otherCount * 2 / 100); i++) await app.poll();
  assert.equal(app.ui.prompt.disabled, true, 'other attempts and fresh idle cannot supply the missing prompt receipt');
  releasePrompt(snapshot({cursor:12}));
  await sending;
  return {...app, complete:current, setSnapshot:next => {current = next;}};
}

for (const count of [119, 120, 121, 4096]) {
  test(`late original receipt recovers its terminal after ${count} other-client attempts`, async t => {
    const app = await delayedEarlyCancel(t, count);
    await app.poll();
    assert.equal(app.ui.prompt.disabled, false, 'the exact persisted terminal remains available beyond the rendering cache');
    const read = app.calls.findLastIndex(call => call.method === 'session.events' && call.params.after === 11 && call.params.limit === 1);
    assert.ok(read >= 0, 'retrieve only the receipt cursor, without replaying intervening history');
    assert.ok(app.calls.slice(read + 1).some(call => call.method === 'session.get'), 'request idle after retrieving the terminal');
    assert.equal(app.calls.filter(call => call.method === 'task.resume').length, 1);
    assert.equal(app.calls.filter(call => call.method === 'session.cancel').length, 1);
    assert.match(app.ui['event-count'].textContent, /^120 ·/);
  });
}

for (const [name, corrupt] of [
  ['missing', () => []],
  ['different cursor', ([item]) => [{...item, cursor:13}]],
  ['different session', ([item]) => [{...item, session_id:'other-session'}]],
  ['different lease session', ([item]) => [{...item, event:{...item.event, lease:{...item.event.lease, session_id:'other-session'}}}]],
  ['different attempt', ([item]) => [{...item, event:{...item.event, lease:{...item.event.lease, attempt_id:'other-attempt'}}}]],
  ['different worker', ([item]) => [{...item, event:{...item.event, lease:{...item.event.lease, worker_id:'other-worker'}}}]],
  ['different fence', ([item]) => [{...item, event:{...item.event, lease:{...item.event.lease, fence:99}}}]],
  ['incomplete lease', ([item]) => [{...item, attempt:{session_id:'s1'}, event:{...item.event, lease:{session_id:'s1'}}}]],
  ['agent end', ([item]) => [{...item, event:{type:'agent', event:{type:'agent_end', messages:[]}}}]],
  ['different protocol', ([item]) => [{...item, protocol_version:2}]],
  ['mismatched complete leases', ([item]) => [{...item, attempt:{...item.attempt, worker_id:'other-worker'}}]],
  ['read disconnect', () => ({disconnect:true})],
]) {
  test(`historical terminal recovery rejects ${name} evidence`, async t => {
    let invalid = true;
    const app = await delayedEarlyCancel(t, 121, records => invalid ? corrupt(records) : records);
    await app.poll();
    assert.equal(app.ui.prompt.disabled, true, 'idle cannot replace a correctly paired durable terminal');
    invalid = false;
    await app.poll();
    assert.equal(app.ui.prompt.disabled, false, 'a later valid read can recover without resending any write');
    assert.equal(app.calls.filter(call => call.method === 'task.resume').length, 1);
    assert.equal(app.calls.filter(call => call.method === 'session.cancel').length, 1);
  });
}

test('recovered terminal still requires a covering idle snapshot', async t => {
  const app = await delayedEarlyCancel(t);
  app.setSnapshot(snapshot({cursor:11}));
  await app.poll();
  assert.equal(app.ui.prompt.disabled, true, 'an older snapshot cannot cover a newly recovered proof');
  app.setSnapshot(app.complete);
  await app.poll();
  assert.equal(app.ui.prompt.disabled, false);
});

test('a delayed unresolved snapshot cannot undo the exact reconciliation already consumed', async t => {
  const unknown = {call:{id:'c1'}, state:'outcome_unknown', result:{outcome_unknown:true}};
  let current = snapshot({cursor:12, state:'needs_reconciliation', unresolved_calls:[unknown]});
  let events = [];
  const app = await page(t, async (method, params) => {
    if (method === 'session.list') return [current];
    if (method === 'session.get') return current;
    if (method === 'session.events') return events.filter(event => event.cursor > params.after);
  });
  events = [record(13, {type:'reconciled', reconciliation:{call_id:'c1'}}, null)];
  await app.poll(); // consumes cursor 13, then gets the old cursor-12 snapshot
  assert.equal(app.ui.prompt.disabled, true, 'a fresh authoritative idle is still required');
  current = snapshot({cursor:13});
  await app.poll();
  assert.equal(app.ui.prompt.disabled, false, 'the consumed reconciliation must survive the old read');
});
