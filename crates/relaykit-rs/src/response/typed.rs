//! Prepared protocol data and direct wire serialization for cross-format responses.
//! Source lists and target envelopes remain typed; heterogeneous fields use
//! bounded protocol islands and ambiguous inputs retain the Value contract.
#[path = "typed_native.rs"]
mod native;
#[path = "typed_routes.rs"]
mod routes;
use super::{
    citations, hosted,
    typed_claude::{ClaudeBlock, ClaudeInput},
    typed_usage::CanonicalUsage,
};
use crate::{Context, ConversionStep, Diagnostic, Error, Format, Quality, Response};
use serde::{Serialize, Serializer};
use serde_json::Value;
use std::{borrow::Cow, io::Write, sync::OnceLock};

/// A response decoded once from a caller-owned JSON buffer.
///
/// Claude fields borrow unescaped strings. The other protocols prepare native
/// structs once, and output views borrow their decoded fields. Dynamic arguments
/// and annotations remain protocol islands. Duplicate fields, case-folded names,
/// hosted events, grounding metadata, and same-format routes retain the
/// established compatibility representation when a direct route is unavailable.
#[derive(Debug)]
pub struct PreparedResponse<'a> {
    format: Format,
    original: &'a [u8],
    input: PreparedInput<'a>,
}
#[derive(Debug)]
#[allow(
    clippy::large_enum_variant,
    reason = "keep the common prepared source inline without another heap allocation"
)]
enum PreparedInput<'a> {
    Claude {
        source: ClaudeInput<'a>,
        compatibility: OnceLock<Response>,
    },
    Native {
        source: native::NativeInput,
        compatibility: OnceLock<Response>,
    },
    Compatibility(Response),
}
impl<'a> PreparedResponse<'a> {
    pub fn from_slice(format: Format, input: &'a [u8]) -> Result<Self, Error> {
        let prepared = if format == Format::Claude {
            match ClaudeInput::parse(input) {
                Ok(Some(source)) => PreparedInput::Claude {
                    source,
                    compatibility: OnceLock::new(),
                },
                // The Value boundary resolves repeated keys before validation;
                // an earlier invalid duplicate may have been replaced by a
                // valid last value. Preserve that contract on the slow path.
                Ok(None) | Err(_) => PreparedInput::Compatibility(Response::from_value(
                    format,
                    serde_json::from_slice(input)?,
                )?),
            }
        } else {
            match native::NativeInput::parse(format, input) {
                Ok(Some(source)) => PreparedInput::Native {
                    source,
                    compatibility: OnceLock::new(),
                },
                Ok(None) | Err(_) => PreparedInput::Compatibility(Response::from_value(
                    format,
                    serde_json::from_slice(input)?,
                )?),
            }
        };
        Ok(Self {
            format,
            original: input,
            input: prepared,
        })
    }

    pub fn format(&self) -> Format {
        self.format
    }

    /// The unchanged JSON bytes supplied to `from_slice`.
    pub fn original_json(&self) -> &'a [u8] {
        self.original
    }

    /// Whether this prepared source and target use the typed converter.
    pub fn uses_typed_path(&self, target: Format) -> bool {
        if target == self.format {
            return false;
        }
        match &self.input {
            PreparedInput::Claude { source, .. } => {
                target == Format::OpenAi
                    || !source.content.iter().any(is_hosted)
                        && source
                            .content_block
                            .as_deref()
                            .is_none_or(|b| !is_hosted(b))
            }
            PreparedInput::Native { source, .. } => !source.has_hosted(),
            PreparedInput::Compatibility(_) => false,
        }
    }

    fn compatibility(&self) -> Result<&Response, Error> {
        match &self.input {
            PreparedInput::Compatibility(source) => Ok(source),
            PreparedInput::Claude { compatibility, .. }
            | PreparedInput::Native { compatibility, .. } => {
                if let Some(source) = compatibility.get() {
                    return Ok(source);
                }
                let source =
                    Response::from_value(self.format, serde_json::from_slice(self.original)?)?;
                let _ = compatibility.set(source);
                Ok(compatibility
                    .get()
                    .expect("initialized response representation"))
            }
        }
    }
}

