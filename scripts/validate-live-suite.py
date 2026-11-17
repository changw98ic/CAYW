#!/usr/bin/env python3
"""Explicitly authorized local real-model E2E runs; no task replay or model retry."""
import argparse
import concurrent.futures
from datetime import datetime, timezone
import hashlib
import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import time
import threading
import urllib.error
import urllib.request
import uuid

REPO = Path(__file__).resolve().parent.parent
BIN = REPO / "target/debug"
CLIENT = REPO / "examples/headless-client/target/debug/cayw-client"


def save(path, value):
    path.write_text(json.dumps(value, ensure_ascii=False, indent=2) + "\n")


class GatedModel:
    """Forward real upstream bytes once, holding replies at an observed barrier."""
    def __init__(self, gate_when=lambda: True):
        self.entered = threading.Semaphore(0)
        self.release = threading.Event()
        self.receipts = []
        owner = self
        class Relay(BaseHTTPRequestHandler):
            def log_message(self, *args):
                return
            def do_POST(handler):
                body = handler.rfile.read(int(handler.headers["Content-Length"]))
                upstream = http.client.HTTPConnection("127.0.0.1", 3425, timeout=120)
                try:
                    upstream.request("POST", handler.path, body=body, headers={"Content-Type":"application/json"})
                    response = upstream.getresponse()
                    first = response.read1(65536)
                    owner.receipts.append({"request_sha256":hashlib.sha256(body).hexdigest(),
                        "upstream_status":response.status,"started_at":time.time()})
                    if gate_when():
                        owner.entered.release()
                        owner.release.wait()
                    handler.send_response(response.status)
                    handler.send_header("Content-Type", response.getheader("Content-Type"))
                    handler.send_header("Connection", "close")
                    handler.end_headers()
                    handler.wfile.write(first)
                    handler.wfile.flush()
                    while chunk := response.read1(65536):
                        handler.wfile.write(chunk)
                        handler.wfile.flush()
                finally:
                    upstream.close()
        self.server = ThreadingHTTPServer(("127.0.0.1",0),Relay)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
    def __enter__(self):
        self.thread.start()
        return self
    def __exit__(self, *args):
        self.release.set()
        self.server.shutdown()
        self.server.server_close()
        self.thread.join()


