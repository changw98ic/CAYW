// Run after serve-webui-validation.py. Uses only public loopback fixture tokens.
// CAYW_WEB_VALIDATION_URL=http://127.0.0.1:PORT/ node scripts/validate-webui.mjs
// Provide Playwright in the host development environment; no browser dependency
// is required to run or distribute the Rust WebUI itself.
import { chromium } from 'playwright';
import { mkdir, writeFile } from 'node:fs/promises';
import { resolve } from 'node:path';

const url = process.env.CAYW_WEB_VALIDATION_URL;
if (!url || !/^http:\/\/127\.0\.0\.1:\d+\/$/.test(url)) throw new Error('Set CAYW_WEB_VALIDATION_URL to the local fixture URL.');
const output = resolve(process.env.CAYW_WEB_VALIDATION_OUT ?? 'webui-browser-evidence');
await mkdir(output, { recursive: false });
const browser = await chromium.launch();
const context = await browser.newContext({ viewport: { width: 1280, height: 900 } });
const page = await context.newPage();
const checks = [];
const errors = [];
const counts = { prompts: 0, approvals: 0 };
page.on('pageerror', error => errors.push(error.message));
page.on('request', request => {
  if (!request.url().endsWith('/api/rpc')) return;
  try {
    const method = request.postDataJSON()?.method;
    if (method === 'session.prompt') counts.prompts++;
    if (method === 'approval.decide') counts.approvals++;
  } catch {}
});
const summary = { url, started_utc: new Date().toISOString(), source: 'synthetic provider; actual Runtime/Luau/Host/WebUI', checks, counts, errors, passed: false };
try {
  await page.goto(url);
  await page.locator('#web-token').fill('incorrect-local-fixture-token');
  await page.locator('#login-button').click();
  await page.locator('#login-error').filter({ hasText: '令牌无效' }).waitFor();
  checks.push('incorrect_token_rejected');
  await page.locator('#web-token').fill('cayw-local-browser-fixture-web-only');
  await page.locator('#login-button').click();
  await page.locator('#workspace').waitFor({ state: 'visible' });
  checks.push('runtime_health_login');
  await page.locator('#session-label').fill('浏览器自动验收');
  await page.getByRole('button', { name: '创建会话' }).click();
  await page.locator('#session-title').filter({ hasText: '浏览器自动验收' }).waitFor();
  await page.getByLabel('发送消息', { exact: true }).fill('通过真实 Luau 写入并读取文件。');
  await page.keyboard.press('Control+Enter');
  for (let index = 0; index < 2; index++) {
    await page.getByRole('button', { name: '允许一次', exact: true }).waitFor({ state: 'visible', timeout: 7000 });
    await page.getByRole('button', { name: '允许一次', exact: true }).click();
  }
  await page.locator('#session-state').filter({ hasText: '就绪' }).waitFor({ timeout: 10000 });
  await page.locator('#messages').filter({ hasText: 'headless runtime demo' }).waitFor();
  if (counts.prompts !== 1 || counts.approvals !== 2) throw new Error('unexpected prompt or approval count');
  checks.push('keyboard_send', 'two_explicit_browser_approvals', 'real_luau_file_result');
  await page.waitForFunction(() => document.querySelector('#event-count').textContent.includes('游标'));
  const promptsBeforeReconnect = counts.prompts;
  await page.route('**/api/rpc', async route => {
    if (route.request().postDataJSON()?.method === 'session.events') await route.abort('connectionreset');
    else await route.continue();
  });
  await page.locator('#connection-status').filter({ hasText: '正在重连' }).waitFor({ timeout: 7000 });
  await page.unroute('**/api/rpc');
  await page.locator('#connection-status').filter({ hasText: '已连接本机' }).waitFor({ timeout: 12000 });
  if (counts.prompts !== promptsBeforeReconnect) throw new Error('reconnect repeated a prompt');
  checks.push('cursor_reconnect_without_prompt_replay');
  await page.locator('#session-label').fill('浏览器取消验收');
  await page.getByRole('button', { name: '创建会话' }).click();
  await page.locator('#session-title').filter({ hasText: '浏览器取消验收' }).waitFor();
  await page.getByLabel('发送消息', { exact: true }).fill('等待审批时取消');
  await page.getByRole('button', { name: '发送', exact: true }).click();
  await page.getByRole('button', { name: '允许一次', exact: true }).waitFor({ state: 'visible', timeout: 6000 });
  await page.getByRole('button', { name: '停止运行', exact: true }).click();
  await page.getByRole('button', { name: '允许一次', exact: true }).waitFor({ state: 'hidden', timeout: 6000 });
  await page.waitForFunction(() => document.querySelector('#cancel-button').disabled);
  checks.push('cancel_while_awaiting_approval');
  await page.screenshot({ path: resolve(output, 'desktop.png'), fullPage: true });
  await page.setViewportSize({ width: 390, height: 844 });
  const layout = await page.evaluate(() => ({
    horizontalOverflow: document.documentElement.scrollWidth > innerWidth,
    storageEmpty: localStorage.length === 0 && sessionStorage.length === 0,
    tokenInputEmpty: document.querySelector('#web-token').value === '',
    tokenInUrl: location.search.length > 0,
    missingLabels: [...document.querySelectorAll('input,textarea')].filter(input => !input.labels?.length).map(input => input.id),
  }));
  summary.layout = layout;
  if (layout.horizontalOverflow || !layout.storageEmpty || !layout.tokenInputEmpty || layout.tokenInUrl || layout.missingLabels.length) throw new Error('mobile, labels or token retention check failed');
  checks.push('phone_layout', 'explicit_input_labels', 'no_browser_token_storage');
  await page.screenshot({ path: resolve(output, 'mobile.png'), fullPage: true });
  if (errors.length) throw new Error('unexpected page errors');
  summary.passed = true;
} catch (error) {
  summary.error = error.message;
  process.exitCode = 1;
} finally {
  summary.finished_utc = new Date().toISOString();
  await writeFile(resolve(output, 'summary.json'), `${JSON.stringify(summary, null, 2)}\n`);
  await browser.close();
  console.log(JSON.stringify(summary, null, 2));
}
