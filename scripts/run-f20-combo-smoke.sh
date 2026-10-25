#!/usr/bin/env bash
# F20 / SLICE Q — base combo smoke entry (docs+skeleton). Does not seal F20.
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
IGNORED="$(rg -c 'ignored' "$LOG" || true)"
PASSED="$(rg -o '[0-9]+ passed' "$LOG" | tail -1 || true)"
SUMMARY="$OUT/summary.json"
python3 - << PY
import hashlib, json, os, platform, time
from pathlib import Path
out = Path(${OUT@Q})
log = Path(${LOG@Q})
status = int(${STATUS})
body = log.read_bytes()
summary = {
    "slice": "Q",
    "feature": "F20",
    "kind": "combo_acceptance_skeleton_smoke",
    "passed": status == 0,
    "exit_code": status,
    "cargo_test": "cargo test -p cayw-runtime --test f20_combo_smoke --locked",
    "log_sha256": hashlib.sha256(body).hexdigest(),
    "platform": {
        "system": platform.system(),
        "machine": platform.machine(),
        "release": platform.release(),
    },
    "utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
    "chinese_ready_facts": [
        "切片 Q 只交付 F20 组合验收骨架与底座烟测，不是新调度器。",
        "底座已覆盖同 Session SQLite 重开、跨 Session 隔离、同场 agent.run 子代理；ContentStore 重开限 macOS。",
        "结果释放/后台归属/MCP 同场仍为 pending，等 A/P/F14 合入后再封口。",
        "不宣称 F20 或 #33 复选框完成；本 PR 保持未合并直至封口轮次。",
    ],
    "pending_ignored": [
        "pending_result_release_aligns_with_process_cleanup",
        "pending_background_ownership_survives_reopen",
        "pending_mcp_lifecycle_shares_session_results",
    ],
}
(out / "summary.json").write_text(json.dumps(summary, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
print(json.dumps({"summary": str(out / "summary.json"), "passed": summary["passed"], "exit_code": status}, ensure_ascii=False))
PY
exit "$STATUS"
