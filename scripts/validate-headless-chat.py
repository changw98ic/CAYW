#!/usr/bin/env python3
"""Validate installed Headless Runtime/client binaries using a local HTTP fixture.

Run with python3 -I. This script never loads model credentials or calls Magpie.
The fixture provides synthetic SSE responses; Luau, Host, Device, IPC, files and
persistent sessions use the supplied Rust executables. Use a fresh --out path.
"""
from __future__ import annotations

import argparse
import base64
from datetime import datetime, timezone
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import queue
import re
import secrets
import signal
import socket
import subprocess
import sys
import threading
import time

WATCHDOG = 60
ROUTE = 'group/auto-deepseek-v4-1-flash'
FIRST = 'CASE:round1 Read source.txt, copy its complete contents to round1.txt, and report the observed nonce.'
SECOND = 'CASE:round2 Continue from the previous observed fact. Read round1.txt and write round2.txt with continued: prepended. Report that same nonce.'


def require(condition, reason):
    if not condition:
        raise AssertionError(reason)


def save(path, value):
    with path.open('x') as output:
        json.dump(value, output, ensure_ascii=False, indent=2)
        output.write('\n')
        output.flush()
        os.fsync(output.fileno())


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def wait_for(check, reason, seconds=WATCHDOG):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        value = check()
        if value:
            return value
        time.sleep(0.02)
    raise TimeoutError(reason)


class Running:
    """Capture one owned process continuously, including a live JSONL stream."""
    def __init__(self, argv, environment, directory, name):
        self.argv = [str(value) for value in argv]
        self.directory = directory
        self.name = name
        self.rows = []
        self.lines = queue.Queue()
        self.stdout_file = (directory / (name + '.stdout.jsonl')).open('x')
        self.stderr_file = (directory / (name + '.stderr.log')).open('x')
        self.child = subprocess.Popen(self.argv, env=environment,
                                      stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                                      stderr=self.stderr_file, text=True, bufsize=1)
        self.started_at = datetime.now(timezone.utc).isoformat()
        self.reader = threading.Thread(target=self.read, daemon=True)
        self.reader.start()
        self.finished = False

    def read(self):
        for line in self.child.stdout:
            self.stdout_file.write(line)
            self.stdout_file.flush()
            try:
                row = json.loads(line)
            except ValueError:
                row = {'invalid_jsonl': line.rstrip()}
            self.rows.append(row)
            self.lines.put(row)

    def finish(self, expected=0):
        code = self.child.wait(timeout=WATCHDOG)
        self.reader.join(timeout=5)
        require(not self.reader.is_alive(), self.name + ': stdout reader did not finish')
        if not self.finished:
            self.child.stdout.close()
            self.stdout_file.close()
            self.stderr_file.close()
            save(self.directory / (self.name + '.command.json'), {
                'argv': self.argv, 'pid': self.child.pid,
                'started_at': self.started_at,
                'finished_at': datetime.now(timezone.utc).isoformat(),
                'exit_code': code,
            })
            self.finished = True
        if expected is not None:
            require(code == expected, self.name + ': exit ' + str(code))
        require(not any('invalid_jsonl' in row for row in self.rows),
                self.name + ': non-JSONL output')
        return self.rows, code

    def stop(self, graceful=True):
        if self.child.poll() is None:
            if graceful:
                self.child.send_signal(signal.SIGINT)
            else:
                self.child.kill()
            try:
                self.child.wait(timeout=15)
            except subprocess.TimeoutExpired:
                self.child.kill()
                self.child.wait(timeout=15)
        return self.finish(expected=None)


