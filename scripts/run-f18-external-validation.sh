#!/usr/bin/env bash
# F18 SLICE L entrypoint: load GATEWAY_* (no secret printing) then run the
# external/fixture visual-editor acceptance harness.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
# shellcheck disable=SC1091
source "$ROOT/scripts/load-gateway-magpie-env.sh"
OUT="${1:-.local/f18-external-validation/run-$(date -u +%Y%m%dT%H%M%SZ)}"
shift || true
exec node "$ROOT/scripts/validate-visual-editor-external.mjs" --out "$OUT" "$@"
