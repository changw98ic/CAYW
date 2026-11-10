// macOS browser -> WebUI -> unique Client -> workspace Runtime -> live model
// -> approval -> Host effect -> final browser rendering acceptance.
//
// CAYW_PLAYWRIGHT_MODULE=/absolute/playwright/index.mjs \
// CAYW_CHROMIUM='/Applications/Google Chrome.app/Contents/MacOS/Google Chrome' \
// node scripts/validate-live-webui-e2e.mjs TARGET_DEBUG NEW_EVIDENCE_DIR
import assert from 'node:assert/strict';
import {spawn, execFileSync} from 'node:child_process';
import {createHash} from 'node:crypto';
import {mkdir, readFile, realpath, writeFile} from 'node:fs/promises';
import {dirname, resolve} from 'node:path';
import {fileURLToPath, pathToFileURL} from 'node:url';
import {once} from 'node:events';
import {release} from 'node:os';
import {setTimeout as delay} from 'node:timers/promises';

assert.equal(process.platform, 'darwin', 'This validation is scoped to macOS.');
const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const binDir = resolve(process.argv[2] || resolve(root, 'target/debug'));
assert(process.argv[3], 'Provide a new evidence directory.');
const out = resolve(process.argv[3]);
const modelId = process.env.CAYW_E2E_MODEL || 'auto/deepseek-v4.1-flash';
const modelBaseUrl = process.env.CAYW_E2E_BASE_URL || 'http://127.0.0.1:3425/v1';
const playwrightName = process.env.CAYW_PLAYWRIGHT_MODULE || 'playwright';
const {chromium} = await import(playwrightName.startsWith('/') ? pathToFileURL(playwrightName).href : playwrightName);

await mkdir(out, {recursive: false});
const work = resolve(out, 'work');
const workspace = resolve(work, 'workspace');
const home = resolve(work, 'home');
const caywHome = resolve(home, 'cayw');
await mkdir(workspace, {recursive: true});
await mkdir(home, {mode: 0o700});

const binary = resolve(binDir, 'cayw');
const webuiBinary = resolve(binDir, 'cayw-webui');
const sha = bytes => createHash('sha256').update(bytes).digest('hex');
const digest = async path => sha(await readFile(path));
const secrets = new Set();
const redact = value => [...secrets].reduce(
  (text, secret) => secret ? text.replaceAll(secret, '[redacted]') : text,
  String(value),
);
const record = (name, value) => writeFile(
  resolve(out, `${name}.json`),
  `${redact(JSON.stringify(value, null, 2))}\n`,
  {flag: 'wx'},
);
const sourcePaths = [
  'Cargo.toml',
  'Cargo.lock',
  'scripts/validate-live-webui-e2e.mjs',
  'crates/cayw-runtime/src/main.rs',
  'crates/cayw-runtime/src/cli_workspaces.rs',
  'crates/cayw-runtime/src/cli_settings.rs',
  'crates/cayw-runtime/src/provider.rs',
  'crates/cayw-runtime/src/session.rs',
  'crates/cayw-agent/src/agent.rs',
  'crates/cayw-agent/src/execution.rs',
  'crates/cayw-ai/src/providers/openai.rs',
  'crates/cayw-webui/src/lib.rs',
  'crates/cayw-webui/assets/app.js',
  'crates/cayw-webui/assets/index.html',
];
const sourceReceipt = Object.fromEntries(await Promise.all(
  sourcePaths.map(async path => [path, await digest(resolve(root, path))]),
));
const binaryReceipt = await digest(binary);
const webuiBinaryReceipt = await digest(webuiBinary);
await record('source-receipt', sourceReceipt);

const summary = {
  schema: 'cayw.live-webui-e2e-validation.v1',
  started_utc: new Date().toISOString(),
  platform: process.platform,
  architecture: process.arch,
  os_release: release(),
  node: process.version,
  base_commit: execFileSync('git', ['rev-parse', 'HEAD'], {cwd: root, encoding: 'utf8'}).trim(),
  working_diff_sha256: sha(execFileSync(
    'git',
    ['diff', '--binary', 'HEAD'],
    {cwd: root, maxBuffer: 64 * 1024 * 1024},
  )),
  binary: {path: binary, sha256: binaryReceipt},
  webui_binary: {path: webuiBinary, sha256: webuiBinaryReceipt},
  model: {requested: modelId, base_url: modelBaseUrl, authentication: 'none'},
  assertions: [],
  passed: false,
};
const checks = value => summary.assertions.push(value);
const processState = {child: null, stdout: '', stderr: ''};
let browser;
let page;
let browserErrors = [];
let rpcTrace = [];

