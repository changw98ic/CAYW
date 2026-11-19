"""Offline regressions for the live validator's completion summary (no requests)."""
import contextlib
import importlib.util
import io
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location(
    'runtime_errors_live', Path(__file__).with_name('validate-runtime-errors-live.py'))
LIVE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(LIVE)


class CompletionTests(unittest.TestCase):
    def run_case(self, interrupt=None, fail=None):
        def step(name):
            if name == interrupt:
                raise KeyboardInterrupt(name)
            if name == fail:
                raise RuntimeError(name)

        class Relay:
            def __init__(self, *args):
                pass

            def close(self):
                step('close')

            def summary(self):
                return dict.fromkeys(('base', 'upstream', 'actual_posts', 'max_actual_posts',
                                      'refused_posts', 'reported_usage_sum', 'unknown_count'), 0)

        class Normal:
            def __init__(self, mode, args, relay):
                self.mode, self.dir, self.identity = mode, args.out, mode
                self.ev = {'model_usage': {'usage_total_sum': 0, 'relay_request_numbers': []}}

            def run(self):
                step('normal-' + self.mode)

            def close(self):
                pass

        def preflight(*args):
            step('preflight')
            return {'checked': True}

        def writes(mode, *args):
            step('writes-' + mode)
            return {'mode': mode, 'actual_provider_calls': 0}  # Real shape has no passed.

        def probe(case, mode, *args):
            step(case + '-' + mode)
            return {'case': case, 'mode': mode}  # Real shape has no passed.

        with tempfile.TemporaryDirectory() as td:
            root = Path(td)
            binary = root / 'placeholder'
            binary.write_bytes(b'not executed')
            output = root / 'evidence'
            argv = ['validator', '--runtime', str(binary), '--client', str(binary),
                    '--probe', str(binary), '--out', str(output)]
            with patch.object(sys, 'argv', argv), \
                    patch.object(LIVE, 'resolve_credential', return_value=('synthetic', 'test')), \
                    patch.object(LIVE, 'Relay', Relay), patch.object(LIVE, 'Normal', Normal), \
                    patch.object(LIVE, 'source_manifest', return_value={'complete': True}), \
                    patch.object(LIVE, 'preflight', preflight), \
                    patch.object(LIVE, 'writes_step', writes), \
                    patch.object(LIVE, 'root_probe', probe), \
                    contextlib.redirect_stdout(io.StringIO()):
                if interrupt:
                    with self.assertRaises(KeyboardInterrupt):
                        LIVE.main()
                    code = None
                else:
                    code = LIVE.main()
            return code, json.loads((output / 'summary.json').read_text())

    def test_all_eight_real_result_shapes_pass(self):
        code, report = self.run_case()
        self.assertEqual(code, 0)
        self.assertTrue(report['passed'])
        self.assertEqual(len(report['scenarios']), 8)

    def test_interruptions_never_pass(self):
        for name in ['preflight', 'close'] + [case + '-' + mode
                for case in ('normal', 'writes', 'storage', 'panic') for mode in ('sdk', 'ipc')]:
            with self.subTest(name=name):
                _, report = self.run_case(interrupt=name)
                self.assertFalse(report['passed'])
                self.assertTrue(report['failures'])

    def test_failed_or_incomplete_run_never_passes(self):
        for name in ('preflight', 'normal-sdk', 'writes-ipc', 'storage-sdk', 'panic-ipc', 'close'):
            with self.subTest(name=name):
                code, report = self.run_case(fail=name)
                self.assertEqual(code, 1)
                self.assertFalse(report['passed'])
                self.assertTrue(report['failures'])


if __name__ == '__main__':
    unittest.main()
