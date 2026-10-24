// Load with Playwright MCP browser_run_code_unsafe after navigating to the editor.
// Standalone runner: scripts/validate-visual-editor.mjs.
// Uses a public local validation token, never a production credential.
async (page, options = {}) => {
  const url = options.url || page.url();
  const token = options.token || 'cayw-issue30-local-validation-token-20261023';
  const evidence = options.evidence || '/Volumes/ORICO/CAYW/.claude/worktrees/issue-30-visual-editor/.local/issue30-validation/browser';
  const checks = [];
  const errors = [];
  const requests = [];
  const summary = { url, checks, errors, requests, passed: false };
  const shot = async name => page.screenshot({path: `${evidence}-${name}.png`, fullPage: false});
  const onError = error => errors.push(error.message);
  const onRequest = request => {
    const address = new URL(request.url());
    if (address.pathname.startsWith('/api/')) requests.push({path: address.pathname, method: request.method()});
  };
  page.on('pageerror', onError);
  page.on('request', onRequest);
  const assert = (condition, message) => { if (!condition) throw new Error(message); };
  const frame = () => page.frames().find(candidate => candidate !== page.mainFrame() && /^http:\/\/127\.0\.0\.1:\d+\//.test(candidate.url()));
  const waitFrame = async () => {
    await page.locator('#preview-frame').waitFor();
    await page.waitForFunction(() => document.querySelector('#preview-frame')?.src?.includes('cayw_edit=1'));
    for (let i = 0; i < 100; i += 1) {
      const current = frame();
      const expected = await page.locator('#preview-frame').getAttribute('src');
      if (current && current.url() === expected && await current.locator('.hero-name').count() && await current.locator('#cayw-editor-overlay').count()) return current;
      await page.waitForTimeout(50);
    }
    throw new Error('Instrumented source page did not become ready');
  };
  const waitIdle = async () => {
    await page.waitForFunction(() => document.querySelector('#enter-edit')?.disabled === false);
    return waitFrame();
  };
  const select = async selector => {
    const current = await waitIdle();
    await current.locator(selector).first().click();
    await page.locator('#selection-label').waitFor({state:'visible'});
    return current;
  };
  const rightClick = async selector => {
    const current = await waitIdle();
    await current.locator(selector).first().click({button:'right'});
    await page.locator('#context-menu').waitFor({state:'visible'});
    return current;
  };
  const applyField = async (slug, value) => {
    const inputSlug = slug.startsWith('color-') ? `${slug}-hex` : slug;
    await page.getByTestId(`editor-menu-${inputSlug}`).fill(value);
    await page.getByTestId(`editor-menu-${slug}-apply`).click();
    return waitIdle();
  };
  const assertSelection = async selector => {
    const current=await waitIdle();
    const expected=await current.locator(selector).first().getAttribute('data-cayw-node');
    const response=await page.request.get(`${url}api/document`,{headers:{Authorization:`Bearer ${token}`}});
    assert(response.ok(),'Cannot verify selected source target');
    const document=await response.json();
    assert(document.snapshot.selected_id===expected,`Selection restored to ${document.snapshot.selected_id}, expected ${expected} for ${selector}`);
    await page.locator('#selection-label').waitFor({state:'visible'});
    await current.waitForFunction(id=>{
      const target=document.querySelector(`[data-cayw-node="${id}"]`);
      const ring=document.querySelector('.cayw-ring');
      if(!target||!ring||getComputedStyle(ring).display==='none')return false;
      const a=target.getBoundingClientRect(),b=ring.getBoundingClientRect();
      return Math.abs(a.x-b.x)<2&&Math.abs(a.y-b.y)<2&&Math.abs(a.width-b.width)<2&&Math.abs(a.height-b.height)<2;
    },expected);
  };
  try {
    assert(/^http:\/\/127\.0\.0\.1:\d+\/$/.test(url), 'Editor URL must be a loopback root');
    await page.setViewportSize({width:1600,height:1100});
    await page.emulateMedia({reducedMotion:'reduce'});
    await page.goto(url);
    await page.locator('#token-input').fill('incorrect-local-validation-token-000000');
    await page.locator('#connect-submit').click();
    await page.locator('#connect-error').waitFor({state:'visible'});
    checks.push('incorrect_token_rejected');
    await page.locator('#token-input').fill(token);
    await page.locator('#connect-submit').click();
    await page.locator('#workspace').waitFor({state:'visible'});
    let current = await waitFrame();
    const measure = () => ({
      width:innerWidth,
      overflow:document.documentElement.scrollWidth-innerWidth,
      nodes:['.hero-name','#top .hero-actions','.bento','.card'].map(selector=>{
        const element=document.querySelector(selector);
        const rect=element.getBoundingClientRect();
        const style=getComputedStyle(element);
        return {selector,x:rect.x,y:rect.y,width:rect.width,height:rect.height,font:style.font,color:style.color};
      })
    });
    await current.evaluate(()=>document.fonts.ready);
    const geometry = async () => (await waitFrame()).evaluate(measure);
    const before = await geometry();
    const original = await page.context().newPage();
    let plain;
    try {
      await original.setViewportSize({width:before.width,height:await current.evaluate(()=>innerHeight)});
      const address = new URL(current.url());
      address.search='';
      address.hash='';
      await original.goto(address.href);
      await original.evaluate(()=>document.fonts.ready);
      plain = await original.evaluate(measure);
      assert(await original.locator('[data-cayw-node]').count()===0,'Plain original preview includes editor instrumentation');
      assert(JSON.stringify(plain)===JSON.stringify(before),'Source instrumentation changed the original page layout at the same viewport');
      await original.screenshot({path:`${evidence}-original.png`,fullPage:false});
    } finally {await original.close();}
    await page.locator('#enter-edit').click();
    await page.waitForFunction(() => document.querySelector('#enter-edit').getAttribute('aria-pressed')==='true');
    const after = await geometry();
    assert(JSON.stringify(before)===JSON.stringify(after), 'Entering editing changed the original layout');
    checks.push('existing_unregistered_page_enters_without_layout_change');
    summary.geometry = {plain,before,after};
    await shot('entered');
    assert(await page.locator('[data-testid^="editor-palette-item-"]').count()===11,'The complete native component catalog is not available');
    await page.locator('#palette-search').fill('FAQ');
    assert(await page.getByTestId('editor-palette-item-faq').isVisible(),'Palette search did not find the FAQ block');
    assert(await page.locator('[data-testid^="editor-palette-item-"]:visible').count()===1,'Palette search did not filter components');
    await page.locator('#palette-search').fill('');
    checks.push('native_component_catalog_and_search');

    await select('.hero-name');
    assert((await page.locator('#selection-tag').textContent()).includes('span'), 'Text selection does not identify source element');
    await page.locator('#selection-parent').click();
    await page.locator('#selection-tag').filter({hasText:'h1'}).waitFor();
    checks.push('source_text_and_parent_selection');

    await rightClick('.hero-name');
    await page.getByTestId('editor-menu-tab-color').click();
    await applyField('color-text', '#29b6a1');
    current = await waitIdle();
    const color = await current.locator('.hero-name').evaluate(element=>getComputedStyle(element).color);
    assert(color==='rgb(41, 182, 161)', `Unexpected text color: ${color}`);
    await page.locator('#undo').click();
    current=await waitIdle();
    assert(await current.locator('.hero-name').evaluate(element=>getComputedStyle(element).color)===before.nodes[0].color,'Undo did not restore the original text style');
    await page.locator('#redo').click();
    current=await waitIdle();
    assert(await current.locator('.hero-name').evaluate(element=>getComputedStyle(element).color)==='rgb(41, 182, 161)','Redo did not restore the text style');
    checks.push('context_text_color','style_undo_redo');

    await rightClick('.hero-name');
    await page.getByTestId('editor-menu-tab-font').click();
    await applyField('font-size', '19');
    current = await waitIdle();
    assert(await current.locator('.hero-name').evaluate(element=>getComputedStyle(element).fontSize)==='19px', 'Font size was not applied');
    await rightClick('.hero-name');
    await page.getByTestId('editor-menu-tab-font').click();
    await page.getByTestId('editor-menu-font-weight').selectOption('700');
    await waitIdle();
    checks.push('context_font_size_and_weight');

    await rightClick('.hero-name');
    await page.getByTestId('editor-menu-text').fill('Issue 30 真实页面编辑');
    await page.getByTestId('editor-menu-text-apply').click();
    current = await waitIdle();
    assert(await current.locator('.hero-name').textContent()==='Issue 30 真实页面编辑', 'Text did not update');
    await page.locator('#undo').click();
    current = await waitIdle();
    assert((await current.locator('.hero-name').textContent()).includes('Code As You Wish'), 'Undo did not restore source text');
    await page.locator('#redo').click();
    current = await waitIdle();
    assert(await current.locator('.hero-name').textContent()==='Issue 30 真实页面编辑', 'Redo did not restore edited text');
    checks.push('text_undo_redo');

    await select('#top .hero-actions a.btn-secondary');
    await page.locator('#selection-add').click();
    await page.getByTestId('editor-popover-placement-after').click();
    await page.getByTestId('editor-popover-item-input').click();
    current = await waitIdle();
    assert(await current.locator('#top .hero-actions input').count()===1, 'Context insert did not add an input to the source project');
    await assertSelection('#top .hero-actions [data-cayw-component="input"]');
    checks.push('context_plus_inserts_native_component','insert_recovers_exact_source_selection');

    await rightClick('#top .hero-actions input');
    await page.getByTestId('editor-menu-tab-size').click();
    await applyField('width','180');
    current = await waitIdle();
    assert(await current.locator('#top .hero-actions input').evaluate(element=>getComputedStyle(element).width)==='180px', 'Width edit did not apply');
    checks.push('context_width');
    await shot('edited');

    await rightClick('.hero-name');
    await page.getByTestId('editor-menu-tab-font').click();
    await page.getByTestId('editor-menu-font-family').selectOption({label:'衬线（Georgia）'});
    current = await waitIdle();
    assert((await current.locator('.hero-name').evaluate(element=>getComputedStyle(element).fontFamily)).includes('Georgia'), 'Font family did not apply');
    await rightClick('.hero-name');
    await page.getByTestId('editor-menu-tab-font').click();
    await applyField('line-height','1.6');
    current = await waitIdle();
    assert(Math.abs(parseFloat(await current.locator('.hero-name').evaluate(element=>getComputedStyle(element).lineHeight))-30.4)<0.1,'Line height did not apply');
    checks.push('context_font_family_and_line_height');

    for (const [slug,value,property,expected] of [
      ['color-background','#243447','backgroundColor','rgb(36, 52, 71)'],
      ['color-border','#ffaa33','borderTopColor','rgb(255, 170, 51)']
    ]) {
      await rightClick('#top .hero-actions input');
      await page.getByTestId('editor-menu-tab-color').click();
      await applyField(slug,value);
      current = await waitIdle();
      assert(await current.locator('#top .hero-actions input').evaluate((element,key)=>getComputedStyle(element)[key],property)===expected,`${property} did not apply`);
    }
    await rightClick('#top .hero-actions input');
    await page.getByTestId('editor-menu-tab-size').click();
    await applyField('height','48');
    current = await waitIdle();
    assert(await current.locator('#top .hero-actions input').evaluate(element=>getComputedStyle(element).height)==='48px','Height did not apply');
    checks.push('context_background_border_and_height');

    await page.keyboard.press('Escape');
    await select('#top .hero-actions input');
    current = await waitIdle();
    const handle = await current.locator('.cayw-handle[data-dir="e"]').boundingBox();
    assert(handle,'Resize handle is missing');
    await page.mouse.move(handle.x+handle.width/2,handle.y+handle.height/2);
    await page.mouse.down();
    await page.mouse.move(handle.x+handle.width/2+24,handle.y+handle.height/2,{steps:12});
    assert(await current.locator('.cayw-drop').isVisible(),'Resize feedback is missing');
    await shot('resize-feedback');
    const resized = page.waitForResponse(response=>response.url().endsWith('/api/operation')&&response.request().method()==='POST');
    await page.mouse.up();
    await resized;
    current = await waitIdle();
    await current.locator('#top .hero-actions input').evaluate(element=>element.getBoundingClientRect());
    await current.waitForFunction(()=>getComputedStyle(document.querySelector('#top .hero-actions input')).width==='204px');
    assert(await current.locator('#top .hero-actions input').evaluate(element=>getComputedStyle(element).width)==='204px','Pointer resize did not change width');
    checks.push('pointer_resize_with_feedback');
    current=await waitIdle();
    await current.locator('.card').first().click({position:{x:12,y:20}});
    await page.locator('#selection-tag').filter({hasText:/^article/}).waitFor();
    checks.push('existing_card_selection');

    const drag = async (sourceSelector,targetSelector,fraction) => {
      current = await waitIdle();
      await page.waitForTimeout(450);
      await current.locator(sourceSelector).scrollIntoViewIfNeeded();
      const source = await current.locator(sourceSelector).boundingBox();
      const target = await current.locator(targetSelector).boundingBox();
      assert(source&&target,'Drag source or target is missing');
      await page.mouse.move(source.x+Math.min(12,source.width/2),source.y+source.height/2);
      await page.mouse.down();
      await page.mouse.move(target.x+target.width*fraction,target.y+target.height/2,{steps:16});
      assert(await current.locator('.cayw-line').isVisible()||await current.locator('.cayw-drop').isVisible(),'Drag feedback is missing');
      await shot('drag-feedback');
      const previousSrc=await page.locator('#preview-frame').getAttribute('src');
      const moved=page.waitForResponse(response=>response.url().endsWith('/api/operation')&&response.request().method()==='POST');
      await page.mouse.up();
      const response=await moved;
      assert(response.ok(),`Move rejected: ${await response.text()}`);
      await page.waitForFunction(src=>document.querySelector('#preview-frame').src!==src,previousSrc);
      return waitIdle();
    };
    current = await drag('#top .hero-actions a.btn-ghost','#top .hero-actions a.btn-secondary',0.12);
    const order = await current.locator('#top .hero-actions').first().locator(':scope > a').allTextContents();
    assert(order[1].trim()==='架构文档'&&order[2].trim()==='本地体验','Pointer reorder did not change source order');
    await assertSelection('#top .hero-actions a.btn-ghost');
    checks.push('move_recovers_exact_source_selection');
    await page.locator('#undo').click();
    current = await waitIdle();
    assert((await current.locator('#top .hero-actions').first().locator(':scope > a').allTextContents())[1].trim()==='本地体验','Undo did not restore layout');
    await page.locator('#redo').click();
    current = await waitIdle();
    assert((await current.locator('#top .hero-actions').first().locator(':scope > a').allTextContents())[1].trim()==='架构文档','Redo did not restore layout');
    checks.push('pointer_reorder_and_layout_history');

    await select('#top .hero-actions input');
    await page.locator('#selection-parent').click();
    await page.getByTestId('editor-palette-item-container').click();
    current = await waitIdle();
    const insertedContainer = '[data-cayw-component="container"]';
    assert(await current.locator(insertedContainer).count()===1,'Container insertion failed');
    await current.locator(insertedContainer).scrollIntoViewIfNeeded();
    const sourceBox=await current.locator('#top .hero-actions input').boundingBox();
    const containerBox=await current.locator(insertedContainer).boundingBox();
    assert(sourceBox&&containerBox,'Cross-container target is missing');
    const inside=await current.locator(insertedContainer).evaluate(element=>{
      const rect=element.getBoundingClientRect();
      for(const x of [rect.left+rect.width*.5,rect.left+rect.width*.4]) for(const y of [rect.top+rect.height*.5,rect.top+rect.height*.6]) {
        if(document.elementFromPoint(x,y)===element)return {x,y};
      }
      return null;
    });
    assert(inside,'No visible container padding drop target');
    const frameBox=await page.locator('#preview-frame').boundingBox();
    await page.mouse.move(sourceBox.x+12,sourceBox.y+sourceBox.height/2);
    await page.mouse.down();
    await page.mouse.move(frameBox.x+inside.x,frameBox.y+inside.y,{steps:16});
    assert(await current.locator('.cayw-drop').isVisible(),'Cross-container feedback is missing');
    const crossed=page.waitForResponse(response=>response.url().endsWith('/api/operation')&&response.request().method()==='POST');
    const crossPreviousSrc=await page.locator('#preview-frame').getAttribute('src');
    await page.mouse.up();
    const crossResponse=await crossed;
    assert(crossResponse.ok(),`Cross-container move rejected: ${await crossResponse.text()}`);
    await page.waitForFunction(src=>document.querySelector('#preview-frame').src!==src,crossPreviousSrc);
    current = await waitIdle();
    assert(await current.locator(`${insertedContainer} input`).count()===1,'Cross-container move did not persist in the draft');
    await assertSelection(`${insertedContainer} input`);
    checks.push('pointer_cross_container_move');

    await select('.hero-name');
    current = await waitIdle();
    await current.locator(insertedContainer).scrollIntoViewIfNeeded();
    const dropBox=await current.locator(insertedContainer).boundingBox();
    const paletteBox=await page.getByTestId('editor-palette-item-button').boundingBox();
    const countBefore=await current.locator(`${insertedContainer} button`).count();
    await page.mouse.move(paletteBox.x+paletteBox.width/2,paletteBox.y+paletteBox.height/2);
    await page.mouse.down();
    await page.mouse.move(dropBox.x+dropBox.width*.5,dropBox.y+dropBox.height*.55,{steps:20});
    assert(await current.locator('.cayw-drop').isVisible()||await current.locator('.cayw-line').isVisible(),'Palette drop feedback is missing');
    const inserted=page.waitForResponse(response=>response.url().endsWith('/api/operation')&&response.request().method()==='POST');
    const palettePreviousSrc=await page.locator('#preview-frame').getAttribute('src');
    await page.mouse.up();
    const insertResponse=await inserted;
    assert(insertResponse.ok(),`Palette insert rejected: ${await insertResponse.text()}`);
    await page.waitForFunction(src=>document.querySelector('#preview-frame').src!==src,palettePreviousSrc);
    current = await waitIdle();
    assert(await current.locator(`${insertedContainer} button`).count()===countBefore+1,'Palette drag inserted at the old selection instead of the actual drop target');
    await assertSelection(`${insertedContainer} button`);
    checks.push('palette_pointer_drop_uses_actual_target');

    await page.locator('#save').click();
    await page.waitForFunction(()=>document.querySelector('#save-state').textContent==='已保存');
    checks.push('save_reaches_host');
    await page.locator('#preview-mode').click();
    current = await waitIdle();
    await current.locator('#top .hero-actions input').fill('原有页面中的新输入框');
    assert(await current.locator('#top .hero-actions input').inputValue()==='原有页面中的新输入框','Inserted input is not interactive');
    checks.push('inserted_control_interactive');

    const desktopWidth = await current.evaluate(()=>({viewport:innerWidth,overflow:document.documentElement.scrollWidth-innerWidth}));
    await page.getByTestId('editor-device-mobile').click();
    current = await waitIdle();
    await current.waitForFunction(() => innerWidth === 390);
    await current.locator('[data-nav-toggle]').click();
    assert(await current.locator('[data-nav-toggle]').getAttribute('aria-expanded')==='true','Original mobile navigation stopped working');
    const mobileWidth = await current.evaluate(()=>({viewport:innerWidth,overflow:document.documentElement.scrollWidth-innerWidth}));
    assert(mobileWidth.viewport===390,'Mobile preview is not 390px wide');
    assert(mobileWidth.overflow<=1,'Editing introduced mobile overflow beyond the measured 1px baseline');
    summary.responsive={desktopWidth,mobileWidth};
    checks.push('original_navigation_and_mobile_layout');
    await shot('mobile');

    const retained = await page.evaluate(()=>({
      url:location.href,
      tokenInput:document.querySelector('#token-input').value,
      localStorageKeys:Object.keys(localStorage),
      sessionStorageKeys:Object.keys(sessionStorage),
      iframeSrc:document.querySelector('#preview-frame').src
    }));
    assert(retained.tokenInput==='' && retained.localStorageKeys.length===0 && retained.sessionStorageKeys.length===0,'Editor credential persisted in browser storage');
    assert(!retained.url.includes(token)&&!retained.iframeSrc.includes(token),'Token leaked into a URL');
    assert(!requests.some(request=>request.path==='/api/rpc'||request.path.includes('prompt')),'Deterministic edit invoked an Agent or model');
    summary.retained=retained;
    checks.push('no_browser_credential_storage','deterministic_edits_without_model_calls');
    assert(errors.length===0,`Page errors: ${errors.join('; ')}`);
    summary.passed=true;
  } catch(error) {
    summary.error=error.message;
    await shot('failure').catch(()=>{});
  } finally {
    page.off('pageerror',onError);
    page.off('request',onRequest);
  }
  return summary;
}
