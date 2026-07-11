# new-api Backend Rust 移植清单

目标是让 Halolake control-api 逐步替换 new-api backend，并复用 new-api
frontend。实现方式保持双平面架构：

- gateway-monoio 负责 relay 热路径。
- control-api 负责后台 API、DB、快照发布、计费、任务。
- 两者通过 `SnapshotSource`、`SnapshotPublisher`、`UsageEventSink` 等 service
  trait 解耦。

## 已完成

- workspace 拆分为 gateway、control-api、domain、control-plane 等 crate。
- gateway 支持本地 TOML snapshot。
- control-api 支持 internal snapshot API。
- gateway 支持启动时从 control-api HTTP snapshot source 拉取初始 snapshot，并通过
  `control.snapshot_poll_interval_ms` 做版本化 polling 热更新。
- control-plane 定义 `SnapshotSource`、`SnapshotPublisher`、`UsageEventSink`、
  `ChannelFeedbackSink`。
- control-plane 提供 `StaticSnapshotSource`、`MemorySnapshotBus`、
  `MemoryUsageEventSink`、`NoopUsageEventSink`。
- control-api 提供基础 `/healthz`、`/internal/gateway/snapshot`。
- control-api 提供最小 new-api 风格 public endpoints：
  - `GET /api/setup`
  - `POST /api/setup`
  - `GET /api/status`
  - `GET /api/notice`
  - `GET /api/user-agreement`
  - `GET /api/privacy-policy`
  - `GET /api/about`
  - `GET /api/home_page_content`
- control-api 的 setup bootstrap 已按 new-api 语义创建单个 root 用户，并写入
  `SelfUseModeEnabled`、`DemoSiteEnabled` 初始化选项；重复初始化会返回兼容错误。
- control-api 支持 `POST /api/user/register` password register 路径：
  - 读取 `RegisterEnabled`、`PasswordRegisterEnabled`、`EmailVerificationEnabled`
    开关。
  - 创建普通启用用户，密码使用 bcrypt。
  - 兼容 new-api 的 `GENERATE_DEFAULT_TOKEN` 语义，可通过
    `GenerateDefaultToken` option 或环境变量开启默认 token 创建。
  - `DefaultUseAutoGroup` 开启时默认 token 使用 `auto` group。
- new-api web 前端已从 `ref/new-api/web` 提取到 `web/new-api`：
  - 保留 default/classic 两套主题源码和 workspace lockfile。
  - control-api 支持 `[web]` 静态目录配置，能按 `theme.frontend` 选择
    default/classic 构建产物。
  - 未命中静态文件的普通 web 路径返回 `index.html`，保持 SPA fallback；
    `/api`、`/internal`、`/v1` 仍保留给后端/API 路由。
- control-api 提供 `GET /api/models`，当前从 snapshot 返回模型列表，后续接用户
  权限和分组过滤。
- control-api 提供内存版用户/session API 骨架：
  - `POST /api/user/login`
  - `GET /api/user/logout`
  - `GET /api/user/self`
  - `PUT /api/user/self`
  - `DELETE /api/user/self`
  - `GET /api/user/groups`
  - `GET /api/user/self/groups`
  - `GET /api/user/models`
  - `GET /api/user/`
  - `GET /api/user/search`
  - `GET /api/user/:id`
  - `POST /api/user/`
  - `PUT /api/user/`
  - `DELETE /api/user/:id`
  - `POST /api/user/manage`
- control-api 配置支持 `[[users]]` bootstrap，登录后设置 new-api 兼容的
  `session` cookie，并校验 `New-Api-User` header。
- control-plane 用户密码使用 bcrypt，与 new-api `Password2Hash`/`ValidatePasswordAndHash`
  路径保持格式兼容；配置中的明文 bootstrap 密码会在 control-api 启动时自动 hash，
  新建/更新用户也会 hash 后再进入管理数据。
- control-api `GET /api/user/token` 会生成并保存用户 access token；
  后台 API 鉴权支持 cookie session，也支持 `Authorization` access token。
- control-api 支持 management storage backend：
  - 默认 `memory`
  - 可配置 `sqlite` / `postgres`
  - SQLite 启动时创建 users/tokens/channels/model_mappings/control_meta 表
  - Postgres 同表结构（BIGINT/`$n` 占位符），management/options/usage/prefill 可落库
  - 变更先走同一套 `service_async::Service` 管理逻辑，再持久化当前管理数据
  - `examples/control-api-sqlite.toml`、`examples/control-api-postgres.toml` 提供示例
