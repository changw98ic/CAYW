#!/usr/bin/env python3
"""Prepare controlled tasks; independently grade outputs; summarize reviewed runs.

No model calls, runtime replacement, automatic approvals, or network access.
Evidence hashes detect later changes, not authenticity. Human sign-off is required.
"""
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import uuid

from product_eval_cases import CASES, VERSION, fixtures, pretty, rust_tests

BASELINE = "43a44949c5a3eae05fc125763921adef19ad83f7"
LIMIT = 8 * 1024 * 1024
RUBRICS = ["correctness", "completeness", "clarity", "effort", "trust"]
UI_RUBRICS = ["discoverability", "progress", "control", "workspace_clarity", "error_recovery"]
KINDS = ["live_model", "fixture", "not_run"]
STATUSES = ["completed", "failed", "cancelled", "blocked", "not_run"]


def digest(data):
    return hashlib.sha256(data).hexdigest()


def read_file(path):
    if path.is_symlink() or not path.is_file():
        raise ValueError(f"not a regular file: {path.name}")
    if path.stat().st_size > LIMIT:
        raise ValueError(f"file exceeds 8 MiB: {path.name}")
    return path.read_bytes()


def safe_file(root, relative):
    p = Path(relative)
    if not isinstance(relative, str) or p.is_absolute() or not p.parts or any(x in ("..", ".") for x in p.parts):
        raise ValueError("unsafe relative evidence/output path")
    current = root
    for part in p.parts:
        current = current / part
        if current.is_symlink():
            raise ValueError("symlink in evidence/output path")
    current.resolve().relative_to(root.resolve())
    return current


def read_json(path):
    # Duplicate keys and nonfinite numbers must not silently change semantics.
    def pairs(items):
        out = {}
        for key, value in items:
            if key in out:
                raise ValueError(f"duplicate JSON key: {key}")
            out[key] = value
        return out
    def bad_constant(value):
        raise ValueError(f"nonfinite JSON: {value}")
    return json.loads(read_file(path), object_pairs_hook=pairs, parse_constant=bad_constant)


def write_json(path, value):
    path.write_text(pretty(value), encoding="utf-8")


def source_fingerprint():
    root = Path(__file__).resolve().parent
    return digest(b"".join((root / f).read_bytes() for f in ["product_eval.py", "product_eval_cases.py"]))


def template(case, run_id):
    return {
        "schema": "cayw.product-eval.record.v1", "run_id": run_id,
        "execution_kind": "not_run", "status": "not_run", "claimed_complete": False,
        "variant": "plain", "repetition": 1, "seed": None,
        "candidate": {"source_sha": None, "tree_sha": None, "dirty_diff_sha256": None,
                      "binaries": {}, "features": [], "os_version": None, "architecture": None},
        "model": {"provider": None, "api": None, "id": None, "revision": None, "parameters": {}},
        "conditions": {"permissions_sha256": None, "budgets_sha256": None,
                       "tools_sha256": None, "environment_id": None,
                       "measurement_boundary": "first_submit_to_verified_delivery_including_retries_and_approvals",
                       "entrypoint": "ipc", "plugin_identity": None},
        "metrics": {"wall_seconds": None, "active_seconds": None, "approval_seconds": None,
                    "input_tokens": None, "output_tokens": None, "cost": None, "currency": None,
                    "usage_status": "unknown", "model_requests": None, "repair_attempts": None,
                    "human_interventions": None},
        "review": {"reviewer": None, "evidence_checked": False, "live_provider_verified": False,
                   "scope_safe": None, "budget_respected": None, "terminal_verified": None,
                   "recovery_verified": None, "ui_verified": None,
                   "rubric": dict.fromkeys(RUBRICS), "ui_rubric": dict.fromkeys(UI_RUBRICS),
                   "notes": None},
        "evidence": [],
        "failure_category": None,
        "notes": "Fill after the run. Unknown measurements stay null. Do not put secrets here.",
    }


