// Actual browser -> bundled WebUI -> authenticated runtime. No external models.
// CAYW_PLAYWRIGHT_MODULE=/absolute/playwright/index.mjs [CAYW_CHROMIUM=/path/chromium]
// node scripts/validate-product-settings-browser.mjs TARGET_DEBUG NEW_EVIDENCE_DIR
import assert from 'node:assert/strict';
import {spawn} from 'node:child_process';
import {mkdir, readFile, writeFile} from 'node:fs/promises';
import {resolve, join} from 'node:path';
import {createInterface} from 'node:readline';
import {once} from 'node:events';
const bin = resolve(process.argv[2]); const out = resolve(process.argv[3]);
await mkdir(out); const workspace = join(out,'workspace'); await mkdir(workspace);
const home = join(out,'home'); await mkdir(home, {mode:0o700});
const module = process.env.CAYW_PLAYWRIGHT_MODULE || 'playwright';
const {chromium} = await import(module);
const child = spawn(join(bin,'cayw'), ['start','--workspace',workspace,'--offline','--demo-authorized'], {
  env:{PATH:process.env.PATH,HOME:home,CAYW_HOME:join(home,'cayw')}, stdio:['ignore','pipe','pipe'], detached:true,
});
let stderr = ''; child.stderr.on('data', bytes => { stderr += bytes; });
let browser;
const checks = [];
try {
  const lines = createInterface({input:child.stdout});
  const ready = JSON.parse(await Promise.race([once(lines,'line').then(([line])=>line),new Promise((_,reject)=>setTimeout(()=>reject(new Error('startup timeout')),40000).unref())]));
  const token = await readFile(ready.web_token_file,'utf8');
  browser = await chromium.launch({headless:true,...(process.env.CAYW_CHROMIUM ? {executablePath:process.env.CAYW_CHROMIUM} : {})});
  const page = await browser.newPage({viewport:{width:1280,height:900}});
  const errors = []; page.on('pageerror', error=>errors.push(error.message));
  await page.goto(ready.url);
  await page.locator('#web-token').fill(token); await page.locator('#login-button').click();
  await page.locator('#workspace').waitFor({state:'visible'});
  await page.locator('#open-settings').click();
  await page.locator('#settings-workspace').filter({hasText:workspace}).waitFor();
  await page.locator('#settings-status').filter({hasText:'已完成'}).waitFor();
  const form = page.locator('#model-settings-form');
  await form.locator('[name=name]').fill('browser-daily');
  await form.locator('[name=service]').selectOption('anthropic');
  assert.equal(await form.locator('[name=api]').inputValue(), 'messages');
  assert.equal(await form.locator('[name=api_version]').inputValue(), '2023-06-01');
  await form.locator('[name=service]').selectOption('openai');
  await form.locator('[name=id]').fill('browser-model');
  await form.locator('[name=base_url]').fill('http://127.0.0.1:9/v1');
  await form.locator('button[type=submit]').click();
  await page.locator('#model-profiles').filter({hasText:'browser-daily'}).waitFor();
  assert(await page.locator('#model-profiles button').filter({hasText:'使用'}).isDisabled());
  checks.push('browser model save and provider-specific defaults, offline use disabled');
  const descriptor = {executable:join(bin,'cayw-runtime-plugin-fixture'),argv:['echo'],manifest:{name:'process',version:'1.0.0',protocol_version:1,kind:'external',capabilities:[{name:'process.echo',description:'Offline process lifecycle fixture',parameters:{type:'object',additionalProperties:false}}],permissions:{}}};
  await page.locator('#plugin-settings-form textarea').fill(JSON.stringify(descriptor));
  await page.locator('#plugin-settings-form button[type=submit]').click();
  await page.locator('#plugin-profiles').filter({hasText:'process 1.0.0'}).waitFor();
  await page.locator('#plugin-profiles button').filter({hasText:'启用'}).click();
  await page.locator('#settings-status').filter({hasText:'请先审阅'}).waitFor();
  await page.locator('#plugin-process-approval').check();
  await page.locator('#plugin-profiles button').filter({hasText:'启用'}).click();
  await page.locator('#plugin-profiles').filter({hasText:'可用'}).waitFor();
  assert.equal(await page.locator('#plugin-process-approval').isChecked(),false);
  await page.screenshot({path:join(out,'settings.png'),fullPage:true});
  checks.push('browser plugin register, mandatory process consent, actual process enable');
  await page.locator('#plugin-profiles button').filter({hasText:'停用'}).click();
  await page.locator('#plugin-profiles').filter({hasText:'已停用'}).waitFor();
  await page.locator('#close-settings').click();
  await page.locator('#session-label').fill('Browser offline task'); await page.locator('#create-button').click();
  await page.locator('#session-title').filter({hasText:'Browser offline task'}).waitFor();
  await page.locator('#prompt').fill('Run the offline fixture'); await page.locator('#send-button').click();
  await page.waitForFunction(()=>document.querySelector('#messages').textContent.includes('headless runtime demo'));
  assert.equal(await readFile(join(workspace,'cayw-demo.txt'),'utf8'),'headless runtime demo');
  await page.screenshot({path:join(out,'conversation.png'),fullPage:true});
  await page.reload(); await page.locator('#web-token').fill(token); await page.locator('#login-button').click();
  await page.locator('#session-title').filter({hasText:'Browser offline task'}).waitFor();
  assert.equal(errors.length,0,JSON.stringify(errors));
  checks.push('browser create/prompt, real file output, refresh/reconnect to persistent conversation');
  await page.setViewportSize({width:390,height:844});
  await page.locator('#open-settings').click(); await page.locator('#settings-status').filter({hasText:'已完成'}).waitFor();
  assert(await page.evaluate(()=>document.querySelector('#settings-dialog').getBoundingClientRect().width <= innerWidth));
  await page.screenshot({path:join(out,'settings-mobile.png'),fullPage:true});
  await writeFile(join(out,'summary.json'),JSON.stringify({passed:true,platform:process.platform,checks,errors},null,2)+'\n');
  console.log(JSON.stringify({passed:true,checks}));
} finally {
  await browser?.close();
  if(child.exitCode===null) { child.kill('SIGINT'); await once(child,'exit'); }
  await writeFile(join(out,'process.stderr'),stderr);
}