- control-api 配置层已识别 new-api 的数据库 DSN 语义：
  - `SQLITE_PATH` 会映射为 SQLite URL。
  - `SQL_DSN=local...` 推断为 SQLite。
  - `SQL_DSN=postgres://...`/`postgresql://...` 推断为 PostgreSQL。
  - 其他非空 `SQL_DSN` 推断为 MySQL。
  - `LOG_SQL_DSN` 可识别 MySQL/PostgreSQL/SQLite/ClickHouse 日志库类型。
  - 当前除 memory/SQLite 外会明确报“recognized but not implemented yet”，避免静默
    退回错误 backend。
- control-api 支持 usage/log/data storage backend：
  - 默认 `memory`
  - SQLite 模式复用同一个 `sqlite_url`
  - SQLite 启动时创建 `usage_events` 表并加载到查询投影
  - internal usage ingestion 写入 SQLite 后再更新内存投影
  - `request_id` 作为主键用于幂等去重
- control-api 的 token 管理路由已挂用户 session，按当前用户 id 隔离 token。
- control-api 的 channel 管理路由已挂 admin session 权限，普通用户不可访问。
- control-api 提供 internal usage ingestion：
  - `POST /internal/gateway/usage`
  - body 为 `UsageEventBatch`
  - 使用 `x-halolake-internal-key` 保护
  - 写入 usage event 后，对新接受的 success events 执行 usage settlement：
    按 new-api 的基础文本计费公式计算 quota，累加 user/token/channel
    `used_quota`，并更新 token `accessed_time`
    - fixed price: `ModelPrice * QuotaPerUnit * GroupRatio`
    - ratio price: `(prompt_tokens + completion_tokens * CompletionRatio) *
      ModelRatio * GroupRatio`
    - cache price: control-plane 会按 `CacheRatio` 和 `CreateCacheRatio` 把
      cache read/cache creation tokens 从 base prompt 中拆出后分别计费，再乘
      `ModelRatio * GroupRatio`。
    - image/audio token ratio: 若 upstream usage details 带 `image_tokens` 或
      `audio_tokens`，control-plane 会按 `ImageRatio`/`AudioRatio` 从 base
      prompt 中拆出后分别计费。
    - `GroupGroupRatio[user_group][using_group]` 会覆盖普通 `GroupRatio`
    - snapshot 已发布 token user group、原始 token group、effective using group、
      channel groups 和 group routing options；gateway/router 会校验
      `UserUsableGroups`，普通 group 只在匹配 channel 内选路，`auto` group 会按
      `AutoGroups` 顺序在用户可用组内选择首个有候选的 group。
    - gateway 会把本次 route 的实际 group 带回 usage event，control-plane
      结算优先使用该 group；旧事件没有 group 时继续按 token/channel/user 反推。
    - cross-group retry 仍待补齐，需要先完成 gateway relay retry loop。
    - settlement 后会把实际 quota 回写到 usage event，log/stat/data 聚合优先使用
      该 quota；旧事件没有 quota 时继续用 token 数兜底。
  - memory/SQLite usage store 均按 `request_id` 去重，重复上报不会重复扣费
- control-api 提供 log 查询投影：
  - `GET /api/log/`
  - `GET /api/log/search`，按 new-api 返回 deprecated
  - `GET /api/log/self`
  - `GET /api/log/self/search`，按 new-api 返回 deprecated
  - `GET /api/log/stat`
  - `GET /api/log/self/stat`
  - log record 已填充 quota、group、is_stream、ip、request_id、
    upstream_request_id 字段；其中 group 优先使用 gateway usage event 实际 group，
    旧事件再按 token/channel/user 管理数据推导，upstream_request_id 来自常见
    upstream 响应头。
  - log `other` 字段会在存在 usage details 时写入紧凑 JSON，包括
    total/cache/image/audio token details。
  - log list 支持按时间、类型、模型、用户名、token 名称、channel、group、
    request_id、upstream_request_id 过滤；用户名/token/channel 会同时匹配
    numeric id、snapshot id 和管理面名称。
- management store 从配置 snapshot 导入时保留原始 token/channel snapshot id，
  管理面继续对前端暴露数字 id，发布给 gateway 的 snapshot 不会把
  `openai-main` 等路由 id 重建成 `1`。
