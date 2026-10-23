// Repeatable loopback fault injection around the actual Runtime + gateway.
// node runtime-browser.mjs RUNTIME_BINARY WEBUI_BINARY NEW_EVIDENCE_DIRECTORY CORE_FIXTURE_BINARY [--approval-gated]
// --approval-gated leaves the real offline task waiting for permission so Stop
// can be tested before delivering any attempt facts to the browser.
// Uses the explicit offline provider; it never contacts a live model.
import { spawn, execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { createServer, request } from 'node:http';
import { once } from 'node:events';
import { mkdir, writeFile, appendFile, readFile, rm } from 'node:fs/promises';
import { createHash, randomBytes } from 'node:crypto';
import { join } from 'node:path';

const [runtimeBinary, webBinary, evidence, coreFixture, option] = process.argv.slice(2);
if (option && option !== '--approval-gated') throw new Error('Unknown fixture option');
const approvalGated = option === '--approval-gated';
if (!runtimeBinary || !webBinary || !evidence) throw new Error('Provide Runtime binary, WebUI binary and a NEW evidence directory');
await mkdir(evidence); // Refuse to overwrite earlier evidence.
const workspace = join(evidence, 'workspace');
await mkdir(workspace);
const ipcToken = randomBytes(24).toString('hex');
const webToken = randomBytes(24).toString('hex');
const processes = [];
let mode = 'normal';
let held = null;
let delayedSnapshot = null;
let lastPromptResult = null;
let observed = {};
let withheldTerminal = null;
const trace = async entry => appendFile(join(evidence, 'http-trace.jsonl'), JSON.stringify({ at:new Date().toISOString(), ...entry }) + '\n');

function launch(binary, args, env, pattern, log) {
  const child = spawn(binary, args, { env:{...process.env, ...env}, stdio:['ignore','pipe','pipe'] });
  processes.push(child);
  return new Promise((resolve, reject) => {
    let buffer = '';
    child.on('error', reject);
    child.on('exit', code => reject(new Error(`${binary} exited with ${code}`)));
    child.stderr.on('data', chunk => appendFile(join(evidence, log), chunk));
    child.stdout.on('data', chunk => {
      appendFile(join(evidence, log), chunk);
      buffer += chunk;
      const match = buffer.match(pattern);
      if (match) resolve(match[1]);
    });
  });
}

const ipcAddress = await launch(runtimeBinary, ['serve','--db',join(evidence, 'runtime.sqlite'),'--workspace',workspace,'--listen','127.0.0.1:0','--token-env','CAYW_WEBUI_TEST_IPC','--offline',...(approvalGated ? [] : ['--demo-authorized'])], {CAYW_WEBUI_TEST_IPC:ipcToken}, /"listening":"([^"]+)"/, 'runtime.log');
const webAddress = await launch(webBinary, [], {CAYW_IPC_ADDR:ipcAddress,CAYW_IPC_TOKEN:ipcToken,CAYW_WEB_TOKEN:webToken,CAYW_WEB_ADDR:'127.0.0.1:0'}, /http:\/\/([^/\s]+)/, 'gateway.log');

async function rpc(method, params={}) {
  const response = await fetch(`http://${webAddress}/api/rpc`, {method:'POST',headers:{'Content-Type':'application/json',Authorization:`Bearer ${webToken}`},body:JSON.stringify({method,params})});
  const body = await response.json();
  await trace({client:'independent',method,params,status:response.status,body});
  if (body.error) throw new Error(JSON.stringify(body.error));
  return body.result;
}

