// macOS browser acceptance through the actual visual editor binary and Host SDK.
// Supply Playwright in the development host; the application does not depend on it.
import {spawn,spawnSync,execFileSync} from 'node:child_process';
import {createHash} from 'node:crypto';
import {mkdir, readFile, writeFile, cp} from 'node:fs/promises';
import {resolve, dirname} from 'node:path';
import {fileURLToPath} from 'node:url';
import {once} from 'node:events';
import {release} from 'node:os';

if (process.platform !== 'darwin') throw new Error('This acceptance run is scoped to macOS.');
const {chromium} = await import(process.env.CAYW_PLAYWRIGHT_MODULE || 'playwright');
const binary = resolve(process.env.CAYW_EDITOR_BINARY || 'target/debug/cayw-visual-editor');
const site = resolve(process.env.CAYW_EDITOR_VALIDATION_SITE || 'site');
const agency = process.env.CAYW_EDITOR_VALIDATION_AGENCY;
const output = resolve(process.env.CAYW_EDITOR_VALIDATION_OUT || '.local/issue30-browser-evidence');
const token = process.env.CAYW_EDITOR_TOKEN || 'cayw-issue30-local-validation-token-20261023';
if (!/^[\x21-\x7e]{32,}$/.test(token)) throw new Error('The local editor token must contain at least 32 visible ASCII characters.');
await mkdir(output, {recursive:false});
const here = dirname(fileURLToPath(import.meta.url));
const load = async name => new Function(`return (${await readFile(resolve(here,name),'utf8')});`)();
const validate = await load('validate-visual-editor-browser.js');
const extended = await load('validate-visual-editor-extended.js');
const sha = bytes => createHash('sha256').update(bytes).digest('hex');
const digest = async path => sha(await readFile(path));
const assert = (condition,message) => {if(!condition)throw new Error(message);};
const record = async (name,value) => writeFile(resolve(output,`${name}.json`),`${JSON.stringify(value,null,2)}\n`,{flag:'wx'});
const live = new Set();
const receiptPaths=['Cargo.toml','Cargo.lock','crates/cayw-visual-editor/Cargo.toml','crates/cayw-visual-editor/src/main.rs','crates/cayw-visual-editor/src/lib.rs','crates/cayw-visual-editor/src/preview.rs','crates/cayw-visual-editor/src/project.rs','crates/cayw-visual-editor/src/audit.rs','crates/cayw-visual-editor/assets/index.html','crates/cayw-visual-editor/assets/editor.js','crates/cayw-visual-editor/assets/editor.css','crates/cayw-visual-editor/assets/bridge.js','crates/cayw-visual-editor/tests/project.rs','crates/cayw-visual-editor/tests/server.rs','scripts/validate-visual-editor.mjs','scripts/validate-visual-editor-browser.js','scripts/validate-visual-editor-extended.js'];
const sourceReceipt=Object.fromEntries(await Promise.all(receiptPaths.map(async path=>[path,await digest(path)])));
await record('source-receipt',sourceReceipt);
const summary={started_utc:new Date().toISOString(),base_commit:execFileSync('git',['rev-parse','HEAD'],{encoding:'utf8'}).trim(),platform:process.platform,architecture:process.arch,os_release:release(),node:process.version,binary_sha256:await digest(binary),cases:[],passed:false};

