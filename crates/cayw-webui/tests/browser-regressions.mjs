// Loaded by Playwright MCP browser_run_code_unsafe after runtime-browser.mjs.
// Uses only loopback test controls and the shipped browser interface.
async (page) => {
  const evidence = `/tmp/cayw-webui-playwright-${Date.now()}`;
  const save = async (name, data) => {
    const response=await page.request.post(new URL('/__test__/evidence',page.url()).href,{data:{name,data}});
    if(!response.ok()) throw new Error(await response.text());
  };
  const results = [];
  const check = (condition, message) => { if (!condition) throw new Error(message); };
  const until = async (condition, message) => {
    for (let i=0;i<150;i++) { if(await condition()) return; await page.waitForTimeout(100); }
    throw new Error(message);
  };
  const control = async (path) => {
    const response=await page.request.post(new URL(`/__test__/${path}`, page.url()).href);
    check(response.ok(),await response.text());
    return response.json();
  };
  const status = async () => (await page.request.get(new URL('/__test__/status',page.url()).href)).json();
  const reconnect = async () => {
    await page.locator('#reconnect').click();
    await until(()=>page.locator('#workspace').isVisible(),'reconnect failed');
  };
  const select = async id => {
    await page.locator('#refresh-sessions').click();
    await until(()=>page.locator('#session-list button').filter({hasText:id.slice(0,20)}).count().then(Boolean),'new session missing');
    await page.locator('#session-list button').filter({hasText:id.slice(0,20)}).click();
    await until(()=>page.locator('#prompt').isEnabled(),'session did not become idle');
  };
  const capture = async name => {
    await page.screenshot({path:`${evidence}-${name}.png`,fullPage:true});
    await save(name,{html:await page.content(),status:await status(),screenshot:`${evidence}-${name}.png`});
  };
  try {
    const lost=await control('session/drop');
    await reconnect(); await select(lost.id);
    await page.locator('#prompt').fill('This request must never reach Runtime');
    await page.locator('#send-button').click();
    await until(async()=> (await page.locator('#blocked-text').innerText()).includes('回执待确认'),'transport ambiguity not shown');
    const other=await control('other-client');
    await until(async()=> (await page.locator('#event-count').innerText()).includes(`游标 ${other.cursor}`),'other client terminal not consumed');
    check(await page.locator('#prompt').isDisabled(),'unrelated execution unlocked unknown prompt');
    check((await status()).observed.dropped===1,'unexpected duplicate prompt dispatch');
    const storage=await page.evaluate(()=>({local:Object.keys(localStorage),session:Object.keys(sessionStorage)}));
    check(!storage.local.length&&!storage.session.length,'credentials or state leaked to browser storage');
    const notice=await page.locator('#blocked-text').innerText();
    check(notice.includes('宿主核对')&&notice.includes('事件记录')&&notice.includes('新建会话'),'unknown result lacks usable guidance');
    await capture('unknown-request-stays-blocked');
    results.push({case:'unknown prompt and other client real Runtime execution',pass:true,session:lost.id,terminal:other.cursor,storage});

    const delayed=await control('session/hold');
    await select(delayed.id);
    await page.locator('#prompt').fill('Complete offline work but delay only my HTTP receipt');
    await page.locator('#send-button').click();
    await until(async()=> (await status()).held,'prompt receipt was not held');
    const terminal=(await status()).lastPromptResult.result.cursor;
    await until(async()=> (await page.locator('#event-count').innerText()).includes(`游标 ${terminal}`),'completed prompt terminal not consumed');
    check(await page.locator('#cancel-button').isEnabled(),'reproduction requires stop while receipt is held');
    await page.locator('#cancel-button').click();
    await until(async()=> (await status()).observed.cancel?.result?.cancelled===false,'actual Runtime did not return no-op cancel');
    check(await page.locator('#prompt').isDisabled(),'pending prompt unlocked before its own receipt');
    await capture('noop-cancel-before-receipt');
    await control('release');
    await until(()=>page.locator('#prompt').isEnabled(),'no-op cancel stranded the completed prompt');
    check(await page.locator('#cancel-button').isDisabled(),'stop still enabled on settled idle');
    check(!(await page.locator('#messages').innerText()).includes('Use execute_code for bounded'),'system instruction exposed in chat history');
    await capture('noop-cancel-settled');
    results.push({case:'delayed prompt receipt after terminal plus actual cancelled:false',pass:true,session:delayed.id,terminal});

    const reconciliation=await control('session/reconciliation');
    await page.locator('#refresh-sessions').click();
    const item=page.locator('#session-list button').filter({hasText:reconciliation.id.slice(0,20)});
    await until(()=>item.count().then(Boolean),'reconciliation session missing');
    await item.click();
    await until(async()=>(await page.locator('#session-state').innerText()).includes('待核对'),'durable unknown call missing');
    const reconciled=await control('reconcile');
    await until(async()=>(await status()).observed.delayedSnapshot?.cursor===reconciliation.cursor,'old snapshot was not injected');
    await until(()=>page.locator('#prompt').isEnabled(),'old snapshot erased consumed Core reconciliation');
    check((await page.locator('#event-count').innerText()).includes(`游标 ${reconciled.cursor}`),'reconciliation fact not consumed');
    await capture('reconciliation-survives-old-snapshot');
    results.push({case:'real Core reconciliation consumed before delayed unresolved projection',pass:true,session:reconciliation.id,oldCursor:reconciliation.cursor,terminal:reconciled.cursor});
    await save('results',{results,evidence});
    return {results,evidence};
  } catch(error) {
    await capture('failure');
    await save('failure-detail',{error:error.stack});
    throw error;
  }
}