class Suite:
    def __init__(self, root):
        self.root = root.resolve()
        self.private = self.root / "private"
        self.evidence = self.root / "evidence"
        self.env = {k: os.environ[k] for k in ("PATH", "TMPDIR", "LANG") if k in os.environ}
        self.env.update(HOME=str(self.private / "home"), CAYW_HOME=str(self.private / "cayw"),
                        CAYW_MAIN_SESSION=str(self.private / "main-session"), NO_COLOR="1")

    def workspace(self, name):
        return self.root / name

    def command(self, args, name, good=True):
        prefix = self.evidence / (name + "-" + uuid.uuid4().hex)
        with prefix.with_suffix(".stdout.log").open("w") as out, prefix.with_suffix(".stderr.log").open("w") as err:
            p = subprocess.run([str(v) for v in args], env=self.env, cwd=REPO, stdout=out, stderr=err)
        save(prefix.with_suffix(".command.json"), {"args": [str(v) for v in args], "exit_code": p.returncode})
        if good and p.returncode:
            raise RuntimeError(f"{name}: exit {p.returncode}; {prefix}.stderr.log")
        return p.returncode, prefix.with_suffix(".stdout.log")

    def rpc(self, method, params=None, work="lifecycle", good=True):
        ready = json.loads((self.private / "web-ready.json").read_text())
        body = {"method": method, "params": {} if params is None else params}
        if work is not None:
            registry = self.rpc("settings.workspaces.list", work=None)
            body["workspace_id"] = next(w["id"] for w in registry["workspaces"]
                if w["path"] == str(self.workspace(work).resolve()))
        request = urllib.request.Request(ready["url"] + "api/rpc", data=json.dumps(body).encode(),
                  headers={"Content-Type": "application/json", "Origin": ready["url"].rstrip("/")})
        try:
            # task.resume is the outer execution request and has no task limit.
            with urllib.request.urlopen(request, timeout=None if method == "task.resume" else 120) as response:
                result = json.load(response)
        except urllib.error.HTTPError as error:
            result = json.load(error)
        save(self.evidence / ("rpc-" + method + "-" + uuid.uuid4().hex + ".json"), {"request": body, "response": result})
        if good and "error" in result:
            raise RuntimeError(method + ": " + json.dumps(result["error"]))
        return result["result"] if "result" in result else result

    def start(self):
        log = (self.evidence / ("client-" + uuid.uuid4().hex + ".stderr.log")).open("w")
        p = subprocess.Popen([str(BIN / "cayw"), "start", "--ui", "web", "--no-open",
                              "--workspace", str(self.workspace("lifecycle"))],
                             env=self.env, cwd=REPO, stdout=subprocess.PIPE, stderr=log,
                             text=True, start_new_session=True)
        ready = json.loads(p.stdout.readline())
        assert ready["owned_client"], "test must own its isolated Client"
        save(self.private / "web-ready.json", ready)
        save(self.private / "client-process.json", {"pid": p.pid, "started": self.process_start(p.pid)})
        p.stdout.close()
        log.close()
        return ready

    @staticmethod
    def process_start(pid):
        p = subprocess.run(["ps", "-p", str(pid), "-o", "lstart="], text=True, capture_output=True)
        return p.stdout.strip() if p.returncode == 0 else None

    def stop(self):
        owner = json.loads((self.private / "client-process.json").read_text())
        current = self.process_start(owner["pid"])
        if current is None:
            return
        assert current == owner["started"], "Client PID was reused"
        os.kill(owner["pid"], signal.SIGINT)
        while self.process_start(owner["pid"]) is not None:
            time.sleep(0.1)

    def create(self, label, work="lifecycle"):
        return self.rpc("session.create", {"label": label}, work)["id"]

    def prompt(self, session, text, name, work="lifecycle", good=True):
        (self.evidence / (name + ".prompt.txt")).write_text(text)
        return self.command([CLIENT, "--workspace", self.workspace(work), "prompt", "--session", session,
               "--text", text, "--cursor-file", self.private / (session + ".cursor.json")], name, good)

    def events(self, session, name, work="lifecycle"):
        after, records = 0, []
        while True:
            page = self.rpc("session.events", {"session_id": session, "after": after, "limit": 1000, "wait_ms": 0}, work)
            if not page:
                break
            assert [r["cursor"] for r in page] == list(range(after + 1, after + 1 + len(page)))
            records.extend(page)
            after = page[-1]["cursor"]
        save(self.evidence / (name + ".events.json"), records)
        return records

    def setup(self):
        self.root.mkdir(exist_ok=False)
        self.evidence.mkdir()
        self.private.mkdir(mode=0o700)
        (self.private / "home").mkdir()
        for name in ["lifecycle", "product"]:
            self.workspace(name).mkdir()
            self.command([BIN / "cayw", "init", "--workspace", self.workspace(name)], "init-" + name)
        node = str(Path(shutil.which("node")).resolve())
        self.command([node, "--input-type=module", "-e", "import {DatabaseSync} from 'node:sqlite'; console.log(typeof DatabaseSync)"], "node-preflight")
        profiles = {
            "node-tests": {"kind": "test", "program": node, "args": ["--test"], "allow_filter": False},
        }
        config = {"budgets": {"host_call_timeout_ms": 120000},
                  "scheduler": {"max_concurrent_runs": 2}, "profiles": profiles, "permissions": []}
        save(self.private / "cayw/config.json", config)
        ready = self.start()
        with urllib.request.urlopen("http://127.0.0.1:3425/v1/models", timeout=120) as response:
            catalog = json.load(response)
        selected = next(m for m in catalog["data"] if m["id"] == "opencode-go/deepseek-v4.1-flash")
        model = {"service": "magpie", "api": "chat_completions", "api_version": "v1",
                 "base_url": "http://127.0.0.1:3425/v1", "id": selected["id"],
                 "max_output_tokens": selected["max_output_tokens"], "context_window": selected["context_window"]}
        save(self.private / "model.json", model)
        self.command([BIN / "cayw", "models", "add", "e2e", "--file", self.private / "model.json",
                      "--workspace", self.workspace("lifecycle")], "model-add")
        for name in ["lifecycle", "product"]:
            self.command([BIN / "cayw", "models", "use", "e2e", "--workspace", self.workspace(name)], "model-use-" + name)
        save(self.root / "invocation.json", {"source_commit": subprocess.check_output(["git","rev-parse","HEAD"],cwd=REPO,text=True).strip(),
             "source_diff_sha256": hashlib.sha256(subprocess.check_output(["git","diff","HEAD","--binary"],cwd=REPO)).hexdigest(),
             "harness_sha256": {str(path.relative_to(REPO)): hashlib.sha256(path.read_bytes()).hexdigest()
                 for path in [Path(__file__).resolve(), REPO / "scripts/prompts/team-taskdesk.txt"]},
             "model": model, "config": config, "platform": {"system": os.uname().sysname,
                 "release": os.uname().release, "machine": os.uname().machine},
             "binaries": {name: hashlib.sha256((BIN/name).read_bytes()).hexdigest()
                 for name in ["cayw","cayw-webui","cayw-visual-editor","cayw-trace-debug"]},
             "started_utc": datetime.now(timezone.utc).isoformat()})
        print(json.dumps(ready))

    def approval(self):
        session = self.create("Real model handles pending approval")
        save(self.root / "approval-session.json", {"session_id": session})
        self.prompt(session, "请用会话共享值的 load helper 读取 approval-probe。若缺少值，如实说明；若需要操作授权，请你自己按本次读取需求评估并决定，不要等人批准。不要写任何文件。", "approval")
        self.verify_approval()

    def verify_approval(self):
        session = json.loads((self.root / "approval-session.json").read_text())["session_id"]
        records = self.events(session, "approval")
        decisions = [r["event"]["approval"] for r in records if r["event"]["type"] == "approval_decided"]
        assert any(a["decision"]["actor"] == "model-auto" and a["decision"]["decision"] == "approve" for a in decisions), decisions
        scopes = [r["event"]["data"] for r in records if r["event"]["type"] == "host"
                  and r["event"]["kind"] == "approval_scope_decided"]
        assert any(s["actor"] == "model-auto" and s["choice"] in ["allow_once", "allow_task"] for s in scopes), scopes
        tasks = self.rpc("session.tasks", {"session_id": session})
        assert len(tasks) == 1 and tasks[0]["state"] == "completed" and not tasks[0]["holds_slot"]
        save(self.root / "approval-result.json", {"passed": True, "session_id": session, "decisions": decisions})

    def deny_approval(self):
        session = self.create("Real model denies a pending operation")
        save(self.root / "deny-session.json", {"session_id": session})
        self.prompt(session, "这是拒绝路径验收。用 execute_code 的 load helper 读取 deny-probe，以产生真实待审批项。看到该读取的待审批项后，你必须自己通过 approval.decide 选择 deny。不要批准，不要重新发起读取，不要写文件。等原 cell 返回明确的未执行结果，再如实说明。", "deny-approval")
        records = self.events(session, "deny-approval")
        decisions = [r["event"]["approval"] for r in records if r["event"]["type"] == "approval_decided"]
        assert len(decisions) == 1 and decisions[0]["decision"]["actor"] == "model-auto"
        assert decisions[0]["decision"]["decision"] == "deny", decisions
        calls = [r["event"]["call"] for r in records if r["event"]["type"] == "capability_settled"
                 and r["event"]["call"]["name"] == "cells.load"]
        assert len(calls) == 1 and calls[0]["outcome"]["type"] == "not_executed", calls
        tasks = self.rpc("session.tasks", {"session_id": session})
        assert len(tasks) == 1 and tasks[0]["state"] == "completed" and not tasks[0]["holds_slot"]
        save(self.root / "deny-result.json", {"passed": True, "session_id": session, "decisions": decisions, "calls": calls})

    def snapshot(self):
        print(json.dumps(self.rpc("runtime.health"), ensure_ascii=False))
        print(json.dumps(self.rpc("session.list"), ensure_ascii=False)[:5000])

    def queue(self):
        with GatedModel() as relay:
            try:
                self.select_relay("queue", relay)
                self.run_queue(relay)
            finally:
                self.restore_model()

    def select_relay(self, name, relay):
        model = json.loads((self.private / "model.json").read_text())
        model["base_url"] = f"http://127.0.0.1:{relay.server.server_port}/v1"
        save(self.private / (name+"-model.json"), model)
        self.command([BIN / "cayw","models","add",name+"-relay","--file",self.private / (name+"-model.json"),
                      "--workspace",self.workspace("lifecycle")], name+"-model-add")
        self.command([BIN / "cayw","models","use",name+"-relay","--workspace",self.workspace("lifecycle")], name+"-model-use")

    def restore_model(self):
        self.command([BIN / "cayw","models","use","e2e","--workspace",self.workspace("lifecycle")], "model-restore")

    def run_queue(self, relay):
        sessions = [self.create("Queue real model " + str(i)) for i in range(10)]
        tasks = [self.rpc("task.enqueue", {"session_id": s, "text":
                 "只回复 CAYW_QUEUE_"+str(i)+"，不调用工具。"}) for i,s in enumerate(sessions)]
        save(self.root / "queue-submitted.json", tasks)
        assert len(tasks) == 10 and all(t["state"] == "queued" for t in tasks)
        peak, observations = 0, []
        with concurrent.futures.ThreadPoolExecutor(max_workers=10) as pool:
            futures = [pool.submit(self.rpc, "task.resume", {"task_id": t["id"]}, good=False) for t in tasks]
            try:
                observed = 0
                while observed < 2:
                    if relay.entered.acquire(timeout=0.1):
                        observed += 1
                    if any(f.done() for f in futures):
                        raise AssertionError("queue task ended before two upstream replies: " + str([f.result() for f in futures if f.done()]))
                held = [self.rpc("task.get", {"task_id": t["id"]}) for t in tasks]
                save(self.root / "queue-blocked.json", held)
                assert sum(t["holds_slot"] for t in held) == 2, held
                assert sum(t["state"] == "queued" for t in held) == 8, held
            finally:
                relay.release.set()
            while not all(f.done() for f in futures):
                health = self.rpc("runtime.health")
                peak = max(peak, health["running"])
                observations.append(health)
                time.sleep(0.1)
            replies = [f.result() for f in futures]
        final = [self.rpc("task.get", {"task_id": t["id"]}) for t in tasks]
        for i, session in enumerate(sessions):
            self.events(session, "queue-" + str(i))
        assert all(t["state"] == "completed" and not t["holds_slot"] for t in final), final
        assert peak <= 2
        assert len(relay.receipts) == 10 and all(r["upstream_status"] == 200 for r in relay.receipts), relay.receipts
        save(self.root / "queue-result.json", {"passed": True, "submitted": tasks, "final": final,
             "health_observations": observations, "replies": replies, "upstream_receipts":relay.receipts})

    def cancel(self):
        checkpoint = self.workspace("lifecycle") / "checkpoint.txt"
        with GatedModel(gate_when=checkpoint.exists) as relay:
            try:
                self.select_relay("cancel", relay)
                self.run_cancel(relay, checkpoint)
            finally:
                self.restore_model()

    def run_cancel(self, relay, checkpoint):
        session = self.create("Real cancellation and restart recovery")
        save(self.root / "cancel-session.json", {"session_id": session})
        with concurrent.futures.ThreadPoolExecutor(max_workers=1) as pool:
            future = pool.submit(self.prompt, session,
                "先用 execute_code 执行 return cayw.call('fs.write', {path='checkpoint.txt', content='one-effect'})，写完后再详细解释这项操作及验证方法。", "cancel", good=False)
            while not relay.entered.acquire(timeout=0.1):
                if future.done():
                    raise AssertionError("model ended before its post-checkpoint request: " + str(future.result()))
            assert checkpoint.read_text() == "one-effect"
            tasks = self.rpc("session.tasks", {"session_id": session})
            assert len(tasks) == 1
            task = tasks[0]
            target = {key: task[key] for key in ["session_id", "worker_id", "attempt_id", "fence"]}
            target["task_id"] = task["id"]
            assert self.rpc("session.cancel", target)["cancelled"]
            relay.release.set()
            future.result()
        terminal = self.rpc("task.get", {"task_id": task["id"]})
        assert terminal["state"] == "cancelled" and not terminal["holds_slot"], terminal
        assert checkpoint.read_text() == "one-effect"
        before = self.events(session, "cancel-before-restart")
        self.restore_model()
        self.stop()
        self.start()
        self.recover_cancel()

    def recover_cancel(self):
        session = json.loads((self.root / "cancel-session.json").read_text())["session_id"]
        tasks = self.rpc("session.tasks", {"session_id": session})
        assert len(tasks) == 1 and tasks[0]["state"] == "cancelled", tasks
        terminal = tasks[0]
        checkpoint = self.workspace("lifecycle") / "checkpoint.txt"
        before = json.loads((self.evidence / "cancel-before-restart.events.json").read_text())
        self.command([CLIENT, "--workspace", self.workspace("lifecycle"), "recover", "--session", session], "explicit-recover")
        assert checkpoint.read_text() == "one-effect"
        after = self.events(session, "cancel-after-restart")
        count = lambda rows: sum(r["event"]["type"] == "host" and r["event"]["kind"] == "model_request"
                                  and r["event"]["data"]["record"] == "started" for r in rows)
        assert count(before) == count(after), "restart/recover replayed a model request"
        self.prompt(session, "读取已有 checkpoint.txt。不要重做已经完成的写入。把你实际读到的完整内容写到 recovered.txt。", "recovery")
        assert checkpoint.read_text() == "one-effect"
        assert (self.workspace("lifecycle") / "recovered.txt").read_text() == checkpoint.read_text()
        final_events = self.events(session, "recovery-final")
        assert sum(r["event"]["type"] == "capability_settled" and r["event"]["call"]["name"] == "fs.write"
                   and r["event"]["call"]["arguments"]["path"] == "checkpoint.txt" for r in final_events) == 1
        save(self.root / "cancel-result.json", {"passed": True, "session_id": session, "cancelled_task": terminal,
             "replayed_requests_on_recover": 0, "effect_lines": 1})

    def prepare_product(self):
        for name in ["checkpoint.txt", "recovered.txt"]:
            path = self.workspace("lifecycle") / name
            shutil.copyfile(path, self.evidence / ("recovery-" + name))
            path.unlink()
        session = self.create("Team collaboration task board", work="lifecycle")
        save(self.root / "product-session.json", {"session_id": session})

    def start_trace(self):
        session = json.loads((self.root / "product-session.json").read_text())["session_id"]
        web = json.loads((self.private / "web-ready.json").read_text())
        self.start_app("trace", [BIN / "cayw-trace-debug", "--store", self.private / "trace.json",
            "--workspace", self.workspace("lifecycle"), "--session", session],
            {"CAYW_IPC_ADDR":web["client"], "CAYW_IPC_TOKEN":(self.private / "cayw/ipc.token").read_text().strip(),
             "CAYW_TRACE_WEB_TOKEN":"cayw-e2e-trace-public-fixture-20261117"})

    def start_editor(self):
        assert (self.workspace("lifecycle") / "web/index.html").is_file()
        self.start_app("editor", [BIN / "cayw-visual-editor", "--project", self.workspace("lifecycle"),
            "--page", "web/index.html", "--state-dir", self.private / "editor-state"],
            {"CAYW_EDITOR_TOKEN":"cayw-e2e-editor-public-fixture-20261117"})

    def start_app(self, name, argv, extra):
        log = (self.evidence / (name + ".stderr.log")).open("w")
        p = subprocess.Popen([str(v) for v in argv], cwd=REPO, env={**self.env, **extra},
                             stdout=subprocess.PIPE, stderr=log, text=True, start_new_session=True)
        ready = json.loads(p.stdout.readline())
        save(self.private / (name + "-ready.json"), ready)
        save(self.private / (name + "-process.json"), {"pid":p.pid, "started":self.process_start(p.pid)})
        p.stdout.close()
        log.close()
        print(json.dumps({"application":name, "ready":ready}))

    def product(self):
        session = json.loads((self.root / "product-session.json").read_text())["session_id"]
        assert not self.rpc("session.tasks", {"session_id": session}, work="lifecycle"), "original task is submitted only once"
        original = (REPO / "scripts/prompts/team-taskdesk.txt").read_text()
        prompt = original + "\n宿主配置：Node 24 已验证，支持 node:sqlite。固定测试入口为 test.run {profile:'node-tests'}，执行 node --test，不传 filter。前端入口使用 web/index.html。读取工具定义后使用目录中的能力。工具需要授权时，根据工具结果里的实际待审批项自行决定。计划更新的 expected_version 首次用0，后续取上次 plan.update 结果中的 version。实际启动服务的 README 命令要打印监听 URL 并支持 PORT 环境变量。为独立可视化预览提供可选的 ?api= 后端地址参数（默认同源），后端仅对显式配置的本机预览 Origin 放行跨域，以便编辑器预览连接实际协作服务。"
        code, log = self.prompt(session, prompt, "product", work="lifecycle", good=False)
        self.events(session, "product", work="lifecycle")
        save(self.root / "product-run.json", {"session_id": session, "prompt_exit_code": code, "log": str(log)})
        if code:
            raise RuntimeError("product model task failed; see preserved log")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=["setup", "start", "stop", "approval", "verify_approval", "deny_approval", "queue", "cancel", "recover_cancel", "prepare_product", "start_trace", "start_editor", "product", "snapshot"])
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    getattr(Suite(args.out), args.mode)()
