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
async function stop(child){if(child.exitCode===null&&child.signalCode===null){child.kill('SIGINT');const [code]=await once(child,'exit');assert.equal(code,0);}}
try{
 const app=await launch(['--ui','web','--workspace',workspace,'--offline','--demo-authorized']);
 assert.equal(app.ready.browser_open,'opened');assert.equal(app.ready.access,'local');assert.equal(app.ready.login_required,false);
 assert(!app.ready.launch_url&&!app.ready.web_token_file);
 const devtools=await until(async()=>{try{return (await readFile(join(profile,'DevToolsActivePort'),'utf8')).trim().split('\n')}catch{return null}});
 browser=await chromium.connectOverCDP(`http://127.0.0.1:${devtools[0]}`);
 const context=browser.contexts()[0];const page=await until(async()=>context.pages().find(p=>p.url().startsWith(app.ready.url)));
 const errors=[];page.on('pageerror',e=>errors.push(e.message));
 await page.locator('#workspace').waitFor({state:'visible'});assert.equal(page.url(),app.ready.url);
 assert.equal((await context.cookies(app.ready.url)).filter(c=>c.name.startsWith('cayw_session_')).length,0);
 assert.equal(await page.locator('#web-token').count(),0);assert(await page.locator('#logout').isHidden());
 assert.equal(await page.evaluate(()=>localStorage.length+sessionStorage.length),0);
 const rpc=async(base)=>fetch(base+'api/rpc',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({method:'runtime.health',params:{}})});
 assert.equal((await rpc(app.ready.url)).status,200);
 assert.equal((await fetch(app.ready.url+'?launch=old')).status,400);
 assert.equal((await fetch(app.ready.url+'api/browser/launch',{method:'POST'})).status,404);
 const requests=[];page.on('request',r=>{if(r.url().endsWith('/api/rpc')){assert(!r.headers().authorization);requests.push(r.postDataJSON().method)}});
 await page.locator('#session-label').fill('Local without authentication');await page.locator('#create-button').click();
 await page.locator('#session-title').filter({hasText:'Local without authentication'}).waitFor();
 await page.locator('#prompt').fill('offline task');await page.locator('#send-button').click();
 await page.waitForFunction(()=>document.querySelector('#messages').textContent.includes('headless runtime demo'));
 assert.equal(await readFile(join(workspace,'cayw-demo.txt'),'utf8'),'headless runtime demo');
 await page.reload();await page.locator('#session-title').filter({hasText:'Local without authentication'}).waitFor();
 assert.equal(requests.filter(m=>m==='task.enqueue').length,1);
 checks.push('explicit optional WebUI uses real system xdg-open and Chromium; local access has no account, token, ticket or session cookie; real task and refresh do not replay');
 const second=await repeat(['--ui','web','--workspace',workspace,'--no-open']);assert.equal(second.url,app.ready.url);assert.equal(second.browser_open,'disabled');
 const count=await readFile(join(out,'opener-count'),'utf8');
 for(const overrides of [{SSH_CONNECTION:'synthetic ssh session'},{SSH_TTY:'/dev/synthetic'},{DISPLAY:'',WAYLAND_DISPLAY:''}]){
  const value=await repeat(['--ui','web','--workspace',workspace],overrides);assert(['ssh','no-desktop'].includes(value.browser_open));assert.equal(value.url,app.ready.url);
 }
 assert.equal(await readFile(join(out,'opener-count'),'utf8'),count);
 const failed=await repeat(['--ui','web','--workspace',workspace],{BROWSER:'/bin/false',PATH:'/usr/bin:/bin'});assert.equal(failed.browser_open,'failed');assert.equal((await rpc(failed.url)).status,200);
 checks.push('duplicate WebUI start reuses service; no-open, SSH and headless keep clean usable URL; opener failure preserves service');
 await stop(app.child);
 // Explicit LAN mode is exercised on loopback, without changing machine networking.
 const openLan=await launch(['--ui','web','--lan','127.0.0.1:0','--workspace',workspace,'--offline','--no-open']);
 assert.equal(openLan.ready.access,'lan');assert.equal(openLan.ready.login_required,false);assert.equal((await rpc(openLan.ready.url)).status,200);
 await page.goto(openLan.ready.url);await page.locator('#workspace').waitFor({state:'visible'});
 await page.locator('#network-notice').filter({hasText:'未配置账号'}).waitFor();assert(await page.locator('#logout').isHidden());
 await page.reload();await page.locator('#workspace').waitFor({state:'visible'});await stop(openLan.child);
 checks.push('LAN mode without account starts and permits real requests without login; non-blocking exposure notice and refresh work');
 const fixture=JSON.parse(execFileSync('python3',['scripts/validate-default-entry.py','--account-fixture',bin,join(out,'account-fixture')],{encoding:'utf8'}));
 const lan=await launch(['--ui','web','--lan','127.0.0.1:0','--account-file',fixture.account_file,'--workspace',workspace,'--offline','--no-open']);
 assert.equal(lan.ready.login_required,true);assert.equal((await rpc(lan.ready.url)).status,401);
 await page.goto(lan.ready.url);await page.locator('#account-fields').waitFor({state:'visible'});assert(await page.locator('#workspace').isHidden());
 await page.locator('#username').fill('browser-operator');await page.locator('#password').fill('wrong-password');await page.locator('#login-button').click();
 await page.locator('#login-error').filter({hasText:'账号或密码错误'}).waitFor();assert.equal(await page.locator('#password').inputValue(),'');
 await page.locator('#password').fill('synthetic-password-for-isolated-tests');await page.locator('#login-button').click();await page.locator('#workspace').waitFor({state:'visible'});
 assert.equal(await page.locator('#password').inputValue(),'');assert(await page.locator('#logout').isVisible());
 const cookie=(await context.cookies(lan.ready.url)).find(c=>c.name.startsWith('cayw_session_'));assert(cookie?.httpOnly&&cookie.sameSite==='Strict');
 assert.equal(await page.evaluate(()=>localStorage.length+sessionStorage.length),0);assert(!(await page.evaluate(()=>document.cookie)).includes('cayw_session_'));
 await page.reload();await page.locator('#workspace').waitFor({state:'visible'});
 await page.locator('#logout').click();await page.locator('#account-fields').waitFor({state:'visible'});assert(await page.locator('#workspace').isHidden());
 await page.reload();await page.locator('#account-fields').waitFor({state:'visible'});assert(await page.locator('#workspace').isHidden());
 await stop(lan.child);
 checks.push('explicit optional account command uses hidden PTY password input; configured LAN denies anonymous RPC, rejects bad login, accepts password, restores on refresh and revokes on logout');
 const external=await launch(['serve','--workspace',workspace,'--offline']);
 const reused=await launch(['--ui','web','--workspace',workspace,'--no-open']);assert.equal(reused.ready.owned_runtime,false);
 await page.goto(reused.ready.url);await page.locator('#workspace').waitFor({state:'visible'});await stop(reused.child);
 assert.equal(external.child.exitCode,null);execFileSync(join(bin,'cayw'),['models','list','--workspace',workspace],{env,stdio:'pipe'});await stop(external.child);
 checks.push('optional WebUI reuses and preserves independently owned Runtime');
 const simultaneous=await Promise.all([launch(['--ui','web','--workspace',workspace,'--no-open']),launch(['--ui','web','--workspace',workspace,'--no-open'])]);
 assert.equal(simultaneous[0].ready.url,simultaneous[1].ready.url);await until(()=>simultaneous.some(item=>item.child.exitCode!==null));for(const item of simultaneous)await stop(item.child);
 checks.push('concurrent optional WebUI starts wait and share one gateway');
 assert.deepEqual(errors,[]);
 await writeFile(join(out,'summary.json'),JSON.stringify({passed:true,platform:process.platform,checks,errors,opener:'/usr/bin/xdg-open',browser:chromiumPath,macos:'not run',lan_network:'isolated loopback LAN mode'},null,2)+'\n');
 console.log(JSON.stringify({passed:true,checks}));
}catch(error){ console.error(error); throw error; }finally{
 await browser?.close();
 try{process.kill(Number(await readFile(join(out,'chromium.pid'),'utf8')),'SIGTERM')}catch{}
 for(const child of children.reverse())await stop(child);
 await writeFile(join(out,'processes.stderr'),logs.map(fn=>fn()).join('\n'));
}
