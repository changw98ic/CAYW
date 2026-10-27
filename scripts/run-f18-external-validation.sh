#!/usr/bin/env bash
# F18 entrypoint for the external/fixture editor HTTP validation.
# No model credentials are required or loaded.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
OUT="${1:-.local/f18-external-validation/run-$(date -u +%Y%m%dT%H%M%SZ)}"
shift || true
exec node "$ROOT/scripts/validate-visual-editor-external.mjs" --out "$OUT" "$@"
