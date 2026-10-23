#!/usr/bin/env python3
"""Start actual offline Runtime and WebUI binaries for reproducible browser checks.

Usage: python3 -I scripts/serve-webui-validation.py --out NEW_DIRECTORY
The two tokens below are public local-test fixtures, never production credentials.
Only loopback listeners and the synthetic provider are used. Each tool call still
requires explicit approval in the browser. Termination stops both child services.
"""
from __future__ import annotations

import argparse
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import queue
import signal
import subprocess
import sys
import threading

IPC_TOKEN = "cayw-local-browser-fixture-ipc-only"
WEB_TOKEN = "cayw-local-browser-fixture-web-only"


def first_line(child: subprocess.Popen) -> str:
    result: queue.Queue = queue.Queue(maxsize=1)
    threading.Thread(target=lambda: result.put(child.stdout.readline()), daemon=True).start()
    try:
        line = result.get(timeout=30)
    except queue.Empty:
        raise RuntimeError("service startup timed out; inspect the service log") from None
    if not line:
        raise RuntimeError("service exited before announcing readiness; inspect the service log")
    return line.strip()


def stop(child: subprocess.Popen) -> None:
    if child.poll() is None:
        child.terminate()
        try:
            child.wait(timeout=10)
        except subprocess.TimeoutExpired:
            child.kill()
            child.wait(timeout=10)
    if child.stdout:
        child.stdout.close()


def main() -> int:
    root = Path(__file__).resolve().parent.parent
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--runtime", type=Path, default=root / "target/debug/cayw")
    parser.add_argument("--webui", type=Path, default=root / "target/debug/cayw-webui")
    parser.add_argument("--lifetime", type=int, default=600)
    args = parser.parse_args()
    if not 1 <= args.lifetime <= 3600:
        parser.error("--lifetime must be between 1 and 3600 seconds")
    runtime_binary = args.runtime.resolve(strict=True)
    web_binary = args.webui.resolve(strict=True)
    output = args.out.resolve()
    output.mkdir(parents=True, exist_ok=False)
    workspace = output / "workspace"
    workspace.mkdir()
    halted = threading.Event()
    for signum in (signal.SIGINT, signal.SIGTERM):
        signal.signal(signum, lambda *_: halted.set())
    children = []
    logs = []
    environment = os.environ.copy()
    environment["CAYW_BROWSER_FIXTURE_IPC"] = IPC_TOKEN
    evidence = {"started_utc": datetime.now(timezone.utc).isoformat(),
                "source": "synthetic provider; actual Runtime, Luau, Host and WebUI binaries",
                "permissions": "no configured grants; browser must explicitly approve each capability",
                "runtime_sha256": hashlib.sha256(runtime_binary.read_bytes()).hexdigest(),
                "webui_sha256": hashlib.sha256(web_binary.read_bytes()).hexdigest()}
    try:
        runtime_log = (output / "runtime.log").open("w")
        logs.append(runtime_log)
        runtime = subprocess.Popen([str(runtime_binary), "serve", "--db", str(output / "sessions.sqlite"),
                                    "--workspace", str(workspace), "--listen", "127.0.0.1:0",
                                    "--token-env", "CAYW_BROWSER_FIXTURE_IPC", "--offline"],
                                   env=environment, stdout=subprocess.PIPE, stderr=runtime_log, text=True)
        children.append(runtime)
        ready = json.loads(first_line(runtime))
        if ready.get("offline_fixture") is not True:
            raise RuntimeError("Runtime did not confirm the offline fixture")
        environment.update(CAYW_IPC_ADDR=ready["listening"], CAYW_IPC_TOKEN=IPC_TOKEN,
                           CAYW_WEB_TOKEN=WEB_TOKEN, CAYW_WEB_ADDR="127.0.0.1:0")
        gateway_log = (output / "webui.log").open("w")
        logs.append(gateway_log)
        gateway = subprocess.Popen([str(web_binary)], env=environment, stdout=subprocess.PIPE,
                                   stderr=gateway_log, text=True)
        children.append(gateway)
        line = first_line(gateway)
        prefix = "网页界面：http://"
        if not line.startswith(prefix):
            raise RuntimeError("WebUI did not announce the expected loopback URL")
        address = line[len(prefix):].rstrip("/")
        evidence.update(url=f"http://{address}/", ipc_address=ready["listening"],
                        web_token=WEB_TOKEN, runtime_pid=runtime.pid, webui_pid=gateway.pid)
        (output / "connection.json").write_text(json.dumps(evidence, indent=2) + "\n")
        print(json.dumps({"url": evidence["url"], "web_token": WEB_TOKEN,
                          "connection": str(output / "connection.json")}), flush=True)
        # Drain remaining gateway announcements; stderr and runtime are otherwise quiet.
        threading.Thread(target=lambda: [None for _ in gateway.stdout], daemon=True).start()
        for _ in range(args.lifetime):
            if halted.wait(1):
                break
            if any(child.poll() is not None for child in children):
                raise RuntimeError("a validation service stopped unexpectedly")
        return 0
    except (OSError, ValueError, KeyError, RuntimeError) as error:
        evidence["error"] = str(error)
        print(str(error), file=sys.stderr)
        return 1
    finally:
        for child in reversed(children):
            stop(child)
        for log in logs:
            log.close()
        evidence["finished_utc"] = datetime.now(timezone.utc).isoformat()
        (output / "services.json").write_text(json.dumps(evidence, indent=2) + "\n")


if __name__ == "__main__":
    raise SystemExit(main())
