#!/bin/sh
set -eu

CONTROL_CONFIG="${HALOLAKE_CONTROL_CONFIG:-/app/config/control-api.toml}"
GATEWAY_CONFIG="${HALOLAKE_GATEWAY_CONFIG:-/app/config/gateway.toml}"
CREDENTIALS_FILE="${HALOLAKE_CREDENTIALS_FILE:-/data/halolake-credentials.txt}"

mkdir -p /data

# If secrets were auto-generated into the credentials file, export them for this process
# tree so the gateway can read the same internal key without baking it into config.
if [ -f "${CREDENTIALS_FILE}" ]; then
  # shellcheck disable=SC1090
  # Parse KEY=VALUE lines (ignore comments).
  while IFS= read -r line || [ -n "$line" ]; do
    case "$line" in
      \#*|'') continue ;;
      session_secret=*)
        if [ -z "${SESSION_SECRET:-}" ]; then
          export SESSION_SECRET="${line#session_secret=}"
        fi
        ;;
      internal_secret=*)
        val="${line#internal_secret=}"
        if [ -z "${HALOLAKE_INTERNAL_SECRET:-}" ]; then
          export HALOLAKE_INTERNAL_SECRET="$val"
        fi
        # gateway.toml uses internal_key; also export common env name if gateway supports it later
        if [ -z "${HALOLAKE_INTERNAL_KEY:-}" ]; then
          export HALOLAKE_INTERNAL_KEY="$val"
        fi
        ;;
    esac
  done < "${CREDENTIALS_FILE}"
fi

echo "starting control-api (${CONTROL_CONFIG}) ..."
halolake-control-api --config "${CONTROL_CONFIG}" &
CONTROL_PID=$!

# Allow control-api to bootstrap credentials + bind before gateway polls.
sleep 2

# Re-read credentials after control-api may have just generated them.
if [ -f "${CREDENTIALS_FILE}" ]; then
  while IFS= read -r line || [ -n "$line" ]; do
    case "$line" in
      \#*|'') continue ;;
      internal_secret=*)
        val="${line#internal_secret=}"
        export HALOLAKE_INTERNAL_SECRET="$val"
        export HALOLAKE_INTERNAL_KEY="$val"
        ;;
      session_secret=*)
        export SESSION_SECRET="${line#session_secret=}"
        ;;
    esac
  done < "${CREDENTIALS_FILE}"
  echo "bootstrap credentials file present: ${CREDENTIALS_FILE}"
  echo "  (username/password are in that file — not printed here)"
fi

# If gateway config has empty internal_key, rewrite a runtime copy with the secret.
RUNTIME_GATEWAY_CONFIG="${GATEWAY_CONFIG}"
if [ -n "${HALOLAKE_INTERNAL_KEY:-}" ]; then
  RUNTIME_GATEWAY_CONFIG="/tmp/gateway.runtime.toml"
  # Replace empty internal_key = "" if present; otherwise append under [control].
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

while kill -0 "${CONTROL_PID}" 2>/dev/null && kill -0 "${GATEWAY_PID}" 2>/dev/null; do
  sleep 2
done
term
exit 1