- control-plane 已把 new-api 常见 OpenAI-compatible channel type
  （OpenRouter、DeepSeek、Moonshot、SiliconFlow、Codex 等）映射为 gateway
  `OpenAi` provider；gateway 暂不支持的异步/视频类 channel 会在 snapshot 中跳过，
  避免一个非 Stage1 渠道阻断控制面发布。
- gateway 支持可选 `control.usage_url`，relay 返回后 fire-and-forget 上报
  `UsageEventBatch` 到 control-api；当前上报 request/channel/token/model、
  HTTP 状态、延迟、stream 标记、peer IP、upstream request id，以及响应中的
  usage token。
  - OpenAI `usage.prompt_tokens/completion_tokens/total_tokens`
  - OpenAI Responses 风格 `usage.input_tokens/output_tokens`
  - Claude `usage.input_tokens/output_tokens/cache_creation_input_tokens/cache_read_input_tokens`
  - Gemini `usageMetadata.*TokenCount`
  - OpenAI `prompt_tokens_details.cached_tokens/cached_creation_tokens`、
    Responses `input_tokens_details.cached_tokens/cached_creation_tokens`、
    Claude cache read/create 和 Gemini `cachedContentTokenCount` 会作为 cache
    usage 上报给 control-plane。
  - OpenAI/Responses `image_tokens/audio_tokens` 与 Gemini
    `promptTokensDetails[].modality` 的 image/audio token 会作为 usage details
    上报。
  - 非流式 JSON 响应在 relay 返回前提取 usage。
  - 流式 SSE 响应通过 response body wrapper 读取下游 `data:` payload，流结束或
    body drop 时再上报一次，避免先上报空 token 后被幂等去重。
- gateway 支持可选 `control.channel_feedback_url`，在真实 upstream 非 2xx/transport
  error 时 fire-and-forget 上报 `ChannelFeedbackBatch` 到 control-api。
  control-api 按 new-api 风格的 `AutomaticDisableChannelEnabled`、
  `AutomaticDisableStatusCodes`、`AutomaticDisableKeywords`、channel `auto_ban`
  判断是否自动禁用；多 key channel 会按 snapshot 保留的原始 key index 写入
  `multi_key_status_list = 3`，所有 key 都不可用时再把 channel 置为 auto-disabled
  并发布新 snapshot。
- gateway router-core 对同一 requested model 保留多个 channel mapping，不再被
  `HashMap` 覆盖；relay 前使用无锁 seed 按 channel `weight` 选择候选 channel，
  再在 channel 的 `api_keys` 内 round-robin 选择 key；snapshot 中同步保留
  `api_key_indexes`，使 gateway 不暴露完整 key 也能反馈失败 key 的原始索引。
- control-api 提供 data usage 聚合：
  - `GET /api/data/`
  - `GET /api/data/users`
  - `GET /api/data/self`
  - `GET /api/data/flow`
  - `GET /api/data/flow/self`
- control-api 提供 root-only options 兼容接口：
  - `GET /api/option/`
  - `PUT /api/option/`
  - `POST /api/option/rest_model_ratio`
  - `GET/DELETE /api/option/channel_affinity_cache`
  - 内存和 SQLite storage 均可用，SQLite 模式使用同库 `options` 表持久化
  - 返回时按 new-api 规则过滤 `*Token`、`*Secret`、`*Key`、`*secret`、
    `*api_key` 等敏感项，并补 `CompletionRatioMeta`
  - `rest_model_ratio` 目前将 `ModelRatio` 恢复到 Halolake 内置默认值；new-api
    的完整默认倍率表会随 pricing/ratio cache 一起迁移。
  - channel affinity 默认配置已加入 `channel_affinity_setting.*` 和
    `monitor_setting.*` options，默认规则包含 new-api 的 codex/claude CLI trace
    规则；control-api 会随管理快照发布亲和配置，gateway 热路径按 model/path/
    user-agent/header/body 解析亲和 key，并在成功 relay 后记录到本地内存缓存。
- control-api 提供 admin-only `GET /api/group/`，从 options `GroupRatio` 和
  当前用户/token/channel 管理数据合并 group 名称。
- control-api 提供 root-only `POST /api/channel/:id/key`，通过
  `RevealChannelKeyRequest` service 返回渠道 key；step-up 2FA 已有基础能力，
  细粒度审计待后续补。
