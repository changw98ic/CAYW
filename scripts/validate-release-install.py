#!/usr/bin/env python3
"""Accept a real packaged macOS arm64 release, without a model or user state.

Usage: python3 scripts/validate-release-install.py BUNDLE NEW_EVIDENCE_DIRECTORY
Only logs, BUILD.json and hashes go into evidence. Ephemeral HOME/CAYW_HOME,
IPC credentials and databases stay outside it. No paid/provider calls are made.
"""
from contextlib import contextmanager
import fcntl
import hashlib
import json
import os
from pathlib import Path
import platform
import pty
import re
import select
import shutil
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time
import urllib.error
import urllib.request

PROGRAMS = ('cayw', 'cayw-client', 'cayw-tui', 'cayw-webui')
WATCHDOG = 120


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


@contextmanager
def isolated_directory(processes):
    path = Path(tempfile.mkdtemp(prefix='cayw-release-', dir='/tmp'))
    try:
        yield path
    finally:
        if all(child.poll() is not None for child, _ in processes):
            shutil.rmtree(path)
        else:
            print(f'Cleanup unconfirmed: isolated test state retained at {path}', file=sys.stderr)


def main():
    if len(sys.argv) != 3:
        raise SystemExit(__doc__)
    if platform.system() != 'Darwin' or platform.machine() != 'arm64':
        raise SystemExit('Real release acceptance requires macOS arm64; this platform is unverified.')
    bundle = Path(sys.argv[1]).resolve()
    evidence = Path(sys.argv[2]).resolve()
    evidence.mkdir(parents=True, exist_ok=False)
    manifest = json.loads((bundle / 'BUILD.json').read_text())
    assert manifest['target'] == 'aarch64-apple-darwin', manifest
    assert manifest['runtime_features'] == ['plugins'], manifest
    shutil.copyfile(bundle / 'BUILD.json', evidence / 'BUILD.json')
    summary = {'platform': platform.platform(), 'machine': platform.machine(),
               'build': manifest, 'fixture': 'synthetic sessions and offline demo; no real model',
               'binary_sha256': {p: digest(bundle / 'bin' / p) for p in PROGRAMS},
               'checks': [], 'passed': False,
               'limits': ['No historical release binary or cross-version database migration tested.',
                          'Synthetic old executables exercise installer replacement only.',
                          'TUI connected/quit tested; no full interactive terminal or browser UI acceptance here.',
                          'SIGKILL interruption uses shell fixtures; no power-loss durability validation.',
                          'BUILD.json remains package-only, not an installed provenance registry.']}
    processes = []
    terminal_fds = []
    sequence = 0
    # Short paths avoid macOS Unix socket limits. Never artifact this directory.
    with isolated_directory(processes) as temporary:
        root = Path(temporary)
        home = root / 'home'
        home.mkdir(mode=0o700)
        state = root / 'state'
        workspace = root / 'workspace'
        workspace.mkdir()
        installed = home / '.local/bin'
        env = {'PATH': os.environ['PATH'], 'HOME': str(home), 'CAYW_HOME': str(state)}
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))

        def command(args, good=True):
            nonlocal sequence
            sequence += 1
            result = subprocess.run([str(arg) for arg in args], env=env, cwd=workspace,
                                    stdin=subprocess.DEVNULL, text=True, capture_output=True,
                                    timeout=WATCHDOG)
            (evidence / f'{sequence:02d}-command.json').write_text(json.dumps({
                'argv': [str(arg) for arg in args], 'returncode': result.returncode,
                'stdout': result.stdout, 'stderr': result.stderr}, ensure_ascii=False, indent=2))
            assert (result.returncode == 0) == good, result.stdout + result.stderr
            return result

        def install(force=False, good=True):
            args = ['sh', bundle / 'install.sh']
            if force:
                args.append('--force')
            return command(args, good)

        def hashes(directory):
            return {str(path.relative_to(directory)): digest(path)
                    for path in directory.rglob('*') if path.is_file()}

        def assert_installed():
            for program in PROGRAMS:
                assert digest(installed / program) == summary['binary_sha256'][program]
                assert os.access(installed / program, os.X_OK)
            assert not (installed / '.cayw-install.lock').exists()

        def launch(binary=None, offline=False):
            binary = binary or installed / 'cayw'
            args = [str(binary), 'start', '--ui', 'web', '--no-open', '--workspace', str(workspace)]
            if offline:
                args.extend(['--offline', '--demo-authorized'])
            number = len(processes) + 1
            stderr = (evidence / f'launch-{number}.stderr').open('w')
            child = subprocess.Popen(args, env=env, cwd=workspace, stdin=subprocess.DEVNULL,
                                     stdout=subprocess.PIPE, stderr=stderr, text=True,
                                     start_new_session=True)
            processes.append((child, stderr))
            assert select.select([child.stdout], [], [], WATCHDOG)[0], 'startup watchdog expired'
            line = child.stdout.readline()
            (evidence / f'launch-{number}.json').write_text(line)
            assert line, f'launcher exited: {child.poll()}'
            ready = json.loads(line)
            assert ready['ui'] == 'web' and ready['access'] == 'local', ready
            assert child.poll() is None, ready
            return child, ready

        def tui_smoke():
            # Bare installed cayw must open the real default TUI, not only parse help.
            master, slave = pty.openpty()
            terminal_fds.append(master)
            fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 32, 110, 0, 0))

            def terminal_setup():
                os.setsid()
                fcntl.ioctl(slave, termios.TIOCSCTTY, 0)

            log = (evidence / 'tui.pty.log').open('wb')
            try:
                child = subprocess.Popen([str(installed / 'cayw')], cwd=workspace,
                    env=dict(env, TERM='xterm-256color'), stdin=slave, stdout=slave,
                    stderr=slave, preexec_fn=terminal_setup)
            except Exception:
                log.close()
                raise
            finally:
                os.close(slave)
            processes.append((child, log))
            output = bytearray()

            def read_terminal():
                if select.select([master], [], [], 0.1)[0]:
                    try:
                        chunk = os.read(master, 65536)
                    except OSError:
                        return
                    output.extend(chunk)
                    log.write(chunk)
                    log.flush()
                    if b'\x1b[6n' in chunk:
                        os.write(master, b'\x1b[1;1R')

            deadline = time.monotonic() + WATCHDOG
            while not re.search(r'CAYW\s+•\s+connected', output.decode(errors='replace')):
                assert child.poll() is None, 'TUI exited before connecting; see tui.pty.log'
                assert time.monotonic() < deadline, 'TUI readiness watchdog expired'
                read_terminal()
            assert not list(state.rglob('models.json')), 'no-model TUI unexpectedly configured a model'
            os.write(master, b'\x11')  # TUI Ctrl+Q; launcher must also reap its owned Client.
            deadline = time.monotonic() + WATCHDOG
            while child.poll() is None:
                assert time.monotonic() < deadline, 'TUI shutdown watchdog expired'
                read_terminal()
            assert child.returncode == 0, 'TUI quit did not finish cleanly'
            assert not (state / 'client.json').exists(), 'TUI launcher left Client discovery behind'

        def stop(child):
            assert child.poll() is None, 'launcher exited unexpectedly'
            child.send_signal(signal.SIGINT)
            assert child.wait(timeout=WATCHDOG) == 0, 'graceful stop failed'

        def rpc(ready, method, params=None):
            request = urllib.request.Request(ready['url'] + 'api/rpc',
                data=json.dumps({'method': method, 'params': params or {}}).encode(),
                headers={'Content-Type': 'application/json'})
            with opener.open(request, timeout=WATCHDOG) as response:
                result = json.load(response)
            assert 'result' in result, result
            return result['result']

        def client(*args, good=True):
            output = command([installed / 'cayw-client', *args], good)
            records = [json.loads(line) for line in (output.stdout if good else output.stderr).splitlines()]
            return next(record for record in reversed(records)
                        if record.get('type') == ('result' if good else 'error'))

        try:
            assert not state.exists()
            install()
            assert_installed()
            assert not state.exists(), 'installer must not initialize application state'
            for program in PROGRAMS:
                command([installed / program, '--help'])
            summary['checks'].append('fresh default-prefix install of all four real packaged binaries; help; no state access')

            tui_smoke()
            summary['checks'].append('bare installed cayw starts real no-model TUI in a PTY; connected readiness, Ctrl+Q and owned Client shutdown')

            child, ready = launch()
            health = rpc(ready, 'runtime.health')
            assert health['provider'] == 'unconfigured' and health['model'] == 'unconfigured', health
            created = client('create', '--label', 'release acceptance synthetic session')['result']
            session_id = created['id']
            assert client('get', '--session', session_id)['result']['id'] == session_id
            rejected = client('prompt', '--session', session_id, '--text', 'synthetic no-model rejection', good=False)
            assert rejected['code'] == 'model_not_configured' and rejected['outcome_unknown'] is False, rejected
            assert client('tasks', '--session', session_id)['result'] == []
            assert any(view['id'] == session_id for view in rpc(ready, 'session.list'))
            before = hashes(installed)
            refused = install(force=True, good=False)
            assert '运行中的 CAYW' in refused.stderr
            assert hashes(installed) == before
            assert rpc(ready, 'runtime.health')['provider'] == 'unconfigured'
            stop(child)
            summary['checks'].append('no-model installed WebUI/Client and independent IPC client; create/query, known prompt rejection, no task admitted; live upgrade refusal preserves running service')

            # A separately installed/running copy must also prevent mixing versions
            # at the target prefix. This is the same candidate binary, not an old release.
            alternate = root / 'other-prefix/bin'
            shutil.copytree(installed, alternate)
            child, ready = launch(alternate / 'cayw')
            refused = install(force=True, good=False)
            assert '运行中的 CAYW' in refused.stderr
            assert hashes(installed) == before
            assert client('get', '--session', session_id)['result']['id'] == session_id
            stop(child)
            summary['checks'].append('running Client/UI from another prefix blocks target installation; graceful restart preserves Session')

            state_before = hashes(state)
            # Different executable bytes are intentional synthetic prior-version
            # fixtures. They are never presented as a historical Rust release.
            for program in PROGRAMS:
                (installed / program).write_text('#!/bin/sh\n# synthetic prior install\nexit 0\n')
                (installed / program).chmod(0o755)
            old = hashes(installed)
            install(good=False)
            assert hashes(installed) == old
            install(force=True)
            assert_installed()
            assert hashes(state) == state_before, 'installer changed persistent application data'
            child, ready = launch()
            assert rpc(ready, 'runtime.health')['provider'] == 'unconfigured'
            assert client('get', '--session', session_id)['result']['id'] == session_id
            assert client('tasks', '--session', session_id)['result'] == []
            stop(child)
            summary['checks'].append('different-file install rejects without --force; complete forced replacement preserves stopped state byte-for-byte; restart retains original Session without replay')

            child, ready = launch(offline=True)
            demo = client('create', '--label', 'release offline fixture')['result']['id']
            output = command([installed / 'cayw-client', 'prompt', '--session', demo, '--text', 'run offline fixture'])
            records = [json.loads(line) for line in output.stdout.splitlines()]
            assert any(record.get('type') == 'prompt_finished' for record in records)
            assert (workspace / 'cayw-demo.txt').read_text() == 'headless runtime demo'
            stop(child)
            summary['checks'].append('installed plugins-only release executes offline synthetic demo through real Client/IPC/Runtime/Host and stops cleanly')
            summary['passed'] = True
        except Exception as error:
            summary['error'] = str(error)
            raise
        finally:
            cleanup_errors = []
            for child, stderr in reversed(processes):
                if child.poll() is None:
                    child.send_signal(signal.SIGINT)
                    try:
                        if child.wait(timeout=WATCHDOG) != 0:
                            cleanup_errors.append(f'process {child.pid} exited unsuccessfully')
                    except subprocess.TimeoutExpired:
                        cleanup_errors.append(f'process {child.pid} cleanup unconfirmed')
                        # CI contains only our isolated synthetic processes. Do not
                        # call a forced kill or mark this as successful cleanup.
                stderr.close()
                if child.stdout is not None:
                    child.stdout.close()
            for master in terminal_fds:
                os.close(master)
            if cleanup_errors:
                summary['passed'] = False
                summary['cleanup_errors'] = cleanup_errors
            (evidence / 'summary.json').write_text(json.dumps(summary, ensure_ascii=False, indent=2) + '\n')
            if cleanup_errors:
                raise RuntimeError('; '.join(cleanup_errors))
    print(json.dumps(summary, ensure_ascii=False, indent=2))


if __name__ == '__main__':
    main()
