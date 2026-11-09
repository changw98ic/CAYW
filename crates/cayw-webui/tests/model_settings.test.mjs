// Shipped app + synthetic DOM/fetch fixtures. No real credentials or providers.
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import vm from 'node:vm';

const source = await readFile(new URL('../assets/app.js', import.meta.url), 'utf8');
const html = await readFile(new URL('../assets/index.html', import.meta.url), 'utf8');
const syntheticKey = 'SYNTHETIC_NOT_A_REAL_KEY';
const publicModel = () => ({ service:'openai', api:'chat_completions', base_url:'https://fixture.invalid/v1', id:'synthetic-model', api_version:'v1', max_output_tokens:4096, authentication:'plain_file' });
const deferred = () => { let resolve, reject; const promise = new Promise((yes, no) => { resolve = yes; reject = no; }); return { promise, resolve, reject }; };
const idle = async () => { for (let i = 0; i < 12; i++) await new Promise(resolve => setImmediate(resolve)); };

class Element {
  constructor(tag = 'div') { this.tagName = tag; this.children = []; this.listeners = new Map(); this.dataset = {}; this.value = ''; this.disabled = false; this.checked = false; this.ownText = ''; }
  get textContent() { return this.ownText + this.children.map(child => child.textContent).join(''); }
  set textContent(value) { this.ownText = String(value); this.children = []; }
  append(...children) { this.children.push(...children); }
  replaceChildren(...children) { this.children = children; this.ownText = ''; if (this.tagName === 'select') this.value = children[0]?.value || ''; }
  addEventListener(name, fn) { const listeners = this.listeners.get(name) || []; listeners.push(fn); this.listeners.set(name, listeners); }
  async fire(name) { for (const fn of this.listeners.get(name) || []) await fn({ preventDefault() {} }); }
  setAttribute(name, value) { this[name] = value; }
  removeAttribute(name) { delete this[name]; }
  showModal() { this.open = true; }
  close() { if (this.open) { this.open = false; void this.fire('close'); } }
  focus() {}
  get classList() { return { add() {} }; }
}

async function page(handler = async () => undefined) {
  const ui = Object.fromEntries([...html.matchAll(/id="([^"]+)"/g)].map(match => [match[1], new Element()]));
  for (const match of html.matchAll(/<[^>]*id="([^"]+)"[^>]*>/g)) {
    ui[match[1]].disabled = /\bdisabled\b/.test(match[0]);
    ui[match[1]].hidden = /\bhidden\b/.test(match[0]);
  }
  const defaults = { name:'', service:'openai', api:'chat_completions', base_url:'https://api.openai.com/v1', id:'', api_version:'v1', credential:'remove', api_key:'', max_output_tokens:'4096' };
  const form = ui['model-settings-form'];
  form.elements = Object.fromEntries(Object.keys(defaults).map(name => [name, new Element(['service','api','credential'].includes(name) ? 'select' : 'input')]));
  form.reset = () => { for (const [name, value] of Object.entries(defaults)) form.elements[name].value = value; form.elements.api_key.disabled = true; };
  form.reset();
  ui['plugin-settings-form'].elements = { name:new Element(), descriptor:new Element() };
  const win = new Element(); win.location = { hash:'' }; win.history = { replaceState() {} }; win.matchMedia = () => ({ matches:false });
  let workspace = 'a';
  const calls = [];
  const models = { daily:publicModel(), backup:publicModel() };
  const modelList = () => ({ selected:'daily', models, health:{ provider:'openai', model:'synthetic-model' } });
  const fetch = async (url, options = {}) => {
    if (url === '/api/auth/status') return { ok:true, json:async () => ({ mode:'local', authenticated:true }) };
    if (url === '/api/settings/account') return { ok:true, json:async () => ({ can_manage:false }) };
    if (url === '/api/auth/logout') { await handler('logout'); return { ok:true }; }
    assert.equal(url, '/api/rpc');
    const call = JSON.parse(options.body); calls.push(call);
    let result = await handler(call.method, call.params, call.workspace_id);
    if (result === undefined) {
      switch (call.method) {
        case 'runtime.health': result = { status:'ok', protocol_version:3 }; break;
        case 'settings.workspaces.list': result = { workspaces:['a','b'].map(id => ({ id, path:`/fixture/${id}`, state:'open' })) }; break;
        case 'settings.workspaces.open': workspace = call.params.id; result = { id:workspace, path:`/fixture/${workspace}` }; break;
        case 'settings.workspace': result = { workspace:`/fixture/${call.workspace_id || workspace}`, known_workspaces:[] }; break;
        case 'settings.models.list': result = modelList(); break;
        case 'settings.models.put': {
          const { name, model, credential } = call.params;
          const authentication = credential === 'keep' ? models[name]?.authentication : credential === 'replace' ? 'plain_file' : 'none';
          const { api_key, ...publicFields } = model;
          models[name] = { ...publicFields, authentication }; result = modelList(); break;
        }
        case 'settings.plugins.list': result = { entries:[] }; break;
        case 'session.list': result = []; break;
        default: result = {};
      }
    }
    return { ok:true, json:async () => ({ result }) };
  };
  const context = vm.createContext({
    document:{ getElementById:id => ui[id], createElement:tag => new Element(tag), createDocumentFragment:() => new Element('#fragment'), addEventListener() {} },
    window:win, fetch, AbortController, URLSearchParams, TypeError,
    setTimeout:() => 1, clearTimeout() {},
    FormData:class { constructor(form) { this.entries = Object.entries(form.elements).filter(([,field]) => !field.disabled).map(([name,field]) => [name,field.value]); } [Symbol.iterator]() { return this.entries[Symbol.iterator](); } },
  });
  vm.runInContext(source.replaceAll('export ', ''), context, { filename:'app.js' });
  await idle();
  assert.equal(ui.workspace.hidden, false, ui['login-error'].textContent);
  const open = async () => { await ui['open-settings'].fire('click'); await idle(); };
  const edit = async (name = 'daily') => { const row = ui['model-profiles'].children.find(row => row.children[0]?.textContent.startsWith(`${name} ·`)); assert.ok(row); await row.children.find(node => node.textContent === '编辑').fire('click'); await idle(); };
  const credential = async value => { form.elements.credential.value = value; await form.elements.credential.fire('change'); };
  const submit = async () => { await form.fire('submit'); await idle(); };
  const puts = () => calls.filter(call => call.method === 'settings.models.put');
  await open();
  return { ui, fields:form.elements, calls, win, open, edit, credential, submit, puts };
}

