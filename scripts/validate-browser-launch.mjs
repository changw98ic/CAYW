// Real CLI -> system xdg-open -> Chromium -> WebUI -> offline Runtime.
// Test-only HOME, BROWSER handler and Chromium profile; no desktop settings changed.
// node scripts/validate-browser-launch.mjs BIN_DIRECTORY NEW_EVIDENCE_DIR
import assert from 'node:assert/strict';
import {spawn, execFileSync} from 'node:child_process';
import {mkdir, readFile, writeFile, chmod} from 'node:fs/promises';
import {join, resolve} from 'node:path';
import {createInterface} from 'node:readline';
import {once} from 'node:events';
import {setTimeout as delay} from 'node:timers/promises';
const bin=resolve(process.argv[2]), out=resolve(process.argv[3]);
await mkdir(out); const home=join(out,'home'), workspace=join(out,'workspace'), profile=join(out,'chromium-profile');
await mkdir(home,{mode:0o700}); await mkdir(workspace);
const chromiumPath=process.env.CAYW_CHROMIUM || '/usr/bin/chromium';
const {chromium}=await import(process.env.CAYW_PLAYWRIGHT_MODULE || 'playwright');
const quote=value=>`'${value.replaceAll("'", "'\\''")}'`;
const handler=join(out,'test-browser');
await writeFile(handler,`#!/bin/sh\nprintf 'opened\\n' >> ${quote(join(out,'opener-count'))}\n${quote(chromiumPath)} --headless --no-sandbox --remote-debugging-port=0 --user-data-dir=${quote(profile)} "$1" >${quote(join(out,'chromium.stderr'))} 2>&1 &\nprintf '%s' "$!" > ${quote(join(out,'chromium.pid'))}\n`);
await chmod(handler,0o700);
const xdg=join(out,'xdg');await mkdir(xdg);
const env={XDG_DATA_HOME:xdg,XDG_DATA_DIRS:xdg,XDG_CONFIG_HOME:xdg,XDG_CONFIG_DIRS:xdg,PATH:process.env.PATH,HOME:home,CAYW_HOME:join(home,'cayw'),DISPLAY:':99',BROWSER:handler,XDG_CURRENT_DESKTOP:'X-Generic'};
const children=[],checks=[],logs=[];let browser;
async function until(fn){const end=Date.now()+30000;while(true){const v=await fn();if(v)return v;assert(Date.now()<end,'timed out');await delay(50);}}
async function launch(args,overrides={}){
 const child=spawn(join(bin,'cayw'),args,{env:{...env,...overrides},stdio:['ignore','pipe','pipe'],detached:true});children.push(child);
 let stderr='';child.stderr.on('data',b=>stderr+=b);logs.push(()=>stderr);
 const lines=createInterface({input:child.stdout});
 const [line]=await Promise.race([once(lines,'line'),once(child,'exit').then(([code])=>{throw new Error(`startup exited ${code}: ${stderr}`)}),new Promise((_,reject)=>setTimeout(()=>reject(new Error(`startup timeout: ${stderr}`)),40000).unref())]);
 return {child,ready:JSON.parse(line)};
}
async function repeat(args,overrides={}){const r=await launch(args,overrides);if(r.child.exitCode===null)await once(r.child,'exit');assert.equal(r.child.exitCode,0);return r.ready;}
async function stop(child){if(child.exitCode===null){child.kill('SIGINT');const [code]=await once(child,'exit');assert.equal(code,0);}}
try{
 const app=await launch(['start','--workspace',workspace,'--offline','--demo-authorized']);
 assert.equal(app.ready.browser_open,'opened');assert(!app.ready.launch_url);assert(!app.ready.web_token_file);
 const devtools=await until(async()=>{try{return (await readFile(join(profile,'DevToolsActivePort'),'utf8')).trim().split('\n')}catch{return null}});
 browser=await chromium.connectOverCDP(`http://127.0.0.1:${devtools[0]}`);
 const context=browser.contexts()[0];const page=await until(async()=>context.pages().find(p=>p.url().startsWith(app.ready.url)));
 const errors=[];page.on('pageerror',e=>errors.push(e.message));
 await page.locator('#workspace').waitFor({state:'visible'});assert.equal(page.url(),app.ready.url);
 const cookies=await context.cookies(app.ready.url);const cookie=cookies.find(c=>c.name.startsWith('cayw_session_'));
 assert(cookie?.httpOnly);assert.equal(cookie.sameSite,'Strict');assert(!(await page.evaluate(()=>document.cookie)).includes('cayw_session_'));
 assert.equal(await page.evaluate(()=>localStorage.length+sessionStorage.length),0);
 const requests=[];page.on('request',r=>{if(r.url().endsWith('/api/rpc'))requests.push(r.postDataJSON().method)});
 await page.locator('#session-label').fill('Automatic launch');await page.locator('#create-button').click();
 await page.locator('#session-title').filter({hasText:'Automatic launch'}).waitFor();
 await page.locator('#prompt').fill('offline task');await page.locator('#send-button').click();
 await page.waitForFunction(()=>document.querySelector('#messages').textContent.includes('headless runtime demo'));
 assert.equal(await readFile(join(workspace,'cayw-demo.txt'),'utf8'),'headless runtime demo');
 await page.reload();await page.locator('#session-title').filter({hasText:'Automatic launch'}).waitFor();
 assert.equal(page.url(),app.ready.url);assert.equal(requests.filter(m=>m==='task.enqueue').length,1);
 checks.push('real system xdg-open launches Chromium; automatic cookie login, offline task, clean URL and refresh without token/replay');
 const count=await readFile(join(out,'opener-count'),'utf8');
 const second=await repeat(['start','--workspace',workspace]);assert.equal(second.url,app.ready.url);assert.equal(second.browser_open,'opened');
 await until(async()=>(await readFile(join(out,'opener-count'),'utf8')).length>count.length);
 assert.equal(app.child.exitCode,null);checks.push('duplicate start automatically opens a new authenticated page and reuses owned service');
 const noOpen=await repeat(['start','--workspace',workspace,'--no-open']);assert.equal(noOpen.browser_open,'disabled');assert(noOpen.launch_url);
 const beforeSkip=await readFile(join(out,'opener-count'),'utf8');
 const fresh=await browser.newContext();const freshPage=await fresh.newPage();
 assert.equal((await freshPage.goto(app.ready.url+'?launch='+'0'.repeat(64))).status(),403);
 await freshPage.goto(noOpen.launch_url);await freshPage.locator('#workspace').waitFor({state:'visible'});assert.equal(freshPage.url(),app.ready.url);
 const other=await browser.newContext();assert.equal((await (await other.newPage()).goto(noOpen.launch_url)).status(),403);await other.close();
 await freshPage.locator('#logout').click();await freshPage.locator('#login-panel').waitFor({state:'visible'});
 await freshPage.reload();await freshPage.locator('#login-error').filter({hasText:'cayw start'}).waitFor();
 assert(await freshPage.locator('#workspace').isHidden());await fresh.close();
 checks.push('no-open clickable one-use link, invalid and replayed link rejection, logout revokes session, refresh shows command recovery');
 const ssh=await repeat(['start','--workspace',workspace],{SSH_CONNECTION:'synthetic ssh session'});assert.equal(ssh.browser_open,'ssh');assert(ssh.launch_url);
 const sshTty=await repeat(['start','--workspace',workspace],{SSH_TTY:'/dev/synthetic'});assert.equal(sshTty.browser_open,'ssh');
 const headless=await repeat(['start','--workspace',workspace],{DISPLAY:'',WAYLAND_DISPLAY:''});assert.equal(headless.browser_open,'no-desktop');assert(headless.launch_url);
 assert.equal(await readFile(join(out,'opener-count'),'utf8'),beforeSkip);
 const failed=await repeat(['start','--workspace',workspace],{BROWSER:'/bin/false',PATH:'/usr/bin:/bin'});assert.equal(failed.browser_open,'failed');assert(failed.launch_url);assert.equal(app.child.exitCode,null);
 const fallbackPage=await context.newPage();await fallbackPage.goto(failed.launch_url);await fallbackPage.locator('#workspace').waitFor({state:'visible'});await fallbackPage.close();
 checks.push('no-open, SSH_CONNECTION, SSH_TTY and no-desktop do not invoke opener; opener failure leaves service and a usable link');
 await stop(app.child);
 const restarted=await launch(['start','--workspace',workspace,'--offline','--no-open']);
 const oldTicket=new URL(ssh.launch_url).search;const rejected=await fetch(restarted.ready.url+oldTicket,{redirect:'manual'});assert.equal(rejected.status,403);
 await page.goto(restarted.ready.launch_url);await page.locator('#session-title').filter({hasText:'Automatic launch'}).waitFor();
 assert.equal(restarted.ready.browser_open,'disabled');checks.push('quit/restart rejects previous gateway launch ticket and new link restores persisted session without token');
 await stop(restarted.child);
 const external=await launch(['serve','--workspace',workspace,'--offline']);
 const reused=await launch(['start','--workspace',workspace,'--no-open']);assert.equal(reused.ready.owned_runtime,false);
 await page.goto(reused.ready.launch_url);await page.locator('#workspace').waitFor({state:'visible'});await stop(reused.child);
 assert.equal(external.child.exitCode,null);execFileSync(join(bin,'cayw'),['models','list','--workspace',workspace],{env,stdio:'pipe'});await stop(external.child);
 checks.push('reuses independently owned runtime; browser/launcher shutdown leaves external runtime alive and queryable');
 const simultaneous=await Promise.all([launch(['start','--workspace',workspace,'--no-open']),launch(['start','--workspace',workspace,'--no-open'])]);
 assert.equal(simultaneous[0].ready.url,simultaneous[1].ready.url);
 assert(simultaneous.every(app=>app.ready.launch_url));
 for(const item of simultaneous)await stop(item.child);
 checks.push('concurrent first start waits for readiness and both commands obtain usable entry links for one gateway');
 assert.deepEqual(errors,[]);
 await writeFile(join(out,'summary.json'),JSON.stringify({passed:true,platform:process.platform,checks,errors,opener:'/usr/bin/xdg-open',browser:chromiumPath,macos:'not run'},null,2)+'\n');
 console.log(JSON.stringify({passed:true,checks}));
}finally{
 await browser?.close();
 try{process.kill(Number(await readFile(join(out,'chromium.pid'),'utf8')),'SIGTERM')}catch{}
 for(const child of children.reverse())await stop(child);
 await writeFile(join(out,'processes.stderr'),logs.map(fn=>fn()).join('\n'));
}
