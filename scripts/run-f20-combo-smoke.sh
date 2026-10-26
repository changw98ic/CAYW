#!/usr/bin/env bash
# F20 / SLICE Q — combo smoke entry. exit 0 means the harness ran cleanly for
# the tests that are enabled on this branch; it does NOT mean F20 / #33 is
# sealed. Ignored/pending Q items are recorded explicitly and keep
# f20_combo_complete=false.
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
set +e
cargo test -p cayw-runtime --test f20_combo_smoke --locked 2>&1 | tee "$LOG"
STATUS=${PIPESTATUS[0]}
set -e
python3 - << PY
import hashlib, json, platform, re, time
from pathlib import Path
out = Path(${OUT@Q})
log = Path(${LOG@Q})
status = int(${STATUS})
body = log.read_text(encoding="utf-8", errors="replace")
body_bytes = log.read_bytes()

def count(pat: str) -> int:
    m = re.search(pat, body)
    return int(m.group(1)) if m else 0

passed_n = count(r"(\d+) passed")
failed_n = count(r"(\d+) failed")
ignored_n = count(r"(\d+) ignored")
# Cargo summary line like: test result: ok. 3 passed; 0 failed; 1 ignored; ...
ignored_names = re.findall(r"^test (\S+) \.\.\. ignored", body, flags=re.M)
passed_names = re.findall(r"^test (\S+) \.\.\. ok$", body, flags=re.M)

# Map exercised tests -> Q ids claimed on this run.
CLAIM_MAP = {
    "base_combo_session_reopen_isolation_and_child_agent": ["Q1", "Q2", "Q3"],
    "base_combo_content_survives_reopen_on_macos": ["Q1"],
    "process_cleanup_and_settled_result_survive_reopen": ["Q4"],
    "result_release_not_registered_on_tip": ["Q4"],
    "mcp_resources_share_session_and_survive_reopen": ["Q6"],
    "mcp_stdio_process_lifecycle_cleanup_and_session_reopen": ["Q6"],
    # Tip-tree combo proves strongest same-tree path; does NOT clear Q8 seal.
    "tip_tree_session_child_process_and_mcp_stdio_combo": ["Q4", "Q6"],
}
PENDING_MAP = {
    "pending_background_ownership_survives_reopen": "Q5",
}
questions_exercised = sorted({
    q for name in passed_names for q in CLAIM_MAP.get(name, [])
})
questions_pending = sorted({
    PENDING_MAP[name] for name in ignored_names if name in PENDING_MAP
} | {"Q7", "Q8"})
# Honesty: process/stdio halves do not clear Slice A / F14 ManagedStdio / Q8 seal.
gaps = [
    "Q4 result.release execute path (SLICE A / #40) — tip only proves process cleanup + settled reopen + honest absence of result.release",
    "Q5 bg.* + gateway OwnershipStore (SLICE P / #45)",
    "Q6 F14 ManagedStdio lazy-start/idle-stop (SLICE R / #41) — tip proved MCP resources+reopen AND real stdio spawn/disconnect/reap",
    "Q7 WebUI/TUI/SDK auth IPC same-field consumption",
    "Q8 multi-module same-tree merge seal (needs A+P+R; tip_tree proves process+MCP stdio same Session only — child+Bridge same-Runtime multi-turn still blocked on tip)",
]
base_ok = status == 0 and failed_n == 0
# Hard rule: ignored items must never make f20_combo_complete true.
f20_complete = False
# Optional strict mode: CLAIM_FULL_F20=1 fails the script when anything is ignored
# or when known gaps remain (always, today).
strict = __import__("os").environ.get("CLAIM_FULL_F20", "") == "1"
strict_fail = strict and (ignored_n > 0 or not f20_complete)
summary = {
    "slice": "Q",
    "feature": "F20",
    "kind": "combo_acceptance_smoke",
    "base_smoke_ok": base_ok,
    "passed": base_ok and not strict_fail,
    "f20_combo_complete": f20_complete,
    "ignored_do_not_count_as_seal": True,
    "exit_code": status if not strict_fail else (status or 3),
    "cargo_test": "cargo test -p cayw-runtime --test f20_combo_smoke --locked",
    "cargo_counts": {
        "passed": passed_n,
        "failed": failed_n,
        "ignored": ignored_n,
    },
    "tests_passed": passed_names,
    "tests_ignored": ignored_names,
    "questions_exercised": questions_exercised,
    "questions_pending": questions_pending,
    "remaining_gaps": gaps,
    "log_sha256": hashlib.sha256(body_bytes).hexdigest(),
    "platform": {
        "system": platform.system(),
        "machine": platform.machine(),
        "release": platform.release(),
    },
    "utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
    "chinese_ready_facts": [
        "切片 Q 交付 F20 组合验收烟测：Q1–Q3 底座 + Q4 进程清理/结果重开 + Q4 result.release 缺席证明 + Q6 MCP 资源重开 + Q6 真实 stdio 启停清理 + tip 同树多能力组合。",
        "exit 0 / summary.passed 只表示已启用用例通过；有 ignored 或 remaining_gaps 时 f20_combo_complete 恒为 false，不等于 F20/#33 封口。",
        "Q5 bg.*、Q4 result.release 执行路径（#40）、F14 ManagedStdio 懒起/空闲停（#41）、Q7/Q8 完整封口仍 pending；未合并其他功能 PR。",
        "本 PR 不自动合并；不宣称 F20 或 #33 复选框完成。",
    ],
}
(out / "summary.json").write_text(
    json.dumps(summary, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
)
print(json.dumps({
    "summary": str(out / "summary.json"),
    "base_smoke_ok": summary["base_smoke_ok"],
    "f20_combo_complete": summary["f20_combo_complete"],
    "passed": summary["passed"],
    "exit_code": summary["exit_code"],
    "questions_exercised": questions_exercised,
    "questions_pending": questions_pending,
    "ignored": ignored_names,
}, ensure_ascii=False))
raise SystemExit(summary["exit_code"])
PY
