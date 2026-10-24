// macOS acceptance for authored or explicitly supplied Vite React/Vue projects.
import {spawn, spawnSync, execFileSync} from 'node:child_process';
import {createHash} from 'node:crypto';
import {mkdir, readFile, writeFile, cp, readdir} from 'node:fs/promises';
import {resolve, dirname, relative, sep} from 'node:path';
import {fileURLToPath} from 'node:url';
import {once} from 'node:events';
import {release} from 'node:os';

if (process.platform !== 'darwin') throw new Error('This acceptance is scoped to macOS.');
const {chromium}=await import(process.env.CAYW_PLAYWRIGHT_MODULE || 'playwright');
const binary=resolve(process.env.CAYW_EDITOR_BINARY || '.local/issue30-target/debug/cayw-visual-editor');
const output=resolve(process.env.CAYW_EDITOR_VALIDATION_OUT || '.local/issue30-validation/framework-01');
const token='cayw-issue30-framework-local-validation-20261024';
const here=dirname(fileURLToPath(import.meta.url));
const root=resolve(here,'..');
const validate=new Function(`return (${await readFile(resolve(here,'validate-visual-editor-framework-browser.js'),'utf8')});`)();
await mkdir(output,{recursive:false});
const sha=bytes=>createHash('sha256').update(bytes).digest('hex');
const digest=async path=>sha(await readFile(path));
const assert=(value,message)=>{if(!value)throw new Error(message);};
const record=(name,data)=>writeFile(resolve(output,`${name}.json`),`${JSON.stringify(data,null,2)}\n`,{flag:'wx'});
const paths=['Cargo.toml','Cargo.lock','.gitignore','crates/cayw-visual-editor/Cargo.toml',
  ...['main.rs','lib.rs','project.rs','framework.rs','companion.rs','audit.rs','preview.rs'].map(name=>`crates/cayw-visual-editor/src/${name}`),
  ...['editor.js','editor.css','bridge.js','index.html'].map(name=>`crates/cayw-visual-editor/assets/${name}`),
  ...['package.json','package-lock.json','source.mjs','source.test.mjs','companion.mjs'].map(name=>`crates/cayw-visual-editor/framework/${name}`),
  'scripts/validate-visual-editor-framework.mjs','scripts/validate-visual-editor-framework-browser.js'];
const receipt=Object.fromEntries(await Promise.all(paths.map(async path=>[path,await digest(resolve(root,path))])));
await record('source-receipt',receipt);
const summary={started_utc:new Date().toISOString(),platform:process.platform,architecture:process.arch,os_release:release(),node:process.version,
  base_commit:execFileSync('git',['rev-parse','HEAD'],{cwd:root,encoding:'utf8'}).trim(),binary_sha256:await digest(binary),real_model_requests:0,
  input_kind:process.env.CAYW_FRAMEWORK_INPUT_KIND || 'authored-synthetic-fixture',cases:[],passed:false};
const live=new Set();
let browser;

