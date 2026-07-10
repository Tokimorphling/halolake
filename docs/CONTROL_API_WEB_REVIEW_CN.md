# control-api 与 web 前端完成度 Review

日期：2026-07-10
对照基准：

- 本地 `ref/new-api` 已 fast-forward 到 `f2c7cd33`
  - `f2c7cd33 fix: remove sample special usable groups leaking into pricing page (#5906)`
- 前端源码目录：`web/new-api`
- 后端：`apps/control-api` + `crates/control-plane` + `crates/domain`

本次是完成度/兼容性 review，不是实现清单。目标是判断：当前 control-api 是否已经能支撑从 `ref/new-api/web` 复制过来的完整前端。

## 结论

- control-api 作为 gateway 控制面的最小闭环，已经有可用基础。
- control-api 还不能替换完整 new-api backend。
- web 前端本身可构建、可被 control-api 静态托管；但完整使用会大量 404。
- 基础后台管理可用度粗估约 50%-60%。
- 支付/订阅/OAuth/deployment/异步任务等产品能力仍是明显缺口。

## 验证结果

已通过：

- `cargo check -p halolake-control-api`
- `cargo test -p halolake-control-api`
- `cargo test --workspace`
- `web/new-api/default`：`bun run build`、`bun run typecheck`
- `web/new-api/classic`：`bun run build`
  - classic 没有 `typecheck` script，只能验证 build

本轮 frontend 更新：

- `git -C ref/new-api pull --ff-only` 已更新到 `f2c7cd33`
- `rsync -a --delete` 已把 `ref/new-api/web/` 同步到 `web/new-api/`
- 保留本地 `README.md`、`node_modules`、`dist`
- `web/new-api/default` 已重新构建，生成 `index.f775942f47.js`
- `web/new-api/classic` 已重新构建，生成 `index.40d0d1715e.js`
- classic 构建需要本地兼容补丁：`classic/rsbuild.config.ts` 将 `date-fns` alias 到 Semi 自带的 `date-fns@2.30.0`，否则 `date-fns-tz@1.3.8` 会解析到 workspace 根部 `date-fns@4.4.0` 并触发 exports 错误

运行态 smoke：

- 启动 `examples/control-api.toml`
- `/healthz` 返回 200
- `/api/status` 返回 200 且 `success=true`
- `/` 返回 SPA `index.html`
- 静态 JS chunk 可返回
- `POST /api/user/login` 使用 `root` / `halolake-root-dev` 登录成功
- `GET /api/user/self` 返回 root 用户信息
- `/api/deployments/settings` 返回 404
- `PUT /api/models/?status_only=true` 在前端真实 payload 下返回 422

当前运行进程：

- `halolake-control-api --config examples/control-api.toml`
  - `127.0.0.1:9090`
- `halolake-gateway-monoio --config examples/gateway-control.toml`
  - `127.0.0.1:8082`

gateway 联调：

- `GET http://127.0.0.1:8082/v1/models` 返回 `deepseek-v4-pro`
- `GET /internal/gateway/snapshot` 使用 `x-halolake-internal-key: dev-internal-secret` 返回 snapshot version 1
- `openai-main` 已改为 `api_key_env = "OPENAI_API_KEY"`，control-api 启动时从 `.env` 解析真实 upstream key
- `POST /v1/chat/completions` 不带 token 返回 401 `missing gateway token`
- `POST /v1/chat/completions` 带错误模型返回 403 `model is not allowed for this token`
- `POST /v1/chat/completions` 带 `Authorization: Bearer dev-token` 已实际转发到上游，返回 200，内容为 `pong`
- `POST /v1/chat/completions` 带 `x-api-key: dev-token` 已实际转发到上游，返回 200，内容为 `pong`
- control-api `/api/log` 能看到 gateway 回写的成功请求日志：
  - `type = 2`
  - `content = "consume quota"`
  - `channel = "openai-main"`
  - `prompt_tokens = 9`
  - `completion_tokens = 3`
  - `quota = 12`
- 观察到一次 `POST /v1/completions` 经 gateway 20s 无响应超时；直接打上游同 endpoint 快速返回 401，后续需要单独排查 raw completions 路径或 monoio upstream 连接复用状态

## Findings

### P0: 前端调用面远大于 control-api 已注册路由

control-api 当前注册的 `/api` 路由集中在 `apps/control-api/src/lib.rs` 的 `router()`。
对 frontend 源码做静态路径扫描后：

- backend `/api` route 约 142 条
- frontend 可识别 `/api/...` 字面路径约 173 条
- 没有对应 backend route 的路径约 66 条

缺口不是零散边角，而是整块模块：

- deployment：`/api/deployments/*`
- subscription：`/api/subscription/*`
- 在线支付：`/api/user/amount`、`/api/user/pay`、`/api/user/stripe/*`、`/api/user/creem/*`、`/api/user/waffo*`
- OAuth / custom OAuth：`/api/oauth/*`、`/api/custom-oauth-provider/*`
- 邮箱验证 / 重置密码：`/api/verification`、`/api/reset_password`
- performance：`/api/performance/*`
- MJ / task logs：`/api/mj*`、`/api/task*`
- prefill group：`/api/prefill_group`
- authz catalog：`/api/authz/catalog`
- uptime：`/api/uptime/status`

影响：

- 首页/setup/login/self/token/channel/log/data 等核心后台可部分打开
- 支付、订阅、deployment、OAuth、异步任务相关页面会直接失败

### P1: model 启停接口有 route，但前端真实请求会 422

前端只发送：

```json
{ "id": 1, "status": 0 }
```

到：

```http
PUT /api/models/?status_only=true
```

