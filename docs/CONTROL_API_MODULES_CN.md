# control-api 模块分层（core / admin-extras / compat-stubs）

目标：在不拆部署进程的前提下，把 `apps/control-api` 的职责边界标清楚，便于：

- 判断「对齐 new-api」时哪些必须做、哪些故意不做
- 后续抽 crate / 关 feature 减体积
- 新人读代码时有地图

对应 Cargo features（`apps/control-api/Cargo.toml`）：

```toml
default = ["core", "admin-extras", "compat-stubs"]
core = []
admin-extras = []
compat-stubs = []
```

当前 **默认全开**，行为与改前一致。  
`compat-stubs` 已在路由层 `cfg` 控制：关闭后不挂载 `compat.rs` 里的支付/OAuth/订阅/部署等壳路由。

---

## 1. `core` — 代理管理闭环（必须）

面向「用 new-api 前端管 token/渠道 + gateway 转发」的最小完整集。

| 区域 | 路径 / 模块 | 说明 |
|------|-------------|------|
| 健康与站点 | `/healthz`, `/api/setup`, `/api/status` | 启动与前端 bootstrap |
| 认证 | `/api/user/login|logout|register`, 2FA, passkey, session | 本地账号体系 |
| 用户 / 令牌 | `/api/user/*`, `/api/token/*` | 管理面主体 |
| 渠道 | `/api/channel/*`（CRUD/search/tag/ops/test 等） | 与 gateway snapshot 直连 |
| 配置 | `/api/option/*`, `/api/group` | 运行参数与分组 |
| 用量日志 | `/api/log/*`, `/api/data/*`, `/api/usage/*` | gateway 回写查询 |
| 兑换码充值 | `/api/redemption/*`, `/api/user/topup*`（兑换路径） | **不含**第三方在线支付 |
| 系统任务 / 实例 | `/api/system-task/*`, `/api/system-info/*` | 运维基础 |
| Gateway 内部 | `/internal/gateway/snapshot|usage|channel-feedback` | 数据面契约 |
| 静态 Web | SPA fallback + 内嵌 dist | 前端托管 |
| 存储 | `storage::{Management,Option,Usage}Store` | memory/sqlite/mysql/pg |
| 安全 | `security`, `session`, `http_auth` | 会话与 2FA/passkey |

**对齐优先级：高。** 渠道 list 过滤/排序/tag_mode、用户令牌语义等应继续按 `ref/new-api` 补齐。

---

## 2. `admin-extras` — 可选管理增强（建议保留）

不是支付类，但是「全家桶后台」周边能力；关掉后前端部分菜单会空或失败。

| 区域 | 路径 / 模块 | 说明 |
|------|-------------|------|
| 模型目录 | `/api/models/*`, `/api/vendors/*` | 元数据 CRUD + upstream sync |
| 倍率同步 | `/api/ratio_sync/*`, `ratio_sync` | 上游 ratio 拉取 |
| 代理池 | `/api/proxy/*`, `proxy`, `proxy_probe` | HTTP 代理节点 |
| 渠道导入 | `/api/channel/import/*`, `auth_import`, `codex_auth_import`, `sub2api_data_import` | 批量凭证导入 |
| 渠道专用 | codex/ollama/upstream_updates、`channel_special` | 厂商专用操作 |
| 签到 | `/api/user/checkin` | 可选运营 |
| Prefill | `/api/prefill_group/*` | 预填分组 |
| Playground | `playground` | 调试台 |
| 亲和缓存 | `/api/option/channel_affinity_cache` | 渠道亲和 |
| Pricing 只读 | `/api/pricing`, `/api/rankings`, `/api/perf-metrics*` | 展示向 |

**对齐优先级：中。** 与网关正确性弱相关，但运营常用。

