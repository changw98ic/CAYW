// Run through Playwright MCP or validate-visual-editor-framework.mjs.
// These checks drive the editor UI; project source writes are checked by the runner.
async (page, options = {}) => {
  const {url, token, evidence, framework, normalUrl, mode = 'edit'} = options;
  const checks = [];
  const errors = [];
  const requests = [];
  const result = {framework, mode, url, checks, errors, requests, passed:false};
  const assert = (value, message) => {if (!value) throw new Error(message);};
  const onError = error => errors.push(error.message);
  const onRequest = request => {
    const address = new URL(request.url());
    if (address.pathname.startsWith('/api/')) requests.push({path:address.pathname,method:request.method()});
  };
  page.on('pageerror', onError);
  page.on('request', onRequest);
  const documentData = async () => {
    const response = await page.request.get(`${url}api/document`, {headers:{Authorization:`Bearer ${token}`}});
    assert(response.ok(), `Cannot read source snapshot: ${response.status()}`);
    return response.json();
  };
  const waitFrame = async () => {
    await page.waitForFunction(() => document.querySelector('#enter-edit')?.disabled === false);
    for (let attempt=0; attempt<240; attempt+=1) {
      const expected = await page.locator('#preview-frame').getAttribute('src');
      const frame = page.frames().find(candidate => candidate.url() === expected);
      if (frame && await frame.locator('#hero-title[data-cayw-node]').count()
          && await frame.locator('#cayw-editor-overlay').count()) return frame;
      await page.waitForTimeout(50);
    }
    throw new Error('Framework render did not mount mapped nodes and the bridge');
  };
  const select = async selector => {
    const frame = await waitFrame();
    const target=frame.locator(selector).first();
    const id=await target.getAttribute('data-cayw-node');
    await target.click();
    const crumb = page.locator(`#inspector-body .crumb[data-node-id="${id}"]`);
    await crumb.waitFor();
    if (!((await crumb.getAttribute('class')) || '').split(' ').includes('current')) await crumb.click();
    await page.waitForFunction(id=>document.querySelector('#inspector-body .crumb.current')?.getAttribute('data-node-id')===id,id);
    await page.locator('#selection-label').waitFor({state:'visible'});
    return frame;
  };
  const submit = async action => {
    const response = page.waitForResponse(response => response.url() === `${url}api/operation` && response.request().method() === 'POST');
    await action();
    const returned = await response;
    const data = await returned.json();
    assert(returned.ok(), `Source operation failed: ${JSON.stringify(data)}`);
    await waitFrame();
    return data;
  };
  const text = async (selector, value, context=false) => {
    const frame = await waitFrame();
    if (context) {
      await frame.locator(selector).first().click({button:'right'});
      await page.locator('#context-menu').waitFor({state:'visible'});
      await page.getByTestId('editor-menu-text').fill(value);
      await submit(() => page.getByTestId('editor-menu-text-apply').click());
    } else {
      await select(selector);
      await page.getByTestId('editor-sidebar-text').fill(value);
      await submit(() => page.getByTestId('editor-sidebar-text-apply').click());
    }
    await page.keyboard.press('Escape');
    assert(await (await waitFrame()).locator(selector).first().textContent() === value, `Text not rendered for ${selector}`);
  };
  const style = async (selector, slug, value) => {
    await select(selector);
    const input = slug.startsWith('color-') ? `${slug}-hex` : slug;
    await page.getByTestId(`editor-sidebar-${input}`).fill(value);
    await submit(() => page.getByTestId(`editor-sidebar-${slug}-apply`).click());
  };
  const selectionMatches = async selector => {
    const frame = await waitFrame();
    const id = await frame.locator(selector).first().getAttribute('data-cayw-node');
    const data = await documentData();
    assert(id === data.snapshot.selected_id, `Selection is not the source node for ${selector}`);
    assert(data.snapshot.nodes.some(node => node.id === id && /\.(jsx|tsx|vue)$/.test(node.file)), 'Selection has no framework source file');
    await page.locator('#selection-label').waitFor({state:'visible'});
    await frame.waitForFunction(id => {
      const target = document.querySelector(`[data-cayw-node="${id}"]`);
      const ring = document.querySelector('.cayw-ring');
      if (!target || !ring || getComputedStyle(ring).display === 'none') return false;
      const a=target.getBoundingClientRect(), b=ring.getBoundingClientRect();
      return ['x','y','width','height'].every(key=>Math.abs(a[key]-b[key])<2);
    }, id);
  };
  const save = async expected => {
    const response = page.waitForResponse(response=>response.url()===`${url}api/save`);
    await page.locator('#save').click();
    const returned = await response;
    const data = await returned.json();
    assert(returned.status()===expected, `Save expected ${expected}, got ${returned.status()}: ${JSON.stringify(data)}`);
    if (expected===200) await waitFrame();
    return data;
  };
  const shot = async name => {if (evidence) await page.screenshot({path:`${evidence}-${name}.png`,fullPage:false});};
  try {
    await page.setViewportSize({width:1600,height:1100});
    await page.emulateMedia({reducedMotion:'reduce'});
    await page.goto(url);
    await page.locator('#token-input').fill(token);
    await page.locator('#connect-submit').click();
    await page.locator('#workspace').waitFor({state:'visible'});
    let frame = await waitFrame();
    const initial = await documentData();
    assert(initial.snapshot.adapter === framework, 'The requested framework adapter did not open');
    assert(initial.snapshot.files.length >= 3, 'Framework project did not enroll multiple files');
    if (mode==='conflict-save') {
      await save(409);
      await page.locator('#conflict-banner').waitFor({state:'visible'});
      checks.push('external_source_conflict_stops_save');
      await page.locator('#conflict-reload').click();
      frame = await waitFrame();
      assert((await frame.locator('#hero-title').textContent()).includes('External source'), 'Reload did not retain the external edit');
      await page.locator('#enter-edit').click();
      await text('#hero-description', `${framework} conflict reload retained external source`);
      await save(200);
      checks.push('explicit_reload_retains_external_source_and_allows_save');
    } else if (mode==='conflict-draft') {
      await page.locator('#enter-edit').click();
      await text('#hero-description', 'Unsaved stale draft must not replace external source');
      checks.push('conflict_draft_created_without_disk_write');
    } else if (mode==='restart') {
      assert(await frame.locator('#hero-title').textContent() === `${framework} visual source saved 中文 😀`, 'Saved framework text was lost at restart');
      assert(await frame.locator('[data-testid="cayw-component-button"]').count() === 1, 'Inserted JSX/template component was lost at restart');
      assert(await frame.locator('[data-testid="cayw-component-hero"]').count() === 1, 'Inserted styled section was lost at restart');
      assert(await frame.locator('#secondary-title').textContent() === `${framework} second source file saved`, 'Second source file was lost at restart');
      checks.push('saved_framework_sources_survive_restart');
      await page.locator('#enter-edit').click();
      await text('#hero-description', `${framework} second edit after restart`);
      await save(200);
      checks.push('restart_reedit_and_host_save');
    } else {
      const measure = () => ({
        width:innerWidth,overflow:document.documentElement.scrollWidth-innerWidth,
        nodes:['#hero-title','#primary-action','#static-cards','#original-image'].map(selector=>{
          const element=document.querySelector(selector), rect=element.getBoundingClientRect(), css=getComputedStyle(element);
          return {selector,x:rect.x,y:rect.y,width:rect.width,height:rect.height,font:css.font,color:css.color};
        })
      });
      await frame.evaluate(()=>document.fonts.ready);
      const mapped = await frame.evaluate(measure);
      const normal = await page.context().newPage();
      try {
        await normal.setViewportSize({width:mapped.width,height:await frame.evaluate(()=>innerHeight)});
        await normal.goto(normalUrl || initial.normal_preview_url);
        await normal.locator('#hero-title').waitFor();
        await normal.evaluate(()=>document.fonts.ready);
        const plain = await normal.evaluate(measure);
        assert(JSON.stringify(mapped)===JSON.stringify(plain), 'Source instrumentation changed measured layout');
        assert(await normal.locator('[data-cayw-node]').count()===0, 'Ordinary preview contains source instrumentation');
        assert(await normal.locator('script[src*="__cayw_bridge"]').count()===0, 'Ordinary preview contains editor bridge');
        result.geometry={plain,mapped};
      } finally {await normal.close();}
      await page.locator('#enter-edit').click();
      assert(JSON.stringify(await frame.evaluate(measure))===JSON.stringify(mapped), 'Entering edit changed measured layout');
      checks.push('normal_mapped_and_edit_geometry_match_without_plain_instrumentation');
      const beforeTitle = await frame.locator('#hero-title').textContent();
      for (const failure of ['network', 'service']) {
        await select('#hero-title');
        const operationUrl = `${url}api/operation`;
        const reject = route => failure === 'network'
          ? route.abort('connectionreset')
          : route.fulfill({status:503,json:{error:{message:'Temporary test service error',outcome_unknown:false}}});
        await page.route(operationUrl, reject);
        try {
          await page.getByTestId('editor-sidebar-text').fill('Rejected request must not lock editing');
          const returned = failure === 'network'
            ? page.waitForEvent('requestfailed', request => request.url() === operationUrl)
            : page.waitForResponse(response => response.url() === operationUrl);
          await page.getByTestId('editor-sidebar-text-apply').click();
          await returned;
          await page.waitForFunction(() => document.querySelector('#enter-edit')?.disabled === false);
          await select('#hero-title');
          assert(!await page.locator('#unknown-banner').isVisible(), 'Known request failure was marked as an unknown save');
          assert(await page.getByTestId('editor-sidebar-text-apply').isEnabled(), 'Known request failure locked the editor');
          assert((await documentData()).snapshot.revision === initial.snapshot.revision, 'Rejected request changed the source');
        } finally {
          await page.unroute(operationUrl, reject);
        }
        checks.push(`${failure}_operation_failure_keeps_editing_available`);
      }
      await select('#hero-title');
      const operationUrl = `${url}api/operation`;
      let appliedResponse;
      let appliedRequests = 0;
      const dropResponse = async route => {
        appliedRequests += 1;
        const response = await route.fetch({maxRedirects:0,maxRetries:0});
        appliedResponse = await response.json();
        await route.abort('connectionreset');
      };
      await page.route(operationUrl, dropResponse);
      try {
        await page.getByTestId('editor-sidebar-text').fill('Applied draft recovered after response loss');
        const failed = page.waitForEvent('requestfailed', request => request.url() === operationUrl);
        await page.getByTestId('editor-sidebar-text-apply').click();
        await failed;
        frame = await waitFrame();
        await frame.locator('#hero-title').filter({hasText:'Applied draft recovered after response loss'}).waitFor();
        assert(appliedResponse.snapshot.revision !== initial.snapshot.revision, 'The response-loss operation did not reach the server');
        assert(!await page.locator('#save').isDisabled(), 'Recovered draft was not available for save');
        assert(!await page.locator('#unknown-banner').isVisible(), 'An in-memory edit was treated as an unknown source write');
        assert(appliedRequests === 1, 'The lost-response edit was replayed');
      } finally {
        await page.unroute(operationUrl, dropResponse);
      }
      await submit(() => page.locator('#undo').click());
      assert(await (await waitFrame()).locator('#hero-title').textContent() === beforeTitle, 'Recovered edit could not be undone');
      checks.push('applied_edit_response_loss_refreshes_draft_without_replay');
      await select('#hero-title');
      appliedRequests = 0;
      const documentUrl = `${url}api/document`;
      const failRefresh = route => route.abort('connectionreset');
      await page.route(operationUrl, dropResponse);
      await page.route(documentUrl, failRefresh);
      try {
        await page.getByTestId('editor-sidebar-text').fill('Draft retained while reconnecting');
        await page.getByTestId('editor-sidebar-text-apply').click();
        await page.locator('#connect-view').waitFor({state:'visible'});
        assert(appliedResponse.snapshot.revision !== initial.snapshot.revision, 'The disconnected edit did not reach the server');
        assert(appliedRequests === 1, 'The disconnected edit was replayed');
        assert(!await page.locator('#workspace').isVisible(), 'An outdated draft remained available for editing');
      } finally {
        await page.unroute(operationUrl, dropResponse);
        await page.unroute(documentUrl, failRefresh);
      }
      await page.locator('#token-input').fill(token);
      await page.locator('#connect-submit').click();
      await page.locator('#workspace').waitFor({state:'visible'});
      frame = await waitFrame();
      assert(await frame.locator('#hero-title').textContent() === 'Draft retained while reconnecting', 'Reconnect lost the service draft');
      assert(!await page.locator('#save').isDisabled(), 'Reconnect did not restore the dirty draft');
      assert(!await page.locator('#unknown-banner').isVisible(), 'Reconnect marked the draft as an unknown source write');
      await page.locator('#enter-edit').click();
      await submit(() => page.locator('#undo').click());
      assert(await (await waitFrame()).locator('#hero-title').textContent() === beforeTitle, 'Reconnected draft could not be undone');
      checks.push('failed_draft_refresh_reconnects_without_replay_or_source_unknown');
      for (const [selector, slug, value, property, expected] of [
        ['#primary-action', 'width', '5001', 'width', '5001px'],
        ['#hero-title', 'font-size', '250', 'fontSize', '250px'],
        ['#hero-title', 'line-height', '8', 'lineHeight', '8'],
        ['#primary-action', 'width', 'clamp(180px, 25vw, 320px)', 'width', 'clamp(180px, 25vw, 320px)'],
        ['#static-cards', 'grid-columns', '16', 'gridTemplateColumns', 'repeat(16, minmax(0px, 1fr))'],
      ]) {
        result.active_check = {selector, slug, value};
        await style(selector, slug, value);
        assert(await (await waitFrame()).locator(selector).evaluate((element, property) => element.style[property], property) === expected,
          `Valid ${slug} was restricted: ${value}`);
        await submit(() => page.locator('#undo').click());
      }
      checks.push('valid_css_values_and_sizes_have_no_arbitrary_ui_caps');
      await text('#hero-title', `${framework} visual source saved 中文 😀`, true);
      await selectionMatches('#hero-title');
      checks.push('context_text_writes_framework_source_and_restores_selection');
      await submit(()=>page.locator('#undo').click());
      assert(await (await waitFrame()).locator('#hero-title').textContent()===beforeTitle, 'Undo did not restore framework text');
      await submit(()=>page.locator('#redo').click());
      assert(await (await waitFrame()).locator('#hero-title').textContent()===`${framework} visual source saved 中文 😀`, 'Redo did not restore edited text');
      checks.push('framework_text_undo_redo');
      await style('#hero-title','color-text','#123456');
      assert(await (await waitFrame()).locator('#hero-title').evaluate(element=>getComputedStyle(element).color)==='rgb(18, 52, 86)', 'Framework style is not rendered');
      await style('#primary-action','width','210');
      assert(Math.abs((await (await waitFrame()).locator('#primary-action').boundingBox()).width-210)<1, 'Framework width is not rendered');
      checks.push('framework_static_style_and_width');
      await select('#original-image');
      await page.getByTestId('editor-sidebar-src').fill('/alternate.svg');
      await submit(()=>page.getByTestId('editor-sidebar-src-apply').click());
      frame=await waitFrame();
      await frame.locator('#original-image').evaluate(image=>image.decode());
      const imageSnapshot=await documentData();
      assert(imageSnapshot.snapshot.files.some(file=>/\bsrc\s*=\s*["']\/alternate\.svg["']/.test(file.source)), 'Image source did not change in project source');
      assert(await frame.locator('#original-image').evaluate(image=>image.naturalWidth>0), 'Compiled alternate image did not decode');
      checks.push('framework_image_attribute_and_decode');
      await text('#secondary-title',`${framework} second source file saved`);
      await selectionMatches('#secondary-title');
      const secondary = await documentData();
      assert(secondary.snapshot.nodes.find(node=>node.id===secondary.snapshot.selected_id).file!==initial.snapshot.nodes.find(node=>node.tag==='h1').file, 'Second component did not map to a different source file');
      checks.push('second_component_source_file_edit');
      await select('#insertion-target');
      await page.getByTestId('editor-placement-inside').click();
      await submit(()=>page.getByTestId('editor-palette-item-button').click());
      await selectionMatches('[data-testid="cayw-component-button"]');
      checks.push('canonical_component_inserts_valid_framework_syntax');
      await select('[data-testid="cayw-component-button"]');
      await submit(()=>page.locator('#selection-duplicate').click());
      frame=await waitFrame();
      assert(await frame.locator('[data-testid="cayw-component-button"]').count()===2, 'Inserted static button was not duplicated');
      await submit(()=>page.locator('#selection-delete').click());
      assert(await (await waitFrame()).locator('[data-testid="cayw-component-button"]').count()===1, 'Duplicate deletion failed');
      await submit(()=>page.locator('#undo').click());
      assert(await (await waitFrame()).locator('[data-testid="cayw-component-button"]').count()===2, 'Deletion undo failed');
      await submit(()=>page.locator('#undo').click());
      assert(await (await waitFrame()).locator('[data-testid="cayw-component-button"]').count()===1, 'Duplicate undo failed');
      checks.push('framework_duplicate_delete_and_undo');
      frame=await waitFrame();
      await frame.locator('#second-card').scrollIntoViewIfNeeded();
      const from=await frame.locator('#second-card').boundingBox(), to=await frame.locator('#first-card').boundingBox();
      const moved=page.waitForResponse(response=>response.url()===`${url}api/operation`);
      await page.mouse.move(from.x+from.width/2,from.y+12);
      await page.mouse.down();
      await page.mouse.move(from.x+from.width/2,from.y+26,{steps:4});
      const rowFlow=await frame.locator('#static-cards').evaluate(element=>{
        const css=getComputedStyle(element);
        return /grid/.test(css.display)||(/flex/.test(css.display)&&css.flexDirection.startsWith('row'));
      });
      await page.mouse.move(rowFlow?to.x+2:to.x+to.width/2,rowFlow?to.y+12:to.y+2,{steps:16});
      await page.mouse.up();
      const moveResponse=await moved;
      assert(moveResponse.ok(),`Pointer move failed: ${await moveResponse.text()}`);
      result.pointer_move=JSON.parse(moveResponse.request().postData());
      assert(result.pointer_move.kind==='move'&&result.pointer_move.placement==='before','Pointer did not choose the before drop zone');
      frame=await waitFrame();
      assert(await frame.locator('#static-cards').evaluate(element=>element.querySelector('article').id)==='second-card','Pointer move did not reorder source cards');
      await selectionMatches('#second-card');
      await submit(()=>page.locator('#undo').click());
      await submit(()=>page.locator('#redo').click());
      checks.push('framework_pointer_reorder_undo_redo_selection');
      for(const selector of ['#dynamic-list li','.shared-label']) {
        frame=await waitFrame();
        await frame.locator(selector).first().click();
        await page.locator('.readonly-panel').waitFor({state:'visible'});
        assert(await page.locator('.readonly-panel').innerText(),`Missing readonly explanation for ${selector}`);
      }
      checks.push('loop_and_duplicate_instances_remain_readonly');
      await select('#dynamic-style');
      const beforeDynamic=await documentData();
      const refused=page.waitForResponse(response=>response.url()===`${url}api/operation`);
      await page.getByTestId('editor-sidebar-width').fill('199');
      await page.getByTestId('editor-sidebar-width-apply').click();
      const refusedResponse=await refused;
      assert(refusedResponse.status()===400,'Dynamic style was overwritten');
      const refusedData=await refusedResponse.json();
      assert(refusedData.error && !refusedData.error.outcome_unknown,'Dynamic style rejection did not give a known error');
      const afterDynamic=await documentData();
      assert(beforeDynamic.snapshot.revision===afterDynamic.snapshot.revision,'Rejected dynamic style changed the draft');
      checks.push('dynamic_style_refused_without_replacing_expression');
      await select('#insertion-target');
      await page.getByTestId('editor-placement-inside').click();
      await submit(()=>page.getByTestId('editor-palette-item-hero').click());
      await selectionMatches('[data-testid="cayw-component-hero"]');
      frame=await waitFrame();
      const heroStyle=await frame.getByTestId('cayw-component-hero').evaluate(element=>{
        const names=[];
        const walk=rules=>{
          for(const rule of rules){
            if(rule.type===CSSRule.KEYFRAMES_RULE)names.push(rule.name);
            if(rule.cssRules)walk(rule.cssRules);
          }
        };
        for(const sheet of document.styleSheets){try{walk(sheet.cssRules);}catch{}}
        return {display:getComputedStyle(element).display,keyframes:names};
      });
      assert(heroStyle.display==='grid'&&heroStyle.keyframes.includes('cayw-fade-up'),'Inserted section lost its compiled CSS');
      const styledSnapshot=await documentData();
      const heroFile=styledSnapshot.snapshot.files.find(file=>file.path===styledSnapshot.snapshot.nodes.find(node=>node.id===styledSnapshot.snapshot.selected_id).file);
      if(framework==='vue'){
        const template=heroFile.source.match(/<template\b[^>]*>([\s\S]*?)<\/template>/)?.[1];
        assert(template&&!/<style\b/i.test(template),'Vue component style was left inside the template');
        const baselineFile=initial.snapshot.files.find(file=>file.path===heroFile.path);
        for(const block of baselineFile.source.match(/<script\b[\s\S]*?<\/script>|<style\b[^>]*\bscoped\b[\s\S]*?<\/style>/g)||[]){
          assert(heroFile.source.includes(block),'Vue insertion changed an existing script or scoped style block');
        }
      }
      checks.push('styled_section_compiles_and_preserves_existing_sfc_blocks');
      result.inserted_section_style=heroStyle;
      const unsaved=await documentData();
      const baseline=new Map(initial.snapshot.files.map(file=>[file.path,file.source]));
      result.dirty_source_files=unsaved.snapshot.files.filter(file=>baseline.get(file.path)!==file.source).map(file=>file.path);
      assert(unsaved.dirty && result.dirty_source_files.length>=2,'Multiple source files are not dirty');
      if(options.beforeSave) await options.beforeSave(unsaved);
      await shot('before-save');
      const saved=await save(200);
      assert(!saved.dirty,'Successful Host save left dirty source');
      checks.push('multifile_host_save');
      await page.locator('#preview-mode').click();
      frame=await waitFrame();
      const counterBefore=await frame.locator('#counter').innerText();
      await frame.locator('#counter').click();
      await frame.waitForFunction(before=>document.querySelector('#counter').innerText!==before,counterBefore);
      await frame.locator('#name-input').fill('Framework interaction');
      await frame.getByTestId('form-submit').click();
      await frame.waitForFunction(()=>document.querySelector('#form-output').textContent.trim()==='Hello, Framework interaction.');
      checks.push('original_counter_and_form_interactions_preserved');
      await page.locator('.device-btn[data-device="mobile"]').click();
      frame=await waitFrame();
      await frame.waitForFunction(()=>innerWidth===390);
      const mobile=await frame.evaluate(()=>({width:innerWidth,overflow:document.documentElement.scrollWidth-innerWidth}));
      assert(mobile.overflow<=0,`Edited framework page added mobile overflow: ${mobile.overflow}`);
      result.mobile=mobile;
      checks.push('framework_mobile_width');
      await shot('saved-mobile');
    }
    assert(!requests.some(request=>/prompt|rpc|model|provider/.test(request.path)), 'Visual editing requested a model');
    if(mode==='edit') {
      const data=await documentData();
      result.source_files=data.snapshot.files.map(file=>({path:file.path,sha256:file.sha256}));
    }
    const stored=await page.evaluate(()=>({local:Object.keys(localStorage),session:Object.keys(sessionStorage)}));
    assert(!JSON.stringify(stored).includes(token),'Editor credential was stored in browser storage');
    checks.push('no_model_request_or_token_storage');
    assert(errors.length===0,`Browser page errors: ${errors.join('; ')}`);
    result.passed=true;
  } catch(error) {
    result.error=error.message;
    await shot('failure').catch(()=>{});
  } finally {
    page.off('pageerror',onError);
    page.off('request',onRequest);
  }
  return result;
}