async function until(label, read, accept, timeout = 180000) {
  const deadline = Date.now() + timeout;
  let value;
  do {
    value = await read();
    if (accept(value)) return value;
    await delay(100);
  } while (Date.now() < deadline);
  throw new Error(`${label} did not become ready; last value: ${redact(JSON.stringify(value))}`);
}

async function launch() {
  const child = spawn(binary, ['start', '--ui', 'web', '--no-open', '--workspace', workspace], {
    cwd: root,
    env: {
      PATH: process.env.PATH,
      TMPDIR: process.env.TMPDIR,
      LANG: 'en_US.UTF-8',
      HOME: home,
      CAYW_HOME: caywHome,
      CAYW_MAIN_SESSION: resolve(work, 'main-session'),
    },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  processState.child = child;
  const ready = new Promise((accept, reject) => {
    let parsed = false;
    child.on('error', reject);
    child.on('exit', (code, signal) => {
      if (!parsed) {
        reject(new Error(`cayw exited before readiness (${code}/${signal}): ${redact(processState.stderr)}`));
      }
    });
    child.stderr.on('data', bytes => { processState.stderr += bytes; });
    child.stdout.on('data', bytes => {
      processState.stdout += bytes;
      if (!parsed && processState.stdout.includes('\n')) {
        try {
          parsed = true;
          accept(JSON.parse(processState.stdout.split('\n')[0]));
        } catch (error) {
          reject(error);
        }
      }
    });
  });
  const timeout = new Promise((_, reject) => {
    setTimeout(() => reject(new Error('cayw startup timeout')), 45000);
  });
  const value = await Promise.race([ready, timeout]);
  try {
    secrets.add((await readFile(resolve(caywHome, 'ipc.token'), 'utf8')).trim());
  } catch {}
  await record('startup', value);
  return value;
}

async function stop() {
  const child = processState.child;
  if (!child) return;
  let status = [child.exitCode, child.signalCode];
  if (child.exitCode === null && child.signalCode === null) {
    const ended = once(child, 'exit');
    child.kill('SIGINT');
    const timer = setTimeout(() => child.kill('SIGKILL'), 15000);
    try {
      status = await ended;
    } finally {
      clearTimeout(timer);
    }
  }
  await writeFile(resolve(out, 'cayw-stdout.log'), redact(processState.stdout), {flag: 'wx'});
  await writeFile(resolve(out, 'cayw-stderr.log'), redact(processState.stderr), {flag: 'wx'});
  await record('shutdown', {exit_code: status[0], signal: status[1]});
  assert.equal(status[0], 0, `cayw did not stop cleanly: ${JSON.stringify(status)}`);
}

async function screenshot(name) {
  if (!page) return;
  await page.locator('input[type="password"]').evaluateAll(inputs => {
    for (const input of inputs) input.value = '';
  });
  await page.screenshot({path: resolve(out, `${name}.png`), fullPage: true});
}

try {
  const catalogResponse = await fetch(`${modelBaseUrl.replace(/\/$/, '')}/models`, {
    signal: AbortSignal.timeout(10000),
  });
  assert.equal(catalogResponse.status, 200, 'The selected live model gateway is unavailable.');
  const catalog = await catalogResponse.json();
  assert(
    Array.isArray(catalog.data) && catalog.data.some(model => model.id === modelId),
    `The model catalog does not contain ${modelId}.`,
  );
  await record('model-catalog', {
    base_url: modelBaseUrl,
    entries: catalog.data.length,
    selected: modelId,
  });
  checks('The live gateway catalog contains the selected model before product startup');

  const ready = await launch();
  assert.equal(ready.owned_client, true);
  assert.equal(ready.workspace_id, 'local');
  assert.equal(ready.access, 'local');
  assert.equal(ready.login_required, false);
  assert.equal(ready.browser_open, 'disabled');
  checks('A fresh isolated macOS product start owns one Client and one Client-managed WebUI');

  async function rpc(method, params = {}) {
    const response = await fetch(`${ready.url}api/rpc`, {
      method: 'POST',
      headers: {'Content-Type': 'application/json', Origin: new URL(ready.url).origin},
      body: JSON.stringify({method, params, workspace_id: ready.workspace_id}),
      signal: AbortSignal.timeout(180000),
    });
    const body = await response.json();
    assert(response.ok && !body.error, `${method}: ${JSON.stringify(body)}`);
    return body.result;
  }

  const beforeHealth = await rpc('runtime.health');
  const beforeModels = await rpc('settings.models.list');
  assert.equal(beforeHealth.status, 'ok');
  assert.equal(beforeModels.selected, null);
  assert.deepEqual(beforeModels.models, {});
  checks('The fresh Client, Runtime and WebUI start without a selected model');

  browser = await chromium.launch({
    headless: true,
    ...(process.env.CAYW_CHROMIUM
      ? {executablePath: process.env.CAYW_CHROMIUM}
      : {channel: 'chrome'}),
  });
  summary.browser_version = browser.version();
  const context = await browser.newContext({
    viewport: {width: 1440, height: 1000},
    reducedMotion: 'reduce',
  });
  page = await context.newPage();
  browserErrors = [];
  rpcTrace = [];
  page.on('pageerror', error => browserErrors.push(error.message));
  page.on('console', message => {
    if (message.type() === 'error') browserErrors.push(message.text());
  });
  page.on('response', response => {
    if (!response.url().endsWith('/api/rpc')) return;
    let method = 'unknown';
    try {
      method = response.request().postDataJSON()?.method || method;
    } catch {}
    rpcTrace.push({method, status: response.status()});
  });

  await page.goto(ready.open_url || ready.url);
  await page.locator('#workspace').waitFor({state: 'visible'});
  await page.locator('#open-settings').click();
  await page.locator('#settings-status').filter({hasText: '已完成'}).waitFor();
  const form = page.locator('#model-settings-form');
  await form.locator('[name=name]').fill('live-browser-e2e');
  await form.locator('[name=service]').selectOption('magpie');
  await form.locator('[name=api]').selectOption('chat_completions');
  await form.locator('[name=base_url]').fill(modelBaseUrl);
  await form.locator('[name=id]').fill(modelId);
  await form.locator('[name=api_version]').fill('v1');
  await form.locator('[name=max_output_tokens]').fill('2048');
  assert.equal(await form.locator('[name=api_key]').inputValue(), '');
  await form.locator('button[type=submit]').click();
  const profile = page.locator('#model-profiles > div').filter({hasText: 'live-browser-e2e'});
  await profile.waitFor();
  await profile.getByRole('button', {name: '使用', exact: true}).click();
  await page.locator('#model-profiles')
    .filter({hasText: `当前：magpie / ${modelId}`})
    .waitFor();
  await profile.filter({hasText: '已保存选择'}).waitFor();

  const models = await rpc('settings.models.list');
  assert.equal(models.selected, 'live-browser-e2e');
  assert.equal(models.models['live-browser-e2e'].authentication, 'none');
  assert.equal(models.health.provider, 'magpie');
  assert.equal(models.health.model, modelId);
  const storedModelsPath = resolve(caywHome, 'models.json');
  const storedModels = JSON.parse(await readFile(storedModelsPath, 'utf8'));
  assert.equal(Object.hasOwn(storedModels.models['live-browser-e2e'], 'api_key'), false);
  await record('model-configuration', {
    public: models,
    stored_without_api_key: true,
    stored_path: storedModelsPath,
  });
  await page.locator('#close-settings').click();
  checks('The browser saves and hot-selects a no-auth live model through settings RPC');

  await page.locator('#session-label').fill('Live browser E2E');
  await page.locator('#create-button').click();
  await page.getByRole('heading', {name: 'Live browser E2E', exact: true}).waitFor();
  const sessions = await rpc('session.list');
  assert.equal(sessions.length, 1);
  const sessionId = sessions[0].id;
  summary.session_id = sessionId;

  const fileMarker = 'CAYW_LIVE_WEBUI_E2E_FILE';
  const finalMarker = 'CAYW_LIVE_WEBUI_E2E_OK';
  const prompt = [
    'Complete this exact local acceptance task.',
    'You MUST call execute_code with action "exec" and this exact Luau script:',
    `return cayw.call('fs.write',{path='live-e2e.txt',content='${fileMarker}'})`,
    'Use yield_time_ms 30000 and max_output_tokens 32768.',
    'Do not call any other capability. Do not claim success before the tool result.',
    `After the tool reports success, reply with exactly ${finalMarker} and nothing else.`,
  ].join('\n');
  await page.locator('#prompt').fill(prompt);
  await page.locator('#send-button').click();

  const approval = page.locator('#approvals .approval-card').filter({hasText: '写入文件'});
  await approval.waitFor({state: 'visible', timeout: 180000});
  await approval.getByText('文件：live-e2e.txt', {exact: true}).waitFor();
  await screenshot('approval-visible');
  await approval.getByRole('button', {name: '允许一次', exact: true}).click();
  checks('The live model requests execute_code and the browser exposes the exact fs.write approval');

  await page.locator('#messages article.message.assistant .body')
    .filter({hasText: finalMarker})
    .waitFor({timeout: 180000});
  await page.locator('#session-state').filter({hasText: '就绪'}).waitFor({timeout: 30000});
  assert.equal(await readFile(resolve(workspace, 'live-e2e.txt'), 'utf8'), fileMarker);

  const snapshot = await rpc('session.get', {session_id: sessionId});
  const tasks = await rpc('session.tasks', {session_id: sessionId});
  const overview = await rpc('session.context_overview', {session_id: sessionId});
  const events = await rpc('session.events', {session_id: sessionId, after: 0, limit: 1000});
  const toolCalls = snapshot.messages.flatMap(message => Array.isArray(message.content)
    ? message.content.filter(part => part.type === 'toolCall' && part.name === 'execute_code')
    : []);
  const toolResults = snapshot.messages.filter(
    message => message.role === 'toolResult' && message.tool_name === 'execute_code',
  );
  const finalMessages = snapshot.messages.filter(message => (
    message.role === 'assistant'
      && Array.isArray(message.content)
      && message.content.some(part => part.type === 'text' && part.text.trim() === finalMarker)
  ));
  assert.equal(snapshot.state, 'idle');
  assert.equal(snapshot.pending_approvals.length, 0);
  assert.equal(snapshot.unresolved_calls.length, 0);
  assert.equal(snapshot.unresolved_capabilities.length, 0);
  assert.equal(tasks.length, 1);
  assert.equal(tasks[0].state, 'completed');
  assert.equal(toolCalls.length, 1);
  assert.equal(toolResults.length, 1);
  assert.equal(toolResults[0].is_error, false);
  assert.equal(toolResults[0].outcome_unknown, false);
  assert.equal(toolResults[0].structured_content?.status, 'completed');
  assert.equal(finalMessages.length, 1);
  assert.equal(overview.evolution.length, 2);
  assert(overview.evolution.every(turn => turn.usage.total_tokens > 0));
  summary.real_generation_requests = overview.evolution.length;
  summary.known_usage_tokens = overview.tokens.total_tokens;
  checks('One browser task completes a real two-request model/tool/model loop with one Host file effect');

  await screenshot('completed-conversation');
  await page.reload();
  await page.getByRole('heading', {name: 'Live browser E2E', exact: true}).waitFor();
  await page.locator('#messages article.message.assistant .body')
    .filter({hasText: finalMarker})
    .waitFor();
  assert.equal(await page.locator('#session-state').textContent(), '就绪');
  checks('A full page reload renders the durable final answer without replay');

  const requiredMethods = [
    'settings.models.put',
    'settings.models.use',
    'session.create',
    'task.enqueue',
    'task.resume',
    'approval.decide',
    'session.events',
    'session.get',
  ];
  for (const method of requiredMethods) {
    assert(
      rpcTrace.some(entry => entry.method === method && entry.status === 200),
      `Browser did not complete ${method}`,
    );
  }
  assert.deepEqual(browserErrors, []);
  await record('browser-rpc-trace', rpcTrace);
  await record('runtime-result', {
    session: snapshot,
    tasks,
    context_overview: overview,
    events,
    file: {path: 'live-e2e.txt', content: fileMarker},
    model: models,
  });
  await record('browser-errors', browserErrors);
  checks('Browser RPC trace, Runtime terminal state, events, usage and disk effect agree');
  summary.passed = true;
} catch (error) {
  summary.error = redact(error.stack || error.message);
  process.exitCode = 1;
  if (page) {
    await screenshot('failure').catch(captureError => {
      summary.capture_error = redact(captureError.message);
    });
    await writeFile(resolve(out, 'failure-page.html'), redact(await page.content()), {flag: 'wx'})
      .catch(() => {});
  }
  await record('browser-rpc-trace-failure', rpcTrace).catch(() => {});
  await record('browser-errors-failure', browserErrors).catch(() => {});
} finally {
  if (browser) {
    await browser.close().catch(error => {
      summary.browser_cleanup_error = redact(error.message);
      summary.passed = false;
      process.exitCode = 1;
    });
  }
  await stop().catch(error => {
    summary.cleanup_error = redact(error.message);
    summary.passed = false;
    process.exitCode = 1;
  });
  const changedSources = [];
  for (const [path, before] of Object.entries(sourceReceipt)) {
    if (before !== await digest(resolve(root, path))) changedSources.push(path);
  }
  summary.changed_sources = changedSources;
  summary.binary_changed = binaryReceipt !== await digest(binary);
  summary.webui_binary_changed = webuiBinaryReceipt !== await digest(webuiBinary);
  if (changedSources.length || summary.binary_changed || summary.webui_binary_changed) {
    summary.passed = false;
    summary.error ||= 'Product, validation source or binary changed during the run.';
    process.exitCode = 1;
  }
  summary.finished_utc = new Date().toISOString();
  await record('summary', summary);
  console.log(JSON.stringify({
    passed: summary.passed,
    out,
    assertions: summary.assertions.length,
    real_generation_requests: summary.real_generation_requests,
    known_usage_tokens: summary.known_usage_tokens,
    error: summary.error,
    cleanup_error: summary.cleanup_error,
  }, null, 2));
}