/// Accounting returned alongside a prepared conversion. The typed variant
/// copies numeric counters and borrows an already-prepared provider snapshot.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
#[allow(
    clippy::large_enum_variant,
    reason = "typed accounting copies counters and references without allocating"
)]
pub enum ResponseUsage<'a> {
    Canonical(CanonicalUsage<'a>),
    Compatibility(Value),
}
impl<'a> ResponseUsage<'a> {
    pub fn canonical(&self) -> Option<&CanonicalUsage<'a>> {
        match self {
            Self::Canonical(value) => Some(value),
            Self::Compatibility(_) => None,
        }
    }
    pub fn to_value(&self) -> Result<Value, Error> {
        Ok(serde_json::to_value(self)?)
    }
}

/// An owned or borrowed protocol result that serializes directly as its payload.
/// Call `to_value` only when a dynamic JSON tree is required by the caller.
#[derive(Debug)]
pub struct ResponseView<'a> {
    format: Format,
    inner: ResponseViewInner<'a>,
}
#[derive(Debug)]
#[allow(
    clippy::large_enum_variant,
    reason = "the direct writer keeps its borrowed output on the stack"
)]
enum ResponseViewInner<'a> {
    Chat(ChatResponse<'a>),
    Pivot(routes::ChatPivot<'a>),
    Claude(routes::ClaudeResponse<'a>),
    Gemini(routes::GeminiResponse<'a>),
    Responses(routes::ResponsesResponse<'a>),
    Compatibility(Response),
}
impl ResponseView<'_> {
    pub fn format(&self) -> Format {
        self.format
    }
    pub fn is_typed(&self) -> bool {
        !matches!(&self.inner, ResponseViewInner::Compatibility(_))
    }
    pub fn write_to<W: Write>(&self, writer: W) -> Result<(), Error> {
        Ok(serde_json::to_writer(writer, self)?)
    }
    pub fn to_vec(&self) -> Result<Vec<u8>, Error> {
        Ok(serde_json::to_vec(self)?)
    }
    pub fn to_value(&self) -> Result<Value, Error> {
        Ok(serde_json::to_value(self)?)
    }
}
impl Serialize for ResponseView<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match &self.inner {
            ResponseViewInner::Chat(value) => value.serialize(serializer),
            ResponseViewInner::Pivot(value) => value.serialize(serializer),
            ResponseViewInner::Claude(value) => value.serialize(serializer),
            ResponseViewInner::Gemini(value) => value.serialize(serializer),
            ResponseViewInner::Responses(value) => value.serialize(serializer),
            ResponseViewInner::Compatibility(value) => value.serialize(serializer),
        }
    }
}

/// Result metadata for the prepared API. The existing `ConversionResult`
/// continues to expose `Option<Value>` without a new generic parameter.
#[derive(Debug, Serialize)]
pub struct PreparedConversionResult<'a, T> {
    pub value: T,
    pub from: Format,
    pub to: Format,
    pub converter: String,
    pub quality: Option<Quality>,
    pub steps: Vec<ConversionStep>,
    pub diagnostics: Vec<Diagnostic>,
    pub usage: Option<ResponseUsage<'a>>,
    pub stream: bool,
}
pub type PreparedResponseResult<'a> = PreparedConversionResult<'a, ResponseView<'a>>;
pub type ResponseWriteResult<'a> = PreparedConversionResult<'a, ()>;

