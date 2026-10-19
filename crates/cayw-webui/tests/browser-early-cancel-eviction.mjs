// Playwright MCP runner; start a FRESH runtime-browser.mjs with --approval-gated.
// Every receipt and event comes from the offline Runtime. Only delivery is held.
// Fill the event rendering cache with real attempts; this is not a throughput test.
async (page) => {
  const check = (value, message) => { if (!value) throw new Error(message); };
  const until = async (fn, message) => {
    for (let i = 0; i < 300; i++) { if (await fn()) return; await page.waitForTimeout(100); }
    throw new Error(message);
  };
  const control = async (path, data) => {
    const response = await page.request.post(new URL(`/__test__/${path}`, page.url()).href, {data, timeout:120000});
    check(response.ok(), await response.text());
    return response.json();
  };
  const status = async () => (await page.request.get(new URL('/__test__/status', page.url()).href)).json();
  const state = () => page.evaluate(() => ({
    promptDisabled:document.getElementById('prompt').disabled,
    stopDisabled:document.getElementById('cancel-button').disabled,
    sendDisabled:document.getElementById('send-button').disabled,
    sendLabel:document.getElementById('send-button').textContent,
    cursor:document.getElementById('event-count').textContent,
    notice:document.getElementById('blocked-text').textContent,
    cachedCursors:[...document.querySelectorAll('#event-log > details > summary')].map(node => Number(node.textContent.match(/^#(\d+)/)?.[1])),
  }));
  const manifest = await (await page.request.get(new URL('/__test__/manifest', page.url()).href)).json();
  let eviction;
  let before;
  let session;
  let proof;
  try {
    session = await control('session/early-cancel');
    const {token} = await (await page.request.get(new URL('/__test__/login', page.url()).href)).json();
    await page.locator('#web-token').fill(token);
    await page.locator('#login-button').click();
    await until(() => page.locator('#prompt').isEnabled(), 'session did not load');
    check(await page.locator('#session-list [aria-current="true"]').count() === 1, 'selected session missing');
    check((await page.locator('#session-list [aria-current="true"]').textContent()).includes(session.id.slice(0, 20)), 'runner requires a fresh service with its own session selected');
    await page.locator('#prompt').fill('Stop this offline task before any attempt is visible');
    await page.locator('#send-button').click();
    await until(async () => (await status()).observed.lastSnapshot?.result?.pending_approvals?.length > 0, 'offline task did not reach approval');
    check((await state()).cursor.endsWith(`游标 ${session.cursor}`), 'attempt reached browser before early cancel');
    await page.locator('#cancel-button').click();
    await until(async () => (await status()).observed.cancel?.result?.cancelled && (await status()).held && (await status()).observed.withheldTerminal, 'original cancel/terminal/receipt missing');
    const terminal = (await status()).observed.withheldTerminal;
    check(terminal.protocol_version === 3 && terminal.session_id === session.id && JSON.stringify(terminal.attempt) === JSON.stringify(terminal.event.lease), 'invalid original terminal identity');
    await control('deliver-terminal');
    await until(async () => (await state()).cursor.endsWith(`游标 ${terminal.cursor}`), 'original terminal not consumed');
    for (let attempt = 0; attempt < 16; attempt++) {
      eviction = await control('advance-clients', {count:1});
      const cursor = eviction.terminals.at(-1).cursor;
      await until(async () => (await state()).cursor.endsWith(`游标 ${cursor}`), 'browser did not consume the independent attempt');
      before = await state();
      check(before.promptDisabled && before.sendDisabled && !before.stopDisabled, 'other attempts changed the original operation lock');
      if (eviction.completed >= 2 && before.cachedCursors.length === 120 && before.cachedCursors.every(cursor => cursor > terminal.cursor)) break;
    }
    check(before.cachedCursors.length === 120 && before.cachedCursors.every(cursor => cursor > terminal.cursor), 'original terminal was not evicted within the explicit 16-attempt test limit');
    check(eviction.originalReceipt.cursor === terminal.cursor && JSON.stringify(eviction.originalTerminal) === JSON.stringify(terminal), 'original receipt no longer matches its terminal');
    check(new Set(eviction.terminals.map(record => record.attempt.attempt_id)).size === eviction.completed, 'independent attempts reused an identity');
    const historical = page.waitForResponse(response => {
      if (!response.request().url().endsWith('/api/rpc') || response.request().method() !== 'POST') return false;
      let call;
      try { call = response.request().postDataJSON(); } catch { return false; }
      return call.method === 'session.events' && call.params.session_id === session.id && call.params.after === terminal.cursor - 1 && call.params.limit === 1;
    }, {timeout:30000});
    await control('release');
    const response = await historical;
    check(response.ok(), 'historical terminal read failed');
    proof = await response.json();
    check(proof.result?.length === 1 && JSON.stringify(proof.result[0]) === JSON.stringify(terminal), 'historical read did not return the exact original terminal');
    await until(() => page.locator('#prompt').isEnabled(), 'exact original terminal was evicted and never recovered');
    check(await page.locator('#cancel-button').isDisabled(), 'settled Stop still enabled');
    await page.locator('#prompt').fill('Next independent task draft');
    check(await page.locator('#send-button').isEnabled(), 'Send did not enable');
    check((await state()).cursor === before.cursor, 'historical read changed the forward event cursor');
    return await finish(true);
  } catch (error) {
    return await finish(false, String(error.stack));
  }
  async function finish(pass, error) {
    const screenshot = `${manifest.evidence}/eviction.png`;
    await page.screenshot({path:screenshot, fullPage:true});
    const after = await state();
    const result = {pass, error, scope:'Original terminal recovery after eviction from the 120-event rendering cache', attemptLimit:16, session:session?.id, manifest, before, after, proof, originalReceipt:eviction?.originalReceipt, originalTerminal:eviction?.originalTerminal, subsequentTerminalCount:eviction?.completed, subsequentTerminals:eviction?.terminals, finalCursor:eviction?.terminals.at(-1)?.cursor, screenshot};
    await control('evidence', {name:'eviction-result', data:result});
    return result;
  }
}