async function start(project,state,name) {
  const argv=['--project',project,'--page','index.html','--state-dir',state,'--addr','127.0.0.1:0','--preview-addr','127.0.0.1:0'];
  const child=spawn(binary,argv,{env:{...process.env,CAYW_EDITOR_TOKEN:token},stdio:['ignore','pipe','pipe']});
  const stderr=[];
  let stdout='';
  child.stderr.on('data',chunk=>stderr.push(chunk));
  let urls;
  const ready=new Promise((accept,reject)=>{
    child.on('error',reject);
    child.on('exit',code=>{if(!urls)reject(new Error(`${name} exited before ready: ${code}; ${Buffer.concat(stderr)}`));});
    child.stdout.on('data',chunk=>{
      stdout+=chunk;
      if(!urls&&stdout.includes('\n')) {
        try {urls=JSON.parse(stdout.split('\n')[0]);accept(urls);} catch(error){reject(error);}
      }
    });
  });
  const run={child,name,argv,stdout:()=>stdout,stderr:()=>Buffer.concat(stderr).toString(),finished:false};
  live.add(run);
  const watchdog=setTimeout(()=>{child.kill('SIGKILL');},15000);
  try {
    await ready;
    assert(/^http:\/\/127\.0\.0\.1:\d+\/$/.test(urls.editor_url),'Invalid editor URL');
    run.urls=urls;
    await record(`${name}-start`,{binary,argv,pid:child.pid,urls});
    return run;
  } finally {clearTimeout(watchdog);}
}
async function stop(run) {
  if(run.finished)return;
  let status;
  if(run.child.exitCode===null&&run.child.signalCode===null) {
    const ended=once(run.child,'exit');
    run.child.kill('SIGINT');
    const watchdog=setTimeout(()=>run.child.kill('SIGKILL'),10000);
    try {status=await ended;} finally {clearTimeout(watchdog);}
  } else status=[run.child.exitCode,run.child.signalCode];
  run.finished=true;
  live.delete(run);
  await writeFile(resolve(output,`${run.name}-stdout.log`),run.stdout(),{flag:'wx'});
  await writeFile(resolve(output,`${run.name}-stderr.log`),run.stderr(),{flag:'wx'});
  await record(`${run.name}-stop`,{exit_code:status[0],signal:status[1]});
  assert(status[0]===0,`${run.name} did not stop cleanly: ${JSON.stringify(status)}`);
}
async function check(name,result) {
  await record(name,result);
  summary.cases.push({name,passed:result.passed,checks:result.checks,error:result.error});
  assert(result.passed,`${name}: ${result.error}`);
}
async function audit(state,name) {
  const raw=await readFile(resolve(state,'calls.jsonl'),'utf8');
  const rows=raw.trim().split('\n').map(line=>JSON.parse(line));
  const pending=new Set();
  for(const row of rows) {
    if(row.event==='started')pending.add(row.call_id);
    else if(row.event==='settled'||row.event==='dropped')pending.delete(row.call_id);
    assert(!row.outcome_unknown,'Acceptance retained an unknown Host call');
  }
  assert(pending.size===0,'Acceptance left an unsettled Host call');
  assert(rows.some(row=>row.capability==='fs.write'),'No actual Host write was recorded');
  const serverRaw=await readFile(resolve(state,'server-events.jsonl'),'utf8');
  const serverRows=serverRaw.trim().split('\n').map(line=>JSON.parse(line));
  const serverPending=new Set();
  for(const row of serverRows) {
    if(row.event==='started') {
      assert(!serverPending.has(row.request),'Repeated pending server request');
      serverPending.add(row.request);
    } else if(row.event==='settled') {
      assert(serverPending.delete(row.request),'Server result has no paired start');
    }
    assert(!row.outcome_unknown,'Acceptance retained an unknown editor request');
  }
  assert(serverPending.size===0,'Acceptance left an unsettled editor request');
  assert(!raw.includes(token)&&!serverRaw.includes(token),'Audit retained an editor credential');
  await record(name,{rows:rows.length,pending:pending.size,sha256:sha(raw),capabilities:[...new Set(rows.map(row=>row.capability).filter(Boolean))],server_rows:serverRows.length,server_pending:serverPending.size,server_sha256:sha(serverRaw)});
}

