#!/usr/bin/env python3
"""Real CLI -> authenticated WebUI -> Runtime smoke. Synthetic local fixtures only.
Usage: python3 scripts/validate-product-workflows.py TARGET_DEBUG NEW_EVIDENCE_DIR
macOS is the product acceptance platform. Other hosts only provide development evidence.
"""
import concurrent.futures
import json
import hashlib
import os
from pathlib import Path
import select
import signal
import subprocess
import sys
import time
import urllib.error
import urllib.request

binaries = Path(sys.argv[1]).resolve()
evidence = Path(sys.argv[2]).resolve()
evidence.mkdir()
workspace = evidence / 'workspace'
workspace.mkdir()
root = evidence / 'home'
root.mkdir(mode=0o700)
env = {'PATH': os.environ['PATH'], 'HOME': str(root), 'CAYW_HOME': str(root / 'cayw'),
       'CAYW_PRODUCT_TEST_KEY': 'synthetic-local-fixture-key'}
processes = []
checks = []

def cli(*args, input=None, good=True):
    result = subprocess.run([str(binaries/'cayw'), *args], cwd=workspace, env=env,
                            input=input, text=True, capture_output=True, timeout=30)
    if good:
        assert result.returncode == 0, result.stderr
    else:
        assert result.returncode != 0, result.stdout
    return result

def launch(*args, work=workspace):
    stderr = open(evidence/f'process-{len(processes)}.stderr', 'w')
    child = subprocess.Popen([str(binaries/'cayw'), 'start', '--workspace', str(work), *args],
                             cwd=workspace, env=env, stdin=subprocess.DEVNULL,
                             stdout=subprocess.PIPE, stderr=stderr, text=True, start_new_session=True)
    processes.append(child)
    assert select.select([child.stdout], [], [], 40)[0], 'startup did not report readiness'
    line = child.stdout.readline()
    assert line, f'launcher exited: {child.poll()}; inspect stderr'
    ready = json.loads(line)
    assert 'url' in ready, ready
    token = Path(ready['web_token_file']).read_text()
    assert token not in line
    assert Path(ready['web_token_file']).stat().st_mode & 0o777 == 0o600
    return child, ready, token

def stop(child):
    if child.poll() is None:
        child.send_signal(signal.SIGINT)
        assert child.wait(timeout=40) == 0

def rpc(ready, token, method, params=None, good=True):
    request = urllib.request.Request(ready['url']+'api/rpc',
        data=json.dumps({'method':method, 'params':params or {}}).encode(),
        headers={'Authorization':'Bearer '+token, 'Content-Type':'application/json'})
    try:
        with urllib.request.urlopen(request, timeout=40) as response:
            body = json.load(response)
    except urllib.error.HTTPError as error:
        body = json.load(error)
    if good:
        assert 'result' in body, body
        return body['result']
    assert 'error' in body, body
    return body['error']

