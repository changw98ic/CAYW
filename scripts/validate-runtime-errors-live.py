#!/usr/bin/env python3
"""Live SDK/IPC normal path, Core write checks, storage errors and worker panic.

Run with ``python3 -I``. Real model calls go through a loopback relay that forwards
upstream bytes without retry or invented replies. Only MAGPIE_API_KEY from the
caller environment is used; when it is absent on loopback the documented public
Magpie application id is injected into child environments only. Credential values
and authentication headers are never printed or saved.
"""
from __future__ import annotations

import argparse, gzip, hashlib, http.client, json, os, platform, secrets, signal
import subprocess, sys, threading, time
from datetime import datetime, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.error import HTTPError
from urllib.parse import urlsplit
from urllib.request import HTTPRedirectHandler, Request, build_opener

WATCHDOG, PROMPT_WATCHDOG, PROBE_WATCHDOG = 30, 240, 300
MAX_POSTS, MAX_REQUEST, MAX_RESPONSE = 32, 16 << 20, 64 << 20
NORMAL_TOKENS = PROBE_TOKENS = 1_000_000
ALLOCATION_CAP = 10_000_000
PUBLIC_APP_ID = 'magpie-cayw-runtime-validation'
TOKEN_ENV = 'CAYW_RUNTIME_LIVE_IPC_TOKEN'
PROMPT = ('Complete this file task in one execute_code cell, then answer the nonce from its '
          'result. Use action=exec, yield_time_ms=10000, max_output_tokens=4096 and this script: '
          'local r = cayw.call("fs.read", {path="source.txt"}); '
          'local w = cayw.call("fs.write", {path="proof.txt", content=r.content}); '
          'return {source=r.content, written=w}. '
          'The host has pre-authorized only those exact fs.read and fs.write arguments. '
          'Keep variables local to this cell. Do not use store, load or another capability. '
          'If the cell is still running, wait on its original cell_id and next_cursor until '
          'terminal without replaying exec. Finally answer the nonce contained in source.txt.')
HOPS = {'connection', 'transfer-encoding', 'keep-alive', 'content-length', 'proxy-connection'}
SECRETS = {'authorization', 'proxy-authorization', 'x-api-key', 'cookie', 'set-cookie'}


def need(condition, reason):
    if not condition:
        raise AssertionError(reason)


def now():
    return datetime.now(timezone.utc).isoformat()


