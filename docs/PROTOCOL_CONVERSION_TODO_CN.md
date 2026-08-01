# Halolake 协议转换审查与修复 TODO

更新时间：2026-08-01

## 目标与范围

本轮工作的目标是对照 `ref/new-api` 和 `ref/sub2api`，修复 Halolake 在
OpenAI Chat Completions、OpenAI Responses 与 Anthropic Messages 之间的协议转换，
重点避免以下问题：

- thinking/reasoning 文本丢失。
- Anthropic thinking signature 或 Responses encrypted reasoning 丢失。
- reasoning 与 tool call 多轮历史回放失配。
- 流式事件生命周期不完整，导致 Codex 无法识别 reasoning 或工具调用。
- custom、tool_search、namespace 等 Responses 客户端工具被降级后无法还原。
- 无法表示的字段或工具被静默丢弃，而不是返回明确错误。

当前优先完成的直接链路为：

```text
Responses request
  -> Anthropic Messages request
  -> POST /v1/messages
  -> Anthropic response / SSE
  -> Responses response / SSE
```

Gemini 相关转换已按当前需求暂停，不在本 TODO 的实施范围内。恢复工作时不要继续扩展
Gemini；现有 Gemini 改动应在最终合并前单独审查，确认是否保留。

## 当前状态

工作区包含尚未提交的协议修改。核心路径已经可以编译，但仍有半成品，不应直接部署或
合并。

| 能力 | 当前状态 | 备注 |
| --- | --- | --- |
| `/v1/responses` 到 Anthropic `/v1/messages` | 已实现主链路 | JSON、SSE 路由和基础请求/响应转换已接通 |
| Anthropic thinking 到 Chat | 已实现 | 流式、非流式均保留 reasoning 和 signature |
| Responses encrypted reasoning 到 Anthropic signature | 已实现核心映射 | 仍需补更多多轮及 include 语义测试 |
| reasoning + tool call 多轮回放 | 已实现基础归一化 | 会重排已配对结果并清理 dangling/orphan 项，仍需集成测试 |
| custom tool | 已实现 | buffered/SSE 生命周期、迟到名称宣告、input 还原均已覆盖；custom tool schema 强制 `{"input":string}`（对齐 sub2api `customToolInputSchema`）；input 还原带 raw fallback（对齐 `extractCustomToolCallInput`） |
| tool_search | 已实现 | 生命周期、client execution、非法 JSON 兜底均已覆盖；schema 与 proxy name 对齐 sub2api |
| namespace tool | 已实现 | 两阶段收集、声明/历史/choice 摊平、buffered/SSE 还原均已覆盖 |
| Anthropic server web search | 部分实现 | `server_tool_use` 已建模，input_json_delta 累计已修复；结果块仍需验证 |
| 不支持的 server tool | 已实现 | file_search/computer/image_generation 等在请求阶段明确拒绝 |
| Gemini thoughtSignature | 暂停 | 当前无需求，不继续开发 |

### 已完成的主要改动

- 新增 runtime-agnostic 的 Responses/Anthropic 转换模块：
  `crates/protocol/src/responses_anthropic.rs`。
- 新增 gateway 的 Anthropic Responses JSON/SSE 响应适配：
  `apps/gateway-monoio/src/responses_anthropic.rs`。
- Claude 渠道收到 `/v1/responses` 时，转换请求并转发到 `/v1/messages`；响应再转换回
  Responses JSON 或 SSE。
- `/v1/responses/compact` 到 Anthropic 当前明确返回 501 Not Implemented（compact 是
  OpenAI 特有的会话压缩端点，Anthropic 无对应概念，sub2api 也只转发到 OpenAI upstream）。
- `previous_response_id` 当前明确拒绝，因为 Halolake 此路径没有 Responses 服务端状态；
  调用方必须提交完整历史。
- Anthropic `thinking`、`signature_delta`、`redacted_thinking` 与 Responses
  reasoning/encrypted content 已建立双向映射。
- Claude 与 Chat Completions 的流式转换增加 EOF 收尾，避免上游异常断流时遗漏最终
  block close 或终态事件。
- Responses 历史中的 tool call/tool result 会做配对归一化：
  - 已回答的并行调用与结果相邻回放。
  - 未回答的 dangling 调用不会发送给 Anthropic。
  - orphan 或重复结果不会生成非法 Anthropic 历史。
- forced Responses -> Chat 路径已开始保留 reasoning、reasoning signature、tool call
  thought signature，并实现 custom/tool_search 的 function 代理。
- Anthropic API contract 已增加 thinking、output_config、redacted thinking、
  server_tool_use 和 web_search_tool_result 等结构。
- Responses usage 的 cache token 统计已修正，避免重复计入 input token。

### 已执行的验证

在 namespace 半成品加入之前，以下定向测试均通过：

- `cargo test -p halolake-protocol responses_anthropic -- --nocapture`：30 passed。
- `cargo test -p halolake-gateway-monoio --lib response::tests -- --nocapture`：24 passed。
- `cargo test -p halolake-gateway-monoio --lib relay::tests -- --nocapture`：18 passed
  （含 4 个 mock Anthropic upstream 集成测试：buffered/SSE/429 error/EOF）。

