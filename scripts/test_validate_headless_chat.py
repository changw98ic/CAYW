#!/usr/bin/env python3
"""Local tests for the evidence recorder. No installed binaries or live services."""
import copy
import hashlib
import http.client
import importlib.util
import json
import os
from pathlib import Path
import socketserver
import subprocess
import sys
import tempfile
import threading
import unittest

SCRIPT = Path(__file__).with_name('validate-headless-chat.py')
SPEC = importlib.util.spec_from_file_location('headless_validation', SCRIPT)
validation = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(validation)


class EvidenceTests(unittest.TestCase):
    def test_ipc_boundaries_preserve_exact_frames_and_hello_association(self):
        # A dropped recorder write must fail even if the protocol check still passes.
        received = []
        class Handler(socketserver.StreamRequestHandler):
            def handle(self):
                connection = []
                received.append(connection)
                authenticated = False
                for raw in self.rfile:
                    frame = json.loads(raw)
                    connection.append(raw.decode())
                    method = frame['method']
                    params = frame['params']
                    response = {'version': 1, 'id': frame['id']}
                    if frame['version'] != 1:
                        code = 'unsupported_version'
                    elif method == 'hello' and params['token'] == 'synthetic-token':
                        authenticated = True
                        response['result'] = {'authenticated': True}
                        code = None
                    elif method == 'hello':
                        code = 'unauthorized'
                    elif not authenticated:
                        code = 'handshake_required'
                    elif 'actor' in params:
                        code = 'invalid_params'
                    elif method == 'pretend.approve_everything':
                        code = 'method_not_found'
                    else:
                        code = 'session_error'
                    if code:
                        response['error'] = {'code': code, 'message': 'synthetic boundary'}
                    self.wfile.write(json.dumps(response).encode() + b'\n')
                    self.wfile.flush()
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            server = socketserver.ThreadingTCPServer(('127.0.0.1', 0), Handler)
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                scenario = validation.Scenario.__new__(validation.Scenario)
                scenario.address = '127.0.0.1:%d' % server.server_address[1]
                scenario.token = 'synthetic-token'
                scenario.directory = directory
                scenario.ipc_boundaries('synthetic-session')
            finally:
                server.shutdown()
                server.server_close()
                thread.join(timeout=5)
            files = sorted(directory.glob('ipc-boundary-*.frames.jsonl'))
            self.assertEqual(len(files), 4, 'every connection needs its original frame journal')
            sent = []
            for path in files:
                rows = [json.loads(line) for line in path.read_text().splitlines()]
                self.assertEqual([r['sequence'] for r in rows], list(range(1, len(rows) + 1)))
                self.assertEqual(rows[0]['direction'], 'connected')
                self.assertEqual(rows[-1]['direction'], 'closed')
                pending = {}
                auth = None
                for row in rows:
                    self.assertTrue(row['observed_at'])
                    if row['direction'] not in ('client_to_server', 'server_to_client'):
                        continue
                    raw = row['utf8'].encode()
                    self.assertEqual(row['bytes'], len(raw))
                    self.assertEqual(row['sha256'], hashlib.sha256(raw).hexdigest())
                    self.assertEqual(row['frame'], json.loads(raw))
                    frame = row['frame']
                    if row['direction'] == 'client_to_server':
                        pending[frame['id']] = frame
                        self.assertEqual(row['authenticated_by'], auth)
                        sent.append(row['utf8'])
                    else:
                        request = pending.pop(frame['id'])
                        if request['method'] == 'hello' and 'result' in frame:
                            auth = frame['id']
                        self.assertEqual(row['authenticated_by'], auth)
                self.assertFalse(pending)
            self.assertEqual(sorted(sent), sorted(raw for connection in received for raw in connection))
            checks = json.loads((directory / 'ipc-boundaries.json').read_text())
            self.assertEqual(len(checks), 7)
            self.assertTrue(all(check['frames'] in [p.name for p in files] for check in checks))

    def test_process_observations_save_only_owned_children_with_raw_exit_status(self):
        # Removing the OS receipts must fail although the parsed process list is correct.
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            child = subprocess.Popen(['/bin/sleep', '30'], env={'PATH': '/usr/bin:/bin'})
            try:
                scenario = validation.Scenario.__new__(validation.Scenario)
                scenario.directory = directory
                scenario.observation_sequence = 0
                rows = scenario.processes(os.getpid())
                self.assertEqual([row['pid'] for row in rows], [child.pid])
                records = sorted(directory.glob('ps-*.command.json'))
                self.assertEqual(len(records), 2, 'save child discovery and selected ps command receipts')
                ps = next(json.loads(p.read_text()) for p in records
                          if json.loads(p.read_text())['argv'][0] == '/bin/ps')
                self.assertEqual(ps['exit_code'], 0)
                self.assertEqual(ps['argv'][1:3], ['-p', str(child.pid)])
                raw = (directory / ps['stdout']).read_text()
                self.assertEqual([int(line.split()[0]) for line in raw.splitlines()], [child.pid])
                self.assertIn('/bin/sleep 30', raw)
                self.assertEqual(hashlib.sha256(raw.encode()).hexdigest(), ps['stdout_sha256'])
            finally:
                child.terminate()
                child.wait(timeout=5)

    def test_cancel_detects_original_sleep_after_its_parent_exits(self):
        # Omitting original PID/start-time at the terminal check must fail here.
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            parent = subprocess.Popen([
                sys.executable, '-I', '-c',
                "import subprocess,sys; sys.stdin.readline(); "
                "p=subprocess.Popen(['/bin/sleep','30'],env={'PATH':'/usr/bin:/bin'}); "
                "print(p.pid,flush=True); sys.stdin.readline()"],
                stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            state = {'started_pid': None, 'ended': False, 'launches': 0}
            task = {'id': 'synthetic-original-task', 'worker_id': 'synthetic-worker'}
            class Prompt:
                child = parent
                def finish(self, expected=None):
                    parent.stdin.write('exit\n')
                    parent.stdin.flush()
                    parent.wait(timeout=5)
                    state['ended'] = True
                    return [], 1
            class Repeat:
                name = 'synthetic-repeat'
                def finish(self, expected=None):
                    return [], 1
            class Fixture:
                def count(self, case):
                    return 0
            class Scenario(validation.Scenario):
                def create(self, label):
                    return 'synthetic-session'
                def launch(self, *args):
                    state['launches'] += 1
                    if state['launches'] == 1:
                        return Prompt()
                    validation.save(directory / 'synthetic-repeat.stderr.log', {
                        'code': 'needs_reconciliation', 'session_id': 'synthetic-session',
                        'outcome_unknown': True})
                    return Repeat()
                def pending(self, session):
                    return {'id': 'synthetic-approval'}
                def decide(self, *args):
                    parent.stdin.write('start\n')
                    parent.stdin.flush()
                    state['started_pid'] = int(parent.stdout.readline())
                def tasks(self, session):
                    return [dict(task, state='outcome_unknown' if state['ended'] else 'running')]
                def get(self, session):
                    return {'state': 'needs_reconciliation'}
                def call(self, command, *args, expected=0):
                    if command == 'cancel':
                        value = {'cancelled': True, 'target': {'task_id': task['id']}}
                    else:
                        value = [{'kind': 'processes_stopped', 'task_id': task['id'], 'cursor': 1,
                                  'data': {'state': 'drained', 'worker_id': task['worker_id']}},
                                 {'kind': 'run_outcome_unknown', 'task_id': task['id'], 'cursor': 2}]
                    return [{'type': 'result', 'command': command, 'result': value}]
            scenario = Scenario.__new__(Scenario)
            scenario.directory = directory
            scenario.mode = 'sdk'
            scenario.observation_sequence = 0
            scenario.fixture = Fixture()
            try:
                with self.assertRaisesRegex(AssertionError, 'original process remains after task terminal'):
                    scenario.cancel_tool()
                self.assertIsNotNone(parent.poll())
                self.assertIsNotNone(state['started_pid'])
                os.kill(state['started_pid'], 0)
                receipts = [json.loads(path.read_text()) for path in sorted(directory.glob('ps-*.command.json'))]
                last = next(row for row in reversed(receipts)
                            if row['argv'][:3] == ['/bin/ps', '-p', str(state['started_pid'])])
                raw = (directory / last['stdout']).read_text()
                fields = raw.split(None, 7)
                self.assertEqual(int(fields[0]), state['started_pid'])
                self.assertNotEqual(int(fields[1]), parent.pid)
            finally:
                if state['started_pid']:
                    try:
                        os.kill(state['started_pid'], 15)
                    except ProcessLookupError:
                        pass
                if parent.poll() is None:
                    parent.terminate()
                    parent.wait(timeout=5)
                parent.stdin.close()
                parent.stdout.close()
                parent.stderr.close()

    def test_crash_rechecks_previously_observed_process_identity(self):
        # A live original process with a changed parent must fail the crash check.
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            parent = subprocess.Popen([
                sys.executable, '-I', '-c',
                "import subprocess,sys; p=subprocess.Popen(['/bin/sleep','30'],"
                "env={'PATH':'/usr/bin:/bin'}); print(p.pid,flush=True); sys.stdin.readline()"],
                stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            pid = int(parent.stdout.readline())
            scenario = validation.Scenario.__new__(validation.Scenario)
            scenario.directory = directory
            scenario.observation_sequence = 0
            original = scenario.processes(parent.pid)
            self.assertEqual([row['pid'] for row in original], [pid])
            parent.stdin.write('exit\n')
            parent.stdin.flush()
            parent.wait(timeout=5)
            class Server:
                child = parent
                def stop(self, graceful=True):
                    return None
            class Prompt:
                def finish(self, expected=None):
                    return [], 1
            class Scenario(validation.Scenario):
                def create(self, label):
                    return 'synthetic-session'
                def launch(self, *args):
                    return Prompt()
                def pending(self, session):
                    return {'id': 'synthetic-approval'}
                def tasks(self, session):
                    return [{'id': 'synthetic-task', 'state': 'interrupted' if getattr(self, 'recovered', False) else 'running'}]
                def checkpoint(self, identity, name, case):
                    return {'tasks': self.tasks(identity), 'http': {'case_count': 1, 'total_count': 1}}
                def start_server(self):
                    self.server = Server()
                def call(self, command, *args, expected=0):
                    self.recovered = True
                    return [{'type': 'result', 'command': command,
                             'result': {'id': 'synthetic-session', 'state': 'idle'}}]
            crash = Scenario.__new__(Scenario)
            crash.directory = directory
            crash.observation_sequence = scenario.observation_sequence
            crash.server = Server()
            crash.observed_processes = {parent.pid: original}
            try:
                with self.assertRaisesRegex(AssertionError, 'unapproved profile ran before crash'):
                    crash.crash_pending()
                os.kill(pid, 0)
            finally:
                try:
                    os.kill(pid, 15)
                except ProcessLookupError:
                    pass
                parent.stdin.close()
                parent.stdout.close()
                parent.stderr.close()

    def test_disconnect_saves_complete_observations_on_both_sides_of_socket_close(self):
        # Removing either checkpoint or accepting any pre-stream event must fail.
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            state = {'running': False, 'cancelled': False, 'partial_observed': False}
            task = {'id': 'synthetic-original-task', 'state': 'running'}
            identity = 'synthetic-disconnect-session'
            original = [self.core_event(identity, cursor) for cursor in range(1, 1003)]
            class Handler(socketserver.StreamRequestHandler):
                def handle(self):
                    for raw in self.rfile:
                        frame = json.loads(raw)
                        if frame['method'] == 'hello':
                            value = {'authenticated': True}
                        elif frame['method'] == 'task.enqueue':
                            value = task
                        else:
                            state['running'] = True
                            continue
                        self.wfile.write(json.dumps({'version': 1, 'id': frame['id'],
                                                     'result': value}).encode() + b'\n')
                        self.wfile.flush()
                    state['cancelled'] = True
            server = socketserver.ThreadingTCPServer(('127.0.0.1', 0), Handler)
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            class Fixture:
                requests = []
                def count(self, case):
                    return int(state['running'])
            class Scenario(validation.Scenario):
                def create(self, label):
                    return identity
                def get(self, session):
                    messages = []
                    if state['running']:
                        state['partial_observed'] = True
                        messages = [{'role': 'assistant', 'content': 'streaming partial for disconnect'}]
                    return {'id': session, 'cursor': len(original) + int(state['cancelled']),
                            'messages': messages}
                def events(self, session, after=0, limit=1000, cursor=None):
                    rows = copy.deepcopy(original)
                    if state['running']:
                        state['partial_observed'] = True
                        rows[0]['event']['data']['content'] = 'streaming partial for disconnect'
                    if state['cancelled']:
                        rows.append(EvidenceTests.core_event(identity, 1003))
                    return rows[after:after + limit]
                def tasks(self, session):
                    return [dict(task, state='cancelled' if state['cancelled'] else 'running')]
            scenario = Scenario.__new__(Scenario)
            scenario.directory = directory
            scenario.address = '127.0.0.1:%d' % server.server_address[1]
            scenario.token = 'synthetic-token'
            scenario.fixture = Fixture()
            try:
                scenario.disconnect_execution()
            finally:
                server.shutdown()
                server.server_close()
                thread.join(timeout=5)
            self.assertTrue(state['partial_observed'], 'disconnect must observe the actual partial message')
            self.assertTrue((directory / 'disconnect-before-close.json').is_file(),
                            'save a complete checkpoint before closing the connection')
            before = json.loads((directory / 'disconnect-before-close.json').read_text())
            after = json.loads((directory / 'disconnect-after-close.json').read_text())
            self.assertEqual(len(before['events']), 1002)
            self.assertEqual(len(after['events']), 1003)
            self.assertEqual(before['events'], after['events'][:1002])
            self.assertEqual(before['ledger']['records'], [original[1]])
            self.assertEqual(before['ledger']['source'], 'original Core host/model_request events')
            self.assertEqual(after['snapshot']['cursor'], after['events'][-1]['cursor'])
            self.assertEqual(after['tasks'][0]['id'], task['id'])
            result = json.loads((directory / 'disconnect-result.json').read_text())
            self.assertTrue(result['connection_close']['close_started_at'])
            self.assertTrue(result['connection_close']['closed_at'])
            self.assertLessEqual(result['connection_close']['closed_monotonic_ns'], after['monotonic_ns'])
            self.assertTrue((directory / result['connection_close']['frames']).is_file())

    @staticmethod
    def core_event(identity, cursor):
        event = {'type': 'host', 'kind': 'synthetic-progress', 'data': {}}
        if cursor == 2:
            event = {'type': 'host', 'kind': 'model_request',
                     'data': {'record': 'started', 'request_id': 'synthetic-request'}}
        return {'session_id': identity, 'cursor': cursor, 'event': event}

    def test_crash_saves_http_counts_os_receipts_and_recovered_events(self):
        # Dropping the recovered event/OS observations must fail although recovery succeeds.
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            identity = 'synthetic-crash-session'
            state = {'recovered': False}
            original = {'id': 'synthetic-original-task', 'state': 'running'}
            class Child:
                pid = 700001
            class Server:
                child = Child()
                def stop(self, graceful=True):
                    state['stopped'] = True
            class Prompt:
                def finish(self, expected=None):
                    return [], 1
            class Fixture:
                requests = [{'number': 1}]
                def count(self, case):
                    return 1
            class Scenario(validation.Scenario):
                def create(self, label):
                    return identity
                def launch(self, *args):
                    return Prompt()
                def pending(self, session):
                    return {'id': 'synthetic-approval'}
                def tasks(self, session):
                    return [dict(original, state='interrupted') if state['recovered'] else original.copy()]
                def get(self, session):
                    return {'id': session, 'state': 'idle' if state['recovered'] else 'awaiting_approval',
                            'cursor': 4 if state['recovered'] else 3, 'messages': []}
                def events(self, session, after=0, limit=1000, cursor=None):
                    length = 4 if state['recovered'] else 3
                    return [EvidenceTests.core_event(identity, n)
                            for n in range(after + 1, min(after + limit, length) + 1)]
                def processes(self, parent, original=None):
                    path = self.directory / ('synthetic-os-%d.json' % self.observation_sequence)
                    self.observation_sequence += 1
                    validation.save(path, {'parent_pid': parent, 'fixture': True, 'rows': []})
                    return []
                def start_server(self):
                    self.server = Server()
                    self.server.child = type('Restarted', (), {'pid': 700002})()
                def call(self, command, *flags, expected=0):
                    if command == 'recover':
                        state['recovered'] = True
                        return [{'type': 'result', 'command': command, 'result': self.get(identity)}]
                    return []
            scenario = Scenario.__new__(Scenario)
            scenario.directory = directory
            scenario.observation_sequence = 0
            scenario.server = Server()
            scenario.fixture = Fixture()
            scenario.crash_pending()
            self.assertTrue((directory / 'crash-before-exit.json').is_file(),
                            'save HTTP counts, events and ledger before crashing')
            before = json.loads((directory / 'crash-before-exit.json').read_text())
            after = json.loads((directory / 'crash-after-recovery.json').read_text())
            self.assertEqual(before['http']['case_count'], after['http']['case_count'])
            self.assertEqual(before['http']['total_count'], after['http']['total_count'])
            self.assertEqual(before['events'], after['events'][:3])
            self.assertEqual(len(after['events']), 4)
            self.assertEqual(after['tasks'][0]['id'], original['id'])
            self.assertGreaterEqual(scenario.observation_sequence, 2)
            self.assertTrue((directory / 'crash-after-late-approval.json').is_file())
            result = json.loads((directory / 'crash-pending-result.json').read_text())
            self.assertEqual(result['server_pid_before'], 700001)
            self.assertEqual(result['server_pid_after'], 700002)

    def test_tool_arguments_are_split_across_sse_deltas_before_tcp_segmentation(self):
        # Sending one complete arguments delta must fail even if TCP is split into writes.
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            fixture = validation.Fixture(directory)
            connection = http.client.HTTPConnection('127.0.0.1', fixture.server.server_port, timeout=5)
            try:
                body = {'model': validation.ROUTE, 'stream': True,
                        'stream_options': {'include_usage': True},
                        'messages': [{'role': 'user', 'content': 'CASE:round1 synthetic file task'}]}
                connection.request('POST', '/v1/chat/completions', json.dumps(body),
                                   {'Authorization': 'Bearer offline-http-fixture'})
                reply = connection.getresponse()
                self.assertEqual(reply.status, 200)
                raw = reply.read()
            finally:
                connection.close()
                fixture.close()
            self.assertEqual(raw, (directory / 'response-001.sse').read_bytes())
            frames = [json.loads(line[6:]) for line in raw.decode().splitlines()
                      if line.startswith('data: ') and line != 'data: [DONE]']
            calls = [call for frame in frames for choice in frame.get('choices', [])
                     for call in choice.get('delta', {}).get('tool_calls', [])]
            fragments = [call['function']['arguments'] for call in calls
                         if call.get('function', {}).get('arguments')]
            self.assertGreaterEqual(len(fragments), 3, 'protocol deltas must fragment the JSON arguments')
            arguments = json.loads(''.join(fragments))
            self.assertEqual(arguments['action'], 'exec')
            self.assertIn("path='source.txt'", arguments['script'])
            self.assertIn("path='round1.txt'", arguments['script'])
            self.assertEqual([call['id'] for call in calls if call.get('id')], ['offline-call-001'])
            transport = json.loads((directory / 'response-001.transport.json').read_text())
            self.assertEqual(transport['tool_argument_deltas'], len(fragments))
            self.assertEqual(transport['tcp_write_bytes'], 37)
            self.assertTrue(transport['complete'])


if __name__ == '__main__':
    unittest.main(verbosity=2)
