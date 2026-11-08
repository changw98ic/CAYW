#!/usr/bin/env python3
"""Synthetic installer fault tests. Not macOS or real-binary release evidence.

uname/ps and executables are explicit fixtures, including on non-macOS hosts.
The actual macOS package acceptance lives in validate-release-install.py.
"""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

PROGRAMS = ('cayw', 'cayw-client', 'cayw-tui', 'cayw-webui')
INSTALLER = Path(__file__).resolve().with_name('install-cli.sh')


def executable(path, body):
    path.write_text('#!/bin/sh\n' + body)
    path.chmod(0o755)


class InstallerTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='cayw installer fixture ')
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.bundle = self.root / 'release with spaces'
        (self.bundle / 'bin').mkdir(parents=True)
        shutil.copyfile(INSTALLER, self.bundle / 'install.sh')
        for program in PROGRAMS:
            executable(self.bundle / 'bin' / program,
                       '# new fixture version\n'
                       'case "$0" in */prefix/bin/cayw-webui)\n'
                       '  [ "${FAIL_INSTALLED_HELP:-0}" = 0 ] || exit 1;; esac\n'
                       'exit 0\n')
        self.sums()
        self.prefix = self.root / 'prefix'
        self.bin = self.prefix / 'bin'
        self.bin.mkdir(parents=True)
        self.home = self.root / 'home'
        self.home.mkdir()
        self.state = self.home / '.cayw'
        self.state.mkdir()
        (self.state / 'sentinel').write_bytes(b'private synthetic state unchanged')
        shim = self.root / 'shim'
        shim.mkdir()
        executable(shim / 'uname', 'case "$1" in -s) echo Darwin;; -m) echo arm64;; esac\n')
        executable(shim / 'ps', 'printf "%s\\n" "${FIXTURE_PS:-}"\n')
        executable(shim / 'mv', '''
case "$2" in
    */new.cayw-webui)
        if [ "${FAULT:-}" = term-late ]; then kill -TERM "$PPID"; exit 1; fi;;
    */new.cayw-client)
        case "${FAULT:-}" in
            fail|rollback-fail) exit 1;;
            term) kill -TERM "$PPID"; exit 1;;
            kill) kill -KILL "$PPID"; exit 1;;
        esac;;
    */restore.cayw)
        [ "${FAULT:-}" != rollback-fail ] || exit 1;;
esac
exec "$REAL_MV" "$@"
''')
        self.env = dict(os.environ, PATH=str(shim) + os.pathsep + os.environ['PATH'],
                        HOME=str(self.home), CAYW_HOME=str(self.state), REAL_MV=shutil.which('mv'))
        for key in ('FAULT', 'FIXTURE_PS', 'FAIL_INSTALLED_HELP'):
            self.env.pop(key, None)

    def sums(self):
        (self.bundle / 'SHA256SUMS').write_text(''.join(
            f'{hashlib.sha256((self.bundle / "bin" / p).read_bytes()).hexdigest()}  bin/{p}\n'
            for p in PROGRAMS))

    def old(self, programs=PROGRAMS):
        for program in programs:
            executable(self.bin / program, '# old fixture version\nexit 0\n')
        return self.snapshot()

    def snapshot(self):
        return {p: (self.bin / p).read_bytes() if (self.bin / p).exists() else None
                for p in PROGRAMS}

    def install(self, force=False, good=True, **env):
        args = ['sh', str(self.bundle / 'install.sh'), '--prefix', str(self.prefix)]
        if force:
            args.append('--force')
        result = subprocess.run(args, env=dict(self.env, **env), cwd=self.root,
                                text=True, capture_output=True, timeout=20)
        self.assertEqual(result.returncode == 0, good, result.stdout + result.stderr)
        self.assertEqual((self.state / 'sentinel').read_bytes(), b'private synthetic state unchanged')
        return result

    def assert_new(self):
        for program in PROGRAMS:
            self.assertEqual((self.bin / program).read_bytes(), (self.bundle / 'bin' / program).read_bytes())
        self.assertFalse((self.bin / '.cayw-install.lock').exists())

    def test_fresh_install_repeat_and_current_guidance(self):
        result = self.install()
        self.assert_new()
        self.install()
        self.assert_new()
        self.assertIn('cayw conf', result.stdout)
        self.assertNotIn('cayw configure', result.stdout)
        self.assertIn('api_key', result.stdout)
        self.assertIn('models.json', result.stdout)

    def test_upgrade_requires_force_and_replaces_complete_set(self):
        before = self.old()
        self.install(good=False)
        self.assertEqual(self.snapshot(), before)
        self.install(force=True)
        self.assert_new()

    def test_checksum_failure_preserves_originals(self):
        before = self.old()
        with (self.bundle / 'bin/cayw-tui').open('a') as stream:
            stream.write('# damaged\n')
        self.install(force=True, good=False)
        self.assertEqual(self.snapshot(), before)

    def test_duplicate_checksum_rejected(self):
        before = self.old()
        sums = self.bundle / 'SHA256SUMS'
        sums.write_text(sums.read_text() + sums.read_text().splitlines()[0] + '\n')
        self.install(force=True, good=False)
        self.assertEqual(self.snapshot(), before)

    def test_running_client_in_other_prefix_refuses_even_force(self):
        before = self.old()
        result = self.install(force=True, good=False,
                              FIXTURE_PS=f' {os.getuid()} 1234 /other prefix/bin/cayw')
        self.assertIn('1234', result.stderr)
        self.assertEqual(self.snapshot(), before)

    def test_unrelated_process_does_not_block(self):
        self.install(FIXTURE_PS=f' {os.getuid()} 1234 /other/bin/not-cayw')
        self.assert_new()

    def test_existing_transaction_is_not_overwritten(self):
        before = self.old()
        lock = self.bin / '.cayw-install.lock'
        lock.mkdir()
        (lock / 'backup').write_text('retain this')
        self.install(force=True, good=False)
        self.assertEqual((lock / 'backup').read_text(), 'retain this')
        self.assertEqual(self.snapshot(), before)

    def test_symlink_destination_is_rejected(self):
        outside = self.root / 'outside'
        executable(outside, 'exit 0\n')
        (self.bin / 'cayw').symlink_to(outside)
        self.install(force=True, good=False)
        self.assertTrue((self.bin / 'cayw').is_symlink())
        self.assertEqual(outside.read_text(), '#!/bin/sh\nexit 0\n')

    def test_replace_failure_rolls_back_all_four(self):
        before = self.old()
        self.install(force=True, good=False, FAULT='fail')
        self.assertEqual(self.snapshot(), before)
        self.assertFalse((self.bin / '.cayw-install.lock').exists())

    def test_term_during_replacement_rolls_back(self):
        before = self.old()
        self.install(force=True, good=False, FAULT='term')
        self.assertEqual(self.snapshot(), before)
        self.assertFalse((self.bin / '.cayw-install.lock').exists())

    def test_term_after_three_replacements_rolls_back(self):
        before = self.old()
        self.install(force=True, good=False, FAULT='term-late')
        self.assertEqual(self.snapshot(), before)
        self.assertFalse((self.bin / '.cayw-install.lock').exists())

    def test_fresh_interruption_removes_partial_install(self):
        self.install(force=True, good=False, FAULT='term')
        self.assertEqual(self.snapshot(), dict.fromkeys(PROGRAMS))

    def test_partial_existing_set_restores_absences(self):
        before = self.old(('cayw-client', 'cayw-webui'))
        self.install(force=True, good=False, FAULT='fail')
        self.assertEqual(self.snapshot(), before)

    def test_post_install_help_failure_rolls_back(self):
        before = self.old()
        self.install(force=True, good=False, FAIL_INSTALLED_HELP='1')
        self.assertEqual(self.snapshot(), before)

    def test_uncatchable_kill_retains_complete_backup_and_blocks_retry(self):
        before = self.old()
        self.install(force=True, good=False, FAULT='kill')
        lock = self.bin / '.cayw-install.lock'
        self.assertTrue((lock / 'replacing').exists())
        for program in PROGRAMS:
            self.assertEqual((lock / f'old.{program}').read_bytes(), before[program])
        self.install(force=True, good=False)
        self.assertTrue(lock.exists())

    def test_failed_rollback_retains_backups_and_blocks_retry(self):
        before = self.old()
        result = self.install(force=True, good=False, FAULT='rollback-fail')
        self.assertIn('恢复未完成', result.stderr)
        lock = self.bin / '.cayw-install.lock'
        for program in PROGRAMS:
            self.assertEqual((lock / f'old.{program}').read_bytes(), before[program])
        self.install(force=True, good=False)