try:
    child, ready, token = launch('--offline', '--demo-authorized')
    repeated = json.loads(cli('start', '--workspace', str(workspace)).stdout)
    assert repeated['url'] == ready['url']
    checks.append('unified Web startup, private token, duplicate start reuse')
    assert rpc(ready, token, 'settings.workspace')['workspace'] == str(workspace)
    assert rpc(ready, 'wrong-token', 'runtime.health', good=False)['code']
    session = rpc(ready, token, 'session.create', {'label':'offline end-to-end'})
    session_id = session['id']
    rpc(ready, token, 'session.prompt', {'session_id':session_id, 'text':'offline fixture'})
    assert (workspace/'cayw-demo.txt').read_text() == 'headless runtime demo'
    count = len(rpc(ready, token, 'session.list'))
    # Reconnect reads existing state; no prompt is retried.
    assert len(rpc(ready, token, 'session.list')) == count
    checks.append('authenticated real offline prompt, Host file effect, reconnect without replay')
    model = {'service':'openai', 'api':'chat_completions','api_version':'v1','id':'local-fixture',
             'base_url':'http://127.0.0.1:9/v1','max_output_tokens':64,'api_key_env':'CAYW_PRODUCT_TEST_KEY'}
    rpc(ready, token, 'settings.models.put', {'name':'daily','model':model})
    rpc(ready, token, 'settings.models.use', {'name':'daily'}, good=False)
    bad = dict(model, max_output_tokens=0)
    rpc(ready, token, 'settings.models.put', {'name':'daily','model':bad}, good=False)
    assert rpc(ready, token, 'settings.models.list')['models']['daily'] == model
    checks.append('named models persist, invalid update preserves model, offline selection rejected')
    descriptor = {'executable':str(binaries/'cayw-runtime-plugin-fixture'),'argv':['echo'],
        'manifest':{'name':'process','version':'1.0.0','protocol_version':1,'kind':'external',
        'capabilities':[{'name':'process.echo','description':'Offline process lifecycle fixture',
                         'parameters':{'type':'object','additionalProperties':False}}], 'permissions':{}}}
    descriptor_file = evidence/'local-plugin.json'
    descriptor_file.write_text(json.dumps(descriptor))
    cli('plugins', 'install', '--file', str(descriptor_file))
    rpc(ready, token, 'settings.plugins.enable', {'name':'process','approve_process':False}, good=False)
    live = rpc(ready, token, 'settings.plugins.enable', {'name':'process','approve_process':True})
    assert live['entries'][0]['available']
    binding_session = rpc(ready, token, 'session.create', {'label':'plugin binding'})['id']
    rpc(ready, token, 'settings.plugins.bind', {'name':'process','session_id':binding_session})
    rpc(ready, token, 'settings.plugins.update', {'name':'process','descriptor':descriptor}, good=False)
    rpc(ready, token, 'settings.plugins.disable', {'name':'process'})
    assert rpc(ready, token, 'session.get', {'session_id':binding_session})['frozen']
    original = rpc(ready, token, 'settings.plugins.list')['entries'][0]
    invalid = json.loads(json.dumps(descriptor)); invalid['manifest']['protocol_version'] = 99
    rejected_update = rpc(ready, token, 'settings.plugins.update', {'name':'process','descriptor':invalid}, good=False)
    assert not rejected_update['outcome_unknown'], rejected_update
    assert rpc(ready, token, 'settings.plugins.list')['entries'][0] == original
    # Valid descriptor, failing process: handshake failure remains visible.
    failing = dict(descriptor, argv=['idle-exit', str(workspace/'exit-gate')])
    # /usr/bin/false is a real local process and cannot speak plugin IPC.
    failing['executable'] = '/usr/bin/false'
    if not Path(failing['executable']).exists(): failing['executable'] = '/bin/false'
    rpc(ready, token, 'settings.plugins.update', {'name':'process','descriptor':failing})
    failed_enable = rpc(ready, token, 'settings.plugins.enable', {'name':'process','approve_process':True}, good=False)
    assert failed_enable['outcome_unknown'], failed_enable
    failed = rpc(ready, token, 'settings.plugins.list')['entries'][0]
    assert not failed['enabled'] and failed['last_error']
    rpc(ready, token, 'settings.plugins.update', {'name':'process','descriptor':descriptor})
    rpc(ready, token, 'settings.plugins.enable', {'name':'process','approve_process':True})
    # Make the management lock invalid: stop must fail before unloading, with
    # explicit uncertainty; repair only our fixture file and retry explicitly.
    state_dir = next((root/'cayw'/'workspaces').iterdir())
    lock = state_dir/'init.lock'; backup_lock = state_dir/'init-lock.bak'
    lock.rename(backup_lock); lock.mkdir()
    try:
        failed_stop = rpc(ready, token, 'settings.plugins.disable', {'name':'process'}, good=False)
        assert failed_stop['outcome_unknown']
        live = rpc(ready, token, 'settings.plugins.list')['entries'][0]
        assert live['enabled'] and live['available']
    finally:
        lock.rmdir(); backup_lock.rename(lock)
    rpc(ready, token, 'settings.plugins.disable', {'name':'process'})
    rpc(ready, token, 'settings.plugins.enable', {'name':'process','approve_process':True})
    checks.append('plugin registration, explicit approval, actual handshake, bind/freeze, failed update and failed enable')
    checks.append('management persistence failure preserves live status and uncertainty; explicit retry after fixture repair')
    stop(child)
    child, restarted, token = launch('--offline', '--demo-authorized')
    assert rpc(restarted, token, 'settings.plugins.list')['entries'][0]['available']
    assert rpc(restarted, token, 'session.get', {'session_id':binding_session})['frozen']
    assert len(rpc(restarted, token, 'session.list')) == count+1
    rpc(restarted, token, 'settings.plugins.remove', {'name':'process'})
    removed = rpc(restarted, token, 'settings.plugins.list')
    assert not removed['entries'] and not removed['loaded']
    checks.append('restart restores approved plugin and durable sessions; uninstall registration')
    stop(child)
    # EOF at every wizard stage must leave an existing config byte-for-byte.
    config = root/'cayw'/'config.json'
    original = config.read_bytes()
    for partial in ['', 'openai\n', 'openai\n\n', 'openai\n\nhttp://127.0.0.1:9/v1\nmodel\n\n64\nno\n']:
        cli('configure', input=partial)
        assert config.read_bytes() == original
    checks.append('configuration interruption and declined save preserve existing file')
    # Cancel a real approval-waiting task using its current persisted identity.
    (workspace/'cayw-demo.txt').unlink()
    child, gated, token = launch('--offline')
    cancel_session = rpc(gated, token, 'session.create', {'label':'cancel approval wait'})['id']
    task = rpc(gated, token, 'task.enqueue', {'session_id':cancel_session,'text':'wait for file approval'})
    with concurrent.futures.ThreadPoolExecutor() as pool:
        running = pool.submit(rpc, gated, token, 'task.resume', {'task_id':task['id']})
        deadline = time.monotonic()+30
        while True:
            snapshot = rpc(gated, token, 'session.get', {'session_id':cancel_session})
            if snapshot['pending_approvals']: break
            assert time.monotonic() < deadline, snapshot
            time.sleep(.02)
        current = rpc(gated, token, 'task.get', {'task_id':task['id']})
        target = {key:current[key] for key in ['session_id','worker_id','attempt_id','fence']}
        target['task_id'] = current['id']
        assert rpc(gated, token, 'session.cancel', target)['cancelled']
        running.result(timeout=40)
    assert rpc(gated, token, 'task.get', {'task_id':task['id']})['state'] == 'cancelled'
    assert not (workspace/'cayw-demo.txt').exists()
    checks.append('cancel approval-waiting task by current Task/Attempt/fence; no file side effect')
    stop(child)
    # Attach to an independently started runtime; exiting the launcher keeps it.
    raw = subprocess.Popen([str(binaries/'cayw'),'serve','--offline','--workspace',str(workspace)],
        cwd=workspace,env=env,stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,
        stderr=open(evidence/'attached-runtime.stderr','w'),text=True,start_new_session=True)
    processes.append(raw)
    assert select.select([raw.stdout], [], [], 40)[0]
    assert json.loads(raw.stdout.readline())['listening']
    child, attached, token = launch()
    assert not attached['owned_runtime']
    stop(child)
    assert raw.poll() is None
    cli('models','list')
    stop(raw)
    checks.append('attach to existing runtime; launcher shutdown preserves its owner')
    # Select a model with a synthetic env key; constructing adapters sends no request.
    child, online, token = launch()
    rpc(online, token, 'settings.models.use', {'name':'daily'})
    assert rpc(online, token, 'runtime.health')['model'] == 'local-fixture'
    rpc(online, token, 'settings.models.put', {'name':'second','model':dict(model,id='second')})
    rpc(online, token, 'settings.models.use', {'name':'second'})
    assert rpc(online, token, 'runtime.health')['model'] == 'second'
    rpc(online, token, 'settings.models.remove', {'name':'second'}, good=False)
    checks.append('live model activation and switch without restart or model request; active removal refused')
    # Both CLI import and replacement wizard use the same live management path.
    model_file = evidence/'model.json'; model_file.write_text(json.dumps(dict(model,id='from-cli')))
    cli('models','add','from-cli','--file',str(model_file))
    cli('models','use','from-cli')
    assert rpc(online, token, 'runtime.health')['model'] == 'from-cli'
    configured = json.loads(cli('configure', input='openai\n\nhttp://127.0.0.1:9/v1\nfrom-wizard\nCAYW_PRODUCT_TEST_KEY\n64\ny\n').stdout)
    assert configured['activation']['applied']
    assert rpc(online, token, 'runtime.health')['model'] == 'from-wizard'
    configured_bytes = config.read_bytes()
    selected_bytes = (state_dir/'models.json').read_bytes()
    cli('configure', input='')
    assert config.read_bytes() == configured_bytes
    assert (state_dir/'models.json').read_bytes() == selected_bytes
    checks.append('CLI import/use and configure hot application; interrupted replacement preserves configured model')
    stop(child)
    # A was selected online. With its service stopped, configure B must update
    # the very same selected profile used by the next startup, without a key read.
    absent_key = json.loads(cli('configure', input='openai\n\nhttp://127.0.0.1:9/v1\noffline-no-key\nCAYW_ABSENT_PRODUCT_KEY\n64\ny\n').stdout)
    assert absent_key['activation'] is None
    offline_b = json.loads(cli('configure', input='openai\n\nhttp://127.0.0.1:9/v1\noffline-B\nCAYW_PRODUCT_TEST_KEY\n64\ny\n').stdout)
    assert offline_b['activation'] is None
    child, rebooted, token = launch()
    assert rpc(rebooted, token, 'runtime.health')['model'] == 'offline-B'
    assert rpc(rebooted, token, 'settings.models.list')['models']['configured']['id'] == 'offline-B'
    # Failed activation keeps the desired profile visible, returns a failing CLI
    # status and leaves the actual live adapter unchanged. No model request.
    failed = cli('configure', input='openai\n\nhttp://127.0.0.1:9/v1\nunavailable-key-model\nCAYW_ABSENT_PRODUCT_KEY\n64\ny\n', good=False)
    assert json.loads(failed.stdout)['activation']['applied'] is False
    assert 'CAYW_ABSENT_PRODUCT_KEY' in failed.stderr
    assert rpc(rebooted, token, 'runtime.health')['model'] == 'offline-B'
    cli('configure', input='openai\n\nhttp://127.0.0.1:9/v1\noffline-B\nCAYW_PRODUCT_TEST_KEY\n64\ny\n')
    stop(child)
    checks.append('online A -> stop -> offline configure B -> restart selects B; failed activation is explicit and preserves live model')
    alternate = evidence/'other-workspace'; alternate.mkdir()
    child, other, token = launch(work=alternate)
    assert not rpc(other, token, 'settings.models.list')['models']
    assert not rpc(other, token, 'session.list')
    known = rpc(other, token, 'settings.workspace')['known_workspaces']
    assert str(workspace) in known and str(alternate) in known
    checks.append('workspace selection creates isolated model/session state and lists registered roots')
    stop(child)
    def digest(path):
        value = hashlib.sha256()
        with open(path,'rb') as stream:
            for chunk in iter(lambda:stream.read(1024*1024), b''): value.update(chunk)
        return value.hexdigest()
    report = {'passed':True,'platform':sys.platform,'checks':checks,
        'binaries':{name:digest(binaries/name) for name in ['cayw','cayw-webui','cayw-runtime-plugin-fixture']}}
    (evidence/'summary.json').write_text(json.dumps(report, ensure_ascii=False, indent=2)+'\n')
    print(json.dumps(report, ensure_ascii=False, indent=2))
finally:
    for child in reversed(processes):
        if child.poll() is None:
            child.send_signal(signal.SIGINT)
            try: child.wait(timeout=40)
            except subprocess.TimeoutExpired:
                print(f'Unconfirmed cleanup for owned launcher PID {child.pid}', file=sys.stderr)