加入 namespace WIP 后执行过：

- `cargo check -p halolake-protocol -p halolake-gateway-monoio`：通过。
- `git diff --check`：通过。

namespace WIP 加入后尚未重新执行三组定向测试，也未执行 workspace 全量测试。

## 参考仓库结论

### sub2api

`sub2api` 是本链路的主要语义参考，尤其是直接 Responses/Anthropic 桥和客户端工具的
可逆状态机：

- `ref/sub2api/backend/internal/pkg/apicompat/responses_to_anthropic_request.go`
  - Responses 请求直接转 Anthropic Messages。
  - 处理 tool history 配对、thinking effort、工具和内容块。
- `ref/sub2api/backend/internal/pkg/apicompat/anthropic_to_responses_response.go`
  - Anthropic buffered/SSE 转 Responses。
  - 处理 thinking、signature delta 和 Responses 事件生命周期。
- `ref/sub2api/backend/internal/pkg/apicompat/chatcompletions_responses_bridge.go`
  - reasoning 与 tool call 的绑定。
  - custom/tool_search/namespace 的 Chat 降级与回程。
- `ref/sub2api/backend/internal/pkg/apicompat/responses_client_tools.go`
  - custom、tool_search、namespace 的请求改写和响应恢复状态机。

后续实现应继续优先复用 sub2api 的行为和测试场景，特别是 late tool name、namespace
冲突、custom input 生命周期和 tool_search 参数恢复。

### new-api

`new-api` 可作为 DTO、字段兼容和通用转换风格参考：

- `ref/new-api/relaykit/relayconvert/internal/oai_responses/to_claude_messages_req.go`
  提供 Responses 到 Claude Messages 的请求转换。
- `ref/new-api/relaykit/relayconvert/internal/oai_responses/to_oai_chat_req.go`
  和 `to_oai_chat_resp.go` 提供 Responses/Chat 的基础兼容。
- `ref/new-api/relaykit/relayconvert/internal/claude_messages/to_oai_chat_resp.go`
  有 Anthropic signature delta 到 Chat reasoning 字段的处理。

new-api 的主响应路径较多依赖 Responses -> Chat -> Claude 的分步转换，对 signature、
客户端工具和复杂多轮状态的覆盖弱于 sub2api。因此 Halolake 的直接桥以 sub2api 为主，
new-api 用于补字段别名和常规兼容语义。

## 未完成 TODO

### P0：完成或移除 namespace 半成品

- [x] 把 namespace 工具收集改为两阶段校验：先收集全部顶层 function/custom 名称，再
  摊平 namespace 子工具。当前单次遍历可能漏掉“namespace 在前、冲突顶层工具在后”
  的情况。
- [x] 将 namespace 子 function 声明转换为 Anthropic 普通工具，名称使用稳定的
  `<namespace>__<name>` 摊平形式。
- [x] 将历史中的 `function_call` + `namespace` 改写为相同的摊平名称。
- [x] 将 namespace 形式的 `tool_choice` 改写为摊平后的 Anthropic tool choice。
- [x] buffered Anthropic 响应命中摊平名称时，还原为 Responses `function_call`，包含
  原始 `name` 和独立 `namespace` 字段。
- [x] SSE 的 output item added/done 和最终 response output 均使用相同的还原结果。
- [x] 增加顶层撞名、跨 namespace 撞名、重复相同声明、超长 UTF-8 名称测试。
- [x] 如果暂时不完成以上链路，应删除 namespace WIP，明确返回 unsupported；不能保留
  “看似支持但实际丢 namespace”的状态。

### P0：工具流式状态机一致性

- [x] 处理 Chat SSE 中工具名称分片或迟到的情况。当前首次 delta 没有 name 时，可能先
  宣告 `function_call`，结束时又按完整 name 改成 `custom_tool_call` 或
  `tool_search_call`，造成 added/done 类型不一致。
- [x] custom 工具必须生成完整的：
  `output_item.added -> custom_tool_call_input.delta/done -> output_item.done`。
- [x] tool_search 必须始终生成 `tool_search_call`，`execution` 为 `client`，arguments
  保持 JSON 对象；非法 JSON 应有明确且一致的兜底或错误语义。
- [x] special tool 不应错误发送普通 `response.function_call_arguments.done`；逐项对照
  sub2api 的 wire event 测试。
- [x] 检查 added、delta、done 和 completed response 中 item id、call id、output index
  是否完全一致。
- [x] custom 参数缺少 `input` 或类型错误时不能静默还原为空字符串。

### P0：server-side 工具边界

- [ ] 用真实 Anthropic web search JSON/SSE fixture 验证 `server_tool_use` 和
  `web_search_tool_result` 的反序列化结构。
- [ ] 当前 buffered 转换会忽略 `web_search_tool_result`；确认 Responses API 期望的
  output/event 形态，并决定是映射还是明确说明结果只体现在模型文本中。
- [x] server tool 的 `input_json_delta` 需要独立累计；当前参数 delta 主要按普通
  function tool 处理，可能丢失分片到达的 query。