相关位置：

- `web/new-api/default/src/features/models/api.ts`
- `web/new-api/classic/src/hooks/models/useModelsData.jsx`

后端 handler 先按完整 `ModelRecord` 反序列化：

- `apps/control-api/src/lib.rs`：`update_model_meta`
- `apps/control-api/src/catalog.rs`：`ModelRecord.model_name` 为必填字段

实测返回：

```text
HTTP/1.1 422 Unprocessable Entity
Failed to deserialize the JSON body into the target type: missing field `model_name`
```

这会直接破坏模型列表里的 enable/disable 操作。
注意：service 层已经支持 `status_only`，问题在 HTTP adapter 的反序列化形状。

### P1: 还不是可替换 new-api 的持久化/运维后端

当前能力：

- 默认 memory store
- SQLite 可用
- 配置层可识别 new-api 风格 `SQL_DSN` / `LOG_SQL_DSN` / `SQLITE_PATH`

仍缺：

- Postgres/MySQL 实际 store
- 完整 new-api schema 迁移
- session 持久化 / 过期清理
- session secret 签名或等价安全策略

session 当前是进程内 `MemorySessionStore`，cookie 是裸 UUID：

```text
session=<uuid>; Path=/; Max-Age=2592000; HttpOnly; SameSite=Strict
```

本地开发可用，生产替换 new-api 不够。

### P2: web 已更新到最新 ref，但 classic 构建存在依赖兼容补丁

本次已重新同步到 `ref/new-api` 的 `f2c7cd33`：

```sh
rsync -a --delete \
  --exclude 'README.md' \
  --exclude 'node_modules' \
  --exclude 'dist' \
  --exclude '.DS_Store' \
  ref/new-api/web/ web/new-api/
```

同步后 `web/new-api` 源码与 `ref/new-api/web` 对齐，保留了本地：

- `web/new-api/README.md`
- `node_modules`
- `dist`

为了让 classic 在当前 workspace 依赖树下可构建，`web/new-api/classic/rsbuild.config.ts` 额外加了：

- `date-fns` alias 到 `@douyinfe/semi-ui/node_modules/date-fns`

原因：

- classic 依赖 Semi UI，Semi 使用 `date-fns-tz@1.3.8`
- workspace 根部同时有 default 前端依赖 `date-fns@4.4.0`
- `date-fns-tz@1.3.8` 引用 `date-fns/format/index.js` 等旧 subpath
- `date-fns@4` 的 package exports 不再暴露这些 subpath

如果希望 `web/new-api` 与 upstream 完全零差异，需要改为在依赖层固定 classic 的 `date-fns@2.x` 解析，或者等 upstream 处理 classic/default workspace 的依赖冲突。

### P2: `[web].theme = "classic"` 时 `/api/status` 可能仍返回 default

静态文件选择：

- `selected_web_dist()` 会用 option `theme.frontend`，否则 fallback 到 `[web].theme`

`/api/status`：

- `theme` 字段只用 option，硬编码 fallback `"default"`

如果只在 TOML 配：

```toml
[web]
theme = "classic"
```

则可能出现：

- 静态资源发 classic
- `/api/status.theme` 仍告诉前端 default

## 完成度分层

### 已经比较可用

- setup / status
- password login / logout / session / self
- 基础 2FA / passkey 路径
- token 管理
- channel CRUD / tag / ops / multi-key / codex / ollama 兼容接口
- usage ingestion + settlement 基础路径
- log / data 查询
- options
- model/vendor catalog
- redemption / topup 兑换码路径
- system-task / system-info 基础能力
- pricing / rankings / perf-metrics 只读

### 明显未完成

- OAuth / custom OAuth / email verification / password reset
- 在线支付拉单与 webhook
- subscription 管理与支付
- deployment 管理
- MJ / task 异步日志与后台
- performance 管理接口
- prefill group
- authz catalog
- Postgres/MySQL
- 生产级 session / 审计 / 细粒度权限

### 前端本身

- default / classic 两套主题源码可构建
- control-api 支持按主题托管静态资源与 SPA fallback
- frontend 不是阻塞点；阻塞点在 control-api 兼容面

## 建议优先级

1. 先修已注册但前端不可用的兼容 bug
   - 尤其是 `PUT /api/models/?status_only=true`
2. 按 frontend 高频页面补齐最小 API 集合
   - login/self/token/channel/log/data/options/catalog/redemption
3. 明确“当前目标是否包含支付/订阅/deployment/OAuth”
   - 若不包含，建议前端临时隐藏对应入口
4. 再做生产化
   - session 安全与持久化
   - Postgres/MySQL
   - 审计与细粒度 authz

## 参考命令

```sh
# 同步最新 frontend 源码
git -C ref/new-api pull --ff-only
rsync -a --delete \
  --exclude 'README.md' \
  --exclude 'node_modules' \
  --exclude 'dist' \
  --exclude '.DS_Store' \
  ref/new-api/web/ web/new-api/

# 构建 frontend
cd web/new-api
bun install
(cd default && VITE_REACT_APP_VERSION=halolake-dev bun run build)
(cd classic && VITE_REACT_APP_VERSION=halolake-dev bun run build)

# 启动 control-api
set -a; . ./.env; set +a
cargo run -p halolake-control-api -- --config examples/control-api.toml

# 启动 gateway
set -a; . ./.env; set +a
cargo run -p halolake-gateway-monoio -- --config examples/gateway-control.toml
```

示例服务地址：

- `http://127.0.0.1:9090/`
- `http://127.0.0.1:8082/v1/models`
- 默认 bootstrap root：`root` / `halolake-root-dev`
