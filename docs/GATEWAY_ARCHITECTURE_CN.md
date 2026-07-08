# Halolake Gateway 架构说明

## 设计边界

Halolake gateway 的目标不是把 new-api 的后台业务整块搬进 Monoio，而是替换高频请求转发热路径。

当前边界：

- 协议格式、解析、转换：参考 `ref/new-api` 已验证实现。
- 网络转发、服务组合、上下文传递：参考 `ref/monolake` 风格。
- 共享协议和路由 crate 不依赖 Tokio 或 Monoio。
- gateway 热路径不访问数据库，不做同步文件 I/O，不持有全局大锁。

## Workspace

```text
crates/api-contract
  OpenAI / Claude DTO。

crates/protocol
  runtime-agnostic 协议转换。
  只实现 new-api 中已有且明确的转换路径。

crates/router-core
  本地配置快照、token 校验、模型映射、渠道路由。

apps/gateway-monoio
  Monoio 数据面，可作为 lib 嵌入，也可作为 bin 通过配置文件启动。
```

## 为什么不是 Tokio 一把梭

Tokio 适合 control plane、DB、后台任务、管理 API。gateway data plane 更接近 L7 proxy：

- 连接和请求数高。
- 单请求 CPU 逻辑较薄。
- 生命周期主要是 client <-> gateway <-> upstream。
- 配置来自本地只读快照。

因此 gateway 使用 Monoio thread-per-core 模型，尽量让每个 worker 独立处理连接和上游连接池，减少跨线程同步。

## Service / Layer 风格

gateway 代码应持续保持 service pipeline 风格，而不是把所有逻辑写进一个 handler：

```text
HTTP boundary
  -> request context
  -> auth/route service
  -> protocol service
  -> relay service
  -> response mapper
```

当前实现已经使用 `service_async::Service` 拆分：

- `ChatGatewayService`
- `ClaudeMessagesGatewayService`
- `RawOpenAiGatewayService`
- `AuthRouteService`
- `RelayService`

后续新增能力应优先作为 layer/service 插入，例如：

- quota check
- rate limit
- model rewrite
- fallback
- usage event
- metrics/tracing
- request/response override

## certain_map 上下文

请求上下文使用 `certain_map!` 承载逐层附加的数据：

```text
RequestId
PeerAddr
DownstreamProtocol
RequestAuth
RouteContext
```

这样每层 service 可以通过 trait bound 声明自己需要哪些上下文，而不是依赖一个巨大的可变 struct。

例如 relay service 只要求：

```text
ParamRef<RequestAuth>
ParamRef<RouteContext>
ParamRef<RequestId>
ParamRef<PeerAddr>
```

auth/route 层负责把 `RequestAuth` 和 `RouteContext` 写入 context，后续 billing、metrics、日志层都可以零散读取，不需要共享 Mutex。

## 协议转换原则

协议转换只跟随 new-api 已有行为，不主动发明复杂跨协议转换。

当前做：

- OpenAI `/v1/chat/completions` -> Claude `/v1/messages`
- Claude `/v1/messages` -> OpenAI chat response / SSE
- Claude `/v1/messages` -> Claude upstream passthrough
- OpenAI `/v1/chat/completions` -> Gemini `generateContent` / `streamGenerateContent`
- Claude `/v1/messages` -> OpenAI Chat -> Gemini `generateContent`，返回时 Gemini -> OpenAI -> Claude
- Gemini `generateContent` -> OpenAI Chat，返回时 OpenAI -> Gemini
- Gemini `generateContent` -> Gemini upstream passthrough
- OpenAI raw endpoints -> OpenAI upstream passthrough
- OpenAI `/v1/images/generations` -> OpenAI image upstream passthrough
- OpenAI `/v1/images/edits` -> OpenAI image upstream passthrough，支持 JSON 和 multipart/form-data
- OpenAI `/v1/edits` -> OpenAI upstream passthrough
- OpenAI `/v1/images/generations` -> Gemini Imagen `predict`，返回 OpenAI image response

当前不做：

- OpenAI Responses API -> Claude
- Claude Messages -> OpenAI Responses
- Gemini native -> Claude upstream 直转，new-api 的 Claude adaptor 也未实现该路径
- OpenAI image edits -> Gemini，new-api 没有实现该路径
- OpenAI image -> Claude，new-api 的 Claude adaptor 也未实现
- audio/files/fine-tuning 跨协议转换

如果 new-api 没有做某条转换，默认认为它有兼容性或语义风险，Halolake 也不做。

## 配置文件

bin 入口只需要一个 TOML 配置：

```bash
cargo run -p halolake-gateway-monoio -- --config examples/gateway.toml
```

配置包含：

- server：监听地址、请求体大小。
- protocol：Claude version、是否透传 anthropic-beta、Gemini API version。
- upstream：连接和读取超时。
- auth：接受 Bearer 和 x-api-key。
- tokens：本地 token 快照。
- channels：上游渠道。
- model_mappings：模型到渠道和上游模型映射。

示例见 `examples/gateway.toml`。

## 后续演进

优先级建议：

1. 把 auth/route/protocol/relay 更系统地整理成 `FactoryLayer`/`FactoryStack`。
2. 增加 usage event service，但只 fire-and-forget，不在热路径落库。
3. 增加 failover service，基于只读快照和 worker-local 状态。
4. 增加配置热更新，worker-local 原子替换。
5. 用真实 opencode / Claude Code / Codex 请求做兼容测试集。