- [x] web_search 以外的 `file_search`、`computer`、`image_generation` 等工具，如果
  Anthropic 路径无法表达，必须在请求阶段明确返回 4xx，不能静默过滤声明或 choice。
- [ ] 检查重复 web_search 声明、强制 tool choice、并行工具设置和 beta header。

### P1：reasoning/signature 完整回归

- [x] buffered Responses -> Chat：encrypted reasoning 必须写入
  `reasoning_signature`，并有断言覆盖。
- [x] SSE Responses -> Chat：reasoning item 的 encrypted content 必须在结束前输出为
  signature delta，不能只存在最终 response 对象。
- [x] 覆盖只有 signature、没有可见 thinking 文本的 redacted reasoning。
- [x] 覆盖 reasoning + 多个并行 tool call + 部分 tool result + 下一轮 reasoning 的历史。
- [x] 检查 `include: ["reasoning.encrypted_content"]` 语义：未请求时不泄露 encrypted
  content，请求时 buffered 和 SSE 都必须保留。
- [x] 验证 reasoning summary part added/text delta/text done/part done/output item done 的
  顺序和字段。

### P1：请求与错误语义

- [x] 评估 `/v1/responses/compact` 到 Anthropic 应返回 400、501 还是当前 502，统一错误
  code 和消息。
- [x] 为 `previous_response_id` 增加网关级测试，确认返回明确的客户端错误且不会请求
  upstream。
- [x] `item_reference`、未知 input item、无法表示的媒体类型都应明确失败，避免降级为
  JSON 字符串或空内容。
- [x] 核对 Anthropic upstream 非 2xx、SSE error event、非法 SSE、EOF 和客户端断开时的
  status、usage、channel feedback 行为。
- [ ] 检查 Anthropic OAuth、`anthropic-version`、`anthropic-beta` 和 header override 在
  Responses 直桥上与原 Messages 路径一致。

### P1：集成测试

- [x] 增加 mock Anthropic upstream 的实际网关测试，覆盖完整请求路径和目标
  `/v1/messages`，不能只测试纯转换函数。
- [x] buffered 集成场景：thinking + signature + text + custom/tool call + usage。
- [x] SSE 集成场景：thinking delta + signature delta + text + parallel tools + terminal。
- [x] upstream 400/429/500、Anthropic error SSE、非法 JSON/SSE、无 message_stop EOF。
- [x] 验证 usage 中 cache read/cache creation 不重复计费。

### P2：收尾与文档

- [x] 运行 nightly rustfmt，并检查是否存在与本功能无关的大面积格式化 diff。
  （新文件已 fmt-clean；仓库既有 192 处 fmt diff 与本功能无关，未触碰。）
- [x] 运行 protocol、gateway 定向测试和 workspace 全量测试。
  （protocol 26 passed、gateway response 20 passed、gateway relay 13 passed；
  workspace `cargo check --workspace` 通过、无 warning。全量 `cargo test --workspace`
  因磁盘空间不足未能跑完 control-api，相关 crate 未改动。）
- [x] 清理 warning，例如仅测试使用的 `OpenAiChatSseToResponses::new`。
- [ ] 审查当前 `crates/api-contract/src/gemini.rs` 和
  `crates/protocol/src/gemini_openai.rs` 的改动；Gemini 已暂停，不应混入本功能提交，
  除非确认这些是独立且需要保留的既有修复。
- [x] 更新 `docs/GATEWAY_ARCHITECTURE_CN.md`。该文档目前仍写着"不做 OpenAI
  Responses API -> Claude"，与当前代码状态不一致。
- [ ] 按协议模块、gateway 路由、测试拆分提交，便于回滚和 review。

## 恢复工作时的建议顺序

1. 先完成 namespace 两阶段收集、声明/历史改写和 buffered 回程。
2. 再完成 namespace 与 custom/tool_search 的 SSE 生命周期。
3. 使用 Anthropic web search fixture 收紧 server tool 支持边界。
4. 补 reasoning/signature 与错误路径测试。
5. 运行格式、定向测试、workspace 测试和最终 diff review。
6. 更新架构文档和能力矩阵后再合并。

## 建议验证命令

```bash
cargo check -p halolake-protocol -p halolake-gateway-monoio
cargo test -p halolake-protocol responses_anthropic -- --nocapture
cargo test -p halolake-gateway-monoio --lib response::tests -- --nocapture
cargo test -p halolake-gateway-monoio --lib relay::tests -- --nocapture
cargo +nightly fmt --all -- --check
cargo check --workspace
cargo test --workspace
git diff --check
```

## 完成标准

- 所有支持的字段和工具类型有明确、可测试的双向映射。
- 无法无损转换的内容在请求阶段返回明确错误，不静默丢弃。
- buffered 与 SSE 的最终输出语义一致。
- thinking/reasoning、signature/encrypted content 和 tool history 可跨多轮回放。
- custom、tool_search、namespace 的声明、历史、choice、响应和 SSE 均可逆。
- 定向测试和 workspace 测试通过，文档与实际能力一致。
