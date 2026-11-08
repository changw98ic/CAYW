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
env = {'PATH': os.environ['PATH'], 'HOME': str(root), 'CAYW_HOME': str(root / 'cayw')}
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
    child = subprocess.Popen([str(binaries/'cayw'), 'start', '--ui', 'web', '--no-open', '--workspace', str(work), *args],
                             cwd=workspace, env=env, stdin=subprocess.DEVNULL,
                             stdout=subprocess.PIPE, stderr=stderr, text=True, start_new_session=True)
    processes.append(child)
    assert select.select([child.stdout], [], [], 40)[0], 'startup did not report readiness'
    line = child.stdout.readline()
    assert line, f'launcher exited: {child.poll()}; inspect stderr'
    ready = json.loads(line)
    assert 'url' in ready, ready
    assert ready['access'] == 'local'
    assert 'web_token_file' not in ready and 'launch_url' not in ready
    return child, ready, None

def stop(child):
    if child.poll() is None:
        child.send_signal(signal.SIGINT)
        assert child.wait(timeout=40) == 0

def rpc(ready, token, method, params=None, good=True, workspace_id=None):
    body = {'method':method, 'params':params or {}}
    if workspace_id is not None:
        body['workspace_id'] = workspace_id
    request = urllib.request.Request(ready['url']+'api/rpc',
        data=json.dumps(body).encode(),
        headers={'Content-Type':'application/json'})
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
    repeated = json.loads(cli('start', '--ui', 'web', '--no-open', '--workspace', str(workspace)).stdout)
    assert repeated['url'] == ready['url']
    checks.append('explicit optional local Web startup without login or ticket, duplicate start reuse')
    assert rpc(ready, token, 'settings.workspace')['workspace'] == str(workspace)
    assert rpc(ready, None, 'runtime.health')['status'] == 'ok'
    session = rpc(ready, token, 'session.create', {'label':'offline end-to-end'})
    session_id = session['id']
    rpc(ready, token, 'session.prompt', {'session_id':session_id, 'text':'offline fixture'})
    assert (workspace/'cayw-demo.txt').read_text() == 'headless runtime demo'
    count = len(rpc(ready, token, 'session.list'))
    # Reconnect reads existing state; no prompt is retried.
    assert len(rpc(ready, token, 'session.list')) == count
    checks.append('authenticated real offline prompt, Host file effect, reconnect without replay')
    model_key = 'synthetic-local-fixture-key'
    model = {'service':'openai', 'api':'chat_completions','api_version':'v1','id':'local-fixture',
             'base_url':'http://127.0.0.1:9/v1','max_output_tokens':64,'api_key':model_key}
    rpc(ready, token, 'settings.models.put', {'name':'daily','model':model})
    rpc(ready, token, 'settings.models.use', {'name':'daily'}, good=False)
    bad = dict(model, max_output_tokens=0)
    rpc(ready, token, 'settings.models.put', {'name':'daily','model':bad}, good=False)
    listed_models = rpc(ready, token, 'settings.models.list')
    public_daily = dict(model); public_daily.pop('api_key'); public_daily['authentication'] = 'plain_file'
    assert listed_models['models']['daily'] == public_daily
    assert model_key not in json.dumps(listed_models)
    state_dir = next((root/'cayw'/'workspaces').iterdir())
    models_file = state_dir/'models.json'
    assert models_file.stat().st_mode & 0o777 == 0o600
    assert json.loads(models_file.read_text())['models']['daily']['api_key'] == model_key
    checks.append('named models keep an optional plain-text key only in private models.json; public settings redact it; invalid update and offline selection are rejected')
    descriptor = {'executable':str(binaries/'cayw-runtime-plugin-fixture'),'argv':['echo'],
        'manifest':{'name':'process','version':'1.0.0','protocol_version':1,
        'role':'workspace_architecture','kind':'external',
        'capabilities':[{'name':'process.echo','description':'Offline process lifecycle fixture',
                         'parameters':{'type':'object','additionalProperties':False}}], 'permissions':{}}}
    descriptor_file = evidence/'local-plugin.json'
    descriptor_file.write_text(json.dumps(descriptor))
    cli('plugins', 'install', '--file', str(descriptor_file))
    rpc(ready, token, 'settings.plugins.enable', {'name':'process','approve_process':False}, good=False)
    live = rpc(ready, token, 'settings.plugins.enable', {'name':'process','approve_process':True})
    assert live['entries'][0]['available']
    ordinary_session = rpc(ready, token, 'session.create', {'label':'ordinary plugin-independent session'})['id']
    rpc(ready, token, 'settings.plugins.update', {'name':'process','descriptor':descriptor}, good=False)
    rpc(ready, token, 'settings.plugins.disable', {'name':'process'})
    ordinary_view = rpc(ready, token, 'session.get', {'session_id':ordinary_session})
    assert not ordinary_view['frozen'] and ordinary_view.get('binding') is None
    original = rpc(ready, token, 'settings.plugins.list')['entries'][0]
    invalid = json.loads(json.dumps(descriptor)); invalid['manifest']['protocol_version'] = 99
    rejected_update = rpc(ready, token, 'settings.plugins.update', {'name':'process','descriptor':invalid}, good=False)
    assert not rejected_update['outcome_unknown'], rejected_update
    assert rpc(ready, token, 'settings.plugins.list')['entries'][0] == original
    # Keep an unidentifiable failed process separate from the healthy plugin.
    # Its cleanup block must not be cleared by update, disable, or remove.
    failing = json.loads(json.dumps(descriptor))
    failing['manifest']['name'] = 'failed-process'
    failing['argv'] = ['idle-exit', str(workspace/'exit-gate')]
    # /usr/bin/false is a real local process and cannot speak plugin IPC.
    failing['executable'] = '/usr/bin/false'
    if not Path(failing['executable']).exists(): failing['executable'] = '/bin/false'
    rpc(ready, token, 'settings.plugins.install', {'descriptor':failing})
    failed_enable = rpc(ready, token, 'settings.plugins.enable', {'name':'failed-process','approve_process':True}, good=False)
    assert failed_enable['outcome_unknown'], failed_enable
    failed = next(entry for entry in rpc(ready, token, 'settings.plugins.list')['entries']
                  if entry['name'] == 'failed-process')
    assert not failed['enabled'] and failed['cleanup_blocked'] and failed['last_error'], failed
    repaired = json.loads(json.dumps(failing))
    repaired['executable'] = descriptor['executable']
    repaired['argv'] = descriptor['argv']
    for method, params in [
        ('settings.plugins.update', {'name':'failed-process','descriptor':repaired}),
        ('settings.plugins.disable', {'name':'failed-process'}),
        ('settings.plugins.remove', {'name':'failed-process'}),
    ]:
        blocked = rpc(ready, token, method, params, good=False)
        assert blocked['outcome_unknown'], (method, blocked)
    rpc(ready, token, 'settings.plugins.enable', {'name':'process','approve_process':True})
    # Make the management lock invalid: stop must fail before unloading, with
    # explicit uncertainty; repair only our fixture file and retry explicitly.
    lock = state_dir/'init.lock'; backup_lock = state_dir/'init-lock.bak'
    lock.rename(backup_lock); lock.mkdir()
    try:
        failed_stop = rpc(ready, token, 'settings.plugins.disable', {'name':'process'}, good=False)
        assert failed_stop['outcome_unknown']
        live = next(entry for entry in rpc(ready, token, 'settings.plugins.list')['entries']
                    if entry['name'] == 'process')
        assert live['enabled'] and live['available']
    finally:
        lock.rmdir(); backup_lock.rename(lock)
    rpc(ready, token, 'settings.plugins.disable', {'name':'process'})
    rpc(ready, token, 'settings.plugins.enable', {'name':'process','approve_process':True})
    checks.append('workspace architecture plugin registration, explicit approval and handshake; disable keeps an ordinary Session usable; failed update and failed enable remain explicit')
    checks.append('an unidentifiable failed plugin keeps its cleanup block and rejects blind update, disable, and remove while an independent healthy plugin remains usable')
    checks.append('management persistence failure preserves live status and uncertainty; explicit retry after fixture repair')
    stop(child)
    child, restarted, token = launch('--offline', '--demo-authorized')
    restarted_plugins = rpc(restarted, token, 'settings.plugins.list')
    restarted_process = next(entry for entry in restarted_plugins['entries']
                             if entry['name'] == 'process')
    assert restarted_process['available']
    ordinary_view = rpc(restarted, token, 'session.get', {'session_id':ordinary_session})
    assert not ordinary_view['frozen'] and ordinary_view.get('binding') is None
    assert len(rpc(restarted, token, 'session.list')) == count+1
    rpc(restarted, token, 'settings.plugins.disable', {'name':'process'})
    rpc(restarted, token, 'settings.plugins.remove', {'name':'process'})
    removed = rpc(restarted, token, 'settings.plugins.list')
    assert not removed['loaded']
    assert len(removed['entries']) == 1, removed
    retained_failure = removed['entries'][0]
    assert retained_failure['name'] == 'failed-process' and retained_failure['cleanup_blocked'], retained_failure
    checks.append('restart restores the healthy workspace plugin and durable ordinary Session; explicit disable precedes its global removal while the unrelated failed instance stays blocked')
    stop(child)
    # EOF at every explicit conf stage leaves both canonical local files byte-for-byte.
    config = root/'cayw'/'config.json'
    original_config = config.read_bytes()
    original_models = models_file.read_bytes()
    for partial in ['', 'openai\n', 'openai\n\n', 'openai\n\nhttp://127.0.0.1:9/v1\nmodel\n\n64\nno\n']:
        cli('conf', input=partial)
        assert config.read_bytes() == original_config
        assert models_file.read_bytes() == original_models
    removed_command = cli('configure', good=False)
    assert 'expected `start`' in removed_command.stderr
    assert config.read_bytes() == original_config and models_file.read_bytes() == original_models
    checks.append('conf interruption and declined save preserve models.json; removed configure command is rejected without state changes')
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
    # Attach to an independently started unique Client; exiting WebUI keeps it.
    raw = subprocess.Popen([str(binaries/'cayw'),'serve','--offline','--workspace',str(workspace)],
        cwd=workspace,env=env,stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,
        stderr=open(evidence/'attached-client.stderr','w'),text=True,start_new_session=True)
    processes.append(raw)
    assert select.select([raw.stdout], [], [], 40)[0]
    assert json.loads(raw.stdout.readline())['listening']
    child, attached, token = launch()
    assert not attached['owned_client']
    stop(child)
    assert raw.poll() is None
    cli('models','list')
    stop(raw)
    checks.append('WebUI attaches to the existing unique Client; WebUI shutdown preserves the Client owner')
    # Constructing adapters from an optional plain-text key sends no model request.
    child, online, token = launch()
    rpc(online, token, 'settings.models.use', {'name':'daily'})
    assert rpc(online, token, 'runtime.health')['model'] == 'local-fixture'
    rpc(online, token, 'settings.models.put', {'name':'second','model':dict(model,id='second')})
    rpc(online, token, 'settings.models.use', {'name':'second'})
    assert rpc(online, token, 'runtime.health')['model'] == 'second'
    rpc(online, token, 'settings.models.remove', {'name':'second'}, good=False)
    checks.append('live model activation and switch need no restart or model request; active removal is refused')
    # CLI import and explicit conf use the same selected workspace profile.
    model_file = evidence/'model.json'; model_file.write_text(json.dumps(dict(model,id='from-cli')))
    cli('models','add','from-cli','--file',str(model_file))
    cli('models','use','from-cli')
    assert rpc(online, token, 'runtime.health')['model'] == 'from-cli'
    configured = json.loads(cli('conf', input='openai\n\nhttp://127.0.0.1:9/v1\nfrom-conf\nsynthetic-local-fixture-key\n64\ny\n').stdout)
    assert configured['activation']['applied']
    assert rpc(online, token, 'runtime.health')['model'] == 'from-conf'
    configured_private = json.loads(models_file.read_text())
    assert configured_private['selected'] == 'configured'
    assert configured_private['models']['configured']['api_key'] == model_key
    configured_public = rpc(online, token, 'settings.models.list')
    assert configured_public['models']['configured']['authentication'] == 'plain_file'
    assert model_key not in json.dumps(configured_public)
    configured_bytes = models_file.read_bytes()
    cli('conf', input='')
    assert config.read_bytes() == original_config
    assert models_file.read_bytes() == configured_bytes
    checks.append('models add/use and conf hot-apply one workspace profile; public status never returns its plain-text key')
    stop(child)
    # A blank key is a real no-authentication profile, not an environment lookup.
    no_auth = json.loads(cli('conf', input='openai\n\nhttp://127.0.0.1:9/v1\noffline-no-key\n\n64\ny\n').stdout)
    assert no_auth['activation'] is None
    no_auth_private = json.loads(models_file.read_text())
    assert no_auth_private['selected'] == 'configured'
    assert 'api_key' not in no_auth_private['models']['configured']
    child, rebooted, token = launch()
    assert rpc(rebooted, token, 'runtime.health')['model'] == 'offline-no-key'
    no_auth_public = rpc(rebooted, token, 'settings.models.list')['models']['configured']
    assert no_auth_public['authentication'] == 'none' and 'api_key' not in no_auth_public
    keyed = json.loads(cli('conf', input='openai\n\nhttp://127.0.0.1:9/v1\noffline-B\nsynthetic-local-fixture-key\n64\ny\n').stdout)
    assert keyed['activation']['applied']
    assert rpc(rebooted, token, 'runtime.health')['model'] == 'offline-B'
    assert json.loads(models_file.read_text())['models']['configured']['api_key'] == model_key
    assert model_key not in json.dumps(rpc(rebooted, token, 'settings.models.list'))
    stop(child)
    child, rebooted, token = launch()
    assert rpc(rebooted, token, 'runtime.health')['model'] == 'offline-B'
    checks.append('blank key starts as no-authentication; a later plain-text key hot-applies, persists, and remains redacted after restart')
    owner = child
    alternate = evidence/'other-workspace'; alternate.mkdir()
    other_command, other, token = launch(work=alternate)
    assert other['owned_client'] is False and other['reused'] is True
    assert other['client'] == rebooted['client'] and other['url'] == rebooted['url']
    other_id = other['workspace_id']; assert other_id != 'local'
    catalog = rpc(rebooted, token, 'settings.workspaces.list')
    assert any(entry['id'] == other_id and entry['path'] == str(alternate) for entry in catalog['workspaces'])
    opened = rpc(rebooted, token, 'settings.workspaces.open', {'id':other_id})
    assert opened['id'] == other_id
    assert not rpc(rebooted, token, 'settings.models.list', workspace_id=other_id)['models']
    assert not rpc(rebooted, token, 'session.list', workspace_id=other_id)
    known = rpc(rebooted, token, 'settings.workspace', workspace_id=other_id)['known_workspaces']
    assert str(workspace) in known and str(alternate) in known
    assert rpc(rebooted, token, 'runtime.health')['model'] == 'offline-B'
    stop(other_command)
    stop(owner)
    checks.append('one Client and one WebUI route two workspaces with isolated model and Session state; selecting the second workspace does not change the first')
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
