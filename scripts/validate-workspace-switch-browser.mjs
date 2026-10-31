// Registered local workspaces, actual gateway/Runtime, and an in-flight browser
// task. No screenshots are exported; no external model or real keys are used.
// CAYW_PLAYWRIGHT_MODULE=/absolute/playwright/index.mjs CAYW_CHROMIUM=/path/chromium
// node scripts/validate-workspace-switch-browser.mjs BIN_DIRECTORY NEW_EVIDENCE_DIR
import assert from 'node:assert/strict';
import {spawn, execFileSync} from 'node:child_process';
import {mkdir, readFile, writeFile, access} from 'node:fs/promises';
import {resolve, join} from 'node:path';
import {createInterface} from 'node:readline';
import {once} from 'node:events';
import {setTimeout as delay} from 'node:timers/promises';
const bin = resolve(process.argv[2]); const out = resolve(process.argv[3]);
await mkdir(out); const home = join(out,'home'); await mkdir(home,{mode:0o700});
const roots = Object.fromEntries(['A','B','C'].map(name=>[name,join(out,name)]));
for (const root of Object.values(roots)) await mkdir(root);
const env = {PATH:process.env.PATH,HOME:home,CAYW_HOME:join(home,'cayw')};
for (const root of Object.values(roots)) execFileSync(join(bin,'cayw'),['init','--workspace',root],{env,stdio:'pipe'});
const children = []; const logs = [];
async function launch(args) {
  const child = spawn(join(bin,'cayw'),args,{env,stdio:['ignore','pipe','pipe'],detached:true}); children.push(child);
  let stderr = ''; child.stderr.on('data', bytes=>{stderr+=bytes}); logs.push(()=>stderr);
  const lines = createInterface({input:child.stdout});
  const [line] = await Promise.race([once(lines,'line'),new Promise((_,reject)=>setTimeout(()=>reject(new Error('readiness timeout')),40000).unref())]);
  return {child,ready:JSON.parse(line)};
}
async function stop(child) { if(child.exitCode===null){ child.kill('SIGINT'); const [code]=await once(child,'exit');assert.equal(code,0); } }
async function until(fn) { const deadline=Date.now()+30000; while(true){const result=await fn();if(result)return result;assert(Date.now()<deadline,'state transition timed out');await delay(30);} }
const {chromium} = await import(process.env.CAYW_PLAYWRIGHT_MODULE || 'playwright');
let browser; const checks=[]; const receipts={};
try {
  const external = await launch(['serve','--offline','--workspace',roots.C]);
  const app = await launch(['start','--offline','--workspace',roots.A]);
  const token = await readFile(app.ready.web_token_file,'utf8');
  async function rpc(method,params={},workspace_id='local',good=true) {
    const response=await fetch(app.ready.url+'api/rpc',{method:'POST',headers:{'Content-Type':'application/json',Authorization:`Bearer ${token}`},body:JSON.stringify({method,params,workspace_id})});
    const body=await response.json();
    if(good){assert(response.ok,JSON.stringify(body));assert('result' in body);return body.result;}
    assert(body.error,JSON.stringify(body));return body.error;
  }
  const catalog=await rpc('settings.workspaces.list');
  const id = name=>catalog.workspaces.find(item=>item.path===roots[name]).id;
  assert.equal(id('A'),'local');
  assert.equal((await rpc('settings.workspaces.open',{id:'/unregistered/arbitrary/path'},'local',false)).outcome_unknown,false);
  assert.equal((await rpc('settings.workspaces.open',{id:'local',path:'/etc'},'local',false)).outcome_unknown,false);
  checks.push('only registered opaque IDs accepted; arbitrary paths/extra fields rejected');
  browser=await chromium.launch({headless:true,...(process.env.CAYW_CHROMIUM?{executablePath:process.env.CAYW_CHROMIUM}:{})});
  const page=await browser.newPage({viewport:{width:1280,height:900}});const errors=[];const requests=[];
  page.on('pageerror',e=>errors.push(e.message));
  page.on('request',request=>{if(request.url().endsWith('/api/rpc')){const body=request.postDataJSON();requests.push({method:body.method,workspace_id:body.workspace_id,session_id:body.params.session_id,task_id:body.params.task_id});}});
  await page.goto(app.ready.url);await page.locator('#web-token').fill(token);await page.locator('#login-button').click();
  await page.locator('#workspace-choice option').filter({hasText:roots.B}).waitFor({state:'attached'});
  await until(async()=>await page.locator('#wb-editor').isDisabled());
  assert(await page.locator('#wb-trace').isDisabled());
  assert.match(await page.locator('#optional-tools-status').textContent(),/未安装/);
  assert.match(await page.locator('#optional-tools-status').textContent(),/cayw-visual-editor --help/);
  assert.match(await page.locator('#optional-tools-status').textContent(),/Pi 兼容/);
  checks.push('default UI disables absent Editor/Trace and shows install/start paths and Pi build/enable state');
  await page.locator('#session-label').fill('A in-flight');await page.locator('#create-button').click();
  await page.locator('#session-title').filter({hasText:'A in-flight'}).waitFor();
  await page.locator('#prompt').fill('Wait for exact file approval');await page.locator('#send-button').click();
  const aSession=(await until(async()=>{const list=await rpc('session.list');return list.find(s=>s.label==='A in-flight');})).id;
  await until(async()=>{const s=await rpc('session.get',{session_id:aSession});return s.pending_approvals.length>0;});
  const aTask=(await rpc('session.tasks',{session_id:aSession}))[0];assert(['running','waiting_approval'].includes(aTask.state));
  receipts.original_task={id:aTask.id,worker_id:aTask.worker_id,attempt_id:aTask.attempt_id,fence:aTask.fence};
  await page.locator('#workspace-choice').selectOption(id('B'));await page.locator('#switch-workspace').click();
  await page.locator('#workspace-switch-status').filter({hasText:`当前：${roots.B}`}).waitFor();
  assert.deepEqual(await rpc('session.list',{},id('B')),[]);
  const aAfter=await rpc('task.get',{task_id:aTask.id});
  for(const key of ['id','worker_id','attempt_id','fence']) assert.equal(aAfter[key],aTask[key]);
  assert.equal(aAfter.state,aTask.state);assert.equal(aAfter.cancel_requested,false);
  assert.equal((await rpc('session.get',{session_id:aSession},id('B'),false)).code,'workspace_mismatch');
  assert.equal((await rpc('task.resume',{task_id:aTask.id},id('B'),false)).code,'workspace_mismatch');
  const reusedB=await rpc('settings.workspaces.open',{id:id('B')});
  assert.equal(reusedB.reused,true);assert.equal(reusedB.managed,true);assert(!('ipc_addr' in reusedB));
  checks.push('switch A to managed B keeps original running Task/Attempt/fence and request; repeat open reuses runtime; cross-owner requests rejected');
  await page.locator('#session-label').fill('B isolated');await page.locator('#create-button').click();
  await page.locator('#session-title').filter({hasText:'B isolated'}).waitFor();
  assert.equal((await rpc('session.list',{},id('B'))).length,1);
  assert.equal((await rpc('session.list')).length,1);
  await page.locator('#workspace-choice').selectOption('local');await page.locator('#switch-workspace').click();
  await page.locator('#workspace-switch-status').filter({hasText:`当前：${roots.A}`}).waitFor();
  await page.locator('#session-title').filter({hasText:'A in-flight'}).waitFor();
  await page.locator('#approvals').waitFor({state:'visible'});
  assert.equal(requests.filter(r=>r.method==='task.resume').length,1,'switching does not replay or replace resume');
  assert.equal(requests.find(r=>r.method==='task.resume').workspace_id,'local');
  await page.locator('#cancel-button').click();
  const terminal = await until(async()=>{const task=await rpc('task.get',{task_id:aTask.id});return ['failed','cancelled','completed','interrupted','outcome_unknown'].includes(task.state) ? task : null;});
  assert.equal(terminal.state,'cancelled'); receipts.cancelled_task_state = terminal.state;
  await assert.rejects(access(join(roots.A,'cayw-demo.txt')));
  checks.push('switch back restores original session/approval; only original task resumes, explicit cancellation prevents file effect');
  await page.locator('#workspace-choice').selectOption(id('C'));await page.locator('#switch-workspace').click();
  await page.locator('#workspace-switch-status').filter({hasText:`当前：${roots.C}`}).waitFor();
  const reusedC=await rpc('settings.workspaces.open',{id:id('C')});assert(reusedC.reused);assert.equal(reusedC.managed,false);
  checks.push('browser switches to externally owned C by reusing it');
  assert.deepEqual(errors,[]);
  await browser.close();browser=null;await stop(app.child);
  assert.equal(external.child.exitCode,null,'gateway/host shutdown must not stop external C');
  execFileSync(join(bin,'cayw'),['models','list','--workspace',roots.C],{env,stdio:'pipe'});
  // Managed B is cleaned up by the original host; external C is left to its owner.
  let stoppedB = false;
  try { execFileSync(join(bin,'cayw'),['models','list','--workspace',roots.B],{env,stdio:'pipe',encoding:'utf8',timeout:30000}); }
  catch(error) { assert(error.status !== 0); assert.match(String(error.stderr),/no local service for this workspace/); stoppedB=true; }
  assert(stoppedB,'managed B unexpectedly remained connectable');
  checks.push('host shutdown removes its managed B; external C stays alive and queryable');
  const report={passed:true,platform:process.platform,checks,receipts};
  await writeFile(join(out,'summary.json'),JSON.stringify(report,null,2)+'\n');console.log(JSON.stringify(report));
} finally {
  await browser?.close();
  for(const child of children.reverse())await stop(child);
  await writeFile(join(out,'process.stderr'),logs.map(read=>read()).join('\n'));
}
