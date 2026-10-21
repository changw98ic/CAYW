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
    let body = '';
    for await (const chunk of req) body += chunk;
    const call = JSON.parse(body);
    calls.push(call);
    let result = await handler(call.method, call.params);
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
    window: { addEventListener() {}, matchMedia: () => ({ matches: false }) }, AbortController, TypeError,
    fetch: (url, options) => fetch(`http://127.0.0.1:${server.address().port}${url}`, options),
    setTimeout: (fn, delay) => { const id = ++nextTimer; timers.set(id, { fn, delay }); return id; },
    clearTimeout: id => timers.delete(id),
  });
  vm.runInContext(source.replaceAll('export ', ''), context, { filename: 'app.js' });
  elements['web-token'].value = '0123456789abcdef0123456789abcdef';
  await elements['login-form'].dispatch('submit');
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
  assert.equal(app.calls.filter(call => call.method === 'session.prompt').length, 1);
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
  app.ui['web-token'].value = '0123456789abcdef0123456789abcdef';
  await app.ui['login-form'].dispatch('submit');
  assert.equal(app.ui.prompt.disabled,true,'reauthentication is not settlement');
  await app.poll();
  assert.equal(app.ui.prompt.disabled,true);
  assert.equal(app.calls.filter(call=>call.method==='session.prompt').length,1);
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
    if (method === 'session.cancel') return {cancelled:true};
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
  assert.equal(app.calls.filter(call => call.method === 'session.prompt').length, 1);
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
  assert.equal(app.calls.filter(call => call.method === 'session.prompt').length, 1);
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
    assert.equal(app.calls.filter(call => call.method === 'session.prompt').length, 1);
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
    if (method === 'session.cancel') return {cancelled:true};
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
    assert.equal(app.calls.filter(call => call.method === 'session.prompt').length, 1);
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
    assert.equal(app.calls.filter(call => call.method === 'session.prompt').length, 1);
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

test('projects and plugins management pages switch views and preserve uncertain write locks across projects', async t => {
  const frozenSession = snapshot({
    id: 's-plugin',
    label: '插件会话',
    binding: {
      plugin: 'strongflow',
      version: '1.0.0',
      registry_id: 'reg-sf',
      generation: 2,
      frozen: true,
      reason: 'host unloaded strongflow',
      manifest: { capabilities: [{ name: 'strongflow.plan' }] },
    },
  });
  let activeProjectSeen = null;
  const app = await page(t, async (method, params) => {
    if (method === 'projects.list') {
      return [
        { id: 'default', label: '默认项目', address: '127.0.0.1:4100', default: true },
        { id: 'secondary', label: '第二项目', address: '127.0.0.1:4101', default: false },
      ];
    }
    if (method === 'capabilities.list') return [{ name: 'strongflow.plan' }];
    if (method === 'session.list') return [frozenSession];
    if (method === 'session.get') return frozenSession;
    if (method === 'session.events') return [];
    if (method === 'session.fork') {
      activeProjectSeen = params.label;
      return snapshot({ id: 's-forked', label: params.label, cursor: 3 });
    }
  });
  await app.ui['nav-projects'].dispatch('click');
  assert.equal(app.ui['projects-page'].hidden, false);
  assert.equal(app.ui['conversation-view'].hidden, true);
  await app.ui['refresh-projects'].dispatch('click');
  assert.match(app.ui['projects-status'].textContent, /2 个本机项目/);

  await app.ui['nav-plugins'].dispatch('click');
  assert.equal(app.ui['plugins-page'].hidden, false);
  await app.ui['refresh-plugins'].dispatch('click');
  assert.match(app.ui['plugin-list'].textContent, /strongflow/);
  assert.match(app.ui['plugin-list'].textContent, /host unloaded strongflow/);
  assert.equal(activeProjectSeen, null);

  // Switch to secondary project and verify project parameter routing.
  await app.ui['nav-projects'].dispatch('click');
  const secondaryCard = app.ui['project-list'].children[1];
  const switchButton = secondaryCard.children.at(-1).children[0];
  await switchButton.dispatch('click');
  assert.equal(app.ui['conversation-view'].hidden, false);
  assert.ok(app.calls.some(call => call.project === 'secondary' && call.method === 'session.list'));
});
