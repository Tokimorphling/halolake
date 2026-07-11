# Self-hosted one-shot deploy (Halolake)

## Image

GitHub Actions builds and pushes:

`ghcr.io/<github-owner>/<repo>:latest` (on version tags `v*`)  
`ghcr.io/<github-owner>/<repo>:main` (on push to main)

Workflow: `.github/workflows/docker.yml`

Make the package public (or `docker login ghcr.io`) before pull.

## Quick start

```bash
export HALOLAKE_IMAGE=ghcr.io/<owner>/<repo>:latest
export SESSION_SECRET="$(openssl rand -hex 32)"

mkdir -p data
docker compose -f docker-compose.pull.yml up -d
```

- Admin UI / control-api: http://localhost:9090  
- Gateway: http://localhost:8082  

Default seed user (from image config): **username** `root` / password `halolake-root-dev`  
(Login field is username, not email.) Enable 2FA under profile after first login.

Host-network (Linux):

```bash
docker run -d --name halolake --network host \
  -e SESSION_SECRET="$SESSION_SECRET" \
  -v "$PWD/data:/data" \
  "$HALOLAKE_IMAGE"
```

## Local build

```bash
docker compose -f docker-compose.host.yml up --build
```

## Optional OTEL

```bash
export OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4317
export OTEL_SERVICE_NAME=halolake
# add to compose environment or docker run -e
```

## Auth import

See [AUTH_IMPORT.md](./AUTH_IMPORT.md) for Sub2API / CLIProxyAPI / Codex imports.
