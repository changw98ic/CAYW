// Supplementary browser checks. The runner owns the actual services and source files.
async (page, options) => {
  const {url, token, mode, evidence} = options;
  const checks=[];
  const errors=[];
  const requests=[];
  const resources=[];
  const summary={url,mode,checks,errors,requests,resources,passed:false};
  const onResponse=response=>{
    const address=new URL(response.url());
    if(/^https?:$/.test(address.protocol)&&address.hostname!=='127.0.0.1')resources.push({url:`${address.origin}${address.pathname}`,status:response.status()});
  };
  const onFailed=request=>{
    const address=new URL(request.url());
    if(/^https?:$/.test(address.protocol))resources.push({url:`${address.origin}${address.pathname}`,failure:request.failure()?.errorText});
  };
  page.on('response',onResponse);
  page.on('requestfailed',onFailed);
  const assert=(condition,message)=>{if(!condition)throw new Error(message);};
  const onError=error=>errors.push(error.message);
  const onRequest=request=>{
    const path=new URL(request.url()).pathname;
    if(path.startsWith('/api/'))requests.push({path,method:request.method()});
  };
  page.on('pageerror',onError);
  page.on('request',onRequest);
  let current;
  const idle=async()=>{
    await page.waitForFunction(()=>document.querySelector('#enter-edit')?.disabled===false);
    await page.waitForFunction(()=>Boolean(document.querySelector('#preview-frame')?.src));
    for(let attempt=0;attempt<600;attempt+=1) {
      const expected=await page.locator('#preview-frame').getAttribute('src');
      const candidate=page.frames().find(frame=>frame!==page.mainFrame()&&frame.url()===expected);
      if(candidate&&await candidate.locator('#cayw-editor-overlay').count()) {
        current=candidate;
        return current;
      }
      await page.waitForTimeout(50);
    }
    throw new Error('Instrumented source frame did not become ready');
  };
  const mutate=async action=>{
    const previous=await page.locator('#preview-frame').getAttribute('src');
    const responsePromise=page.waitForResponse(response=>response.url().endsWith('/api/operation')&&response.request().method()==='POST');
    await action();
    const response=await responsePromise;
    assert(response.ok(),`Operation rejected: ${await response.text()}`);
    await page.waitForFunction(src=>document.querySelector('#preview-frame').src!==src,previous);
    return idle();
  };
  const right=async selector=>{
    current=await idle();
    await current.locator(selector).first().click({button:'right'});
    await page.locator('#context-menu').waitFor();
  };
  const text=async(selector,value)=>{
    await right(selector);
    await page.getByTestId('editor-menu-text').fill(value);
    return mutate(()=>page.getByTestId('editor-menu-text-apply').click());
  };
  const save=async()=>{
    const responsePromise=page.waitForResponse(response=>response.url().endsWith('/api/save'));
    responsePromise.catch(()=>{});
    await page.locator('#save').click();
    const response=await responsePromise;
    assert(response.ok(),`Save rejected: ${await response.text()}`);
    await page.waitForFunction(()=>document.querySelector('#save-state').textContent==='已保存');
    return idle();
  };
  try {
    await page.setViewportSize({width:1600,height:1100});
    await page.emulateMedia({reducedMotion:'reduce'});
    await page.goto(url);
    await page.locator('#token-input').fill(token);
    await page.locator('#connect-submit').click();
    await page.locator('#workspace').waitFor();
    current=await idle();
    await page.locator('#enter-edit').click();
    await current.waitForFunction(()=>document.documentElement.classList.contains('cayw-editing'));
    if(mode==='agency') {
      const geometry=()=>current.locator('#portfolio .portfolio-item').first().evaluate(element=>{
        const rect=element.getBoundingClientRect();
        return {width:rect.width,height:rect.height,font:getComputedStyle(element).font};
      });
      summary.geometry=await geometry();
      // Portfolio images have a real hover layer above them; use an unobscured
      // original team image for a pointer selection without altering page CSS.
      await right('#team .team-member img');
      assert((await page.locator('#selection-tag').textContent()).startsWith('img'),'Existing image was not selected');
      await page.getByTestId('editor-menu-src').fill('assets/img/team/2.jpg');
      current=await mutate(()=>page.getByTestId('editor-menu-src-apply').click());
      await current.locator('#team .team-member img').first().evaluate(image=>image.decode());
      assert((await current.locator('#team .team-member img').first().getAttribute('src'))==='assets/img/team/2.jpg','Existing image source was not changed');
      await page.keyboard.press('Escape');
      await page.locator('#selection-parent').click();
      await page.locator('#selection-tag').filter({hasText:/^div/}).waitFor();
      checks.push('existing_real_image_replacement_and_parent');
      await right('#portfolio .portfolio-caption-heading');
      current=await text('#portfolio .portfolio-caption-heading','Issue 30 image project');
      await save();
      checks.push('image_and_text_saved_through_host');
      await page.locator('#preview-mode').click();
      current=await idle();
      await current.locator('.portfolio-link').first().click();
      await current.locator('#portfolioModal1.show').waitFor();
      await page.screenshot({path:`${evidence}-modal.png`});
      await current.locator('#portfolioModal1 [data-bs-dismiss="modal"]').first().click();
      await current.locator('#portfolioModal1.show').waitFor({state:'hidden'});
      checks.push('original_modal_opens_and_closes');
      await page.getByTestId('editor-device-mobile').click();
      await current.waitForFunction(()=>innerWidth===390);
      await current.locator('.navbar-toggler').click();
      await current.locator('.navbar-collapse.show').waitFor();
      assert(await current.locator('.navbar-toggler').getAttribute('aria-expanded')==='true','Agency mobile navigation failed');
      summary.mobile=await current.evaluate(()=>({width:innerWidth,overflow:document.documentElement.scrollWidth-innerWidth,images:[...document.images].filter(image=>image.complete&&image.naturalWidth>0).length}));
      assert(summary.mobile.overflow===0,'Agency has new mobile overflow');
      await page.screenshot({path:`${evidence}-mobile.png`});
      checks.push('original_real_page_controls_and_mobile_layout');
    } else if(mode==='dynamic') {
      for(const selector of ['#generated','#shadow-host','#drawing','#embedded','#clone-target','#template']) {
        await right(selector);
        assert((await page.locator('#selection-tag').textContent()).includes('只读'),`${selector} is not read-only`);
        assert(await page.getByTestId('editor-menu-bridge-parent').isVisible(),`${selector} lacks an explicit parent action`);
        assert(!await current.locator('.cayw-handle').first().isVisible(),`${selector} exposes resize handles`);
      }
      assert(await current.locator('#generated').textContent()==='脚本生成内容','Dynamic content stopped rendering');
      checks.push('generated_shadow_canvas_iframe_duplicate_regions_readonly');
      await text('#static-text','静态相邻区域仍可保存');
      await save();
      assert(await current.locator('#generated').textContent()==='脚本生成内容','Editing a neighboring static node lost dynamic content');
      checks.push('neighboring_static_region_edits_and_preserves_dynamic_rendering');
      await page.keyboard.press('Escape');
      const operationCount=requests.filter(request=>request.path==='/api/operation'&&request.method==='POST').length;
      const spoof=await current.evaluate(()=>{
        const id=document.querySelector('#static-text').getAttribute('data-cayw-node');
        parent.postMessage({type:'cayw:operation',operation:{kind:'text',id,text:'伪造消息不应修改源码'}},'*');
        const channel=new MessageChannel();
        parent.postMessage({type:'cayw:connect',url:location.href},'*',[channel.port2]);
        channel.port1.postMessage({type:'cayw:operation',operation:{kind:'text',id,text:'替换通道不应修改源码'}});
        return id;
      });
      await page.waitForTimeout(400);
      assert(requests.filter(request=>request.path==='/api/operation'&&request.method==='POST').length===operationCount,'Page script forged an editor operation');
      assert(await current.locator('#static-text').textContent()==='静态相邻区域仍可保存','Forged preview messages changed the source draft');
      summary.forged_target=spoof;
      checks.push('preview_window_operation_and_channel_replacement_rejected');
      await right('#content-box');
      await page.getByTestId('editor-menu-tab-font').click();
      assert((await page.getByTestId('editor-menu-font-family').inputValue()).includes('Georgia'),'Font selector did not reflect existing Georgia');
      await page.getByTestId('editor-menu-tab-color').click();
      const colorRow=page.getByTestId('editor-menu-color-text-hex').locator('..').locator('..');
      assert(!(await colorRow.textContent()).includes('透明'),'Opaque RGB red was labelled transparent');
      const alpha=await page.getByTestId('editor-menu-color-background-hex').inputValue();
      assert(alpha==='rgba(0, 0, 0, 0.5)'||/^#[a-f0-9]{8}$/i.test(alpha),'Existing background alpha was removed from color input');
      current=await mutate(()=>page.getByTestId('editor-menu-color-background-apply').click());
      const preservedAlpha=await current.locator('#content-box').evaluate(element=>getComputedStyle(element).backgroundColor);
      assert(/^rgba\(0, 0, 0, 0\.50?\d*\)$/.test(preservedAlpha),'Applying an unchanged color lost alpha');
      checks.push('font_and_opaque_color_reflect_current_style','unchanged_color_application_preserves_alpha');
      await page.keyboard.press('Escape');
      await current.locator('#content-box').click();
      const oldSize=await current.locator('#content-box').evaluate(element=>{const rect=element.getBoundingClientRect();return {width:rect.width,height:rect.height};});
      const handle=await current.locator('.cayw-handle[data-dir="e"]').boundingBox();
      assert(handle,'Content-box resize handle is missing');
      await page.mouse.move(handle.x+handle.width/2,handle.y+handle.height/2);
      await page.mouse.down();
      await page.mouse.move(handle.x+handle.width/2+24,handle.y+handle.height/2,{steps:12});
      const resizeResponse=page.waitForResponse(response=>response.url().endsWith('/api/operation'));
      const beforeResize=await page.locator('#preview-frame').getAttribute('src');
      await page.mouse.up();
      assert((await resizeResponse).ok(),'Content-box resize was rejected');
      await page.waitForFunction(src=>document.querySelector('#preview-frame').src!==src,beforeResize);
      current=await idle();
      const newSize=await current.locator('#content-box').evaluate(element=>{const rect=element.getBoundingClientRect();return {width:rect.width,height:rect.height,boxSizing:getComputedStyle(element).boxSizing};});
      assert(newSize.boxSizing==='content-box'&&Math.abs(newSize.width-oldSize.width-24)<1&&newSize.height===oldSize.height,'Resize added padding or border twice');
      summary.resize={before:oldSize,after:newSize};
      checks.push('content_box_resize_tracks_pointer_without_padding_jump');
      await page.keyboard.press('Escape');
      await page.waitForTimeout(450);
      await current.locator('#content-box').click();
      const source=await current.locator('#content-box').boundingBox();
      const frameBox=await page.locator('#preview-frame').boundingBox();
      const beforeLeave=requests.filter(request=>request.path==='/api/operation'&&request.method==='POST').length;
      await page.mouse.move(source.x+20,source.y+20);
      await page.mouse.down();
      await page.mouse.move(source.x+30,source.y+35,{steps:8});
      await page.mouse.move(frameBox.x-20,source.y+35,{steps:12});
      await page.mouse.up();
      await page.mouse.move(source.x+50,source.y+40);
      await current.waitForFunction(()=>!document.querySelector('.cayw-ghost')&&[...document.querySelectorAll('.cayw-line,.cayw-drop')].every(element=>getComputedStyle(element).display==='none'));
      assert(requests.filter(request=>request.path==='/api/operation'&&request.method==='POST').length===beforeLeave,'Leaving the iframe committed an unintended drag');
      checks.push('leaving_iframe_cancels_drag_and_clears_feedback');
      await page.waitForTimeout(450);
      await current.locator('#absolute').click();
      const before=await current.locator('#absolute').boundingBox();
      const previous=await page.locator('#preview-frame').getAttribute('src');
      await page.mouse.move(before.x+20,before.y+before.height/2);
      await page.mouse.down();
      await page.mouse.move(before.x+60,before.y+before.height/2+30,{steps:16});
      const responsePromise=page.waitForResponse(response=>response.url().endsWith('/api/operation'));
      await page.mouse.up();
      assert((await responsePromise).ok(),'Position drag rejected');
      await page.waitForFunction(src=>document.querySelector('#preview-frame').src!==src,previous);
      current=await idle();
      summary.position=await current.locator('#absolute').evaluate(element=>({left:getComputedStyle(element).left,top:getComputedStyle(element).top,position:getComputedStyle(element).position,transform:getComputedStyle(element).transform}));
      assert(summary.position.position==='absolute'&&summary.position.left==='80px'&&summary.position.top==='70px','Positioned element did not retain its position model');
      await save();
      checks.push('absolute_position_drag_retains_position_and_transform');
      await page.screenshot({path:`${evidence}-dynamic.png`});
    } else if(mode==='restart') {
      assert(await current.locator('.hero-name').textContent()==='Issue 30 真实页面编辑','Saved text did not survive process restart');
      assert(await current.locator('[data-cayw-component="container"] input').count()===1,'Saved structure did not survive process restart');
      assert(await current.locator('[data-cayw-component="container"] button').count()===1,'Saved palette insertion did not survive process restart');
      assert(await current.locator('input[name="cayw-input"]').evaluate(element=>getComputedStyle(element).width)==='204px','Saved resize did not survive restart');
      checks.push('process_restart_retains_text_style_and_structure');
      await text('.hero-name','Issue 30 重启后继续编辑');
      await save();
      checks.push('reenter_editor_and_save_again_after_restart');
    } else if(mode==='conflict-draft') {
      await text('.hero-name','旧映射草稿不得覆盖新源码');
      checks.push('draft_prepared_before_external_edit');
    } else if(mode==='conflict-save') {
      const responsePromise=page.waitForResponse(response=>response.url().endsWith('/api/save'));
      await page.locator('#save').click();
      const response=await responsePromise;
      assert(response.status()===409,'External change did not block saving');
      await page.locator('#conflict-banner').waitFor();
      assert(await page.locator('#save').isDisabled(),'Conflict did not disable saving');
      checks.push('external_source_change_returns_409_and_locks_writes');
      await page.getByTestId('editor-conflict-reload').click();
      await page.waitForFunction(()=>document.querySelector('#save-state').textContent==='已保存');
      current=await idle();
      assert(await current.locator('#external-change-proof').count()===1,'Reload did not read the external source');
      await text('.hero-name','Issue 30 冲突重载后继续编辑');
      await save();
      checks.push('explicit_reload_refreshes_mapping_before_next_save');
    } else throw new Error(`Unknown verification mode ${mode}`);
    assert(errors.length===0,`Browser errors: ${errors.join('; ')}`);
    assert(!requests.some(request=>request.path==='/api/rpc'||request.path.includes('prompt')),'A deterministic edit invoked a model');
    summary.passed=true;
  } catch(error) {
    summary.error=error.message;
    await page.screenshot({path:`${evidence}-failure.png`}).catch(()=>{});
  } finally {
    page.off('pageerror',onError);
    page.off('request',onRequest);
    page.off('response',onResponse);
    page.off('requestfailed',onFailed);
  }
  return summary;
}