class PackageTests(unittest.TestCase):
    """Check assembly wiring with fake Cargo; this never proves a Rust build."""

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='cayw package fixture ')
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / 'scripts').mkdir()
        for name in ('package-cli.sh', 'install-cli.sh'):
            shutil.copyfile(INSTALLER.with_name(name), self.root / 'scripts' / name)
        (self.root / 'LICENSE').write_text('synthetic package fixture license')
        shim = self.root / 'shim'
        shim.mkdir()
        executable(shim / 'uname', 'case "$1" in -s) echo Darwin;; -m) echo arm64;; esac\n')
        executable(shim / 'git', '''case "$1" in
status) exit 0;;
rev-parse) echo 0123456789012345678901234567890123456789;;
diff) exit "${SOURCE_CHANGED:-0}";;
*) exit 1;;
esac
''')
        cargo = shim / 'cargo'
        cargo.write_text('#!' + sys.executable + '\n' + '''import json, os, pathlib, sys
args = sys.argv[1:]
with open(os.environ['CARGO_FIXTURE_LOG'], 'a') as stream:
    stream.write(json.dumps(args) + "\\n")
out = pathlib.Path(args[args.index('--target-dir') + 1]) / args[args.index('--target') + 1] / 'release'
out.mkdir(parents=True, exist_ok=True)
for i, arg in enumerate(args):
    if arg == '--bin':
        binary = out / args[i + 1]
        binary.write_text('#!/bin/sh\\n# synthetic packaging fixture\\nexit 0\\n')
        binary.chmod(0o755)
''')
        cargo.chmod(0o755)
        self.output = self.root / 'assembled release'
        self.log = self.root / 'cargo-calls.jsonl'
        self.env = dict(os.environ, PATH=str(shim) + os.pathsep + os.environ['PATH'],
                        CARGO_TARGET_DIR='relative target with spaces',
                        CARGO_BUILD_TARGET='ignored-wrong-target', CARGO_FIXTURE_LOG=str(self.log))
        self.env.pop('SOURCE_CHANGED', None)

    def package(self, good=True, **env):
        result = subprocess.run(['sh', str(self.root / 'scripts/package-cli.sh'), str(self.output)],
                                env=dict(self.env, **env), text=True, capture_output=True, timeout=20)
        self.assertEqual(result.returncode == 0, good, result.stdout + result.stderr)
        return result

    def test_all_four_programs_and_independent_workspace_are_explicit(self):
        self.package()
        calls = [json.loads(line) for line in self.log.read_text().splitlines()]
        self.assertEqual(len(calls), 3)
        self.assertIn('--no-default-features', calls[0])
        self.assertEqual(calls[0][calls[0].index('--features') + 1], 'plugins')
        self.assertEqual(calls[2][calls[2].index('--manifest-path') + 1],
                         'examples/headless-client/Cargo.toml')
        for call in calls:
            self.assertEqual(call[call.index('--target') + 1], 'aarch64-apple-darwin')
            self.assertEqual(call[call.index('--target-dir') + 1],
                             str(self.root / 'relative target with spaces'))
        for program in PROGRAMS:
            self.assertTrue(os.access(self.output / 'bin' / program, os.X_OK))
        self.assertEqual(json.loads((self.output / 'BUILD.json').read_text())['runtime_features'], ['plugins'])
        checked = subprocess.run(['shasum', '-a', '256', '-c', 'SHA256SUMS'],
                                 cwd=self.output, capture_output=True, text=True)
        self.assertEqual(checked.returncode, 0, checked.stderr)

    def test_source_change_during_build_refuses_bundle(self):
        self.package(good=False, SOURCE_CHANGED='1')
        self.assertFalse(self.output.exists())
        self.assertEqual(list(self.root.glob('.cayw-package.*')), [])


if __name__ == '__main__':
    unittest.main(verbosity=2)
