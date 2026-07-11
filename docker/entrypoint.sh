#!/bin/sh
set -eu

CONTROL_CONFIG="${HALOLAKE_CONTROL_CONFIG:-/app/config/control-api.toml}"
GATEWAY_CONFIG="${HALOLAKE_GATEWAY_CONFIG:-/app/config/gateway.toml}"
CREDENTIALS_FILE="${HALOLAKE_CREDENTIALS_FILE:-/data/halolake-credentials.txt}"
CONTROL_URL="${HALOLAKE_CONTROL_URL:-http://127.0.0.1:9090}"
# How long to wait for control-api /healthz before starting gateway.
WAIT_CONTROL_SECS="${HALOLAKE_WAIT_CONTROL_SECS:-90}"

mkdir -p /data

load_credentials() {
  if [ ! -f "${CREDENTIALS_FILE}" ]; then
    return 0
  fi
  while IFS= read -r line || [ -n "$line" ]; do
    case "$line" in
      \#*|'') continue ;;
      session_secret=*)
        export SESSION_SECRET="${line#session_secret=}"
        ;;
      internal_secret=*)
        val="${line#internal_secret=}"
        export HALOLAKE_INTERNAL_SECRET="$val"
        export HALOLAKE_INTERNAL_KEY="$val"
        ;;
    esac
  done < "${CREDENTIALS_FILE}"
}

# Pre-load if file already exists from a previous run.
load_credentials

echo "starting control-api (${CONTROL_CONFIG}) ..."
halolake-control-api --config "${CONTROL_CONFIG}" &
CONTROL_PID=$!

echo "waiting for control-api at ${CONTROL_URL}/healthz (up to ${WAIT_CONTROL_SECS}s) ..."
i=0
while [ "$i" -lt "$WAIT_CONTROL_SECS" ]; do
  if ! kill -0 "${CONTROL_PID}" 2>/dev/null; then
    echo "control-api exited before becoming healthy" >&2
    wait "${CONTROL_PID}" || true
    exit 1
  fi
  if curl -fsS "${CONTROL_URL}/healthz" >/dev/null 2>&1; then
    echo "control-api is healthy"
    break
  fi
  i=$((i + 1))
  sleep 1
done

if ! curl -fsS "${CONTROL_URL}/healthz" >/dev/null 2>&1; then
  echo "control-api did not become healthy within ${WAIT_CONTROL_SECS}s" >&2
  kill "${CONTROL_PID}" 2>/dev/null || true
  wait "${CONTROL_PID}" 2>/dev/null || true
  exit 1
fi

# Re-load secrets generated on first boot.
load_credentials
if [ -f "${CREDENTIALS_FILE}" ]; then
  echo "bootstrap credentials file present: ${CREDENTIALS_FILE}"
  echo "  (username/password are in that file — not printed here)"
fi

RUNTIME_GATEWAY_CONFIG="${GATEWAY_CONFIG}"
if [ -n "${HALOLAKE_INTERNAL_KEY:-}" ]; then
  RUNTIME_GATEWAY_CONFIG="/tmp/gateway.runtime.toml"
  if grep -q 'internal_key' "${GATEWAY_CONFIG}" 2>/dev/null; then
    sed "s|^internal_key *= *\"\"|internal_key = \"${HALOLAKE_INTERNAL_KEY}\"|" \
      "${GATEWAY_CONFIG}" > "${RUNTIME_GATEWAY_CONFIG}"
  else
    cp "${GATEWAY_CONFIG}" "${RUNTIME_GATEWAY_CONFIG}"
    printf '\n[control]\ninternal_key = "%s"\n' "${HALOLAKE_INTERNAL_KEY}" >> "${RUNTIME_GATEWAY_CONFIG}"
  fi
fi

echo "starting gateway (${RUNTIME_GATEWAY_CONFIG}) ..."
halolake-gateway-monoio --config "${RUNTIME_GATEWAY_CONFIG}" &
GATEWAY_PID=$!

term() {
  echo "shutting down..."
  kill "${CONTROL_PID}" "${GATEWAY_PID}" 2>/dev/null || true
  wait "${CONTROL_PID}" "${GATEWAY_PID}" 2>/dev/null || true
}
trap term INT TERM

# Keep container alive while both are running; restart gateway if it dies but control is up.
while kill -0 "${CONTROL_PID}" 2>/dev/null; do
  if ! kill -0 "${GATEWAY_PID}" 2>/dev/null; then
    echo "gateway exited; restarting in 3s ..."
    sleep 3
    if ! kill -0 "${CONTROL_PID}" 2>/dev/null; then
      break
    fi
    load_credentials
    if [ -n "${HALOLAKE_INTERNAL_KEY:-}" ] && [ -f "${GATEWAY_CONFIG}" ]; then
      RUNTIME_GATEWAY_CONFIG="/tmp/gateway.runtime.toml"
      if grep -q 'internal_key' "${GATEWAY_CONFIG}" 2>/dev/null; then
        sed "s|^internal_key *= *\"\"|internal_key = \"${HALOLAKE_INTERNAL_KEY}\"|" \
          "${GATEWAY_CONFIG}" > "${RUNTIME_GATEWAY_CONFIG}"
      else
        cp "${GATEWAY_CONFIG}" "${RUNTIME_GATEWAY_CONFIG}"
        printf '\n[control]\ninternal_key = "%s"\n' "${HALOLAKE_INTERNAL_KEY}" >> "${RUNTIME_GATEWAY_CONFIG}"
      fi
    fi
    echo "starting gateway (${RUNTIME_GATEWAY_CONFIG}) ..."
    halolake-gateway-monoio --config "${RUNTIME_GATEWAY_CONFIG}" &
    GATEWAY_PID=$!
  fi
  sleep 2
done

echo "control-api exited"
term
exit 1
