#!/usr/bin/env python3
"""Exercise the real headless binary over authenticated IPC without a model service.

Usage: python3 -I scripts/validate-headless.py --binary target/debug/cayw --out NEW_DIRECTORY
The synthetic provider emits Luau; the Rust runtime performs actual file operations.
"""
from __future__ import annotations

import argparse
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import platform
import queue
import secrets
import socket
import subprocess
import sys
import tempfile
import threading
import time


class Client:
    def __init__(self, address: str, token: str) -> None:
        host, port = address.rsplit(":", 1)
        self.connection = socket.create_connection((host, int(port)), timeout=30)
        self.reader = self.connection.makefile("rb")
        self.sequence = 0
        self.request("hello", {"token": token})

    def request(self, method: str, params: dict) -> object:
        self.sequence += 1
        identity = str(self.sequence)
        frame = json.dumps({"version": 1, "id": identity, "method": method, "params": params}).encode()
        if len(frame) > 1024 * 1024:
            raise ValueError("request exceeds harness frame limit")
        self.connection.sendall(frame + b"\n")
        line = self.reader.readline(1024 * 1024 + 2)
        if not line.endswith(b"\n") or len(line) > 1024 * 1024 + 1:
            raise ValueError("missing, truncated or oversized IPC response")
        result = json.loads(line)
        if result.get("version") != 1 or result.get("id") != identity:
            raise ValueError("mismatched IPC response identity or version")
        if ("error" in result) == ("result" in result):
            raise ValueError("IPC response requires exactly one result or error")
        if "error" in result:
            raise RuntimeError(json.dumps(result["error"]))
        return result["result"]

    def close(self) -> None:
        self.reader.close()
        self.connection.close()

    def __enter__(self) -> Client:
        return self

    def __exit__(self, *_: object) -> None:
        self.close()


class Server:
    def __init__(self, binary: Path, root: Path, token: str, logs: Path) -> None:
        environment = os.environ.copy()
        environment["CAYW_HARNESS_TOKEN"] = token
        self.stderr = (logs / f"server-{time.monotonic_ns()}.log").open("w")
        command = [str(binary), "serve", "--db", str(root / "sessions.db"),
                   "--workspace", str(root / "workspace"), "--listen", "127.0.0.1:0",
                   "--token-env", "CAYW_HARNESS_TOKEN", "--offline"]
        self.started = time.monotonic()
        self.child = subprocess.Popen(command, env=environment, stdout=subprocess.PIPE,
                                      stderr=self.stderr, text=True)
        ready: queue.Queue = queue.Queue(maxsize=1)
        threading.Thread(target=lambda: ready.put(self.child.stdout.readline()), daemon=True).start()
        try:
            line = ready.get(timeout=30)
            information = json.loads(line)
            if information.get("offline_fixture") is not True:
                raise ValueError("binary did not confirm offline fixture mode")
            self.address = information["listening"]
            self.startup_seconds = time.monotonic() - self.started
        except (queue.Empty, ValueError, KeyError):
            self.stop()
            raise RuntimeError("headless binary did not become ready; inspect server log") from None

    def stop(self) -> None:
        if self.child.poll() is None:
            self.child.terminate()
            try:
                self.child.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.child.kill()
                self.child.wait(timeout=10)
        if self.child.stdout:
            self.child.stdout.close()
        self.stderr.close()


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    output = args.out.resolve()
    output.mkdir(parents=True, exist_ok=False)
    summary = {"started_utc": datetime.now(timezone.utc).isoformat(),
               "platform": platform.platform(), "machine": platform.machine(),
               "python": sys.version, "binary": str(binary),
               "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
               "source": "synthetic provider, actual IPC/Rust/Luau/filesystem",
               "permissions": "no configured grants; each capability approved by this explicit test client",
               "input": "offline fixture creates and reads cayw-demo.txt", "checks": [], "passed": False}
    token = secrets.token_urlsafe(32)
    server = None
    try:
        with tempfile.TemporaryDirectory(prefix="cayw-headless-harness-") as temporary:
            root = Path(temporary).resolve()
            (root / "workspace").mkdir()
            server = Server(binary, root, token, output)
            summary["startup_seconds"] = server.startup_seconds
            try:
                with Client(server.address, token + "wrong"):
                    pass
            except (RuntimeError, ValueError, OSError):
                summary["checks"].append("wrong_token_rejected")
            else:
                raise AssertionError("incorrect IPC token was accepted")
            with Client(server.address, token) as control:
                health = control.request("runtime.health", {})
                assert health["status"] == "ok" and health["protocol_version"] == 1
                snapshot = control.request("session.create", {"label": "independent IPC validation"})
                identity = snapshot["id"]
                summary["session_id"] = identity
                approvals = []
                with ThreadPoolExecutor(max_workers=1) as executor:
                    def prompt() -> object:
                        with Client(server.address, token) as sender:
                            return sender.request("session.prompt", {"session_id": identity, "text": "Run the offline file round trip."})
                    future = executor.submit(prompt)
                    deadline = time.monotonic() + 60
                    while not future.done():
                        if time.monotonic() >= deadline:
                            control.request("session.cancel", {"session_id": identity})
                            raise TimeoutError("offline task did not settle within harness deadline")
                        snapshot = control.request("session.get", {"session_id": identity})
                        for approval in snapshot["pending_approvals"]:
                            if approval["id"] not in approvals:
                                control.request("approval.decide", {"session_id": identity,
                                                "approval_id": approval["id"], "decision": "allow_once"})
                                approvals.append(approval["id"])
                        time.sleep(0.02)
                    final = future.result()
                assert final["id"] == identity and final["state"] == "idle", final
                assert len(approvals) == 2, approvals
                assert (root / "workspace/cayw-demo.txt").read_text() == "headless runtime demo"
                summary["checks"].extend(["health_contract", "session_created", "real_luau_file_round_trip", "two_explicit_approvals"])
                records = []
                cursor = 0
                while True:
                    page = control.request("session.events", {"session_id": identity, "after": cursor, "limit": 7})
                    if not page:
                        break
                    for event in page:
                        assert event["session_id"] == identity and event["cursor"] == cursor + 1
                        cursor = event["cursor"]
                        records.append(event)
                assert cursor == final["cursor"] and cursor > 0
                (output / "events.jsonl").write_text("".join(json.dumps(record) + "\n" for record in records))
                try:
                    control.request("session.events", {"session_id": identity, "after": cursor + 1, "limit": 7})
                except RuntimeError:
                    summary["checks"].append("future_cursor_rejected")
                else:
                    raise AssertionError("future cursor was accepted")
                saved_cursor = cursor
                saved_messages = final["messages"]
            server.stop()
            server = Server(binary, root, token, output)
            with Client(server.address, token) as resumed:
                recovered = resumed.request("session.get", {"session_id": identity})
                assert recovered["id"] == identity and recovered["cursor"] == saved_cursor
                assert recovered["messages"] == saved_messages
                assert recovered["state"] == "idle"
                assert resumed.request("session.events", {"session_id": identity, "after": saved_cursor, "limit": 7}) == []
                summary["checks"].extend(["contiguous_durable_event_pages", "restart_preserves_session_history", "cursor_resume_without_prompt_replay"])
            server.stop()
            server = None
        summary["passed"] = True
    except (OSError, ValueError, AssertionError, RuntimeError, TimeoutError) as error:
        summary["error"] = str(error)
    finally:
        if server is not None:
            server.stop()
        summary["finished_utc"] = datetime.now(timezone.utc).isoformat()
        (output / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
        print(json.dumps(summary, indent=2))
    return 0 if summary["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
