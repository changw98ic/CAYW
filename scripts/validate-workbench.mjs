// macOS workbench acceptance with real Runtime, gateway, Editor and Trace services.
// Example: CAYW_PLAYWRIGHT_MODULE=/tmp/playwright-env/node_modules/playwright/index.mjs \
// node scripts/validate-workbench.mjs --bin-dir /absolute/target/debug --out /new/evidence/dir
import assert from 'node:assert/strict';
import {spawn, execFileSync} from 'node:child_process';
import {createHash, randomBytes} from 'node:crypto';
import {mkdir, readFile, writeFile} from 'node:fs/promises';
import {createServer} from 'node:http';
import {dirname, resolve} from 'node:path';
import {fileURLToPath, pathToFileURL} from 'node:url';
import {once} from 'node:events';
import {release} from 'node:os';
import {setTimeout as delay} from 'node:timers/promises';

assert.equal(process.platform, 'darwin', 'This validation is scoped to macOS.');
const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const options = new Map();
for (let index = 2; index < process.argv.length; index += 2) {
  const name = process.argv[index];
  assert(['--out', '--bin-dir', '--playwright-module'].includes(name), `Unknown argument: ${name}`);
  assert(process.argv[index + 1] && !options.has(name), `Missing or repeated argument: ${name}`);
  options.set(name, process.argv[index + 1]);
}
assert(options.has('--out'), '--out must name a new evidence directory.');
const out = resolve(options.get('--out'));
const binDir = resolve(options.get('--bin-dir') || resolve(root, 'target/debug'));
const moduleName = options.get('--playwright-module') || process.env.CAYW_PLAYWRIGHT_MODULE || 'playwright';
const {chromium} = await import(moduleName.startsWith('/') ? pathToFileURL(moduleName).href : moduleName);
await mkdir(dirname(out), {recursive: true});
await mkdir(out, {recursive: false});
const work = resolve(out, 'work');
const workspace = resolve(work, 'workspace');
await mkdir(workspace, {recursive: true});
const tokens = Object.fromEntries(['ipc', 'web', 'editor', 'trace', 'provider'].map(key => [key, randomBytes(32).toString('hex')]));
const sha = bytes => createHash('sha256').update(bytes).digest('hex');
const digest = async path => sha(await readFile(path));
const redact = value => Object.values(tokens).reduce((text, token) => text.replaceAll(token, '[redacted]'), String(value));
const record = (name, value) => writeFile(resolve(out, `${name}.json`), `${redact(JSON.stringify(value, null, 2))}\n`, {flag: 'wx'});
const binaries = Object.fromEntries(['cayw', 'cayw-webui', 'cayw-visual-editor', 'cayw-trace-debug'].map(name => [name, resolve(binDir, name)]));
const sourcePaths = [
  'Cargo.toml', 'Cargo.lock', 'scripts/validate-workbench.mjs',
  ...['main.rs', 'lib.rs', 'rpc.rs', 'collaboration.rs', 'plannotator.rs', 'artifacts.rs'].map(name => `crates/cayw-runtime/src/${name}`),
  ...['lib.rs', 'main.rs', 'ownership.rs'].map(name => `crates/cayw-webui/src/${name}`),
  ...['app.js', 'index.html', 'styles.css'].map(name => `crates/cayw-webui/assets/${name}`),
  ...['main.rs', 'lib.rs', 'project.rs'].map(name => `crates/cayw-visual-editor/src/${name}`),
  'crates/cayw-visual-editor/assets/editor.js',
  ...['main.rs', 'lib.rs', 'capture.rs', 'types.rs', 'server.rs'].map(name => `crates/cayw-trace-debug/src/${name}`),
  ...['app.js', 'index.html', 'styles.css'].map(name => `crates/cayw-trace-debug/assets/${name}`),
];
const sourceReceipt = Object.fromEntries(await Promise.all(sourcePaths.map(async path => [path, await digest(resolve(root, path))])));
const binaryReceipt = Object.fromEntries(await Promise.all(Object.entries(binaries).map(async ([name, path]) => [name, {path, sha256: await digest(path)}])));
const summary = {
  schema: 'cayw.workbench-validation.v1', started_utc: new Date().toISOString(),
  platform: process.platform, architecture: process.arch, os_release: release(), node: process.version,
  base_commit: execFileSync('git', ['rev-parse', 'HEAD'], {cwd: root, encoding: 'utf8'}).trim(),
  source_sha256: sha(JSON.stringify(sourceReceipt)),
  working_diff_sha256: sha(execFileSync('git', ['diff', '--binary', 'HEAD'], {cwd: root, maxBuffer: 32 * 1024 * 1024})),
  binaries: binaryReceipt, input_kind: 'fixed-local-model-with-real-services-and-host-effects',
  paid_model_requests: 0, assertions: [], passed: false,
};
await record('source-receipt', sourceReceipt);
const services = [];
let browser;
let provider;
let providerRequests = 0;
const checks = name => summary.assertions.push(name);
async function until(label, read, accept, timeout = 30000) {
  const deadline = Date.now() + timeout;
  let value;
  do {
    value = await read();
    if (accept(value)) return value;
    await delay(50);
  } while (Date.now() < deadline);
  throw new Error(`${label} did not become ready; last value: ${redact(JSON.stringify(value))}`);
}
async function start(name, args, environment, parse) {
  const child = spawn(binaries[name], args, {
    cwd: root, env: {PATH: process.env.PATH, TMPDIR: process.env.TMPDIR, LANG: 'en_US.UTF-8', ...environment},
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  let stdout = '', stderr = '';
  const service = {name, child, args, stdout: () => stdout, stderr: () => stderr};
  services.push(service);
  let ready = false;
  const result = new Promise((accept, reject) => {
    child.on('error', reject);
    child.on('exit', (code, signal) => {
      if (!ready) reject(new Error(`${name} exited before readiness (${code}/${signal}): ${redact(stderr)}`));
    });
    child.stderr.on('data', chunk => { stderr += chunk; });
    child.stdout.on('data', chunk => {
      stdout += chunk;
      if (!ready && stdout.includes('\n')) {
        try {
          const value = parse(stdout.split('\n')[0]);
          ready = true;
          accept(value);
        } catch (error) { reject(error); }
      }
    });
  });
  let timer;
  try {
    const value = await Promise.race([result, new Promise((_, reject) => {
      timer = setTimeout(() => reject(new Error(`${name} readiness deadline`)), 45000);
    })]);
    await record(`${name}-start`, {binary: binaries[name], args, pid: child.pid, ready: value});
    return value;
  } finally { clearTimeout(timer); }
}
async function stop(service) {
  const {child, name} = service;
  let status = [child.exitCode, child.signalCode];
  if (child.exitCode === null && child.signalCode === null) {
    const ended = once(child, 'exit');
    child.kill('SIGINT');
    const timer = setTimeout(() => child.kill('SIGKILL'), 12000);
    try { status = await ended; } finally { clearTimeout(timer); }
  }
  await writeFile(resolve(out, `${name}-stdout.log`), redact(service.stdout()), {flag: 'wx'});
  await writeFile(resolve(out, `${name}-stderr.log`), redact(service.stderr()), {flag: 'wx'});
  await record(`${name}-stop`, {exit_code: status[0], signal: status[1]});
  assert.equal(status[0], 0, `${name} did not stop cleanly: ${JSON.stringify(status)}`);
}
async function screenshot(page, name) {
  // The proof starts after login. Do not include any credential field value.
  await page.locator('input[type="password"]').evaluateAll(inputs => { for (const input of inputs) input.value = ''; });
  await page.screenshot({path: resolve(out, `${name}.png`), fullPage: true});
}

const firstSteps = [
  {id: 'inspect', text: 'Inspect real workspace', status: 'in_progress'},
  {id: 'edit', text: 'Review the file change', status: 'pending'},
];
const secondSteps = [
  {id: 'inspect', text: 'Inspect real workspace', status: 'completed'},
  {id: 'edit', text: 'Review the updated file change', status: 'in_progress'},
];
const writeArgs = {path: 'evidence.txt', content: 'Real Host effect for workbench validation'};
const readArgs = {path: 'evidence.txt'};
const script = `cayw.call('plan.update',{expected_version=0,steps={{id='inspect',text='Inspect real workspace',status='in_progress'},{id='edit',text='Review the file change',status='pending'}}})\ncayw.call('fs.write',{path='evidence.txt',content='Real Host effect for workbench validation'})\ncayw.call('fs.read',{path='evidence.txt'})\nreturn cayw.call('plan.update',{expected_version=1,steps={{id='inspect',text='Inspect real workspace',status='completed'},{id='edit',text='Review the updated file change',status='in_progress'}}})`;
try {
  await writeFile(resolve(workspace, 'index.html'), '<!doctype html><html><head><title>Workbench project</title></head><body><h1 id="project-proof">Workbench real disk project</h1><p>Opened by the actual Editor service.</p></body></html>\n');
  await writeFile(resolve(workspace, 'README.md'), 'Workbench acceptance project.\n');
  provider = createServer(async (request, response) => {
    try {
      assert.equal(request.url, '/v1/chat/completions');
      assert.equal(request.headers.authorization, `Bearer ${tokens.provider}`);
      const chunks = [];
      for await (const chunk of request) chunks.push(chunk);
      const body = JSON.parse(Buffer.concat(chunks).toString());
      providerRequests++;
      await record(`fixed-model-request-${providerRequests}`, body);
      const last = body.messages.at(-1);
      let argumentsValue;
      if (last?.role === 'tool') {
        const texts = typeof last.content === 'string' ? [last.content] : last.content.map(part => {
          assert.equal(part.type, 'text');
          return part.text;
        });
        // The adapter can carry both text and the structured page as separate
        // text blocks. Parse each block; joining them is not a JSON document.
        const pages = texts.map(text => JSON.parse(text));
        const page = pages[0];
        assert(pages.every(item => item.cell_id === page.cell_id && item.status === page.status && item.next_cursor === page.next_cursor));
        if (!['completed', 'failed', 'cancelled', 'outcome_unknown', 'interrupted'].includes(page.status) || page.has_more) {
          argumentsValue = {action: 'wait', cell_id: page.cell_id, cursor: page.next_cursor, yield_time_ms: 30000, max_output_tokens: 32768};
        }
      } else {
        argumentsValue = {action: 'exec', script, yield_time_ms: 30000, max_output_tokens: 32768};
      }
      const delta = argumentsValue
        ? {tool_calls: [{index: 0, id: `workbench-call-${providerRequests}`, type: 'function', function: {name: 'execute_code', arguments: JSON.stringify(argumentsValue)}}]}
        : {content: 'The fixed local model consumed the real Runtime result.'};
      const chunk = (value, reason) => JSON.stringify({id: 'workbench-response', model: 'workbench-local-fixed', choices: [{index: 0, delta: value, finish_reason: reason}]});
      const usage = providerRequests === 1
        ? {prompt_tokens: 12, completion_tokens: 7, total_tokens: 19}
        : {prompt_tokens: 17, completion_tokens: 11, total_tokens: 28};
      response.writeHead(200, {'Content-Type': 'text/event-stream'});
      response.end(`data: ${chunk(delta, null)}\n\ndata: ${chunk({}, argumentsValue ? 'tool_calls' : 'stop')}\n\ndata: ${JSON.stringify({choices: [], usage})}\n\ndata: [DONE]\n\n`);
    } catch (error) {
      (summary.fixed_model_errors ||= []).push(redact(error.stack || error.message));
      response.writeHead(500);
      response.end(redact(error.message));
    }
  });
  provider.listen(0, '127.0.0.1');
  await once(provider, 'listening');
  const configPath = resolve(work, 'runtime-config.json');
  await writeFile(configPath, JSON.stringify({
    model: {service: 'openai', api: 'chat_completions', id: 'workbench-local-fixed', base_url: `http://127.0.0.1:${provider.address().port}/v1`, api_version: 'v1', max_output_tokens: 1024, api_key_env: 'CAYW_WORKBENCH_PROVIDER_KEY'},
    budgets: {max_total_tokens: 8192, max_turns: 8, max_run_time_ms: 180000, max_capability_calls: 16, max_capability_bytes: 1048576, host_wait_ms: 150000, host_call_timeout_ms: 150000},
    permissions: [{capability: 'fs.write', arguments: writeArgs}, {capability: 'fs.read', arguments: readArgs}],
  }, null, 2));
  const runtime = await start('cayw', ['serve', '--db', resolve(work, 'runtime.db'), '--workspace', workspace, '--listen', '127.0.0.1:0', '--token-env', 'CAYW_WORKBENCH_IPC_TOKEN', '--config', configPath], {
    CAYW_WORKBENCH_IPC_TOKEN: tokens.ipc, CAYW_WORKBENCH_PROVIDER_KEY: tokens.provider,
  }, JSON.parse);
  assert.equal(providerRequests, 0, 'Starting Runtime must not call the model');
  const web = await start('cayw-webui', [], {
    CAYW_IPC_ADDR: runtime.listening, CAYW_IPC_TOKEN: tokens.ipc, CAYW_WEB_TOKEN: tokens.web,
    CAYW_OWNERSHIP_PATH: resolve(work, 'ownership.json'),
  }, line => { const match = line.match(/http:\/\/127\.0\.0\.1:\d+\//); assert(match, 'WebUI did not print its actual address'); return {url: match[0]}; });
  async function rpc(method, params = {}, allowError = false) {
    const response = await fetch(`${web.url}api/rpc`, {
      method: 'POST', headers: {Authorization: `Bearer ${tokens.web}`, 'Content-Type': 'application/json', Origin: new URL(web.url).origin},
      body: JSON.stringify({method, params}), signal: AbortSignal.timeout(180000),
    });
    const body = await response.json();
    if (allowError) return {status: response.status, body};
    assert(response.ok && !body.error, `${method}: ${JSON.stringify(body)}`);
    return body.result;
  }
  browser = await chromium.launch(process.env.CAYW_BROWSER_EXECUTABLE
    ? {executablePath: process.env.CAYW_BROWSER_EXECUTABLE, headless: true}
    : {channel: 'chrome', headless: true});
  summary.browser_version = browser.version();
  const context = await browser.newContext({viewport: {width: 1600, height: 1100}, reducedMotion: 'reduce'});
  const page = await context.newPage();
  const browserErrors = [];
  context.on('page', child => child.on('pageerror', error => browserErrors.push(redact(error.message))));
  page.on('pageerror', error => browserErrors.push(redact(error.message)));
  const navigated = await page.goto(web.url);
  const csp = navigated.headers()['content-security-policy'];
  assert.equal(csp, "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'; object-src 'none'", 'Gateway CSP must remain restrictive');
  await page.locator('#web-token').fill(tokens.web);
  await page.locator('#login-button').click();
  await page.locator('#workspace').waitFor({state: 'visible'});
  await page.locator('#session-label').fill('Real workbench acceptance');
  await page.locator('#session-device').fill('local');
  await page.locator('#create-button').click();
  await page.getByRole('heading', {name: 'Real workbench acceptance', exact: true}).waitFor();
  const sessions = await rpc('session.list');
  assert.equal(sessions.length, 1);
  const session = sessions[0].id;
  summary.session_id = session;
  checks('WebUI authenticates, creates and selects a real local Runtime Session');
  const editor = await start('cayw-visual-editor', ['--project', workspace, '--page', 'index.html', '--state-dir', resolve(work, 'editor-state'), '--addr', '127.0.0.1:0', '--preview-addr', '127.0.0.1:0'], {CAYW_EDITOR_TOKEN: tokens.editor}, JSON.parse);
  const trace = await start('cayw-trace-debug', ['--store', resolve(work, 'trace.json'), '--workspace', workspace, '--session', session], {
    CAYW_IPC_ADDR: runtime.listening, CAYW_IPC_TOKEN: tokens.ipc, CAYW_TRACE_WEB_TOKEN: tokens.trace,
  }, JSON.parse);
  await page.locator('#prompt').fill('Run the fixed local plan and file evidence task');
  await page.locator('#send-button').click();
  const firstApprovalView = await until('first plan approval', () => rpc('session.get', {session_id: session}), view => view.pending_approvals?.some(item => item.input.capability === 'plan.update'));
  const firstApproval = firstApprovalView.pending_approvals.find(item => item.input.capability === 'plan.update');
  await rpc('approval.decide', {session_id: session, approval_id: firstApproval.id, decision: 'allow_once'});
  summary.synthetic_approval_decisions = ['first plan.update: allow_once', 'second plan.update: allow_once'];
  const paused = await until('second plan approval', () => rpc('session.get', {session_id: session}), view => view.pending_approvals?.some(item => item.input.capability === 'plan.update' && item.id !== firstApproval.id));
  const approval = paused.pending_approvals.find(item => item.input.capability === 'plan.update' && item.id !== firstApproval.id);
  const firstPlan = await rpc('session.plan', {session_id: session});
  assert.deepEqual(firstPlan.steps, firstSteps);
  assert.match(firstPlan.content_sha256, /^[a-f0-9]{64}$/);
  assert.equal(await readFile(resolve(workspace, 'evidence.txt'), 'utf8'), writeArgs.content);
  await page.locator('#wb-plannotator').click();
  const panel = () => page.locator('.panel-card[data-active="true"] .panel-body');
  await panel().getByText('[in_progress] Inspect real workspace', {exact: true}).waitFor();
  await panel().getByText(new RegExp(firstPlan.content_sha256)).waitFor();
  await panel().locator('textarea').fill('Only the reviewed first plan');
  await screenshot(page, 'plan-v1-real-runtime');
  await rpc('approval.decide', {session_id: session, approval_id: approval.id, decision: 'allow_once'});
  await until('completed task', () => rpc('session.tasks', {session_id: session}), tasks => tasks.length === 1 && tasks[0].state === 'completed');
  const secondPlan = await rpc('session.plan', {session_id: session});
  assert.equal(secondPlan.version, 2);
  assert.deepEqual(secondPlan.steps, secondSteps);
  const staleResponse = page.waitForResponse(response => response.url().endsWith('/api/rpc') && response.request().postDataJSON()?.method === 'plannotator.submit');
  await panel().getByRole('button', {name: '提交版本绑定批注', exact: true}).click();
  const stale = await (await staleResponse).json();
  assert.match(stale.error?.message || '', /plan feedback version conflict/);
  assert.equal(await panel().locator('textarea').inputValue(), 'Only the reviewed first plan');
  assert.deepEqual(await rpc('plannotator.list', {session_id: session}), []);
  await panel().getByText(/意见已保留/).waitFor();
  await screenshot(page, 'stale-feedback-rejected');
  await panel().getByRole('button', {name: '重新读取计划并清空意见', exact: true}).click();
  await panel().getByText('[in_progress] Review the updated file change', {exact: true}).waitFor();
  assert.equal(await panel().locator('textarea').inputValue(), '');
  await panel().locator('textarea').fill('Reviewed the current second plan');
  await panel().getByRole('button', {name: '提交版本绑定批注', exact: true}).click();
  await panel().getByText(/批注已提交！/).waitFor();
  const feedback = await rpc('plannotator.list', {session_id: session});
  assert.equal(feedback.length, 1);
  assert.equal(feedback[0].target.file.version, 2);
  assert.equal(feedback[0].target.file.content_sha256, secondPlan.content_sha256);
  await record('plan-review', {first_plan: firstPlan, second_plan: secondPlan, stale_error: stale.error, saved_feedback: feedback});
  checks('Real session.plan content is shown; stale feedback is rejected and explicit reread clears notes');
  await page.locator('#wb-tasks').click();
  await panel().getByText('[completed] Inspect real workspace', {exact: true}).waitFor();
  await screenshot(page, 'tasks-real-runtime');
  await page.locator('#wb-usage').click();
  await panel().getByText(/Tokens 总量: 47/).waitFor();
  const overview = await rpc('session.context_overview', {session_id: session});
  await record('context-overview', overview);
  assert.equal(overview.tokens.total_tokens, 47);
  assert.equal(overview.cost.status, 'unknown');
  assert.deepEqual(overview.evolution.map(turn => ({turn: turn.turn, input: turn.usage.input, output: turn.usage.output, total_tokens: turn.usage.total_tokens})), [
    {turn: 1, input: 12, output: 7, total_tokens: 19},
    {turn: 2, input: 17, output: 11, total_tokens: 28},
  ]);
  const firstTurn = panel().getByText('[轮次 1] 输入: 12, 输出: 7, 增量: 19', {exact: true});
  const secondTurn = panel().getByText('[轮次 2] 输入: 17, 输出: 11, 增量: 28', {exact: true});
  await firstTurn.waitFor();
  await secondTurn.waitFor();
  await panel().getByText('费用结算状态: unknown', {exact: true}).waitFor();
  await record('context-evolution-visible', {turns: [await firstTurn.innerText(), await secondTurn.innerText()], total_tokens: 47, cost_status: 'unknown'});
  await screenshot(page, 'usage-real-runtime');
  checks('Task and usage panels read actual Runtime records; each turn shows its reported usage and unpriced cost stays unknown');
  await rpc('artifacts.put', {artifact_id: 'acceptance-card', session_id: session, kind: 'html_card', title: 'Stored by real Runtime', body: '<p>Real Runtime artifact</p>', at_ms: Date.now()});
  await page.locator('#wb-artifacts').click();
  await panel().getByText('Stored by real Runtime', {exact: true}).waitFor();
  await panel().getByRole('button', {name: '获取渲染投影', exact: true}).click();
  await panel().getByText(/类型: text\/html/).waitFor();
  await screenshot(page, 'artifacts-real-runtime');
  checks('Artifact panel uses canonical artifacts.list and artifacts.preview against Runtime');
  const crossService = [];
  const crossServiceReads = [];
  const crossServiceErrors = [];
  const observeServiceRequest = request => {
    if ([new URL(editor.editor_url).origin, new URL(trace.web_url).origin].includes(new URL(request.url()).origin)) {
      crossServiceReads.push((async () => {
        const headers = await request.allHeaders();
        const requestData = JSON.stringify({url: request.url(), headers, body: request.postData()});
        crossService.push({url: request.url(), method: request.method(), main_credential: requestData.includes(tokens.web) || requestData.includes(tokens.ipc), referer: headers.referer || null});
      })().catch(error => { crossServiceErrors.push(redact(error.message)); }));
    }
  };
  context.on('request', observeServiceRequest);
  await page.locator('#wb-editor').click();
  await panel().locator('input[id^="ed-addr-"]').fill(editor.editor_url);
  const editorOpened = context.waitForEvent('page');
  await panel().getByRole('button', {name: '新窗口打开编辑器服务', exact: true}).click();
  const editorPage = await editorOpened;
  await editorPage.waitForLoadState();
  assert.equal(editorPage.url(), editor.editor_url);
  assert.equal(await editorPage.evaluate(() => window.opener), null);
  assert.equal(new URL(editorPage.url()).search, '');
  assert.equal(await editorPage.locator('#token-input').inputValue(), '');
  assert.equal((await fetch(`${editor.editor_url}api/document`)).status, 401);
  const editorDocument = editorPage.waitForResponse(response => response.url().endsWith('/api/document') && response.status() === 200);
  await editorPage.locator('#token-input').fill(tokens.editor);
  await editorPage.locator('#connect-submit').click();
  const document = await (await editorDocument).json();
  await editorPage.locator('#workspace').waitFor({state: 'visible'});
  await editorPage.frameLocator('#preview-frame').locator('#project-proof').waitFor();
  assert.equal(await editorPage.frameLocator('#preview-frame').locator('#project-proof').textContent(), 'Workbench real disk project');
  assert(document.snapshot.files.some(file => file.path === 'index.html' && file.source.includes('Workbench real disk project')));
  await record('editor-document', document);
  await screenshot(editorPage, 'editor-real-project');
  checks('Workbench opens actual Editor with no opener/query credentials; separate login reads the disk project');
  await page.locator('#wb-trace').click();
  await panel().locator('input[id^="trace-addr-"]').fill(trace.web_url);
  const traceOpened = context.waitForEvent('page');
  const traceNavigation = context.waitForEvent('response', response => response.url() === trace.web_url && response.request().isNavigationRequest());
  await panel().getByRole('button', {name: '新窗口打开 Trace Debug', exact: true}).click();
  const tracePage = await traceOpened;
  const traceResponse = await traceNavigation;
  await tracePage.waitForLoadState();
  await record('trace-entry', {url: tracePage.url(), status: traceResponse.status(), request_headers: await traceResponse.request().allHeaders(), response_headers: await traceResponse.allHeaders(), title: await tracePage.title(), body: await traceResponse.text()});
  await screenshot(tracePage, 'trace-entry');
  assert.equal(traceResponse.status(), 200, 'Trace must serve its login page for the actual workbench navigation');
  assert.equal(tracePage.url(), trace.web_url);
  assert.equal(await tracePage.evaluate(() => window.opener), null);
  assert.equal(new URL(tracePage.url()).search, '');
  assert.equal(await tracePage.locator('#web-token').inputValue(), '');
  assert.equal((await fetch(`${trace.web_url}api/state`)).status, 401);
  const captured = await until('actual Trace capture', async () => {
    const response = await fetch(`${trace.web_url}api/state`, {headers: {Authorization: `Bearer ${tokens.trace}`}, signal: AbortSignal.timeout(5000)});
    assert.equal(response.status, 200);
    return response.json();
  }, doc => doc.sessions?.[session]?.after >= secondPlan.cursor && Object.values(doc.projects || {}).some(project => project.session_id === session && project.runtime_status === 'completed' && project.calls.some(call => call.name === 'fs.write')));
  const traceLoginResponse = tracePage.waitForResponse(response => new URL(response.url()).pathname === '/api/state' && response.status() === 200);
  await tracePage.locator('#web-token').fill(tokens.trace);
  await tracePage.locator('#login-button').click();
  const browserTrace = await (await traceLoginResponse).json();
  await tracePage.locator('#workspace').waitFor({state: 'visible'});
  await tracePage.locator('#project-select option').first().waitFor({state: 'attached'});
  await tracePage.locator('#project-summary').getByText(session, {exact: true}).waitFor();
  const selectedProject = await tracePage.locator('#project-select').inputValue();
  assert.equal(browserTrace.projects[selectedProject].session_id, session);
  assert.equal(browserTrace.projects[selectedProject].runtime_status, 'completed');
  assert(browserTrace.projects[selectedProject].calls.some(call => call.name === 'fs.write'));
  assert(browserTrace.sessions[session].after >= secondPlan.cursor);
  await record('trace-browser-document', browserTrace);
  await record('trace-document', captured);
  await screenshot(tracePage, 'trace-real-runtime');
  context.off('request', observeServiceRequest);
  await Promise.all(crossServiceReads);
  assert.deepEqual(crossServiceErrors, [], 'All cross-service credential checks must complete');
  assert(crossService.length > 0);
  assert(crossService.every(request => !request.main_credential));
  assert(crossService.filter(request => request.method === 'GET' && [editor.editor_url, trace.web_url].includes(request.url)).every(request => !request.referer));
  assert(Object.values(captured.projects).some(project => project.session_id === session && project.runtime_status === 'completed'));
  await record('service-isolation', {gateway_csp: csp, requests: crossService, main_credential_forwarded: false, opener_isolated: true});
  checks('Workbench opens actual Trace; separate login reads real Core events and Host calls');
  assert.deepEqual(browserErrors, []);
  await record('browser-errors', browserErrors);
  assert.equal(providerRequests, 2);
  summary.fixed_local_model_requests = providerRequests;
  summary.passed = true;
} catch (error) {
  summary.error = redact(error.stack || error.message);
  process.exitCode = 1;
  if (browser) {
    for (const [index, failedPage] of browser.contexts().flatMap(context => context.pages()).entries()) {
      try {
        await screenshot(failedPage, `failure-page-${index + 1}`);
        await record(`failure-page-${index + 1}`, {url: failedPage.url(), title: await failedPage.title(), html: await failedPage.content()});
      } catch (captureError) { (summary.failure_capture_errors ||= []).push(redact(captureError.message)); }
    }
  }
} finally {
  if (browser) await browser.close().catch(error => { summary.browser_cleanup_error = redact(error.message); summary.passed = false; process.exitCode = 1; });
  for (const service of services.reverse()) {
    await stop(service).catch(error => { (summary.cleanup_errors ||= []).push(redact(error.message)); summary.passed = false; process.exitCode = 1; });
  }
  if (provider) { provider.closeAllConnections(); await new Promise(accept => provider.close(accept)); }
  const after = Object.fromEntries(await Promise.all(sourcePaths.map(async path => [path, await digest(resolve(root, path))])));
  summary.changed_sources = sourcePaths.filter(path => sourceReceipt[path] !== after[path]);
  summary.changed_binaries = [];
  for (const [name, receipt] of Object.entries(binaryReceipt)) if (receipt.sha256 !== await digest(receipt.path)) summary.changed_binaries.push(name);
  if (summary.changed_sources.length || summary.changed_binaries.length) { summary.passed = false; process.exitCode = 1; }
  summary.fixed_local_model_requests = providerRequests;
  summary.finished_utc = new Date().toISOString();
  await record('summary', summary);
  console.log(JSON.stringify({passed: summary.passed, out, assertions: summary.assertions.length, fixed_local_model_requests: providerRequests, paid_model_requests: 0, error: summary.error, cleanup_errors: summary.cleanup_errors}, null, 2));
}
