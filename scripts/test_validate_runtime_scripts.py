"""Offline checks for example selection and the product lock fault fixture."""
import ast
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

SCRIPTS = Path(__file__).parent


class RuntimeScriptTests(unittest.TestCase):
    def test_runtime_example_matches_manifest(self):
        import tomllib
        source = ast.parse((SCRIPTS / 'validate-runtime.py').read_text())
        targets = {}
        for node in ast.walk(source):
            if isinstance(node, ast.Tuple) and len(node.elts) == 2 \
                    and isinstance(node.elts[0], ast.Constant) \
                    and node.elts[0].value in ('offline-example', 'headless-example'):
                command = eval(compile(ast.Expression(node.elts[1]), '<command>', 'eval'),
                               {'cargo': ['cargo'], 'output': Path('evidence')})
                targets[command[command.index('-p') + 1]] = command[command.index('--example') + 1]
        self.assertEqual(targets['cayw-agent'], 'offline')
        self.assertEqual(targets['cayw-runtime'], 'runtime_offline')
        manifest = tomllib.loads((SCRIPTS.parent / 'crates/cayw-runtime/Cargo.toml').read_text())
        self.assertIn(targets['cayw-runtime'], [target['name'] for target in manifest['example']])

    def run_lock_fixture(self, fault=None):
        source = ast.parse((SCRIPTS / 'validate-product-workflows.py').read_text())
        main = next(node for node in source.body if isinstance(node, ast.Try))
        start = next(i for i, node in enumerate(main.body) if isinstance(node, ast.Assign)
                     and any(isinstance(t, ast.Name) and t.id == 'data_dir' for t in node.targets))
        end = next(i for i in range(start, len(main.body)) if isinstance(main.body[i], ast.Try))
        block = ast.Module(body=main.body[start:end + 1], type_ignores=[])
        code = compile(ast.fix_missing_locations(block), '<lock-fixture>', 'exec')
        with tempfile.TemporaryDirectory() as td:
            data = Path(td) / 'cayw'
            data.mkdir()
            lock = data / 'init.lock'
            lock.write_text('original')
            mkdir, rename = Path.mkdir, Path.rename

            def make(path, *args, **kwargs):
                if fault == 'mkdir' and path == lock:
                    raise OSError('injected mkdir')
                return mkdir(path, *args, **kwargs)

            def move(path, *args, **kwargs):
                if fault == 'rename' and path == lock:
                    raise OSError('injected rename')
                return rename(path, *args, **kwargs)

            calls = []

            def rpc(ready, token, method, *args, **kwargs):
                calls.append(method)
                if fault == 'rpc':
                    raise OSError('injected rpc')
                if method == 'settings.plugins.disable':
                    self.assertTrue(lock.is_dir())
                    return {'outcome_unknown': True}
                return {'entries': [{'name': 'process', 'enabled': True, 'available': True}]}

            scope = {'Path': Path, 'env': {'CAYW_HOME': str(data)}, 'rpc': rpc,
                     'ready': {}, 'token': None}
            with patch.object(Path, 'mkdir', make), patch.object(Path, 'rename', move):
                if fault:
                    with self.assertRaises(OSError):
                        exec(code, scope)
                else:
                    exec(code, scope)
                    self.assertEqual(len(calls), 2)
            self.assertEqual(lock.read_text(), 'original')
            self.assertFalse((data / 'init-lock.bak').exists())

    def test_lock_fixture_restores_original(self):
        for fault in (None, 'rename', 'mkdir', 'rpc'):
            with self.subTest(fault=fault):
                self.run_lock_fixture(fault)


if __name__ == '__main__':
    unittest.main()