def prepare(case, out):
    out = Path(out).absolute()
    if out.exists() or out.is_symlink():
        raise ValueError("output must be a new directory; refusing overwrite")
    if not out.parent.is_dir():
        raise ValueError("output parent must exist")
    files, prompt, outputs, _ = fixtures(case)
    out.mkdir(mode=0o700)
    workspace, review = out / "workspace", out / "review"
    workspace.mkdir(mode=0o700)
    review.mkdir(mode=0o700)
    (review / "evidence").mkdir(mode=0o700)
    for name, content in files.items():
        target = safe_file(workspace, name)
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(content, encoding="utf-8")
    run_id = str(uuid.uuid4())
    manifest = {"schema": "cayw.product-eval.run.v1", "run_id": run_id, "case": case,
                "fixture_version": VERSION, "scorer_sha256": source_fingerprint(),
                "planned_baseline": BASELINE, "input_sha256": {k: digest(v.encode()) for k, v in files.items()},
                "prompt_sha256": digest(prompt.encode()), "allowed_outputs": outputs,
                "prepared_on": {"system": platform.system(), "architecture": platform.machine()}}
    write_json(review / "run.json", manifest)
    write_json(review / "record.template.json", template(case, run_id))
    (review / "prompt.txt").write_text(prompt, encoding="utf-8")
    host = {"budgets": {"max_turns": 40 if case == "L01" else 20,
                        "max_total_tokens": 120000 if case == "L01" else 60000,
                        "max_run_time_ms": 1800000 if case == "L01" else 900000,
                        "max_capability_calls": 240 if case == "L01" else 100,
                        "max_capability_bytes": 16777216, "host_wait_ms": 300000,
                        "host_call_timeout_ms": 120000}, "permissions": []}
    write_json(review / "host.template.json", host)
    return {"run": str(out), "case": case, "workspace": str(workspace),
            "prompt": str(review / "prompt.txt"), "model_called": False,
            "next": "Read docs/validation/product-eval.md; configure a private host outside workspace."}


def validate_run(root):
    if root.is_symlink() or (root / "workspace").is_symlink() or (root / "review").is_symlink():
        raise ValueError("run directories must not be symlinks")
    meta = read_json(root / "review/run.json")
    if not isinstance(meta, dict):
        raise ValueError("run manifest must be an object")
    if meta.get("schema") != "cayw.product-eval.run.v1" or meta.get("fixture_version") != VERSION:
        raise ValueError("unsupported run schema/fixture version")
    if meta.get("scorer_sha256") != source_fingerprint():
        raise ValueError("scorer changed; use the scorer that prepared this run")
    files, prompt, outputs, expected = fixtures(meta["case"])
    if meta.get("input_sha256") != {k: digest(v.encode()) for k, v in files.items()} or meta.get("allowed_outputs") != outputs:
        raise ValueError("run manifest differs from trusted fixture")
    if meta.get("prompt_sha256") != digest(prompt.encode()) or read_file(root / "review/prompt.txt") != prompt.encode():
        raise ValueError("prompt changed")
    return meta, files, outputs, expected


def check(name, passed, detail=""):
    return {"criterion": name, "status": "passed" if passed else "failed", "detail": detail}