const proxy = createServer(async (req, res) => {
  try {
    if (req.url === '/__test__/login') {
      res.writeHead(200,{'Content-Type':'application/json'});
      res.end(JSON.stringify({token:webToken})); return;
    }
    if (req.url === '/__test__/manifest') {
      res.writeHead(200,{'Content-Type':'application/json'});
      res.end(JSON.stringify(manifest)); return;
    }
    if (req.url === '/favicon.ico') { res.writeHead(204); res.end(); return; }
    if (req.url === '/__test__/evidence' && req.method === 'POST') {
      let body=''; for await(const chunk of req) body+=chunk;
      const report=JSON.parse(body);
      if (!/^[a-z-]+$/.test(report.name)) throw new Error('Invalid evidence name');
      await writeFile(join(evidence,`${report.name}.json`),JSON.stringify(report.data,null,2),{flag:'wx'});
      res.end('{}'); return;
    }
    if (req.url === '/__test__/status') {
      res.writeHead(200,{'Content-Type':'application/json'});
      res.end(JSON.stringify({mode,held:Boolean(held),lastPromptResult,observed})); return;
    }
    if (req.url === '/__test__/release' && req.method === 'POST') {
      if (!held) throw new Error('No delayed prompt receipt');
      held.response.writeHead(held.status,held.headers); held.response.end(held.body); held=null;
      if (mode !== 'early-cancel') mode='normal';
      res.end('{}'); return;
    }
    if (req.url === '/__test__/deliver-terminal' && req.method === 'POST') {
      if (mode !== 'early-cancel') throw new Error('No early cancel case');
      observed.terminalReleased=true;
      mode='normal';
      res.end('{}'); return;
    }
    if (req.url === '/__test__/advance-clients' && req.method === 'POST') {
      let raw = ''; for await (const chunk of req) raw += chunk;
      const {count} = JSON.parse(raw);
      if (!Number.isSafeInteger(count) || count < 1 || count > 10 || mode !== 'normal' || !held) throw new Error('Expected held receipt and at most 10 clients per batch after releasing terminal');
      const session = observed.session;
      if (!observed.eviction) {
        const original = JSON.parse(held.body).result;
        const originalEvents = await rpc('session.events',{session_id:session, after:original.cursor - 1, limit:1});
        observed.eviction = {requested:0, completed:0, originalReceipt:original, originalTerminal:originalEvents[0], terminals:[]};
      }
      observed.eviction.requested += count;
      for (let i = 0; i < count; i++) {
        const pending = rpc('session.prompt',{session_id:session,text:`Independent client ${i}: finish a different offline prompt`}).then(result => ({result}), error => ({error:String(error)}));
        let completed = false;
        pending.then(() => {completed = true;});
        const deadline = Date.now() + 15000;
        while (!completed && Date.now() < deadline) {
          const current = await rpc('session.get',{session_id:session});
          for (const approval of current.pending_approvals ?? []) await rpc('approval.decide',{session_id:session,approval_id:approval.id,decision:'deny'});
          if (!completed) await new Promise(resolve => setTimeout(resolve, 10));
        }
        if (!completed) throw new Error(`Offline client ${i} did not terminate`);
        const completion = await pending;
        if (completion.error) throw new Error(completion.error);
        const terminal = (await rpc('session.events',{session_id:session, after:completion.result.cursor - 1, limit:1}))[0];
        if (!['attempt_finished','attempt_recovered'].includes(terminal?.event?.type)) throw new Error('Missing independent durable terminal');
        observed.eviction.terminals.push(terminal);
        observed.eviction.completed++;
      }
      res.writeHead(200,{'Content-Type':'application/json'});res.end(JSON.stringify(observed.eviction));return;
    }
    if (req.url === '/__test__/other-client' && req.method === 'POST') {
      const result=await rpc('session.prompt',{session_id:observed.session,text:'Independent client: finish a different prompt'});
      res.writeHead(200,{'Content-Type':'application/json'}); res.end(JSON.stringify(result)); return;
    }
    if (req.url === '/__test__/reconcile' && req.method === 'POST') {
      if(!coreFixture) throw new Error('Provide the compiled Core SDK fixture');
      delayedSnapshot=await rpc('session.get',{session_id:observed.session});
      const {stdout}=await promisify(execFile)(coreFixture,['reconcile',join(evidence,'runtime.sqlite'),observed.session]);
      const result=JSON.parse(stdout);
      await trace({client:'host-sdk',operation:'reconcile',result});
      res.writeHead(200,{'Content-Type':'application/json'});res.end(stdout);return;
    }
    if (req.url?.startsWith('/__test__/session/') && req.method === 'POST') {
      mode=req.url.split('/').at(-1); observed={}; lastPromptResult=null; withheldTerminal=null;
      if (mode === 'early-cancel' && !approvalGated) throw new Error('Early cancel requires --approval-gated');
      // Each synthetic demo starts without the previous case's output. The
      // Host refuses an overwrite without the expected digest by design.
      await rm(join(workspace,'cayw-demo.txt'),{force:true});
      let result=await rpc('session.create',{label:`Regression ${mode}`}); observed.session=result.id;
      if (mode === 'early-cancel') observed.initialSnapshot=result;
      if(mode==='reconciliation') {
        if(!coreFixture) throw new Error('Provide the compiled Core SDK fixture');
        const {stdout}=await promisify(execFile)(coreFixture,['unknown',join(evidence,'runtime.sqlite'),result.id]);
        result=JSON.parse(stdout);
        await trace({client:'host-sdk',operation:'create-unknown-call',result});
      }
      res.writeHead(200,{'Content-Type':'application/json'}); res.end(JSON.stringify(result)); return;
    }
    let body=''; for await(const chunk of req) body+=chunk;
    const call=req.url==='/api/rpc'?JSON.parse(body):null;
    if (call?.method==='task.enqueue' && mode==='drop') {
      observed.dropped=(observed.dropped??0)+1;
      await trace({client:'browser',method:call.method,params:call.params,forwarded:false,fault:'drop before upstream dispatch'});
      // Send a partial response first so Chromium cannot transparently replay
      // the POST on an idle keep-alive connection reset.
      res.writeHead(200,{'Content-Type':'application/json','Content-Length':'1000'});
      res.flushHeaders(); res.write('{'); setTimeout(()=>res.destroy(),20); return;
    }
    const headers={...req.headers,host:webAddress};
    if(headers.origin) headers.origin=`http://${webAddress}`;
    delete headers.connection;
    const upstream=request(`http://${webAddress}${req.url}`,{method:req.method,headers}, response => {
      const chunks=[];
      response.on('data', chunk=>chunks.push(chunk));
      response.on('end', async()=>{
        const data=Buffer.concat(chunks);
        const result=call?JSON.parse(data.toString()):null;
        if(call) await trace({client:'browser',method:call.method,params:call.params,status:response.statusCode,body:result});
        if(call?.method==='task.resume') lastPromptResult=result;
        if(call?.method==='task.enqueue') observed.admitted=result;
        if(call?.method==='task.get') observed.originalTask=result;
        if(call?.method==='session.cancel') {
          const task=observed.originalTask?.result;
          if(!task || call.params.session_id!==task.session_id || call.params.task_id!==task.id
              || call.params.worker_id!==task.worker_id || call.params.attempt_id!==task.attempt_id
              || call.params.fence!==task.fence) throw new Error('Browser cancellation differs from its original Task target');
          observed.cancelTarget=call.params;
          observed.cancel=result;
        }
        if(call?.method==='session.events') observed.lastEvents=result;
        if(call?.method==='session.get') observed.lastSnapshot=result;
        if(call?.method==='session.get' && mode==='early-cancel' && !observed.cancel?.result?.cancelled && !lastPromptResult) {
          await trace({client:'fault-injection',operation:'withhold-running-projection',cursor:result.result?.cursor});
          res.writeHead(200,{'Content-Type':'application/json'});res.end(JSON.stringify({result:observed.initialSnapshot}));return;
        }
        if(call?.method==='session.events' && mode==='early-cancel') {
          const records=result.result;
          if(!Array.isArray(records)) throw new Error('Runtime returned invalid events');
          if(!observed.cancel?.result?.cancelled) {
            await trace({client:'fault-injection',operation:'withhold-events-before-stop',cursors:records.map(record=>record.cursor)});
            res.writeHead(200,{'Content-Type':'application/json'});res.end(JSON.stringify({result:[]}));return;
          }
          const terminal=records.find(record=>['attempt_finished','attempt_recovered'].includes(record.event?.type));
          if(terminal) withheldTerminal=terminal;
          const delivered=withheldTerminal?records.filter(record=>record.cursor<withheldTerminal.cursor):records;
          observed.deliveredEvents={result:delivered};
          observed.withheldTerminal=withheldTerminal;
          await trace({client:'fault-injection',operation:'withhold-exact-terminal',terminal:withheldTerminal,delivered:delivered.map(record=>record.cursor)});
          res.writeHead(200,{'Content-Type':'application/json'});res.end(JSON.stringify({result:delivered}));return;
        }
        if(call?.method==='task.resume' && ['hold','early-cancel'].includes(mode)) {
          held={response:res,status:response.statusCode,headers:response.headers,body:data}; return;
        }
        if(call?.method==='session.get' && delayedSnapshot) {
          observed.delayedSnapshot=delayedSnapshot;
          await trace({client:'fault-injection',operation:'old-snapshot-after-new-events',result:delayedSnapshot});
          res.writeHead(200,{'Content-Type':'application/json'}); res.end(JSON.stringify({result:delayedSnapshot})); delayedSnapshot=null; return;
        }
        res.writeHead(response.statusCode,response.headers); res.end(data);
      });
    });
    upstream.on('error',error=>{res.writeHead(502,{'Content-Type':'application/json'});res.end(JSON.stringify({error:{code:'test_proxy_failure',message:error.message,outcome_unknown:true}}));});
    upstream.end(body);
  } catch(error) { res.writeHead(500,{'Content-Type':'text/plain'});res.end(error.stack); }
});
proxy.listen(0,'127.0.0.1');
await once(proxy,'listening');
const url=`http://127.0.0.1:${proxy.address().port}`;
const manifest={url,ipcAddress,webAddress,workspace,evidence,offline:true,approvalGated,binaries:{}};
for(const [name,path] of [['runtime',runtimeBinary],['webui',webBinary]]) manifest.binaries[name]={path,sha256:createHash('sha256').update(await readFile(path)).digest('hex')};
await writeFile(join(evidence,'manifest.json'),JSON.stringify(manifest,null,2));
console.log(JSON.stringify(manifest));
async function stop(){proxy.closeAllConnections();proxy.close();for(const child of processes) child.kill('SIGINT');}
process.once('SIGTERM',stop);process.once('SIGINT',stop);
process.once('exit',()=>{for(const child of processes)child.kill('SIGINT');});
