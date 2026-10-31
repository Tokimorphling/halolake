//! Direct target structures and the typed Chat pivot used by composed routes.
use super::super::{
    citations,
    typed_claude::coerce_go_numbers,
    typed_usage::{CanonicalUsage, ClaudeDisplayUsage, GeminiDisplayUsage},
};
use super::{ChatResponse, native::GeminiInput};
use crate::{Context, Error, dto};
use serde::Serialize;
use serde_json::{Value, json};
use std::borrow::Cow;
#[derive(Debug, Serialize)]
#[serde(untagged)]
enum Annotations<'a> {
    Raw(&'a Value),
    Owned(Vec<Value>),
}
impl<'a> Annotations<'a> {
    fn is_empty(&self) -> bool {
        match self {
            Self::Raw(v) => v.is_null() || v.as_array().is_some_and(Vec::is_empty),
            Self::Owned(v) => v.is_empty(),
        }
    }
    fn into_value(self) -> Cow<'a, Value> {
        match self {
            Self::Raw(v) => Cow::Borrowed(v),
            Self::Owned(v) => Cow::Owned(Value::Array(v)),
        }
    }
}
fn cow_empty(value: &str) -> bool {
    value.is_empty()
}

#[derive(Debug, Serialize)]
pub(super) struct ChatPivot<'a> {
    id: Cow<'a, str>,
    model: Cow<'a, str>,
    object: &'static str,
    created: i64,
    choices: Vec<Choice<'a>>,
    usage: CanonicalUsage<'a>,
}
#[derive(Debug, Serialize)]
struct Choice<'a> {
    index: i64,
    message: Message<'a>,
    finish_reason: Cow<'a, str>,
}
#[derive(Debug, Serialize)]
struct Message<'a> {
    role: &'static str,
    content: Cow<'a, str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_content: Option<Cow<'a, str>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<Call<'a>>,
    #[serde(skip_serializing_if = "Annotations::is_empty")]
    annotations: Annotations<'a>,
}
#[derive(Debug, Serialize)]
struct Call<'a> {
    #[serde(skip_serializing_if = "cow_empty")]
    id: Cow<'a, str>,
    #[serde(rename = "type")]
    kind: &'a str,
    function: Function<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    custom: Option<&'a Value>,
}
#[derive(Debug, Serialize)]
struct Function<'a> {
    #[serde(skip_serializing_if = "str::is_empty")]
    name: &'a str,
    arguments: Cow<'a, str>,
}
impl<'a> From<ChatResponse<'a>> for ChatPivot<'a> {
    fn from(source: ChatResponse<'a>) -> Self {
        Self {
            id: Cow::Borrowed(source.id),
            model: Cow::Borrowed(source.model),
            object: source.object,
            created: source.created,
            choices: source
                .choices
                .into_iter()
                .map(|c| Choice {
                    index: c.index,
                    finish_reason: Cow::Borrowed(c.finish_reason),
                    message: Message {
                        role: c.message.role,
                        content: c.message.content,
                        reasoning_content: c.message.reasoning_content.map(Cow::Borrowed),
                        tool_calls: c
                            .message
                            .tool_calls
                            .into_iter()
                            .map(|c| Call {
                                id: Cow::Borrowed(c.id),
                                kind: c.kind,
                                function: Function {
                                    name: c.function.name,
                                    arguments: Cow::Owned(c.function.arguments),
                                },
                                custom: None,
                            })
                            .collect(),
                        annotations: Annotations::Owned(c.message.annotations),
                    },
                })
                .collect(),
            usage: source.usage,
        }
    }
}
fn text_content(value: &Value) -> Cow<'_, str> {
    if let Some(text) = value.as_str() {
        return Cow::Borrowed(text);
    }
    let mut output = Cow::Borrowed("");
    for part in super::super::arr(value) {
        if super::super::s(&part["type"]) == "text" {
            append(&mut output, super::super::s(&part["text"]));
        }
    }
    output
}
fn append<'a>(output: &mut Cow<'a, str>, part: &'a str) {
    if part.is_empty() {
        return;
    }
    if output.is_empty() {
        *output = Cow::Borrowed(part);
    } else {
        output.to_mut().push_str(part);
    }
}
fn separated<'a>(output: &mut Cow<'a, str>, part: &'a str) {
    if part.is_empty() {
        return;
    }
    if output.is_empty() {
        *output = Cow::Borrowed(part);
    } else {
        super::super::append_separated(output.to_mut(), part);
    }
}
fn raw_calls(value: Option<&Value>) -> Vec<Call<'_>> {
    super::super::arr(value.unwrap_or(&Value::Null))
        .iter()
        .map(|call| Call {
            id: Cow::Borrowed(super::super::s(&call["id"])),
            kind: super::super::s(&call["type"]),
            function: Function {
                name: super::super::s(&call["function"]["name"]),
                arguments: Cow::Borrowed(super::super::s(&call["function"]["arguments"])),
            },
            custom: call.get("custom").filter(|v| !v.is_null()),
        })
        .collect()
}
fn message(source: &dto::Message) -> Message<'_> {
    Message {
        role: "assistant",
        content: text_content(&source.content),
        reasoning_content: source
            .reasoning_content
            .as_deref()
            .or(source.reasoning.as_deref())
            .filter(|v| !v.is_empty())
            .map(Cow::Borrowed),
        tool_calls: raw_calls(source.tool_calls.as_ref()),
        annotations: Annotations::Raw(source.annotations.as_ref().unwrap_or(&Value::Null)),
    }
}
pub(super) fn native_chat(source: &dto::OpenAITextResponse) -> ChatPivot<'_> {
    ChatPivot {
        id: Cow::Borrowed(&source.id),
        model: Cow::Borrowed(&source.model),
        object: "chat.completion",
        created: if source.created.is_number() {
            super::super::n(&source.created)
        } else {
            source
                .created
                .as_str()
                .and_then(|s| s.parse::<i64>().ok())
                .filter(|v| *v != 0)
                .unwrap_or_else(super::super::now)
        },
        choices: source
            .choices
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .map(|c| Choice {
                index: c.index,
                message: message(&c.message),
                finish_reason: Cow::Borrowed(&c.finish_reason),
            })
            .collect(),
        usage: CanonicalUsage::normalized_openai(&source.usage),
    }
}
fn output_reasoning(source: &dto::ResponsesOutput) -> Cow<'_, str> {
    let content = source.content.as_deref().unwrap_or(&[]);
    let mut output = Cow::Borrowed("");
    if content.iter().any(|p| !p.text.is_empty()) {
        for p in content {
            separated(&mut output, &p.text);
        }
    } else {
        for p in source.summary.as_deref().unwrap_or(&[]) {
            separated(&mut output, &p.text);
        }
    }
    output
}
fn responses_text(source: &dto::OpenAIResponsesResponse) -> Cow<'_, str> {
    let mut output = Cow::Borrowed("");
    for item in source.output.as_deref().unwrap_or(&[]) {
        if item.r#type == "message" && (item.role.is_empty() || item.role == "assistant") {
            let mut text = Cow::Borrowed("");
            for part in item.content.as_deref().unwrap_or(&[]) {
                if part.r#type == "output_text" {
                    append(&mut text, &part.text);
                }
            }
            if !text.is_empty() {
                if output.is_empty() {
                    output = text;
                } else {
                    super::super::append_separated(output.to_mut(), &text);
                }
            }
        }
    }
    if output.is_empty() {
        for item in source.output.as_deref().unwrap_or(&[]) {
            let mut text = Cow::Borrowed("");
            for part in item.content.as_deref().unwrap_or(&[]) {
                append(&mut text, &part.text);
            }
            if !text.is_empty() {
                if output.is_empty() {
                    output = text;
                } else {
                    super::super::append_separated(output.to_mut(), &text);
                }
            }
        }
    }
    output
}
fn responses_finish(source: &dto::OpenAIResponsesResponse, tools: bool) -> &'static str {
    if source
        .status
        .as_ref()
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        == "incomplete"
    {
        if source
            .incomplete_details
            .as_ref()
            .is_some_and(|v| v.reason.trim() == "content_filter")
        {
            "content_filter"
        } else {
            "length"
        }
    } else if tools {
        "tool_calls"
    } else {
        "stop"
    }
}
fn stringify(value: Option<&Value>) -> Cow<'_, str> {
    match value {
        Some(Value::String(text)) => Cow::Borrowed(text),
        Some(Value::Null) | None => Cow::Borrowed(""),
        Some(value) => Cow::Owned(value.to_string()),
    }
}
pub(super) fn responses_chat(
    source: &dto::OpenAIResponsesResponse,
) -> Result<ChatPivot<'_>, Error> {
    let mut reasoning = Cow::Borrowed("");
    let mut calls = Vec::new();
    let mut annotations = Vec::new();
    for item in source.output.as_deref().unwrap_or(&[]) {
        match item.r#type.as_str() {
            "reasoning" => {
                let part = output_reasoning(item);
                if !part.is_empty() {
                    if reasoning.is_empty() {
                        reasoning = part;
                    } else {
                        super::super::append_separated(reasoning.to_mut(), &part);
                    }
                }
            }
            "function_call" | "custom_tool_call" => {
                if !item.name.trim().is_empty() {
                    calls.push(Call {
                        id: Cow::Borrowed(if item.call_id.trim().is_empty() {
                            item.id.trim()
                        } else {
                            item.call_id.trim()
                        }),
                        kind: "function",
                        function: Function {
                            name: item.name.trim(),
                            arguments: stringify(item.arguments.as_ref()),
                        },
                        custom: None,
                    });
                }
            }
            "message" => {
                for part in item.content.as_deref().unwrap_or(&[]) {
                    for annotation in part.annotations.as_deref().unwrap_or(&[]) {
                        annotations.push(citations::response_annotation_to_chat(annotation)?);
                    }
                }
            }
            _ => {}
        }
    }
    let finish = responses_finish(source, !calls.is_empty());
    Ok(ChatPivot {
        id: if source.id.trim().is_empty() {
            Cow::Owned(crate::new_id("chatcmpl-"))
        } else {
            Cow::Borrowed(source.id.trim())
        },
        model: Cow::Borrowed(&source.model),
        object: "chat.completion",
        created: source.created_at.0,
        choices: vec![Choice {
            index: 0,
            finish_reason: Cow::Borrowed(finish),
            message: Message {
                role: "assistant",
                content: responses_text(source),
                reasoning_content: (!reasoning.is_empty()).then_some(reasoning),
                tool_calls: calls,
                annotations: Annotations::Owned(annotations),
            },
        }],
        usage: source
            .usage
            .as_deref()
            .map(|v| CanonicalUsage::from_openai(v, true))
            .unwrap_or_else(|| CanonicalUsage::from_claude(None)),
    })
}
pub(super) fn gemini_chat<'a>(
    source: &'a GeminiInput,
    ctx: &Context,
) -> Result<ChatPivot<'a>, Error> {
    let mut choices = Vec::new();
    let mut has_calls = false;
    for candidate in source.candidates.as_deref().unwrap_or(&[]) {
        let mut pieces = Vec::new();
        let mut calls = Vec::new();
        let mut reasoning = None;
        for part in candidate.content.parts.as_deref().unwrap_or(&[]) {
            if let Some(media) = &part.inline_data {
                pieces.push(Cow::Owned(format!(
                    "{}(data:{};base64,{})",
                    if media.mime_type.starts_with("image") {
                        "![image]"
                    } else {
                        "[media]"
                    },
                    media.mime_type,
                    media.data
                )));
            } else if let Some(call) = &part.function_call {
                calls.push(Call {
                    id: if call.id.trim().is_empty() {
                        Cow::Owned(crate::new_id("call_"))
                    } else {
                        Cow::Borrowed(call.id.trim())
                    },
                    kind: "function",
                    function: Function {
                        name: &call.name,
                        arguments: Cow::Owned(crate::json::stringify(&call.args)),
                    },
                    custom: None,
                });
            } else if part.thought {
                reasoning = Some(Cow::Borrowed(part.text.as_str()));
            } else if let Some(code) = &part.executable_code {
                pieces.push(Cow::Owned(format!(
                    "```{}\n{}\n```",
                    code.language, code.code
                )));
            } else if let Some(code) = &part.code_execution_result {
                pieces.push(Cow::Owned(format!("```output\n{}\n```", code.output)));
            } else if part.text != "\n" {
                pieces.push(Cow::Borrowed(part.text.as_str()));
            }
        }
        let text = match pieces.len() {
            0 => Cow::Borrowed(""),
            1 => pieces.pop().unwrap(),
            _ => Cow::Owned(
                pieces
                    .iter()
                    .map(|v| v.as_ref())
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
        };
        if !calls.is_empty() {
            has_calls = true;
        }
        let annotations = if let Some(grounding) = &candidate.grounding_metadata {
            // Grounding is a protocol-specific island, prepared only when present.
            let metadata = serde_json::to_value(grounding)?;
            let parts = Value::Array(
                candidate
                    .content
                    .parts
                    .as_deref()
                    .unwrap_or(&[])
                    .iter()
                    .map(|p| json!({"text":p.text,"thought":p.thought}))
                    .collect(),
            );
            citations::gemini_to_chat(&metadata, &json!({"parts":parts}), &text)
        } else {
            Vec::new()
        };
        let finish = if has_calls {
            "tool_calls"
        } else {
            match candidate.finish_reason.as_deref() {
                None | Some("STOP") => "stop",
                Some("MAX_TOKENS") => "length",
                _ => "content_filter",
            }
        };
        choices.push(Choice {
            index: candidate.index,
            finish_reason: Cow::Borrowed(finish),
            message: Message {
                role: "assistant",
                content: text,
                reasoning_content: reasoning,
                tool_calls: calls,
                annotations: Annotations::Owned(annotations),
            },
        });
    }
    Ok(ChatPivot {
        id: if ctx.response_id.is_empty() {
            Cow::Owned(crate::new_id("chatcmpl-"))
        } else {
            Cow::Owned(ctx.response_id.clone())
        },
        model: if ctx.channel_meta_attached {
            Cow::Owned(ctx.upstream_model_name.clone())
        } else {
            Cow::Borrowed("")
        },
        object: "chat.completion",
        created: if ctx.created == 0 {
            super::super::now()
        } else {
            ctx.created
        },
        choices,
        usage: CanonicalUsage::from_gemini(
            source.usage_metadata.as_ref(),
            ctx.estimate_prompt_tokens,
        )
        .unwrap_or_else(|| CanonicalUsage::from_claude(None)),
    })
}