/// Convert prepared protocol data. Output strings may borrow `input`, so the
/// prepared source remains alive while the result is in use.
pub fn convert_prepared_response<'a>(
    ctx: &Context,
    target: Format,
    input: &'a PreparedResponse<'_>,
) -> Result<PreparedResponseResult<'a>, Error> {
    if input.uses_typed_path(target) {
        let (inner, usage, diagnostics) = match &input.input {
            PreparedInput::Claude { source, .. } => {
                let (chat, usage, diagnostics) = claude_to_chat(source, ctx);
                if target == Format::OpenAi {
                    (ResponseViewInner::Chat(chat), usage, diagnostics)
                } else {
                    let accounting = Some(chat.usage.through_chat());
                    let pivot = routes::ChatPivot::from(chat);
                    let value = match target {
                        Format::OpenAiResponses => {
                            ResponseViewInner::Responses(routes::chat_responses(pivot, ctx)?)
                        }
                        Format::Gemini => ResponseViewInner::Gemini(routes::chat_gemini(pivot)),
                        _ => unreachable!(),
                    };
                    let diagnostics = retarget_diagnostics(diagnostics, target);
                    (value, accounting, diagnostics)
                }
            }
            PreparedInput::Native { source, .. } => {
                let (pivot, usage) = match source {
                    native::NativeInput::Chat(source, present) => (
                        routes::native_chat(source),
                        if source.object == "chat.completion.chunk" && !present {
                            None
                        } else {
                            Some(CanonicalUsage::from_openai(&source.usage, false))
                        },
                    ),
                    native::NativeInput::Responses(source) => {
                        let accounting = source
                            .usage
                            .as_deref()
                            .map(|u| CanonicalUsage::from_openai(u, true))
                            .unwrap_or_else(|| CanonicalUsage::from_claude(None));
                        if target == Format::Claude {
                            let metadata =
                                crate::make_result((), input.format, target, true, vec![], None);
                            return Ok(PreparedConversionResult {
                                value: ResponseView {
                                    format: target,
                                    inner: ResponseViewInner::Claude(routes::responses_claude(
                                        source,
                                    )),
                                },
                                from: metadata.from,
                                to: metadata.to,
                                converter: metadata.converter,
                                quality: metadata.quality,
                                steps: metadata.steps,
                                diagnostics: metadata.diagnostics,
                                usage: Some(ResponseUsage::Canonical(accounting)),
                                stream: false,
                            });
                        }
                        (
                            routes::responses_chat(source)?,
                            Some(if target == Format::OpenAi {
                                accounting
                            } else {
                                accounting.through_chat()
                            }),
                        )
                    }
                    native::NativeInput::Gemini(source) => {
                        let accounting = CanonicalUsage::from_gemini(
                            source.usage_metadata.as_ref(),
                            ctx.estimate_prompt_tokens,
                        );
                        (
                            routes::gemini_chat(source, ctx)?,
                            if target == Format::OpenAi {
                                accounting
                            } else {
                                Some(
                                    accounting
                                        .unwrap_or_else(|| CanonicalUsage::from_claude(None))
                                        .through_chat(),
                                )
                            },
                        )
                    }
                };
                let value = match target {
                    Format::OpenAi => ResponseViewInner::Pivot(pivot),
                    Format::Claude => ResponseViewInner::Claude(routes::chat_claude(pivot)),
                    Format::Gemini => ResponseViewInner::Gemini(routes::chat_gemini(pivot)),
                    Format::OpenAiResponses => {
                        ResponseViewInner::Responses(routes::chat_responses(pivot, ctx)?)
                    }
                };
                (value, usage, vec![])
            }
            PreparedInput::Compatibility(_) => unreachable!(),
        };
        let metadata = crate::make_result((), input.format, target, true, diagnostics, None);
        return Ok(PreparedConversionResult {
            value: ResponseView {
                format: target,
                inner,
            },
            from: metadata.from,
            to: metadata.to,
            converter: metadata.converter,
            quality: metadata.quality,
            steps: metadata.steps,
            diagnostics: metadata.diagnostics,
            usage: usage.map(ResponseUsage::Canonical),
            stream: metadata.stream,
        });
    }
    let converted = crate::convert_response(ctx, target, input.compatibility()?)?;
    Ok(PreparedConversionResult {
        value: ResponseView {
            format: target,
            inner: ResponseViewInner::Compatibility(converted.value),
        },
        from: converted.from,
        to: converted.to,
        converter: converted.converter,
        quality: converted.quality,
        steps: converted.steps,
        diagnostics: converted.diagnostics,
        usage: converted.usage.map(ResponseUsage::Compatibility),
        stream: converted.stream,
    })
}

/// Convert and write just the wire payload, returning its accounting and route
/// metadata. A caller-owned `Vec<u8>` can be cleared and reused between calls.
///
/// ```
/// use relaykit::{Context, Format, PreparedResponse, convert_response_to_writer};
/// let json = br#"{"id":"msg_1","content":[{"type":"text","text":"Hello"}]}"#;
/// let input = PreparedResponse::from_slice(Format::Claude, json)?;
/// let mut bytes = Vec::new();
/// let metadata = convert_response_to_writer(&Context::default(), Format::OpenAi, &input, &mut bytes)?;
/// assert_eq!(metadata.to, Format::OpenAi);
/// # Ok::<(), relaykit::Error>(())
/// ```
pub fn convert_response_to_writer<'a, W: Write>(
    ctx: &Context,
    target: Format,
    input: &'a PreparedResponse<'_>,
    writer: W,
) -> Result<ResponseWriteResult<'a>, Error> {
    let result = convert_prepared_response(ctx, target, input)?;
    result.value.write_to(writer)?;
    Ok(PreparedConversionResult {
        value: (),
        from: result.from,
        to: result.to,
        converter: result.converter,
        quality: result.quality,
        steps: result.steps,
        diagnostics: result.diagnostics,
        usage: result.usage,
        stream: result.stream,
    })
}

