# CLIProxyAPI / Sub2API Auth 文件：实现对照与移植手册

> 目标：当 `ref/CLIProxyAPI` 或 `ref/sub2api` 上游更新时，能按本文件**快速 diff → 定位 → 移植**。  
> 运营向用法见 [`AUTH_IMPORT.md`](./AUTH_IMPORT.md)。  
> 本文偏**实现与迁移**，不是用户手册。

---

## 1. 总览

| 来源 | 上游产物 | Halolake 落点 | 主要代码 |
|------|----------|---------------|----------|
| CLIProxyAPI | `auths/*.json`（单账号文件） | Channel（type 按 provider） | `auth_import.rs` |
| Sub2API | 管理员备份 `type=sub2api-data` | Proxy 池 + Channel | `sub2api_data_import.rs` |
| Sub2API / Codex CLI | session / 嵌套 `tokens` / 裸 JWT | Channel type **57** | `codex_auth_import.rs` |

**统一入口**（feature `admin-extras`）：

| HTTP | Handler | 模块 |
|------|---------|------|
| `POST /api/channel/import/auth` | `import_auth_json` | `api_channel.rs` → `auth_import::import_auth` |
| `POST /api/channel/import/auth/upload` | `import_auth_multipart` | 同上（多文件） |
| `POST /api/channel/import/codex-auth` | `import_codex_auth` | 兼容旧路径 |
| `POST /api/channel/import/sub2api-data` | `import_sub2api_data` | 兼容旧路径 |

前端：`web/new-api/default/src/features/channels/components/dialogs/import-data-dialog.tsx`  
网关消费：`ChannelConfig.header_override`（snapshot）→ `gateway-monoio` 上游请求最后应用。

本地对照仓库（勿当运行时依赖）：

```text
ref/CLIProxyAPI/     # Go：auth 落盘 + executor 头
ref/sub2api/         # Go：ExportData / ImportCodexSession
```

---

## 2. 代码地图（改哪里）

### 2.1 Halolake

```text
apps/control-api/src/
  auth_import.rs           # 格式检测 + CLIProxy 分发 + xai 默认头
  codex_auth_import.rs     # Codex OAuth key 归一化 / 身份指纹
  sub2api_data_import.rs   # sub2api-data proxies + accounts
  api_channel.rs           # HTTP 路由（admin-extras cfg）
  channel_special.rs       # Codex refresh/usage（导入后运行时）

crates/control-plane/src/management.rs
  parse_channel_header_override  # setting → ChannelConfig.header_override
  build_snapshot                 # 发布到 gateway

crates/router-core/src/lib.rs
  ChannelConfig.header_override

apps/gateway-monoio/src/
  context.rs / services.rs / relay.rs
  apply_channel_header_override  # 上游请求最后覆盖
```

### 2.2 上游对照（优先 diff 这些路径）

**CLIProxyAPI**

| 主题 | 路径 |
|------|------|
| Auth 文件读写 / 上传 | `internal/api/handlers/management/auth_files*.go` |
| Codex OAuth 存储字段 | `internal/auth/codex/` |
| Claude OAuth | `internal/auth/claude/` |
| xAI OAuth + TokenData | `internal/auth/xai/xai.go` |
| **运行时请求头（关键）** | `internal/runtime/executor/xai_executor.go` → `applyXAIChatHeaders` |
| 常量版本号 | 同文件 `xaiClientVersionValue` 等 |
| 示例 auth | `auths/`、`examples/` |

**Sub2API**

| 主题 | 路径 |
|------|------|
| 备份导出/导入结构 | `backend/internal/handler/admin/account_data.go` |
| `DataPayload` / `DataProxy` / `DataAccount` | 同上 |
| Codex session 导入 | `backend/internal/handler/admin/account_codex_import.go` |
| Account schema | `backend/ent/schema/account.go` |
| Platform / Type 枚举 | `backend/internal/service`（platform 常量） |

---

## 3. 格式契约（稳定面）

### 3.1 CLIProxyAPI 单文件（`auths/*.json`）

检测：`auth_import::detect_format`  
分发：`import_cliproxy_file` 按顶层 `"type"`。

| `type` | Channel type | key 来源 | base_url / 备注 |
|--------|--------------|----------|-----------------|
| `codex` | **57** | OAuth JSON（`codex_auth_import`） | 默认 ChatGPT/Codex 路径；身份指纹 dedupe |
| `claude` | **14** | `access_token` | 默认 models 写死一串 claude-* |
| `gemini` / `gemini-cli` | **24** | `api_key` 或 `access_token` | 默认 gemini-2.5-* |
| `xai` / `x-ai` / `grok` | **48** | `access_token` 或 `api_key` | 见 §4 |
| 其它（antigravity 等） | — | — | **整文件失败**，batch 其它文件继续 |

**Codex 字段（与 CLIProxy `CodexTokenStorage` 对齐）**

| 上游字段 | Halolake |
|----------|----------|
| `access_token` | key JSON `access_token` |
| `refresh_token` | 同上 |
| `id_token` | 同上 |
| `account_id` | 同上 / 身份键 |
| `email` | 同上 |
| `expired` / `last_refresh` | 同上 |
| `type: codex` | 归一化进 key |