> 现状：`admin-extras` 已通过 `admin_extras::mount` + `#[cfg(feature = "admin-extras")]` 挂载；  
> `playground` 同属该 feature。以下 `mod` 也已 feature 门控（core-only 不编译）：  
> `auth_import` / `codex_auth_import` / `sub2api_data_import` / `ratio_sync` /  
> `model_sync` / `proxy_probe` / `channel_special` / `playground`。  
> 对应 HTTP handlers 同样 `cfg`。`ProxyStore` / `CheckinStore` / `CatalogStore` 仍常驻 AppState（snapshot/import 共用或后续可再收）。

---

## 3. `compat-stubs` — 兼容壳（故意不做真业务）

仅保证从 `ref/new-api/web` 拷来的前端 **不 404**。  
产品决策：**支付 / OAuth / 订阅 / 部署 不做。**

| 区域 | 主要路径 | 行为 |
|------|----------|------|
| OAuth | `/api/oauth/*`, custom-oauth-provider, user oauth bindings | not configured / 空列表 |
| 支付 webhook & pay | stripe/creem/waffo/epay、`/api/user/*/pay` | not configured 或 webhook_ok |
| 订阅 | `/api/subscription/*` | 空列表 / not configured |
| 部署 | `/api/deployments/*` | 空列表 / not configured |
| 邮箱验证 / 重置 | `/api/verification`, `/api/reset_password` | stub |
| MJ / task 日志 | `/api/mj/*`, `/api/task/*` | 空列表 |
| 站点文案 | notice / about / privacy / home_page_content | 空字符串 |
| Performance 管理 | `/api/performance/*` | 兼容占位 |
| authz catalog | `/api/authz/catalog` | 静态权限表（非强制鉴权） |

实现集中在 `apps/control-api/src/compat.rs`，由 `compat::mount` 挂载。

关闭示例：

```bash
cargo build -p halolake-control-api --no-default-features --features core,admin-extras
```

---

## 4. 源码地图（目录）

```text
apps/control-api/src/
  lib.rs              # AppState、router 组装、feature 挂载点
  admin_extras.rs     # admin-extras 路由 mount（cfg）
  storage/mod.rs      # re-export → halolake-control-storage
  api_*.rs            # HTTP handlers
  compat.rs           # compat-stubs
  playground.rs       # admin-extras
  security.rs / session.rs / http_auth.rs
  channel_*.rs / ratio_sync.rs / proxy*.rs / *import*.rs  # 多属 admin-extras

crates/control-storage/   # ManagementStore / OptionStore / UsageStore
  src/management.rs
  src/options.rs
  src/usage.rs
```

### storage 说明

| 位置 | 内容 |
|------|------|
| `crates/control-storage` | `ManagementStore` + `OptionStore` + `UsageStore`（memory/sqlite/mysql/pg） |
| `apps/control-api/src/storage` | 薄 re-export，API 路径不变 `crate::storage::*` |

**无行为变更。** 其它 store（proxy/checkin/catalog/…）仍在 control-api 内。

---

## 5. 建议演进顺序

1. ~~文档 + features 声明 + compat cfg~~
2. ~~`storage/` 多文件~~
3. ~~admin-extras 路由 `cfg` 化（`admin_extras::mount`）~~
4. ~~core 渠道 list/search 对齐 sort_by / sort_order / id_sort / tag_mode~~
5. ~~admin-extras 相关 `mod` / handler `cfg`（import、ratio_sync、model_sync、channel_special 等）~~
6. ~~抽出 `halolake-control-storage` crate~~

---

## 6. 与「拆 control-api-ext crate」的关系

| 层 | 现在 | 以后 |
|----|------|------|
| feature | core / admin-extras / compat-stubs | 可原样映射 |
| 目录 | `crates/control-storage`、`compat.rs` | compat / admin → 可选 `control-api-ext` |
| binary | 仍一个 `halolake-control-api` | 仍建议单 binary + feature |

不要为了拆而拆：先用 feature/目录稳住边界即可。