#[derive(Debug, Serialize)]
struct ChatResponse<'a> {
    id: &'a str,
    model: &'a str,
    object: &'static str,
    created: i64,
    choices: [ChatChoice<'a>; 1],
    usage: CanonicalUsage<'a>,
}
#[derive(Debug, Serialize)]
struct ChatChoice<'a> {
    index: i64,
    message: ChatMessage<'a>,
    finish_reason: &'a str,
}
#[derive(Debug, Serialize)]
struct ChatMessage<'a> {
    role: &'static str,
    content: Cow<'a, str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_content: Option<&'a str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<ChatToolCall<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    annotations: Vec<Value>,
}
#[derive(Debug, Serialize)]
struct ChatToolCall<'a> {
    #[serde(skip_serializing_if = "str::is_empty")]
    id: &'a str,
    #[serde(rename = "type")]
    kind: &'static str,
    function: ChatFunction<'a>,
}
#[derive(Debug, Serialize)]
struct ChatFunction<'a> {
    #[serde(skip_serializing_if = "str::is_empty")]
    name: &'a str,
    arguments: String,
}

fn is_hosted(block: &ClaudeBlock<'_>) -> bool {
    block.is_hosted()
}
fn stop_reason(reason: &str) -> &str {
    // Known spellings are ASCII; keep the original unrecognized value.
    let folded;
    let comparable = if reason.is_ascii() {
        reason
    } else {
        folded = reason.to_lowercase();
        &folded
    };
    if comparable.eq_ignore_ascii_case("end_turn")
        || comparable.eq_ignore_ascii_case("stop_sequence")
    {
        "stop"
    } else if comparable.eq_ignore_ascii_case("max_tokens")
        || comparable.eq_ignore_ascii_case("pause_turn")
    {
        "length"
    } else if comparable.eq_ignore_ascii_case("tool_use") {
        "tool_calls"
    } else if comparable.eq_ignore_ascii_case("refusal") {
        "content_filter"
    } else {
        reason
    }
}
fn append_text<'a>(content: &mut Cow<'a, str>, text: &'a str) {
    if text.is_empty() {
        return;
    }
    if content.is_empty() {
        *content = Cow::Borrowed(text);
    } else {
        content.to_mut().push_str(text);
    }
}
fn claude_to_chat<'a>(
    source: &'a ClaudeInput<'_>,
    ctx: &Context,
) -> (
    ChatResponse<'a>,
    Option<CanonicalUsage<'a>>,
    Vec<Diagnostic>,
) {
    let native_usage = source.usage.as_ref().or_else(|| {
        source
            .message
            .as_deref()
            .and_then(|message| message.usage.as_ref())
    });
    let canonical = CanonicalUsage::from_claude(native_usage);
    let metadata_usage = native_usage.map(|_| canonical.clone());
    let mut text = Cow::Borrowed("");
    let mut offset = 0;
    let mut last_thinking = None;
    let mut first = None;
    let mut calls = Vec::new();
    let mut annotations = Vec::new();
    for block in &source.content {
        if is_hosted(block) {
            continue;
        }
        if first.is_none() {
            first = Some(block);
        }
        match block.kind.as_ref() {
            "text" => {
                let part = block.text.as_deref().unwrap_or("");
                if let Some(raw) = block.citations {
                    if let Ok(value) = serde_json::from_str(raw.get()) {
                        annotations.extend(citations::claude_to_chat(&value, part, offset));
                    }
                }
                offset += part.chars().count();
                append_text(&mut text, part);
            }
            "thinking" => {
                if let Some(value) = block.thinking.as_deref() {
                    last_thinking = Some(value);
                }
            }
            "tool_use" => calls.push(ChatToolCall {
                id: &block.id,
                kind: "function",
                function: ChatFunction {
                    name: &block.name,
                    arguments: crate::json::stringify_interface(
                        block.input.as_ref().unwrap_or(&Value::Null),
                    ),
                },
            }),
            _ => {}
        }
    }
    let thinking = last_thinking.filter(|value| !value.is_empty()).or_else(|| {
        first
            .and_then(|block| block.thinking.as_deref())
            .filter(|value| !value.is_empty())
    });
    let response = ChatResponse {
        id: &source.id,
        model: &source.model,
        object: "chat.completion",
        created: if ctx.created == 0 {
            super::now()
        } else {
            ctx.created
        },
        choices: [ChatChoice {
            index: 0,
            finish_reason: stop_reason(&source.stop_reason),
            message: ChatMessage {
                role: "assistant",
                content: text,
                reasoning_content: thinking,
                tool_calls: calls,
                annotations,
            },
        }],
        usage: canonical,
    };
    (response, metadata_usage, diagnostics(source))
}