**xAI 字段（`parse_cliproxy_xai_auth`）**

| 上游字段 | Halolake |
|----------|----------|
| `auth_kind` | `setting.auth_kind`（默认 oauth / api_key） |
| `using_api` | `setting.using_api`；缺省时 oauth→false |
| `access_token` / `api_key` | `channel.key`（Bearer） |
| `refresh_token` | `setting.refresh_token`（**不进 key**，供日后 refresh） |
| `email` / `sub` | setting + 重导匹配 |
| `base_url` | 去尾 `/v1` 后写入 `channel.base_url` |
| `token_endpoint` | setting |
| `headers` | 合并进 `header_override`（覆盖默认） |
| `disabled` | status=2 手动禁用 |

**xAI 默认 base**

| 条件 | base |
|------|------|
| `using_api=true` 或非 oauth | `https://api.x.ai` |
| oauth 且未指定 | `https://cli-chat-proxy.grok.com` |

### 3.2 Sub2API 备份（`sub2api-data`）

上游：`account_data.go` 的 `DataPayload`（`type=sub2api-data`，`version=1`）。

| 字段 | Halolake |
|------|----------|
| `proxies[]` | `ProxyStore`；`proxy_key` 指纹匹配则 **reuse** |
| `accounts[]` | **新建** channel（Codex 身份命中且 `update_existing` 时可更新） |
| `skipped_shadows` | 仅透传/忽略；**不**导入 spark 影子号 |
| group 绑定 | **不做**（等同 `skip_default_group_bind`） |

**Account → Channel type**

| platform | type | Channel |
|----------|------|---------|
| openai | oauth / setup-token | **57** Codex |
| openai | apikey / upstream | **1** OpenAI |
| anthropic / claude | * | **14** |
| gemini / google | * | **24** |

`credentials` map：Codex 走 `parse_flexible_codex_key`；其它平台取常见 key 字段写入 `channel.key`。  
`proxy_key` → 导入后 `channel.proxy_id`。

### 3.3 Codex / Sub2API session（非 CLIProxy 文件）

`codex_auth_import` 兼容：

- 裸 access token（整段或按行）
- `{ "tokens": { access_token, refresh_token, id_token }, email, chatgpt_account_id, ... }`
- 数组 / JSON stream / 混合行

产出与 type-57 渠道 key 一致，供 `channel_special` refresh/usage 使用。

### 3.4 自动检测顺序（`format: auto`）

1. `type` ∈ `sub2api-data` | `sub2api-bundle`  
2. 同时有 `proxies` + `accounts`  
3. `type` ∈ CLIProxy 集合（codex/claude/gemini/xai…）  
4. 扁平 `access_token` + `account_id` 且无嵌套 `tokens` → CLIProxy  
5. 否则 → Codex session 解析器  

改检测逻辑时只动 `auth_import::detect_format` + 单测。

---

## 4. 运行时行为（导入之后）

### 4.1 xAI CLI chat-proxy 身份头（必对齐 CLIProxy）

上游：`xai_executor.go` → `applyXAIChatHeaders`（OAuth + chat-proxy 时）。

Halolake 导入侧：`build_xai_header_override`：

| Header | 值 | 常量位置 |
|--------|-----|----------|
| `X-XAI-Token-Auth` | `xai-grok-cli` | `auth_import.rs` |
| `x-grok-client-version` | `0.2.93` | **与 CLIProxy `xaiClientVersionValue` 同步** |

规则：

1. `!using_api` 且 base 为 `cli-chat-proxy.grok.com` → 注入上表  
2. 文件内 `headers` **后写覆盖**默认（同 CLIProxy custom headers 顺序）  
3. 写入 `channel.header_override` JSON map（字符串值）  
4. Snapshot → gateway `apply_channel_header_override` **最后**应用  

占位符（gateway）：`{api_key}`、`{client_header:Name}`。  
`*` / `re:` 透传规则：**尚未实现**（parse 时跳过）。

**上游改版本号时**：先 diff CLIProxy 常量，再改 Halolake 两个常量 + 单测 `xai_cli_headers_*`。

### 4.2 Codex

- key 为 JSON OAuth blob，不是纯 API key  
- 刷新/用量：`channel_special` + `/api/channel/{id}/codex/*`  
- 身份：`identity_keys_for_channel_key`（email / account_id / token 指纹）

### 4.3 重导与自动禁用

- `update_existing=true`：Codex / xAI 按身份更新  
- xAI 重导：`status=3`（自动禁用）→ 恢复 `1`；手动 `2` 不动  
- 自动禁用策略：`channel_feedback.rs`（对齐 new-api：**不因纯 Transport 禁用**）

---

## 5. 上游更新时的移植清单

### 5.1 CLIProxyAPI 发版