- control-api 提供 token read-only `GET /api/log/token`，兼容 new-api
  `Bearer sk-...` token 解析，并同时按 token 数字 id/snapshot id 匹配 gateway
  usage event。
- control-api 提供 admin-only `GET /api/log/channel_affinity_usage_cache`，返回
  new-api 兼容的 usage cache stats 结构；当前 gateway 亲和缓存是实例本地内存，
  还没有接入 Redis/跨实例共享统计，因此控制面 usage cache 统计仍只表达控制面
  本地视角。
- control-api 提供 checkin 兼容接口：
  - `GET /api/user/checkin`
  - `POST /api/user/checkin`
  - 配置项 `CheckinEnabled`、`CheckinMinQuota`、`CheckinMaxQuota`，默认与 new-api
    一样关闭，额度范围默认 1000-10000。
  - memory/SQLite store 均按 `(user_id, checkin_date)` 防重复；签到成功后通过
    management service 增加用户 quota。
- control-api 提供 legacy `DELETE /api/log/`，按 `target_timestamp` 清理
  memory/SQLite usage events 并返回删除数量。
- control-api 提供 root-only system task 兼容接口：
  - `POST /api/system-task/log-cleanup`
  - `GET /api/system-task/list`
  - `GET /api/system-task/current`
  - `GET /api/system-task/:task_id`
  - task response 的 `payload/state/result` 按 new-api 返回 JSON 对象；当前 runner
    先覆盖手动 `log_cleanup`、`channel_test`、`model_update`，创建后异步执行并更新
    pending/running/succeeded/failed 状态。
  - control-api 可通过 `[system] task_scheduler_enabled = true` 启动单节点 scheduled
    runner，按 `channel_test_interval_seconds` 和 `model_update_interval_seconds` 定时创建
    `channel_test` / `model_update` task；默认关闭，避免开发环境误打真实上游。
  - system task store 支持 memory 和 SQLite；SQLite 模式使用同库 `system_tasks`
    表持久化。
- control-api 提供 root-only system-info 实例接口：
  - `GET /api/system-info/instances`
  - `DELETE /api/system-info/stale-instances`
  - `DELETE /api/system-info/instances/:node_name`
  - 启动后会定时 upsert 当前 control-api 实例，返回 new-api 兼容的
    `node/status/stale_after_seconds/started_at/last_seen_at/info` 结构。
  - system instance store 支持 memory 和 SQLite；SQLite 模式使用同库
    `system_instances` 表持久化。
- control-api 提供一组 channel tag/ops 管理接口：
  - `GET /api/channel/ops`
  - `GET /api/channel/test`
  - `GET /api/channel/test/:id`
  - `GET /api/channel/update_balance`
  - `GET /api/channel/update_balance/:id`
  - `POST /api/channel/fix`
  - `GET /api/channel/fetch_models/:id`
  - `POST /api/channel/fetch_models`
  - `DELETE /api/channel/disabled`
  - `POST /api/channel/tag/disabled`
  - `POST /api/channel/tag/enabled`
  - `PUT /api/channel/tag`
  - `GET /api/channel/tag/models`
  - `POST /api/channel/batch/tag`
  - `POST /api/channel/copy/:id`
  - `POST /api/channel/multi_key/manage`
  - `POST /api/channel/:id/codex/refresh`
  - `GET /api/channel/:id/codex/usage`
  - `GET /api/channel/:id/codex/usage/reset-credits`
  - `POST /api/channel/:id/codex/usage/reset`
  - `POST /api/channel/ollama/pull`
  - `POST /api/channel/ollama/pull/stream`
  - `DELETE /api/channel/ollama/delete`
  - `GET /api/channel/ollama/version/:id`
  - `POST /api/channel/upstream_updates/detect`
  - `POST /api/channel/upstream_updates/detect_all`
  - `POST /api/channel/upstream_updates/apply`
  - `POST /api/channel/upstream_updates/apply_all`
  - 写接口通过 `service_async::Service` 管理请求修改 management store，变更后发布
    gateway snapshot。
  - `fetch_models` 支持 OpenAI-compatible `/v1/models`、Ollama `/api/tags`、
    Gemini `/v1beta/models`、Ali `/compatible-mode/v1/models`、Zhipu v4
    `/api/paas/v4/models`。
  - 余额更新按 new-api 的已有 provider 分支实现：OpenAI/Custom legacy billing、
    AIProxy、API2GPT、AIGC2D、SiliconFlow、DeepSeek、OpenRouter、Moonshot。
  - channel test 当前是 control-api 的 lightweight upstream probe，会构造 new-api
    相同的基础 chat/responses/anthropic/gemini/embedding/rerank/image 测试请求，
    更新 `response_time/test_time`；`GET /api/channel/test` 已按 new-api 改为创建
    `channel_test` system task，状态进度可通过 `/api/system-task/:task_id` 轮询。
    完整 relay billing/log 语义后续交给 gateway relay service 补齐。
  - upstream model update 已实现 detect/apply diff：支持
    `upstream_model_update_*` setting、ignored models、`regex:` ignore、
    model_mapping alias 规则；`detect_all` 已按 new-api 改为创建 `model_update`
    system task，`apply_all` 当前仍为同步扫描版本。
  - Ollama 专用接口会转发 `/api/version`、`/api/pull`、`/api/delete`；pull stream
    当前返回 SSE 兼容事件序列，但不是逐 chunk 透传上游进度。
  - multi-key manage 支持 new-api 的
    `get_key_status/disable_key/enable_key/enable_all_keys/disable_all_keys/delete_key/
    delete_disabled_keys` action；状态保存在 channel `setting` 的
    `multi_key_*` 字段中。control-plane 发布 snapshot 时会过滤 disabled key 并写入
    `api_keys/api_key_indexes`，gateway relay 前用无锁 round-robin 选择 key；
    上游失败后通过 internal channel feedback 自动禁用单个 key 或整个 channel。
  - Codex 专用接口支持 refresh credential 和 WHAM usage/reset credits 代理；refresh
    使用 OpenAI OAuth refresh_token 交换并回写 channel key，WHAM 接口按 new-api
    header 结构带 `Authorization/chatgpt-account-id/originator` 转发。