fn diagnostics(source: &ClaudeInput<'_>) -> Vec<Diagnostic> {
    let mut result = Vec::new();
    if source.stop_reason == "pause_turn"
        || source
            .delta
            .as_deref()
            .and_then(|block| block.stop_reason.as_deref())
            == Some("pause_turn")
    {
        result.push(hosted::diagnostic(Format::Claude, Format::OpenAi, "stop_reason", "continuation_state_lost",
            "Claude pause_turn requires protocol-native continuation state that the target response cannot preserve", false));
    }
    for (index, block) in source.content.iter().enumerate() {
        inspect_block(block, || format!("content[{index}]"), &mut result);
    }
    if let Some(block) = source.content_block.as_deref() {
        inspect_block(block, || "content_block".to_owned(), &mut result);
    }
    let mut hosted_index = 0;
    for block in &source.content {
        if is_hosted(block) {
            result.push(hosted::diagnostic(
                Format::Claude,
                Format::OpenAi,
                format!("hosted_tools[{hosted_index}]"),
                "hosted_tool_event_omitted",
                format!(
                    "openai cannot represent hosted-tool response {:?}",
                    block.kind
                ),
                true,
            ));
            hosted_index += 1;
        }
    }
    result
}
fn inspect_block(block: &ClaudeBlock<'_>, path: impl Fn() -> String, result: &mut Vec<Diagnostic>) {
    let kind = block.kind.trim();
    if is_hosted(block) {
        result.push(hosted::diagnostic(
            Format::Claude,
            Format::OpenAi,
            path(),
            "hosted_tool_unrepresentable",
            format!(
                "openai cannot losslessly represent Claude hosted-tool response block {kind:?}"
            ),
            false,
        ));
        if kind == "mcp_tool_result" {
            let content = block.content.as_ref().unwrap_or(&Value::Null);
            let code = block.error_code.trim();
            let nested_code = super::s(&content["error_code"]).trim();
            let (failed, code) = if block.is_error == Some(true) || !code.is_empty() {
                (true, code)
            } else if content.is_object()
                && (super::s(&content["type"]).trim().ends_with("_error")
                    || !nested_code.is_empty())
            {
                (true, nested_code)
            } else {
                (false, "")
            };
            match hosted::mcp_output(content, if failed { code } else { "" }) {
                Err(error) => result.push(hosted::diagnostic(
                    Format::Claude,
                    Format::OpenAi,
                    format!("{}.content", path()),
                    "mcp_result_unrepresentable",
                    error.to_string(),
                    false,
                )),
                Ok((_, true)) => result.push(hosted::diagnostic(
                    Format::Claude,
                    Format::OpenAi,
                    format!("{}.content", path()),
                    "mcp_text_result_normalized",
                    "Claude's single MCP text block is normalized to a Responses output string",
                    true,
                )),
                _ => {}
            }
        }
    }
    if kind == "redacted_thinking" && !block.data.is_empty() {
        result.push(hosted::diagnostic(
            Format::Claude,
            Format::OpenAi,
            format!("{}.data", path()),
            "continuation_state_lost",
            "Claude encrypted thinking state cannot be represented by the target response",
            false,
        ));
    }
}

fn retarget_diagnostics(mut values: Vec<Diagnostic>, target: Format) -> Vec<Diagnostic> {
    if target == Format::OpenAiResponses {
        values.retain(|d| !(d.path == "stop_reason" && d.code == "continuation_state_lost"));
    }
    for diagnostic in &mut values {
        diagnostic.to = target;
        diagnostic.message = diagnostic
            .message
            .replace("openai cannot", &format!("{target} cannot"));
    }
    values
}
