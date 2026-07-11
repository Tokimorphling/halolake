# Halolake 部署指南

面向自用 / 单机：一个 Docker 镜像同时跑 **control-api**（管理面 + Web）和 **gateway**（API 中继）。

| 组件 | 默认端口 | 作用 |
|------|----------|------|
| control-api | **9090** | 管理 UI、用户/渠道/代理、snapshot、用量回写 |
| gateway | **8082** | OpenAI / Claude / Gemini 兼容入口 → 上游 |

---

## 1. 最快：不 clone 仓库，只下 compose

镜像由 GitHub Actions 推到 GHCR（**linux/arm64**，原生 ARM runner）：

```text
ghcr.io/tokimorphling/halolake:main     # 推 main
ghcr.io/tokimorphling/halolake:latest   # 打 v* tag
```

**前提**：线上机为 **arm64**；已装 Docker + Compose；GHCR 包需 **Public**，或先 `docker login ghcr.io`。

### 1.1 一键（推荐）

```bash
mkdir -p ~/halolake && cd ~/halolake
curl -fsSL -o docker-compose.yml \
  https://raw.githubusercontent.com/Tokimorphling/halolake/main/docker-compose.pull.yml
docker compose up -d
# 首次启动后读取管理员账号（密码不会打到 docker logs）
sleep 3 && cat data/halolake-credentials.txt
```

### 1.1b 挂到现有 Sub2API 网络 + 复用其 Postgres

见仓库根目录 `docker-compose.pull.sub2api.yml`（网络名 `sub2api_sub2api-network`）。

```bash
mkdir -p ~/halolake && cd ~/halolake
curl -fsSL -o docker-compose.yml \
  https://raw.githubusercontent.com/Tokimorphling/halolake/main/docker-compose.pull.sub2api.yml

# 与 sub2api 相同的 Postgres 账号（从 sub2api .env 抄）
export POSTGRES_USER=...
export POSTGRES_PASSWORD=...
# 新建库，勿共用 sub2api 业务库：
docker exec -it sub2api-postgres \
  psql -U "$POSTGRES_USER" -c 'CREATE DATABASE halolake;'

docker compose up -d
cat data/halolake-credentials.txt
```

容器内通过服务名访问：`sub2api-postgres:5432`、代理容器名等。  
本机端口仅绑 `127.0.0.1:9090` / `8082`，交给 Caddy。

等价纯 `docker run`：

```bash
mkdir -p data
docker run -d --name halolake --restart unless-stopped \
  -p 9090:9090 -p 8082:8082 \
  -v "$PWD/data:/data" \
  ghcr.io/tokimorphling/halolake:main
cat data/halolake-credentials.txt
```

私有镜像：

```bash
echo "$GHCR_TOKEN" | docker login ghcr.io -u YOUR_GITHUB_USER --password-stdin
# 可选覆盖镜像：export HALOLAKE_IMAGE=ghcr.io/tokimorphling/halolake:main
```

### 1.2 首次登录凭据（必看）

**空库首次启动**会自动生成强密码与密钥，写入：

```text
./data/halolake-credentials.txt    # 宿主机（已挂载 ./data:/data）
/data/halolake-credentials.txt     # 容器内
```

```bash
# 启动后立刻查看（密码不会出现在 docker logs 里）
cat data/halolake-credentials.txt
# 或
docker exec halolake cat /data/halolake-credentials.txt
```

示例内容：

```text
# Halolake bootstrap credentials (generated once)
username=admin
password=<约43位随机串>
session_secret=...
internal_secret=...
role=root
generated_at=unix:...
```

| 字段 | 含义 |
|------|------|
| `username` / `password` | Web 登录（登录框是 **用户名**，不是邮箱） |
| `session_secret` | Cookie HMAC |
| `internal_secret` | control ↔ gateway 内部 API 密钥 |

文件权限 `0600`。登录后请立刻改密并开启 **2FA**。

### 1.3 Host 网络（Linux）

```bash
docker run -d --name halolake --network host \
  -v "$PWD/data:/data" \
  "$HALOLAKE_IMAGE"
cat data/halolake-credentials.txt
```

macOS Docker Desktop 的 host 网络能力有限，优先用 `docker-compose.pull.yml` 端口映射。

---

## 2. 本地构建

```bash
mkdir -p data
docker compose -f docker-compose.host.yml up --build -d
cat data/halolake-credentials.txt
```

镜像内会：bun 构建 `web/new-api` → cargo release 编入 control-api / gateway。

---

## 3. 环境变量