```text
[ ] git -C ref/CLIProxyAPI fetch && log / diff 上次 pin
[ ] auth 文件 schema
      - internal/auth/{codex,claude,xai}/ 结构体 JSON tag
      - auth_files 上传是否新增 type
[ ] 新 type？ → auth_import 分支 + CHANNEL_TYPE + 默认 models/base
[ ] xai_executor 头 / 版本常量 / base URL 规则
      - applyXAIChatHeaders
      - xaiClientVersionValue / Token-Auth 值
      - chat-proxy vs api.x.ai 分支
[ ] 改 Halolake：auth_import 常量 + build_xai_header_override
[ ] 单测：cargo test -p halolake-control-api xai_ / detects_cliproxy
[ ] 手工：导入 → 看 header_override → 测 chat（非仅 models）
[ ] 更新本文 §3/§4 与 AUTH_IMPORT.md 示例
```

### 5.2 Sub2API 发版

```text
[ ] git -C ref/sub2api fetch && diff account_data.go
[ ] DataPayload version 是否 bump
[ ] DataProxy / DataAccount 新字段（是否影响导入语义）
[ ] Export 是否仍排除 spark shadow；Import 是否强制 credentials
[ ] platform/type 新枚举 → sub2api_data_import 映射表
[ ] ImportCodexSession 输入形变 → codex_auth_import
[ ] proxy_key 算法是否变化（reuse 指纹）
[ ] 单测 + 一份真实 export（脱敏）回归
[ ] 更新本文 §3.2
```

### 5.3 建议 pin 方式

在团队笔记或本文件末尾维护：

```text
ref/CLIProxyAPI  @ <git sha or tag>   上次同步：YYYY-MM-DD
ref/sub2api      @ <git sha or tag>   上次同步：YYYY-MM-DD
```

（本仓库若 submodule 未固定，以你本地 `ref/` checkout 为准。）

---

## 6. 测试入口

```bash
# 格式检测 + xai 头
cargo test -p halolake-control-api xai_ -- --nocapture
cargo test -p halolake-control-api detects_cliproxy -- --nocapture

# header_override 进 snapshot
cargo test -p halolake-control-plane header_override -- --nocapture

# auto-ban 策略（非 auth，但影响「导入后测挂被禁」）
cargo test -p halolake-control-api transport_alone_does_not_disable -- --nocapture
```

手工：

1. UI **Channels → Import credentials** 多文件  
2. 或 `POST /import/auth/upload` multipart  
3. 渠道详情确认 `header_override` / `setting.import_source`  
4. 对 chat-proxy xAI 发真实 completion（拉 models 不够）

---

## 7. 已知缺口（移植时勿误以为已支持）

| 缺口 | 说明 |
|------|------|
| CLIProxy 全 type | antigravity / kimi / … 仍 per-file reject |
| header `*` / `re:` | 未做透传规则 |
| xAI token refresh | refresh 只存 setting，无完整 refresh worker |
| Sub2API 调度语义 | concurrency/priority 未完整还原运行时调度 |
| spark 影子账号 | 导出排除，导入不重建父子链 |
| Group 自动绑定 | 刻意不做 |
| 原文件名 | 仅 remark/setting 元数据 |

---

## 8. 最小移植示例

### 8.1 CLIProxy 调高 Grok CLI 版本

1. Diff：`ref/CLIProxyAPI/.../xai_executor.go` 中 `xaiClientVersionValue`  
2. 改：`apps/control-api/src/auth_import.rs` 的 `XAI_CLIENT_VERSION_VALUE`  
3. 测：`cargo test -p halolake-control-api xai_cli_headers`  
4. 已有渠道：重导 auth 或手改 `header_override`

### 8.2 Sub2API 备份增加 proxy 字段

1. Diff：`DataProxy` JSON tags  
2. 若仅展示字段 → 可忽略  
3. 若影响连通（如新 auth 方式）→ 扩展 `DataProxy` + `CreateProxyRequest` 映射  
4. 保持 `proxy_key` 稳定，避免 duplicate proxies

### 8.3 新 CLIProxy `type: foo`

1. 定 new-api / Halolake channel type id  
2. `import_cliproxy_file` match 臂  
3. key/base/models/setting 约定  
4. 若需特殊头 → `header_override` 或 gateway provider 分支  
5. detect_format 白名单  
6. AUTH_IMPORT + 本文表格

---

## 9. 相关文档

| 文档 | 内容 |
|------|------|
| [`AUTH_IMPORT.md`](./AUTH_IMPORT.md) | API / UI / 用户示例 |
| [`CONTROL_API_MODULES_CN.md`](./CONTROL_API_MODULES_CN.md) | admin-extras / 模块边界 |
| [`CHANNEL_GROUP_COMPAT_CN.md`](./CHANNEL_GROUP_COMPAT_CN.md) | 分组兼容 |

---

## 10. 同步记录（请更新）

| 日期 | CLIProxyAPI | Sub2API | 变更摘要 |
|------|-------------|---------|----------|
| 2026-07-12 | 本地 `ref/CLIProxyAPI` | 本地 `ref/sub2api` | 初版手册；xai 默认 chat-proxy 头；header_override 网关生效 |

（后续移植请追加行，勿删历史。）