- control-api 提供 model/vendor catalog 管理接口：
  - `GET/POST/PUT /api/models/`
  - `GET /api/models/search`
  - `GET /api/models/missing`
  - `GET /api/models/sync_upstream/preview`
  - `POST /api/models/sync_upstream`
  - `GET/DELETE /api/models/:id`
  - `GET/POST/PUT /api/vendors/`
  - `GET /api/vendors/search`
  - `GET/DELETE /api/vendors/:id`
  - catalog store 支持 memory 和 SQLite；SQLite 模式使用 `vendors`/`models` 表。
  - 新实例会从 management snapshot 的 enabled models 和 explicit model mappings
    种子化模型元数据，列表响应会基于当前 channel 管理数据派生 bound channels。
  - sync upstream 按 new-api 默认 URL 读取
    `https://basellm.github.io/llm-metadata/api/newapi/{models,vendors}.json`，
    支持 `SYNC_UPSTREAM_BASE` 和 `locale=en|zh-CN|zh-TW|ja`，preview 返回
    `missing/conflicts/source`，sync 支持 `overwrite[].fields` 选择性覆盖。
- control-api 提供 pricing/ranking/perf 只读接口：
  - `GET /api/pricing`
  - `GET /api/rankings`
  - `GET /api/perf-metrics`
  - `GET /api/perf-metrics/summary`
  - 当前实现从 catalog/options/usage events 即时派生，不额外写 perf_metrics 聚合表。
- control-api 提供 root-only ratio sync 兼容接口：
  - `GET /api/ratio_sync/channels`
  - `POST /api/ratio_sync/fetch`
  - channels 返回当前有 `base_url` 的 channel，并附加 new-api 相同的两个内置
    preset：`官方倍率预设(-100)`、`models.dev 价格预设(-101)`。
  - fetch 支持 new-api 的 `upstreams` 和 `channel_ids` 输入、`/api/pricing`
    列表格式、`ratio_config` map 格式、OpenRouter `/v1/models` 转换、
    models.dev `/api.json` 转换，并按 new-api 的 `differences/test_results`
    结构返回。
