# Halolake × Tokiame × ComfyUI 设计与部署

## 1. 结论

本实现采用以下边界：

- **Halolake control-api**：用户/token 鉴权、节点凭据、工作流白名单、作业与产物元数据、额度预留和最终结算。
- **tokiame-hub**：公网 WebSocket 会话和命令 ACK；不落业务数据，不转发文件内容。
- **tokiame-agent**：从 NAT 内主动连接 hub，调用本机 ComfyUI，保留 SQLite 恢复日志。
- **ComfyUI**：只监听本机或内网，不直接暴露公网。
- **私有 S3-compatible 对象存储**：用户/agent 使用短期签名 URL 直传、直下图片和视频。

这避免了两个风险：

1. 把长连接放入 `gateway-monoio` 会遇到 thread-per-core worker 各自持有状态的问题；一次 HTTP 调度不保证落到持有目标 WebSocket 的 worker。
2. 图片/视频经过 Tokiame 或 Halolake 中转会占用双倍带宽、内存和长请求超时预算。协议 v2 只传 JSON 元数据，不包含 base64/binary body。

```text
用户 ──Bearer token──► Halolake control-api ──internal──► tokiame-hub
 │                           │                               ▲
 │ presigned PUT/GET         │ 作业/额度/产物元数据           │ WSS（NAT 内主动连接）
 ▼                           ▼                               │
私有对象存储             SQLite/MySQL/Postgres          tokiame-agent
 ▲                                                           │ localhost
 └──────────── presigned GET/PUT ────────────────────────────┤
                                                             ▼
                                                          ComfyUI
```

## 2. 为什么使用对象存储

对象存储是这里更合适的数据面：

| 数据 | 传输路径 | 原因 |
|---|---|---|
| 作业命令、进度、取消、ACK | agent ↔ hub | 小 JSON、需要有序会话 |
| 工作流/用户/token/额度元数据 | control-api ↔ DB | 需要持久化和审计 |
| 输入图片、输出图片/视频 | 用户或 agent ↔ S3 | 大文件流式传输、可扩容、可短期授权 |

Bucket 必须保持 private。数据库只保存 `bucket/key` 对应的稳定对象键；签名 URL 不落库。上传签名会绑定 `Content-Type` 和 `x-amz-meta-sha256`，完成回调再用 HEAD 校验大小和 SHA-256 元数据。

当前版本是单个 presigned PUT：单对象上限 **5 GiB**，默认用户输入上限 **2 GiB**。更大的视频需要后续增加 multipart create/sign-parts/complete API，不能通过提高 HTTP timeout 绕过 S3 单 PUT 限制。

## 3. 信任与凭据

系统存在三类互不复用的凭据：

| 凭据 | 使用方 | 权限 |
|---|---|---|
| Halolake API token | 用户 | 创建本人上传/作业、读取本人产物 |
| `tw_*` worker token | tokiame-agent | 只注册一个预创建 worker；服务端仅保存 SHA-256 hash |
| `internal_secret` | control-api ↔ hub | 调用 `/internal/tokiame/*`；不得交给 agent/用户 |

管理员创建 worker 时会自动创建 `channel_type = 100` 的 Tokiame 渠道。worker 离线时渠道自动禁用；注册后按 agent 宣告的 workflow capabilities 同步模型并启用。该渠道不会进入 LLM HTTP gateway snapshot。

工作流文件只存在于 agent 节点。control-api 和 agent 必须同时允许相同的 `id/version/model`。用户参数只能写入 agent 配置中声明的 RFC 6901 JSON Pointer；未知参数、未知工作流和不存在的指针全部拒绝，用户不能提交任意 ComfyUI workflow JSON。

## 4. 作业生命周期

```text
queued → dispatched → running → uploading → completed
   │          │           │           └──► failed
   └──────────┴───────────┴──────────────► failed / cancelled
   └── hub 暂不可用 ─────────────────────► orphaned ──同 Idempotency-Key 重试
```