class Fixture:
    def __init__(self, directory):
        self.directory = directory
        self.requests = []
        self.lock = threading.Lock()
        owner = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = 'HTTP/1.1'

            def log_message(self, *_):
                pass

            def do_POST(self):
                self.close_connection = True
                try:
                    require(self.path == '/v1/chat/completions', 'wrong protocol path')
                    require(self.headers.get('Authorization') == 'Bearer offline-http-fixture',
                            'fixture authentication mismatch')
                    size = int(self.headers.get('Content-Length', '0'))
                    require(0 < size <= 4 * 1024 * 1024, 'fixture request byte limit')
                    request = json.loads(self.rfile.read(size))
                    require(request['model'] == ROUTE, 'requested model route changed')
                    require(request['stream'] is True, 'not a streaming request')
                    require(request['stream_options']['include_usage'] is True,
                            'usage was not requested')
                    with owner.lock:
                        number = len(owner.requests) + 1
                        entry = {'number': number, 'path': self.path, 'request': request,
                                 'source': 'offline synthetic HTTP fixture'}
                        owner.requests.append(entry)
                        save(owner.directory / ('request-%03d.json' % number), entry)
                    last_user = max(index for index, message in enumerate(request['messages'])
                                    if message['role'] == 'user')
                    text = json.dumps(request['messages'][last_user]['content'])
                    match = re.search(r'CASE:([a-z0-9-]+)', text)
                    require(match is not None, 'unrecognized fixture input')
                    case = match[1]
                    tools = [message for message in request['messages'][last_user + 1:]
                             if message['role'] == 'tool']
                    identity = 'offline-response-%03d' % number
                    model = 'offline-served-deepseek-fixture'
                    if case in ('stream-cancel', 'disconnect'):
                        self.send_response(200)
                        self.send_header('Content-Type', 'text/event-stream')
                        self.send_header('Transfer-Encoding', 'chunked')
                        self.end_headers()
                        content = 'streaming partial for ' + case
                        chunk = owner.chunk(identity, model, {'role': 'assistant', 'content': content})
                        self.write_chunk(chunk)
                        until = time.monotonic() + WATCHDOG
                        while time.monotonic() < until:
                            time.sleep(0.1)
                            self.write_chunk(b': heartbeat\n\n')
                        self.write_chunk(b'')
                        return
                    if not tools:
                        if case == 'round1':
                            script = "local r=cayw.call('fs.read',{path='source.txt'}); cayw.call('fs.write',{path='round1.txt',content=r.content}); return r"
                        elif case == 'round2':
                            history = json.dumps(request['messages'][:last_user])
                            require('step1 observed nonce-' in history,
                                    'second user turn lost the first assistant fact')
                            require('tool_call_id' in history, 'first tool results missing from history')
                            script = "local r=cayw.call('fs.read',{path='round1.txt'}); cayw.call('fs.write',{path='round2.txt',content='continued:'..r.content}); return r"
                        elif case == 'deny':
                            script = "return cayw.call('fs.write',{path='denied.txt',content='must not exist'})"
                        elif case in ('tool-cancel', 'crash-before-approval'):
                            script = "return cayw.call('test.run',{profile='hold'})"
                        else:
                            raise AssertionError('unsupported fixture case ' + case)
                        arguments = json.dumps({'action': 'exec', 'script': script,
                                                'yield_time_ms': 30000, 'max_output_tokens': 8192})
                        # Protocol fragments are independent of the TCP writes below.
                        fragments = [arguments[start:start + 23]
                                     for start in range(0, len(arguments), 23)]
                        response = b''
                        for index, fragment in enumerate(fragments):
                            call = {'index': 0, 'function': {'arguments': fragment}}
                            delta = {'tool_calls': [call]}
                            if index == 0:
                                delta['role'] = 'assistant'
                                call.update({'id': 'offline-call-%03d' % number,
                                             'type': 'function'})
                                call['function']['name'] = 'execute_code'
                            response += owner.chunk(identity, model, delta)
                        response += owner.chunk(identity, model, {}, 'tool_calls')
                    else:
                        encoded = json.dumps(tools)
                        if case in ('round1', 'round2'):
                            nonce = re.search(r'nonce-[0-9a-f]{16}', encoded)
                            require(nonce is not None, 'tool output did not contain the actual file fact')
                            word = 'step1 observed ' if case == 'round1' else 'step2 continued '
                            answer = word + nonce[0]
                        elif case == 'deny':
                            require('denied' in encoded or 'permission' in encoded,
                                    'fixture did not receive the actual denial')
                            answer = 'write denied; no file was created'
                        else:
                            raise AssertionError('a cancelled effect must not request another answer')
                        response = owner.chunk(identity, model, {'role': 'assistant', 'content': answer[:8]})
                        response += owner.chunk(identity, model, {'content': answer[8:]})
                        response += owner.chunk(identity, model, {}, 'stop')
                    response += owner.data({'id': identity, 'model': model, 'choices': [],
                                             'usage': {'prompt_tokens': 41, 'completion_tokens': 9}})
                    response += b'data: [DONE]\n\n'
                    with (owner.directory / ('response-%03d.sse' % number)).open('xb') as output:
                        output.write(response)
                    save(owner.directory / ('response-%03d.transport.json' % number), {
                        'source': 'offline synthetic HTTP fixture',
                        'tool_argument_deltas': len(fragments) if not tools else 0,
                        'tool_argument_delta_bytes': 23 if not tools else None,
                        'tcp_write_bytes': 37,
                        'tcp_write_count': (len(response) + 36) // 37,
                        'response_bytes': len(response),
                        'response_sha256': hashlib.sha256(response).hexdigest(),
                        'complete': True,
                    })
                    self.send_response(200)
                    self.send_header('Content-Type', 'text/event-stream')
                    self.send_header('Content-Length', str(len(response)))
                    self.send_header('Connection', 'close')
                    self.end_headers()
                    for start in range(0, len(response), 37):
                        self.wfile.write(response[start:start + 37])
                        self.wfile.flush()
                except (BrokenPipeError, ConnectionResetError):
                    pass
                except Exception as error:
                    with owner.lock:
                        errors = owner.directory / 'fixture-errors.jsonl'
                        with errors.open('a') as output:
                            output.write(json.dumps({'error': str(error)}) + '\n')
                    self.close_connection = True

            def write_chunk(self, payload):
                self.wfile.write(('%x\r\n' % len(payload)).encode() + payload + b'\r\n')
                self.wfile.flush()

        self.server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.base = 'http://127.0.0.1:%d/v1' % self.server.server_port

    @staticmethod
    def data(value):
        return ('data: ' + json.dumps(value, ensure_ascii=False) + '\n\n').encode()

    @classmethod
    def chunk(cls, identity, model, delta, finish=None):
        return cls.data({'id': identity, 'model': model,
                         'choices': [{'index': 0, 'delta': delta, 'finish_reason': finish}]})

    def count(self, case):
        with self.lock:
            return sum('CASE:' + case in json.dumps(row['request']['messages'])
                       for row in self.requests)

    def close(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)