| 变量 | 默认 | 说明 |
|------|------|------|
| `HALOLAKE_CREDENTIALS_FILE` | `/data/halolake-credentials.txt` | 凭据文件路径 |
| `HALOLAKE_ADMIN_USERNAME` | `admin` | 自动创建 root 时的用户名（≤12 字符） |
| `HALOLAKE_AUTO_BOOTSTRAP` | 开启 | 设为 `0`/`false` 时禁用自动 root，改用配置 `[[users]]` |
| `SESSION_SECRET` | 自动生成 | 若已设置则不再写入新 session_secret |
| `HALOLAKE_INTERNAL_SECRET` / `HALOLAKE_INTERNAL_KEY` | 自动生成 | 网关读 internal key |
| `HALOLAKE_CONTROL_CONFIG` | `/app/config/control-api.toml` | control 配置 |
| `HALOLAKE_GATEWAY_CONFIG` | `/app/config/gateway.toml` | gateway 配置 |
| `RUST_LOG` | `info` | 日志级别；请求体预览需 `debug` |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | 未设置 | 设置后导出 OTLP（如 `http://127.0.0.1:4317`） |
| `OTEL_SERVICE_NAME` | 进程名 | OTEL service name |
| `OTEL_EXPORTER_OTLP_PROTOCOL` | `grpc` | 或 `http/protobuf` |

覆盖配置（不重建镜像）：

```yaml
# docker-compose.pull.yml volumes 示例
volumes:
  - ./data:/data
  - ./my-control.toml:/app/config/control-api.toml:ro
  - ./my-gateway.toml:/app/config/gateway.toml:ro
```

---

## 4. 上线检查清单

1. `cat data/halolake-credentials.txt` 保存好用户名/密码  
2. 打开 http://&lt;host&gt;:9090 登录  
3. 修改密码 + 开启 2FA  
4. 配置渠道 / 导入 auth（见 [AUTH_IMPORT.md](./AUTH_IMPORT.md)）  
5. 客户端 API Base URL 指向 gateway：`http://&lt;host&gt;:8082`  
6. 用 Token 调 `/v1/chat/completions` 等做一次冒烟  
7. （可选）配置 OTEL endpoint  

数据目录 `./data` 含 SQLite 与凭据文件，**备份与权限务必管好**。

---

## 5. Auth 导入（特色能力）

管理 UI：**Channels → ⋯ → Import credentials**

或 API：

| 方法 | 路径 | 用途 |
|------|------|------|
| `POST` | `/api/channel/import/auth` | JSON：`content` / `contents[]`，`format=auto` |
| `POST` | `/api/channel/import/auth/upload` | multipart 多文件（类 CLIProxyAPI） |
| `POST` | `/api/channel/import/sub2api-data` | 仅 Sub2API 备份 JSON |
| `POST` | `/api/channel/import/codex-auth` | 仅 Codex session |

支持：

- **Sub2API** `sub2api-data` 导出（proxies + accounts）  
- **CLIProxyAPI** `auths/*.json`（`type: codex|claude|gemini`）  
- **Codex / ChatGPT OAuth** session JSON 或 access token  

详情：[AUTH_IMPORT.md](./AUTH_IMPORT.md)

---

## 6. 架构（单容器）

```text
客户端 ──:8082──► gateway-monoio ──► 上游 API
                      │
                      │ internal (snapshot / usage / feedback)
                      ▼
用户浏览器 ──:9090──► control-api (+ 嵌入 Web)
                      │
                      ▼
                   /data/*.db + credentials.txt
```

entrypoint 会启动两个进程，并把凭据文件里的 `internal_secret` 注入 gateway。

---

## 7. 排障

| 现象 | 处理 |
|------|------|
| 打不开 9090 | `docker logs halolake`；确认端口映射 / 防火墙 |
| 忘记密码 | 空库可删 `data/*.db` 与 credentials 后重建（**会丢数据**）；或 DB 里改用户哈希 |
| gateway 拉不到 snapshot | 确认同一容器内 `internal_secret` 一致；看 credentials 与 entrypoint 日志 |
| 管理 UI 空白 | 镜像需包含前端 dist（正式 Dockerfile 会 build web）；确认用 GHCR / 完整 build |
| 登录框要用邮箱 | 当前字段是 **username**；可把 username 设成邮箱字符串 |

---

## 8. 相关文件

| 文件 | 说明 |
|------|------|
| `Dockerfile` | 多阶段：web + rust + runtime |
| `docker-compose.pull.yml` | 拉镜像一键起 |
| `docker-compose.host.yml` | 本地 build + host 网络 |
| `docker/entrypoint.sh` | 双进程 + 凭据注入 |
| `examples/docker/*.toml` | 镜像内默认配置（无弱密码种子用户） |
| `.github/workflows/docker.yml` | GHCR 发布 |
| [AUTH_IMPORT.md](./AUTH_IMPORT.md) | 凭证导入 |
