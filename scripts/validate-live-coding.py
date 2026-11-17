#!/usr/bin/env python3
"""Opt-in real-model C02 coding acceptance through the unique local Client.

Runs generated Rust tests. Uses a fresh isolated workspace/state, the product's
ModelAuto approval path, a fixed offline Cargo profile and independent checks.
No token/turn/task budget is selected by this runner. Call only with explicit
live-model authorization. This is not an operating-system security sandbox.
"""
import argparse
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import signal
import subprocess
import urllib.request

import product_eval as pe


def save(path, value):
    path.write_text(json.dumps(value, ensure_ascii=False, indent=2) + "\n")


def execution_kind(prompt_log):
    if prompt_log is None:
        return "not_run"
    with prompt_log.open() as lines:
        for line in lines:
            row = json.loads(line)
            event = row.get("event", {}).get("event", {})
            if (event.get("type") == "host" and event["kind"] == "model_request"
                    and event["data"]["record"] == "started"):
                return "live_model"
    return "not_run"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--runtime", type=Path, required=True)
    parser.add_argument("--client", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    runtime, client = args.runtime.resolve(strict=True), args.client.resolve(strict=True)
    root = args.out.resolve()
    pe.prepare("C02", root)
    workspace, review = root / "workspace", root / "review"
    private = root / "private"
    private.mkdir(mode=0o700)
    (private / "home").mkdir()
    (private / "cargo-target").mkdir()
    (workspace / "output").mkdir()
    evidence = review / "evidence"
    environment = {k: os.environ[k] for k in ("PATH", "TMPDIR", "LANG") if k in os.environ}
    environment.update(HOME=str(private / "home"), CAYW_HOME=str(private / "cayw"),
                       CAYW_MAIN_SESSION=str(private / "main-session"), NO_COLOR="1")
    sequence = 0
    prompt_log = None

    def command(program, arguments, name):
        nonlocal sequence, prompt_log
        sequence += 1
        prefix = evidence / (f"{sequence:03d}-" + name)
        if name == "prompt":
            prompt_log = prefix.with_suffix(".stdout.log")
        with prefix.with_suffix(".stdout.log").open("w") as out, prefix.with_suffix(".stderr.log").open("w") as err:
            result = subprocess.run([str(program), *arguments], cwd=workspace,
                                    env=environment, stdout=out, stderr=err)
        save(prefix.with_suffix(".command.json"), {"arguments": arguments, "exit_code": result.returncode})
        if result.returncode:
            raise RuntimeError(f"{name} exited {result.returncode}; see {prefix}.stderr.log")
        return prefix.with_suffix(".stdout.log")

    def result(path):
        rows = [json.loads(line) for line in path.read_text().splitlines() if line.strip()]
        return next(row["result"] for row in reversed(rows) if row.get("type") == "result")

    # Resolve the complete compiler toolchain before the first paid request.
    tools = {name: subprocess.check_output(
        ["rustup", "which", "--toolchain", "1.95.0", name], text=True).strip()
        for name in ("cargo", "rustc", "rustdoc")}
    tool_environment = {"RUSTC": tools["rustc"], "RUSTDOC": tools["rustdoc"],
                        "PATH": str(Path(tools["rustc"]).parent) + os.pathsep + os.defpath}
    test_args = ["test", "--locked", "--offline", "--target-dir", str(private / "cargo-target")]
    with (evidence / "toolchain-preflight.log").open("w") as log:
        preflight = subprocess.run([tools["cargo"], *test_args], cwd=workspace,
                                   env={**environment, **tool_environment}, stdout=log, stderr=subprocess.STDOUT)
    if preflight.returncode:
        raise RuntimeError("Cargo toolchain preflight failed before any model request")
    command(runtime, ["init", "--workspace", str(workspace)], "init")
    config = pe.read_json(review / "host.template.json")
    # Reuse the original acceptance's single Host-call deadline. No extra
    # process or intermediate waiting deadline is added.
    config["budgets"]["host_call_timeout_ms"] = 120000
    config["profiles"] = {"rust-tests": {"kind": "test", "program": tools["cargo"],
                          "args": test_args, "env": tool_environment, "allow_filter": False,
                          "io": {"kind": "pipes"}}}
    config["permissions"] = [{"capability": "fs.read", "arguments": {"path": name}}
                             for name in ("src/lib.rs", "Cargo.toml", "Cargo.lock")]
    config["permissions"].append({"capability": "test.run", "arguments": {"profile": "rust-tests"}})
    save(private / "cayw/config.json", config)
    model = {"service": "magpie", "api": "chat_completions", "api_version": "v1",
             "base_url": os.environ.get("MAGPIE_BASE_URL", "http://127.0.0.1:3425/v1"),
             "id": os.environ.get("MAGPIE_MODEL", "opencode-go/deepseek-v4.1-flash")}
    if "MAGPIE_API_KEY" in os.environ:
        model["api_key"] = os.environ["MAGPIE_API_KEY"]
    headers = {"Authorization": "Bearer " + model["api_key"]} if "api_key" in model else {}
    request = urllib.request.Request(model["base_url"].rstrip("/") + "/models", headers=headers)
    with urllib.request.urlopen(request, timeout=config["budgets"]["host_call_timeout_ms"] / 1000) as response:
        catalog = json.load(response)
    selected = next(entry for entry in catalog["data"] if entry["id"] == model["id"])
    model["max_output_tokens"] = selected["max_output_tokens"]
    if "context_window" in selected:
        model["context_window"] = selected["context_window"]
    save(evidence / "selected-model.json", {key: model[key] for key in
         ("id", "max_output_tokens", "context_window") if key in model})
    model_file = private / "model.json"
    with open(model_file, "x", opener=lambda path, flags: os.open(path, flags, 0o600)) as file:
        json.dump(model, file)
    source = subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip()
    public_model = {k: v for k, v in model.items() if k != "api_key"}
    save(review / "invocation.json", {"source_commit": source,
         "source_diff_sha256": pe.digest(subprocess.check_output(["git", "diff", "HEAD", "--binary"])),
         "runner_sha256": pe.digest(Path(__file__).read_bytes()),
         "model": public_model, "authentication": "key" if "api_key" in model else "none",
         "binaries": {str(p): pe.digest(p.read_bytes()) for p in (runtime, client)},
         "config": config, "approval": "local Client ModelAuto", "started_utc": datetime.now(timezone.utc).isoformat()})
    prompt = (review / "prompt.txt").read_text() + (
        "\n宿主已创建output目录，配置固定测试入口 test.run {profile:\"rust-tests\"}，"
        "完整Cargo工具链已在发送任务前验证。文件操作使用fs.read/fs.write。"
        "说明只引用源码和实际执行输出能证明的事实；需要描述旧实现的具体行为时先实测。"
        "一次完整测试通过且说明准确后直接最终交付，无需反复重写已经核验的文件。")
    (review / "actual-prompt.txt").write_text(prompt)
    server = None
    summary = {"execution_kind": "not_run", "machine_passed": False, "human_source_repairs": 0,
               "human_interventions_during_task": 0}
    try:
        with (evidence / "server.stderr.log").open("w") as errors:
            server = subprocess.Popen([str(runtime), "serve", "--workspace", str(workspace)],
                                      cwd=workspace, env=environment, stdout=subprocess.PIPE,
                                      stderr=errors, text=True)
            ready_line = server.stdout.readline()
            (evidence / "server-ready.json").write_text(ready_line)
            ready = json.loads(ready_line)
            if not ready.get("listening"):
                raise RuntimeError("Client did not become ready")
            command(runtime, ["models", "add", "live", "--file", str(model_file), "--workspace", str(workspace)], "model-add")
            command(runtime, ["models", "use", "live", "--workspace", str(workspace)], "model-use")
            prefix = ["--workspace", str(workspace)]
            session_id = result(command(client, prefix + ["create", "--label", "C02-live"], "create"))["id"]
            prompt_log = command(client, prefix + ["prompt", "--session", session_id, "--text", prompt,
                                                  "--cursor-file", str(private / "events.cursor.json")], "prompt")
            state = result(command(client, prefix + ["get", "--session", session_id], "final-state"))
            tasks = result(command(client, prefix + ["tasks", "--session", session_id], "final-tasks"))
            save(evidence / "final-state.json", state)
            save(evidence / "final-tasks.json", tasks)
            if len(tasks) != 1 or tasks[0]["state"] != "completed" or tasks[0]["holds_slot"]:
                raise AssertionError("original task did not complete and release its slot")
            if state["state"] != "idle" or state["pending_approvals"] or state["unresolved_calls"] or state["unresolved_capabilities"]:
                raise AssertionError("session has unsettled work")
            calls = []
            for line in prompt_log.read_text().splitlines():
                row = json.loads(line)
                event = row.get("event", {}).get("event", {})
                if event.get("type") == "capability_settled" and event["call"]["name"] == "test.run":
                    calls.append(event["call"])
            save(evidence / "model-test-calls.json", calls)
            if not any(c["outcome"]["type"] == "completed" and
                       c["outcome"]["result"]["success"] and
                       c["outcome"]["result"]["output_complete"] and
                       c["outcome"]["result"]["exit_code"] == 0 for c in calls):
                raise AssertionError("model did not execute a successful complete test run")
            meta, files, outputs, expected = pe.validate_run(root)
            checks = pe.machine_checks(root, meta, files, outputs, expected, allow_exec=True, rustc=tools["rustc"])
            save(review / "independent-checks.json", checks)
            if not all(c["status"] == "passed" for c in checks):
                raise AssertionError("independent artifact checks failed")
            summary.update(machine_passed=True, session_id=session_id, task_id=tasks[0]["id"],
                           checks=checks, documentation_review="required separately; not a model self-score")
    except Exception as error:
        summary["error"] = str(error)
        raise
    finally:
        if server is not None:
            server.stdout.close()
            if server.poll() is None:
                server.send_signal(signal.SIGINT)
            summary["server_exit_code"] = server.wait()
            if summary["server_exit_code"]:
                summary["machine_passed"] = False
        summary["finished_utc"] = datetime.now(timezone.utc).isoformat()
        summary["execution_kind"] = execution_kind(prompt_log)
        save(review / "summary.json", summary)
        print(json.dumps(summary, ensure_ascii=False))
    return 0 if summary["machine_passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
