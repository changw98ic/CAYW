"""Synthetic CLI failures only; these tests never contact a model service."""
import contextlib
import importlib.util
import io
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import Mock, patch

spec = importlib.util.spec_from_file_location("live_coding", Path(__file__).with_name("validate-live-coding.py"))
runner = importlib.util.module_from_spec(spec)
spec.loader.exec_module(runner)


class ClassificationTests(unittest.TestCase):
    def failed_run(self, stage):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "run"
            binary = Path(tmp) / "binary"
            binary.write_bytes(b"synthetic executable")
            server = Mock()
            server.stdout = io.StringIO('{"listening":"127.0.0.1:1"}\n')
            server.poll.return_value = None
            server.wait.return_value = 0

            def invoke(argv, **kwargs):
                code = 0
                if "init" in argv:
                    (root / "private/cayw").mkdir()
                elif stage == "configuration" and "add" in argv:
                    code = 1
                elif "create" in argv:
                    kwargs["stdout"].write('{"type":"result","result":{"id":"synthetic-session"}}\n')
                elif "prompt" in argv:
                    # Reserved is not a Provider attempt. Started is, even if
                    # the service then fails without a completion or usage.
                    records = ["reserved", "started"] if stage == "provider" else ["reserved"]
                    for record in records:
                        kwargs["stdout"].write(json.dumps({"type":"event", "event":{"event":{
                            "type":"host", "kind":"model_request", "data":{"record":record}}}}) + "\n")
                    code = 1
                return subprocess.CompletedProcess(argv, code)

            def output(argv, **kwargs):
                if argv[0] == "rustup":
                    return "/usr/bin/true\n"
                return b"" if "diff" in argv else "a" * 40 + "\n"

            catalog = io.StringIO(json.dumps({"data":[{"id":"opencode-go/deepseek-v4.1-flash", "max_output_tokens":1}]}))
            with patch.object(sys, "argv", ["validate-live-coding.py", "--runtime", str(binary),
                     "--client", str(binary), "--out", str(root)]), \
                 patch.dict(runner.os.environ, {}, clear=True), \
                 patch.object(runner.subprocess, "run", side_effect=invoke), \
                 patch.object(runner.subprocess, "check_output", side_effect=output), \
                 patch.object(runner.subprocess, "Popen", return_value=server), \
                 patch.object(runner.urllib.request, "urlopen", return_value=catalog), \
                 contextlib.redirect_stdout(io.StringIO()):
                with self.assertRaises(RuntimeError):
                    runner.main()
            summary = json.loads((root / "review/summary.json").read_text())
            self.assertFalse(summary["machine_passed"])
            server.send_signal.assert_called_once()
            return summary

    def test_configuration_failure_is_not_a_live_attempt(self):
        self.assertEqual(self.failed_run("configuration")["execution_kind"], "not_run")

    def test_reserved_but_not_started_is_not_a_live_attempt(self):
        self.assertEqual(self.failed_run("reserved")["execution_kind"], "not_run")

    def test_started_request_stays_live_when_provider_fails(self):
        self.assertEqual(self.failed_run("provider")["execution_kind"], "live_model")


if __name__ == "__main__":
    unittest.main()
