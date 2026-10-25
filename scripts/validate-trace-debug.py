#!/usr/bin/env python3
"""Offline Issue31 process fixture. Use --help; all tokens below are public test data."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import select
import signal
import socket
import subprocess
import sys
import time

IPC_TOKEN = 'issue31-public-ipc-fixture-0000000000000000'
WEB_TOKEN = 'issue31-public-web-fixture-1111111111111111'


def save(path, value):
    path.write_text(json.dumps(value, ensure_ascii=False, indent=2) + '\n')


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def ready(child, name, directory):
    deadline = time.monotonic() + 40
    lines = []
    while time.monotonic() < deadline:
        if child.poll() is not None:
            raise RuntimeError(name + ' exited before readiness')
        readable, _, _ = select.select([child.stdout], [], [], 0.2)
        if not readable:
            continue
        line = child.stdout.readline()
        if not line:
            continue
        lines.append(line)
        (directory / (name + '.stdout.log')).write_text(''.join(lines))
        try:
            data = json.loads(line)
            if isinstance(data, dict):
                return data
        except json.JSONDecodeError:
            pass
    raise RuntimeError(name + ' did not report readiness')


def stop(child):
    if child.poll() is None:
        child.send_signal(signal.SIGINT)
        try:
            child.wait(timeout=15)
        except subprocess.TimeoutExpired:
            child.terminate()
            child.wait(timeout=10)
    return child.returncode


class Ipc:
    def __init__(self, address):
        host, port = address.rsplit(':', 1)
        self.socket = socket.create_connection((host, int(port)), timeout=10)
        self.socket.settimeout(200)
        self.file = self.socket.makefile('rwb', buffering=0)
        self.count = 0
        response = self.send({'version': 1, 'id': 'hello', 'method': 'hello',
                              'params': {'token': IPC_TOKEN}})
        if 'error' in response:
            raise RuntimeError('fixture IPC authentication failed: ' + json.dumps(response))

    def send(self, request):
        self.file.write(json.dumps(request, separators=(',', ':')).encode() + b'\n')
        response = json.loads(self.file.readline())
        if response.get('id') != request['id']:
            raise RuntimeError('IPC response identity mismatch')
        return response

    def call(self, method, params):
        self.count += 1
        response = self.send({'version': 1, 'id': 'fixture-' + str(self.count),
                              'method': method, 'params': params})
        if 'error' in response:
            raise RuntimeError(method + ': ' + json.dumps(response['error']))
        return response['result']

    def close(self):
        self.file.close()
        self.socket.close()


def normalize_ready(row):
    address = row.get('ipc_addr') or row.get('listening') or row.get('addr') or row.get('address')
    session = row.get('session_id') or row.get('session')
    if not address or not session:
        raise RuntimeError('demo readiness lacks IPC address/session: ' + json.dumps(row))
    return address, session


def task(directory, wait_read=False):
    configuration = json.loads((directory / 'ready.json').read_text())
    ipc = Ipc(configuration['ipc_addr'])
    prefix = 'task-' + str(time.time_ns())
    try:
        text = ('Run the waiting offline trace fixture with input.amount.' if wait_read else
                'Run the initial offline trace condition fixture with input.amount.')
        admitted = ipc.call('task.enqueue', {'session_id': configuration['session_id'],
                                           'text': text})
        save(directory / (prefix + '-admitted.json'), admitted)
        task_id = admitted['id']
        before = time.monotonic()
        result = ipc.call('task.resume', {'task_id': task_id})
        save(directory / (prefix + '-result.json'), result)
        capture(directory, ipc, configuration, prefix, task_id,
                (time.monotonic() - before) * 1000)
    finally:
        ipc.close()


def capture(directory, ipc, configuration, prefix, task_id=None, elapsed_ms=None):
    events = []
    after = 0
    while True:
        page = ipc.call('session.events', {'session_id': configuration['session_id'],
                                          'after': after, 'limit': 32, 'wait_ms': 0})
        if not page:
            break
        events.extend(page)
        after = page[-1]['cursor']
    calls = [row['event']['call'] for row in events
             if row['event']['type'] == 'capability_settled']
    trace = [row for row in events if row['event']['type'] == 'host'
             and row['event'].get('kind') == 'trace_node']
    source = directory / 'demo' / 'workspace' / 'side-effect.txt'
    receipt = {'task_id': task_id, 'session_id': configuration['session_id'],
               'elapsed_ms': elapsed_ms, 'last_cursor': after,
               'trace_events': len(trace), 'calls': len(calls),
               'writes': len([call for call in calls if call['name'] == 'fs.write']),
               'tasks': ipc.call('session.tasks', {'session_id': configuration['session_id']}),
               'side_effect_sha256': digest(source) if source.exists() else None,
               'side_effect_mtime_ns': source.stat().st_mtime_ns if source.exists() else None}
    save(directory / (prefix + '-events.json'), events)
    save(directory / (prefix + '-receipt.json'), receipt)
    print(json.dumps(receipt, ensure_ascii=False))
    return receipt


def inspect(directory):
    configuration = json.loads((directory / 'ready.json').read_text())
    ipc = Ipc(configuration['ipc_addr'])
    try:
        return capture(directory, ipc, configuration, 'inspect-' + str(time.time_ns()))
    finally:
        ipc.close()


def approve_read(directory):
    configuration = json.loads((directory / 'ready.json').read_text())
    ipc = Ipc(configuration['ipc_addr'])
    try:
        view = ipc.call('session.get', {'session_id': configuration['session_id']})
        expected = {'path': 'approval-input.json'}
        pending = [approval for approval in view['pending_approvals']
                   if approval['input']['capability'] == 'fs.read'
                   and json.loads(approval['input']['scope']) == expected
                   and approval['input']['target']['type'] == 'capability_call']
        if len(pending) != 1:
            raise RuntimeError('expected exactly one pending fixture read approval')
        decision = ipc.call('approval.decide', {'session_id': configuration['session_id'],
                                               'approval_id': pending[0]['id'],
                                               'decision': 'allow_once'})
        save(directory / ('read-approval-' + str(time.time_ns()) + '.json'), decision)
        print(json.dumps(decision, ensure_ascii=False))
    finally:
        ipc.close()


def shutdown(directory):
    configuration = json.loads((directory / 'ready.json').read_text())
    (directory / 'shutdown-requested').write_text('fixture requested shutdown\n')
    (directory / 'plugin-stop-requested').write_text('fixture requested shutdown\n')
    for field in ('plugin_pid', 'demo_pid'):
        try:
            os.kill(configuration[field], signal.SIGINT)
        except ProcessLookupError:
            pass


def serve(args):
    directory = args.output.resolve()
    directory.mkdir(parents=True, exist_ok=False)
    environment = dict(os.environ)
    environment['CAYW_TRACE_DEMO_TOKEN'] = IPC_TOKEN
    environment['CAYW_IPC_TOKEN'] = IPC_TOKEN
    environment['CAYW_TRACE_WEB_TOKEN'] = WEB_TOKEN
    demo_errors = (directory / 'demo.stderr.log').open('wb')
    plugin_errors = (directory / 'plugin.stderr.log').open('wb')
    children = []
    commands = []
    try:
        command = [str(args.demo.resolve()), '--root', str(directory / 'demo'),
                   '--listen', '127.0.0.1:0', '--token-env', 'CAYW_TRACE_DEMO_TOKEN']
        commands.append(command)
        demo = subprocess.Popen(command, env=environment, stdout=subprocess.PIPE,
                                stderr=demo_errors, text=True, bufsize=1)
        children.append(demo)
        initial = ready(demo, 'demo', directory)
        address, session = normalize_ready(initial)
        command = [str(args.plugin.resolve()), '--store', str(directory / 'trace.json'),
                   '--session', session, '--ipc-addr', address, '--web-addr', '127.0.0.1:0']
        commands.append(command)
        plugin = subprocess.Popen(command, env=environment, stdout=subprocess.PIPE,
                                  stderr=plugin_errors, text=True, bufsize=1)
        children.append(plugin)
        web = ready(plugin, 'plugin', directory)
        manifest = {'ipc_addr': address, 'session_id': session, 'web_url': web['web_url'],
                    'demo_pid': demo.pid, 'plugin_pid': plugin.pid,
                    'provider': 'fixed offline script_response; real Runtime/Host/Luau/IPC',
                    'platform': sys.platform, 'commands': commands,
                    'binaries': {'demo': digest(args.demo), 'plugin': digest(args.plugin)}}
        save(directory / 'ready.json', manifest)
        print(json.dumps(manifest, ensure_ascii=False), flush=True)
        stopped = False
        def on_signal(_signum, _frame):
            nonlocal stopped
            stopped = True
        signal.signal(signal.SIGINT, on_signal)
        signal.signal(signal.SIGTERM, on_signal)
        while not stopped:
            if (directory / 'shutdown-requested').exists():
                break
            if demo.poll() is not None:
                raise RuntimeError('demo stopped unexpectedly')
            if plugin.poll() is not None and not (directory / 'plugin-stop-requested').exists():
                raise RuntimeError('plugin stopped unexpectedly')
            time.sleep(0.2)
    finally:
        codes = [stop(child) for child in reversed(children)]
        save(directory / 'shutdown.json', {'exit_codes_reverse_start_order': codes})
        demo_errors.close()
        plugin_errors.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest='action', required=True)
    start = sub.add_parser('serve', help='start actual demo + optional plugin, wait for Ctrl+C')
    start.add_argument('--demo', type=Path, required=True)
    start.add_argument('--plugin', type=Path, required=True)
    start.add_argument('--output', type=Path, required=True)
    run = sub.add_parser('task', help='explicitly start one actual offline Runtime task')
    run.add_argument('--output', type=Path, required=True)
    run.add_argument('--wait-read', action='store_true',
                     help='wait for explicit approval of a read inside the input node')
    approve = sub.add_parser('approve-read', help='allow only the fixture pending read once')
    approve.add_argument('--output', type=Path, required=True)
    check = sub.add_parser('inspect', help='capture read-only IPC facts without starting any task')
    check.add_argument('--output', type=Path, required=True)
    finish = sub.add_parser('shutdown', help='stop both processes owned by this fixture')
    finish.add_argument('--output', type=Path, required=True)
    disable = sub.add_parser('stop-plugin', help='stop only this fixture plugin; keep Runtime available')
    disable.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    if args.action == 'serve':
        serve(args)
    elif args.action == 'task':
        task(args.output.resolve(), args.wait_read)
    elif args.action == 'approve-read':
        approve_read(args.output.resolve())
    elif args.action == 'inspect':
        inspect(args.output.resolve())
    elif args.action == 'shutdown':
        shutdown(args.output.resolve())
    else:
        directory = args.output.resolve()
        manifest = json.loads((directory / 'ready.json').read_text())
        (directory / 'plugin-stop-requested').write_text('fixture requested plugin stop\n')
        os.kill(manifest['plugin_pid'], signal.SIGINT)
        print('fixture plugin stop requested; Runtime left running')


if __name__ == '__main__':
    main()