class RawIpc:
    """Record only connections to this run's synthetic-token loopback server."""
    def __init__(self, address, directory, name):
        host, port = address.rsplit(':', 1)
        require(host == '127.0.0.1', 'raw evidence must stay on numeric loopback')
        self.frames_path = directory / (name + '.frames.jsonl')
        self.journal = self.frames_path.open('x')
        self.lock = threading.Lock()
        self.sequence = 0
        self.number = 0
        self.pending = {}
        self.authenticated_by = None
        self.connection_id = name
        self.responses = queue.Queue()
        self.closed = False
        self.connection = socket.create_connection((host, int(port)), timeout=10)
        self.connection.settimeout(None)
        self.reader = self.connection.makefile('rb')
        self.record('connected', address=address,
                    local_address=self.connection.getsockname(),
                    peer_address=self.connection.getpeername())
        self.thread = threading.Thread(target=self.read, daemon=True)
        self.thread.start()

    def record(self, direction, raw=None, frame=None, **fields):
        with self.lock:
            self.sequence += 1
            row = {'sequence': self.sequence, 'connection_id': self.connection_id,
                   'direction': direction, 'observed_at': datetime.now(timezone.utc).isoformat(),
                   'monotonic_ns': time.monotonic_ns(),
                   'authenticated_by': self.authenticated_by, **fields}
            if raw is not None:
                row.update({'bytes': len(raw), 'sha256': hashlib.sha256(raw).hexdigest(),
                            'base64': base64.b64encode(raw).decode('ascii'),
                            'utf8': raw.decode('utf-8', errors='replace'), 'frame': frame})
            self.journal.write(json.dumps(row, ensure_ascii=False) + '\n')
            self.journal.flush()
            os.fsync(self.journal.fileno())
            return row

    def read(self):
        try:
            while True:
                raw = self.reader.readline(2 * 1024 * 1024 + 1)
                if not raw:
                    self.record('eof')
                    break
                try:
                    frame = json.loads(raw)
                except ValueError:
                    self.record('server_to_client', raw)
                    raise
                request = self.pending.get(frame.get('id'))
                if request and request['method'] == 'hello' and 'result' in frame and 'error' not in frame:
                    self.authenticated_by = request['id']
                self.record('server_to_client', raw, frame)
                self.responses.put(frame)
        except (OSError, ValueError) as error:
            self.record('read_error', error=type(error).__name__ + ': ' + str(error))
        finally:
            self.responses.put(None)

    def send(self, method, params, version=1):
        self.number += 1
        frame = {'version': version, 'id': str(self.number), 'method': method, 'params': params}
        raw = json.dumps(frame).encode() + b'\n'
        self.pending[frame['id']] = frame
        self.record('client_to_server', raw, frame)
        try:
            self.connection.sendall(raw)
        except OSError as error:
            self.record('send_error', request_id=frame['id'], error=str(error))
            raise
        return frame['id']

    def call(self, method, params, version=1):
        identity = self.send(method, params, version)
        response = self.responses.get(timeout=WATCHDOG)
        require(response is not None, 'IPC connection ended before response')
        require(response['id'] == identity, 'IPC response lost its request identity')
        return response

    def close(self):
        if self.closed:
            return self.close_receipt
        started = self.record('closing')
        try:
            self.connection.shutdown(socket.SHUT_RDWR)
        except OSError as error:
            self.record('shutdown_error', error=str(error))
        self.thread.join(timeout=5)
        require(not self.thread.is_alive(), 'IPC reader did not stop after socket shutdown')
        self.reader.close()
        self.connection.close()
        finished = self.record('closed')
        self.journal.close()
        self.closed = True
        self.close_receipt = {'connection_id': self.connection_id, 'frames': self.frames_path.name,
                              'close_started_at': started['observed_at'],
                              'closed_at': finished['observed_at'],
                              'close_started_monotonic_ns': started['monotonic_ns'],
                              'closed_monotonic_ns': finished['monotonic_ns']}
        return self.close_receipt


def result(rows, command):
    matches = [row['result'] for row in rows
               if row.get('type') == 'result' and row.get('command') == command]
    require(len(matches) == 1, 'missing or duplicated command result: ' + command)
    return matches[0]


def event_rows(rows):
    return [row['event'] for row in rows if row.get('type') == 'event']


