#!/usr/bin/env python3
"""Authorized real-model regression for the three 2026-11-17 trace findings."""
import argparse
import concurrent.futures
import hashlib
import importlib.util
import json
from pathlib import Path
import shutil
import subprocess
import time

spec = importlib.util.spec_from_file_location("live_suite", Path(__file__).with_name("validate-live-suite.py"))
live = importlib.util.module_from_spec(spec)
spec.loader.exec_module(live)


def run(root):
    # Cargo may replace target/debug binaries during parallel workspace checks.
    # Keep the complete executable inputs used by this run and hash those copies.
    frozen = root.with_name(root.name + "-binaries")
    frozen.mkdir(mode=0o700)
    for name in ["cayw", "cayw-webui", "cayw-visual-editor", "cayw-trace-debug", "cayw-tui"]:
        shutil.copy2(live.BIN / name, frozen / name)
    shutil.copy2(live.CLIENT, frozen / "cayw-client")
    live.BIN = frozen
    live.CLIENT = frozen / "cayw-client"
    suite = live.Suite(root)
    try:
        suite.setup()
        suite.stop()
        node = str(Path(shutil.which("node")).resolve())
        config_file = suite.private / "cayw/config.json"
        config = json.loads(config_file.read_text())
        config["profiles"].update({
            "short-wait": {"kind": "test", "program": node, "args": ["-e", "setTimeout(()=>console.log('short-operation-completed'),2000)"], "allow_filter": False},
            "unknown-effect": {"kind": "test", "program": node, "args": ["-e", "const fs=require('node:fs');fs.appendFileSync('effect.txt','effect\\n');fs.writeFileSync('effect-owner.json',JSON.stringify({pid:process.pid}));setInterval(()=>{},1000)"], "timeout_ms": 1000, "allow_filter": False},
        })
        config["permissions"] = [
            {"capability": "test.run", "arguments": {"profile": "short-wait"}},
            {"capability": "test.run", "arguments": {"profile": "unknown-effect"}},
            {"capability": "fs.write", "arguments": {"path": "resumed.txt", "content": "verified-next-write"}},
        ]
        live.save(config_file, config)
        invocation = json.loads((root / "invocation.json").read_text())
        invocation["config"] = config
        invocation["binaries_dir"] = str(frozen)
        changed = subprocess.check_output(["git", "diff", "--name-only", "HEAD"], cwd=live.REPO, text=True).splitlines()
        added = subprocess.check_output(["git", "ls-files", "--others", "--exclude-standard"], cwd=live.REPO, text=True).splitlines()
        invocation["source_files_sha256"] = {name: hashlib.sha256((live.REPO / name).read_bytes()).hexdigest() for name in sorted(set(changed + added)) if (live.REPO / name).is_file()}
        invocation["harness_sha256"][str(Path(__file__).relative_to(live.REPO))] = hashlib.sha256(Path(__file__).read_bytes()).hexdigest()
        live.save(root / "invocation.json", invocation)
        suite.start()
        approval = suite.create("Real model approves and reads one exact raw value")
        live.save(root / "approval-session.json", {"session_id": approval})
        suite.prompt(approval, "只读取一次 approval-probe。execute_code 脚本严格用 return load('approval-probe')，不要在 Luau 中判断返回值的类型或真假。遇到待审批，用新的 cell 调 approval.pending 和 approval.decide，按本次只读需求自行允许，再 wait 原 cell 到结束。不要重新执行读取，不要写文件。直接按原 cell 的 JSON 输出说明结果。", "approval")
        suite.verify_approval()
        suite.deny_approval()

        short = suite.create("Real model waits for a batch past the old one-second yield")
        suite.prompt(short, "只调用一次 execute_code，参数严格为 action='exec'、script=以下代码。必须省略 yield_time_ms。不要先查工具或输出中间进度。代码：local a=cayw.call('test.run',{profile='short-wait'}); local b=cayw.call('test.run',{profile='short-wait'}); return {a=a,b=b}。完成后根据真实结果回复。", "short-wait")
        rows = suite.events(short, "short-wait")
        starts = [r for r in rows if r["event"]["type"] == "agent" and r["event"]["event"]["type"] == "tool_execution_start"]
        assert len(starts) == 1, starts
        assert starts[0]["event"]["event"]["call"]["arguments"]["action"] == "exec"
        assert "yield_time_ms" not in starts[0]["event"]["event"]["call"]["arguments"]
        pages = [r["event"]["event"]["result"]["structured_content"] for r in rows if r["event"]["type"] == "agent" and r["event"]["event"]["type"] == "tool_execution_end"]
        assert len(pages) == 1 and pages[0]["vm_alive"] is False and pages[0]["status"] == "completed", pages
        calls = [r["event"]["call"] for r in rows if r["event"]["type"] == "capability_settled"]
        assert len(calls) == 2 and all(c["outcome"]["type"] == "completed" and c["outcome"]["result"]["success"] for c in calls)
        live.save(root / "short-wait-result.json", {"passed": True, "session_id": short, "model_tool_calls": 1, "extra_wait_calls": 0})

        session = suite.create("Real model unknown process effect and local operator reconciliation")
        owner = suite.workspace("lifecycle") / "effect-owner.json"
        with concurrent.futures.ThreadPoolExecutor(max_workers=1) as pool:
            result = pool.submit(suite.prompt, session, "只运行一次 execute_code：return cayw.call('test.run',{profile='unknown-effect'})。省略 yield_time_ms。这个固定进程会写 effect.txt 后被宿主超时中断。不要重试、不要重做写入、不要报告成功。", "unknown-effect", good=False)
            while not owner.exists():
                if result.done():
                    raise AssertionError("model returned without starting the intended process: " + str(result.result()))
                time.sleep(0.02)
            pid = json.loads(owner.read_text())["pid"]
            process_start = suite.process_start(pid)
            result.result()
        assert suite.process_start(pid) != process_start or suite.process_start(pid) is None, "original process incarnation is still live"
        effect = suite.workspace("lifecycle") / "effect.txt"
        assert effect.read_text() == "effect\n"
        before = suite.events(session, "unknown-before-confirmation")
        original = next(r["event"]["call"] for r in before if r["event"]["type"] == "capability_settled" and r["event"]["call"]["name"] == "test.run")
        assert original["outcome"]["type"] == "outcome_unknown", original
        tasks = suite.rpc("session.tasks", {"session_id": session})
        assert len(tasks) == 1 and tasks[0]["state"] == "outcome_unknown" and not tasks[0]["holds_slot"], tasks
        suite.command([live.BIN / "cayw", "reconcile", "--workspace", suite.workspace("lifecycle"), "--inspect", session], "inspect-unknown")
        expected = {name: original[name] for name in ["lease", "task_id", "parent_call_id", "source_hash"]}
        expected["execution_id"] = original["key"]["execution_id"]
        decision = {"type": "capability", "reconciliation": {
            "key": original["key"], "expected": expected, "actor": "rpc-user",
            "evidence": json.dumps({"operator": "authorized regression harness", "effect_sha256": hashlib.sha256(effect.read_bytes()).hexdigest(), "effect_content": effect.read_text(), "original_pid": pid, "observed_process_start": process_start, "process_stopped": True, "original_task": tasks[0]}, ensure_ascii=False),
            "outcome": {"type": "failed", "error": "Operator verified one effect.txt write and stopped process; execution was interrupted without a successful exit."},
            "external_provenance": original["external_provenance"],
        }}
        decision_file = suite.private / "verified-decision.json"
        live.save(decision_file, decision)
        for name in ["confirm", "confirm-exact-retry"]:
            suite.command([live.BIN / "cayw", "reconcile", "--workspace", suite.workspace("lifecycle"), "--decision", decision_file], name)
        suite.prompt(session, "这是核对结果后的新任务。只运行一次 execute_code：return cayw.call('fs.write',{path='resumed.txt',content='verified-next-write'})。不得重新执行旧进程或改写 effect.txt。", "write-after-confirmation")
        assert (suite.workspace("lifecycle") / "resumed.txt").read_text() == "verified-next-write"
        assert effect.read_text() == "effect\n"
        after = suite.events(session, "unknown-after-confirmation")
        assert after[:len(before)] == before, "original unknown evidence changed"
        calls = [r["event"]["call"] for r in after if r["event"]["type"] == "capability_started" and r["event"]["call"]["name"] == "test.run"]
        assert len(calls) == 1 and calls[0]["key"] == original["key"], "original process replayed"
        decisions = [r for r in after if r["event"]["type"] == "capability_reconciled"]
        assert len(decisions) == 1, "exact retry must not duplicate the decision"
        inspector = suite.rpc("session.run_inspector", {"session_id": session})
        parent = next(s for s in inspector["snapshot"]["steps"] if s["step_id"] == original["parent_call_id"])
        assert parent["outcome"] != "success", parent
        live.save(root / "unknown-recovery-result.json", {"passed": True, "session_id": session, "original_task": tasks[0], "decision": decision, "inspector_parent": parent, "process_dispatches": 1, "effect_lines": 1})
        live.save(root / "result.json", {"passed": True, "cases": ["model-auto-allow", "model-auto-deny", "default-wait-batch", "unknown-process-local-reconciliation", "exact-reconciliation-retry", "same-workspace-write-after-confirmation"]})
    finally:
        if (suite.private / "client-process.json").exists():
            suite.stop()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out", type=Path, required=True)
    run(parser.parse_args().out.resolve())
