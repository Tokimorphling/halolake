#!/usr/bin/env bash
# Batch-set models= for all channels in a routing group (control-api admin).
#
# Auth (pick one):
#   1) Login:  USERNAME=admin PASSWORD=... ./scripts/set-group-channel-models.sh GROUP MODEL
#   2) Token:  ACCESS_TOKEN=... ./scripts/set-group-channel-models.sh GROUP MODEL
#              (from UI: user self → generate access token, or GET /api/user/token when logged in)
#
# On server (same host as container):
#   BASE_URL=http://127.0.0.1:9090 GROUP=default MODEL=grok-4.5 \
#     USERNAME=admin PASSWORD='...' ./scripts/set-group-channel-models.sh
#
# Dry-run (list only):
#   DRY_RUN=1 ... ./scripts/set-group-channel-models.sh default grok-4.5
#
# Requires: curl, jq

set -euo pipefail

GROUP="${1:-${GROUP:-}}"
MODEL="${2:-${MODEL:-grok-4.5}}"
BASE_URL="${BASE_URL:-http://127.0.0.1:9090}"
PAGE_SIZE="${PAGE_SIZE:-100}"
DRY_RUN="${DRY_RUN:-0}"
COOKIE_JAR="${COOKIE_JAR:-$(mktemp -t halolake-cookies.XXXXXX)}"
cleanup() { rm -f "$COOKIE_JAR" 2>/dev/null || true; }
trap cleanup EXIT

if [[ -z "$GROUP" ]]; then
  echo "usage: GROUP=mygroup [MODEL=grok-4.5] [USERNAME=..] [PASSWORD=..] [ACCESS_TOKEN=..] $0 [GROUP] [MODEL]" >&2
  exit 1
fi

need() { command -v "$1" >/dev/null || { echo "missing dependency: $1" >&2; exit 1; }; }
need curl
need jq

auth_args=()
if [[ -n "${ACCESS_TOKEN:-}" ]]; then
  auth_args=(-H "Authorization: Bearer ${ACCESS_TOKEN}")
elif [[ -n "${USERNAME:-}" && -n "${PASSWORD:-}" ]]; then
  echo "logging in as ${USERNAME} @ ${BASE_URL} ..."
  login_resp=$(curl -sS -c "$COOKIE_JAR" -b "$COOKIE_JAR" \
    -H 'Content-Type: application/json' \
    -X POST "${BASE_URL}/api/user/login" \
    -d "$(jq -nc --arg u "$USERNAME" --arg p "$PASSWORD" '{username:$u,password:$p}')")
  if ! echo "$login_resp" | jq -e '.success == true' >/dev/null 2>&1; then
    echo "login failed: $login_resp" >&2
    exit 1
  fi
  if echo "$login_resp" | jq -e '.data.require_2fa == true' >/dev/null 2>&1; then
    echo "account requires 2FA; use ACCESS_TOKEN from UI instead" >&2
    exit 1
  fi
  auth_args=(-b "$COOKIE_JAR" -c "$COOKIE_JAR")
else
  echo "set USERNAME+PASSWORD or ACCESS_TOKEN" >&2
  exit 1
fi

api_get() {
  curl -sS "${auth_args[@]}" "$@"
}
api_put() {
  curl -sS "${auth_args[@]}" -H 'Content-Type: application/json' -X PUT "$@"
}

echo "listing channels group=${GROUP} base=${BASE_URL} ..."
page=1
ids=()
while true; do
  resp=$(api_get "${BASE_URL}/api/channel?group=$(jq -nr --arg g "$GROUP" '$g|@uri')&page=${page}&page_size=${PAGE_SIZE}")
  if ! echo "$resp" | jq -e '.success == true' >/dev/null 2>&1; then
    echo "list failed: $resp" >&2
    exit 1
  fi
  mapfile -t batch < <(echo "$resp" | jq -r '.data.items[]?.id // empty')
  total=$(echo "$resp" | jq -r '.data.total // 0')
  if [[ ${#batch[@]} -eq 0 ]]; then
    break
  fi
  ids+=("${batch[@]}")
  echo "  page ${page}: +${#batch[@]} (total reported ${total})"
  if [[ ${#ids[@]} -ge "$total" ]]; then
    break
  fi
  page=$((page + 1))
done

if [[ ${#ids[@]} -eq 0 ]]; then
  echo "no channels in group '${GROUP}'"
  exit 0
fi

echo "found ${#ids[@]} channel(s); models -> '${MODEL}'"
ok=0
fail=0
for id in "${ids[@]}"; do
  detail=$(api_get "${BASE_URL}/api/channel/${id}")
  if ! echo "$detail" | jq -e '.success == true' >/dev/null 2>&1; then
    echo "  id=${id} GET failed: $detail" >&2
    fail=$((fail + 1))
    continue
  fi
  # Prefer nested data object; fall back if API returns channel at data root.
  ch=$(echo "$detail" | jq '
    if (.data|type)=="object" and (.data.id!=null) then .data
    elif (.data.item|type)=="object" then .data.item
    else .data end
  ')
  name=$(echo "$ch" | jq -r '.name // ""')
  old_models=$(echo "$ch" | jq -r '.models // ""')
  body=$(echo "$ch" | jq --arg m "$MODEL" '.models = $m | .key = (if (.key|type)=="string" then .key else "" end)
    # empty key => keep existing (server-side); strip list-only fields
    | del(.channel_info)
  ')
  if [[ "$DRY_RUN" == "1" ]]; then
    echo "  [dry-run] id=${id} name=${name} models: '${old_models}' -> '${MODEL}'"
    ok=$((ok + 1))
    continue
  fi
  put_resp=$(api_put "${BASE_URL}/api/channel" -d "$body")
  if echo "$put_resp" | jq -e '.success == true' >/dev/null 2>&1; then
    echo "  ok id=${id} name=${name} ('${old_models}' -> '${MODEL}')"
    ok=$((ok + 1))
  else
    echo "  FAIL id=${id} name=${name}: $put_resp" >&2
    fail=$((fail + 1))
  fi
done

echo "done ok=${ok} fail=${fail}"
[[ "$fail" -eq 0 ]]
