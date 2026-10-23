#!/usr/bin/env python3
"""Freeze reproducible offline Device/Host validation without ambient caches.

Run with python3 -I /absolute/path/to/verify.py /absolute/path/to/CAYW /absolute/cargo.
Every invocation creates a separate evidence directory and retains earlier runs.
"""
import hashlib
import json
import os
from pathlib import Path
import platform
import subprocess
import sys
from datetime import datetime, timezone

root = Path(sys.argv[1]).resolve()
# Keep the cargo proxy basename: rustup dispatches using argv[0]. Resolving its
# symlink would invoke rustup itself instead of Cargo.
cargo = str(Path(sys.argv[2]).absolute())
evidence = Path(__file__).resolve().parent / datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%S.%fZ")
evidence.mkdir()
env = os.environ.copy()
env["TMPDIR"] = str(root / ".local/device-host-tmp")
env["CARGO_TARGET_DIR"] = str(root / ".local/device-host-target")
Path(env["TMPDIR"]).mkdir(parents=True, exist_ok=True)
Path(env["CARGO_TARGET_DIR"]).mkdir(parents=True, exist_ok=True)
commands = [
    ("format", [cargo, "+1.95.0", "fmt", "--manifest-path", str(root / "Cargo.toml"), "-p", "cayw-device", "-p", "cayw-host", "--", "--check"]),
    ("check", [cargo, "+1.95.0", "check", "--manifest-path", str(root / "Cargo.toml"), "-p", "cayw-device", "-p", "cayw-host", "--all-targets", "--locked", "--offline"]),
    ("test", [cargo, "+1.95.0", "test", "--manifest-path", str(root / "Cargo.toml"), "-p", "cayw-device", "-p", "cayw-host", "--locked", "--offline"]),
    ("clippy", [cargo, "+1.95.0", "clippy", "--manifest-path", str(root / "Cargo.toml"), "-p", "cayw-device", "-p", "cayw-host", "--all-targets", "--locked", "--offline", "--", "-D", "warnings"]),
]
# Exact dependency/module paths, not repository discovery or text search.
source_paths = [
    "Cargo.toml", "Cargo.lock",
    "crates/cayw-device/Cargo.toml", "crates/cayw-device/src/lib.rs",
    "crates/cayw-device/src/process.rs", "crates/cayw-device/src/scheduler.rs",
    "crates/cayw-device/tests/process.rs", "crates/cayw-device/tests/scheduler.rs",
    "crates/cayw-device/tests/fixtures/process.rs",
    "crates/cayw-host/Cargo.toml", "crates/cayw-host/src/lib.rs",
    "crates/cayw-host/src/process.rs", "crates/cayw-host/src/filesystem.rs",
    "crates/cayw-host/src/catalog.rs", "crates/cayw-host/tests/host.rs",
    "crates/cayw-host/tests/repair_regressions.rs",
    "crates/cayw-device/validation/cwd-drain-20261019/verify.py",
]
red_logs = ["drain-red.log", "cwd-red.log", "exec-red.log"]
def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()

before = {path: digest(root / path) for path in source_paths}
results = []
for label, command in commands:
    log = evidence / (label + ".log")
    with log.open("wb") as output:
        result = subprocess.run(command, cwd=root, env=env, stdout=output, stderr=subprocess.STDOUT)
    results.append({"check": label, "argv": command, "exit_code": result.returncode, "log": str(log), "sha256": digest(log)})
    print(json.dumps(results[-1]), flush=True)
after = {path: digest(root / path) for path in source_paths}
manifest = {
    "platform": platform.platform(), "machine": platform.machine(),
    "toolchain": subprocess.check_output([cargo, "+1.95.0", "--version"], cwd=root, env=env, text=True).strip(),
    "tmpdir": env["TMPDIR"], "target_dir": env["CARGO_TARGET_DIR"],
    "sources_before": before, "sources_after": after, "sources_stable": before == after,
    "regression_red_logs": {name: digest(Path(__file__).parent / name) for name in red_logs},
    "checks": results,
    "limitations": [
        "macOS arm64 execution only; Linux Docker daemon did not answer read-only probes, Windows not executed",
        "Non-Unix process execution remains explicitly unsupported",
        "Git snapshots share max_file_bytes/max_list_entries across metadata, objects and worktree; links and special files are refused",
        "Git source edits in place are observational, not an atomic repository transaction",
        "Managed process groups cannot contain descendants that deliberately escape via setsid",
    ],
}
manifest_path = evidence / "evidence.json"
manifest_path.write_text(json.dumps(manifest, indent=2) + "\n")
print(json.dumps({"evidence": str(manifest_path), "sha256": digest(manifest_path), "sources_stable": before == after}), flush=True)
sys.exit(0 if before == after and all(item["exit_code"] == 0 for item in results) else 1)