- control-api 提供 payment compliance 和基础 billing/redemption/topup 兼容接口：
  - `POST /api/option/payment_compliance`
  - `GET/POST/PUT /api/redemption/`
  - `GET /api/redemption/search`
  - `GET/DELETE /api/redemption/:id`
  - `DELETE /api/redemption/invalid`
  - `GET /api/user/topup/info`
  - `GET /api/user/topup/self`
  - `GET /api/user/topup`
  - `POST /api/user/topup`
  - `POST /api/user/topup/complete`
  - billing store 支持 memory 和 SQLite；SQLite 模式使用 `redemptions`/`topups` 表。
  - 兑换码状态、搜索、创建数量限制、过期检查、重复兑换失败语义按 new-api
    `model/redemption.go` 和 `controller/redemption.go` 实现；兑换失败对用户只暴露
    `redeem.failed`。
  - `/api/user/topup` 目前覆盖 new-api 的兑换码充值路径；在线支付拉单和 webhook
    尚未实现。

## API 响应约定

new-api 后台 API 大多返回：

```json
{
  "success": true,
  "message": "",
  "data": {}
}
```

分页接口通常把分页信息放在 `data` 内：

```json
{
  "success": true,
  "message": "",
  "data": {
    "items": [],
    "total": 0,
    "page": 1,
    "page_size": 10
  }
}
```

Halolake 已在 `crates/api-contract` 中加入 `ApiResponse<T>` 和 `Page<T>`。

## 优先级 P0: 控制面核心

这些直接影响 gateway 可用性和 frontend 基础管理能力。

- 用户与认证：
  - 已有内存版 password login/session/self/user CRUD 骨架。
  - 已有 bcrypt password hash/verify。
  - 已有 access token 生成和鉴权。
  - 已有 SQLite management 持久化。
  - 已有 `GET/POST /api/setup` root bootstrap 和初始化选项写入。
  - 已有 `POST /api/user/register` password register；email verification 暂未实现。
  - 已有 2FA 基础能力：`/api/user/2fa/*`、`/api/verify`、password login
    pending 2FA session，memory/SQLite `two_fas` 和 `two_fa_backup_codes`
    持久化，TOTP SHA1/6 位/30 秒窗口、bcrypt backup code hash。
  - 已有 passkey WebAuthn 基础能力：按 new-api 路径实现
    register/login/verify begin/finish、discoverable login、passkey status/delete/admin
    reset，memory/SQLite `passkey_credentials` 和 `passkey_sessions` 持久化。
  - 待做：Postgres/MySQL、真实 session secret、OAuth、细粒度 authz/audit，以及
    passkey user verification/attachment 等高级策略与 new-api 设置完全对齐。
- token：
  - 已有内存版 `GET/POST/PUT/DELETE /api/token`、search、batch delete、
    key reveal、token usage；管理路由按当前登录用户隔离。
  - 已有 SQLite management 持久化。
  - 已有 token 过期/额度耗尽/user disabled 对 gateway snapshot 的 runtime
    enabled 判断；usage settlement 会在 token quota 耗尽时标记 exhausted，并发布
    新 snapshot 供 gateway polling。
  - 已有 token IP allowlist：control-plane 按 new-api 的 allow_ips 换行格式发布
    snapshot，gateway 在 router-core index 阶段预解析 exact IP/CIDR，热路径按 peer
    IP 拒绝不在列表中的请求。
  - 已有 gateway usage ingestion 对 token `accessed_time` 的基础更新；即使事件不产生
    quota，也会记录 token 最近使用时间。
  - 待做：auth middleware 级别的 access time 精细更新、Postgres/MySQL 和审计。
- channel：
  - 已有内存版 `GET/POST/PUT/DELETE /api/channel`、search、models、
    status/batch、delete batch、copy、fix；路由按 admin 权限保护，变更后发布
    snapshot。
  - 已有 root-only `POST /api/channel/:id/key` 返回渠道密钥。
  - 已有 ops/tag/disabled channel 管理接口，支持 memory 和 SQLite 持久化。
  - 已有 fetch models、余额更新、lightweight channel test、channel_test 手动系统任务、
    upstream_updates detect/apply、model_update 手动系统任务、Ollama 管理、
    multi-key manage、Codex usage/refresh 基础兼容。
  - 已有 SQLite management 持久化。
  - 待做：Codex auto-refresh scheduled task、channel test 的完整 relay/billing/log
    行为、Postgres/MySQL、敏感操作强制 step-up 策略和审计。
- snapshot build：
  - 从 DB users/tokens/channels/model mappings 构建 `GatewaySnapshot`
  - channel CRUD 后发布新 snapshot version
  - token/user 变更后发布新 snapshot version

## 当前限制

- control-api 默认仍是 memory store；配置 SQLite 后 users/tokens/channels/
  model_mappings、usage_events、options、catalog、redemptions/topups、
  prefill_groups 会持久化。