async function start(project,state,framework,name) {
  const argv=['--project',project,'--state-dir',state,'--framework',framework,'--addr','127.0.0.1:0','--preview-addr','127.0.0.1:0'];
  const child=spawn(binary,argv,{cwd:root,env:{...process.env,CAYW_EDITOR_TOKEN:token},stdio:['ignore','pipe','pipe']});
  let stdout='',stderr='',urls;
  const run={child,argv,name,finished:false,stdout:()=>stdout,stderr:()=>stderr};
  live.add(run);
  child.stderr.on('data',chunk=>{stderr+=chunk;});
  const ready=new Promise((accept,reject)=>{
    child.on('error',reject);
    child.on('exit',(code,signal)=>{if(!urls)reject(new Error(`${name} exited before ready: ${code}/${signal}; ${stderr}`));});
    child.stdout.on('data',chunk=>{
      stdout+=chunk;
      if(!urls&&stdout.includes('\n')) {
        try {urls=JSON.parse(stdout.split('\n')[0]);accept(urls);}catch(error){reject(error);}
      }
    });
  });
  const timer=setTimeout(()=>child.kill('SIGINT'),90000);
  try {await ready;}finally{clearTimeout(timer);}
  run.urls=urls;
  await record(`${name}-start`,{binary,argv,pid:child.pid,urls});
  assert(urls.normal_preview_url&&urls.normal_preview_url!==urls.preview_url,'Framework ordinary/edit previews must be separate');
  return run;
}
async function stop(run) {
  if(run.finished)return;
  let status;
  if(run.child.exitCode===null&&run.child.signalCode===null) {
    const ended=once(run.child,'exit');
    run.child.kill('SIGINT');
    const timer=setTimeout(()=>run.child.kill('SIGKILL'),15000);
    try{status=await ended;}finally{clearTimeout(timer);}
  } else status=[run.child.exitCode,run.child.signalCode];
  run.finished=true;
  live.delete(run);
  await writeFile(resolve(output,`${run.name}-stdout.log`),run.stdout(),{flag:'wx'});
  await writeFile(resolve(output,`${run.name}-stderr.log`),run.stderr(),{flag:'wx'});
  await record(`${run.name}-stop`,{exit_code:status[0],signal:status[1]});
  assert(status[0]===0,`${run.name} did not stop cleanly: ${JSON.stringify(status)}`);
  for(const url of Object.values(run.urls || {})) {
    if(typeof url!=='string'||!url.startsWith('http:'))continue;
    let serving=false;
    try{await fetch(url,{signal:AbortSignal.timeout(1000)});serving=true;}catch{}
    assert(!serving,`Listener remains after shutdown: ${url}`);
  }
}
async function build(project,name) {
  const built=spawnSync('npm',['run','build'],{cwd:project,encoding:'utf8',timeout:120000});
  await writeFile(resolve(output,`${name}.log`),`${built.stdout || ''}${built.stderr || ''}`,{flag:'wx'});
  assert(built.status===0,`${name} build failed: ${built.stderr || built.error || built.stdout}`);
  return {exit_code:built.status,log_sha256:await digest(resolve(output,`${name}.log`))};
}
async function sourceFiles(project) {
  const result={};
  const walk=async relative=>{
    const entries=await readdir(resolve(project,relative),{withFileTypes:true});
    for(const entry of entries.sort((a,b)=>a.name.localeCompare(b.name))) {
      if(entry.name.startsWith('.')||['node_modules','dist','build'].includes(entry.name))continue;
      const path=relative?`${relative}/${entry.name}`:entry.name;
      if(entry.isDirectory())await walk(path);
      else if(entry.isFile())result[path]=await digest(resolve(project,path));
    }
  };
  await walk('');
  return result;
}
async function audit(state,name) {
  const result={};
  for(const log of ['calls.jsonl','server-events.jsonl']) {
    const raw=await readFile(resolve(state,log),'utf8');
    const rows=raw.trim().split('\n').filter(Boolean).map(line=>JSON.parse(line));
    const pending=new Set();
    for(const row of rows) {
      const key=row.call_id || row.request;
      if(row.event==='started'){assert(!pending.has(key),'Duplicate audit start');pending.add(key);}
      else if(['settled','dropped'].includes(row.event)){assert(pending.delete(key),'Audit end without start');}
      assert(!row.outcome_unknown,'Unexpected unknown audit record');
    }
    assert(pending.size===0,'Unsettled audit call');
    assert(!raw.includes(token),'Audit contains editor credential');
    if(log==='calls.jsonl')assert(rows.some(row=>row.capability==='fs.write'),'No Host source write was recorded');
    result[log]={rows:rows.length,pending:pending.size,sha256:sha(raw),capabilities:[...new Set(rows.map(row=>row.capability).filter(Boolean))]};
  }
  await record(name,result);
}
async function check(name,result) {
  await record(name,result);
  summary.cases.push({name,passed:result.passed,checks:result.checks,error:result.error});
  assert(result.passed,`${name}: ${result.error}`);
}
try {
  browser=await chromium.launch(process.env.CAYW_BROWSER_EXECUTABLE?{executablePath:process.env.CAYW_BROWSER_EXECUTABLE}:{});
  summary.browser_version=browser.version();
  for(const framework of ['react','vue']) {
    const input=resolve(process.env[`CAYW_EDITOR_VALIDATION_${framework.toUpperCase()}`] || `.local/issue30-framework-inputs/${framework}-vite`);
    const project=resolve(output,`${framework}-project`), state=resolve(output,`${framework}-state`);
    await cp(input,project,{recursive:true,errorOnExist:true,force:false,filter:path=>!['dist','build'].includes(relative(input,path).split(sep)[0])});
    const before=await sourceFiles(project);
    await record(`${framework}-baseline-sources`,before);
    const originalBuild=await build(project,`${framework}-baseline-build`);
    await record(`${framework}-baseline-build-result`,originalBuild);
    const context=await browser.newContext({viewport:{width:1600,height:1100},reducedMotion:'reduce'});
    const page=await context.newPage();
    let service=await start(project,state,framework,`${framework}-first`);
    await check(`${framework}-edit`,await validate(page,{framework,url:service.urls.editor_url,normalUrl:service.urls.normal_preview_url,token,
      evidence:resolve(output,`${framework}-browser`),beforeSave:async()=>{
        const current=await sourceFiles(project);
        assert(JSON.stringify(current)===JSON.stringify(before),'Unsaved framework draft changed project files');
        await record(`${framework}-unsaved-disk`,{unchanged:true,files:current});
      }}));
    const saved=await sourceFiles(project);
    const changed=Object.keys(saved).filter(path=>saved[path]!==before[path]);
    assert(changed.length>=2&&changed.every(path=>/\.(jsx|tsx|vue)$/.test(path)),`Unexpected saved paths: ${JSON.stringify(changed)}`);
    for(const path of changed) {
      const content=await readFile(resolve(project,path),'utf8');
      assert(!/data-cayw-node|data-cayw-readonly|data-cayw-reason|__cayw_bridge/.test(content),'Framework source contains response instrumentation');
    }
    await record(`${framework}-saved-sources`,{files:saved,changed});
    await record(`${framework}-saved-build-result`,await build(project,`${framework}-saved-build`));
    await stop(service);
    service=await start(project,state,framework,`${framework}-restart`);
    await check(`${framework}-restart`,await validate(page,{framework,url:service.urls.editor_url,token,mode:'restart',evidence:resolve(output,`${framework}-restart`)}));
    await check(`${framework}-conflict-draft`,await validate(page,{framework,url:service.urls.editor_url,token,mode:'conflict-draft',evidence:resolve(output,`${framework}-conflict`)}));
    const currentDocument=await fetch(`${service.urls.editor_url}api/document`,{headers:{Authorization:`Bearer ${token}`}}).then(response=>response.json());
    const sourcePath=currentDocument.snapshot.nodes.find(node=>node.tag==='h1'&&node.editable).file;
    const original=await readFile(resolve(project,sourcePath),'utf8');
    const external=original.replace(`${framework} visual source saved 中文 😀`,'External source retained');
    assert(original!==external,'Could not construct explicit external source change');
    await writeFile(resolve(project,sourcePath),external);
    await check(`${framework}-conflict-save`,await validate(page,{framework,url:service.urls.editor_url,token,mode:'conflict-save',evidence:resolve(output,`${framework}-conflict`)}));
    const afterConflict=await readFile(resolve(project,sourcePath),'utf8');
    assert(afterConflict.includes('External source retained')&&!afterConflict.includes('Unsaved stale draft'),'External source was lost');
    await record(`${framework}-conflict-sources`,{external_sha256:sha(external),after_save_sha256:sha(afterConflict),external_retained:true});
    await stop(service);
    await record(`${framework}-final-build-result`,await build(project,`${framework}-final-build`));
    await audit(state,`${framework}-audit`);
    await context.close();
  }
  summary.passed=true;
} catch(error) {
  summary.error=error.stack || error.message;
  process.exitCode=1;
} finally {
  for(const service of live)await stop(service).catch(error=>{summary.cleanup_error=error.message;summary.passed=false;process.exitCode=1;});
  if(browser)await browser.close();
  const after=Object.fromEntries(await Promise.all(paths.map(async path=>[path,await digest(resolve(root,path))])));
  summary.source_changed_during_run=paths.filter(path=>receipt[path]!==after[path]);
  if(summary.source_changed_during_run.length||summary.binary_sha256!==await digest(binary)) {
    summary.passed=false;
    summary.error='Product, validation source or binary changed during the run';
    process.exitCode=1;
  }
  summary.finished_utc=new Date().toISOString();
  await record('summary',summary);
  console.log(JSON.stringify(summary,null,2));
}