def save(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open('x') as output:
        json.dump(value, output, ensure_ascii=False, indent=2)
        output.write('\n')
        output.flush()
        os.fsync(output.fileno())


def saveb(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open('xb') as output:
        output.write(value)
        output.flush()
        os.fsync(output.fileno())


def sha(value):
    return hashlib.sha256(value).hexdigest()


def shaf(path):
    return sha(path.read_bytes())


def last_json(text):
    for line in reversed(text.splitlines()):
        if line.strip():
            try:
                return json.loads(line)
            except ValueError:
                pass
    raise AssertionError('probe stdout has no JSON summary')


def clean(headers):
    return {k.lower(): v for k, v in headers.items() if k.lower() not in SECRETS}


def auth_present(headers):
    return any(k.lower() in SECRETS for k in headers)


def wait_for(check, reason, seconds=WATCHDOG):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        value = check()
        if value:
            return value
        time.sleep(0.02)
    raise TimeoutError(reason)


def find_key(value, key):
    if isinstance(value, dict):
        if key in value:
            return value[key]
        for child in value.values():
            found = find_key(child, key)
            if found is not None:
                return found
    elif isinstance(value, list):
        for child in value:
            found = find_key(child, key)
            if found is not None:
                return found
    return None


def find_values(value, key, found):
    if isinstance(value, dict):
        for name, child in value.items():
            if name == key:
                found.append(child)
            find_values(child, key, found)
    elif isinstance(value, list):
        for child in value:
            find_values(child, key, found)
    return found


def minimal_env(credential=None):
    env = {name: os.environ[name] for name in ('PATH', 'HOME', 'TMPDIR', 'LANG', 'LC_ALL')
           if name in os.environ}
    env.setdefault('PATH', '/usr/bin:/bin:/usr/sbin:/sbin')
    env['NO_COLOR'] = '1'
    if credential is not None:
        env['MAGPIE_API_KEY'] = credential
    return env


class Running:
    """Capture one owned process, including full stdout/stderr and JSONL rows."""

    def __init__(self, argv, env, directory, name):
        self.argv = [str(v) for v in argv]
        self.dir, self.name, self.rows = directory, name, []
        self.outp, self.errp = directory / (name + '.stdout.log'), directory / (name + '.stderr.log')
        self.outf, self.errf = self.outp.open('x'), self.errp.open('x')
        self.started = now()
        self.proc = subprocess.Popen(self.argv, env=env, stdin=subprocess.DEVNULL,
                                     stdout=subprocess.PIPE, stderr=self.errf,
                                     text=True, bufsize=1)
        self.pid, self.done = self.proc.pid, False
        self.thread = threading.Thread(target=self.read, daemon=True)
        self.thread.start()

    def read(self):
        try:
            for line in self.proc.stdout:
                self.outf.write(line)
                self.outf.flush()
                try:
                    row = json.loads(line)
                except ValueError:
                    row = {'invalid_jsonl': line.rstrip('\n')}
                self.rows.append(row)
        except Exception:
            pass
        finally:
            try:
                self.proc.stdout.close()
            except OSError:
                pass

    def finish(self, expected=0, timeout=WATCHDOG):
        timed = False
        try:
            code = self.proc.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            timed = True
            self.proc.kill()
            code = self.proc.wait(timeout=15)
        if self.done:
            return self.rows, code
        self.thread.join(timeout=5)
        if self.thread.is_alive():
            try:
                self.proc.stdout.close()
            except OSError:
                pass
            self.thread.join(timeout=5)
        need(not self.thread.is_alive(), self.name + ': stdout reader did not finish')
        self.outf.close()
        self.errf.close()
        stdout, stderr = self.outp.read_bytes(), self.errp.read_bytes()
        save(self.dir / (self.name + '.command.json'), {
            'argv': self.argv, 'pid': self.pid, 'started_at': self.started,
            'finished_at': now(), 'exit_code': code, 'timed_out': timed,
            'stdout': self.outp.name, 'stderr': self.errp.name,
            'stdout_bytes': len(stdout), 'stdout_sha256': sha(stdout),
            'stderr_bytes': len(stderr), 'stderr_sha256': sha(stderr),
            'environment_policy': 'minimal allowlist; secret values are never recorded'})
        self.done = True
        need(not timed, self.name + ': process timed out')
        if expected is not None:
            need(code == expected, self.name + ': exit ' + str(code))
        need(not any('invalid_jsonl' in row for row in self.rows),
             self.name + ': non-JSONL output')
        return self.rows, code

    def stop(self, timeout=30):
        if self.proc.poll() is None:
            self.proc.send_signal(signal.SIGINT)
            try:
                self.proc.wait(timeout=timeout)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait(timeout=15)
        return self.finish(expected=None, timeout=15)


class NoRedirect(HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def http_get(url, headers=None):
    started = now()
    try:
        with build_opener(NoRedirect).open(Request(url, headers=headers or {}),
                                            timeout=30) as response:
            body, status, response_headers = response.read(MAX_RESPONSE + 1), response.status, dict(response.headers)
    except HTTPError as error:
        body, status, response_headers = error.read(), error.code, dict(error.headers or {})
    except Exception as error:
        return ({'url': url, 'started_at': started, 'finished_at': now(),
                 'error': type(error).__name__ + ': ' + str(error),
                 'authorization_sent': bool(headers and 'Authorization' in headers)}, b'')
    return ({'url': url, 'started_at': started, 'finished_at': now(), 'status': status,
             'headers': clean(response_headers),
             'authorization_sent': bool(headers and 'Authorization' in headers),
             'body_bytes': len(body), 'body_sha256': sha(body),
             'body_truncated': len(body) > MAX_RESPONSE}, body)


def parse_usage(body, content_type):
    usage, model, response_id, ended = {}, None, None, False
    try:
        if 'text/event-stream' in content_type.lower():
            for line in body.splitlines():
                if not line.startswith(b'data:'):
                    continue
                payload = line[5:].strip()
                if payload == b'[DONE]':
                    ended = True
                    continue
                try:
                    value = json.loads(payload)
                except ValueError:
                    continue
                model = value.get('model') or model
                response_id = value.get('id') or response_id
                if isinstance(value.get('usage'), dict):
                    usage.update(value['usage'])
        elif body:
            value = json.loads(body)
            model, response_id = value.get('model'), value.get('id')
            if isinstance(value.get('usage'), dict):
                usage = value['usage']
            ended = True
    except Exception:
        return usage, model, response_id, False, None, False
    prompt = usage.get('prompt_tokens', usage.get('input_tokens'))
    completion = usage.get('completion_tokens', usage.get('output_tokens'))
    total = usage.get('total_tokens')
    if ended and isinstance(prompt, int) and isinstance(completion, int):
        return (usage, model, response_id, True,
                total if isinstance(total, int) else prompt + completion, True)
    return usage, model, response_id, False, None, ended


class Relay:
    """Transparent loopback HTTP relay with complete evidence and a POST guard."""

    def __init__(self, upstream_base, directory, credential):
        directory.mkdir(parents=True, exist_ok=True)
        self.dir, self.credential = directory, credential
        self.up = urlsplit(upstream_base)
        self.base_path = self.up.path.rstrip('/')
        self.entries, self.refusals, self.posts, self.lock = [], [], 0, threading.Lock()
        owner = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = 'HTTP/1.1'

            def log_message(self, *_):
                pass

            def do_GET(self):
                owner.forward(self, 'GET')

            def do_POST(self):
                owner.forward(self, 'POST')

        self.server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.base = 'http://127.0.0.1:%d/v1' % self.server.server_port

    def post_count(self):
        with self.lock:
            return self.posts

    def entries_since(self, index):
        with self.lock:
            return list(self.entries[index:])

    def await_posts(self, start, expected, timeout=60):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            with self.lock:
                posts = [entry for entry in self.entries if entry.get('method') == 'POST']
                window = posts[start:]
                if len(window) >= expected and all('complete' in entry
                                                   for entry in window[:expected]):
                    return window[:expected]
            time.sleep(0.02)
        raise TimeoutError('relay POST evidence did not finalize')

    def target(self, path):
        return self.base_path + (path[3:] if path.startswith('/v1') else path)

    def respond_error(self, handler, status, message):
        body = json.dumps({'error': {'message': message}}).encode()
        handler.send_response(status)
        handler.send_header('Content-Type', 'application/json')
        handler.send_header('Content-Length', str(len(body)))
        handler.send_header('Connection', 'close')
        handler.end_headers()
        handler.close_connection = True
        try:
            handler.wfile.write(body)
            handler.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass

    def forward(self, handler, method):
        started = now()
        length = int(handler.headers.get('Content-Length', '0') or '0')
        if length > MAX_REQUEST:
            self.respond_error(handler, 413, 'relay request body limit')
            return
        body = handler.rfile.read(length) if length else b''
        with self.lock:
            number = len(self.entries) + len(self.refusals) + 1
            if method == 'POST':
                if self.posts >= MAX_POSTS:
                    refusal = {'number': number, 'method': method, 'client_path': handler.path,
                               'relay_refused': True, 'reason': 'actual POST hard guard',
                               'started_at': started, 'finished_at': now()}
                    self.refusals.append(refusal)
                    save(self.dir / ('%04d.refused.json' % number), refusal)
                    self.respond_error(handler, 429, 'relay actual POST limit reached')
                    return
                self.posts += 1
        prefix, target = '%04d' % number, self.target(handler.path)
        entry = {'number': number, 'method': method, 'client_path': handler.path,
                 'upstream_path': target, 'request_headers': clean(dict(handler.headers)),
                 'authorization_present': auth_present(handler.headers),
                 'request_bytes': len(body), 'request_sha256': sha(body),
                 'started_at': started, 'complete': False}
        try:
            saveb(self.dir / (prefix + '.request.body'), body)
        except OSError as error:
            entry['evidence_error'] = str(error)
        headers = {k.lower(): v for k, v in handler.headers.items()
                   if k.lower() not in HOPS and k.lower() not in ('accept-encoding', 'host')}
        headers.update({'accept-encoding': 'identity', 'host': self.up.netloc})
        if body:
            headers['content-length'] = str(len(body))
        if not any(k.lower() == 'authorization' for k in headers):
            headers['Authorization'] = 'Bearer ' + self.credential
        connection, response, received = None, None, bytearray()
        content_type, expected_length, error_text, status = '', None, None, 502
        response_headers, downstream_closed, sent = {}, False, False
        try:
            cls = http.client.HTTPSConnection if self.up.scheme == 'https' else http.client.HTTPConnection
            connection = cls(self.up.hostname, self.up.port or (443 if self.up.scheme == 'https' else 80),
                             timeout=300)
            connection.request(method, target, body=body or None, headers=headers)
            response = connection.getresponse()
            status, content_type = response.status, response.getheader('Content-Type', '')
            expected_length = response.getheader('Content-Length')
            response_headers = clean(dict(response.getheaders()))
            handler.send_response(status)
            for key, value in response.getheaders():
                if key.lower() not in HOPS:
                    handler.send_header(key, value)
            if expected_length is not None:
                handler.send_header('Content-Length', expected_length)
            handler.send_header('Connection', 'close')
            handler.end_headers()
            handler.close_connection = sent = True
            while True:
                chunk = response.read1(65536)
                if not chunk:
                    break
                received.extend(chunk)
                if len(received) > MAX_RESPONSE:
                    error_text = 'relay response body limit'
                    break
                if not downstream_closed:
                    try:
                        handler.wfile.write(chunk)
                        handler.wfile.flush()
                    except (BrokenPipeError, ConnectionResetError):
                        downstream_closed = True
        except Exception as error:
            error_text = type(error).__name__ + ': ' + str(error)
            if not sent:
                try:
                    self.respond_error(handler, 502, 'relay upstream failed')
                except OSError:
                    pass
        finally:
            if response is not None:
                response.close()
            if connection is not None:
                connection.close()
        raw = bytes(received)
        complete = error_text is None and len(raw) < MAX_RESPONSE
        if expected_length is not None:
            try:
                complete = complete and len(raw) == int(expected_length)
            except ValueError:
                complete = False
        parsed = raw
        if response_headers.get('content-encoding', '').lower() == 'gzip':
            try:
                parsed = gzip.decompress(parsed)
            except OSError:
                complete = False
        usage, model, response_id, usage_ok, usage_total, ended = parse_usage(parsed, content_type)
        if not ended:
            complete = False
        if not complete:
            usage_ok, usage_total = False, None
        entry.update({'finished_at': now(), 'response_status': status,
                      'response_headers': response_headers, 'response_bytes': len(raw),
                      'response_sha256': sha(raw), 'complete': complete, 'error': error_text,
                      'model': model, 'response_id': response_id, 'usage': usage,
                      'usage_status': 'reported' if usage_ok else ('partial' if usage else 'unknown'),
                      'usage_total': usage_total,
                      'downstream_closed_early': downstream_closed})
        try:
            saveb(self.dir / (prefix + '.response.body'), raw)
            save(self.dir / (prefix + '.json'), entry)
        except OSError as error:
            entry['evidence_error'] = str(error)
        with self.lock:
            self.entries.append(entry)

    def summary(self):
        with self.lock:
            entries = list(self.entries)
        posts = [entry for entry in entries if entry.get('method') == 'POST']
        reported = [entry for entry in posts if entry.get('usage_status') == 'reported']
        return {'base': self.base, 'upstream': self.up.geturl(),
                'actual_posts': len(posts), 'max_actual_posts': MAX_POSTS,
                'refused_posts': len(self.refusals), 'requests': entries,
                'reported_usage_sum': sum(entry['usage_total'] for entry in reported),
                'unknown_count': sum(1 for entry in posts
                                     if entry.get('usage_status') != 'reported')}

    def close(self):
        if getattr(self, 'closed', False):
            return
        try:
            self.await_posts(0, self.post_count(), timeout=30)
        except TimeoutError:
            pass
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)
        save(self.dir / 'summary.json', self.summary())
        self.closed = True


class Normal:
    def __init__(self, mode, args, relay, credential):
        self.mode, self.args, self.relay, self.credential = mode, args, relay, credential
        self.dir = args.out / (mode + '-normal')
        self.dir.mkdir()
        self.workspace = self.dir / 'workspace'
        self.workspace.mkdir()
        self.db, self.config_path = self.dir / 'session.db', self.dir / 'runtime.json'
        self.token = secrets.token_urlsafe(32)
        self.nonce = 'nonce-' + secrets.token_hex(8)
        self.source = json.dumps({'value': secrets.randbelow(900000) + 100000,
                                  'nonce': self.nonce}, separators=(',', ':'))
        (self.workspace / 'source.txt').write_text(self.source)
        self.config = {
            'model': {'service': 'magpie', 'api': 'chat_completions', 'id': args.model,
                      'base_url': relay.base, 'api_version': 'v1',
                      'max_output_tokens': 4096, 'api_key_env': 'MAGPIE_API_KEY'},
            'budgets': {'max_total_tokens': NORMAL_TOKENS, 'max_turns': 4,
                        'max_run_time_ms': 180000, 'host_wait_ms': 170000,
                        'host_call_timeout_ms': 170000},
            'permissions': [
                {'capability': 'fs.read', 'arguments': {'path': 'source.txt'}},
                {'capability': 'fs.write',
                 'arguments': {'path': 'proof.txt', 'content': self.source}}]}
        save(self.config_path, self.config)
        self.seq = self.server_no = 0
        self.server = self.address = None
        self.children, self.identity = [], None
        self.session_view = self.task_list = self.event_list = None
        self.post_start = self.post_end = self.post_entries = None
        self.ev = {'schema': 'cayw.runtime-errors-live.normal.v1', 'mode': mode,
                   'workspace': str(self.workspace), 'database': str(self.db),
                   'config': str(self.config_path), 'nonce': self.nonce,
                   'source_sha256': sha(self.source.encode()), 'source_bytes': len(self.source),
                   'prompt_text': PROMPT, 'permissions': self.config['permissions'],
                   'credential_source': args.credential_source}

    def env(self, credentials=False):
        env = minimal_env(self.credential if credentials else None)
        if self.mode == 'ipc':
            env[TOKEN_ENV] = self.token
        return env

    def start_server(self, credentials=True):
        self.server_no += 1
        self.server = Running([self.args.runtime, 'serve', '--db', self.db,
                               '--workspace', self.workspace, '--listen', '127.0.0.1:0',
                               '--token-env', TOKEN_ENV, '--config', self.config_path],
                              self.env(credentials), self.dir, 'server-%03d' % self.server_no)
        self.children.append(self.server)

        def ready():
            if self.server.rows:
                return self.server.rows[0]
            need(self.server.proc.poll() is None, 'IPC server exited before readiness')
            return None

        row = wait_for(ready, 'IPC server not ready')
        conf = row.get('configuration') or {}
        need(row.get('offline_fixture') is False, 'IPC server used offline mode')
        need(row.get('model_configuration') == self.config['model'],
             'IPC server did not use the exact model configuration')
        need(conf.get('max_total_tokens') == NORMAL_TOKENS, 'IPC token budget changed')
        need(conf.get('max_turns') == 4 and conf.get('max_run_time_ms') == 180000,
             'IPC run budget changed')
        need(conf.get('exact_permission_count') == 2, 'IPC permission count changed')
        self.address = row['listening']
        self.ev['server_ready_%d' % self.server_no] = {
            'listening': self.address, 'model': row.get('model'),
            'model_configuration': row.get('model_configuration'), 'configuration': conf}

    def stop_server(self):
        if self.server is None:
            return
        rows, code = self.server.stop()
        need(self.server.proc.poll() is not None, 'IPC server did not exit')
        self.ev['server_exit_%d' % self.server_no] = {'exit_code': code, 'rows': rows}
        self.server = self.address = None

    def launch(self, command, *flags, credentials=False):
        self.seq += 1
        argv = [self.args.client, '--mode', self.mode]
        argv += (['--db', self.db, '--workspace', self.workspace, '--config', self.config_path]
                 if self.mode == 'sdk' else ['--address', self.address, '--token-env', TOKEN_ENV])
        argv += [command, *flags]
        child = Running(argv, self.env(credentials), self.dir, '%03d-%s' % (self.seq, command))
        self.children.append(child)
        return child

    def call(self, command, *flags, expected=0, credentials=False, timeout=WATCHDOG):
        return self.launch(command, *flags, credentials=credentials).finish(expected, timeout)

    @staticmethod
    def result(rows, command):
        matches = [row['result'] for row in rows
                   if row.get('type') == 'result' and row.get('command') == command]
        need(len(matches) == 1, 'missing or duplicated result: ' + command)
        return matches[0]

    def create(self):
        return self.result(self.call('create', '--label', self.mode + ' normal real model')[0],
                           'create')['id']

    def get(self):
        return self.result(self.call('get', '--session', self.identity)[0], 'get')

    def tasks(self):
        return self.result(self.call('tasks', '--session', self.identity)[0], 'tasks')

    def events_page(self, cursor_path, after=None):
        flags = ['--session', self.identity, '--limit', '1000', '--cursor-file', str(cursor_path)]
        if after is not None:
            flags += ['--after', str(after)]
        rows, _ = self.call('events', *flags)
        summary = self.result(rows, 'events')
        carried = [row['event'] for row in rows if row.get('type') == 'event']
        need(summary['count'] == len(carried), 'event page count differs from emitted rows')
        return carried, summary

    def all_events(self, name):
        cursor_path, records, first, last = self.dir / (name + '.cursor.json'), [], True, None
        while True:
            page, last = self.events_page(cursor_path, 0 if first else None)
            first = False
            records.extend(page)
            if last['count'] == 0:
                break
        need(json.loads(cursor_path.read_text())['after'] == last['after'],
             'saved cursor differs from the last event page')
        return records

    def prompt(self):
        cursor_path = self.dir / 'prompt.cursor.json'
        rows, _ = self.launch('prompt', '--session', self.identity, '--text', PROMPT,
                              '--cursor-file', str(cursor_path), credentials=True).finish(
            0, PROMPT_WATCHDOG)
        finished = [row for row in rows if row.get('type') == 'prompt_finished']
        need(len(finished) == 1, 'prompt has no unique terminal')
        need(not [row for row in rows if row.get('type') == 'error'], 'prompt emitted an error row')
        need(finished[0]['task']['state'] == 'completed', 'prompt task was not completed')
        need(json.loads(cursor_path.read_text())['after'] == finished[0]['after'],
             'prompt cursor and terminal differ')
        return finished[0]

    def verify(self, terminal):
        tasks = self.tasks()
        need(len(tasks) == 1 and tasks[0]['state'] == 'completed', 'Task is not one Completed Task')
        task = tasks[0]
        need(task.get('attempt_id') and task.get('worker_id') and task['fence'] > 0,
             'Task lost original attempt, worker or fence')
        need(terminal['task']['id'] == task['id'], 'terminal belongs to another Task')
        need(terminal['task'].get('attempt_id') == task['attempt_id'],
             'terminal lost the original Attempt')
        view, events = self.get(), self.all_events('events')
        need(events, 'event log is empty')
        need(view['cursor'] == events[-1]['cursor'], 'Session cursor differs from events')
        need([event['cursor'] for event in events] == list(range(1, view['cursor'] + 1)),
             'durable event cursor has a gap')
        need(all(event['session_id'] == self.identity for event in events),
             'event belongs to another Session')
        need(not view.get('pending_approvals'), 'pre-authorized run requested approval')
        need(not view.get('unresolved_capabilities'), 'run left unresolved capabilities')
        assistants = [m for m in view['messages'] if m.get('role') == 'assistant']
        results = [m for m in view['messages'] if m.get('role') == 'toolResult']
        calls = [part for m in assistants for part in m.get('content', [])
                 if part.get('type') == 'toolCall']
        need(calls, 'model made no Luau tool call')
        need(all(call.get('name') == 'execute_code' for call in calls),
             'model used a tool other than Luau execute_code')
        ids = [call.get('id') for call in calls]
        need(len(set(ids)) == len(ids), 'tool call identity repeated')
        need(set(m.get('tool_call_id') for m in results) == set(ids),
             'tool result pairing differs from tool calls')
        need(all(not m.get('is_error') and not m.get('outcome_unknown') for m in results),
             'tool result failed or is unknown')
        scripts = ' '.join(json.dumps(call.get('arguments')) for call in calls)
        need(all(token in scripts for token in ('fs.read', 'fs.write', 'source.txt', 'proof.txt')),
             'Luau script did not use the exact file capabilities and paths')
        need(self.nonce in json.dumps(results), 'tool results did not contain the source nonce')
        final_text = '\n'.join(part.get('text', '') for part in assistants[-1].get('content', [])
                               if part.get('type') == 'text')
        need(final_text.strip(), 'final assistant content has no text')
        need(self.nonce in final_text, 'final assistant content did not answer the nonce')
        settled_calls = [e['event']['call'] for e in events
                         if e['event'].get('type') == 'capability_settled']
        started_keys = {json.dumps(e['event']['call']['key'], sort_keys=True) for e in events
                        if e['event'].get('type') == 'capability_started'}
        need(len(settled_calls) >= 2, 'fewer than two capability terminals')
        expected_capabilities = {
            ('fs.read', json.dumps({'path': 'source.txt'}, sort_keys=True)),
            ('fs.write', json.dumps({'path': 'proof.txt', 'content': self.source},
                                    sort_keys=True))}
        observed_capabilities = set()
        for call in settled_calls:
            need(json.dumps(call['key'], sort_keys=True) in started_keys,
                 'capability terminal has no original start')
            need(call.get('stage') == 'settled', 'capability record is not settled')
            need((call.get('outcome') or {}).get('type') == 'completed',
                 'capability did not complete')
            need(call.get('task_id') == task['id'], 'capability lost the original Task')
            lease = call.get('lease') or {}
            need((lease.get('attempt_id'), lease.get('worker_id'), lease.get('fence')) ==
                 (task['attempt_id'], task['worker_id'], task['fence']),
                 'capability lost original Attempt, worker or fence')
            observed_capabilities.add(
                (call.get('name'), json.dumps(call.get('arguments'), sort_keys=True)))
        need(observed_capabilities == expected_capabilities,
             'capability calls differ from the exact pre-authorized calls')
        proof = self.workspace / 'proof.txt'
        need(proof.is_file(), 'proof.txt was not created')
        need(proof.read_text() == self.source, 'proof.txt content is not the exact source')
        self.session_view, self.task_list, self.event_list = view, tasks, events
        self.ev.update({'session_id': self.identity, 'task': task, 'event_count': len(events),
                        'events_sha256': sha(json.dumps(events, sort_keys=True).encode()),
                        'messages_count': len(view['messages']),
                        'assistant_responses': len(assistants), 'tool_calls': calls,
                        'tool_results': len(results), 'capability_terminals': len(settled_calls),
                        'proof_sha256': shaf(proof), 'proof_bytes': proof.stat().st_size})
        save(self.dir / 'session.json', view)
        save(self.dir / 'tasks.json', tasks)
        save(self.dir / 'events.json', events)

    def verify_usage(self, entries):
        assistants = [m for m in self.session_view['messages'] if m.get('role') == 'assistant']
        need(len(assistants) == len(entries), 'assistant responses differ from relay POSTs')
        need(all(e['complete'] and e['usage_status'] == 'reported' for e in entries),
             'relay saw an incomplete or unreported normal response')
        for message, entry in zip(assistants, entries):
            need(entry.get('model') and message.get('response_model') == entry.get('model'),
                 'persisted response model differs from relay model')
            need(entry.get('response_id') and message.get('response_id') == entry.get('response_id'),
                 'persisted response id differs from relay id')
            need(message.get('usage_status') == 'reported' and entry['usage_total'] > 0,
                 'normal usage was not reported')
            need(message['usage']['total_tokens'] == entry['usage_total'],
                 'persisted usage differs from relay response usage')
        self.post_entries = entries
        self.ev['model_usage'] = {
            'responses': len(assistants),
            'response_models': [m.get('response_model') for m in assistants],
            'usage_total_sum': sum(e['usage_total'] for e in entries),
            'relay_request_numbers': [e['number'] for e in entries],
            'relay_usage_status': [e['usage_status'] for e in entries]}

    def verify_reopen(self):
        if self.mode == 'ipc':
            self.stop_server()
            self.start_server(True)
        reopened, tasks, events = self.get(), self.tasks(), self.all_events('events-reopened')
        need(reopened == self.session_view, 'new process changed Session state')
        need(tasks == self.task_list, 'new process changed Task identity')
        need(events == self.event_list, 'new process changed durable events')
        need(self.relay.post_count() == self.post_end, 'reopen submitted another model request')
        if self.mode == 'ipc':
            self.stop_server()
        self.ev['reopen'] = {'mode': self.mode, 'session_equal': True, 'tasks_equal': True,
                             'events_equal': True, 'read_without_credential': True,
                             'relay_posts_unchanged': True}

    def inspect(self, name, entries):
        before, out = self.relay.post_count(), self.dir / (name + '.json')
        summary, payload = run_probe([self.args.probe, '--case', 'inspect', '--db', self.db,
                                      '--workspace', self.workspace, '--config', self.config_path,
                                      '--session', self.identity, '--out', out],
                                     self.env(False), self.dir, name, 120)
        need(self.relay.post_count() == before, 'inspect made a provider request')
        check_summary(summary, 'inspect', 0)
        need(out.is_file(), 'inspect did not save its detailed output')
        verified = check_ledger(payload, entries)
        save(self.dir / (name + '.summary.json'), {
            'summary': summary, 'output': out.name, 'output_sha256': shaf(out),
            'ledger': verified})
        self.ev[name] = verified
        return verified

    def run(self):
        need(self.nonce not in PROMPT, 'nonce leaked into the prompt')
        self.post_start = self.relay.post_count()
        if self.mode == 'ipc':
            self.start_server(True)
        try:
            self.identity = self.create()
            terminal = self.prompt()
            self.verify(terminal)
            self.post_end = self.relay.post_count()
            need(self.post_end > self.post_start, 'normal run made no model request')
            self.verify_usage(self.relay.await_posts(
                self.post_start, self.post_end - self.post_start))
            self.verify_reopen()
            self.inspect('ledger-after-restart', self.post_entries)
            self.ev['passed'] = True
            save(self.dir / 'result.json', self.ev)
        except Exception as error:
            self.ev['error'] = type(error).__name__ + ': ' + str(error)
            save(self.dir / 'error.json', self.ev)
            raise
        finally:
            if self.server is not None:
                try:
                    self.stop_server()
                except Exception:
                    pass

    def close(self):
        for child in reversed(self.children):
            if not child.done:
                try:
                    child.stop()
                except Exception:
                    pass
        self.server = None


def run_probe(argv, env, directory, name, timeout):
    started, timed = now(), False
    try:
        done = subprocess.run(argv, env=env, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                              stderr=subprocess.PIPE, timeout=timeout)
        code, stdout, stderr = done.returncode, done.stdout, done.stderr
    except subprocess.TimeoutExpired as error:
        timed, code, stdout, stderr = True, None, error.stdout or b'', error.stderr or b''
    saveb(directory / (name + '.probe.stdout.log'), stdout)
    saveb(directory / (name + '.probe.stderr.log'), stderr)
    save(directory / (name + '.probe.command.json'), {
        'argv': [str(v) for v in argv], 'started_at': started, 'finished_at': now(),
        'exit_code': code, 'timed_out': timed,
        'stdout': name + '.probe.stdout.log', 'stdout_bytes': len(stdout),
        'stdout_sha256': sha(stdout), 'stderr': name + '.probe.stderr.log',
        'stderr_bytes': len(stderr), 'stderr_sha256': sha(stderr),
        'environment_policy': 'minimal allowlist; credentials only when the case needs a model'})
    need(not timed, name + ': probe timed out')
    need(code == 0, name + ': probe exit ' + str(code) +
         '; see ' + str(directory / (name + '.probe.stderr.log')))
    summary = last_json(stdout.decode('utf-8', errors='replace'))
    out = Path(argv[argv.index('--out') + 1]) if '--out' in argv else None
    payload = {}
    if out is not None and out.is_file():
        try:
            payload = json.loads(out.read_text())
        except ValueError:
            payload = {'invalid_json': out.name}
    need(code == 0, name + ': probe exit ' + str(code))
    return summary, payload


def check_summary(summary, case, calls):
    need(isinstance(summary, dict), case + ': probe summary is not an object')
    need(summary.get('status') == 'ok', case + ': probe status is not ok')
    if 'case' in summary:
        need(summary['case'] == case, case + ': probe case changed')
    need(summary.get('actual_provider_calls') == calls, case + ': provider call count differs')
    need(summary.get('unexpected_provider_calls', 0) == 0,
         case + ': unexpected provider calls')


def terminal_name(record):
    value = record.get('terminal')
    if isinstance(value, dict):
        for name in ('completed', 'failed', 'cancelled', 'dropped', 'missing_terminal',
                     'recording_failed', 'invalid_usage', 'budget_exceeded', 'rejected'):
            if name in value:
                return name
        return next(iter(value)) if len(value) == 1 else value
    return value


def check_ledger(payload, entries):
    ledger, requests = payload.get('model_requests'), None
    if isinstance(ledger, dict) and isinstance(ledger.get('requests'), list):
        ledger, requests = ledger, ledger['requests']
    elif isinstance(ledger, list):
        requests = ledger
    else:
        found = find_key(payload, 'model_requests')
        if isinstance(found, dict) and isinstance(found.get('requests'), list):
            ledger, requests = found, found['requests']
        elif isinstance(found, list):
            requests = found
    need(isinstance(requests, list) and requests, 'inspect output has no model request ledger')
    need(isinstance(ledger, dict), 'inspect ledger is only a request list')
    need(len(requests) == len(entries), 'ledger request count differs from relay POST count')
    by_response = {entry.get('response_id'): entry for entry in entries
                   if entry.get('response_id')}
    need(len(by_response) == len(entries), 'relay entry has no unique response id')
    settled_sum, observations = 0, 0
    used = set()
    for request in requests:
        need(terminal_name(request) == 'completed', 'ledger request did not complete')
        need(request.get('outstanding_reservation_tokens') == 0,
             'ledger request still has an outstanding reservation')
        history = request.get('observations')
        need(isinstance(history, list) and history, 'ledger request has no observations')
        observations += len(history)
        final = history[-1].get('observation') if isinstance(history[-1], dict) else None
        need(isinstance(final, dict), 'ledger final observation is not an object')
        need(final.get('usage_status') == 'reported', 'ledger final usage is not reported')
        response_id, response_model = final.get('response_id'), final.get('response_model')
        need(isinstance(response_id, str) and response_id, 'ledger final response id is missing')
        need(isinstance(response_model, str) and response_model,
             'ledger final response model is missing')
        entry = by_response.get(response_id)
        need(entry is not None, 'ledger response id has no relay request')
        need(response_model == entry.get('model'),
             'ledger response model differs from relay response')
        settled = request.get('settled_tokens')
        need(isinstance(settled, int) and settled > 0, 'ledger request has no settled tokens')
        need(final.get('usage', {}).get('total_tokens') == settled,
             'ledger final usage differs from settled tokens')
        need(entry.get('usage_total') == settled,
             'ledger settled tokens differ from relay response usage')
        used.add(response_id)
        settled_sum += settled
    need(used == set(by_response), 'ledger did not account for every relay request')
    need(ledger.get('budget_unknown') is False, 'ledger budget_unknown is not false')
    need(ledger.get('reserved_tokens') == 0, 'ledger still reserves tokens after completion')
    need(ledger.get('settled_tokens') == settled_sum, 'ledger settled_tokens differs from requests')
    need(settled_sum == sum(entry['usage_total'] for entry in entries),
         'ledger settled tokens differ from relay usage sum')
    return {'requests': len(requests), 'settled_tokens': settled_sum,
            'reserved_tokens': ledger.get('reserved_tokens'),
            'budget_unknown': ledger.get('budget_unknown'), 'observations': observations,
            'relay_response_ids': sorted(used),
            'cost_status': ledger.get('cost_status'), 'cost_amount': ledger.get('cost_amount'),
            'cost_currency': ledger.get('cost_currency')}


def preflight(args, credential):
    directory = args.out / 'preflight'
    directory.mkdir()
    up = urlsplit(args.base_url)
    hello_meta, hello_body = http_get(up.scheme + '://' + up.netloc + '/api/hello')
    models_meta, models_body = http_get(args.base_url + '/models',
                                        {'Authorization': 'Bearer ' + credential})
    save(directory / 'hello.json', hello_meta)
    saveb(directory / 'hello.body', hello_body)
    save(directory / 'models.json', models_meta)
    saveb(directory / 'models.body', models_body)
    result = {'hello': hello_meta, 'models': models_meta, 'credential_source': args.credential_source,
              'requested_model': args.model, 'catalog_hit': False, 'catalog_count': None,
              'catalog_suffix_hit': False}
    try:
        data = json.loads(models_body).get('data') or []
        ids = [item.get('id') for item in data if isinstance(item, dict)]
        result.update({'catalog_count': len(ids), 'catalog_hit': args.model in ids,
                       'catalog_suffix_hit': any(isinstance(name, str) and
                                                 name.split('/')[-1] == args.model.split('/')[-1]
                                                 for name in ids),
                       'catalog_ids_sha256': sha(json.dumps(ids).encode())})
    except ValueError as error:
        result['catalog_error'] = str(error)
    try:
        hello = json.loads(hello_body)
        result.update({'hello_name': hello.get('name'), 'hello_version': hello.get('version')})
    except ValueError:
        result['hello_error'] = 'hello body is not JSON'
    need(hello_meta.get('status') == 200, 'GET /api/hello did not return 200')
    need(models_meta.get('status') == 200, 'GET /v1/models did not return 200')
    need(result['catalog_hit'], 'requested model is absent from /v1/models')
    save(directory / 'summary.json', result)
    return result


def writes_step(mode, normal, args):
    need(normal is not None and normal.identity is not None,
         mode + ': normal database has no verified Session')
    normal.inspect('ledger-before-writes', normal.post_entries)
    before, out = normal.relay.post_count(), normal.dir / 'writes.json'
    summary, payload = run_probe([args.probe, '--case', 'writes', '--db', normal.db,
                                  '--session', normal.identity, '--out', out],
                                 normal.env(False), normal.dir, 'writes', 120)
    need(normal.relay.post_count() == before, 'writes probe made a provider request')
    check_summary(summary, 'writes', 0)
    need(out.is_file(), 'writes probe did not save its detailed output')
    if 'ok' in payload:
        need(payload['ok'] is True, 'writes detailed output is not ok')
    if 'phase' in payload:
        need(payload['phase'] == 'complete', 'writes detailed output is incomplete')
    result = {'mode': mode, 'session': normal.identity, 'summary': summary,
              'output': out.name, 'output_sha256': shaf(out),
              'relay_posts_unchanged': True, 'actual_provider_calls': 0}
    save(normal.dir / 'writes-result.json', result)
    return result


def assistant_text(message):
    return '\n'.join(part.get('text', '') for part in message.get('content', [])
                     if part.get('type') == 'text')


def check_probe_reply(payload, expected):
    view = payload.get('baseline_session') or payload.get('session')
    need(isinstance(view, dict) and isinstance(view.get('messages'), list),
         'probe detailed output has no Session view')
    assistants = [m for m in view['messages'] if m.get('role') == 'assistant']
    need(assistants, 'probe detailed output has no assistant reply')
    message, text = assistants[-1], assistant_text(assistants[-1])
    need(expected in text, 'last assistant content lacks the expected baseline marker')
    need(message.get('usage_status') == 'reported', 'last assistant usage is not reported')
    need(isinstance(message.get('response_id'), str) and message['response_id'],
         'last assistant response_id is missing')
    need(isinstance(message.get('response_model'), str) and message['response_model'],
         'last assistant response_model is missing')
    return message


def reopen_check(case, mode, root, args, relay, payload, entries, session, tasks, events, ledger):
    before, out = relay.post_count(), root / 'reopen-inspect.json'
    summary, reopened = run_probe([args.probe, '--case', 'inspect', '--db', root / 'session.db',
                                   '--workspace', root / 'workspace',
                                   '--config', root / 'runtime.json',
                                   '--session', session['id'], '--out', out],
                                  minimal_env(None), root, case + '-' + mode + '-reopen', 120)
    need(relay.post_count() == before, case + ': reopen inspect made a provider request')
    check_summary(summary, 'inspect', 0)
    need(out.is_file(), case + ': reopen inspect did not save its detailed output')
    need(reopened.get('session') == session, case + ': reopen changed the Session')
    need(reopened.get('tasks') == tasks, case + ': reopen changed Task identity')
    need(reopened.get('events') == events, case + ': reopen changed durable events')
    need(reopened.get('model_requests') == ledger, case + ': reopen changed the request ledger')
    verified = check_ledger(reopened, entries)
    result = {'summary': summary, 'output': out.name, 'output_sha256': shaf(out),
              'session_equal': True, 'tasks_equal': True, 'events_equal': True,
              'model_requests_equal': True, 'relay_posts_unchanged': True,
              'ledger': verified}
    if case == 'panic':
        task = tasks[0]
        need(task.get('state') == 'draining', 'panic Task is no longer draining')
        need(task.get('holds_slot') is True, 'panic Task released its cleanup hold')
        need(reopened['tasks'][0] == task, 'reopen changed the draining panic Task')
        if isinstance(payload.get('health'), dict):
            need(payload['health'].get('running') == 1, 'panic health running count changed')
        result['panic_task_draining'] = True
        result['panic_holds_slot'] = True
    else:
        need(len(payload.get('corruptions') or []) == 2,
             'storage did not retain two corrupted queued rows')
        need(len(payload.get('write_failures') or []) == 2,
             'storage did not retain two write-failure rows')
        need(payload.get('mutated_records_retained') is True,
             'storage did not retain its corrupt children')
        result['corrupt_children_retained'] = True
    save(root / 'reopen-result.json', result)
    return result


def root_probe(case, mode, args, relay, credential):
    root = args.out / (case + '-' + mode)
    root.mkdir()
    workspace, database = root / 'workspace', root / 'session.db'
    workspace.mkdir()
    config = root / 'runtime.json'
    save(config, {
        'model': {'service': 'magpie', 'api': 'chat_completions', 'id': args.model,
                  'base_url': relay.base, 'api_version': 'v1',
                  'max_output_tokens': 4096, 'api_key_env': 'MAGPIE_API_KEY'},
        'budgets': {'max_total_tokens': PROBE_TOKENS, 'max_turns': 1,
                    'max_run_time_ms': 180000, 'host_wait_ms': 170000,
                    'host_call_timeout_ms': 170000}, 'permissions': []})
    before, out = relay.post_count(), root / (case + '.json')
    summary, payload = run_probe([args.probe, '--case', case, '--mode', mode, '--db', database,
                                  '--workspace', workspace, '--config', config, '--out', out],
                                 minimal_env(credential), root, case + '-' + mode, PROBE_WATCHDOG)
    after = relay.post_count()
    need(after - before == 1, case + ': expected exactly one actual provider request')
    entries = relay.await_posts(before, 1)
    need(len(entries) == 1, case + ': relay did not capture the request')
    check_summary(summary, case, 1)
    need(out.is_file(), case + ': probe did not save its detailed output')
    if 'ok' in payload:
        need(payload['ok'] is True, case + ': detailed output is not ok')
    need(entries[0]['complete'], case + ': baseline response is incomplete')
    need(entries[0]['usage_status'] == 'reported', case + ': baseline usage is not reported')
    expected = 'CAYW_REAL_STORAGE_BASELINE_OK' if case == 'storage' else 'CAYW_REAL_WORKER_PANIC_OK'
    check_probe_reply(payload, expected)
    verified = check_ledger(payload, entries)
    session = payload['baseline_session'] if case == 'storage' else payload['session']
    tasks = [payload['baseline_task']] if case == 'storage' else [payload['task']]
    events = payload['baseline_events'] if case == 'storage' else payload['events']
    ledger = payload['model_requests']
    reopen = reopen_check(case, mode, root, args, relay, payload, entries,
                          session, tasks, events, ledger)
    result = {'case': case, 'mode': mode, 'summary': summary, 'output': out.name,
              'output_sha256': shaf(out), 'relay_request': entries[0],
              'relay_posts_delta': after - before, 'ledger': verified,
              'reopen': reopen, 'unknown_response_retained': not entries[0]['complete']}
    save(root / 'result.json', result)
    return result


def resolve_credential(base_url):
    key = os.environ.get('MAGPIE_API_KEY', '').strip()
    if key:
        return key, 'MAGPIE_API_KEY'
    if urlsplit(base_url).hostname in ('127.0.0.1', 'localhost', '::1'):
        return PUBLIC_APP_ID, 'local-public-app-id'
    raise AssertionError('MAGPIE_API_KEY is required for a non-loopback upstream')


REQUIRED_SOURCES = [
    'Cargo.toml', 'Cargo.lock', 'scripts/validate-runtime-errors-live.py',
    'crates/cayw-runtime/Cargo.toml',
    'crates/cayw-runtime/examples/runtime_error_live.rs',
    'crates/cayw-runtime/examples/runtime_error_live/writes.rs',
    'examples/headless-client/Cargo.toml', 'examples/headless-client/Cargo.lock',
    'examples/headless-client/src/main.rs', 'examples/headless-client/src/api.rs',
    'examples/headless-client/src/cursor.rs', 'examples/headless-client/src/options.rs',
    'crates/cayw-runtime/src/error.rs', 'crates/cayw-runtime/src/execution.rs',
    'crates/cayw-core/src/store.rs', 'crates/cayw-queue/src/queue.rs']
OPTIONAL_SOURCES = [
    'crates/cayw-runtime/src/launch.rs', 'crates/cayw-runtime/src/provider.rs',
    'crates/cayw-runtime/src/permissions.rs', 'crates/cayw-runtime/src/main.rs',
    'crates/cayw-runtime/src/lib.rs', 'crates/cayw-runtime/src/model_requests.rs',
    'crates/cayw-runtime/src/queue.rs', 'crates/cayw-runtime/src/session.rs',
    'crates/cayw-core/src/types.rs', 'crates/cayw-ai/src/types.rs']


def capture(argv, timeout=20):
    started = now()
    try:
        done = subprocess.run(argv, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                              stderr=subprocess.PIPE, timeout=timeout, text=True,
                              env={**minimal_env(), 'GIT_TERMINAL_PROMPT': '0',
                                   'GIT_OPTIONAL_LOCKS': '0'})
        return {'argv': argv, 'started_at': started, 'finished_at': now(),
                'exit_code': done.returncode, 'stdout': done.stdout,
                'stderr': done.stderr}, done.returncode
    except Exception as error:
        return {'argv': argv, 'started_at': started, 'finished_at': now(),
                'error': type(error).__name__ + ': ' + str(error)}, None


def read_git_head(root):
    dotgit = root / '.git'
    if dotgit.is_file():
        text = dotgit.read_text().strip()
        if not text.startswith('gitdir:'):
            raise AssertionError('.git file has no gitdir')
        gitdir = Path(text.split(':', 1)[1].strip())
    elif dotgit.is_dir():
        gitdir = dotgit
    else:
        raise AssertionError('no .git entry')
    if not gitdir.is_absolute():
        gitdir = (root / gitdir).resolve()
    head = (gitdir / 'HEAD').read_text().strip()
    if not head.startswith('ref: '):
        return head, 'HEAD'
    reference = head[5:].strip()
    common = gitdir
    commondir = gitdir / 'commondir'
    if commondir.is_file():
        common = Path(commondir.read_text().strip())
        if not common.is_absolute():
            common = (gitdir / common).resolve()
    for directory in (gitdir, common):
        loose = directory / reference
        if loose.is_file():
            return loose.read_text().strip(), str(loose)
    for directory in (gitdir, common):
        packed = directory / 'packed-refs'
        if packed.is_file():
            for line in packed.read_text().splitlines():
                fields = line.split()
                if len(fields) == 2 and fields[1] == reference:
                    return fields[0], 'packed-refs:' + str(packed)
    raise AssertionError('git reference is not present')


def source_manifest(args):
    root = Path(__file__).resolve().parent.parent
    files, missing, optional_missing = [], [], []
    for rel in REQUIRED_SOURCES + OPTIONAL_SOURCES:
        path = root / rel
        if not path.is_file():
            (missing if rel in REQUIRED_SOURCES else optional_missing).append(rel)
            continue
        files.append({'path': rel, 'bytes': path.stat().st_size, 'sha256': shaf(path)})
    binaries = []
    for name, path in (('runtime', args.runtime), ('client', args.client),
                       ('probe', args.probe)):
        binaries.append({'name': name, 'path': str(path), 'bytes': path.stat().st_size,
                         'sha256': shaf(path)})
    git, code = capture(['git', '-C', str(root), 'rev-parse', 'HEAD'])
    head, head_source = None, None
    if code == 0 and git.get('stdout', '').strip():
        head, head_source = git['stdout'].strip(), 'git rev-parse'
    else:
        try:
            head, head_source = read_git_head(root)
        except Exception as error:
            git['head_fallback_error'] = type(error).__name__ + ': ' + str(error)
    status, _ = capture(['git', '-C', str(root), 'status', '--porcelain', '--',
                         *REQUIRED_SOURCES, *OPTIONAL_SOURCES])
    manifest = {'schema': 'cayw.runtime-errors-live.source.v1', 'repo_root': str(root),
                'head': head, 'head_source': head_source, 'head_command': git,
                'git_status': status, 'files': files, 'binaries': binaries,
                'required_missing': missing, 'optional_missing': optional_missing,
                'complete': not missing and head is not None}
    save(args.out / 'source-manifest.json', manifest)
    return manifest


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--runtime', type=Path, required=True)
    parser.add_argument('--client', type=Path, required=True)
    parser.add_argument('--probe', type=Path, required=True)
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--token-budget', type=int, default=100_000_000)
    parser.add_argument('--base-url', default='http://127.0.0.1:3425/v1')
    parser.add_argument('--model', default='opencode-go/deepseek-v4.1-flash')
    args = parser.parse_args()
    args.runtime, args.client, args.probe = (p.resolve(strict=True) for p in
                                             (args.runtime, args.client, args.probe))
    args.out = args.out.resolve()
    args.out.mkdir(parents=True, exist_ok=False)
    args.base_url = args.base_url.rstrip('/')
    up = urlsplit(args.base_url)
    need(up.scheme in ('http', 'https') and up.hostname, 'base URL must be absolute HTTP(S)')
    need(args.token_budget > 0, 'token budget must be positive')
    credential, source = resolve_credential(args.base_url)
    args.credential_source = source
    relay = Relay(args.base_url, args.out / 'relay', credential)
    allocations = [NORMAL_TOKENS, NORMAL_TOKENS, PROBE_TOKENS, PROBE_TOKENS,
                   PROBE_TOKENS, PROBE_TOKENS]
    report = {
        'schema': 'cayw.runtime-errors-live.v1', 'started_at': now(),
        'platform': platform.platform(), 'machine': platform.machine(), 'python': sys.version,
        'runtime': str(args.runtime), 'runtime_sha256': shaf(args.runtime),
        'client': str(args.client), 'client_sha256': shaf(args.client),
        'probe': str(args.probe), 'probe_sha256': shaf(args.probe),
        'base_url': args.base_url, 'model': args.model, 'credential_source': source,
        'credential_printed': False,
        'token_budget': {'user_authorized': args.token_budget,
                         'allocations': [
                             {'scenario': 'normal-sdk', 'tokens': NORMAL_TOKENS},
                             {'scenario': 'normal-ipc', 'tokens': NORMAL_TOKENS},
                             {'scenario': 'storage-sdk', 'tokens': PROBE_TOKENS},
                             {'scenario': 'storage-ipc', 'tokens': PROBE_TOKENS},
                             {'scenario': 'panic-sdk', 'tokens': PROBE_TOKENS},
                             {'scenario': 'panic-ipc', 'tokens': PROBE_TOKENS}],
                         'allocated_sum': sum(allocations),
                         'under_ten_million': sum(allocations) <= ALLOCATION_CAP,
                         'under_user_authorization': sum(allocations) <= args.token_budget,
                         'note': 'writes probes reuse the normal database and allocate no root'},
        'cost': {'status': 'unknown',
                 'note': 'No pricing was verified; token usage is not a cost report.'},
        'scenarios': {}, 'failures': [], 'passed': False}
    need(report['token_budget']['under_ten_million'], 'allocations exceed the ten-million cap')
    need(report['token_budget']['under_user_authorization'],
         'allocations exceed the user authorization')
    try:
        manifest = source_manifest(args)
    except Exception as error:
        manifest = {'complete': False,
                    'error': type(error).__name__ + ': ' + str(error)}
        save(args.out / 'source-manifest-error.json', manifest)
    report['source_manifest'] = {
        'path': 'source-manifest.json', 'head': manifest.get('head'),
        'head_source': manifest.get('head_source'), 'complete': manifest.get('complete'),
        'required_missing': manifest.get('required_missing'),
        'optional_missing': manifest.get('optional_missing'),
        'file_count': len(manifest.get('files') or []),
        'binary_count': len(manifest.get('binaries') or []),
        'error': manifest.get('error')}
    normals = {}
    try:
        if not manifest.get('complete'):
            report['failures'].append(
                'source manifest incomplete: ' +
                json.dumps({'required_missing': manifest.get('required_missing'),
                            'head': manifest.get('head'), 'error': manifest.get('error')}))
        else:
            try:
                report['preflight'] = preflight(args, credential)
            except Exception as error:
                report['failures'].append('preflight: ' + type(error).__name__ + ': ' +
                                          str(error))
                report['preflight'] = {'error': type(error).__name__ + ': ' + str(error)}
        if not report['failures']:
            for mode in ('sdk', 'ipc'):
                normal = Normal(mode, args, relay, credential)
                normals[mode] = normal
                try:
                    normal.run()
                    report['scenarios']['normal-' + mode] = {
                        'passed': True, 'result': str(normal.dir / 'result.json'),
                        'session': normal.identity,
                        'usage_total': normal.ev['model_usage']['usage_total_sum'],
                        'relay_requests': normal.ev['model_usage']['relay_request_numbers']}
                except Exception as error:
                    report['failures'].append('normal-' + mode + ': ' +
                                              type(error).__name__ + ': ' + str(error))
                    report['scenarios']['normal-' + mode] = {
                        'passed': False, 'session': normal.identity,
                        'error': type(error).__name__ + ': ' + str(error)}
                    break
                finally:
                    normal.close()
            for mode in ('sdk', 'ipc') if not report['failures'] else ():
                try:
                    report['scenarios']['writes-' + mode] = writes_step(mode, normals.get(mode), args)
                except Exception as error:
                    report['failures'].append('writes-' + mode + ': ' +
                                              type(error).__name__ + ': ' + str(error))
                    report['scenarios']['writes-' + mode] = {
                        'passed': False, 'error': type(error).__name__ + ': ' + str(error)}
            for case in ('storage', 'panic'):
                if report['failures']:
                    break
                for mode in ('sdk', 'ipc'):
                    name = case + '-' + mode
                    try:
                        report['scenarios'][name] = root_probe(case, mode, args, relay, credential)
                    except Exception as error:
                        report['failures'].append(name + ': ' + type(error).__name__ +
                                                  ': ' + str(error))
                        report['scenarios'][name] = {
                            'passed': False, 'error': type(error).__name__ + ': ' + str(error)}
                        break
    finally:
        relay.close()
        summary = relay.summary()
        report['relay'] = {key: summary[key] for key in
                           ('base', 'upstream', 'actual_posts', 'max_actual_posts',
                            'refused_posts', 'reported_usage_sum', 'unknown_count')}
        report['finished_at'] = now()
        report['passed'] = not report['failures']
        save(args.out / 'summary.json', report)
        print(json.dumps(report, ensure_ascii=False, indent=2))
    return 0 if report['passed'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