for (const name of ['daily','backup']) test(`editing ${name} preserves credentials and only changes requested metadata`, async () => {
  const app = await page(); await app.edit(name);
  assert.equal(app.fields.api_key.value, ''); assert.equal(app.fields.api_key.disabled, true); assert.equal(app.fields.name.readOnly, true);
  app.fields.max_output_tokens.value = '2048'; await app.submit();
  assert.equal(app.puts().length, 1);
  const { authentication, ...model } = publicModel();
  assert.deepEqual(app.puts()[0].params, { name, credential:'keep', model:{ ...model, max_output_tokens:2048 } });
  assert.equal(app.fields.api_key.value, ''); assert.equal(app.fields.name.value, '');
});

test('replacement clears the input before save completes and refresh may fail without retaining the key', async () => {
  const pending = deferred(); let saving = false;
  const app = await page(async method => { if (method === 'settings.models.put') { saving = true; await pending.promise; return {}; } if (method === 'settings.models.list' && saving) throw new Error('refresh unavailable'); });
  await app.edit(); await app.credential('replace'); app.fields.api_key.value = syntheticKey;
  await app.submit();
  assert.equal(app.puts()[0].params.credential, 'replace'); assert.equal(app.puts()[0].params.model.api_key, syntheticKey);
  assert.equal(app.fields.api_key.value, '');
  pending.resolve(); await idle(); assert.equal(app.fields.api_key.value, ''); assert.equal(app.fields.name.value, '');
});

test('explicit removal and new unauthenticated or authenticated profiles have unambiguous payloads', async () => {
  const app = await page(); await app.edit(); await app.credential('remove'); await app.submit();
  assert.equal(app.puts()[0].params.credential, 'remove'); assert.equal(Object.hasOwn(app.puts()[0].params.model, 'api_key'), false);
  app.fields.name.value = 'local'; app.fields.id.value = 'fixture'; await app.submit();
  assert.equal(app.puts()[1].params.credential, 'remove');
  app.fields.name.value = 'new'; app.fields.id.value = 'fixture'; await app.credential('replace'); app.fields.api_key.value = syntheticKey; await app.submit();
  assert.equal(app.puts()[2].params.credential, 'replace'); assert.equal(app.puts()[2].params.model.api_key, syntheticKey);
});

test('a new form cannot silently overwrite an existing profile', async () => {
  const app = await page(); app.fields.name.value = 'daily'; app.fields.id.value = 'fixture'; await app.submit();
  assert.equal(app.puts().length, 0); assert.match(app.ui['settings-status'].textContent, /已存在/);
});