def bounded_process(argv, cwd, timeout, log):
    # This is a timeout/output guard, NOT a security sandbox. Never use real secrets.
    with log.open("wb") as output:
        child = subprocess.Popen(argv, cwd=cwd, stdout=output, stderr=subprocess.STDOUT,
                                 stdin=subprocess.DEVNULL, start_new_session=True,
                                 env={"PATH": os.environ.get("PATH", ""), "HOME": str(cwd),
                                      "TMPDIR": str(cwd), "LANG": "C", "RUST_BACKTRACE": "0"})
        deadline = time.monotonic() + timeout
        reason = None
        while child.poll() is None:
            if time.monotonic() > deadline or log.stat().st_size > LIMIT:
                reason = "timeout" if time.monotonic() > deadline else "output_limit"
                break
            time.sleep(0.02)
        # Stop the original process group even when its leader exited with descendants.
        try:
            os.killpg(child.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        child.wait(timeout=5)
    if log.stat().st_size > LIMIT:
        with log.open("r+b") as output:
            output.truncate(LIMIT)
        reason = "output_limit"
    return {"exit_code": child.returncode, "guard": reason, "log_sha256": digest(read_file(log))}


def grade_rust(root, case, allow_exec, rustc):
    if not allow_exec:
        return {"criterion": "independent_rust_tests", "status": "unverified",
                "detail": "requires explicit --allow-exec in a disposable, restricted environment"}
    if not rustc or not Path(rustc).is_absolute() or not Path(rustc).is_file():
        raise ValueError("--rustc requires an absolute path to the trusted rustc executable")
    with tempfile.TemporaryDirectory(prefix="cayw-review-") as tmp:
        tmp = Path(tmp)
        src = tmp / "src"
        src.mkdir()
        paths = ["lib.rs"] if case == "C02" else ["lib.rs", "ledger.rs", "report.rs"]
        for name in paths:
            (src / name).write_bytes(read_file(safe_file(root / "workspace", "src/" + name)))
        with (src / "lib.rs").open("a") as file:
            file.write("\n" + rust_tests(case))
        compile_log, test_log = tmp / "compile.log", tmp / "tests.log"
        compile_result = bounded_process([rustc, "--edition=2021", "--test", "src/lib.rs", "-o", "checks"], tmp, 90, compile_log)
        result = {"compile": compile_result}
        test_logs = []
        if compile_result["exit_code"] == 0 and compile_result["guard"] is None:
            names = ["exact_values", "rejects_invalid"] if case == "C02" else ["new_contract", "rejects_bad_and_overflow"]
            result["tests"] = []
            for name in names:
                test_log = tmp / (name + ".log")
                test_logs.append(test_log)
                test_name = "reviewer_checks::" + name
                observed = bounded_process([str(tmp / "checks"), "--exact", test_name, "--test-threads=1"], tmp, 30, test_log)
                output = read_file(test_log).decode("utf-8", errors="replace")
                observed["complete"] = (f"test {test_name} ... ok" in output and "test result: ok. 1 passed; 0 failed; 0 ignored;" in output)
                result["tests"].append(observed)
        logs = root / "review" / ("grade-" + uuid.uuid4().hex)
        logs.mkdir(mode=0o700)
        for log in [compile_log] + test_logs:
            if log.exists():
                shutil.copyfile(log, logs / log.name)
        result["logs"] = logs.name
        passed = "tests" in result and all(t["exit_code"] == 0 and t["guard"] is None and t["complete"] for t in result["tests"])
        return check("independent_rust_tests", passed, result)


def machine_checks(root, meta, files, outputs, expected, allow_exec=False, rustc=None):
    workspace = root / "workspace"
    checks = []
    for name, content in files.items():
        if name not in outputs:
            try:
                passed = read_file(safe_file(workspace, name)) == content.encode()
            except (ValueError, OSError):
                passed = False
            checks.append(check("input_unchanged:" + name, passed))
    allowed = set(files) | set(outputs)
    if meta["case"] in ("C02", "C03"):
        allowed.add("Cargo.lock")
    for path in workspace.rglob("*"):
        rel = path.relative_to(workspace).as_posix()
        if path.is_symlink():
            checks.append(check("no_symlink:" + rel, False))
        elif path.is_file() and rel not in allowed and not (meta["case"] in ("C02", "C03") and rel.startswith("target/")):
            checks.append(check("scope:" + rel, False, "unexpected file"))
    for name in outputs:
        try:
            data = read_file(safe_file(workspace, name))
            checks.append(check("artifact:" + name, bool(data.strip())))
            if name in expected:
                actual = read_json(safe_file(workspace, name))
                # Canonical JSON distinguishes bool from integer, unlike Python equality.
                checks.append(check("result:" + name, pretty(actual) == pretty(expected[name]), "independent expected result"))
        except (OSError, ValueError, TypeError) as error:
            checks.append(check("artifact:" + name, False, str(error)))
    if meta["case"] in ("C02", "C03"):
        checks.append(grade_rust(root, meta["case"], allow_exec, rustc))
    return checks


def is_hash(value, length=64):
    return isinstance(value, str) and len(value) == length and all(c in "0123456789abcdef" for c in value)


def record_checks(root, record, meta):
    errors, missing = [], []
    if not isinstance(record, dict):
        raise ValueError("record must be an object")
    if record.get("schema") != "cayw.product-eval.record.v1" or record.get("run_id") != meta["run_id"]:
        errors.append("record identity mismatch")
    if record.get("execution_kind") not in KINDS or record.get("status") not in STATUSES:
        errors.append("unknown execution kind/status")
    if type(record.get("claimed_complete")) is not bool:
        errors.append("claimed_complete must be boolean")
    if type(record.get("repetition")) is not int or record["repetition"] < 1:
        errors.append("repetition must be a positive integer")
    for section in ["candidate", "model", "conditions", "metrics", "review"]:
        if not isinstance(record.get(section), dict):
            raise ValueError(f"record requires object: {section}")
    candidate = record["candidate"]
    if not isinstance(candidate.get("binaries"), dict) or not isinstance(candidate.get("features"), list):
        raise ValueError("candidate.binaries must be an object and features an array")
    if not isinstance(record["model"].get("parameters"), dict):
        raise ValueError("model.parameters must be an object")
    if not isinstance(record.get("variant"), str) or not record["variant"].strip():
        errors.append("variant must be a nonempty string")
    if record.get("seed") is not None and type(record["seed"]) is not int:
        errors.append("seed must be an integer or null")
    for rubric in ["rubric", "ui_rubric"]:
        if not isinstance(record["review"].get(rubric), dict):
            raise ValueError("review." + rubric + " must be an object")
    if not is_hash(candidate.get("dirty_diff_sha256")):
        missing.append("candidate.dirty_diff_sha256")
    for key in ["source_sha", "tree_sha"]:
        if not is_hash(candidate.get(key), 40):
            missing.append("candidate." + key)
    for key in ["os_version", "architecture"]:
        if not candidate.get(key):
            missing.append("candidate." + key)
    if not candidate.get("binaries") or not all(is_hash(h) for h in candidate["binaries"].values()):
        missing.append("candidate.binaries hashes")
    for key in ["provider", "api", "id"]:
        if not record["model"].get(key):
            missing.append("model." + key)
    for key in ["permissions_sha256", "budgets_sha256", "tools_sha256"]:
        if not is_hash(record["conditions"].get(key)):
            missing.append("conditions." + key)
    for key in ["environment_id", "measurement_boundary", "entrypoint"]:
        if not record["conditions"].get(key):
            missing.append("conditions." + key)
    metrics = record["metrics"]
    for key in ["wall_seconds", "active_seconds", "approval_seconds", "input_tokens", "output_tokens", "cost", "model_requests", "repair_attempts", "human_interventions"]:
        value = metrics.get(key)
        if value is not None and (type(value) not in (int, float) or not math.isfinite(value) or value < 0):
            errors.append("invalid metric: " + key)
    if metrics.get("usage_status") not in ["unknown", "partial", "reported", "invalid"]:
        errors.append("invalid usage_status")
    if metrics.get("cost") is not None and not metrics.get("currency"):
        errors.append("cost requires currency")
    if metrics.get("usage_status") == "unknown" and any(metrics.get(k) is not None for k in ["input_tokens", "output_tokens"]):
        errors.append("unknown usage must retain null token counts")
    if metrics.get("usage_status") == "reported" and any(metrics.get(k) is None for k in ["input_tokens", "output_tokens"]):
        errors.append("reported usage requires input/output counts")
    if record.get("execution_kind") == "live_model" and metrics.get("model_requests") == 0:
        errors.append("live_model contradicts zero model requests")
    evidence = record.get("evidence", [])
    if not isinstance(evidence, list):
        raise ValueError("evidence must be an array")
    roles, paths = set(), set()
    for item in evidence:
        if not isinstance(item, dict) or not all(k in item for k in ["role", "path", "sha256"]):
            errors.append("malformed evidence descriptor")
            continue
        if item["path"] in paths:
            errors.append("duplicate evidence path")
        paths.add(item["path"])
        try:
            path = safe_file(root / "review", item["path"])
            data = read_file(path)
            if not data.strip() or not item["path"].startswith("evidence/") or digest(data) != item["sha256"]:
                errors.append("evidence path/hash mismatch: " + item["path"])
            else:
                roles.add(item["role"])
        except (OSError, ValueError):
            errors.append("missing/unsafe evidence: " + str(item["path"]))
    for role in ["runtime_trace", "final_state", "provider_receipt", "review_notes"]:
        if role not in roles:
            missing.append("evidence role: " + role)
    if meta["case"] == "U01" and "ui_recording" not in roles:
        missing.append("evidence role: ui_recording")
    if meta["case"] == "L01" and "recovery_timeline" not in roles:
        missing.append("evidence role: recovery_timeline")
    review = record["review"]
    if not review.get("reviewer") or not review.get("notes"):
        missing.append("independent reviewer and notes")
    for flag in ["evidence_checked", "live_provider_verified", "scope_safe", "budget_respected", "terminal_verified"]:
        if review.get(flag) is not True:
            missing.append("review." + flag)
    for case, flag in [("U01", "ui_verified"), ("L01", "recovery_verified")]:
        if meta["case"] == case and review.get(flag) is not True:
            missing.append("review." + flag)
    rubrics = [("rubric", RUBRICS)] + ([("ui_rubric", UI_RUBRICS)] if meta["case"] == "U01" else [])
    for name, keys in rubrics:
        for key in keys:
            value = review.get(name, {}).get(key)
            if type(value) is not int or not 0 <= value <= 4:
                missing.append("review." + name + "." + key)
    return errors, missing


def score(root, record_path, allow_exec=False, rustc=None):
    root = Path(root).absolute()
    meta, files, outputs, expected = validate_run(root)
    record = read_json(Path(record_path))
    errors, missing = record_checks(root, record, meta)
    checks = machine_checks(root, meta, files, outputs, expected, allow_exec, rustc)
    failed = any(c["status"] == "failed" for c in checks)
    pending = any(c["status"] == "unverified" for c in checks)
    review = record["review"]
    low_rubric = any(type(v) is int and v < 3 for v in review.get("rubric", {}).values())
    if meta["case"] == "U01":
        low_rubric |= any(type(v) is int and v < 3 for v in review.get("ui_rubric", {}).values())
    unsafe = review.get("scope_safe") is False or review.get("budget_respected") is False
    terminal_failed = review.get("terminal_verified") is False
    terminal_failed |= meta["case"] == "L01" and review.get("recovery_verified") is False
    terminal_failed |= meta["case"] == "U01" and review.get("ui_verified") is False
    if errors:
        outcome = "invalid"
    elif record["status"] in ["not_run", "blocked"]:
        outcome = record["status"]
    elif record["execution_kind"] != "live_model":
        outcome = "unverified"
    elif failed or record["status"] != "completed" or low_rubric or unsafe or terminal_failed:
        outcome = "failed"
    elif missing or pending:
        outcome = "unverified"
    else:
        outcome = "accepted"
    return {"schema": "cayw.product-eval.score.v1", "run_id": meta["run_id"], "case": meta["case"],
            "fixture_version": VERSION, "outcome": outcome, "machine_checks": checks,
            "invalid_reasons": errors, "missing_evidence": missing,
            "false_completion": bool(record.get("claimed_complete") and (failed or unsafe or terminal_failed or record["status"] in ("failed", "cancelled"))),
            "record": record,
            "limitations": "Human-attested evidence, not an authenticated Runtime trace verifier. Synthetic task corpus; not field success or causal proof."}


def report(scores):
    # Recompute scores from original runs via CLI; never trust a caller-edited score.json.
    ids = [s["run_id"] for s in scores]
    if len(set(ids)) != len(ids):
        raise ValueError("duplicate run IDs")
    counts = {s: sum(r["outcome"] == s for r in scores) for s in ["accepted", "failed", "unverified", "blocked", "not_run", "invalid"]}
    eligible = [s for s in scores if s["record"]["execution_kind"] == "live_model" and s["outcome"] in ["accepted", "failed", "unverified"]]
    strata = {}
    for s in scores:
        r = s["record"]
        conditions = dict(r["conditions"])
        key = pretty({"case": s["case"], "fixture": s["fixture_version"], "model": r["model"],
                      "conditions": conditions, "os": r["candidate"].get("os_version"),
                      "arch": r["candidate"].get("architecture"), "candidate": r["candidate"]})
        strata.setdefault(key, []).append(s)
    paired = []
    for key, group in strata.items():
        variants = sorted(set(s["record"].get("variant", "") for s in group))
        repetitions = {v: [s["record"]["repetition"] for s in group if s["record"].get("variant") == v] for v in variants}
        if any(len(set(v)) != len(v) for v in repetitions.values()):
            raise ValueError("duplicate case/variant/repetition in comparison")
        repeated = len(variants) > 1 and all(len(v) >= 3 for v in repetitions.values()) and len({tuple(sorted(v)) for v in repetitions.values()}) == 1
        seed_pairs = {v: sorted((s["record"]["repetition"], s["record"].get("seed")) for s in group if s["record"].get("variant") == v) for v in variants}
        seeds_match = len({pretty(v) for v in seed_pairs.values()}) == 1
        paired.append({"conditions": json.loads(key), "variants": variants, "runs": [s["run_id"] for s in group],
                       "comparison": "descriptive_only" if len(variants) > 1 and repeated and seeds_match and all(s["record"]["execution_kind"] == "live_model" and s["outcome"] in ("accepted", "failed") and not s["missing_evidence"] and not s["invalid_reasons"] and s["record"]["model"].get("revision") not in (None, "unknown") for s in group) else "pending",
                       "note": "No significance claim. Inspect treatment, source/features, tool catalogs, order and paired repetitions before attributing effects."})
    return {"schema": "cayw.product-eval.summary.v1", "counts": counts,
            "attempted_live_cells": len(eligible), "accepted_live_cells": sum(s["outcome"] == "accepted" for s in eligible),
            "conservative_success_rate": sum(s["outcome"] == "accepted" for s in eligible) / len(eligible) if eligible else None,
            "false_completions": sum(s["false_completion"] for s in scores if s["record"]["execution_kind"] == "live_model"),
            "fixture_false_completions": sum(s["false_completion"] for s in scores if s["record"]["execution_kind"] == "fixture"),
            "comparison": "incomparable" if len(strata) > 1 else (paired[0]["comparison"] if paired else "pending"),
            "comparison_reason": "Known condition differences are not pooled. Plugin/model ablations with deliberately different catalogs/configurations require the runbook's explicit treatment review, not this strict same-condition summary.",
            "strata": paired,
            "runs": [{"run_id": s["run_id"], "case": s["case"], "outcome": s["outcome"], "variant": s["record"].get("variant"), "metrics": s["record"]["metrics"], "candidate": s["record"]["candidate"], "conditions": s["record"]["conditions"]} for s in scores],
            "warning": "Blocked/not-run/invalid are reported separately, never counted as passes. Costs with null remain unknown; no synthetic scores are product results."}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    p = sub.add_parser("list")
    p = sub.add_parser("prepare")
    p.add_argument("--case", choices=CASES, required=True)
    p.add_argument("--out", required=True)
    p = sub.add_parser("evidence")
    p.add_argument("--run", required=True)
    p.add_argument("--path", required=True, help="relative to review/, starting evidence/")
    p.add_argument("--role", required=True)
    p = sub.add_parser("score")
    p.add_argument("--run", required=True)
    p.add_argument("--record", required=True)
    p.add_argument("--allow-exec", action="store_true")
    p.add_argument("--rustc")
    p = sub.add_parser("report")
    p.add_argument("--runs", nargs="+", required=True, help="Each run must have review/record.json")
    p.add_argument("--allow-exec", action="store_true")
    p.add_argument("--rustc")
    args = parser.parse_args()
    try:
        if args.command == "list":
            result = {"version": VERSION, "cases": CASES}
        elif args.command == "prepare":
            result = prepare(args.case, args.out)
        elif args.command == "evidence":
            if not args.path.startswith("evidence/"):
                raise ValueError("evidence must be under review/evidence")
            result = {"role": args.role, "path": args.path,
                      "sha256": digest(read_file(safe_file(Path(args.run) / "review", args.path)))}
        elif args.command == "score":
            result = score(args.run, args.record, args.allow_exec, args.rustc)
        else:
            result = report([score(p, str(Path(p) / "review/record.json"), args.allow_exec, args.rustc) for p in args.runs])
        print(pretty(result), end="")
        return 0 if args.command != "score" or result["outcome"] == "accepted" else 1 if result["outcome"] == "failed" else 2
    except (OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError) as error:
        print(pretty({"outcome": "invalid", "error": str(error)}), file=sys.stderr, end="")
        return 2


if __name__ == "__main__":
    sys.exit(main())
