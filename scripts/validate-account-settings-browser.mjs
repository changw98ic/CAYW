// Real browser -> paired local settings / LAN gateway -> real offline Runtime.
// Uses only temporary state and this host's own private interface.
// node scripts/validate-account-settings-browser.mjs BIN_DIR NEW_EVIDENCE_DIR
import assert from 'node:assert/strict';
import {spawn,execFileSync} from 'node:child_process';
import {mkdir,readFile,writeFile,stat,rmdir,unlink} from 'node:fs/promises';
import {join,resolve} from 'node:path';
import {networkInterfaces} from 'node:os';
import {request as httpRequest} from 'node:http';
import {createInterface} from 'node:readline';
import {once} from 'node:events';
const bin=resolve(process.argv[2]),out=resolve(process.argv[3]);await mkdir(out);
const home=join(out,'home'),workspace=join(out,'workspace'),accountDir=join(out,'accounts'),accountFile=join(accountDir,'operator.json');
await mkdir(home,{mode:0o700});await mkdir(workspace);await mkdir(accountDir,{mode:0o700});
const address=Object.values(networkInterfaces()).flat().find(x=>x.family==='IPv4'&&!x.internal&&/^(10\.|172\.(1[6-9]|2\d|3[01])\.|192\.168\.)/.test(x.address))?.address;
assert(address,'isolated private interface is required for real non-loopback peer checks');
const env={PATH:process.env.PATH,HOME:home,CAYW_HOME:join(home,'cayw')};
const {chromium}=await import(process.env.CAYW_PLAYWRIGHT_MODULE||'playwright');
let browser;const diagnostics=[];const children=[],logs=[],checks=[];const firstPassword='synthetic-account-ui-one',secondPassword='synthetic-account-ui-two';
async function launch(args){
 const child=spawn(join(bin,'cayw'),args,{env,stdio:['ignore','pipe','pipe'],detached:true});children.push(child);
 let stderr='';child.stderr.on('data',b=>stderr+=b);logs.push(()=>stderr);
 const lines=createInterface({input:child.stdout});
 const [line]=await Promise.race([once(lines,'line'),once(child,'exit').then(([code])=>{throw new Error(`startup exited ${code}: ${stderr}`)}),new Promise((_,reject)=>setTimeout(()=>reject(new Error('startup timeout')),30000).unref())]);
 return {child,ready:JSON.parse(line)};
}
async function stop(child){if(child.exitCode===null&&child.signalCode===null){child.kill('SIGINT');const [code]=await once(child,'exit');assert.equal(code,0);}}
const rpc=(base)=>new Promise((resolve,reject)=>{
 const req=httpRequest(new URL('api/rpc',base),{method:'POST',headers:{'Content-Type':'application/json'}},res=>{res.resume();res.on('end',()=>resolve({status:res.statusCode}));});
 req.setTimeout(10000,()=>req.destroy(new Error(`runtime.health timed out for ${base}`)));req.on('error',reject);req.end(JSON.stringify({method:'runtime.health',params:{}}));
});
async function raw(base,headers,data){
 return new Promise((resolve,reject)=>{
  const req=httpRequest(new URL('api/settings/account',base),{method:'POST',headers:{'Content-Type':'application/json',...headers}},res=>{let text='';res.on('data',b=>text+=b);res.on('end',()=>resolve({status:res.statusCode,body:JSON.parse(text),source:req.socket.localAddress}));});req.on('error',reject);req.end(JSON.stringify(data));
 });
}
async function pageHasNoAccountForm(page){
 for(const id of ['open-settings','settings-dialog','account-settings-form','save-account','account-username','account-password','account-confirm','account-settings-status','close-settings']){
  assert.equal(await page.locator(`#${id}`).count(),0,`${id} is not on the conversation page`);
 }
}
const payload=(password,revision=null)=>({username:'browser-operator',password,confirm_password:password,expected_revision:revision});
try{
 const app=await launch(['--ui','web','--lan',`${address}:0`,'--account-file',accountFile,'--workspace',workspace,'--offline','--no-open']);
 assert.equal(app.ready.login_required,false);assert.equal((await rpc(app.ready.url)).status,200);assert.notEqual(app.ready.local_url,app.ready.url);
 browser=await chromium.launch({headless:true,...(process.env.HTTP_PROXY?{proxy:{server:process.env.HTTP_PROXY,bypass:`localhost,127.0.0.1,${address}`}}:{}),...(process.env.CAYW_CHROMIUM?{executablePath:process.env.CAYW_CHROMIUM}:{})});
 const localContext=await browser.newContext(),remoteContext=await browser.newContext();
 const admin=await localContext.newPage(),remote=await remoteContext.newPage();const errors=[];
 for(const page of [admin,remote]){page.on('pageerror',e=>errors.push(e.message));page.on('requestfailed',r=>diagnostics.push({url:r.url(),failure:r.failure()?.errorText}));page.on('response',r=>{if(r.status()>=400)diagnostics.push({url:r.url(),status:r.status()});});}
 await remote.goto(app.ready.url);await remote.locator('#workspace').waitFor({state:'visible',timeout:10000});await pageHasNoAccountForm(remote);
 await remote.locator('#network-notice').filter({hasText:'cayw account'}).waitFor();
 const denied=await raw(app.ready.url,{},payload(firstPassword));assert.equal(denied.status,403);assert(!denied.source.startsWith('127.'));
 const forged=await raw(app.ready.url,{Host:new URL(app.ready.local_url).host,Origin:new URL(app.ready.local_url).origin,'X-Forwarded-For':'127.0.0.1',Forwarded:'for=127.0.0.1'},payload(firstPassword));assert.equal(forged.status,403);
 await assert.rejects(readFile(accountFile));checks.push('unconfigured LAN still opens; actual non-loopback browser and forged Host/forwarded headers cannot claim local account management');
 await admin.goto(app.ready.local_url);await admin.locator('#workspace').waitFor({state:'visible'});assert(await admin.locator('#logout').isHidden());await pageHasNoAccountForm(admin);
 const unconfigured=await (await localContext.request.get(app.ready.local_url+'api/settings/account')).json();
 assert.equal(unconfigured.can_manage,true);assert.equal(unconfigured.configured,false);assert.match(unconfigured.message,/未配置/);
 const mismatched=await raw(app.ready.local_url,{},{username:'browser-operator',password:firstPassword,confirm_password:'different-confirmation',expected_revision:null});
 assert.equal(mismatched.status,400);assert.equal(mismatched.body.error.code,'invalid_params');assert.match(mismatched.body.error.message,/两次密码不一致/);
 await assert.rejects(readFile(accountFile));
 // A real filesystem failure confined to the separate temporary account folder.
 await rmdir(accountDir);await writeFile(accountDir,'temporary failure fixture');
 const failedSave=await raw(app.ready.local_url,{},payload(firstPassword));
 assert.equal(failedSave.status,500);assert.equal(failedSave.body.error.code,'account_save_failed');
 assert.equal((await rpc(app.ready.url)).status,200);await assert.rejects(readFile(accountFile));
 await unlink(accountDir);await mkdir(accountDir,{mode:0o700});
 const restored=await (await localContext.request.get(app.ready.local_url+'api/settings/account')).json();
 assert.equal(restored.configured,false);assert.match(restored.message,/未配置/);
 checks.push('conversation page has no account form; mismatched passwords are rejected and not saved; real save failure is explicit and leaves LAN unconfigured/open');
 const saved=await raw(app.ready.local_url,{},payload(firstPassword));assert.equal(saved.status,200);assert.equal(saved.body.login_required,true);
 const stored=await readFile(accountFile,'utf8');assert(!stored.includes(firstPassword));assert.match(stored,/\$argon2id\$/);assert.equal((await stat(accountFile)).mode&0o777,0o600);
 const status=await (await localContext.request.get(app.ready.local_url+'api/settings/account')).json();assert(status.configured&&status.login_required&&status.can_manage);
 const duplicate=await raw(app.ready.local_url,{},payload(firstPassword));assert.equal(duplicate.status,409);assert.equal(await readFile(accountFile,'utf8'),stored);
 assert.equal((await raw(app.ready.local_url,{Origin:'https://evil.invalid'},payload(firstPassword,status.revision))).status,403);
 assert.equal((await raw(app.ready.local_url,{Host:'evil.invalid'},payload(firstPassword,status.revision))).status,403);
 assert.equal((await rpc(app.ready.url)).status,401);assert.equal((await rpc(app.ready.local_url)).status,200);assert(!JSON.stringify(status).includes(firstPassword));assert(!('password_hash' in status));
 await admin.reload();await admin.locator('#workspace').waitFor({state:'visible'});assert.equal(await admin.locator('#network-notice').textContent(),'');
 assert.equal(await admin.evaluate(()=>localStorage.length+sessionStorage.length),0);
 checks.push('loopback account save stores only a 0600 Argon2id digest and immediately enables LAN login; stale duplicate is 409 without overwrite; Host/Origin restrictions remain');
 await remote.locator('#refresh-sessions').click();await remote.locator('#account-fields').waitFor({state:'visible'});
 await remote.locator('#username').fill('browser-operator');await remote.locator('#password').fill('wrong-password');await remote.locator('#login-button').click();await remote.locator('#login-error').filter({hasText:'账号或密码错误'}).waitFor();
 await remote.locator('#password').fill(firstPassword);await remote.locator('#login-button').click();await remote.locator('#workspace').waitFor({state:'visible'});await remote.reload();await remote.locator('#workspace').waitFor({state:'visible'});
 await pageHasNoAccountForm(remote);
 assert.equal((await remoteContext.request.post(app.ready.url+'api/settings/account',{data:payload(secondPassword,status.revision)})).status(),403);
 const rotated=await raw(app.ready.local_url,{},payload(secondPassword,status.revision));assert.equal(rotated.status,200);assert.equal(rotated.body.login_required,true);
 assert.equal((await remoteContext.request.post(app.ready.url+'api/rpc',{data:{method:'runtime.health',params:{}}})).status(),401);
 await remote.reload();await remote.locator('#account-fields').waitFor({state:'visible'});await remote.locator('#username').fill('browser-operator');await remote.locator('#password').fill(firstPassword);await remote.locator('#login-button').click();await remote.locator('#login-error').filter({hasText:'账号或密码错误'}).waitFor();await remote.locator('#password').fill(secondPassword);await remote.locator('#login-button').click();await remote.locator('#workspace').waitFor({state:'visible'});
 await remote.locator('#logout').click();await remote.reload();await remote.locator('#account-fields').waitFor({state:'visible'});assert(await remote.locator('#workspace').isHidden());
 await admin.reload();await admin.locator('#workspace').waitFor({state:'visible'});assert(await admin.locator('#logout').isHidden());
 checks.push('configured LAN rejects bad login, accepts saved password and refreshes; remote logged-in users still cannot alter account; local password update revokes old sessions; logout and local exemption work');
 const repeat=JSON.parse(execFileSync(join(bin,'cayw'),['--ui','web','--no-open','--account-file',accountFile,'--workspace',workspace],{env,encoding:'utf8'}));assert.equal(repeat.url,app.ready.local_url);assert.equal(repeat.lan_url,app.ready.url);
 await stop(app.child);
 await assert.rejects(fetch(app.ready.local_url));await assert.rejects(fetch(app.ready.url));
 const local=await launch(['--ui','web','--account-file',accountFile,'--workspace',workspace,'--offline','--no-open']);await admin.goto(local.ready.url);await admin.locator('#workspace').waitFor({state:'visible'});await pageHasNoAccountForm(admin);const localStatus=await (await localContext.request.get(local.ready.url+'api/settings/account')).json();assert.match(localStatus.message,/下次启动局域网模式/);assert.equal((await rpc(local.ready.url)).status,200);await stop(local.child);
 const restarted=await launch(['--ui','web','--lan',`${address}:0`,'--account-file',accountFile,'--workspace',workspace,'--offline','--no-open']);assert.equal(restarted.ready.lan_login_enabled,true);assert.equal((await rpc(restarted.ready.url)).status,401);assert.equal((await rpc(restarted.ready.local_url)).status,200);await stop(restarted.child);
 checks.push('repeated local WebUI command reaches paired management; both listeners shut down; local-only mode explains future LAN activation; saved account survives restart');
 assert.deepEqual(errors,[]);assert(!logs.map(f=>f()).join('\n').includes(firstPassword));assert(!logs.map(f=>f()).join('\n').includes(secondPassword));
 await writeFile(join(out,'summary.json'),JSON.stringify({passed:true,platform:process.platform,checks,errors,peer_validation:'same isolated host private interface, real non-loopback TCP peer',macos:process.platform==='darwin'?'validated':'not run'},null,2)+'\n');console.log(JSON.stringify({passed:true,checks}));
}catch(error){console.error(JSON.stringify({diagnostics}));console.error(error);throw error;}finally{
 await browser?.close();for(const child of children.reverse())await stop(child);await writeFile(join(out,'processes.stderr'),logs.map(f=>f()).join('\n'));
}
