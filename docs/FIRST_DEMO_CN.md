# Halolake First Demo

这份文档记录当前第一个可运行 demo：`control-api` 作为控制面发布 snapshot，`gateway-monoio` 从控制面拉取 snapshot，并把 OpenAI-compatible 请求转发到真实 upstream。

## 组件

- `halolake-control-api`
  - 监听：`127.0.0.1:9090`
  - 配置：`examples/control-api.toml`
  - 职责：后台 API、web 静态托管、内部 snapshot、usage/log/data 回写
- `halolake-gateway-monoio`
  - 监听：`127.0.0.1:8082`
  - 配置：`examples/gateway-control.toml`
  - 职责：OpenAI/Claude/Gemini 兼容入口、鉴权、路由、真实 upstream relay
- `web/new-api`
  - 从 `ref/new-api/web` 同步
  - default/classic 两套主题都可 build

## 准备

复制环境变量模板，填入真实 upstream key：

```sh
cp .env.example .env
```

至少需要：

```sh
OPENAI_API_KEY=...
```

当前 demo 的 `openai-main` 使用：

```toml
base_url = "https://ioll.pp.ua"
api_key_env = "OPENAI_API_KEY"
models = ["deepseek-v4-pro"]
```

安装前端依赖并构建静态资源：

```sh
cd web/new-api
bun install
(cd default && VITE_REACT_APP_VERSION=halolake-demo bun run build)
(cd classic && VITE_REACT_APP_VERSION=halolake-demo bun run build)
```

classic 构建有一个本地兼容处理：`classic/rsbuild.config.ts` 将 `date-fns` alias 到 Semi UI 自带的 `date-fns@2.30.0`，避免 classic 的 `date-fns-tz@1.3.8` 解析到 workspace 根部 `date-fns@4.x`。

## 启动

推荐一键启动：

```sh
make demo
```

等价命令：

```sh
./scripts/first-demo.sh
```

脚本会：

- 读取 `.env`
- 检查 `OPENAI_API_KEY`
- 在缺少前端 dist 时自动构建 default/classic web
- 启动 control-api 和 gateway
- 等待 `9090`、`8082` 就绪
- 按 `Ctrl-C` 时同时清理两个子进程

强制重建前端：

```sh
./scripts/first-demo.sh --rebuild-web
```

跳过前端构建检查：

```sh
./scripts/first-demo.sh --skip-web-build
```

手动启动方式如下。

先加载 `.env`：

```sh
set -a
. ./.env
set +a
```

启动 control-api：

```sh
cargo run -p halolake-control-api -- --config examples/control-api.toml
```

另开一个终端，同样加载 `.env`，启动 gateway：

```sh
set -a
. ./.env
set +a
cargo run -p halolake-gateway-monoio -- --config examples/gateway-control.toml
```

访问 web：

```text
http://127.0.0.1:9090/
```

默认登录：

```text
root / halolake-root-dev
```

## 快速验证

control-api 健康检查：

```sh
curl -sS http://127.0.0.1:9090/healthz
```

检查 gateway 从 control-api 拿到的模型：

```sh
curl -sS http://127.0.0.1:8082/v1/models | jq
```

检查 control-api 发布的 snapshot，不打印真实 key，只看 key 是否已从环境变量解析：

```sh
curl -sS \
  -H 'x-halolake-internal-key: dev-internal-secret' \
  http://127.0.0.1:9090/internal/gateway/snapshot |
  jq '{channels:[.snapshot.channels[] | {id, provider, base_url, api_key_len:(.api_key|length), api_key_env, models}], model_mappings:.snapshot.model_mappings}'
```

通过 gateway 发真实 OpenAI-compatible 请求：

```sh
curl -sS http://127.0.0.1:8082/v1/chat/completions \
  -H 'Authorization: Bearer dev-token' \
  -H 'Content-Type: application/json' \
  -d '{
    "model": "deepseek-v4-pro",
    "messages": [{"role": "user", "content": "请只回复 pong"}],
    "max_tokens": 16,
    "stream": false
  }' | jq
```

