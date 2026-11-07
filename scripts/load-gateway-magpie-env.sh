#!/usr/bin/env bash
# Load Magpie credentials for F18 / Issue #30 acceptance from a dotenv file.
#
# Default dotenv path: /workspace/.env (box shared secrets). Override with
# CAYW_GATEWAY_ENV=/path/to/.env. Never prints secret values.
#
# Mapping (only when the Magpie variable is unset or empty):
#   GATEWAY_KEY  -> MAGPIE_API_KEY
#   GATEWAY_URL  -> MAGPIE_BASE_URL  (appends /v1 when the path is empty or "/")
#
# Usage:
#   source scripts/load-gateway-magpie-env.sh
#   # or:  eval "$(scripts/load-gateway-magpie-env.sh --export)"
#
# Safe to source multiple times. Exits 0 even when the file is missing so callers
# can continue into public-only / fixture modes.

set -euo pipefail

EXPORT_MODE=0
if [[ "${1:-}" == "--export" ]]; then
  EXPORT_MODE=1
fi

ENV_FILE="${CAYW_GATEWAY_ENV:-/workspace/.env}"
REPORT_PRESENT=0
REPORT_URL_SET=0
REPORT_KEY_SET=0
REPORT_BASE=""

append_v1() {
  local raw="$1"
  raw="${raw%"${raw##*[![:space:]]}"}"
  raw="${raw#"${raw%%[![:space:]]*}"}"
  raw="${raw%\"}"
  raw="${raw#\"}"
  raw="${raw%\'}"
  raw="${raw#\'}"
  if [[ -z "$raw" ]]; then
    printf '%s' ""
    return
  fi
  # strip trailing slash for path check
  local trimmed="${raw%/}"
  case "$trimmed" in
    */v1) printf '%s' "$trimmed" ;;
    *) printf '%s' "${trimmed}/v1" ;;
  esac
}

if [[ -f "$ENV_FILE" ]]; then
  REPORT_PRESENT=1
  # Parse KEY=VALUE lines without sourcing (avoids executing shell metacharacters).
  while IFS= read -r line || [[ -n "$line" ]]; do
    case "$line" in
      ''|\#*) continue ;;
    esac
    if [[ "$line" != *=* ]]; then
      continue
    fi
    key="${line%%=*}"
    value="${line#*=}"
    key="${key%"${key##*[![:space:]]}"}"
    key="${key#"${key%%[![:space:]]*}"}"
    case "$key" in
      GATEWAY_URL)
        if [[ -z "${MAGPIE_BASE_URL:-}" ]]; then
          mapped="$(append_v1 "$value")"
          if [[ -n "$mapped" ]]; then
            export MAGPIE_BASE_URL="$mapped"
            REPORT_URL_SET=1
            REPORT_BASE="$mapped"
          fi
        fi
        ;;
      GATEWAY_KEY)
        if [[ -z "${MAGPIE_API_KEY:-}" ]]; then
          # trim surrounding whitespace and optional quotes only
          v="$value"
          v="${v%"${v##*[![:space:]]}"}"
          v="${v#"${v%%[![:space:]]*}"}"
          v="${v%\"}"; v="${v#\"}"
          v="${v%\'}"; v="${v#\'}"
          if [[ -n "$v" ]]; then
            export MAGPIE_API_KEY="$v"
            REPORT_KEY_SET=1
          fi
        fi
        ;;
    esac
  done < "$ENV_FILE"
fi

status_line() {
  local key_state="absent"
  if [[ -n "${MAGPIE_API_KEY:-}" ]]; then
    key_state="present"
  fi
  local base_state="unset"
  if [[ -n "${MAGPIE_BASE_URL:-}" ]]; then
    base_state="set"
  fi
  printf 'load-gateway-magpie-env: env_file=%s present=%s mapped_url=%s mapped_key=%s MAGPIE_BASE_URL=%s MAGPIE_API_KEY=%s\n' \
    "$ENV_FILE" "$REPORT_PRESENT" "$REPORT_URL_SET" "$REPORT_KEY_SET" "$base_state" "$key_state" >&2
}

if [[ "$EXPORT_MODE" -eq 1 ]]; then
  status_line
  if [[ -n "${MAGPIE_BASE_URL:-}" ]]; then
    printf 'export MAGPIE_BASE_URL=%q\n' "$MAGPIE_BASE_URL"
  fi
  if [[ -n "${MAGPIE_API_KEY:-}" ]]; then
    printf 'export MAGPIE_API_KEY=%q\n' "$MAGPIE_API_KEY"
  fi
else
  # When sourced, only print the status line to stderr.
  status_line
fi