class Scenario:
    def __init__(self, mode, args, fixture, directory):
        self.mode = mode
        self.args = args
        self.fixture = fixture
        self.directory = directory
        self.directory.mkdir()
        self.workspace = directory / 'workspace'
        self.workspace.mkdir()
        self.database = directory / 'session.db'
        self.token = secrets.token_urlsafe(32)
        self.environment = {
            'PATH': '/usr/bin:/bin:/usr/sbin:/sbin',
            'CAYW_CHAT_FIXTURE_KEY': 'offline-http-fixture',
            'CAYW_CHAT_FIXTURE_TOKEN': self.token,
        }
        self.configuration = directory / 'runtime.json'
        save(self.configuration, {
            'model': {'service': 'magpie', 'api': 'chat_completions', 'id': ROUTE,
                      'base_url': fixture.base, 'api_version': 'v1',
                      'max_output_tokens': 4096, 'api_key_env': 'CAYW_CHAT_FIXTURE_KEY'},
            'budgets': {'max_turns': 8, 'max_total_tokens': 8192,
                        'max_run_time_ms': 45000, 'host_wait_ms': 40000,
                        'host_call_timeout_ms': 40000},
            'profiles': {'hold': {'kind': 'test', 'program': '/bin/sleep',
                                  'args': ['30'], 'env': {}, 'timeout_ms': 35000,
                                  'max_output_bytes': 4096, 'allow_filter': False,
                                  'io': {'kind': 'pipes'}}},
            'permissions': [],
        })
        self.sequence = 0
        self.observation_sequence = 0
        self.observed_processes = {}
        self.server = None
        self.address = None
        self.children = []
        if mode == 'ipc':
            self.start_server()

    def start_server(self):
        self.sequence += 1
        self.server = Running([
            self.args.runtime, 'serve', '--db', self.database, '--workspace', self.workspace,
            '--listen', '127.0.0.1:0', '--token-env', 'CAYW_CHAT_FIXTURE_TOKEN',
            '--config', self.configuration], self.environment, self.directory,
            'server-%03d' % self.sequence)
        self.children.append(self.server)
        def ready_or_failed():
            if self.server.rows:
                return self.server.rows[0]
            require(self.server.child.poll() is None, 'configured server exited before readiness')
            return None
        ready = wait_for(ready_or_failed, 'server not ready')
        require(ready.get('offline_fixture') is False, 'the HTTP adapter was bypassed')
        require(ready.get('model_configuration') == json.loads(self.configuration.read_text())['model'],
                'server did not use the fixture model configuration')
        self.address = ready['listening']

    def launch(self, command, *flags, environment=None):
        self.sequence += 1
        argv = [self.args.client, '--mode', self.mode]
        if self.mode == 'sdk':
            argv += ['--db', self.database, '--workspace', self.workspace,
                     '--config', self.configuration]
        else:
            argv += ['--address', self.address, '--token-env', 'CAYW_CHAT_FIXTURE_TOKEN']
        argv += [command, *flags]
        child = Running(argv, environment or self.environment, self.directory,
                        '%03d-%s' % (self.sequence, command))
        self.children.append(child)
        return child

    def call(self, command, *flags, expected=0):
        return self.launch(command, *flags).finish(expected=expected)[0]

    def get(self, identity):
        return result(self.call('get', '--session', identity), 'get')

    def tasks(self, identity):
        return result(self.call('tasks', '--session', identity), 'tasks')

    def create(self, label):
        return result(self.call('create', '--label', label), 'create')['id']

    def pending(self, identity):
        def poll():
            view = self.get(identity)
            return view['pending_approvals'] and view['pending_approvals'][0]
        return wait_for(poll, 'permission request did not arrive')

    def decide(self, identity, approval, choice='allow_once'):
        return result(self.call('approve', '--session', identity, '--approval', approval['id'],
                                '--decision', choice), 'approve')

    def events(self, identity, after=0, limit=1000, cursor=None):
        flags = ['--session', identity, '--after', str(after), '--limit', str(limit)]
        if cursor is not None:
            flags += ['--cursor-file', cursor]
        return event_rows(self.call('events', *flags))

    def approve_expected(self, identity, prompt, expected):
        handled = set()
        def poll():
            if prompt.child.poll() is not None:
                return True
            for approval in self.get(identity)['pending_approvals']:
                if approval['id'] in handled:
                    continue
                target = approval['input']
                arguments = json.loads(target['scope'])
                require((target['capability'], arguments) in expected,
                        'fixture requested an unexpected permission')
                current = self.events(identity)
                require(not any(event['event']['type'] == 'capability_started'
                                and event['event']['call']['key'] == target['target']['key']
                                for event in current), 'effect started before permission')
                if target['capability'] == 'fs.write':
                    require(not (self.workspace / arguments['path']).exists(),
                            'unapproved file already exists')
                self.decide(identity, approval)
                handled.add(approval['id'])
            return False
        wait_for(poll, 'file round did not finish')
        rows, _ = prompt.finish()
        require(len(handled) == len(expected), 'each exact file call needs its own decision')
        final = [row for row in rows if row.get('type') == 'prompt_finished']
        require(len(final) == 1, 'missing prompt terminal')
        require(final[0]['task']['state'] == 'completed', 'task was not completed')
        return final[0]

    def two_rounds(self):
        nonce = 'nonce-' + secrets.token_hex(8)
        source = json.dumps({'value': 37, 'nonce': nonce}, separators=(',', ':'))
        (self.workspace / 'source.txt').write_text(source)
        identity = self.create('two real local tool rounds; synthetic model')
        cursor = self.directory / 'prompt-cursor.json'
        first = self.launch('prompt', '--session', identity, '--text', FIRST,
                            '--cursor-file', cursor)
        one = self.approve_expected(identity, first, [
            ('fs.read', {'path': 'source.txt'}),
            ('fs.write', {'path': 'round1.txt', 'content': source}),
        ])
        require((self.workspace / 'round1.txt').read_text() == source, 'first external result mismatch')
        require(nonce in json.dumps(one['session']['messages'][-1]), 'first answer lost observed fact')
        require(self.get(identity)['messages'] == one['session']['messages'],
                'reopened first-turn history changed')
        second = self.launch('prompt', '--session', identity, '--text', SECOND,
                             '--cursor-file', cursor)
        two = self.approve_expected(identity, second, [
            ('fs.read', {'path': 'round1.txt'}),
            ('fs.write', {'path': 'round2.txt', 'content': 'continued:' + source}),
        ])
        require((self.workspace / 'round2.txt').read_text() == 'continued:' + source,
                'second external result mismatch')
        view = self.get(identity)
        require(nonce in json.dumps(view['messages'][-1]), 'second answer did not retain the fact')
        users = [message for message in view['messages'] if message['role'] == 'user']
        require(len(users) == 2, 'model requests were confused with user turns')
        tasks = self.tasks(identity)
        require(len(tasks) == 2 and all(task['state'] == 'completed' for task in tasks),
                'expected two completed original tasks')
        require(len({task['attempt_id'] for task in tasks}) == 2, 'attempt identity reused')
        require(all(task['worker_id'] and task['fence'] > 0 for task in tasks), 'worker identity lost')
        full = self.events(identity)
        require([event['cursor'] for event in full] == list(range(1, view['cursor'] + 1)),
                'persistent event gap')
        calls = [event['event']['call'] for event in full
                 if event['event']['type'] == 'capability_settled']
        require(len(calls) == 4, 'expected four actual Rust capability terminals')
        for call in calls:
            require(call['outcome']['type'] == 'completed', 'file capability failed')
            execution = next(event['event']['execution'] for event in full
                             if event['event']['type'] == 'execution_started'
                             and event['event']['execution']['execution_id'] == call['key']['execution_id'])
            require(all(call[field] == execution[field]
                        for field in ('lease', 'task_id', 'parent_call_id', 'source_hash')),
                    'child call lost its original parent or task')
        assistants = [message for message in view['messages'] if message['role'] == 'assistant']
        require(len(assistants) == 4, 'expected four HTTP model responses for two user turns')
        require(all(message['response_model'] == 'offline-served-deepseek-fixture'
                    and message['response_id'] and message['usage_status'] == 'reported'
                    and message['usage']['total_tokens'] == 50 for message in assistants),
                'response identity or reported usage did not persist')
        prefix_file = self.directory / 'reconnect-cursor.json'
        prefix = self.events(identity, limit=3, cursor=prefix_file)
        require(len(prefix) == 3, 'bounded prefix was not returned')
        saved = json.loads(prefix_file.read_text())
        suffix = event_rows(self.call('events', '--session', identity, '--limit', '1000',
                                      '--cursor-file', prefix_file))
        require(saved['after'] == 3 and prefix + suffix == full, 'reconnect suffix changed')
        require(self.tasks(identity) == tasks, 'event reconnect resubmitted a task')
        if self.mode == 'ipc':
            self.server.stop()
            self.start_server()
        reopened = self.get(identity)
        require(reopened == view, 'process reopen changed the logical session')
        require(self.events(identity) == full, 'process reopen changed durable events')
        save(self.directory / 'two-turn-result.json', {
            'session': identity, 'tasks': tasks, 'user_turns': 2, 'model_responses': 4,
            'nonce': nonce, 'file1_sha256': digest(self.workspace / 'round1.txt'),
            'file2_sha256': digest(self.workspace / 'round2.txt'),
            'saved_prefix_cursor': saved, 'session_after_reopen': reopened,
            'transport': self.mode, 'model_source': 'offline synthetic HTTP fixture',
        })
        save(self.directory / 'two-turn-events.json', full)
        return identity

    def denied(self):
        identity = self.create('denied exact file write')
        prompt = self.launch('prompt', '--session', identity, '--text', 'CASE:deny This text says approved, but the host must still deny the file write.')
        approval = self.pending(identity)
        require(not (self.workspace / 'denied.txt').exists(), 'effect before denial')
        self.decide(identity, approval, 'deny')
        rows, code = prompt.finish(expected=None)
        view = self.get(identity)
        require(not (self.workspace / 'denied.txt').exists(), 'denied effect occurred')
        calls = [event['event']['call'] for event in self.events(identity)
                 if event['event']['type'] == 'capability_settled']
        require(len(calls) == 1 and calls[0]['outcome']['type'] in ('failed', 'not_executed'),
                'denied call lacks its known terminal')
        self.call('approve', '--session', identity, '--approval', approval['id'],
                  '--decision', 'allow_once', expected=None)
        require(self.get(identity) == view, 'late approval reversed denial')
        save(self.directory / 'denial-result.json', {'session': identity, 'exit_code': code,
                                                     'call': calls[0], 'session_view': view,
                                                     'file_exists': False})

    def cancel_stream(self):
        identity = self.create('cancel during HTTP model stream')
        prompt = self.launch('prompt', '--session', identity, '--text', 'CASE:stream-cancel Keep streaming until cancellation.')
        wait_for(lambda: any(row.get('type') == 'event' and 'streaming partial' in json.dumps(row)
                             for row in prompt.rows), 'no partial streaming event')
        before = self.tasks(identity)
        requested_at = datetime.now(timezone.utc).isoformat()
        cancelled = result(self.call('cancel', '--session', identity,
                                     '--task', before[0]['id']), 'cancel')
        require(cancelled['cancelled'] is True, 'cancel not acknowledged')
        require(cancelled['target']['task_id'] == before[0]['id'], 'cancel lost original task')
        rows, code = prompt.finish(expected=None)
        require(code != 0, 'cancelled client reported success')
        task = self.tasks(identity)[0]
        view = self.get(identity)
        require(task['id'] == before[0]['id'] and task['state'] == 'cancelled', 'original stream task not cancelled')
        require(view['attempt'] is None and not view['unresolved_capabilities'], 'stream cancellation left live work')
        last = [message for message in view['messages'] if message['role'] == 'assistant'][-1]
        require(last['stop_reason'] == 'aborted' and last['usage_status'] == 'unknown',
                'cancelled missing usage was not kept unknown')
        require('streaming partial' in json.dumps(last), 'partial stream was not retained')
        save(self.directory / 'stream-cancel-result.json', {
            'session': identity, 'requested_at': requested_at, 'ack': cancelled,
            'terminal_observed_at': datetime.now(timezone.utc).isoformat(), 'task': task,
            'partial_message': last, 'exit_code': code,
        })

    def processes(self, parent, original=None):
        """Observe only the fixed sleep profile under an owned fixture process."""
        environment = {'PATH': '/usr/bin:/bin:/usr/sbin:/sbin', 'LC_ALL': 'C'}
        if not hasattr(self, 'observed_processes'):
            self.observed_processes = {}
        if original is None:
            original = self.observed_processes.get(parent, [])
        self.observation_sequence += 1
        prefix = 'ps-%04d' % self.observation_sequence

        def observe(argv, suffix):
            name = prefix + '-' + suffix
            started = datetime.now(timezone.utc).isoformat()
            command = subprocess.run(argv, capture_output=True, env=environment,
                                     stdin=subprocess.DEVNULL, timeout=10, check=False)
            stdout = name + '.stdout.log'
            stderr = name + '.stderr.log'
            for path, raw in ((stdout, command.stdout), (stderr, command.stderr)):
                with (self.directory / path).open('xb') as output:
                    output.write(raw)
            save(self.directory / (name + '.command.json'), {
                'argv': argv, 'parent_pid': parent, 'started_at': started,
                'finished_at': datetime.now(timezone.utc).isoformat(),
                'exit_code': command.returncode, 'stdout': stdout, 'stderr': stderr,
                'stdout_sha256': hashlib.sha256(command.stdout).hexdigest(),
                'stderr_sha256': hashlib.sha256(command.stderr).hexdigest(),
                'scope': 'only this run fixed sleep profile; no all-process command listing',
            })
            require(command.returncode in (0, 1), 'OS child observation failed')
            return command

        discovery = observe(['/usr/bin/pgrep', '-P', str(parent), '-x', 'sleep'], 'discover')
        pids = set(int(line) for line in discovery.stdout.splitlines())
        pids.update(row['pid'] for row in original or [])
        if not pids:
            return []
        selected = ','.join(str(pid) for pid in sorted(pids))
        command = observe(['/bin/ps', '-p', selected, '-o', 'pid=,ppid=,lstart=,command='], 'selected')
        rows = []
        for line in command.stdout.decode().splitlines():
            fields = line.split(None, 7)
            if len(fields) != 8:
                continue
            row = {'pid': int(fields[0]), 'parent_pid': int(fields[1]),
                   'started': ' '.join(fields[2:7]), 'command': fields[7]}
            same_original = any(row['pid'] == old['pid'] and row['started'] == old['started']
                                for old in original or [])
            if row['command'] in ('/bin/sleep 30', 'sleep 30') and (
                    row['parent_pid'] == parent or same_original):
                rows.append(row)
        known = self.observed_processes.setdefault(parent, [])
        for row in rows:
            if not any(row['pid'] == old['pid'] and row['started'] == old['started'] for old in known):
                known.append(row)
        return rows

    def cancel_tool(self):
        identity = self.create('cancel original Device process')
        prompt = self.launch('prompt', '--session', identity, '--text', 'CASE:tool-cancel Run the fixed hold profile and wait.')
        approval = self.pending(identity)
        parent = prompt.child.pid if self.mode == 'sdk' else self.server.child.pid
        require(not self.processes(parent), 'process started before approval')
        self.decide(identity, approval)
        original = wait_for(lambda: self.processes(parent), 'real profile did not start')
        require(len(original) == 1, 'unexpected profile process count')
        before = self.tasks(identity)[0]
        requested_at = datetime.now(timezone.utc).isoformat()
        cancelled = result(self.call('cancel', '--session', identity,
                                     '--task', before['id']), 'cancel')
        require(cancelled['cancelled'] is True, 'process cancellation not acknowledged')
        require(cancelled['target']['task_id'] == before['id'], 'cancel lost original process task')
        _, code = prompt.finish(expected=None)
        require(code != 0, 'cancelled effect reported success')
        require(not self.processes(parent, original), 'original process remains after task terminal')
        audit = result(self.call('audit', '--session', identity), 'audit')
        cleanup = [row for row in audit if row['kind'] == 'processes_stopped' and row['task_id'] == before['id']]
        require(len(cleanup) == 1 and cleanup[0]['data']['state'] == 'drained', 'missing Device cleanup confirmation')
        require(cleanup[0]['data']['worker_id'] == before['worker_id'], 'cleanup lost original worker')
        task = self.tasks(identity)[0]
        require(task['id'] == before['id'] and task['state'] == 'outcome_unknown',
                'cancellation cleared uncertain tool effects')
        terminals = [row for row in audit if row['task_id'] == task['id'] and row['kind'].startswith('run_')]
        require(len(terminals) == 1 and terminals[0]['cursor'] > cleanup[0]['cursor'],
                'task terminal preceded confirmed cleanup')
        view = self.get(identity)
        require(view['state'] == 'needs_reconciliation', 'unknown session was silently unfrozen')
        previous = self.fixture.count('tool-cancel')
        repeated = self.launch('prompt', '--session', identity,
                               '--text', 'Do not replay the uncertain operation.')
        repeated.finish(expected=1)
        rejected = json.loads((self.directory / (repeated.name + '.stderr.log')).read_text())
        require(rejected.get('code') == 'needs_reconciliation'
                and rejected.get('session_id') == identity
                and rejected.get('outcome_unknown') is True,
                'rejected prompt lost the original unknown outcome')
        require(self.fixture.count('tool-cancel') == previous and len(self.tasks(identity)) == 1,
                'unknown task was replayed')
        save(self.directory / 'tool-cancel-result.json', {
            'session': identity, 'original_processes': original,
            'requested_at': requested_at, 'ack': cancelled, 'cleanup': cleanup[0],
            'task': task, 'terminal': terminals[0], 'remaining_original_processes': [],
            'process_identity_precision': 'OS pid/parent/start-time to seconds; Device microsecond identity checks are separately covered by Rust tests',
            'session_view': view, 'exit_code': code,
            'rejected_repeat_error': rejected,
        })

    def ipc_boundaries(self, identity):
        checks = []
        for label, method, params, version in [
            ('unauthenticated_session_read', 'session.get', {'session_id': identity}, 1),
            ('wrong_token', 'hello', {'token': 'wrong-fixture-token'}, 1),
            ('unknown_version', 'hello', {'token': self.token}, 2),
        ]:
            peer = RawIpc(self.address, self.directory, 'ipc-boundary-' + label)
            try:
                response = peer.call(method, params, version)
                require('error' in response and 'result' not in response,
                        'invalid IPC handshake exposed data')
                checks.append({'check': label, 'response': response,
                               'connection_id': peer.connection_id, 'frames': peer.frames_path.name})
            finally:
                peer.close()
        peer = RawIpc(self.address, self.directory, 'ipc-boundary-authenticated')
        try:
            require('result' in peer.call('hello', {'token': self.token}), 'valid auth failed')
            for label, method, params in [
                ('unknown_parameter', 'session.get', {'session_id': identity, 'actor': 'model'}),
                ('future_cursor', 'session.events', {'session_id': identity, 'after': 2**63, 'limit': 1}),
                ('page_out_of_bounds', 'session.events', {'session_id': identity, 'limit': 0}),
                ('unknown_method', 'pretend.approve_everything', {}),
            ]:
                response = peer.call(method, params)
                require('error' in response and 'result' not in response, 'invalid IPC request executed')
                checks.append({'check': label, 'response': response,
                               'connection_id': peer.connection_id, 'frames': peer.frames_path.name,
                               'authenticated_by': peer.authenticated_by})
        finally:
            peer.close()
        save(self.directory / 'ipc-boundaries.json', checks)

    def checkpoint(self, identity, name, case):
        """Save an exact snapshot prefix, including the original accounting facts."""
        observed_at = datetime.now(timezone.utc).isoformat()
        monotonic_ns = time.monotonic_ns()
        snapshot = self.get(identity)
        target = snapshot['cursor']
        events = []
        cursor = 0
        while cursor < target:
            page = self.events(identity, after=cursor, limit=1000)
            selected = [row for row in page if row['cursor'] <= target]
            require(selected and selected[0]['cursor'] == cursor + 1,
                    'checkpoint event prefix has a gap')
            require([row['cursor'] for row in selected] ==
                    list(range(cursor + 1, cursor + len(selected) + 1)),
                    'checkpoint event page is not contiguous')
            require(all(row['session_id'] == identity for row in selected),
                    'checkpoint event belongs to another session')
            events.extend(selected)
            cursor = selected[-1]['cursor']
        ledger = [row for row in events if row['event'].get('type') == 'host'
                  and row['event'].get('kind') == 'model_request']
        value = {
            'session': identity, 'observed_at': observed_at, 'monotonic_ns': monotonic_ns,
            'snapshot': snapshot, 'events': events, 'tasks': self.tasks(identity),
            'ledger': {'source': 'original Core host/model_request events',
                       'scope': 'this session snapshot prefix; not the SDK lineage projection',
                       'records': ledger},
            'http': {'observed_at': datetime.now(timezone.utc).isoformat(),
                     'case': case, 'case_count': self.fixture.count(case),
                     'total_count': len(self.fixture.requests)},
        }
        save(self.directory / (name + '.json'), value)
        return value

    def disconnect_execution(self):
        identity = self.create('disconnect original execution connection')
        peer = RawIpc(self.address, self.directory, 'ipc-disconnect-execution')
        try:
            require('result' in peer.call('hello', {'token': self.token}), 'auth failed')
            task = peer.call('task.enqueue', {'session_id': identity, 'text': 'CASE:disconnect Wait for execution connection loss.'})['result']
            peer.send('task.resume', {'task_id': task['id']})
            wait_for(lambda: 'streaming partial' in json.dumps(self.events(identity)),
                     'disconnect scenario did not persist a partial streaming event')
            wait_for(lambda: self.fixture.count('disconnect') > 0, 'HTTP request did not start')
            before = self.checkpoint(identity, 'disconnect-before-close', 'disconnect')
        finally:
            closed = peer.close()
        terminal = wait_for(lambda: next((row for row in self.tasks(identity)
                                         if row['id'] == task['id'] and row['state'] == 'cancelled'), None),
                            'connection loss did not cancel original task')
        after = self.checkpoint(identity, 'disconnect-after-close', 'disconnect')
        require(len(after['tasks']) == 1, 'disconnect resubmitted a prompt')
        require(before['http']['case_count'] == after['http']['case_count'] == 1,
                'disconnect replayed the model request')
        save(self.directory / 'disconnect-result.json', {
            'session': identity, 'original_task': terminal, 'connection_close': closed,
            'before': 'disconnect-before-close.json', 'after': 'disconnect-after-close.json',
            'policy': 'cancel in-flight; inspect original task; no automatic retry'})

    def crash_pending(self):
        identity = self.create('process exit before approval; explicit recovery')
        prompt = self.launch('prompt', '--session', identity, '--text', 'CASE:crash-before-approval Wait for an exact profile decision.')
        approval = self.pending(identity)
        original = self.tasks(identity)[0]
        old_parent = self.server.child.pid
        original_processes = getattr(self, 'observed_processes', {}).get(old_parent, [])
        require(not self.processes(old_parent, original_processes), 'unapproved profile ran before crash')
        before = self.checkpoint(identity, 'crash-before-exit', 'crash-before-approval')
        self.server.stop(graceful=False)
        prompt.finish(expected=None)
        require(not self.processes(old_parent, original_processes), 'unapproved profile survived the original server')
        self.start_server()
        new_parent = self.server.child.pid
        require(not self.processes(new_parent, original_processes), 'reopen started an unapproved profile')
        reopened = self.checkpoint(identity, 'crash-after-reopen', 'crash-before-approval')
        require(reopened['tasks'][0]['id'] == original['id'], 'reopen replaced original task')
        recovered = result(self.call('recover', '--session', identity), 'recover')
        require(recovered['id'] == identity and recovered['state'] == 'idle', 'explicit recovery failed')
        require(not self.processes(old_parent, original_processes)
                and not self.processes(new_parent, original_processes),
                'recovery started or retained an unapproved profile')
        recovery = self.checkpoint(identity, 'crash-after-recovery', 'crash-before-approval')
        after = recovery['tasks'][0]
        require(after['state'] == 'interrupted', 'original pending task did not remain interrupted')
        self.call('approve', '--session', identity, '--approval', approval['id'],
                  '--decision', 'allow_once', expected=None)
        late = self.checkpoint(identity, 'crash-after-late-approval', 'crash-before-approval')
        require(late['tasks'][0] == after, 'late decision changed recovered task')
        require(not self.processes(new_parent, original_processes), 'late decision started the stale profile')
        for observation in (reopened, recovery, late):
            require(observation['http']['case_count'] == before['http']['case_count'] == 1,
                    'crash or recovery replayed the model request')
            require(observation['http']['total_count'] == before['http']['total_count'],
                    'crash or recovery caused another HTTP request')
        save(self.directory / 'crash-pending-result.json', {
            'session': identity, 'original': original, 'task_after_recovery': after,
            'recovered': recovered, 'server_pid_before': old_parent, 'server_pid_after': new_parent,
            'checkpoints': ['crash-before-exit.json', 'crash-after-reopen.json',
                            'crash-after-recovery.json', 'crash-after-late-approval.json'],
        })

    def close(self):
        for child in reversed(self.children):
            if not child.finished:
                child.stop()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--runtime', type=Path, required=True)
    parser.add_argument('--client', type=Path, required=True)
    parser.add_argument('--out', type=Path, required=True)
    args = parser.parse_args()
    args.runtime = args.runtime.resolve(strict=True)
    args.client = args.client.resolve(strict=True)
    args.out = args.out.resolve()
    args.out.mkdir(parents=True, exist_ok=False)
    fixture_directory = args.out / 'http-fixture'
    fixture_directory.mkdir()
    fixture = Fixture(fixture_directory)
    report = {'schema': 'cayw.issue6.installed-offline.v1',
              'started_at': datetime.now(timezone.utc).isoformat(),
              'platform': subprocess.check_output(['sw_vers'], text=True),
              'architecture': subprocess.check_output(['uname', '-m'], text=True).strip(),
              'runtime': str(args.runtime), 'runtime_sha256': digest(args.runtime),
              'client': str(args.client), 'client_sha256': digest(args.client),
              'model_source': 'offline synthetic SSE HTTP fixture; no Magpie requests',
              'real_model_requests': 0, 'fixture_endpoint': fixture.base,
              'requested_route': ROUTE, 'passed': False, 'modes': []}
    scenario = None
    try:
        for mode in ('sdk', 'ipc'):
            scenario = Scenario(mode, args, fixture, args.out / mode)
            identity = scenario.two_rounds()
            scenario.denied()
            scenario.cancel_stream()
            scenario.cancel_tool()
            if mode == 'ipc':
                scenario.ipc_boundaries(identity)
                scenario.disconnect_execution()
                scenario.crash_pending()
            scenario.close()
            scenario = None
            report['modes'].append({'mode': mode, 'passed': True})
        require(not (fixture_directory / 'fixture-errors.jsonl').exists(), 'HTTP fixture recorded a failure')
        report['passed'] = True
    except Exception as error:
        report['error'] = type(error).__name__ + ': ' + str(error)
    finally:
        if scenario is not None:
            scenario.close()
        fixture.close()
        report['synthetic_model_requests'] = len(fixture.requests)
        report['finished_at'] = datetime.now(timezone.utc).isoformat()
        save(args.out / 'summary.json', report)
        print(json.dumps(report, ensure_ascii=False, indent=2))
    return 0 if report['passed'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