`x-api-key` 也支持：

```sh
curl -sS http://127.0.0.1:8082/v1/chat/completions \
  -H 'x-api-key: dev-token' \
  -H 'Content-Type: application/json' \
  -d '{
    "model": "deepseek-v4-pro",
    "messages": [{"role": "user", "content": "请只回复 pong"}],
    "max_tokens": 16,
    "stream": false
  }' | jq
```

查看 usage/log 回写：

```sh
curl -sS http://127.0.0.1:9090/api/user/login \
  -H 'Content-Type: application/json' \
  -c /tmp/halolake-control.cookie \
  -d '{"username":"root","password":"halolake-root-dev"}'

curl -sS -b /tmp/halolake-control.cookie \
  'http://127.0.0.1:9090/api/log/?p=0&page_size=5' |
  jq '.data.items[] | {type, content, model_name, quota, prompt_tokens, completion_tokens, channel, token_id, use_time, upstream_request_id}'
```

成功请求应记录为：

- `content = "consume quota"`
- `channel = "openai-main"`
- `token_id = "dev-token"`
- 有 prompt/completion token 和 quota

## 调试

确认端口：

```sh
lsof -nP -iTCP:9090 -sTCP:LISTEN
lsof -nP -iTCP:8082 -sTCP:LISTEN
```

确认进程：

```sh
ps -axo pid,ppid,command | rg 'halolake-(control-api|gateway-monoio)'
```

开启 debug 日志：

```sh
RUST_LOG=halolake_gateway_monoio=debug,halolake_control_api=debug,info \
cargo run -p halolake-gateway-monoio -- --config examples/gateway-control.toml
```

常见问题：

- `invalid internal key`
  - internal API 必须使用 `x-halolake-internal-key: dev-internal-secret`，不是 Bearer token。
- upstream 返回 `Invalid token`
  - 检查 `.env` 是否已加载，以及 control-api snapshot 里 `api_key_len` 是否大于 0。
- gateway 返回 `missing gateway token`
  - 下游请求需要 `Authorization: Bearer dev-token` 或 `x-api-key: dev-token`。
- gateway 返回 `model is not allowed for this token`
  - 请求模型必须在 `examples/control-api.toml` 的 `[[tokens]].allowed_models` 中。
- web 404 或静态资源旧
  - 重新构建 `web/new-api/default`，control-api 默认托管 `web/new-api/default/dist`。

## 纯 gateway 对照启动

`examples/gateway.toml` 不经过 control-api，直接从本地 TOML snapshot 启动 gateway，监听 `8081`：

```sh
set -a
. ./.env
set +a
cargo run -p halolake-gateway-monoio -- --config examples/gateway.toml
```

这个模式适合对照 relay 本身是否可用；完整 demo 推荐使用 `examples/control-api.toml` + `examples/gateway-control.toml`。

## 还缺什么

当前 demo 已经打通：

- control-api 发布 snapshot
- gateway 从 control-api 拉取 snapshot
- `OPENAI_API_KEY` 环境变量 upstream key
- OpenAI-compatible chat 转发成功
- usage/log 回写到 control-api
- new-api web 静态托管

仍未完成或需要优先补齐：

- 前端调用面还没完整覆盖，`/api/deployments/*`、订阅、支付、OAuth、异步任务等仍缺。
- `PUT /api/models/?status_only=true` 当前会因为缺 `model_name` 返回 422，需要兼容前端只传 `{id,status}` 的 payload。
- 默认 storage 是 memory；SQLite 可用，但 MySQL/PostgreSQL 仍只是识别 DSN，尚未实现 store。
- session 仍是进程内 memory session 和裸 UUID cookie，生产化需要签名、过期清理和持久化。
- gateway retry、跨 group retry、更多 raw endpoint 的稳定性仍需补齐。
- `POST /v1/completions` 曾观察到一次经 gateway 超时，直接打上游正常，建议单独排查 raw completions 路径或 monoio upstream 连接复用。
- channel 真实探测、自动禁用、多 key 反馈已经有基础，但还需要更完整的端到端测试。
