# relaykit-rs

[QuantumNous/new-api 的 relaykit](https://github.com/QuantumNous/new-api/tree/d8e65613c09adf3663f960ea4316414a99694751/relaykit) 的纯 Rust 移植：只处理协议 DTO、请求、非流式响应和流式事件转换。

**不包含 HTTP、SSE 读写、渠道选择、模型路由、数据库或异步运行时。** 运行时直接依赖只有 `serde` 和 `serde_json`；构建和使用不需要 Go、FFI 或外部进程。包名为 `relaykit-rs`，Rust 库名为 `relaykit`。

上游兼容基线固定在 `relaykit/v0.3.5`（`d8e65613c09adf3663f960ea4316414a99694751`），见 [UPSTREAM](UPSTREAM)。本项目沿用上游 **AGPL-3.0-only** 许可证，见 [LICENSE](LICENSE)。

### v0.3.5 同步（2026-10-24）

- Chat → Claude 合并相邻 assistant 消息时保留两侧的工具调用，仅去掉程序为无正文工具调用注入的 `...` 占位文本；Claude → Chat 同时保留文本和工具调用。prepared/writer 路径同步这些语义。
- Chat / Claude → Responses 的图片始终带 `detail`：保留并归一化 `low` / `high` / `auto` / `original`，没有有效提示时使用 `auto`，包括 Claude 工具结果中的图片。
- Claude `thinking.block_binding` 和未建模 thinking 字段在原生请求及 DTO 中保留；Responses reasoning item 即使没有摘要文本也输出必需的 `summary: []`。
- DeepSeek 后缀解析支持 `deepseek-flash` 和 `deepseek-v4-*` 的 `-none` / `-low` / `-max`。重新生成 149 个 DTO 的字段定义，包括新增设置字段和 `DeepSeekToolsRequest`；未新增 HTTP 路由、宿主计费或日志脱敏功能。

本次按已发布标签同步，不混入其后 `main` 分支尚未打标签的 JSON Schema 属性顺序和 12 小时缓存计费改动。旧性能报告仍明确对应当时的 v0.3.3，不代表升级后的重新测量。

当前公开 fixture 的 Go v0.3.5 / Rust Release 复测及逐案例数据，见 [升级与性能复测报告](benchmarks/upstream-v0.3.5-2026-10-24/README.md)。

## 转换矩阵

请求、非流式响应、流式响应均支持下列矩阵，以及同协议透传：

| 源 → 目标 | OpenAI Chat | OpenAI Responses | Claude Messages | Gemini |
|---|---|---|---|---|
| OpenAI Chat | — | Good | Fair | Fair |
| OpenAI Responses | Good | — | Fair | Fair |
| Claude Messages | Fair | Fair | — | Discouraged |
| Gemini | Fair | Fair | Discouraged | — |

直接路径和多跳路径与固定版本的上游 registry 一致。特别是 Responses → Claude/Gemini 请求保留直接转换路径，不统一压缩为 Chat 中间格式。响应和流式路径与请求路径不一定相同。

## 请求转换

```rust
use relaykit::{convert_request, Context, Format, Request};
use serde_json::json;

let request = Request::from_value(Format::OpenAi, json!({
    "model": "claude-sonnet-4-5",
    "max_tokens": 1024,
    "messages": [{"role": "user", "content": "Hello!"}]
}))?;

let result = convert_request(&Context::default(), Format::Claude, &request)?;
let json_body = serde_json::to_string(&result.value)?;
let claude: relaykit::dto::ClaudeRequest = result.value.decode()?;
assert_eq!(claude.model, "claude-sonnet-4-5");
# Ok::<(), relaykit::Error>(())
```

也可以从类型化 DTO 构造请求，不必传协议枚举：

```rust
use relaykit::{dto, Request};

let request = Request::try_from(dto::GeneralOpenAIRequest {
    model: "gpt-test".into(),
    max_tokens: Some(1024),
    messages: Some(vec![dto::Message {
        role: "user".into(),
        content: serde_json::json!("Hello!"),
        ..Default::default()
    }]),
    ..Default::default()
})?;
# Ok::<(), relaykit::Error>(())
```

DTO 中的 `Option` 保留未设置与显式零值的差别。`Option<Vec<T>>` 在相应的 wire 字段中保留 `null` 与 `[]` 的差别；实际是否省略取决于上游字段的 `omitempty` 规则。协议的动态联合内容、JSON Schema 和厂商扩展使用 `serde_json::Value`。

`Request` / `Response` 显式关联协议，`Format`、`Quality`、`Severity` 和 `LossPolicy` 使用枚举；错误使用 `Result<T, Error>`。转换内部的动态 JSON 被限制在协议边界和转换模块内，不要求宿主实现 Go 风格的巨大接口或注册全局可变配置。

## 配置与媒体

```rust
use relaykit::Context;
use std::sync::Arc;

let mut context = Context::default();
context.options.claude.default_max_tokens = Some(Arc::new(|_model| 4096));
context.media.get_base64_data = Some(Arc::new(|source, mime| {
    // 由宿主解析 URL / data URL / base64；返回 (base64_data, mime_type)。
    // 本示例仅演示回调签名。
    Ok((source.to_owned(), mime.to_owned()))
}));
```

- 转换成 Claude 时必须有 `max_tokens`，或配置 `default_max_tokens`；缺少配置会返回错误。推理预算可能按上游规则提高 token 上限。
- 将请求模型名预先设置为目标模型；crate 不替宿主进行模型映射。
- `Context` 应按请求创建，并在该请求与对应响应之间复用，以恢复 Responses 自定义工具、命名空间和相关元数据。
- 媒体回调按 `Context` 注入，无全局 resolver；需要解析却未配置回调时返回错误。
- `Options::web_search` 支持宿主自定义 hosted web search 编码；`WebSearchEncoding::native_tool` 保留厂商原生工具 JSON，不额外附加 function 包装。
- `Options::gemini.supports_json_schema` 由宿主按目标模型和 API 版本配置。返回 `true` 时，typed Schema 无法完整表达的工具参数和结构化输出保留为 `parametersJsonSchema` / `responseJsonSchema`；未配置或返回 `false` 时，使用 typed Schema 并报告损耗。`additionalProperties` 和 `$schema` 本身不会触发字段切换，Responses custom tools 保持 typed 参数。
- 工具损耗默认 `LossPolicy::Allow`，返回诊断；`Safe` / `Strict` 仅对请求阶段的工具诊断执行拒绝，普通推理或媒体转换诊断仍返回结果，不会中断响应或流。
- 模型后缀不会隐式改变模型名。需要时显式调用 `reasoning::parse_*`，将 intent 写入 `Context::reasoning_conversion`。
- `reasoning::claude_effort` 返回模型接受的 effort；转换同步 Haiku 5.5、Fable/Mythos 等模型能力。Gemini 2.5 Pro 的 `xhigh` / `max` 使用 32768 thinking budget。

## 非流式响应

```rust
use relaykit::{convert_response, Context, Format, Response};
use serde_json::json;

let response = Response::from_value(Format::Claude, json!({
    "id": "msg_1", "type": "message", "role": "assistant",
    "model": "claude-test", "content": [{"type":"text", "text":"Hello!"}],
    "stop_reason": "end_turn", "usage": {"input_tokens": 4, "output_tokens": 2}
}))?;
let result = convert_response(&Context::default(), Format::OpenAi, &response)?;
assert_eq!(result.value.as_value()["choices"][0]["message"]["content"], "Hello!");
# Ok::<(), relaykit::Error>(())
```

结果包含 `value`、`from`、`to`、`converter`、`quality`、`steps`、`diagnostics` 和统一 `usage`。可以通过 `convert_request_by_id` / `convert_response_by_id` 固定转换器，或通过 `convert_request_via` 指定直接转换步骤。

### 准备协议数据并直接编码

四种协议间的 12 条非流式响应路径都提供类型化转换。Claude 输入的常规字符串借用原始 JSON，Chat / Responses / Gemini 输入使用协议结构体；工具参数、厂商扩展等动态数据在准备时解析。输出借用已准备的数据，canonical usage 用结构体计算，直接序列化时无需创建完整 `Value` 输出树：

```rust
use relaykit::{Context, Format, PreparedResponse, convert_response_to_writer};

let bytes = br#"{"id":"msg_1","content":[{"type":"text","text":"Hello!"}],"usage":{"input_tokens":4,"output_tokens":2}}"#;
let prepared = PreparedResponse::from_slice(Format::Claude, bytes)?;
let mut output = Vec::new();
let metadata = convert_response_to_writer(
    &Context::default(), Format::OpenAi, &prepared, &mut output,
)?;
assert_eq!(metadata.to, Format::OpenAi);
# Ok::<(), relaykit::Error>(())
```

需要先取得结果时调用 `convert_prepared_response`，随后使用 `result.value.write_to(...)` 或 `to_vec()`。`ResponseView::to_value()` 显式生成动态 JSON 树；`ResponseUsage::canonical()` 提供 typed token counters。结果可能借用 prepared source，Rust 生命周期确保输入在结果使用期间仍有效。完整示例见 [examples/typed_response.rs](examples/typed_response.rs)。

类型化路径覆盖普通文本、thinking、工具调用、citations 和 usage。部分 hosted/grounding 扩展、同协议转换，以及需要保留旧解析行为的字段大小写、重复字段等输入使用 Value 兼容路径；可用 `prepared.uses_typed_path(target)` / `result.value.is_typed()` 检查实际路径。既有 `Response` / `convert_response` API 保留。此入口只启用 `serde_json/raw_value` feature，没有新增依赖包。

请求可使用对应的 `PreparedRequest::from_slice`、`convert_prepared_request` 和 `convert_request_to_writer`。准备阶段解析并归一化一次源 `Value`，输出借用其中的历史消息；这与响应的原生类型化解析有所区别。当前借用输出覆盖 Claude → Chat/Responses 和 Responses → Chat/Claude/Gemini 五条路径的常见消息形状；复杂媒体、hosted 工具及其他路径使用已准备源的兼容转换。接口保留媒体回调、工具状态、推理配置、损耗策略和 conversion chain。`RequestView::to_value()` 会显式重新生成输出树，直接转发时优先 `write_to` 或 `to_vec`。示例见 [examples/prepared_paths.rs](examples/prepared_paths.rs)。

## 流式响应

```rust
use relaykit::{Context, Format, ResponseStreamState, StreamOptions};
use serde_json::json;

let context = Context::default();
let mut stream = ResponseStreamState::new(
    Format::OpenAi,
    Format::OpenAiResponses,
    StreamOptions {
        id: "resp_1".into(),
        model: "gpt-test".into(),
        include_usage: true,
        ..Default::default()
    },
)?;
let mut events = stream.convert_chunk(&context, &json!({
    "id": "chatcmpl_1", "model": "gpt-test", "object": "chat.completion.chunk",
    "choices": [{"index":0, "delta":{"role":"assistant", "content":"Hello"}}]
}))?;
events.extend(stream.finalize(&context)?);
let usage = stream.usage();
# Ok::<(), relaykit::Error>(())
```

每条上游流使用独立状态，必须在结束时调用 `finalize`。工具调用参数可以跨 chunk 到达，Claude block 索引、Responses output 索引、usage 和终止事件由状态机维护。同一 Chat chunk 的 thinking 与回答文本均保留；usage 合并保留缓存模态的显式零值，并按模态合并 Gemini 原生计费快照。`emit_sequence_number` 显式启用 Responses 事件序号，默认行为与上游一致。

`PreparedStreamChunk::from_slice` / `from_value` 可以预先校验和归一化单个事件，再由 `ResponseStreamState::convert_prepared_chunk` 使用。Prepared chunk 是不可变的规范化 `Value`，可在不同流状态间复用；它不包含 SSE framing，也不是原生类型化流式解析器。一次性消费事件时，既有 `convert_chunk` 也能获得 owned 输出归一化和 usage 合并的优化。

返回的是可直接编码的协议事件 JSON；Go 内部用于 Responses 的 `Type` / `Payload` 包装不属于 wire payload，不在 Rust 输出中重复包装。SSE `event:` 可从 payload 的 `type` 字段读取。crate 不写入 `[DONE]` 或 SSE framing。

## 兼容验证

测试保留上游原始 golden fixtures，并使用固定版本 Go 实现生成额外对照数据。比较包括 JSON 字段、缺省值、空值、事件顺序、usage 和诊断；规范化随机 ID、时间戳以及 Go 专用的内部事件包装；Gemini 混合工具诊断因 Go map 非确定遍历产生的组索引按语义归一，诊断内容与严重度仍完整比较。JSON 对象键顺序和外层空白不作为协议兼容条件；嵌套 JSON 字符串的转义单独测试。

```sh
cargo test --all-targets
cargo test --doc
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check
```

基础测试包含 **24 份上游 golden**，以及 **1,875 个 Go oracle 对照用例**：

| 测试集 | 用例数 |
|---|---:|
| 基础全矩阵 | 176 |
| 请求边界与嵌套 JSON 编码 | 111 |
| 推理配置与模型能力 | 515 |
| 工具、损耗策略与诊断 | 324 |
| 响应、引用与计费用量 | 136 |
| 流式边界 | 228 |
| v0.3.3 升级与一致性回归 | 312 |
| v0.3.5 升级与 DTO / 后缀回归 | 73 |

Prepared API 另有 29 条响应边界、21 条 Responses 请求文本强制转换和 29 条同协议流式 usage 的 pinned Go fixture，并重放既有协议用例验证新旧 API 的正文、usage、诊断、writer 和上下文行为。

以上共 1,954 个 Go 对照条目已用 v0.3.5 核对。升级时可复用原输入更新期望并复制上游 golden（保持无语义变化的随机 ID、时间戳以及 Gemini map 顺序不变）：

```sh
python3 tools/generate_v035_cases.py /tmp/relaykit-go-oracle
python3 tools/refresh_oracle_fixtures.py /tmp/relaykit-go-oracle \
  --golden-dir /path/to/upstream/relaykit/relayconvert/testdata/golden
```

普通测试完全离线使用已提交 fixtures，不调用 Go 或网络。`tools/` 中的开发工具可从固定上游提交重新生成 DTO 字段定义和对照数据；生成器不参与 `cargo build`。例如在上游 `relaykit/` 模块内构建 oracle，再从本仓库运行：

```sh
# 在固定提交的上游 relaykit/ 目录中：
GOWORK=off go build -o /tmp/relaykit-go-oracle /path/to/relaykit-rs/tools/oracle/main.go
# 在本仓库中：
python3 tools/generate_cases.py /tmp/relaykit-go-oracle
python3 tools/generate_request_cases.py /tmp/relaykit-go-oracle
python3 tools/generate_reasoning_cases.py /tmp/relaykit-go-oracle
python3 tools/generate_tool_cases.py /tmp/relaykit-go-oracle
python3 tools/generate_response_cases.py /tmp/relaykit-go-oracle
python3 tools/generate_stream_cases.py /tmp/relaykit-go-oracle
python3 tools/generate_upgrade_cases.py /tmp/relaykit-go-oracle
```

当前兼容基线是固定提交，不自动声称兼容之后的上游行为变化。新增协议特性时，应先增加 Go oracle 对照，再修改转换实现。

运行本地完整示例：

```sh
cargo run --example convert
```

## 真实对话性能对比

基于本地 Codex / Claude 对话重建输入的 Go/Rust 转换一致性、内存、延迟和吞吐量测量，见 [2026-10-12 基准报告](benchmarks/real-conversations-2026-10-12/README.md)。可复现流程见 [benchmark 工具说明](tools/bench/README.md)。真实对话语料保留在仓库外，报告只包含匿名标签和数值统计。

后续的 [faststr 评估与复制优化报告](benchmarks/faststr-evaluation-2026-10-12/README.md) 对比了关闭全部 features 的 faststr 私有字段候选、普通 String 和借用优化。最终保留 wire 字段缓存、输入 map 借用以及重复 ID 复用；上述私有字段替换未显示额外收益，因此没有增加 runtime 依赖。相同真实语料的完整 JSON 路径速度比优化前的中位数为 1.31×。

另有仅开启安全 `serde` feature 的 [FastStr 原生 JSON 存储实验](benchmarks/faststr-serde-evaluation-2026-10-12/README.md)，分开测量真实消息的解析、共享树复制、直接编码及转回 owned Value。它用于验证 FastStr 的 serde 与共享存储能力，不是完整协议转换的跑分。

针对小型非流式响应，另做了 [分配与 Go/Rust 内部表示审查](benchmarks/small-responses-2026-10-12/README.md)。已保留 hosted extraction 借用和嵌套 JSON 对象移动，12 条真实响应路径的完整 JSON 路径成对速度比改善 1.208×，分配请求量从 67.39 降到 48.56 KiB/次；Go 在小响应上仍明显更快。

首轮 [typed Claude→Chat 实测报告](benchmarks/typed-response-2026-10-12/README.md)：相同4条真实响应的完整JSON路径成对速度比中位数为Go的2.52×、Value API的9.07×，typed组中位数2.75µs。该轮收益需要使用 PreparedResponse/direct writer 入口，数据范围仅限当时测量的路线。

最新的 [全路径优化与完整基准报告](benchmarks/full-matrix-2026-10-12/README.md) 比较 Go、优化前 Rust、优化后 Value 接口和 prepared 接口，覆盖 61 条真实路线、24 条公开矩阵路线及 48 条重建流式场景，分别报告完整 JSON 路径、纯转换、分配和 RSS。
