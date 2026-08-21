# syntax=docker/dockerfile:1.7
# Multi-stage: web dist + control-api + gateway.
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

# --- Rust (the container serves the frontend from a separate runtime layer) ---
FROM rust:1-bookworm AS rust-source
WORKDIR /src
RUN apt-get update \
    && apt-get install -y --no-install-recommends cmake \
    && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock ./
COPY apps apps
COPY crates crates
COPY examples examples

# sharing=locked: BuildKit runs both builder stages in parallel; without this they
# race unpacking crates into the shared registry ("failed to open .cargo-ok: File exists").
# Note: cargo's package lock lives in /usr/local/cargo, outside these mounts, so it
# cannot arbitrate between the two stage containers.
FROM rust-source AS control-builder
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    cargo build --profile dist -p halolake-control-api \
    --no-default-features \
    --features core,admin-extras,compat-stubs,allocator-mimalloc,passkey \
    && mkdir -p /artifacts \
    && cp target/dist/halolake-control-api /artifacts/

FROM rust-source AS gateway-builder
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    cargo build --profile dist -p halolake-gateway-monoio \
    && mkdir -p /artifacts \
    && cp target/dist/halolake-gateway-monoio /artifacts/

# --- Shared runtime ---
FROM debian:bookworm-slim AS runtime-base
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
ENV RUST_LOG=info

# Build one-service images with `docker build --target <name> ...`.
FROM runtime-base AS control-api
RUN apt-get update \
    && apt-get install -y --no-install-recommends curl \
    && rm -rf /var/lib/apt/lists/*
COPY --from=control-builder /artifacts/halolake-control-api /usr/local/bin/
COPY --from=web /web/default/dist /app/web/default
COPY examples/docker/control-api.toml /app/config/control-api.toml
RUN mkdir -p /data
ENV HALOLAKE_CONTROL_CONFIG=/app/config/control-api.toml
VOLUME ["/data"]
EXPOSE 9090
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
  CMD curl -fsS http://127.0.0.1:9090/healthz || exit 1
ENTRYPOINT ["halolake-control-api"]
CMD ["--config", "/app/config/control-api.toml"]

FROM runtime-base AS gateway
COPY --from=gateway-builder /artifacts/halolake-gateway-monoio /usr/local/bin/
COPY examples/docker/gateway.toml /app/config/gateway.toml
ENV HALOLAKE_GATEWAY_CONFIG=/app/config/gateway.toml
EXPOSE 8082
ENTRYPOINT ["halolake-gateway-monoio"]
CMD ["--config", "/app/config/gateway.toml"]

# Default image runs control-api and gateway together.
FROM runtime-base AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends curl \
    && rm -rf /var/lib/apt/lists/*

COPY --from=control-builder /artifacts/halolake-control-api /usr/local/bin/
COPY --from=gateway-builder /artifacts/halolake-gateway-monoio /usr/local/bin/
COPY --from=web /web/default/dist /app/web/default
COPY examples/docker /app/config
COPY docker/entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh && mkdir -p /data

ENV HALOLAKE_CONTROL_CONFIG=/app/config/control-api.toml \
    HALOLAKE_GATEWAY_CONFIG=/app/config/gateway.toml

VOLUME ["/data"]
EXPOSE 9090 8082
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
  CMD curl -fsS http://127.0.0.1:9090/healthz || exit 1
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
