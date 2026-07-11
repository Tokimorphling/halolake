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
- control-api 还不能替换完整 new-api backend 的**业务能力**（支付网关、OAuth 真登录、io.net deployment、SMTP 等外部集成仍为 stub）。
- 前端调用面已补齐：原先约 66 条 404 的 `/api/*` 已注册，页面不再因缺路由直接失败。
- 注意：大量新增路由是兼容 stub（空列表 / “not configured”），不等于业务能力完成。
- web 前端本身可构建、可被 control-api 静态托管。
- web dist 现在可以打包进 `control-api` 二进制；运行时优先读磁盘 dist，缺失时回退到内置资源。
- 基础后台管理**路由覆盖**粗估约 75%-85%；真实产品能力（在线支付/OAuth/deployment）仍需外部配置后实现。
- 支付/订阅/OAuth/deployment/异步任务等**外部集成**仍是明显缺口（有兼容响应，无第三方联调；支付 webhook 默认拒绝）。

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
- 从非仓库工作目录启动时，磁盘 dist 相对路径不可用，`/` 仍可通过内置 web 资源返回 SPA `index.html`
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

### 已修复: model/token 启停 `status_only`

前端发送：

```json
{ "id": 1, "status": 0 }
```

到：

```http
PUT /api/models/?status_only=true
PUT /api/token/?status_only=true
```

当前：

- `ModelRecord.model_name` 带 `#[serde(default)]`，`{id,status}` 可反序列化
- token update 识别 `status_only`，不会用默认值覆盖 `remain_quota`/`expired_time`
- 单元测试覆盖 status_only payload / query

### P1: 还不是可替换 new-api 的持久化/运维后端

当前能力：

- 默认 memory store
- SQLite 可用
- 配置层可识别 new-api 风格 `SQL_DSN` / `LOG_SQL_DSN` / `SQLITE_PATH`

已补：

- Postgres 主库 Stage 1：management / options / usage / prefill
- prefill_group 持久化（不再是进程内 OnceLock）

仍缺：

- Postgres 上 catalog/billing/security/checkin/system_* 仍回退内存
- MySQL 实际 store
- 完整 new-api schema 迁移 / abilities 表
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

### 已修复: `[web].theme = "classic"` 时 `/api/status` 可能仍返回 default

静态文件选择：

- `selected_web_dist()` 会用 option `theme.frontend`，否则 fallback 到 `[web].theme`

`/api/status.theme` 已改为同样 fallback 到 `[web].theme`，避免只在 TOML 配 classic 时前后端主题不一致。

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
- authz catalog（仅静态兼容返回）
- Postgres 其余 store / MySQL
- 生产级 session / 审计 / 细粒度权限
- 在线支付 / OAuth / subscription / deployment（仍为 stub）

### 前端本身

- default / classic 两套主题源码可构建
- control-api 支持按主题托管静态资源、SPA fallback，以及编译期内置 web dist fallback
- frontend 不是阻塞点；阻塞点在 control-api 兼容面

## 建议优先级

1. ~~`status_only` 422~~ 已修
2. 补齐 Postgres 剩余 store（catalog/billing/security/checkin/system_*）
3. 明确“当前目标是否包含支付/订阅/deployment/OAuth”
   - 若不包含，建议前端临时隐藏对应入口
4. 再做生产化
   - session 安全与持久化
   - MySQL / 行级 SQL（替代全量 dump）
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