const browser=await chromium.launch(process.env.CAYW_BROWSER_EXECUTABLE?{executablePath:process.env.CAYW_BROWSER_EXECUTABLE}:{});
summary.browser_version=browser.version();
try {
  const context=await browser.newContext({viewport:{width:1600,height:1100},reducedMotion:'reduce'});
  const page=await context.newPage();
  const project=resolve(output,'site-project');
  const state=resolve(output,'site-state');
  await cp(site,project,{recursive:true,errorOnExist:true,force:false});
  const before=await readFile(resolve(project,'index.html'),'utf8');
  const originalAssets={css:await digest(resolve(project,'styles.css')),js:await digest(resolve(project,'script.js'))};
  let service=await start(project,state,'site-first');
  await check('site-edit',await validate(page,{url:service.urls.editor_url,token,evidence:resolve(output,'site-browser')}));
  const saved=await readFile(resolve(project,'index.html'),'utf8');
  assert(saved!==before&&!saved.includes('data-cayw-node')&&!saved.includes('data-cayw-readonly')&&!saved.includes('data-cayw-reason')&&!saved.includes('__cayw_bridge'),'Save wrote no source change or persisted instrumentation');
  assert(originalAssets.css===await digest(resolve(project,'styles.css'))&&originalAssets.js===await digest(resolve(project,'script.js')),'Editing changed original CSS or JavaScript');
  const response=await fetch(service.urls.preview_url);
  assert(await response.text()===saved,'Ordinary preview does not serve saved source');
  await record('site-source',{before_sha256:sha(before),saved_sha256:sha(saved),assets:originalAssets,plain_preview_matches_disk:true});
  await writeFile(resolve(output,'site-before.html'),before,{flag:'wx'});
  await writeFile(resolve(output,'site-saved.html'),saved,{flag:'wx'});
  const sourceDiff=spawnSync('git',['diff','--no-index','--no-ext-diff','--','site-before.html','site-saved.html'],{cwd:output,encoding:'utf8'});
  assert(sourceDiff.status===1,'Expected a concrete saved source diff');
  await writeFile(resolve(output,'site-source.diff'),sourceDiff.stdout,{flag:'wx'});
  await stop(service);
  service=await start(project,state,'site-restart');
  await check('site-restart',await extended(page,{url:service.urls.editor_url,token,mode:'restart',evidence:resolve(output,'restart-browser')}));
  await check('conflict-draft',await extended(page,{url:service.urls.editor_url,token,mode:'conflict-draft',evidence:resolve(output,'conflict-browser')}));
  const external=(await readFile(resolve(project,'index.html'),'utf8')).replace('</body>','<p id="external-change-proof">外部编辑必须保留</p></body>');
  await writeFile(resolve(project,'index.html'),external);
  await check('conflict-save',await extended(page,{url:service.urls.editor_url,token,mode:'conflict-save',evidence:resolve(output,'conflict-browser')}));
  const afterConflict=await readFile(resolve(project,'index.html'),'utf8');
  assert(afterConflict.includes('external-change-proof')&&!afterConflict.includes('旧映射草稿不得覆盖新源码'),'Conflict handling lost external source');
  await record('conflict-source',{external_sha256:sha(external),after_reloaded_save_sha256:sha(afterConflict),external_content_retained:true});
  await stop(service);
  await audit(state,'site-host-audit');

  if(agency) {
    const imageProject=resolve(output,'agency-project');
    const imageState=resolve(output,'agency-state');
    await cp(resolve(agency),imageProject,{recursive:true,errorOnExist:true,force:false});
    const imageBefore=await readFile(resolve(imageProject,'index.html'),'utf8');
    service=await start(imageProject,imageState,'agency');
    await check('agency-image',await extended(page,{url:service.urls.editor_url,token,mode:'agency',evidence:resolve(output,'agency-browser')}));
    const imageSaved=await readFile(resolve(imageProject,'index.html'),'utf8');
    assert(imageBefore!==imageSaved&&!imageSaved.includes('data-cayw-node'),'Existing image source was not saved cleanly');
    await record('agency-source',{before_sha256:sha(imageBefore),saved_sha256:sha(imageSaved)});
    await stop(service);
    await audit(imageState,'agency-host-audit');
  } else summary.agency='not run; set CAYW_EDITOR_VALIDATION_AGENCY to an unregistered Agency dist directory';

  const fixture=resolve(output,'dynamic-project');
  const fixtureState=resolve(output,'dynamic-state');
  await mkdir(fixture);
  const dynamicHtml=`<!doctype html><html lang="zh-CN"><head><meta charset="utf-8"><title>Dynamic acceptance fixture</title><style>body{font:16px system-ui;margin:30px}section{padding:16px;border:1px solid #aaa;margin:10px 0}.position-area{position:relative;height:220px}#absolute{position:absolute;left:40px;top:40px;transform:translateX(2px);padding:12px;background:#def}canvas,iframe{width:200px;height:60px}</style></head><body><h1 id="static-text">可编辑的静态区域</h1><section id="dynamic-parent"></section><section><div id="shadow-host"></div></section><section><canvas id="drawing"></canvas><iframe id="embedded" srcdoc="<p>独立子页面</p>"></iframe></section><section id="clones"><p id="clone-target">克隆映射</p></section><section class="position-area"><div id="absolute">已有绝对定位元素</div></section><section><div id="content-box" style="box-sizing:content-box;width:220px;height:60px;padding:16px;border:1px solid #000;background:rgba(0,0,0,.5);color:rgb(255,0,0);font-family:Georgia">带边距和半透明背景的现有容器</div><p id="template">{{ title }}</p></section><script>document.querySelector('#dynamic-parent').innerHTML='<p id="generated">脚本生成内容</p>';document.querySelector('#shadow-host').attachShadow({mode:'open'}).innerHTML='<p>Shadow 内容</p>';document.querySelector('#drawing').getContext('2d').fillText('Canvas 内容',10,20);const clone=document.querySelector('#clone-target').cloneNode(true);clone.removeAttribute('id');document.querySelector('#clones').appendChild(clone);</script></body></html>`;
  await writeFile(resolve(fixture,'index.html'),dynamicHtml,{flag:'wx'});
  service=await start(fixture,fixtureState,'dynamic');
  await check('dynamic-and-position',await extended(page,{url:service.urls.editor_url,token,mode:'dynamic',evidence:resolve(output,'dynamic-browser')}));
  await stop(service);
  await audit(fixtureState,'dynamic-host-audit');
  summary.passed=true;
} catch(error) {
  summary.error=error.message;
  process.exitCode=1;
} finally {
  for(const service of live)await stop(service).catch(error=>{summary.cleanup_error=error.message;summary.passed=false;process.exitCode=1;});
  await browser.close();
  const sourceAfter=Object.fromEntries(await Promise.all(receiptPaths.map(async path=>[path,await digest(path)])));
  const changed=receiptPaths.filter(path=>sourceAfter[path]!==sourceReceipt[path]);
  summary.source_changed_during_run=changed;
  if(changed.length||summary.binary_sha256!==await digest(binary)) {
    summary.passed=false;
    summary.error='Source or binary changed during this acceptance run';
    process.exitCode=1;
  }
  summary.finished_utc=new Date().toISOString();
  await record('summary',summary);
  console.log(JSON.stringify(summary,null,2));
}
