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
const child = spawn(join(bin,'cayw'), ['start','--ui','web','--no-open','--workspace',workspace,'--offline','--demo-authorized'], {
  env:{PATH:process.env.PATH,HOME:home,CAYW_HOME:join(home,'cayw')}, stdio:['ignore','pipe','pipe'], detached:true,
});
let stderr = ''; child.stderr.on('data', bytes => { stderr += bytes; });
let browser;
const checks = [];
try {
  const lines = createInterface({input:child.stdout});
  const ready = JSON.parse(await Promise.race([once(lines,'line').then(([line])=>line),new Promise((_,reject)=>setTimeout(()=>reject(new Error('startup timeout')),40000).unref())]));
  browser = await chromium.launch({headless:true,...(process.env.CAYW_CHROMIUM ? {executablePath:process.env.CAYW_CHROMIUM} : {})});
  const page = await browser.newPage({viewport:{width:1280,height:900}});
  const errors = []; page.on('pageerror', error=>errors.push(error.message));
  await page.goto(ready.url);
  await page.locator('#workspace').waitFor({state:'visible'});
  for (const id of ['open-settings','settings-dialog','model-settings-form','plugin-settings-form']) {
    assert.equal(await page.locator(`#${id}`).count(), 0, `${id} is not on the conversation page`);
  }
  checks.push('conversation page has no settings dialog');
  await page.locator('#session-label').fill('Browser offline task'); await page.locator('#create-button').click();
  await page.locator('#session-title').filter({hasText:'Browser offline task'}).waitFor();
  await page.locator('#prompt').fill('Run the offline fixture'); await page.locator('#send-button').click();
  await page.waitForFunction(()=>document.querySelector('#messages').textContent.includes('headless runtime demo'));
  assert.equal(await readFile(join(workspace,'cayw-demo.txt'),'utf8'),'headless runtime demo');
  await page.screenshot({path:join(out,'conversation.png'),fullPage:true});
  await page.reload();
  await page.locator('#session-title').filter({hasText:'Browser offline task'}).waitFor();
  assert.equal(errors.length,0,JSON.stringify(errors));
  checks.push('browser create/prompt, real file output, refresh/reconnect to persistent conversation');
  await page.setViewportSize({width:390,height:844});
  await page.locator('#prompt').waitFor({state:'visible'});
  assert(await page.evaluate(()=>document.querySelector('#conversation').getBoundingClientRect().width <= innerWidth));
  await page.screenshot({path:join(out,'conversation-mobile.png'),fullPage:true});
  await writeFile(join(out,'summary.json'),JSON.stringify({passed:true,platform:process.platform,checks,errors},null,2)+'\n');
  console.log(JSON.stringify({passed:true,checks}));
} finally {
  await browser?.close();
  if(child.exitCode===null) { child.kill('SIGINT'); await once(child,'exit'); }
  await writeFile(join(out,'process.stderr'),stderr);
}
