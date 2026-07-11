# Multi-stage: web dist + control-api + gateway (one-shot deploy).
#
#   docker compose -f docker-compose.host.yml up --build
#   docker compose -f docker-compose.pull.yml up -d   # after GHCR publish
#
# Ports: control-api 9090, gateway 8082

# --- Frontend ---
FROM oven/bun:1 AS web
WORKDIR /web
COPY web/new-api/ ./
RUN bun install
WORKDIR /web/default
RUN bun run build
WORKDIR /web/classic
RUN bun run build || mkdir -p dist

# --- Rust (embeds dist via apps/control-api/build.rs) ---
FROM rust:1-bookworm AS builder
WORKDIR /src
RUN apt-get update \
    && apt-get install -y --no-install-recommends pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock ./
COPY apps apps
COPY crates crates
COPY examples examples
COPY --from=web /web/default/dist web/new-api/default/dist
COPY --from=web /web/classic/dist web/new-api/classic/dist

ENV HALOLAKE_WEB_BUILD_ID=docker
RUN cargo build --release -p halolake-control-api -p halolake-gateway-monoio

# --- Runtime ---
FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libssl3 curl \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=builder /src/target/release/halolake-control-api /usr/local/bin/
COPY --from=builder /src/target/release/halolake-gateway-monoio /usr/local/bin/
COPY examples/docker /app/config
COPY docker/entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh && mkdir -p /data

ENV RUST_LOG=info \
    HALOLAKE_CONTROL_CONFIG=/app/config/control-api.toml \
    HALOLAKE_GATEWAY_CONFIG=/app/config/gateway.toml

VOLUME ["/data"]
EXPOSE 9090 8082
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
  CMD curl -fsS http://127.0.0.1:9090/healthz || exit 1
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