#[derive(Debug, Serialize)]
pub(super) struct ClaudeResponse<'a> {
    #[serde(skip_serializing_if = "cow_empty")]
    id: Cow<'a, str>,
    #[serde(rename = "type")]
    kind: &'static str,
    role: &'static str,
    #[serde(skip_serializing_if = "cow_empty")]
    model: Cow<'a, str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    content: Vec<ClaudeBlock<'a>>,
    #[serde(skip_serializing_if = "cow_empty")]
    stop_reason: Cow<'a, str>,
    usage: ClaudeDisplayUsage<'a>,
}
#[derive(Debug, Serialize)]
#[serde(untagged)]
enum ClaudeBlock<'a> {
    Text {
        #[serde(rename = "type")]
        kind: &'static str,
        text: Cow<'a, str>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        citations: Vec<Value>,
    },
    Thinking {
        #[serde(rename = "type")]
        kind: &'static str,
        thinking: Cow<'a, str>,
    },
    Tool {
        #[serde(rename = "type")]
        kind: &'static str,
        #[serde(skip_serializing_if = "cow_empty")]
        id: Cow<'a, str>,
        #[serde(skip_serializing_if = "str::is_empty")]
        name: &'a str,
        input: Value,
    },
}
fn parsed_arguments(raw: &str, wrap: bool) -> Value {
    let mut input = serde_json::from_str::<Value>(raw)
        .ok()
        .filter(Value::is_object)
        .unwrap_or_else(|| {
            if wrap && !raw.trim().is_empty() {
                json!({"input":raw})
            } else {
                json!({})
            }
        });
    if wrap {
        coerce_go_numbers(&mut input);
    } else {
        input.as_object_mut().unwrap().sort_keys();
    }
    input
}
pub(super) fn chat_claude(source: ChatPivot<'_>) -> ClaudeResponse<'_> {
    let mut content = Vec::new();
    let mut stop = Cow::Borrowed("");
    for choice in source.choices {
        stop = Cow::Owned(super::super::claude_stop(&choice.finish_reason));
        let message = choice.message;
        let reasoning = message.reasoning_content.filter(|v| !v.is_empty());
        if let Some(reasoning) = reasoning.as_ref() {
            content.push(ClaudeBlock::Thinking {
                kind: "thinking",
                thinking: reasoning.clone(),
            });
        }
        if !message.content.is_empty() || (reasoning.is_none() && message.tool_calls.is_empty()) {
            let annotations = message.annotations.into_value();
            let cs = citations::to_claude(&annotations, &message.content);
            content.push(ClaudeBlock::Text {
                kind: "text",
                text: message.content,
                citations: cs,
            });
        }
        for call in message.tool_calls {
            content.push(ClaudeBlock::Tool {
                kind: "tool_use",
                id: call.id,
                name: call.function.name,
                input: parsed_arguments(&call.function.arguments, false),
            });
        }
    }
    ClaudeResponse {
        id: source.id,
        kind: "message",
        role: "assistant",
        model: source.model,
        content,
        stop_reason: stop,
        usage: source.usage.claude_display(),
    }
}
pub(super) fn responses_claude(source: &dto::OpenAIResponsesResponse) -> ClaudeResponse<'_> {
    let mut content = Vec::new();
    let mut tools = false;
    for item in source.output.as_deref().unwrap_or(&[]) {
        match item.r#type.as_str() {
            "reasoning" => {
                let text = output_reasoning(item);
                if !text.is_empty() {
                    content.push(ClaudeBlock::Thinking {
                        kind: "thinking",
                        thinking: text,
                    });
                }
            }
            "message" => {
                if !item.role.is_empty() && item.role != "assistant" {
                    continue;
                }
                for part in item.content.as_deref().unwrap_or(&[]) {
                    if part.r#type != "output_text" {
                        continue;
                    }
                    let annotations = Value::Array(part.annotations.clone().unwrap_or_default());
                    content.push(ClaudeBlock::Text {
                        kind: "text",
                        text: Cow::Borrowed(&part.text),
                        citations: citations::to_claude(&annotations, &part.text),
                    });
                }
            }
            "function_call" | "custom_tool_call" => {
                tools = true;
                let args = stringify(item.arguments.as_ref());
                content.push(ClaudeBlock::Tool {
                    kind: "tool_use",
                    id: Cow::Borrowed(if item.call_id.trim().is_empty() {
                        item.id.trim()
                    } else {
                        item.call_id.trim()
                    }),
                    name: &item.name,
                    input: parsed_arguments(&args, true),
                });
            }
            _ => {}
        }
    }
    if content.is_empty() {
        content.push(ClaudeBlock::Text {
            kind: "text",
            text: Cow::Borrowed(""),
            citations: Vec::new(),
        });
    }
    ClaudeResponse {
        id: Cow::Borrowed(&source.id),
        kind: "message",
        role: "assistant",
        model: Cow::Borrowed(&source.model),
        content,
        stop_reason: Cow::Owned(super::super::claude_stop(responses_finish(source, tools))),
        usage: source
            .usage
            .as_deref()
            .map(|v| CanonicalUsage::from_openai(v, true))
            .unwrap_or_else(|| CanonicalUsage::from_claude(None))
            .claude_display(),
    }
}
#[derive(Debug, Serialize)]
pub(super) struct GeminiResponse<'a> {
    candidates: Vec<GeminiCandidate<'a>>,
    #[serde(rename = "usageMetadata")]
    usage: GeminiDisplayUsage<'a>,
}
#[derive(Debug, Serialize)]
struct GeminiCandidate<'a> {
    index: i64,
    content: GeminiContent<'a>,
    #[serde(rename = "finishReason")]
    finish_reason: &'static str,
    #[serde(rename = "safetyRatings")]
    safety_ratings: [(); 0],
}
#[derive(Debug, Serialize)]
struct GeminiContent<'a> {
    role: &'static str,
    parts: Vec<GeminiPart<'a>>,
}
#[derive(Debug, Serialize)]
#[serde(untagged)]
enum GeminiPart<'a> {
    Text {
        #[serde(skip_serializing_if = "cow_empty")]
        text: Cow<'a, str>,
    },
    Call {
        #[serde(rename = "functionCall")]
        call: GeminiFunction<'a>,
    },
}
#[derive(Debug, Serialize)]
struct GeminiFunction<'a> {
    name: &'a str,
    args: Value,
    #[serde(skip_serializing_if = "cow_empty")]
    id: Cow<'a, str>,
}
pub(super) fn chat_gemini(source: ChatPivot<'_>) -> GeminiResponse<'_> {
    let mut candidates = Vec::with_capacity(source.choices.len());
    for choice in source.choices {
        let mut parts = Vec::new();
        if !choice.message.content.is_empty() {
            parts.push(GeminiPart::Text {
                text: choice.message.content,
            });
        }
        for call in choice.message.tool_calls {
            let mut args = super::super::gemini_arguments(&call.function.arguments);
            coerce_go_numbers(&mut args);
            parts.push(GeminiPart::Call {
                call: GeminiFunction {
                    name: call.function.name,
                    args,
                    id: call.id,
                },
            });
        }
        candidates.push(GeminiCandidate {
            index: choice.index,
            content: GeminiContent {
                role: "model",
                parts,
            },
            finish_reason: match choice.finish_reason.as_ref() {
                "length" => "MAX_TOKENS",
                "content_filter" => "SAFETY",
                _ => "STOP",
            },
            safety_ratings: [],
        });
    }
    GeminiResponse {
        candidates,
        usage: source.usage.gemini_display(),
    }
}
#[derive(Debug, Serialize)]
pub(super) struct ResponsesResponse<'a> {
    id: Cow<'a, str>,
    object: &'static str,
    created_at: i64,
    status: &'static str,
    instructions: Option<()>,
    max_output_tokens: i64,
    model: Cow<'a, str>,
    output: Vec<Output<'a>>,
    parallel_tool_calls: bool,
    previous_response_id: Option<()>,
    reasoning: Option<()>,
    store: bool,
    temperature: i64,
    tool_choice: Option<()>,
    tools: Option<()>,
    top_p: i64,
    truncation: Option<()>,
    usage: CanonicalUsage<'a>,
    user: Option<()>,
    metadata: Option<()>,
    #[serde(skip_serializing_if = "Option::is_none")]
    incomplete_details: Option<Incomplete>,
}
#[derive(Debug, Serialize)]
struct Incomplete {
    reason: &'static str,
}
#[derive(Debug, Serialize)]
struct OutputContent<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    text: Cow<'a, str>,
    annotations: Vec<Value>,
}
#[derive(Debug, Serialize)]
struct Summary<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    text: Cow<'a, str>,
}
#[derive(Debug, Serialize)]
struct Output<'a> {
    #[serde(rename = "type")]
    kind: Cow<'a, str>,
    id: Cow<'a, str>,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<Option<Vec<OutputContent<'a>>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    quality: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    size: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    summary: Option<Vec<Summary<'a>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    call_id: Option<Cow<'a, str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    namespace: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    arguments: Option<Arguments<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    input: Option<String>,
}
#[derive(Debug, Serialize)]
#[serde(untagged)]
enum Arguments<'a> {
    Text(Cow<'a, str>),
    Raw(&'a Value),
}
impl<'a> Output<'a> {
    fn new(kind: Cow<'a, str>, id: Cow<'a, str>, status: &'static str) -> Self {
        Self {
            kind,
            id,
            status,
            role: Some(""),
            content: Some(None),
            quality: Some(""),
            size: Some(""),
            summary: None,
            call_id: None,
            name: None,
            namespace: None,
            arguments: None,
            input: None,
        }
    }
}
pub(super) fn chat_responses<'a>(
    source: ChatPivot<'a>,
    ctx: &Context,
) -> Result<ResponsesResponse<'a>, Error> {
    let id = if source.id.trim().is_empty() {
        Cow::Owned(crate::new_id("resp_"))
    } else {
        match source.id {
            Cow::Borrowed(text) => Cow::Borrowed(text.trim()),
            Cow::Owned(text) => Cow::Owned(text.trim().to_owned()),
        }
    };
    let mut output = Vec::new();
    let mut reason = "";
    if let Some(choice) = source.choices.into_iter().next() {
        reason = match choice.finish_reason.trim() {
            "length" => "max_output_tokens",
            "content_filter" => "content_filter",
            _ => "",
        };
        let status = if reason.is_empty() {
            "completed"
        } else {
            "incomplete"
        };
        let message = choice.message;
        if let Some(text) = message.reasoning_content.filter(|v| !v.is_empty()) {
            let mut item = Output::new(
                Cow::Borrowed("reasoning"),
                Cow::Owned(format!("{id}_reasoning_0")),
                status,
            );
            item.summary = Some(vec![Summary {
                kind: "summary_text",
                text,
            }]);
            output.push(item);
        }
        if !message.content.is_empty() {
            let mut item = Output::new(
                Cow::Borrowed("message"),
                Cow::Owned(format!("{id}_msg_0")),
                status,
            );
            item.role = Some("assistant");
            item.content = Some(Some(vec![OutputContent {
                kind: "output_text",
                text: message.content,
                annotations: citations::to_responses(&message.annotations.into_value())?,
            }]));
            output.push(item);
        }
        let tools = ctx.responses_tool_state();
        for (index, call) in message.tool_calls.into_iter().enumerate() {
            let function = call.kind.is_empty() || call.kind == "function";
            if function && call.function.name.trim().is_empty() {
                continue;
            }
            let call_id = if call.id.trim().is_empty() {
                Cow::Owned(format!("{id}_call_{index}"))
            } else {
                match call.id {
                    Cow::Borrowed(id) => Cow::Borrowed(id.trim()),
                    Cow::Owned(id) => Cow::Owned(id.trim().to_owned()),
                }
            };
            let mut item = Output::new(
                Cow::Borrowed(if function { "function_call" } else { call.kind }),
                call_id.clone(),
                status,
            );
            item.call_id = Some(call_id);
            if function {
                let (namespace, name) = tools.responses_tool_name(call.function.name);
                item.name = (!name.is_empty()).then_some(name);
                item.namespace = (!namespace.is_empty()).then_some(namespace);
                if tools.is_custom_tool(call.function.name) {
                    item.kind = Cow::Borrowed("custom_tool_call");
                    let args = call.function.arguments;
                    item.input = Some(
                        serde_json::from_str::<Value>(&args)
                            .ok()
                            .and_then(|mut v| v.as_object_mut().and_then(|o| o.remove("input")))
                            .map(|v| {
                                if let Value::String(text) = v {
                                    text
                                } else {
                                    crate::json::stringify(&v)
                                }
                            })
                            .unwrap_or_else(|| args.into_owned()),
                    );
                    item.role = None;
                    item.content = None;
                    item.quality = None;
                    item.size = None;
                } else {
                    item.arguments = Some(Arguments::Text(call.function.arguments));
                }
            } else if let Some(custom) = call.custom {
                item.arguments = Some(Arguments::Raw(custom));
            }
            output.push(item);
        }
    }
    Ok(ResponsesResponse {
        id,
        object: "response",
        created_at: source.created,
        status: if reason.is_empty() {
            "completed"
        } else {
            "incomplete"
        },
        instructions: None,
        max_output_tokens: 0,
        model: source.model,
        output,
        parallel_tool_calls: false,
        previous_response_id: None,
        reasoning: None,
        store: false,
        temperature: 0,
        tool_choice: None,
        tools: None,
        top_p: 0,
        truncation: None,
        usage: source.usage.through_chat(),
        user: None,
        metadata: None,
        incomplete_details: (!reason.is_empty()).then_some(Incomplete { reason }),
    })
}
