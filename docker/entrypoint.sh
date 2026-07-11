#!/bin/sh
set -eu

CONTROL_CONFIG="${HALOLAKE_CONTROL_CONFIG:-/app/config/control-api.toml}"
GATEWAY_CONFIG="${HALOLAKE_GATEWAY_CONFIG:-/app/config/gateway.toml}"

# Optional: ensure sqlite data dir exists when using docker sqlite config.
mkdir -p /data

echo "starting control-api (${CONTROL_CONFIG}) ..."
halolake-control-api --config "${CONTROL_CONFIG}" &
CONTROL_PID=$!

# Give control-api a moment before gateway polls snapshot.
sleep 1

echo "starting gateway (${GATEWAY_CONFIG}) ..."
halolake-gateway-monoio --config "${GATEWAY_CONFIG}" &
GATEWAY_PID=$!

term() {
  echo "shutting down..."
  kill "${CONTROL_PID}" "${GATEWAY_PID}" 2>/dev/null || true
  wait "${CONTROL_PID}" "${GATEWAY_PID}" 2>/dev/null || true
}
trap term INT TERM

# Exit if either process dies.
while kill -0 "${CONTROL_PID}" 2>/dev/null && kill -0 "${GATEWAY_PID}" 2>/dev/null; do
  sleep 2
done
term
exit 1
