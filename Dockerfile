# Multi-stage: control-api + gateway-monoio (host network friendly).
#   docker compose -f docker-compose.host.yml up --build

FROM rust:1-bookworm AS builder
WORKDIR /src

RUN apt-get update \
    && apt-get install -y --no-install-recommends pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock ./
COPY apps apps
COPY crates crates
COPY web web

RUN cargo build --release -p halolake-control-api -p halolake-gateway-monoio

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=builder /src/target/release/halolake-control-api /usr/local/bin/
COPY --from=builder /src/target/release/halolake-gateway-monoio /usr/local/bin/
COPY examples/docker /app/config
COPY docker/entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh && mkdir -p /data

ENV RUST_LOG=info \
    HALOLAKE_CONTROL_CONFIG=/app/config/control-api.toml \
    HALOLAKE_GATEWAY_CONFIG=/app/config/gateway.toml \
    SESSION_SECRET=change-me-in-production

VOLUME ["/data"]
EXPOSE 9090 8082
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