- **Postgres 主库已可用（Stage 1）**：`storage.backend = "postgres"` 或
  `SQL_DSN=postgres://...` 时，management / options / usage_events / prefill
  会落库（memory 投影 + 全量持久化，与 SQLite 同模式）。示例配置见
  `examples/control-api-postgres.toml`。
- catalog / billing / security / checkin / system_task / system_instance 在
  Postgres 模式下仍回退内存（启动时 warn），需后续补齐。
- MySQL 与 ClickHouse 日志库仍识别但未实现。
- 用户密码已按 bcrypt hash 存入管理数据；SQLite/Postgres schema 已有，完整
  new-api 行级 schema/abilities 表尚未迁移。
- token 已挂 UserAuth 语义，channel 已挂 AdminAuth 语义；RootAuth-only
  的敏感接口尚未补完。
- session cookie 兼容 new-api 的 cookie 名 `session`，但尚未做签名/加密。
- 用户权限中的 sidebar/authz 字段先返回兼容形状，细粒度权限模型未落库。
- checkin 日期当前按 UTC 日历日计算；后续增加站点时区配置后再对齐 new-api
  `time.Now()` 的本地日期语义。
- system task runner 当前执行 API 手动创建的 `log_cleanup`、`channel_test`、
  `model_update`；可配置开启单节点 scheduled runner 定时创建 `channel_test` 和
  `model_update`。当前仍使用单进程 claim 语义；new-api 的多节点 lease/heartbeat
  和 stale lock 恢复逻辑后续随 task-poll 一起补齐。
- system-info 当前上报 Halolake control-api 的 node/runtime/host/role 基础信息；
  CPU、内存、磁盘资源指标暂未采集。
- channel affinity 当前已完成控制面 options/cache stats API、默认规则和 gateway
  热路径本地内存命中/记录；usage cache 统计仍未接入 gateway 实例本地缓存，
  也没有 Redis/跨实例共享语义。
- web 前端源码已提取并可构建；`dist`/`node_modules` 不纳入 git，需要部署或本地
  运行前执行 `web/new-api/README.md` 中的构建命令。

## 优先级 P1: 计费和日志

- control-api 已有 `UsageEventBatch` internal ingestion 和 log/stat 查询。
- gateway 已有可选 HTTP usage 上报。
- SQLite 模式已支持基于 `request_id` 的 usage 幂等落库。
- control-plane 已有 `SettleUsageRequest` service，internal usage ingestion 会对 accepted
  success events 按 options 中的 ModelPrice/ModelRatio/CompletionRatio/GroupRatio/
  GroupGroupRatio 更新 user/token/channel quota 投影，并随 management store 持久化。
- Postgres usage 已落库；待做：MySQL usage 落库、ClickHouse 日志库。
- redemption/topup：
  - 已有 redemption CRUD/search/get/delete、invalid cleanup、兑换码充值。
  - 已有 topup info、自身/全局 topup 记录查询、管理员手动补单 service。
  - 已有 payment compliance 确认入口和 `topup/info` 兼容返回。
  - 待做：支付拉单、Stripe/Creem/Waffo/Epay webhook、topup 创建流程、
    订单级并发互斥的 DB 原子化、subscription payment。
- quota：
  - 已有 usage event 后结算骨架，支持 new-api 基础文本计费：
    ModelPrice、ModelRatio、CompletionRatio、GroupRatio、GroupGroupRatio。
  - 已有 snapshot/router 级基础 group 路由：token group 会从 token
    覆盖值或用户 group 推导，channel group 从渠道配置发布，gateway 仅在匹配
    group 的渠道内选路。
  - 已有 `UserUsableGroups` 校验、`AutoGroups` 顺序选路、`GroupSpecialUsableGroup`
    基础叠加，以及 usage event 实际 group 回传结算。
  - 已有 cache read/cache creation token 上报和 `CacheRatio`/`CreateCacheRatio`
    基础文本计费。
  - 已有 image/audio token details 上报和 `ImageRatio`/`AudioRatio` 基础文本计费。
  - 待做：cross-group retry、tool surcharge/tiered billing、音频专用价格表、
    预扣、失败回滚、subscription funding、trust quota、quota notification。
