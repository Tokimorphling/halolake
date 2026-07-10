# Rust Gateway 设计草案

## 目标

研发一个 Rust backend，兼容 new-api 前端和核心转发能力，并优先把用户请求转发链路做成高性能 gateway。

整体采用双平面架构：

```text
Control Plane: Tokio + axum
Data Plane:    Monoio gateway
Shared Core:   runtime-agnostic crates
```

第一阶段重点不是完整复刻 new-api 全部后台能力，而是先跑通高性能协议转发热路径。

## 核心判断

Monoio 的 thread-per-core 模型适合 data plane 热路径：

- 多连接、高并发、I/O-bound
- 每个请求 CPU 逻辑较薄
- 请求生命周期主要是 client <-> gateway <-> upstream
- 状态主要来自本地快照，而不是每次请求查数据库

不建议把完整后台业务都放进 Monoio：

- 管理 API
- 用户、渠道、模型 CRUD
- DB 事务
- 复杂计费结算
- 后台任务
- 报表、审计、批处理

这些更适合 Tokio 生态。

`ref/monolake` 是你的参考gateway实现，
这个代码仓库中，使用了大量的service-async抽象，HRTB，certain_map等组件
async fucntion in traits，GAT等等高级泛型和trait技巧，让代码更简洁，美观，分层的同时
尽量减少额外开销，达到最高性能
我要求你的gatewat开发也要参考monolake来实现，多用高级的Rust features

## 仓库结构建议(参考，crate名字建议加上halolake之类的前缀)

```text
crates/
  api-contract/
    # new-api 前端兼容 DTO、分页、错误格式、公共响应结构

  domain/
    # 用户、令牌、渠道、模型、价格、额度、用量等核心类型

  protocol/
    # OpenAI Chat <-> Claude Messages <-> Gemini 等协议转换
    # 尽量保持纯函数，不依赖 Tokio/Monoio

  router-core/
    # 模型映射、渠道选择、权重、故障熔断、限流接口

  billing-core/
    # usage 结构、计费规则、预扣/结算事件、价格计算纯逻辑

apps/
  control-api/
    # Tokio + axum
    # 管理 API、DB、鉴权、配置发布、计费落库

  gateway-monoio/
    # Monoio
    # 用户请求入口、协议转换、upstream relay、SSE streaming
```

## 依赖边界

共享核心 crate 必须尽量 runtime-agnostic：

```text
api-contract/domain/protocol/router-core/billing-core
  不依赖 Tokio
  不依赖 Monoio
  不直接访问 DB
  不启动 HTTP server/client
```

应用层负责运行时绑定：

```text
control-api
  Tokio
  axum
  sqlx
  redis
  reqwest
  tracing

gateway-monoio
  Monoio
  Monoio HTTP stack
  本地配置快照
  轻量 metrics/event reporter
```

## 请求链路

```text
Client
  -> gateway-monoio
  -> token/key 本地校验
  -> quota/rate-limit 本地快照校验
  -> model mapping
  -> channel selection
  -> protocol conversion
  -> upstream request
  -> response/SSE conversion
  -> Client
  -> usage event 异步上报
```

gateway 不应该在热路径里同步访问数据库。

## 配置同步

Control Plane 负责维护权威配置：

- users
- tokens
- channels
- model mappings
- quota snapshots
- rate limit policies
- pricing rules
- failover policies

Gateway 使用本地快照：

```text
control-api
  -> 生成配置快照
  -> 推送或 gateway 拉取
  -> gateway 每个 worker 持有本地副本
```

推荐策略：

- 配置快照带版本号
- 支持增量更新，但第一版可以全量替换
- 每个 Monoio worker 持有自己的只读副本
- 更新时用原子替换，避免热路径加锁
- 当前 monoio gateway 使用 `arc-swap` 持有当前 `SnapshotState`，后台
  `SnapshotSource` polling 拿到新版本后整块替换；请求热路径只做 atomic load。

## Control Plane 与 Snapshot 边界

`control-api` 应该作为独立服务实现，但 gateway 不应该直接依赖这个 app。
两者之间通过小而稳定的 service trait 通信：

```text
SnapshotSource
  gateway 从这里获取最新 GatewaySnapshot

SnapshotPublisher
  control-api 构建快照后发布到这里

UsageEventSink
  gateway 将 usage / billing / audit 事件上报到这里

ChannelFeedbackSink
  gateway 将 upstream status/transport failure 上报到这里，由控制面决定是否
  自动禁用 channel 或 multi-key 中的单个 key
```

这样同一套核心逻辑可以支持不同部署形态：

```text
单进程嵌入:
  control-api + gateway
  -> MemorySnapshotBus

多进程部署:
  control-api
  -> HTTP internal API
  -> gateway polling / long-poll

测试:
  gateway
  -> Static/File/Memory SnapshotSource
```

第一版推荐使用 HTTP internal API 做进程间通信：

- `GET /internal/gateway/snapshot?since_version=N`
- `POST /internal/gateway/usage`
- `POST /internal/gateway/channel-feedback`
- 未变化时返回 not-modified 语义
- 有变化时返回完整 `GatewaySnapshot`
- 之后可以升级为 long-poll、SSE 或增量 patch
- 内部接口需要 shared secret / HMAC，生产部署再考虑 mTLS

Memory 模式只用于同进程嵌入和测试。两个独立进程之间不做普通 memory
共享，避免为了过早优化引入 shared memory 的复杂度。