for (const [field, value] of [['base_url','https://other.invalid/v1'], ['service','magpie'], ['api','responses'], ['api_version','v2']]) test(`changing ${field} requires reentry or explicit credential removal`, async () => {
  const app = await page(); await app.edit(); app.fields[field].value = value; await app.submit();
  assert.equal(app.puts().length, 0); assert.match(app.ui['settings-status'].textContent, /重新输入密钥/);
  await app.credential('replace'); app.fields.api_key.value = syntheticKey; await app.submit();
  assert.equal(app.puts().length, 1); assert.equal(app.puts()[0].params.credential, 'replace');
});

for (const event of ['close-button','close','cancel','pagehide','logout']) test(`${event} clears secrets and the model edit context`, async () => {
  const app = await page(); await app.edit(); await app.credential('replace'); app.fields.api_key.value = syntheticKey;
  app.ui['account-password'].value = syntheticKey; app.ui['account-confirm'].value = syntheticKey;
  if (event === 'close-button') await app.ui['close-settings'].fire('click');
  else if (event === 'pagehide') await app.win.fire('pagehide');
  else if (event === 'logout') await app.ui.logout.fire('click');
  else await app.ui['settings-dialog'].fire(event);
  await idle();
  assert.equal(app.fields.api_key.value, ''); assert.equal(app.fields.name.value, ''); assert.equal(app.fields.name.readOnly, false);
  assert.equal(app.ui['account-password'].value, ''); assert.equal(app.ui['account-confirm'].value, '');
});

test('workspace switch clears secrets at start and drops forms reopened while activation is pending', async () => {
  const pending = deferred();
  const app = await page(async (method, params) => { if (method === 'settings.workspaces.open') { await pending.promise; return { id:params.id, path:`/fixture/${params.id}` }; } });
  await app.edit(); await app.credential('replace'); app.fields.api_key.value = syntheticKey;
  app.ui['workspace-choice'].value = 'b'; const switching = app.ui['switch-workspace'].fire('click'); await idle();
  assert.equal(app.fields.api_key.value, ''); assert.equal(app.fields.name.value, '');
  await app.open(); await app.edit(); await app.credential('replace'); app.fields.api_key.value = syntheticKey;
  pending.resolve(); await switching; await idle();
  assert.equal(app.fields.api_key.value, ''); assert.equal(app.fields.name.value, ''); assert.equal(app.ui['settings-dialog'].open, false);
  await app.open(); app.fields.name.value = 'b-only'; app.fields.id.value = 'fixture'; await app.submit();
  assert.equal(app.puts()[0].workspace_id, 'b');
});

test('late list response after close cannot resurrect old profile buttons', async () => {
  let delay = false; const pending = deferred();
  const app = await page(async method => { if (method === 'settings.models.list' && delay) return pending.promise; });
  delay = true; const refreshing = app.ui['refresh-settings'].fire('click'); await idle(); await app.ui['close-settings'].fire('click');
  pending.resolve({ selected:'daily', models:{ daily:publicModel() }, health:{ provider:'openai', model:'fixture' } }); await refreshing; await idle();
  assert.equal(app.ui['model-profiles'].children.length, 0); assert.equal(app.fields.name.value, '');
});

test('late save receipt cannot overwrite a new form after close and reopen', async () => {
  const pending = deferred(); const app = await page(async method => { if (method === 'settings.models.put') return pending.promise; });
  await app.edit(); await app.credential('replace'); app.fields.api_key.value = syntheticKey; await app.submit();
  await app.ui['close-settings'].fire('click'); await app.open(); app.fields.name.value = 'new-profile'; await app.credential('replace'); app.fields.api_key.value = 'SYNTHETIC_NEW_DRAFT';
  pending.resolve({}); await idle();
  assert.equal(app.fields.name.value, 'new-profile'); assert.equal(app.fields.api_key.value, 'SYNTHETIC_NEW_DRAFT'); assert.equal(app.puts().length, 1);
});

test('failed saves clear typed secrets and never retry automatically', async () => {
  const app = await page(async method => { if (method === 'settings.models.put') throw new Error('connection lost'); });
  await app.edit(); await app.credential('replace'); app.fields.api_key.value = syntheticKey; await app.submit();
  assert.equal(app.fields.api_key.value, ''); assert.equal(app.puts().length, 1); assert.match(app.ui['settings-status'].textContent, /连接|lost/);
});

test('changing endpoint input after typing a new key clears it', async () => {
  const app = await page(); await app.edit(); await app.credential('replace'); app.fields.api_key.value = syntheticKey;
  app.fields.base_url.value = 'https://other.invalid/v1'; await app.fields.base_url.fire('input');
  assert.equal(app.fields.api_key.value, ''); await app.submit(); assert.equal(app.puts().length, 0);
});