- log 查询：
  - 已有 admin/self/token list 和 stat，SQLite 模式可重启后继续查询。
  - 已有 legacy `DELETE /api/log/` 按时间清理历史 usage events。
  - 已有 `/api/system-task/log-cleanup` 异步清理入口，并可通过
    `/api/system-task/current|list|:task_id` 轮询状态。
  - 已有 quota 回写、group/is_stream/ip/request_id/upstream_request_id 字段。
  - 已有 usage details JSON `other` 字段。
  - 待做：更细的 new-api log content 和管理审计日志内容。
- data usage：
  - 已有 usage event 小时聚合，SQLite 模式可重启后继续查询；聚合 quota
    使用 settlement 回写后的实际扣费值。
  - 待做：new-api 风格 quota_data 独立表、后台 flush/cache、flow 维度完全对齐。

## 优先级 P2: 配置、模型和价格

- options/settings：
  - 已有 `GET /api/option/`、`PUT /api/option/`
  - 已有启动默认值、配置覆盖、SQLite 持久化、敏感项过滤和基础 JSON 校验
  - 已有 `POST /api/option/payment_compliance`
  - 已有 `POST /api/option/rest_model_ratio`
  - 已有 channel affinity options 默认值、cache stats/clear 接口和 gateway
    channel affinity 执行路径
  - 已有 new-api 默认 `GroupRatio`/`UserUsableGroups`、`AutoGroups` snapshot
    发布和 `/api/pricing` usable/auto groups 兼容返回。
  - 待做：完整 ratio/pricing 运行时语义、支付/OAuth 相关设置副作用、
    new-api 默认倍率表和 ratio cache 联动
- model metadata：
  - 已有本地 catalog CRUD/search/missing、`sync_upstream/preview`、
    `sync_upstream` 和 memory/SQLite 持久化。
  - 待做：new-api sync 的 ETag/body cache、重试退避/TLS insecure 配置、
    和 pricing cache 的完整联动。
- vendor metadata：
  - 已有本地 catalog CRUD/search 和 memory/SQLite 持久化
  - 待做：远程 sync 时的 vendor upsert/merge 策略、pricing vendor cache 完整联动
- pricing/rankings/perf metrics public endpoints：
  - 已有本地只读实现，覆盖 pricing、rankings、perf-metrics summary/detail
  - 已有 ratio_sync channels/fetch，支持从上游 pricing/ratio_config/OpenRouter/
    models.dev 拉取并计算本地差异。
  - 待做：new-api perf_metrics 独立聚合表、TTL/cache、header nav module 权限、
    更完整的 endpoint metadata、复杂 billing/pricing cache 兼容、ratio sync
    SSRF 防护和 TLS insecure 配置项对齐。

## 优先级 P3: 异步任务和边缘功能

- system task：
  - 已有 log cleanup、channel_test、model_update 手动任务和 current/list/detail API。
  - 已有可配置启用的单节点 channel_test/model_update scheduled runner。
  - 待做：多节点 lease/heartbeat、midjourney_poll、async_task_poll。
- system-info：
  - 已有 instances list 和 stale instance delete API，支持 memory/SQLite。
  - 待做：多节点 gateway/control 实例心跳、资源指标采集、稳定 node name 配置项。
- midjourney/task/video/suno/kling 等异步任务后台。
- OAuth、passkey 高级策略完全对齐。
- subscription/payment webhook、在线充值拉单、支付渠道回调。
- deployment 管理。

## 完成度 Review

完整 review 见 `docs/CONTROL_API_WEB_REVIEW_CN.md`。

摘要：

- control-api 已具备 gateway 控制面最小闭环。
- 完整 web 前端调用面仍有大量缺失 API，尤其是 payment/subscription/OAuth/deployment/task。
- model/token `status_only` 已修复（`{id,status}` 可反序列化；token 不会被默认值覆盖）。
- prefill_group 已持久化（memory/SQLite/Postgres）。
- memory + SQLite + Postgres（management/options/usage/prefill）可用；MySQL 与
  生产级 session 仍未完成。

## 实现原则

- DB/storage 先抽 trait，再提供 SQLite/Postgres 实现。
- axum handler 只做 HTTP adapter，业务逻辑放在 `service_async::Service`。
- handler 构造 `ControlContext(certain_map)`，service 通过 trait bound 声明依赖。
- gateway 热路径不访问 DB，不拿 control-plane 锁。
- snapshot 更新采用版本化全量发布，后续再做增量。
- new-api 没有实现或明确标记 not implemented 的 relay 能力，不主动发明转换。
