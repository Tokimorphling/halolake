#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CONTROL_CONFIG="${CONTROL_CONFIG:-examples/control-api.toml}"
GATEWAY_CONFIG="${GATEWAY_CONFIG:-examples/gateway-control.toml}"
ENV_FILE="${ENV_FILE:-.env}"
BUILD_WEB=auto

usage() {
  cat <<'USAGE'
Usage: scripts/first-demo.sh [--rebuild-web] [--skip-web-build]

Starts the first demo stack:
  - control-api on 127.0.0.1:9090
  - gateway-monoio on 127.0.0.1:8082
  - web served by control-api from web/new-api/default/dist

Environment:
  ENV_FILE=.env
  CONTROL_CONFIG=examples/control-api.toml
  GATEWAY_CONFIG=examples/gateway-control.toml
  RUST_LOG=info
USAGE
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --rebuild-web)
      BUILD_WEB=yes
      shift
      ;;
    --skip-web-build)
      BUILD_WEB=no
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

cd "$ROOT_DIR"

if [[ ! -f "$ENV_FILE" ]]; then
  echo "missing $ENV_FILE; copy .env.example to .env and fill OPENAI_API_KEY" >&2
  exit 1
fi

set -a
# shellcheck disable=SC1090
. "$ENV_FILE"
set +a

if [[ -z "${OPENAI_API_KEY:-}" ]]; then
  echo "OPENAI_API_KEY is not set in $ENV_FILE" >&2
  exit 1
fi

need_cmd() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "missing required command: $1" >&2
    exit 1
  fi
}

need_cmd cargo
need_cmd curl
if [[ "$BUILD_WEB" != no ]]; then
  need_cmd bun
fi

require_free_port() {
  local port="$1"
  if lsof -nP -iTCP:"$port" -sTCP:LISTEN >/dev/null 2>&1; then
    echo "port $port is already in use; stop the existing process or use manual configs" >&2
    lsof -nP -iTCP:"$port" -sTCP:LISTEN >&2 || true
    exit 1
  fi
}

require_free_port 9090
require_free_port 8082

dist_missing=false
[[ -f web/new-api/default/dist/index.html ]] || dist_missing=true
[[ -f web/new-api/classic/dist/index.html ]] || dist_missing=true

if [[ "$BUILD_WEB" == yes || ( "$BUILD_WEB" == auto && "$dist_missing" == true ) ]]; then
  echo "[demo] building web assets"
  (
    cd web/new-api
    bun install
    (cd default && VITE_REACT_APP_VERSION=halolake-demo bun run build)
    (cd classic && VITE_REACT_APP_VERSION=halolake-demo bun run build)
  )
elif [[ "$BUILD_WEB" == auto ]]; then
  echo "[demo] web dist exists; use --rebuild-web to rebuild"
fi

wait_for_http() {
  local url="$1"
  local name="$2"
  local attempt

  for attempt in $(seq 1 60); do
    if curl -fsS "$url" >/dev/null 2>&1; then
      echo "[demo] $name is ready: $url"
      return 0
    fi
    sleep 0.5
  done

  echo "$name did not become ready: $url" >&2
  return 1
}

CONTROL_PID=""
GATEWAY_PID=""

cleanup() {
  local status=$?
  trap - EXIT INT TERM
  if [[ -n "$GATEWAY_PID" ]]; then
    kill "$GATEWAY_PID" >/dev/null 2>&1 || true
  fi
  if [[ -n "$CONTROL_PID" ]]; then
    kill "$CONTROL_PID" >/dev/null 2>&1 || true
  fi
  wait "$GATEWAY_PID" "$CONTROL_PID" >/dev/null 2>&1 || true
  exit "$status"
}

trap cleanup EXIT INT TERM

echo "[demo] starting control-api with $CONTROL_CONFIG"
RUST_LOG="${RUST_LOG:-info}" cargo run -p halolake-control-api -- --config "$CONTROL_CONFIG" &
CONTROL_PID=$!

wait_for_http "http://127.0.0.1:9090/healthz" "control-api"

echo "[demo] starting gateway with $GATEWAY_CONFIG"
RUST_LOG="${RUST_LOG:-info}" cargo run -p halolake-gateway-monoio -- --config "$GATEWAY_CONFIG" &
GATEWAY_PID=$!

wait_for_http "http://127.0.0.1:8082/v1/models" "gateway"

cat <<'READY'

[demo] ready
  web:     http://127.0.0.1:9090/
  gateway: http://127.0.0.1:8082/v1/models

Try:
  curl -sS http://127.0.0.1:8082/v1/chat/completions \
    -H 'Authorization: Bearer dev-token' \
    -H 'Content-Type: application/json' \
    -d '{"model":"deepseek-v4-pro","messages":[{"role":"user","content":"请只回复 pong"}],"max_tokens":16,"stream":false}'

Press Ctrl-C to stop both processes.
READY

wait "$CONTROL_PID" "$GATEWAY_PID"
