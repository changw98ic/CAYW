#!/usr/bin/env bash
# Run the integrated Runtime scenarios and retain their actual scope and source.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
OUT="${1:-.local/f20-combo-smoke/run-$(date -u +%Y%m%dT%H%M%SZ)}"
if [[ -e "$OUT" ]]; then
  echo "refusing to overwrite existing evidence dir: $OUT" >&2
  exit 2
fi
mkdir -p "$OUT"
LOG="$OUT/cargo-test.log"
source_receipt() {
  python3 -I - "$1" <<'PY'
import hashlib
import json
import os
import subprocess
import sys
from pathlib import Path

paths = subprocess.check_output([
    "git", "ls-files", "--others", "--exclude-standard", "-z", "--",
    "Cargo.toml", "Cargo.lock", "crates", "scripts", "docs", "AGENTS.md", "README.md",
]).split(b"\0")
untracked = {}
for raw in sorted(path for path in paths if path):
    path = Path(os.fsdecode(raw))
    body = os.fsencode(os.readlink(path)) if path.is_symlink() else path.read_bytes()
    untracked[str(path)] = hashlib.sha256(body).hexdigest()
receipt = {
    "head": subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip(),
    "tracked_diff_sha256": hashlib.sha256(
        subprocess.check_output(["git", "diff", "--binary", "HEAD"])
    ).hexdigest(),
    "untracked_source_sha256": untracked,
}
Path(sys.argv[1]).write_text(json.dumps(receipt, ensure_ascii=False, indent=2) + "\n")
PY
}
source_receipt "$OUT/source-before.json"
set +e
cargo test -p cayw-runtime --test f20_combo_smoke --test f20_mcp_lifecycle --test f20_background --test integrated_ipc --locked 2>&1 | tee "$LOG"
STATUS=${PIPESTATUS[0]}
set -e
source_receipt "$OUT/source-after.json"
python3 -I - "$OUT" "$LOG" "$STATUS" <<'PY'
import hashlib
import json
import platform
import re
import sys
import time
from pathlib import Path

out, log = map(Path, sys.argv[1:3])
status = int(sys.argv[3])
raw = log.read_bytes()
body = raw.decode("utf-8", errors="replace")
passed = re.findall(r"^test (\S+) \.\.\. ok$", body, flags=re.M)
ignored = re.findall(r"^test (\S+) \.\.\. ignored", body, flags=re.M)
failed = re.findall(r"^test (\S+) \.\.\. FAILED$", body, flags=re.M)
scenarios = {
    "session_reopen_isolation_and_child": "base_combo_session_reopen_isolation_and_child_agent",
    "device_content_reopen": "base_combo_content_survives_reopen_on_macos",
    "process_cleanup_and_result_reopen": "process_cleanup_and_settled_result_survive_reopen",
    "result_read_release_reopen": "result_read_and_release_survive_reopen",
    "background_ownership_reopen": "background_ownership_survives_reopen",
    "mcp_resource_reopen": "mcp_resources_share_session_and_survive_reopen",
    "mcp_process_cleanup": "mcp_stdio_process_lifecycle_cleanup_and_session_reopen",
    "mcp_lazy_idle_recycle": "managed_mcp_lifecycle_and_result_reopen",
    "child_process_mcp_reopen": "session_child_process_and_mcp_stdio_survive_reopen",
    "sdk_authenticated_ipc_observation": "authenticated_ipc_and_sdk_observe_the_same_persistent_run",
}
results = {
    name: {"test": test, "status": "passed" if test in passed else
           "failed" if test in failed else "ignored" if test in ignored else "not_run"}
    for name, test in scenarios.items()
}
missing = [name for name, row in results.items() if row["status"] != "passed"]
source_before = json.loads((out / "source-before.json").read_text())
source_after = json.loads((out / "source-after.json").read_text())
source_unchanged = source_before == source_after
ok = status == 0 and bool(passed) and not failed and not ignored and not missing and source_unchanged
exit_code = status or (0 if ok else 3)
summary = {
    "feature": "F20",
    "kind": "runtime_integration_scenarios",
    "passed": ok,
    "exit_code": exit_code,
    "cargo_command": "cargo test -p cayw-runtime --test f20_combo_smoke --test f20_mcp_lifecycle --test f20_background --test integrated_ipc --locked",
    "tests_passed": passed,
    "tests_failed": failed,
    "tests_ignored": ignored,
    "scenarios": results,
    "unpassed_scenarios": missing,
    "scope": {
        "model": "fixed local responses; synthetic token usage; no paid model calls",
        "execution": "actual Luau, Host/Device processes, stdio MCP, SQLite and loopback IPC",
        "ui": "browser and TUI interactions have separate evidence",
        "issue_acceptance": "this script reports its scenarios, not full F01-F20 acceptance",
    },
    "log_sha256": hashlib.sha256(raw).hexdigest(),
    "source": source_after,
    "source_unchanged_during_run": source_unchanged,
    "platform": {"system": platform.system(), "machine": platform.machine(), "release": platform.release()},
    "utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
}
(out / "summary.json").write_text(json.dumps(summary, ensure_ascii=False, indent=2) + "\n")
print(json.dumps({"summary": str(out / "summary.json"), "passed": ok,
                  "exit_code": exit_code, "unpassed_scenarios": missing,
                  "source_unchanged_during_run": source_unchanged}, ensure_ascii=False))
raise SystemExit(exit_code)
PY
