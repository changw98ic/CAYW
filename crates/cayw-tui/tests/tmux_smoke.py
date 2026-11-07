"""Drive the real TUI against Runtime/Core/Host over IPC with offline model input.

Build first with:
  cargo +1.95.0 build -p cayw-tui --bin cayw-tui --example fixture_host --locked
Then run:
  python3 -I crates/cayw-tui/tests/tmux_smoke.py /absolute/path/to/CAYW/target/debug
"""
import hashlib
import json
import os
import platform
from pathlib import Path
import shlex
import signal
import socket
import subprocess
import sys
import tempfile
import time
import uuid

binary_dir = Path(sys.argv[1]).resolve()
socket_name = "cayw-tui-" + uuid.uuid4().hex
output_dir = Path(tempfile.mkdtemp(prefix="cayw-tui-smoke-"))
environment = dict(os.environ, CAYW_IPC_TOKEN="offline-fixture-token", CAYW_IPC_ADDR="127.0.0.1:0", CAYW_FIXTURE_INFO=str(output_dir / "fixture.json"))
fixture = subprocess.Popen([str(binary_dir / "examples" / "fixture_host")], env=environment, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
captures = {}
source_root = Path(__file__).resolve().parents[3]
source_paths = ["Cargo.lock", "Cargo.toml", "crates/cayw-tui/Cargo.toml"]
source_paths += ["crates/cayw-tui/" + path for path in ["src/main.rs", "src/lib.rs", "src/state.rs", "src/driver.rs", "src/transport.rs", "src/input.rs", "src/render.rs", "src/config.rs", "src/terminal.rs", "examples/fixture_host.rs", "tests/support/mod.rs", "tests/tmux_smoke.py", "tests/runtime_ipc.rs", "tests/regressions.rs"]]
source_paths += ["crates/cayw-runtime/src/" + path for path in ["lib.rs", "session.rs", "rpc.rs", "execution.rs", "plugins.rs"]]
source_paths += ["crates/cayw-ipc/src/client.rs", "crates/cayw-ipc/src/server.rs", "crates/cayw-core/src/store.rs"]
source_files = [source_root / path for path in source_paths]
report = {
    "platform":platform.platform(),
    "source_root":str(source_root),
    "source_sha256":{str(path.relative_to(source_root)):hashlib.sha256(path.read_bytes()).hexdigest() for path in sorted(source_files)},
    "binary":str(binary_dir / "cayw-tui"),
    "binary_sha256":hashlib.sha256((binary_dir / "cayw-tui").read_bytes()).hexdigest(),
    "host_sha256":hashlib.sha256((binary_dir / "examples" / "fixture_host").read_bytes()).hexdigest(),
    "fixture":"real Runtime/Core/Host and IPC; deterministic offline Provider",
    "permissions":"allow_once for fs.write in a temporary workspace; explicit user cancel and chat Fork",
    "result":"incomplete","captures":captures,
}

def tmux(*args, check=True):
    return subprocess.run(["tmux", "-L", socket_name, "-f", "/dev/null", *args], check=check, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=environment).stdout

def pane():
    return tmux("capture-pane", "-p", "-t", "smoke")

def wait_for(predicate, stage):
    deadline = time.monotonic() + 10
    latest = ""
    while time.monotonic() < deadline:
        latest = pane()
        if predicate(latest):
            captures[stage] = latest
            return latest
        time.sleep(0.05)
    captures[stage + "_timeout"] = latest
    raise AssertionError(f"{stage} timed out:\n{latest}")

def keys(*values):
    tmux("send-keys", "-t", "smoke", *values)

def text(value):
    tmux("send-keys", "-t", "smoke", "-l", value)


def rpc(method, params):
    host, port = environment["CAYW_IPC_ADDR"].rsplit(":", 1)
    with socket.create_connection((host, int(port)), timeout=5) as connection:
        stream = connection.makefile("rwb")
        def call(request_id, name, values):
            stream.write((json.dumps({"version":1,"id":request_id,"method":name,"params":values}) + "\n").encode())
            stream.flush()
            response = json.loads(stream.readline())
            assert response["id"] == request_id, response
            assert "error" not in response, response
            return response["result"]
        call("h", "hello", {"token":environment["CAYW_IPC_TOKEN"]})
        return call("smoke-check", method, params)


def model_records(events):
    return [record["event"]["data"] for record in events
            if record["event"].get("type") == "host"
            and record["event"].get("kind") == "model_request"]


try:
    assert fixture.stdout is not None
    address = fixture.stdout.readline().strip()
    assert address.startswith("127.0.0.1:"), address
    environment["CAYW_IPC_ADDR"] = address
    fixture_info = json.loads((output_dir / "fixture.json").read_text())
    frozen_id = fixture_info["frozen_id"]
    report["initial_frozen_view"] = rpc("session.get", {"session_id":frozen_id})
    assert report["initial_frozen_view"]["frozen"] is True
    assert report["initial_frozen_view"]["binding"]["plugin"] == "fixture-binding"
    assert report["initial_frozen_view"]["binding"]["reason"]
    command = shlex.quote(str(binary_dir / "cayw-tui")) + '; result=$?; printf "\\nTUI_EXIT=%s\\n" "$result"'
    tmux("new-session", "-d", "-s", "smoke", "-x", "110", "-y", "34", command)
    tmux("set-option", "-t", "smoke", "remain-on-exit", "on")
    wait_for(lambda screen: "Frozen fixture" in screen, "startup")
    keys("C-f")
    wait_for(lambda screen: "Confirm chat Fork" in screen, "fork_confirmation")
    keys("n")
    assert len(rpc("session.list", {})) == 1
    keys("C-n")
    wait_for(lambda screen: "Create session" in screen, "create_dialog")
    text("Terminal fixture")
    keys("Enter")
    wait_for(lambda screen: "Terminal fixture · idle" in screen, "created")
    text("tmux prompt 你好🙂")
    keys("Enter")
    wait_for(lambda screen: "streamed fixture" in screen and "assistant · streaming" in screen, "streamed_text")
    wait_for(lambda screen: "fs.write" in screen and "Approval " in screen, "tool_and_approval")
    keys("M-o")
    wait_for(lambda screen: "fixture completed" in screen and "Message · Enter send" in screen, "approved_and_completed")
    completed_sessions = rpc("session.list", {})
    completed = next(session for session in completed_sessions if session["label"] == "Terminal fixture")
    report["completed_model_records"] = model_records(rpc("session.events", {"session_id":completed["id"], "after":0, "limit":1000}))
    report["completed_context_overview"] = rpc("session.context_overview", {"session_id":completed["id"]})
    completed_records = report["completed_model_records"]
    finished = [record for record in completed_records if record["record"] == "finished"]
    assert len(finished) >= 2
    assert all(record["terminal"] == "completed" and record["settled_tokens"] == 4 for record in finished)
    assert not any(record["record"] == "rejected" for record in completed_records)
    done = [record["observation"] for record in completed_records
            if record["record"] == "observed" and record["observation"]["event"] == "done"]
    assert len(done) == len(finished)
    assert all(record["usage_status"] == "reported" and record["usage"]["input"] == 2
               and record["usage"]["output"] == 2 and record["usage"]["total_tokens"] == 4 for record in done)
    overview = report["completed_context_overview"]
    assert overview["budget_unknown"] is False and overview["ledger_reserved_tokens"] == 0
    assert overview["request_count"] == len(finished) and overview["ledger_settled_tokens"] == 4 * len(finished)
    assert overview["cost"]["status"] == "unknown"
    keys("M-4")
    wait_for(lambda screen: "Logs (" in screen and "capability_settled · fs.write · fixture-output.txt" in screen, "logs_view")
    keys("M-2")
    wait_for(lambda screen: "Files (1)" in screen and "fixture-output.txt" in screen and "offline terminal verification" in screen, "files_view")
    keys("M-3")
    wait_for(lambda screen: "Diff (1)" in screen and "+++ b/fixture-output.txt" in screen and "+offline terminal verification" in screen, "diff_view")
    keys("M-1")
    wait_for(lambda screen: "Chat" in screen and "fixture completed" in screen, "chat_view_restored")
    keys("F1")
    wait_for(lambda screen: "Help & Shortcuts" in screen, "help_dialog")
    keys("Escape")
    wait_for(lambda screen: "Help & Shortcuts" not in screen and "fixture completed" in screen, "help_closed")
    text("kept draft")
    keys("C-r")
    wait_for(lambda screen: "kept draft" in screen and "Sessions refreshed." in screen, "reconnected_with_draft")
    keys("Enter")
    wait_for(lambda screen: "fs.write" in screen and "Approval " in screen, "second_prompt")
    keys("C-x")
    wait_for(lambda screen: "Message · Enter send" in screen and "Approval " not in screen, "cancelled_and_settled")
    keys("Tab", "Up")
    wait_for(lambda screen: "Session frozen" in screen, "selected_frozen")
    keys("C-f")
    wait_for(lambda screen: "Confirm chat Fork" in screen, "fork_confirm_again")
    keys("y")
    wait_for(lambda screen: "Frozen fixture (chat fork) · idle" in screen, "fork_created")
    final_sessions = rpc("session.list", {})
    report["final_sessions"] = final_sessions
    final_source = next(session for session in final_sessions if session["id"] == frozen_id)
    assert final_source == report["initial_frozen_view"]
    forked = next(session for session in final_sessions if session["label"] == "Frozen fixture (chat fork)")
    assert forked["binding"] is None and forked["frozen"] is False
    active = next(session for session in final_sessions if session["label"] == "Terminal fixture")
    assert active["state"] == "idle" and not active["unresolved_calls"]
    report["terminal_events"] = rpc("session.events", {"session_id":active["id"], "after":0, "limit":1000})
    report["tasks"] = rpc("session.tasks", {"session_id":active["id"]})
    assert len(report["tasks"]) == 2
    assert sorted(task["state"] for task in report["tasks"]) == ["cancelled", "completed"]
    report["final_model_records"] = model_records(report["terminal_events"])
    report["final_context_overview"] = rpc("session.context_overview", {"session_id":active["id"]})
    output_file = Path(fixture_info["workspace"]) / "fixture-output.txt"
    report["actual_tool_output"] = output_file.read_text()
    assert report["actual_tool_output"] == "offline terminal verification"
    keys("C-q")
    wait_for(lambda screen: "TUI_EXIT=0" in screen, "restored_and_exited")
    assert all("offline-fixture-token" not in screen for screen in captures.values())
    report["result"] = "passed"
    print(json.dumps({"result":"passed","stages":list(captures),"evidence":str(output_dir / "panes.json")}))
finally:
    cleanup = subprocess.run(["tmux", "-L", socket_name, "kill-server"], stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=environment)
    remaining = subprocess.run(["tmux", "-L", socket_name, "list-sessions"], stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=environment)
    fixture.send_signal(signal.SIGINT)
    try:
        fixture.wait(timeout=3)
    except subprocess.TimeoutExpired:
        fixture.kill()
        fixture.wait(timeout=3)
    report["cleanup"] = {"tmux_socket":socket_name, "kill_server_exit_code":cleanup.returncode,
                         "remaining_sessions_exit_code":remaining.returncode,
                         "fixture_pid":fixture.pid, "fixture_exit_code":fixture.returncode,
                         "complete":remaining.returncode != 0 and fixture.poll() is not None}
    report["source_sha256_after"] = {str(path.relative_to(source_root)):hashlib.sha256(path.read_bytes()).hexdigest() for path in sorted(source_files)}
    report["source_unchanged_during_run"] = report["source_sha256"] == report["source_sha256_after"]
    (output_dir / "panes.json").write_text(json.dumps(report, ensure_ascii=False, indent=2))