## Control Plane Service 风格

`control-api` 使用 Tokio + axum，但 axum handler 只作为 HTTP adapter。
业务逻辑仍然按 `service_async::Service` 组织，避免把所有逻辑写进 handler。

```text
axum handler
  -> 构建 ControlContext(certain_map)
  -> 调用 command/query service
  -> 映射 HTTP response
```

`certain_map` 用于表达每层 service 需要的上下文能力，例如：

```text
RequestId
Actor/AdminAuth
DbPool
PermissionSet
AuditSink
SnapshotPublisher
```

不要设计一个巨大的 `ControlApiClient` 或一个巨大的 `ControlPlaneService`
来承载所有操作。更合适的是按能力拆小 service：

```text
CreateTokenService
UpdateChannelService
BuildSnapshotService
PublishSnapshotService
RecordUsageBatchService
```

DB transaction 应由具体 command service 创建、提交或回滚，不建议长期放在
全局 context 中跨多个业务层传递。

## Snapshot Provider 抽象

gateway 侧只依赖 snapshot provider 抽象，不关心快照来自配置文件、内存、
control-api HTTP，还是未来的消息队列。

推荐请求和响应模型：

```text
SnapshotRequest {
  since_version: Option<u64>
}

SnapshotResponse {
  NotModified { version }
  Updated(GatewaySnapshot)
}
```

推荐实现：

```text
StaticSnapshotSource
  从本地配置构建，适合开发和最小部署

MemorySnapshotSource
  同进程共享，适合嵌入式部署和单元测试

HttpSnapshotSource
  通过 control-api internal API 拉取，适合生产多进程部署
```

这些 provider 不进入请求热路径，只在后台同步任务中运行。请求热路径只读取
已经索引好的 `IndexedSnapshot`。更新时采用版本化全量替换和 atomic swap，
后续再考虑增量。

## 计费与日志

gateway 只做轻量事件生产：

```text
UsageEvent {
  request_id
  user_id
  token_id
  channel_id
  model
  upstream_model
  prompt_tokens
  completion_tokens
  total_tokens
  status
  latency_ms
  created_at
}
```

Control Plane 或独立 billing worker 负责：

- 用量落库
- 额度扣减
- 失败回滚
- 审计日志
- 报表统计

第一版可接受 event at-least-once，上层用 `request_id` 做幂等。

## 协议转换范围

第一阶段 MVP：

```text
OpenAI /v1/chat/completions -> Claude /v1/messages
Claude /v1/messages         -> OpenAI /v1/chat/completions
```

必须支持：

- 非流式响应
- SSE streaming
- system/user/assistant messages
- max_tokens / temperature / top_p
- basic tool calls
- usage 转换
- upstream error 映射

暂不承诺：

- OpenAI Responses API -> Claude
- image/audio/video
- fine-tuning/files
- provider 全量参数兼容
- 复杂 prompt cache 计费细节

## Thread-per-core 约束

为了适配 Monoio，gateway 代码应遵守：

- 热路径避免阻塞调用
- 不做同步文件 I/O
- 不在请求路径里直接写数据库
- 避免跨 worker 共享大锁
- 长 SSE 连接要有超时、限流和 backpressure
- CPU-heavy 工作丢给旁路 worker 或 control plane

适合放进 gateway：

- HTTP relay
- SSE relay
- 本地 token 校验
- 本地路由选择
- 轻量协议转换
- usage event fire-and-forget

不适合放进 gateway：

- 管理后台
- 复杂 DB 事务
- 报表
- 长周期任务
- 大规模 tokenization
- 复杂计费结算

## MVP 里程碑

### M1: gateway skeleton

- 启动 Monoio HTTP server
- 实现 `/v1/chat/completions`
- 固定配置文件加载 channel
- 请求转发到 Claude upstream
- 非流式响应转换回 OpenAI 格式

### M2: streaming

- 支持 OpenAI stream 请求
- Claude SSE -> OpenAI SSE
- 客户端断开处理
- upstream 超时处理

### M3: auth and routing

- 本地 token 校验
- model mapping
- 多 channel 选择
- 基础 failover

### M4: control plane

- Tokio + axum 管理 API
- DB 存储 user/token/channel/model mapping
- 配置快照发布
- gateway 热更新快照

### M5: billing events

- gateway 生成 usage event
- billing worker 幂等消费
- quota 扣减
- usage logs 查询

channel health/auto-ban 不放进 gateway 热路径做持久化。gateway 只发送
`ChannelFeedbackEvent`，包含 request/channel/key index/status/reason；control-api
读取 options 和 channel `auto_ban` 后按 new-api 规则更新管理数据并发布新 snapshot。

## 风险

- Monoio 生态比 Tokio 小，HTTP client/server 组件选择要提前验证。
- SSE 长连接可能导致 worker 负载不均，需要压测观察。
- 协议转换细节多，必须以兼容测试锁定行为。
- usage 计费涉及缓存 token、tool call、stream usage，第一版应控制范围。
- Gateway 与 Control Plane 的配置一致性需要版本化设计。

## 当前结论

采用：

```text
Tokio control-api
Monoio gateway
runtime-agnostic shared crates
```

第一阶段优先交付：

```text
/v1/chat/completions <-> Claude /v1/messages
SSE streaming relay
本地 token 校验
本地渠道路由
usage event 上报
```

不在第一阶段追求完整 new-api backend 复刻。
