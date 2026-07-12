#!/usr/bin/env bash
# Parallel channel connectivity test for one routing group.
#
# Strategy for large groups with few live channels:
#   1) Only list status=enabled (STATUS=1)
#   2) Parallel GET /api/channel/test/{id} (CONCURRENCY)
#   3) Short curl timeout so dead endpoints fail fast (CURL_MAX_TIME)
#   4) Prefer ACCESS_TOKEN (required for parallel)
#
# Usage:
#   ACCESS_TOKEN=... BASE_URL=http://127.0.0.1:9090 \
#     CONCURRENCY=30 CURL_MAX_TIME=12 \
#     ./scripts/test-group-channels.sh mygroup grok-4.5
#
# Env:
#   CONCURRENCY=20  CURL_MAX_TIME=15  STREAM=false  STATUS=1
#   ONLY_OK=1       print only successes

set -euo pipefail

GROUP="${1:-${GROUP:-}}"
MODEL="${2:-${MODEL:-}}"
BASE_URL="${BASE_URL:-http://127.0.0.1:9090}"
PAGE_SIZE="${PAGE_SIZE:-200}"
CONCURRENCY="${CONCURRENCY:-20}"
CURL_MAX_TIME="${CURL_MAX_TIME:-15}"
STREAM="${STREAM:-false}"
STATUS="${STATUS:-1}"
ONLY_OK="${ONLY_OK:-0}"

if [[ -z "$GROUP" ]]; then
  echo "usage: ACCESS_TOKEN=... $0 GROUP [MODEL]" >&2
  exit 1
fi
if [[ -z "${ACCESS_TOKEN:-}" ]]; then
  echo "ACCESS_TOKEN required (parallel tests). Get it from Profile → access token." >&2
  exit 1
fi
command -v curl >/dev/null
command -v jq >/dev/null
command -v xargs >/dev/null

AUTH=(-H "Authorization: Bearer ${ACCESS_TOKEN}")
group_q=$(printf '%s' "$GROUP" | jq -sRr @uri)
model_q=$(printf '%s' "$MODEL" | jq -sRr @uri)
status_q=""
[[ -n "$STATUS" ]] && status_q="&status=${STATUS}"

ids_file=$(mktemp)
ok_file=$(mktemp)
fail_file=$(mktemp)
trap 'rm -f "$ids_file" "$ok_file" "$fail_file"' EXIT

echo "list group=${GROUP} status=${STATUS:-all} ..."
page=1
while true; do
  resp=$(curl -sS "${AUTH[@]}" \
    "${BASE_URL}/api/channel?group=${group_q}&page=${page}&page_size=${PAGE_SIZE}${status_q}")
  echo "$resp" | jq -e '.success == true' >/dev/null \
    || { echo "list failed: $resp" >&2; exit 1; }
  echo "$resp" | jq -r '.data.items[]? | [.id, (.name//""), (.models//"")] | @tsv' >>"$ids_file" || true
  n=$(echo "$resp" | jq -r '.data.items|length // 0')
  total=$(echo "$resp" | jq -r '.data.total // 0')
  got=$(wc -l <"$ids_file" | tr -d ' ')
  echo "  page ${page}: +${n} (${got}/${total})"
  [[ "$n" -eq 0 || "$got" -ge "$total" ]] && break
  page=$((page + 1))
done

total_ids=$(wc -l <"$ids_file" | tr -d ' ')
[[ "$total_ids" -eq 0 ]] && { echo "no channels"; exit 0; }

echo "test ${total_ids} channels  concurrency=${CONCURRENCY}  timeout=${CURL_MAX_TIME}s  model='${MODEL:-auto}'"

export BASE_URL MODEL model_q STREAM CURL_MAX_TIME ACCESS_TOKEN ONLY_OK ok_file fail_file
test_one() {
  local id="$1" name="$2"
  local url="${BASE_URL}/api/channel/test/${id}?stream=${STREAM}"
  [[ -n "${MODEL}" ]] && url+="&model=${model_q}"
  local body http ok time msg
  body=$(curl -sS -m "$CURL_MAX_TIME" -w '\n%{http_code}' \
    -H "Authorization: Bearer ${ACCESS_TOKEN}" "$url" 2>/dev/null || true)
  http=$(printf '%s\n' "$body" | tail -n1)
  body=$(printf '%s\n' "$body" | sed '$d')
  ok=$(printf '%s' "$body" | jq -r '.success // false' 2>/dev/null || echo false)
  time=$(printf '%s' "$body" | jq -r '.time // .data.time // empty' 2>/dev/null || true)
  msg=$(printf '%s' "$body" | jq -r '.message // empty' 2>/dev/null || true)
  if [[ "$ok" == "true" ]]; then
    printf 'OK\t%s\t%s\t%ss\n' "$id" "$name" "${time:-?}"
    echo "$id" >>"$ok_file"
  else
    [[ "$ONLY_OK" == "1" ]] || printf 'FAIL\t%s\t%s\thttp=%s\t%s\n' "$id" "$name" "$http" "${msg:-timeout}"
    echo "$id" >>"$fail_file"
  fi
}
export -f test_one

# tab-separated: id name models → pass id name to test_one
while IFS=$'\t' read -r id name _models; do
  printf '%s\0%s\0' "$id" "$name"
done <"$ids_file" | xargs -0 -P "$CONCURRENCY" -n 2 bash -c 'test_one "$1" "$2"' _

ok_n=$(wc -l <"$ok_file" 2>/dev/null | tr -d ' ' || echo 0)
fail_n=$(wc -l <"$fail_file" 2>/dev/null | tr -d ' ' || echo 0)
echo "done ok=${ok_n} fail=${fail_n} total=${total_ids}"
echo "live ids: $(tr '\n' ' ' <"$ok_file")"