- control-api 在创建作业时原子选择在线且有容量、workflow 匹配的 worker，并预留固定 quota。
- hub 把命令写入目标 session 后等待 agent `CommandAck`，默认 10 秒。
- agent 每个状态事件都写 SQLite pending outbox，收到 control-api 的 `JobEventAck` 后才清除。
- 输出必须完成 S3 PUT，并收到 `ArtifactUploadedAck`，之后 agent 才发送 `completed`。
- `completed` 使用 `UsageEvent.quota` 做固定额度结算；`failed/cancelled` 释放预留且不扣费。
- 完成扣费使用持久化 settlement ledger；ledger key 与用户/token/渠道额度在同一数据库事务提交，断线重投和重启恢复不会重复扣费或漏扣。
- `Idempotency-Key` 以 token 为作用域，重试不会创建第二个作业。

agent 重启后从 SQLite 恢复 `dispatch/prompt_id/event_sequence/pending_event`，继续轮询 `/history/{prompt_id}`。worker 重连会生成新 session；control-api 将同 worker 的未完成作业绑定到新 session，旧 session 的事件会被拒绝。

取消使用 ComfyUI `/queue {"delete":[prompt_id]}`。当前不调用全局 `/interrupt`，因为它可能中断同一 ComfyUI 实例上的其他租户作业。作业进入 `uploading` 后，取消请求返回 `409 Conflict`：此时输出已经产生，必须完成对象上传和额度结算，或以失败终止；只有 `completed && quota_settled` 的输出才能下载，失败/取消留下的部分对象等待生命周期清理。

## 5. 配置与启动

### 5.1 control-api

参考 [tokiame-control-api.toml](../examples/tokiame-control-api.toml)。S3 密钥建议只从环境变量读取：

```bash
export HALOLAKE_S3_ACCESS_KEY_ID=...
export HALOLAKE_S3_SECRET_ACCESS_KEY=...
cargo run -p halolake-control-api -- \
  --config examples/tokiame-control-api.toml
```

MinIO/R2 使用 `endpoint_url`；AWS S3 可省略。R2 通常设置 `force_path_style = true`，region 使用服务商要求的值。

### 5.2 hub

hub 应为单独进程/Deployment，不能按 gateway worker 数复制。首版应保持 **1 replica**；未来要水平扩展时，应增加 worker→hub 一致性路由或 Redis/NATS session directory。

```bash
export HALOLAKE_INTERNAL_SECRET=...
cargo run -p halolake-tokiame-hub -- \
  --config examples/tokiame-hub.toml
```

公网只需暴露 `wss://.../v1/tokiame/connect`。`/internal/tokiame/*` 仅允许 control-api 网络访问。

### 5.3 创建 worker

使用管理员 Web session 或管理员 access token：

```http
POST /api/tokiame/workers
Authorization: Bearer <admin-access-token>
Content-Type: application/json

{"namespace":"home-gpu-01","node_name":"comfyui-01","group":"default"}
```

响应中的 `tw_*` token 只显示一次。轮换接口：

```text
POST /api/tokiame/workers/{worker_id}/rotate-token
PUT  /api/tokiame/workers/{worker_id}/enabled   {"enabled":false}
```

### 5.4 NAT 后的 agent

复制 [tokiame-agent.toml](../examples/tokiame-agent.toml) 和示例 [API workflow](../examples/tokiame-workflows/sdxl-txt2img-api.json)，修改 checkpoint 名称与 JSON Pointer：

```bash
export TOKIAME_WORKER_TOKEN='tw_...'
cargo run -p halolake-tokiame-agent -- \
  --config examples/tokiame-agent.toml
```

agent 只需要：

- 出站访问 hub 的 WSS；
- 出站访问对象存储；
- 访问 `127.0.0.1:8188` 的 ComfyUI。

不需要路由器端口转发。若 agent 与 ComfyUI 共享 input 目录，配置 `comfyui.input_dir`；否则 agent 使用 ComfyUI `/upload/image`，两种路径都以流式文件处理，不 `read_to_end`。输入流一旦超过声明大小会立即中止；输出流受 agent `comfyui.max_output_bytes` 和 control-api 上限双重约束。

