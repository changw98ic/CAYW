// macOS CLI -> unique Client -> Client-owned WebUI -> Playwright -> offline Runtime.
// Uses a new private HOME and evidence directory. It never opens the user's browser.
// node scripts/validate-browser-launch.mjs BIN_DIRECTORY NEW_EVIDENCE_DIR
import assert from 'node:assert/strict';
import { spawn, execFileSync } from 'node:child_process';
import { mkdir, readFile, writeFile, readdir } from 'node:fs/promises';
import { join, resolve } from 'node:path';
import { createInterface } from 'node:readline';
import { once } from 'node:events';
import { setTimeout as delay } from 'node:timers/promises';

assert.equal(process.platform, 'darwin', 'browser launch validation is macOS-only');
const bin = resolve(process.argv[2]);
const out = resolve(process.argv[3]);
await mkdir(out);
const home = join(out, 'home');
const workspace = join(out, 'workspace');
const secondWorkspace = join(out, 'workspace-second');
await mkdir(home, { mode: 0o700 });
await mkdir(workspace);
await mkdir(secondWorkspace);

const { chromium } = await import(process.env.CAYW_PLAYWRIGHT_MODULE || 'playwright');
const env = {
  PATH: process.env.PATH,
  HOME: home,
  CAYW_HOME: join(home, 'cayw'),
};
execFileSync(join(bin, 'cayw'), ['init', '--workspace', secondWorkspace], { env, stdio: 'pipe' });

async function assertNoLauncherState() {
  const roots = await readdir(join(env.CAYW_HOME, 'workspaces'), { withFileTypes: true });
  for (const root of roots.filter(entry => entry.isDirectory())) {
    const names = await readdir(join(env.CAYW_HOME, 'workspaces', root.name));
    assert(!names.some(name => name.startsWith('launcher-')), `legacy launcher state remains in ${root.name}`);
  }
}

const children = [];
const checks = [];
const logs = [];
let browser;

async function until(fn) {
  const end = Date.now() + 30000;
  while (true) {
    const value = await fn();
    if (value) return value;
    assert(Date.now() < end, 'timed out');
    await delay(50);
  }
}

async function launch(args, overrides = {}) {
  const child = spawn(join(bin, 'cayw'), args, {
    env: { ...env, ...overrides },
    stdio: ['ignore', 'pipe', 'pipe'],
    detached: true,
  });
  children.push(child);
  let stderr = '';
  child.stderr.on('data', bytes => { stderr += bytes; });
  logs.push(() => stderr);
  const lines = createInterface({ input: child.stdout });
  const [line] = await Promise.race([
    once(lines, 'line'),
    once(child, 'exit').then(([code]) => { throw new Error(`startup exited ${code}: ${stderr}`); }),
    new Promise((_, reject) => setTimeout(() => reject(new Error(`startup timeout: ${stderr}`)), 40000).unref()),
  ]);
  return { child, ready: JSON.parse(line) };
}

async function repeat(args, overrides = {}) {
  const result = await launch(args, overrides);
  if (result.child.exitCode === null) await once(result.child, 'exit');
  assert.equal(result.child.exitCode, 0);
  return result.ready;
}

async function stop(child) {
  if (child.exitCode === null && child.signalCode === null) {
    child.kill('SIGINT');
    const [code] = await once(child, 'exit');
    assert.equal(code, 0);
  }
}

