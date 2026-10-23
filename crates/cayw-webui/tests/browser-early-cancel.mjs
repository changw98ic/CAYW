// Load with Playwright MCP browser_run_code_unsafe after starting
// runtime-browser.mjs with --approval-gated. All receipts/events come from the
// real offline Runtime; the proxy delays only their delivery.
async (page) => {
  const check = (condition, message) => { if (!condition) throw new Error(message); };
  const until = async (condition, message) => {
    for (let i = 0; i < 150; i++) { if (await condition()) return; await page.waitForTimeout(100); }
    throw new Error(message);
  };
  const control = async path => {
    const response = await page.request.post(new URL(`/__test__/${path}`, page.url()).href);
    check(response.ok(), await response.text());
    return response.json();
  };
  const status = async () => (await page.request.get(new URL('/__test__/status', page.url()).href)).json();
  const save = async (name, data) => {
    const response = await page.request.post(new URL('/__test__/evidence', page.url()).href, {data:{name,data}});
    check(response.ok(), await response.text());
  };
  const state = () => page.evaluate(() => ({
    promptDisabled: document.getElementById('prompt').disabled,
    stopDisabled: document.getElementById('cancel-button').disabled,
    forkDisabled: document.getElementById('open-fork').disabled,
    cursor: document.getElementById('event-count').textContent,
    notice: document.getElementById('blocked-text').textContent,
  }));
  const evidence = (await (await page.request.get(new URL('/__test__/manifest', page.url()).href)).json()).evidence;
  const capture = async (name, data) => {
    const screenshot = `${evidence}/${name}.png`;
    await page.screenshot({path:screenshot, fullPage:true});
    await save(name, {...data, state:await state(), status:await status(), html:await page.content(), screenshot});
  };
  try {
    const session = await control('session/early-cancel');
    await page.locator('#reconnect').click();
    await until(() => page.locator('#prompt').isEnabled(), 'session did not load');
    check((await page.locator('#session-title').innerText()).includes('early-cancel'), 'wrong selected session');
    await page.locator('#prompt').fill('Stop this offline task before its attempt facts reach the browser');
    await page.locator('#send-button').click();
    await until(async () => (await status()).observed.lastSnapshot?.result?.pending_approvals?.length > 0, 'real task never reached approval');
    const before = await state();
    check(before.cursor.includes(`游标 ${session.cursor}`), 'attempt events reached the browser before Stop');
    check(!before.stopDisabled, 'Stop should be available before observing the attempt');
    await page.locator('#cancel-button').click();
    await until(async () => (await status()).observed.cancel?.result?.cancelled === true, 'Runtime did not return cancelled:true');
    const cancelled = (await status()).observed;
    const originalTask = cancelled.originalTask?.result;
    const target = cancelled.cancelTarget;
    check(originalTask?.id === cancelled.admitted?.result?.id && originalTask?.session_id === session.id, 'Stop selected a different admitted Task');
    check(target?.task_id === originalTask.id && target.session_id === session.id
      && target.worker_id === originalTask.worker_id && target.attempt_id === originalTask.attempt_id
      && target.fence === originalTask.fence && target.fence > 0, 'Stop omitted or changed original claimed identity');
    await until(async () => (await status()).held && Boolean((await status()).observed.withheldTerminal), 'real prompt receipt or exact terminal missing');
    await until(async () => !(await status()).observed.lastSnapshot?.result?.attempt, 'Runtime did not reach idle');
    check(await page.locator('#prompt').isDisabled(), 'pending original prompt must remain locked');
    await capture('early-prompt-receipt-withheld', {session:session.id, before});
    const terminal = (await status()).observed.withheldTerminal;
    await control('release');
    // The original success receipt and authoritative idle are now visible, but
    // the proxy still withholds exactly attempt_finished from session.events.
    await until(async () => !(await status()).held, 'prompt receipt was not delivered');
    await until(async () => (await page.locator('#send-button').innerText()) === '发送', 'original prompt receipt did not reach the browser');
    check(await page.locator('#prompt').isDisabled(), 'receipt and idle cannot replace the missing exact terminal');
    check(!(await page.locator('#event-count').innerText()).includes(`游标 ${terminal.cursor}`), 'terminal was not withheld');
    await capture('early-exact-terminal-withheld', {session:session.id, terminal});
    await control('deliver-terminal');
    await until(() => page.locator('#prompt').isEnabled(), 'early known true cancel stranded the session');
    check(await page.locator('#cancel-button').isDisabled(), 'Stop remains enabled after settlement');
    check((await page.locator('#event-count').innerText()).includes(`游标 ${terminal.cursor}`), 'exact terminal not consumed');
    await page.locator('#prompt').fill('Independent next task draft');
    check(await page.locator('#send-button').isEnabled(), 'Send did not re-enable after settlement');
    const storage = await page.evaluate(() => ({local:Object.keys(localStorage), session:Object.keys(sessionStorage)}));
    check(!storage.local.length && !storage.session.length, 'browser storage must stay empty');
    await capture('early-true-cancel-settled', {session:session.id, terminal, storage});
    const result = {pass:true, session:session.id, terminal, before, after:await state(), storage};
    await save('early-results', result);
    return result;
  } catch (error) {
    await capture('early-failure', {error:error.stack});
    throw error;
  }
}
