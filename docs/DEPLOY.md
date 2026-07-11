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

mkdir -p data
docker compose -f docker-compose.pull.yml up -d
```

- Admin UI / control-api: http://localhost:9090  
- Gateway: http://localhost:8082  

### First-boot credentials (important)

On **first start with an empty database**, Halolake generates:

| Key | Where |
|-----|--------|
| Admin **username** (default `admin`) | `/data/halolake-credentials.txt` |
| Strong random **password** | same file |
| `session_secret` | same file (+ used for cookie signing) |
| `internal_secret` | same file (control ↔ gateway) |

```bash
# After the container is up:
docker exec halolake cat /data/halolake-credentials.txt
# or on the host if you mounted ./data:
cat data/halolake-credentials.txt
```

File mode is `0600`. **Password is never printed to container logs** (only a path notice).  
Change the password after first login and enable 2FA.

Env overrides:

| Env | Effect |
|-----|--------|
| `HALOLAKE_CREDENTIALS_FILE` | Path for the credentials file (default `/data/halolake-credentials.txt`) |
| `HALOLAKE_ADMIN_USERNAME` | Admin username when auto-creating root (default `admin`, max 12 chars) |
| `HALOLAKE_AUTO_BOOTSTRAP` | `0`/`false` disables auto root + uses config `[[users]]` seed instead |
| `SESSION_SECRET` | If set, used instead of generating/writing a new session secret |
| `HALOLAKE_INTERNAL_SECRET` / `HALOLAKE_INTERNAL_KEY` | Shared internal API key for gateway |

Host-network (Linux):

```bash
docker run -d --name halolake --network host \
  -v "$PWD/data:/data" \
  "$HALOLAKE_IMAGE"
cat data/halolake-credentials.txt
```

## Local build

```bash
docker compose -f docker-compose.host.yml up --build
cat data/halolake-credentials.txt
```

## Optional OTEL

```bash
export OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4317
export OTEL_SERVICE_NAME=halolake
# add to compose environment or docker run -e
```

## Auth import

See [AUTH_IMPORT.md](./AUTH_IMPORT.md) for Sub2API / CLIProxyAPI / Codex imports.
