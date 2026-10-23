#!/usr/bin/env python3
"""Drive the real TUI and fixture Runtime through tmux and authenticated IPC.

Run with Python -I. No model service is contacted. Artifacts are written only to
an explicit temporary evidence directory; the host owns its temporary workspace.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shlex
import socket
import subprocess
import termios
import time
import uuid


class Rpc:
    def __init__(self, address, token):
        host, port = address.rsplit(":", 1)
        self.socket = socket.create_connection((host, int(port)), timeout=10)
        self.file = self.socket.makefile("rwb", buffering=0)
        self.index = 0
        self.pending = {}
        result = self.call("hello", {"token": token})
        assert result["version"] == 1, result

    def start(self, method, params):
        self.index += 1
        identifier = f"tmux-{self.index}"
        self.file.write((json.dumps({"version": 1, "id": identifier, "method": method, "params": params}) + "\n").encode())
        return identifier

    def finish(self, identifier):
        while identifier not in self.pending:
            line = self.file.readline()
            if not line:
                raise RuntimeError("IPC closed before a terminal result")
            reply = json.loads(line)
            self.pending[reply["id"]] = reply
        reply = self.pending.pop(identifier)
        if "error" in reply:
            raise RuntimeError(reply["error"])
        return reply["result"]

    def call(self, method, params):
        return self.finish(self.start(method, params))

    def close(self):
        self.file.close()
        self.socket.close()


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def restored_terminal(out):
    before = (out / "stty-before.txt").read_text().strip()
    after = (out / "stty-after.txt").read_text().strip()

    def settings(value):
        fields = value.split(":")
        for index, field in enumerate(fields):
            if field.startswith("lflag="):
                # PENDIN is consumed by reading terminal input, including a
                # plain shell `read`; it is not a persistent terminal mode.
                flags = int(field.split("=", 1)[1], 16)
                fields[index] = f"lflag={flags & ~termios.PENDIN:x}"
        return fields

    assert settings(before) == settings(after), (before, after)
    return {"before": before, "after": after, "ignored_input_flag": "PENDIN", "restored": True}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True)
    parser.add_argument("--host", required=True)
    parser.add_argument("--out", required=True)
    args = parser.parse_args()
    out = Path(args.out).resolve()
    out.mkdir(parents=True, exist_ok=False)
    token = uuid.uuid4().hex
    info = out / "fixture-info.json"
    env = dict(os.environ, CAYW_IPC_TOKEN=token, CAYW_IPC_ADDR="127.0.0.1:0", CAYW_FIXTURE_INFO=str(info))
    env.pop("CAYW_FIXTURE_STREAM_RELEASE", None)
    host_log = (out / "fixture-host.stderr").open("w")
    host = subprocess.Popen([args.host], cwd="/tmp", env=env, stdout=subprocess.PIPE, stderr=host_log, text=True)
    session = "cayw-review-" + uuid.uuid4().hex[:10]
    rpc = None
    stages = []
    rpc_evidence = []
    tmux = ["/opt/homebrew/bin/tmux"]
    source_paths = {str(Path(__file__).resolve())}
    for binary in [args.binary, args.host]:
        dependency_file = Path(binary + ".d")
        dependencies = dependency_file.read_text().splitlines()[0].split(": ", 1)[1]
        source_paths.update(path for path in shlex.split(dependencies) if Path(path).is_file())
    source_hashes = {path: digest(path) for path in sorted(source_paths)}

    def run_tmux(*values):
        return subprocess.check_output([*tmux, *values], text=True)

    def pane():
        return run_tmux("capture-pane", "-t", session, "-p", "-J")

    def wait_for(predicate, description, seconds=12):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            result = predicate()
            if result:
                return result
            if host.poll() is not None:
                raise RuntimeError(f"fixture Runtime exited {host.returncode}: {description}")
            time.sleep(0.1)
        raise AssertionError(f"Timed out: {description}")

    def wait_text(text):
        return wait_for(lambda: (text in pane()) and pane(), text)

    def wait_shell():
        return wait_for(lambda: pane().rstrip().splitlines()[-1].strip() == "READY_TO_LAUNCH", "interactive shell ready")

    def key(*values):
        run_tmux("send-keys", "-t", session, *values)

    def text(value):
        run_tmux("send-keys", "-t", session, "-l", value)

    def stage(number, name, screen=None):
        if screen is None:
            screen = pane()
        path = out / f"{number:02}-{name}.txt"
        path.write_text(screen)
        stages.append({"stage": number, "name": name, "capture": str(path), "result": "passed"})
        print(f"stage {number}: {name}", flush=True)

    def get(sid):
        value = rpc.call("session.get", {"session_id": sid})
        rpc_evidence.append({"method": "session.get", "result": value})
        return value

    def all_events(sid):
        records = []
        after = 0
        while True:
            page = rpc.call("session.events", {"session_id": sid, "after": after, "limit": 100})
            if not page:
                return records
            records.extend(page)
            after = page[-1]["cursor"]

    def cancel_original_task(sid, task_id):
        tasks = rpc.call("session.tasks", {"session_id": sid})
        task, = [task for task in tasks if task["id"] == task_id]
        assert task["session_id"] == sid
        target = {"session_id": sid, "task_id": task_id,
                  "worker_id": task["worker_id"], "attempt_id": task["attempt_id"],
                  "fence": task["fence"]}
        result = rpc.call("session.cancel", target)
        rpc_evidence.append({"method": "session.cancel", "params": target, "result": result})

    def select_session(sid):
        # Fresh Apps follow the host's session order, which is not creation order.
        wait_text("Sessions refreshed.")
        sessions = rpc.call("session.list", {})
        index = next(index for index, value in enumerate(sessions) if value["id"] == sid)
        key("Tab", *(["Up"] * len(sessions)), *(["Down"] * index), "Enter")

    try:
        address = host.stdout.readline().strip()
        assert address.startswith("127.0.0.1:"), address
        fixture = json.loads(info.read_text())
        rpc = Rpc(address, token)
        launch_env = out / "launch-env.json"
        launch_env.write_text(json.dumps({"CAYW_IPC_ADDR": address, "CAYW_IPC_TOKEN": token}))
        os.chmod(launch_env, 0o600)
        # The token travels through a private launch file, never captured terminal text.
        launcher = out / "launch.py"
        launcher.write_text("import json, os, sys\nfrom pathlib import Path\nenv=json.loads(Path(sys.argv[1]).read_text())\nos.environ.update(env)\nos.execv(sys.argv[2], [sys.argv[2]])\n")
        command = " ".join(shlex.quote(value) for value in ["python3", "-I", str(launcher), str(launch_env), args.binary])
        wrapped = "stty -g > " + shlex.quote(str(out / "stty-before.txt")) + "; " + command + "; result=$?; stty -g > " + shlex.quote(str(out / "stty-after.txt")) + "; printf 'TUI_EXIT=%s\\n' \"$result\"; PS1=READY_TO_LAUNCH exec /bin/sh -i"
        run_tmux("new-session", "-d", "-s", session, "-x", "140", "-y", "42", wrapped)
        stage(1, "connected", wait_text("connected"))
        stage(2, "authoritative-frozen-binding", wait_text("Session frozen"))
        key("C-f")
        stage(3, "fork-confirmation", wait_text("Confirm chat Fork"))
        key("n")
        wait_for(lambda: "Confirm chat Fork" not in pane(), "fork declined")
        assert len(rpc.call("session.list", {})) == 1
        stage(4, "fork-declined")
        key("C-f")
        wait_text("Confirm chat Fork")
        key("y")
        wait_text("Message · Enter send")
        sessions = rpc.call("session.list", {})
        chat = next(value for value in sessions if value["id"] != fixture["frozen_id"])
        sid = chat["id"]
        assert not chat["frozen"] and chat["binding"] is None
        stage(5, "fork-created")
        # Bracketed paste preserves the Unicode draft and its explicit newline.
        run_tmux("send-keys", "-t", session, "-l", "\x1b[200~draft 你好\nsecond line\x1b[201~")
        stage(6, "unicode-multiline-draft", wait_text("second line"))
        key("Enter")
        stage(7, "actual-stream", wait_text("streamed fixture"))
        stage(8, "host-approval", wait_text("Alt+O allow once"))
        approval = wait_for(lambda: (view := get(sid))["pending_approvals"] and view, "actual host approval")
        assert approval["unresolved_calls"]
        assert not Path(fixture["workspace"], "fixture-output.txt").exists()
        key("M-o")
        stage(9, "approved-tool-terminal", wait_text("fixture completed"))
        terminal = wait_for(lambda: (view := get(sid))["state"] == "idle" and not view["unresolved_calls"] and view, "known settled tool result")
        assert Path(fixture["workspace"], "fixture-output.txt").read_text() == "offline terminal verification"
        assert len(rpc.call("session.tasks", {"session_id": sid})) == 1
        # A second ordinary TUI turn validates cancellation without direct state writes.
        text("cancel this queued work")
        key("Enter")
        wait_text("Alt+O allow once")
        key("C-x")
        stage(10, "user-cancel-terminal", wait_text("Message · Enter send"))
        cancelled = wait_for(lambda: (view := get(sid))["state"] == "idle" and not view["pending_approvals"] and view, "cancelled attempt settled")
        assert cancelled["attempt"] is None and not cancelled["unresolved_calls"]
        # External connection starts a real run while this TUI retains an unsent draft.
        text("retained draft")
        key("C-r")
        stage(11, "reconnect-keeps-draft", wait_text("retained draft"))
        wait_text("Message · Enter send")
        # A fresh process attaches after the actual stream was produced, the review bug.
        run_id = rpc.start("session.prompt", {"session_id": sid, "text": "attach to existing run"})
        wait_for(lambda: get(sid)["pending_approvals"], "external run reached host approval")
        original_task, = [task for task in rpc.call("session.tasks", {"session_id": sid})
                          if task["state"] == "running"]
        original_task_id = original_task["id"]
        key("C-q")
        wait_for(lambda: "CAYW  •  connected" not in pane(), "first process quit")
        wait_text("TUI_EXIT=0")
        wait_shell()
        text(wrapped)
        key("Enter")
        select_session(sid)
        stage(12, "fresh-process-attachment", wait_text("Alt+O allow once"))
        run_tmux("resize-window", "-t", session, "-x", "48", "-y", "15")
        wait_text("connected")
        run_tmux("resize-window", "-t", session, "-x", "140", "-y", "42")
        stage(13, "resize-and-live-host-state", wait_text("Alt+O allow once"))
        key("C-q")
        wait_for(lambda: "CAYW  •  connected" not in pane(), "TUI quit")
        # Only explicit RPC cancellation clears the externally owned work for cleanup.
        assert get(sid)["pending_approvals"], "quitting an attachment must preserve the other connection's run"
        cancel_original_task(sid, original_task_id)
        rpc.finish(run_id)
        wait_shell()
        rpc_evidence.append({"check": "terminal mode restoration", **restored_terminal(out)})
        rpc_evidence.append({"method": "session.events", "result": all_events(sid)})
        # A second real host holds unfinished text, so a newly launched TUI must
        # recover a message_update that was emitted before its first snapshot.
        rpc.close()
        rpc = None
        host.terminate()
        host.wait(timeout=10)
        release_file = out / "stream-release"
        gate_info = out / "gate-fixture-info.json"
        gated_env = dict(env, CAYW_FIXTURE_INFO=str(gate_info), CAYW_FIXTURE_STREAM_RELEASE=str(release_file))
        host = subprocess.Popen([args.host], cwd="/tmp", env=gated_env, stdout=subprocess.PIPE, stderr=host_log, text=True)
        address = host.stdout.readline().strip()
        fixture = json.loads(gate_info.read_text())
        rpc = Rpc(address, token)
        sid = rpc.call("session.create", {"label": "unfinished attach"})["id"]
        run_id = rpc.start("session.prompt", {"session_id": sid, "text": "gated unfinished stream"})
        wait_for(lambda: any(record["event"].get("event", {}).get("message", {}).get("content", []) == [{"type":"text","text":"streamed fixture 你好"}] for record in all_events(sid)), "unfinished stream delta recorded")
        initial = get(sid)
        original_task, = [task for task in rpc.call("session.tasks", {"session_id": sid})
                          if task["state"] == "running"]
        original_task_id = original_task["id"]
        assert initial["state"] == "running" and not initial["pending_approvals"]
        assert not any(message.get("role") == "assistant" for message in initial["messages"])
        launch_env.write_text(json.dumps({"CAYW_IPC_ADDR": address, "CAYW_IPC_TOKEN": token}))
        wait_shell()
        text(wrapped)
        key("Enter")
        select_session(sid)
        wait_text("assistant · streaming")
        screen = wait_text("streamed fixture")
        (out / "fresh-unfinished-stream.txt").write_text(screen)
        rpc_evidence.append({"method":"session.get", "result":initial, "check":"fresh unfinished attachment"})
        key("C-r")
        wait_text("assistant · streaming")
        assert "streamed fixture" in pane()
        key("C-q")
        wait_for(lambda: "CAYW  •  connected" not in pane(), "unfinished attachment quit")
        assert get(sid)["state"] == "running", "quitting must not cancel the external stream"
        cancel_original_task(sid, original_task_id)
        rpc.finish(run_id)
        wait_shell()
        rpc_evidence.append({"check": "terminal mode restoration", **restored_terminal(out)})
        stage(14, "quit-restores-terminal")
        manifest = {"platform": os.uname()._asdict() if hasattr(os.uname(), "_asdict") else list(os.uname()),
                    "provider": "offline deterministic input; actual Runtime/Core/Host/IPC", "address": address,
                    "binary": {"path": args.binary, "sha256": digest(args.binary)},
                    "host": {"path": args.host, "sha256": digest(args.host)}, "stages": stages,
                    "source_sha256": source_hashes,
                    "source_unchanged_during_run": all(digest(path) == value for path, value in source_hashes.items()),
                    "model_calls": "no external model service", "result": "passed"}
        (out / "manifest.json").write_text(json.dumps(manifest, indent=2, ensure_ascii=False))
        (out / "rpc-evidence.json").write_text(json.dumps(rpc_evidence, indent=2, ensure_ascii=False))
    finally:
        if len(stages) != 14:
            try:
                (out / "failure-pane.txt").write_text(pane())
            except subprocess.CalledProcessError:
                pass
        if rpc is not None:
            rpc.close()
        subprocess.run([*tmux, "kill-session", "-t", session], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        host.terminate()
        try:
            host.wait(timeout=10)
        except subprocess.TimeoutExpired:
            host.kill()
            host.wait()
        host_log.close()
        secret = out / "launch-env.json"
        if secret.exists():
            secret.unlink()
        if len(stages) != 14:
            (out / "failure.json").write_text(json.dumps({"stages": stages, "host_exit": host.returncode}, indent=2))


if __name__ == "__main__":
    main()