同一 GHCR 镜像也包含 agent，可在 GPU 主机覆盖 entrypoint：

```bash
docker run --rm --network host \
  --entrypoint halolake-tokiame-agent \
  -e TOKIAME_WORKER_TOKEN \
  -v "$PWD/tokiame:/config" \
  -v "$PWD/tokiame-state:/state" \
  ghcr.io/tokimorphling/halolake:main \
  --config /config/tokiame-agent.toml
```

## 6. 用户调用

先申请输入上传：

```http
POST /v1/media/uploads
Authorization: Bearer sk-<halolake-token>
Content-Type: application/json

{"name":"source.png","content_type":"image/png","size_bytes":123456,"sha256":"<64 hex>"}
```

客户端严格按响应的 `upload.method/upload.url/upload.headers` 直接 PUT 到对象存储，再调用：

```text
POST /v1/media/uploads/{artifact_id}/complete
{"size_bytes":123456,"sha256":"<64 hex>"}
```

创建作业：

```http
POST /v1/media/jobs
Authorization: Bearer sk-<halolake-token>
Idempotency-Key: portrait-20260806-001
Content-Type: application/json

{
  "model":"comfy-sdxl",
  "workflow":"sdxl-txt2img",
  "version":"v1",
  "parameters":{"prompt":"portrait photo","width":1024,"height":1024,"seed":42,"steps":24},
  "input_artifact_ids":[]
}
```

查询与事件：

```text
GET    /v1/media/jobs/{job_id}
GET    /v1/media/jobs/{job_id}/events    # text/event-stream
DELETE /v1/media/jobs/{job_id}
GET    /v1/media/artifacts/{id}/content  # 307 到短期 GET URL
```

`/v1/media/*` 目前位于 control-api，而普通 LLM `/v1/*` 位于 gateway。生产域名应按 path 分流：

```caddyfile
api.example.com {
    @media path /v1/media/*
    reverse_proxy @media 127.0.0.1:9090
    reverse_proxy 127.0.0.1:8082
}

tokiame.example.com {
    reverse_proxy 127.0.0.1:9091
}
```

## 7. Bucket CORS 与生命周期

浏览器直接 PUT 时，bucket CORS 至少允许业务域名的 `PUT/GET/HEAD`，允许 `Content-Type`、`x-amz-meta-sha256` 请求头。不要使用 `*` origin 搭配用户 Cookie；媒体 API 本身使用 Bearer token。

建议增加未完成上传清理任务/生命周期规则，避免申请签名后不 complete 的对象长期占用空间。删除数据库记录前应先删除对象；对象删除失败进入重试队列，不应静默丢失引用。

## 8. 当前限制与下一步

已实现的是可运行的第一阶段，边界明确但仍有这些限制：

1. 单 PUT 最大 5 GiB；超大视频需要 multipart 签名流程。
2. ComfyUI 进度使用 `/history` 轮询，只报告关键状态；后续可接 ComfyUI WebSocket 的 node/progress 事件。
3. 额度目前按 workflow 固定值结算；可扩展为基础价 + 时长/GPU 档位，但必须由服务端策略计算。
4. worker 永久离线时不自动迁移已提交 prompt；现有策略等待相同 agent journal 恢复，或由用户取消后新建作业。
5. hub 当前是单实例内存 session registry；扩容前必须实现会话归属路由，不能直接增加 replica。
6. 管理端已提供 API，尚未加入 Web 管理页面。
7. 媒体 worker/job/artifact 当前持久化为单行候选快照，适合首阶段少量 GPU 节点；大量节点或长期保留作业前，应拆为归一化表、增量 heartbeat 更新和清理/归档任务。

这些限制都不会让大文件回落到 Tokiame/LLM gateway。即使后续增加 multipart、进度或调度器，文件数据面仍应保持对象存储直传。