try {
  const app = await launch(['--ui', 'web', '--workspace', workspace, '--offline', '--demo-authorized', '--no-open']);
  assert.equal(app.ready.browser_open, 'disabled');
  assert.equal(app.ready.access, 'local');
  assert.equal(app.ready.login_required, false);
  assert.equal(app.ready.owned_client, true);
  assert.equal(app.ready.reused, false);
  assert.equal(app.ready.workspace_id, 'local');
  assert.equal(app.ready.open_url, `${app.ready.url}#workspace=local`);
  assert(app.ready.launch_id);
  assert(app.ready.client);
  assert(!app.ready.launch_url && !app.ready.web_token_file);
  await assertNoLauncherState();

  browser = await chromium.launch({
    headless: true,
    ...(process.env.CAYW_CHROMIUM ? { executablePath: process.env.CAYW_CHROMIUM } : {}),
  });
  const context = await browser.newContext();
  const page = await context.newPage();
  const errors = [];
  page.on('pageerror', error => errors.push(error.message));
  await page.goto(app.ready.open_url);
  await page.locator('#workspace').waitFor({ state: 'visible' });
  assert.equal(page.url(), app.ready.open_url);
  assert.equal((await context.cookies(app.ready.url)).filter(cookie => cookie.name.startsWith('cayw_session_')).length, 0);
  assert.equal(await page.locator('#web-token').count(), 0);
  assert(await page.locator('#logout').isHidden());
  assert.equal(await page.evaluate(() => localStorage.length + sessionStorage.length), 0);

  const rpc = async base => fetch(`${base}api/rpc`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ method: 'runtime.health', params: {} }),
  });
  assert.equal((await rpc(app.ready.url)).status, 200);
  assert.equal((await fetch(`${app.ready.url}?launch=old`)).status, 400);
  assert.equal((await fetch(`${app.ready.url}api/browser/launch`, { method: 'POST' })).status, 404);

  const requests = [];
  page.on('request', request => {
    if (request.url().endsWith('/api/rpc')) {
      assert(!request.headers().authorization);
      requests.push(request.postDataJSON().method);
    }
  });
  await page.locator('#session-label').fill('Local without authentication');
  await page.locator('#create-button').click();
  await page.locator('#session-title').filter({ hasText: 'Local without authentication' }).waitFor();
  await page.locator('#prompt').fill('offline task');
  await page.locator('#send-button').click();
  await page.waitForFunction(() => document.querySelector('#messages').textContent.includes('headless runtime demo'));
  assert.equal(await readFile(join(workspace, 'cayw-demo.txt'), 'utf8'), 'headless runtime demo');
  await page.reload();
  await page.locator('#session-title').filter({ hasText: 'Local without authentication' }).waitFor();
  assert.equal(requests.filter(method => method === 'task.enqueue').length, 1);
  checks.push('macOS Playwright opened the Client-scoped URL; local access needed no account or browser token; a real offline task and refresh did not replay');

  const second = await repeat(['--ui', 'web', '--workspace', workspace, '--no-open']);
  assert.equal(second.url, app.ready.url);
  assert.equal(second.browser_open, 'disabled');
  assert.equal(second.launch_id, app.ready.launch_id);
  assert.equal(second.client, app.ready.client);
  assert.equal(second.owned_client, false);

  const scoped = await repeat(['--ui', 'web', '--workspace', secondWorkspace, '--no-open']);
  assert.equal(scoped.url, app.ready.url);
  assert.equal(scoped.launch_id, app.ready.launch_id);
  assert.equal(scoped.client, app.ready.client);
  assert.equal(scoped.reused, true);
  assert.equal(scoped.owned_client, false);
  assert.match(scoped.workspace_id, /^workspace-[0-9a-f]{64}$/);
  assert.equal(scoped.open_url, `${app.ready.url}#workspace=${scoped.workspace_id}`);
  const scopedPage = await context.newPage();
  await scopedPage.goto(scoped.open_url);
  await scopedPage.locator('#workspace').waitFor({ state: 'visible' });
  await scopedPage.waitForFunction(
    workspaceId => document.querySelector('#workspace-choice')?.value === workspaceId,
    scoped.workspace_id,
  );
  assert.equal(await scopedPage.locator('#workspace-choice').inputValue(), scoped.workspace_id);
  assert.equal(page.url(), app.ready.open_url);
  await scopedPage.close();
  await assertNoLauncherState();

  for (const overrides of [{ SSH_CONNECTION: 'synthetic ssh session' }, { SSH_TTY: '/dev/synthetic' }]) {
    const value = await repeat(['--ui', 'web', '--workspace', workspace], overrides);
    assert.equal(value.browser_open, 'ssh');
    assert.equal(value.url, app.ready.url);
  }
  checks.push('one Client and one WebUI served two independent workspace pages; duplicate and SSH starts reused them; no workspace launcher files were created');

  await stop(app.child);

  const openLan = await launch(['--ui', 'web', '--lan', '127.0.0.1:0', '--workspace', workspace, '--offline', '--no-open']);
  assert.equal(openLan.ready.access, 'lan');
  assert.equal(openLan.ready.login_required, false);
  assert.equal((await rpc(openLan.ready.url)).status, 200);
  await page.goto(openLan.ready.open_url);
  await page.locator('#workspace').waitFor({ state: 'visible' });
  await page.locator('#network-notice').filter({ hasText: '未配置账号' }).waitFor();
  assert(await page.locator('#logout').isHidden());
  await page.reload();
  await page.locator('#workspace').waitFor({ state: 'visible' });
  await stop(openLan.child);
  checks.push('loopback LAN mode without an account accepted requests and showed its exposure notice');

  const fixture = JSON.parse(execFileSync('python3', [
    'scripts/validate-default-entry.py',
    '--account-fixture',
    bin,
    join(out, 'account-fixture'),
  ], { encoding: 'utf8' }));
  const lan = await launch([
    '--ui', 'web', '--lan', '127.0.0.1:0', '--account-file', fixture.account_file,
    '--workspace', workspace, '--offline', '--no-open',
  ]);
  assert.equal(lan.ready.login_required, false);
  assert.equal(lan.ready.lan_login_enabled, true);
  assert.notEqual(lan.ready.local_url, lan.ready.url);
  assert.equal((await rpc(lan.ready.url)).status, 401);
  assert.equal((await rpc(lan.ready.local_url)).status, 200);
  await page.goto(`${lan.ready.url}#workspace=${lan.ready.workspace_id}`);
  await page.locator('#account-fields').waitFor({ state: 'visible' });
  assert(await page.locator('#workspace').isHidden());
  await page.locator('#username').fill('browser-operator');
  await page.locator('#password').fill('wrong-password');
  await page.locator('#login-button').click();
  await page.locator('#login-error').filter({ hasText: '账号或密码错误' }).waitFor();
  assert.equal(await page.locator('#password').inputValue(), '');
  await page.locator('#password').fill('synthetic-password-for-isolated-tests');
  await page.locator('#login-button').click();
  await page.locator('#workspace').waitFor({ state: 'visible' });
  assert.equal(await page.locator('#password').inputValue(), '');
  assert(await page.locator('#logout').isVisible());
  const cookie = (await context.cookies(lan.ready.url)).find(item => item.name.startsWith('cayw_session_'));
  assert(cookie?.httpOnly && cookie.sameSite === 'Strict');
  assert.equal(await page.evaluate(() => localStorage.length + sessionStorage.length), 0);
  assert(!(await page.evaluate(() => document.cookie)).includes('cayw_session_'));
  await page.reload();
  await page.locator('#workspace').waitFor({ state: 'visible' });
  await page.locator('#logout').click();
  await page.locator('#account-fields').waitFor({ state: 'visible' });
  assert(await page.locator('#workspace').isHidden());
  await page.reload();
  await page.locator('#account-fields').waitFor({ state: 'visible' });
  assert(await page.locator('#workspace').isHidden());
  await stop(lan.child);
  checks.push('the optional local account denied anonymous LAN RPC, rejected a bad password, restored a valid session, and revoked it on logout');

  const external = await launch(['serve', '--workspace', workspace, '--offline']);
  const reused = await launch(['--ui', 'web', '--workspace', workspace, '--no-open']);
  assert.equal(reused.ready.owned_client, false);
  await page.goto(reused.ready.open_url);
  await page.locator('#workspace').waitFor({ state: 'visible' });
  await stop(reused.child);
  assert.equal(external.child.exitCode, null);
  execFileSync(join(bin, 'cayw'), ['models', 'list', '--workspace', workspace], { env, stdio: 'pipe' });
  await stop(external.child);
  checks.push('a WebUI command reused and preserved an independently owned Client');

  const simultaneous = await Promise.all([
    launch(['--ui', 'web', '--workspace', workspace, '--no-open']),
    launch(['--ui', 'web', '--workspace', workspace, '--no-open']),
  ]);
  assert.equal(simultaneous[0].ready.url, simultaneous[1].ready.url);
  assert.equal(simultaneous[0].ready.launch_id, simultaneous[1].ready.launch_id);
  assert.equal(simultaneous[0].ready.client, simultaneous[1].ready.client);
  await until(() => simultaneous.some(item => item.child.exitCode !== null));
  for (const item of simultaneous) await stop(item.child);
  checks.push('concurrent WebUI starts shared one Client and one gateway');

  assert.deepEqual(errors, []);
  await writeFile(join(out, 'summary.json'), `${JSON.stringify({
    passed: true,
    platform: process.platform,
    checks,
    errors,
    browser: process.env.CAYW_CHROMIUM || 'Playwright bundled Chromium',
    system_browser_opened: false,
    lan_network: 'isolated loopback listener',
  }, null, 2)}\n`);
  console.log(JSON.stringify({ passed: true, checks }));
} catch (error) {
  console.error(error);
  throw error;
} finally {
  await browser?.close();
  for (const child of children.reverse()) await stop(child);
  await writeFile(join(out, 'processes.stderr'), logs.map(read => read()).join('\n'));
}
